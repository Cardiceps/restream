//! Машина состояний ретранслятора и процессы ffmpeg.

use crate::config::Config;
use crate::proxy::{Event, EventKind, Proxy};
use crate::srt::SrtProxy;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Live,
    Fallback,
}

impl State {
    pub fn name(&self) -> &'static str {
        match self {
            State::Idle => "IDLE",
            State::Live => "LIVE",
            State::Fallback => "FALLBACK",
        }
    }
}

/// Один выход: имя площадки, адрес с ключом и живой процесс ffmpeg.
pub struct Output {
    pub name: String,
    pub url: String,
    child: Mutex<Option<Child>>,
    fails: Mutex<u32>,
    next_try: Mutex<Instant>,
    started: Mutex<Instant>,
}

impl Output {
    fn new(name: String, url: String) -> Self {
        Self {
            name,
            url,
            child: Mutex::new(None),
            fails: Mutex::new(0),
            next_try: Mutex::new(Instant::now()),
            started: Mutex::new(Instant::now()),
        }
    }

    fn running(&self) -> bool {
        let mut slot = self.child.lock().unwrap();
        match slot.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn restarts(&self) -> u32 {
        *self.fails.lock().unwrap()
    }

    fn stop(&self) {
        let mut slot = self.child.lock().unwrap();
        if let Some(mut c) = slot.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Запуск ffmpeg. stderr читается отдельным потоком, иначе буфер переполнится
    /// и процесс зависнет.
    fn start(&self, args: &[String]) {
        crate::log_info!("[{}] запуск ffmpeg", self.name);
        let mut cmd = Command::new(&args[0]);
        cmd.args(&args[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        match cmd.spawn() {
            Ok(mut child) => {
                if let Some(err) = child.stderr.take() {
                    let name = self.name.clone();
                    let url = self.url.clone();
                    thread::Builder::new()
                        .name(format!("ffmpeg-{}", name))
                        .spawn(move || pump_stderr(err, &name, &url))
                        .ok();
                }
                *self.started.lock().unwrap() = Instant::now();
                *self.child.lock().unwrap() = Some(child);
            }
            Err(e) => {
                crate::log_warn!("[{}] не удалось запустить ffmpeg: {}", self.name, e);
                *self.next_try.lock().unwrap() = Instant::now() + Duration::from_secs(5);
            }
        }
    }
}

fn pump_stderr(err: impl std::io::Read, name: &str, url: &str) {
    use std::io::{BufRead, BufReader};
    let quiet = crate::log::ffmpeg_quiet();
    for line in BufReader::new(err).lines().map_while(Result::ok) {
        // Ключи стримов в лог не попадают.
        let line = line.replace(url, "<url>");
        if line.trim().is_empty() || quiet {
            continue;
        }
        crate::log_warn!("[ffmpeg:{}] {}", name, line);
    }
}

/// Снимок состояния выходов для control API.
#[derive(Debug, Clone)]
pub struct OutStatus {
    pub name: String,
    pub running: bool,
    pub restarts: u32,
}

/// Всё, что читает control API. Обновляется основным циклом.
pub struct Shared {
    pub state: Mutex<State>,
    pub state_since: Mutex<Instant>,
    pub phone: AtomicBool,
    pub last_event: Mutex<Option<String>>,
    pub latch: AtomicBool,
    pub outputs: Mutex<Vec<OutStatus>>,
}

impl Shared {
    fn new(outs: Vec<OutStatus>) -> Self {
        Self {
            state: Mutex::new(State::Idle),
            state_since: Mutex::new(Instant::now()),
            phone: AtomicBool::new(false),
            last_event: Mutex::new(None),
            latch: AtomicBool::new(false),
            outputs: Mutex::new(outs),
        }
    }

    /// JSON в том же виде, что отдавала прежняя версия: на него опираются скрипты.
    pub fn status_json(&self) -> String {
        let state = *self.state.lock().unwrap();
        let secs = Instant::now()
            .duration_since(*self.state_since.lock().unwrap())
            .as_secs_f64();
        let outs: Vec<String> = self
            .outputs
            .lock()
            .unwrap()
            .iter()
            .map(|o| {
                format!(
                    r#""{}":{{"running":{},"restarts_in_row":{}}}"#,
                    o.name, o.running, o.restarts
                )
            })
            .collect();
        let last = self
            .last_event
            .lock()
            .unwrap()
            .clone()
            .map(|s| format!(r#""{}""#, s))
            .unwrap_or_else(|| "null".to_string());
        format!(
            r#"{{"state":"{}","seconds_in_state":{:.1},"phone_connected":{},"last_event":{},"latched_until_phone_reconnects":{},"outputs":{{{}}}}}"#,
            state.name(),
            secs,
            self.phone.load(Ordering::SeqCst),
            last,
            self.latch.load(Ordering::SeqCst),
            outs.join(",")
        )
    }
}

pub struct Relay {
    cfg: Config,
    outputs: Vec<Output>,
    shared: Arc<Shared>,
    proxy: Arc<Proxy>,
    srt: Option<Arc<SrtProxy>>,
    /// Номер текущей публикации; по нему отсекаются устаревшие события.
    current_gen: std::sync::atomic::AtomicU64,
    events: Receiver<Event>,
    stop_requested: Arc<AtomicBool>,
}

impl Relay {
    pub fn new(cfg: Config, stop_requested: Arc<AtomicBool>) -> Result<Self, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let generations = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let proxy = Proxy::new(
            format!("{}:{}", cfg.listen_host, cfg.listen_port),
            format!("{}:{}", cfg.mtx_host, cfg.mtx_port),
            cfg.live_path.clone(),
            Duration::from_secs_f64(cfg.stall_secs.max(0.1)),
            tx.clone(),
            std::sync::Arc::clone(&generations),
        );
        let srt = if cfg.srt_enabled {
            Some(SrtProxy::new(
                format!("{}:{}", cfg.srt_host, cfg.srt_port),
                format!("{}:{}", cfg.srt_upstream_host, cfg.srt_upstream_port),
                cfg.live_path.clone(),
                Duration::from_secs_f64(cfg.stall_secs.max(0.1)),
                tx,
                generations,
            ))
        } else {
            None
        };
        let outputs: Vec<Output> = cfg
            .outputs
            .iter()
            .map(|o| Output::new(o.name.clone(), o.url.clone()))
            .collect();
        let snapshot = outputs
            .iter()
            .map(|o| OutStatus {
                name: o.name.clone(),
                running: false,
                restarts: 0,
            })
            .collect();
        Ok(Self {
            cfg,
            outputs,
            shared: Arc::new(Shared::new(snapshot)),
            proxy,
            srt,
            current_gen: std::sync::atomic::AtomicU64::new(0),
            events: rx,
            stop_requested,
        })
    }

    pub fn shared(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    pub fn has_outputs(&self) -> bool {
        !self.outputs.is_empty()
    }

    /// Занимает SRT-порт, если приём SRT включён.
    pub fn bind_srt(&self) -> Result<Option<std::net::UdpSocket>, String> {
        match &self.srt {
            None => Ok(None),
            Some(s) => s
                .bind()
                .map(Some)
                .map_err(|e| format!("не удалось занять {} (UDP): {}", self.cfg_srt_addr(), e)),
        }
    }

    fn cfg_srt_addr(&self) -> String {
        format!("{}:{}", self.cfg.srt_host, self.cfg.srt_port)
    }

    pub fn start_srt(&self, sock: std::net::UdpSocket) {
        if let Some(srt) = &self.srt {
            let s = Arc::clone(srt);
            thread::Builder::new()
                .name("srt-accept".into())
                .spawn(move || s.serve(sock))
                .ok();
        }
    }

    /// Занимает порт заранее: ошибка «занято» должна быть видна сразу.
    pub fn bind(&self) -> std::io::Result<std::net::TcpListener> {
        self.proxy.bind()
    }

    pub fn start_proxy(&self, listener: std::net::TcpListener) {
        let proxy = Arc::clone(&self.proxy);
        thread::Builder::new()
            .name("rtmp-accept".into())
            .spawn(move || proxy.serve(listener))
            .ok();
    }

    fn args(&self, out: &Output) -> Vec<String> {
        let mut a: Vec<String> = vec![
            self.cfg.ffmpeg.clone(),
            "-hide_banner".into(),
            "-loglevel".into(),
            if crate::log::ffmpeg_quiet() {
                "error".into()
            } else {
                "warning".into()
            },
            "-nostdin".into(),
        ];
        match *self.shared.state.lock().unwrap() {
            State::Live => {
                a.extend([
                    "-analyzeduration".into(),
                    "2000000".into(),
                    "-probesize".into(),
                    "2000000".into(),
                    "-i".into(),
                    format!("{}/{}", self.cfg.mtx_rtmp, self.cfg.live_path),
                ]);
            }
            _ => {
                a.extend([
                    "-re".into(),
                    "-stream_loop".into(),
                    "-1".into(),
                    "-i".into(),
                    self.cfg.fallback_file.clone(),
                ]);
            }
        }
        a.extend([
            "-c".into(),
            "copy".into(),
            "-f".into(),
            "flv".into(),
            "-flvflags".into(),
            "no_duration_filesize".into(),
            out.url.clone(),
        ]);
        a
    }

    fn set_state(&self, new: State, why: &str) {
        let old = {
            let mut s = self.shared.state.lock().unwrap();
            let old = *s;
            *s = new;
            *self.shared.state_since.lock().unwrap() = Instant::now();
            old
        };
        crate::log_info!("состояние: {} -> {} {}", old.name(), new.name(), why);
        for o in &self.outputs {
            o.stop();
            *o.fails.lock().unwrap() = 0;
            *o.next_try.lock().unwrap() = Instant::now();
        }
        self.snapshot();
    }

    fn snapshot(&self) {
        let mut list = self.shared.outputs.lock().unwrap();
        for (slot, o) in list.iter_mut().zip(self.outputs.iter()) {
            slot.running = o.running();
            slot.restarts = o.restarts();
        }
    }

    fn do_stop(&self, why: &str) {
        if *self.shared.state.lock().unwrap() != State::Idle {
            self.set_state(State::Idle, why);
        }
    }

    /// Готовность пути в MediaMTX: без проверки ffmpeg иногда подключается раньше
    /// публикатора и получает «no stream is available».
    fn path_ready(&self) -> bool {
        if self.cfg.mtx_api.is_empty() {
            return true;
        }
        let url = format!("{}/v3/paths/get/{}", self.cfg.mtx_api, self.cfg.live_path);
        match crate::http::get_json(&url, Duration::from_millis(400)) {
            Some(v) => v.get("ready").and_then(|r| r.as_bool()).unwrap_or(false),
            None => false,
        }
    }

    fn maintain(&self, now: Instant) {
        let state = *self.shared.state.lock().unwrap();
        if state == State::Idle {
            return;
        }
        for o in &self.outputs {
            if o.running() {
                if now.duration_since(*o.started.lock().unwrap()) > Duration::from_secs(30) {
                    *o.fails.lock().unwrap() = 0;
                }
                continue;
            }
            // Процесс умер: считаем попытку и откладываем перезапуск.
            let had_child = {
                let mut slot = o.child.lock().unwrap();
                let had = slot.is_some();
                if had {
                    slot.take();
                }
                had
            };
            if had_child {
                crate::log_warn!("[{}] ffmpeg завершился", o.name);
                let mut f = o.fails.lock().unwrap();
                *f = f.saturating_add(1);
                let wait = 2u64.saturating_pow((*f).min(4)).min(30);
                drop(f);
                *o.next_try.lock().unwrap() = now + Duration::from_secs(wait);
            }
            if now < *o.next_try.lock().unwrap() {
                continue;
            }
            if state == State::Live && !self.path_ready() {
                continue;
            }
            let args = self.args(o);
            o.start(&args);
        }
        self.snapshot();
    }

    pub fn run(&mut self) -> Result<(), String> {
        let poll = Duration::from_secs_f64(self.cfg.poll_secs.max(0.05));
        let mut pending: Option<Instant> = None;

        while crate::sys::running() {
            let deadline = Instant::now() + poll;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                match self.events.recv_timeout(left) {
                    Ok(ev) => self.on_event(ev, &mut pending),
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            self.tick(&mut pending);
        }

        self.proxy.shutdown();
        for o in &self.outputs {
            o.stop();
        }
        self.snapshot();
        crate::log_info!("остановлено");
        Ok(())
    }

    fn on_event(&self, ev: Event, pending: &mut Option<Instant>) {
        // Событие от старой сессии (например, запоздавший drop от прежнего подключения
        // после переподключения) не должно менять состояние.
        if ev.kind != EventKind::Publish {
            let cur = self.current_gen.load(Ordering::SeqCst);
            if ev.gen < cur {
                crate::log_info!(
                    "событие от прежнего подключения (gen {} < {}) — игнорирую",
                    ev.gen,
                    cur
                );
                return;
            }
        } else {
            self.current_gen.store(ev.gen, Ordering::SeqCst);
        }
        let label = match ev.kind {
            EventKind::Publish => "publish",
            EventKind::End => "end",
            EventKind::Drop => "drop",
        };
        *self.shared.last_event.lock().unwrap() = Some(label.to_string());
        match ev.kind {
            EventKind::Publish => {
                self.shared.phone.store(true, Ordering::SeqCst);
                self.shared.latch.store(false, Ordering::SeqCst);
                *pending = Some(Instant::now());
            }
            EventKind::End => {
                self.shared.phone.store(false, Ordering::SeqCst);
                *pending = None;
                if *self.shared.state.lock().unwrap() != State::Idle {
                    self.do_stop("(штатное завершение трансляции)");
                } else {
                    crate::log_info!("телефон завершил трансляцию, ретранслятор и так в IDLE");
                }
            }
            EventKind::Drop => {
                self.shared.phone.store(false, Ordering::SeqCst);
                *pending = None;
            }
        }
    }

    fn tick(&self, pending: &mut Option<Instant>) {
        let now = Instant::now();

        if self.stop_requested.swap(false, Ordering::SeqCst) {
            if *self.shared.state.lock().unwrap() != State::Idle {
                self.shared.latch.store(true, Ordering::SeqCst);
                self.do_stop("(команда stop)");
            } else {
                crate::log_info!("команда stop: ретранслятор уже в IDLE");
            }
        }

        let phone = self.shared.phone.load(Ordering::SeqCst);
        let live_ok = phone
            && pending
                .map(|t| now.duration_since(t).as_secs_f64() >= self.cfg.live_confirm_secs)
                .unwrap_or(false);

        let state = *self.shared.state.lock().unwrap();
        match state {
            State::Idle => {
                if live_ok && !self.shared.latch.load(Ordering::SeqCst) {
                    self.set_state(State::Live, "(телефон на связи)");
                }
            }
            State::Live => {
                if !phone {
                    self.set_state(State::Fallback, "(телефон пропал без завершения — заглушка)");
                }
            }
            State::Fallback => {
                if live_ok {
                    self.set_state(State::Live, "(телефон вернулся)");
                } else if self.cfg.fallback_max_secs > 0.0 {
                    let elapsed = now
                        .duration_since(*self.shared.state_since.lock().unwrap())
                        .as_secs_f64();
                    if elapsed > self.cfg.fallback_max_secs {
                        self.do_stop("(заглушка идёт дольше лимита)");
                    }
                }
            }
        }

        if crate::sys::running() {
            self.maintain(now);
        }
    }
}

impl Relay {
    /// Имена выходов для приветственной строки в логе.
    pub fn outputs_names(&self) -> Vec<String> {
        self.outputs.iter().map(|o| o.name.clone()).collect()
    }
}
