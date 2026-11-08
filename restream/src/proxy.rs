//! Прозрачный TCP-прокси для RTMP: телефон -> restream -> MediaMTX.
//!
//! Байты пересылаются без изменений. Поток клиент→сервер параллельно отдаётся
//! [`TapParser`], который находит команды `publish` и команды завершения.
//! Тишина в сокете дольше `stall` считается обрывом связи.

use crate::chunk::TapParser;
use crate::{log_info, log_warn};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Команды, которыми клиент штатно завершает публикацию.
pub const END_COMMANDS: [&str; 3] = ["FCUnpublish", "deleteStream", "closeStream"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// Клиент начал публикацию на live_path.
    Publish,
    /// Клиент прислал команду завершения.
    End,
    /// Соединение пропало без команды завершения.
    Drop,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub kind: EventKind,
    /// Номер поколения публикации. Растёт на каждом новом `publish`;
    /// события с устаревшим номером игнорируются.
    pub gen: u64,
}

/// Общее для обоих направлений одного соединения.
struct Conn {
    id: u64,
    peer: SocketAddr,
    cli: TcpStream,
    up: TcpStream,
    /// `app` из команды connect.
    app: Mutex<String>,
    /// Клиент публикует именно на live_path.
    publishing: AtomicBool,
    /// Пришла команда завершения: последующее закрытие — не обрыв.
    graceful: AtomicBool,
    /// Вытеснен новым подключением: закрытие не считается обрывом.
    superseded: AtomicBool,
    /// Номер поколения публикации.
    generation: AtomicU64,
    closed: AtomicBool,
}

impl Conn {
    fn close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            let _ = self.cli.shutdown(Shutdown::Both);
            let _ = self.up.shutdown(Shutdown::Both);
        }
    }
}

pub struct Proxy {
    listen: String,
    upstream: String,
    live_path: String,
    stall: Duration,
    events: Sender<Event>,
    next_id: AtomicU64,
    /// Номер текущего поколения публикации (общий с SRT-прокси).
    generations: Arc<AtomicU64>,
    /// Текущий публикатор на live_path: новое подключение вытесняет старое.
    publisher: Mutex<Option<Arc<Conn>>>,
    conns: Mutex<HashMap<u64, Arc<Conn>>>,
}

impl Proxy {
    pub fn new(
        listen: String,
        upstream: String,
        live_path: String,
        stall: Duration,
        events: Sender<Event>,
        generations: Arc<AtomicU64>,
    ) -> Arc<Self> {
        Arc::new(Self {
            listen,
            upstream,
            live_path: live_path.trim_matches('/').to_string(),
            stall,
            events,
            next_id: AtomicU64::new(1),
            generations,
            publisher: Mutex::new(None),
            conns: Mutex::new(HashMap::new()),
        })
    }

    /// Занимает порт до запуска цикла, чтобы ошибка «порт занят» была видна сразу.
    pub fn bind(&self) -> std::io::Result<TcpListener> {
        let l = TcpListener::bind(&self.listen)?;
        log_info!("RTMP-прокси слушает {} -> {}", self.listen, self.upstream);
        Ok(l)
    }

    pub fn serve(self: Arc<Self>, listener: TcpListener) {
        for stream in listener.incoming() {
            if !crate::sys::running() {
                break;
            }
            match stream {
                Ok(cli) => {
                    let me = Arc::clone(&self);
                    thread::Builder::new()
                        .name("rtmp-conn".into())
                        .spawn(move || me.handle(cli))
                        .ok();
                }
                Err(e) => {
                    log_warn!("accept: {}", e);
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    /// Разрывает все соединения: используется при остановке сервиса.
    pub fn shutdown(&self) {
        let all: Vec<_> = self.conns.lock().unwrap().values().cloned().collect();
        for c in all {
            c.close();
        }
    }

    fn emit(&self, kind: EventKind, gen: u64, _c: &Conn) {
        let _ = self.events.send(Event { kind, gen });
    }

    fn handle(self: Arc<Self>, cli: TcpStream) {
        let Ok(peer) = cli.peer_addr() else { return };
        let up = match connect(&self.upstream) {
            Ok(s) => s,
            Err(e) => {
                log_warn!("MediaMTX недоступен ({}): {}", self.upstream, e);
                let _ = cli.shutdown(Shutdown::Both);
                return;
            }
        };
        let _ = cli.set_nodelay(true);
        let _ = up.set_nodelay(true);
        let _ = cli.set_read_timeout(Some(self.stall));

        let (cli_w, up_r) = match (cli.try_clone(), up.try_clone()) {
            (Ok(a), Ok(b)) => (a, b),
            _ => return,
        };

        let conn = Arc::new(Conn {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            peer,
            cli,
            up,
            app: Mutex::new(String::new()),
            publishing: AtomicBool::new(false),
            graceful: AtomicBool::new(false),
            superseded: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        });
        self.conns.lock().unwrap().insert(conn.id, Arc::clone(&conn));

        // MediaMTX -> клиент: чистая пересылка в отдельном потоке.
        let down_conn = Arc::clone(&conn);
        let down = thread::Builder::new()
            .name("rtmp-down".into())
            .spawn(move || {
                pump(up_r, cli_w);
                down_conn.close();
            })
            .ok();

        self.forward(&conn);
        conn.close();
        if let Some(h) = down {
            let _ = h.join();
        }
        self.finish(&conn);
    }

    /// Клиент -> MediaMTX. Здесь же разбор команд и детект тишины.
    fn forward(&self, conn: &Arc<Conn>) {
        let mut parser = TapParser::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match (&conn.cli).read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    if !conn.graceful.load(Ordering::SeqCst)
                        && !conn.superseded.load(Ordering::SeqCst)
                        && !conn.closed.load(Ordering::SeqCst)
                    {
                        log_info!(
                            "[{}] тишина {:.1} с — считаю обрывом связи",
                            conn.peer.ip(),
                            self.stall.as_secs_f64()
                        );
                    }
                    break;
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            if (&conn.up).write_all(&buf[..n]).is_err() {
                break;
            }
            for cmd in parser.feed(&buf[..n]) {
                self.on_command(conn, &cmd);
            }
            if conn.graceful.load(Ordering::SeqCst) {
                // Даём MediaMTX дочитать хвост и закрываемся сами.
                break;
            }
        }
    }

    fn on_command(&self, conn: &Arc<Conn>, cmd: &crate::chunk::Command) {
        match cmd.name.as_str() {
            "connect" => {
                if let Some(app) = cmd.app() {
                    *conn.app.lock().unwrap() = app.trim_matches('/').to_string();
                }
            }
            "publish" => {
                let base = conn.app.lock().unwrap().clone();
                let path = cmd.publish_path(&base);
                if path != self.live_path {
                    log_info!("[{}] публикация на чужой путь — не отслеживаю", conn.peer.ip());
                    return;
                }
                conn.publishing.store(true, Ordering::SeqCst);
                let gen = self.generations.fetch_add(1, Ordering::SeqCst) + 1;
                conn.generation.store(gen, Ordering::SeqCst);
                log_info!("[{}] публикация на {}", conn.peer.ip(), path);

                // Новое подключение вытесняет старое: Larix переподключается быстрее,
                // чем истекает таймаут прежнего сокета.
                let old = self.publisher.lock().unwrap().replace(Arc::clone(conn));
                if let Some(old) = old {
                    if old.id != conn.id {
                        log_info!("[{}] новое подключение вытесняет предыдущее", conn.peer.ip());
                        old.superseded.store(true, Ordering::SeqCst);
                        old.publishing.store(false, Ordering::SeqCst);
                        old.close();
                    }
                }
                self.emit(EventKind::Publish, gen, conn);
            }
            name if END_COMMANDS.contains(&name) => {
                if conn.publishing.load(Ordering::SeqCst)
                    && !conn.graceful.swap(true, Ordering::SeqCst)
                {
                    log_info!("[{}] штатное завершение трансляции ({})", conn.peer.ip(), name);
                    let gen = conn.generation.load(Ordering::SeqCst);
                    self.emit(EventKind::End, gen, conn);
                }
            }
            _ => {}
        }
    }

    fn finish(&self, conn: &Arc<Conn>) {
        self.conns.lock().unwrap().remove(&conn.id);
        let mut slot = self.publisher.lock().unwrap();
        let is_current = slot.as_ref().map(|c| c.id == conn.id).unwrap_or(false);
        if is_current {
            *slot = None;
        }
        drop(slot);
        // Обрыв засчитываем, только если это текущий публикатор и он не завершился штатно.
        if is_current
            && conn.publishing.load(Ordering::SeqCst)
            && !conn.graceful.load(Ordering::SeqCst)
            && !conn.superseded.load(Ordering::SeqCst)
        {
            let gen = conn.generation.load(Ordering::SeqCst);
            self.emit(EventKind::Drop, gen, conn);
        }
    }
}

fn connect(addr: &str) -> std::io::Result<TcpStream> {
    let mut last = None;
    for a in addr.to_socket_addrs()? {
        match TcpStream::connect_timeout(&a, Duration::from_secs(10)) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("нет адреса")))
}

fn pump(mut from: TcpStream, mut to: TcpStream) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}
