//! restream — RTMP-ретранслятор с заглушкой для IRL-стримов.
//!
//! ```text
//! Larix (LTE) --RTMP--> restream (:1935) --> MediaMTX (127.0.0.1:11935) --> ffmpeg --> Twitch/Kick
//! ```
//!
//! Прокси прозрачно пересылает байты и попутно разбирает RTMP-команды телефона, поэтому
//! отличает штатное завершение трансляции (`FCUnpublish` / `deleteStream` / `closeStream`)
//! от обрыва связи (сокет замолчал без команд).
//!
//! Зависимостей нет: только стандартная библиотека Rust.

mod amf0;
mod chunk;
mod config;
mod fallback;
mod http;
mod log;
mod proxy;
mod relay;
mod srt;
mod sys;

pub(crate) use log::{log_error, log_info, log_warn};

use config::Config;
use relay::Relay;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const DEFAULT_CONFIG: &str = "/etc/restream/config.ini";

fn usage() -> ! {
    eprintln!(
        "restream — ретранслятор RTMP с заглушкой\n\
         \n\
         Использование: restream [-c КОНФИГ] КОМАНДА\n\
         \n\
         Команды:\n\
           run             запустить ретранслятор (это делает systemd)\n\
           status          показать состояние\n\
           stop            остановить трансляцию вручную\n\
           make-fallback   создать файл заглушки\n\
         \n\
         Параметры make-fallback:\n\
           --image ФАЙЛ    картинка для заглушки (png/jpg)\n\
           --video ФАЙЛ    видео для заглушки (звук будет отброшен)\n\
           --text ТЕКСТ    текст на стандартной заглушке (по умолчанию «Скоро вернёмся»)\n\
           --seconds N     длительность в секундах (по умолчанию 30)\n\
           -o ФАЙЛ         куда положить"
    );
    std::process::exit(2)
}

fn main() -> ExitCode {
    let all: Vec<String> = std::env::args().skip(1).collect();
    let mut config_path = PathBuf::from(DEFAULT_CONFIG);
    let mut cmd: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();

    let mut i = 0;
    while i < all.len() {
        match all[i].as_str() {
            "-c" | "--config" => {
                i += 1;
                match all.get(i) {
                    Some(p) => config_path = PathBuf::from(p),
                    None => {
                        eprintln!("после -c нужен путь к конфигу");
                        return ExitCode::from(2);
                    }
                }
            }
            "-h" | "--help" => usage(),
            other if cmd.is_none() && !other.starts_with('-') => cmd = Some(other.to_string()),
            other => rest.push(other.to_string()),
        }
        i += 1;
    }

    let Some(cmd) = cmd else { usage() };

    if cmd == "make-fallback" {
        let cfg = match Config::load(&config_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{}", e);
                return ExitCode::FAILURE;
            }
        };
        return match fallback::run(&cfg, &rest) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::FAILURE
            }
        };
    }

    // status и stop идут через control API: конфиг может быть недоступен на чтение
    // обычному пользователю.
    if cmd == "status" || cmd == "stop" {
        return match control_call(&config_path, &cmd) {
            Ok(out) => {
                println!("{}", out);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::FAILURE
            }
        };
    }

    if cmd != "run" {
        eprintln!("неизвестная команда: {}", cmd);
        usage();
    }

    let cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}", e);
            return ExitCode::FAILURE;
        }
    };

    if which(&cfg.ffmpeg).is_none() {
        eprintln!("ffmpeg не найден: {}", cfg.ffmpeg);
        return ExitCode::FAILURE;
    }
    if !Path::new(&cfg.fallback_file).is_file() {
        eprintln!(
            "нет файла заглушки {}. Создай: restream make-fallback",
            cfg.fallback_file
        );
        return ExitCode::FAILURE;
    }

    sys::install_handlers();
    log::set_ffmpeg_quiet(cfg.quiet_ffmpeg);

    let listen = format!("{}:{}", cfg.listen_host, cfg.listen_port);
    let upstream = format!("{}:{}", cfg.mtx_host, cfg.mtx_port);
    let control = (cfg.control_host.clone(), cfg.control_port);
    let live_label = format!("{}/{}", cfg.mtx_rtmp, cfg.live_path);
    let stop_requested = Arc::new(AtomicBool::new(false));

    let mut relay = match Relay::new(cfg, Arc::clone(&stop_requested)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}", e);
            return ExitCode::FAILURE;
        }
    };

    if !relay.has_outputs() {
        log::warn("в конфиге нет ни одного включённого [output.*] — отправлять некуда");
    }

    let listener = match relay.bind() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("не удалось занять {} (порт занят?): {}", listen, e);
            return ExitCode::FAILURE;
        }
    };

    // SRT необязателен: если порт занят, RTMP продолжает работать.
    let srt_sock = match relay.bind_srt() {
        Ok(s) => s,
        Err(e) => {
            log::error(&format!("SRT отключён: {}", e));
            None
        }
    };

    log::info(&format!(
        "live: {} | RTMP {} -> {} | SRT: {} | выходы: {}",
        live_label,
        listen,
        upstream,
        if srt_sock.is_some() {
            "включён".to_string()
        } else {
            "выключен".to_string()
        },
        relay.outputs_names().join(", ")
    ));

    relay.start_proxy(listener);
    if let Some(sock) = srt_sock {
        relay.start_srt(sock);
    }

    // control API получает разделяемое состояние, без ссылок на сам Relay.
    let shared = relay.shared();
    let status_fn: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || shared.status_json());
    http::serve_control(
        &control.0,
        control.1,
        status_fn,
        Arc::clone(&stop_requested),
    );

    match relay.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", e);
            ExitCode::FAILURE
        }
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return if p.is_file() { Some(p) } else { None };
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

fn control_call(config_path: &Path, cmd: &str) -> Result<String, String> {
    let (host, port) = control_addr(config_path);
    let (method, path) = if cmd == "status" {
        ("GET", "/status")
    } else {
        ("POST", "/stop")
    };
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(format!("{}:{}", host, port))
        .map_err(|e| format!("сервис restream не отвечает: {}", e))?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(3)));
    let _ = write!(
        stream,
        "{} {} HTTP/1.0\r\nHost: {}:{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        method, path, host, port
    );
    let mut resp = String::new();
    stream
        .read_to_string(&mut resp)
        .map_err(|e| format!("сервис restream не отвечает: {}", e))?;
    let body = resp
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or(&resp)
        .trim()
        .to_string();
    if body.is_empty() {
        return Err("сервис restream вернул пустой ответ".to_string());
    }
    Ok(body)
}

/// Адрес control API берём из конфига, если он читается.
fn control_addr(config_path: &Path) -> (String, u16) {
    let mut host = "127.0.0.1".to_string();
    let mut port = 8088u16;
    if let Ok(text) = std::fs::read_to_string(config_path) {
        let ini = config::Ini::parse(&text);
        if let Some(h) = ini.get("general", "control_host") {
            host = h.to_string();
        }
        if let Some(p) = ini
            .get("general", "control_port")
            .and_then(|s| s.trim().parse::<u16>().ok())
        {
            port = p;
        }
    }
    (host, port)
}
