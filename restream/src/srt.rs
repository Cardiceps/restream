//! Приём SRT: UDP-прокси перед MediaMTX.
//!
//! В SRT есть полноценный «закрывающий пакет» — управляющий пакет `SHUTDOWN` (тип 5).
//! Клиент шлёт его при штатном завершении публикации и не шлёт при обрыве связи.
//! Поэтому логика та же, что для RTMP: `SHUTDOWN` от клиента — конец эфира,
//! тишина без него — обрыв и заглушка.
//!
//! Путь публикации берётся из `streamid` в handshake:
//! `publish:live/ключ` либо `#!::r=live/ключ,m=publish`.
//!
//! Датаграммы пересылаются без изменений; раскодируется только управляющий слой SRT.

use crate::proxy::{Event, EventKind};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const CTRL_HANDSHAKE: u16 = 0;
const CTRL_SHUTDOWN: u16 = 5;
/// Команда SID в расширениях handshake.
const CMD_SID: u16 = 5;
/// Управляющий пакет SRT: старший бит первого байта.
const CTRL_FLAG: u8 = 0x80;
/// Разумный предел на датаграмму SRT.
const MAX_PKT: usize = 1500;

/// Разобранный `streamid`.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamId {
    pub path: String,
    pub publish: bool,
}

/// `publish:live/key` или `#!::r=live/key,m=publish`.
pub fn parse_stream_id(raw: &str) -> Option<StreamId> {
    let s = raw.trim().trim_end_matches('\0').trim();
    if s.is_empty() {
        return None;
    }

    // Форма SRT access control: "#!::r=path,m=publish".
    if let Some(rest) = s.strip_prefix("#!::") {
        let mut path = String::new();
        let mut publish = true;
        foreach_pair(rest, |k, v| match k {
            "r" | "h" => path = v.trim_matches('/').to_string(),
            "m" => publish = v != "request" && v != "play",
            _ => {}
        });
        return if path.is_empty() {
            None
        } else {
            Some(StreamId { path, publish })
        };
    }

    // Форма "mode:path".
    if let Some((mode, path)) = s.split_once(':') {
        let path = path.trim_matches('/').to_string();
        if path.is_empty() {
            return None;
        }
        return Some(StreamId {
            path,
            publish: mode.eq_ignore_ascii_case("publish"),
        });
    }

    // Форма "path" — публикация по умолчанию.
    let path = s.trim_matches('/').to_string();
    if path.is_empty() {
        None
    } else {
        Some(StreamId {
            path,
            publish: true,
        })
    }
}

/// Разбирает пары `k=v` через запятую (без вложенности).
fn foreach_pair(s: &str, mut f: impl FnMut(&str, &str)) {
    for part in s.split(',') {
        if let Some((k, v)) = part.split_once('=') {
            f(k.trim(), v.trim());
        }
    }
}

/// Достаёт streamid из handshake-пакета SRT.
///
/// Расширения идут последовательностью TLV: `uint16 cmd`, `uint16 длина в 4-байтовых
/// словах`, значение. Смещение плавает между версиями libsrt, поэтому TLV ищутся
/// сканированием, а не по фиксированному офсету.
pub fn stream_id_from_handshake(data: &[u8]) -> Option<String> {
    if data.len() < 32 || data[0] & CTRL_FLAG == 0 || ctrl_type(data) != CTRL_HANDSHAKE {
        return None;
    }
    // Расширения SRT — цепочка TLV, каждое значение кратно 4 байтам, поэтому
    // сканируем только по границам слова. Побайтовый проход здесь опасен:
    // случайно «опознанный» заголовок на неверном смещении уводит разбор дальше
    // настоящего SID.
    let mut off = 32;
    while off + 4 <= data.len() {
        let cmd = u16::from_be_bytes([data[off], data[off + 1]]);
        let words = u16::from_be_bytes([data[off + 2], data[off + 3]]) as usize;
        let len = words * 4;
        if cmd == CMD_SID && len > 0 && off + 4 + len <= data.len() {
            let s = decode_sid(&data[off + 4..off + 4 + len]);
            // Ложное совпадение отсекаем по печатности.
            if !s.is_empty() && s.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
                return Some(s);
            }
        }
        off += 4;
    }
    None
}

/// SID передаётся как массив uint32; каждое 4-байтовое слово уходит в сеть
/// little-endian, поэтому внутри слова порядок байт обратный.
fn decode_sid(val: &[u8]) -> String {
    let mut out = Vec::with_capacity(val.len());
    for chunk in val.chunks(4) {
        let mut b = [0u8; 4];
        b[..chunk.len()].copy_from_slice(chunk);
        out.extend_from_slice(&u32::from_le_bytes(b).to_be_bytes());
    }
    while out.last() == Some(&0) {
        out.pop();
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn ctrl_type(data: &[u8]) -> u16 {
    u16::from_be_bytes([data[0], data[1]]) & 0x7FFF
}

fn is_shutdown(data: &[u8]) -> bool {
    data.len() >= 4 && data[0] & CTRL_FLAG != 0 && ctrl_type(data) == CTRL_SHUTDOWN
}

/// Одна клиентская сессия SRT.
struct Session {
    client: SocketAddr,
    up: UdpSocket,
    last_seen: Mutex<Instant>,
    stream: Mutex<Option<StreamId>>,
    /// Поколение публикации: по нему отсекаются события от старых сессий.
    generation: AtomicU64,
    publishing: AtomicBool,
    graceful: AtomicBool,
    closed: AtomicBool,
}

impl Session {
    fn touch(&self) {
        *self.last_seen.lock().unwrap() = Instant::now();
    }

    fn idle(&self) -> Duration {
        Instant::now().saturating_duration_since(*self.last_seen.lock().unwrap())
    }
}

pub struct SrtProxy {
    listen: String,
    upstream: String,
    live_path: String,
    stall: Duration,
    events: Sender<Event>,
    generations: Arc<AtomicU64>,
    sessions: Mutex<HashMap<SocketAddr, Arc<Session>>>,
}

impl SrtProxy {
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
            generations,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub fn bind(&self) -> std::io::Result<UdpSocket> {
        let s = UdpSocket::bind(&self.listen)?;
        s.set_read_timeout(Some(Duration::from_millis(200)))?;
        crate::log_info!("SRT-прокси слушает {} -> {}", self.listen, self.upstream);
        Ok(s)
    }

    pub fn serve(self: Arc<Self>, sock: UdpSocket) {
        let send = match sock.try_clone() {
            Ok(s) => s,
            Err(e) => {
                crate::log_error!("SRT: не удалось клонировать сокет: {}", e);
                return;
            }
        };
        let mut buf = vec![0u8; MAX_PKT];
        while crate::sys::running() {
            match sock.recv_from(&mut buf) {
                Ok((n, addr)) => self.on_datagram(&send, addr, &buf[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    crate::log_warn!("SRT: приём: {}", e);
                    thread::sleep(Duration::from_millis(200));
                }
            }
            self.reap();
        }
        for s in self.sessions.lock().unwrap().values() {
            s.closed.store(true, Ordering::SeqCst);
        }
    }

    fn on_datagram(self: &Arc<Self>, send: &UdpSocket, addr: SocketAddr, data: &[u8]) {
        let session = {
            let mut map = self.sessions.lock().unwrap();
            match map.get(&addr) {
                Some(s) => Arc::clone(s),
                None => {
                    let up = match UdpSocket::bind("127.0.0.1:0") {
                        Ok(u) => u,
                        Err(e) => {
                            crate::log_warn!("SRT: не открыть upstream-сокет: {}", e);
                            return;
                        }
                    };
                    let _ = up.set_read_timeout(Some(Duration::from_millis(300)));
                    let s = Arc::new(Session {
                        client: addr,
                        up,
                        last_seen: Mutex::new(Instant::now()),
                        stream: Mutex::new(None),
                        generation: AtomicU64::new(0),
                        publishing: AtomicBool::new(false),
                        graceful: AtomicBool::new(false),
                        closed: AtomicBool::new(false),
                    });
                    // Обратное направление: MediaMTX -> клиент.
                    let me = Arc::clone(self);
                    let back_session = Arc::clone(&s);
                    let send2 = match send.try_clone() {
                        Ok(x) => x,
                        Err(_) => return,
                    };
                    thread::Builder::new()
                        .name("srt-back".into())
                        .spawn(move || me.backward(&back_session, &send2))
                        .ok();
                    map.insert(addr, Arc::clone(&s));
                    s
                }
            }
        };

        session.touch();

        if let Some(sid) = stream_id_from_handshake(data) {
            if let Some(parsed) = parse_stream_id(&sid) {
                let mut slot = session.stream.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(parsed.clone());
                    drop(slot);
                    if parsed.publish && parsed.path == self.live_path {
                        let gen = self.generations.fetch_add(1, Ordering::SeqCst) + 1;
                        session.generation.store(gen, Ordering::SeqCst);
                        session.publishing.store(true, Ordering::SeqCst);
                        crate::log_info!("[{}] SRT-публикация на {}", addr.ip(), parsed.path);
                        let _ = self.events.send(Event {
                            kind: EventKind::Publish,
                            gen,
                        });
                    } else if parsed.publish {
                        crate::log_info!(
                            "[{}] SRT-публикация на чужой путь — не отслеживаю",
                            addr.ip()
                        );
                    }
                }
            }
        }

        if is_shutdown(data)
            && session.publishing.load(Ordering::SeqCst)
            && !session.graceful.swap(true, Ordering::SeqCst)
        {
            crate::log_info!("[{}] SRT: штатное завершение трансляции (SHUTDOWN)", addr.ip());
            let _ = self.events.send(Event {
                kind: EventKind::End,
                gen: session.generation.load(Ordering::SeqCst),
            });
        }

        if session.up.send_to(data, &self.upstream).is_err() {
            session.closed.store(true, Ordering::SeqCst);
        }
    }

    fn backward(self: &Arc<Self>, s: &Arc<Session>, send: &UdpSocket) {
        let mut buf = vec![0u8; MAX_PKT];
        while !s.closed.load(Ordering::SeqCst) && crate::sys::running() {
            match s.up.recv_from(&mut buf) {
                Ok((n, _)) => {
                    if send.send_to(&buf[..n], s.client).is_err() {
                        break;
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(_) => break,
            }
        }
    }

    /// Закрывает сессии, замолчавшие дольше `stall`.
    fn reap(self: &Arc<Self>) {
        let stale: Vec<Arc<Session>> = {
            let map = self.sessions.lock().unwrap();
            map.values()
                .filter(|s| s.idle() > self.stall)
                .cloned()
                .collect()
        };
        for s in stale {
            self.sessions.lock().unwrap().remove(&s.client);
            s.closed.store(true, Ordering::SeqCst);

            if s.publishing.load(Ordering::SeqCst) && !s.graceful.load(Ordering::SeqCst) {
                crate::log_info!(
                    "[{}] SRT: тишина {:.1} с — считаю обрывом связи",
                    s.client.ip(),
                    s.idle().as_secs_f64()
                );
                // Отпускаем путь в MediaMTX: иначе он держит сессию и не примет
                // вернувшийся телефон.
                let _ = s.up.send_to(&shutdown_packet(), &self.upstream);
                let _ = self.events.send(Event {
                    kind: EventKind::Drop,
                    gen: s.generation.load(Ordering::SeqCst),
                });
            }
        }
    }
}

/// Пакет SHUTDOWN от имени клиента: освобождает путь в MediaMTX.
fn shutdown_packet() -> Vec<u8> {
    let mut p = vec![0u8; 20];
    p[0] = 0x80; // управляющий
    p[1] = CTRL_SHUTDOWN as u8;
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_publish_prefix() {
        let s = parse_stream_id("publish:live/devkey").unwrap();
        assert_eq!(s.path, "live/devkey");
        assert!(s.publish);
    }

    #[test]
    fn parses_leading_slash() {
        let s = parse_stream_id("publish:/live/devkey/").unwrap();
        assert_eq!(s.path, "live/devkey");
    }

    #[test]
    fn parses_access_control_form() {
        let s = parse_stream_id("#!::r=live/devkey,m=publish").unwrap();
        assert_eq!(s.path, "live/devkey");
        assert!(s.publish);
        let s2 = parse_stream_id("#!::m=request,r=live/devkey").unwrap();
        assert_eq!(s2.path, "live/devkey");
        assert!(!s2.publish);
    }

    #[test]
    fn plain_path_is_publish() {
        let s = parse_stream_id("live/devkey").unwrap();
        assert_eq!(s.path, "live/devkey");
        assert!(s.publish);
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_stream_id("").is_none());
        assert!(parse_stream_id("publish:").is_none());
        assert!(parse_stream_id("#!::m=publish").is_none());
    }

    #[test]
    fn decodes_sid_words() {
        // libsrt укладывает строку в uint32 и отдаёт каждое слово little-endian:
        // "publ" превращается в байты 'l','b','u','p'.
        let mut v = Vec::new();
        for w in ["publ", "ish:", "live", "/dev", "key\0"] {
            // В сети каждое слово идёт в обратном порядке байт.
            let mut b = w.as_bytes().to_vec();
            b.reverse();
            v.extend_from_slice(&b);
        }
        assert_eq!(decode_sid(&v), "publish:live/devkey");
    }

    #[test]
    fn extracts_sid_from_real_handshake() {
        // Реальный handshake от ffmpeg (libsrt): publication с publish:live/devkey
        let hex = "8000000000000000000003160000000000000005000000051fc589b5000005dc00002000ffffffff30b5db96d6dc0c910100007f0000000000000000000000000001000300010504000000bf00780000000500056c6275703a6873696576696c7665642f0079656b";
        let mut data = Vec::new();
        let mut i = 0;
        while i + 2 <= hex.len() {
            data.push(u8::from_str_radix(&hex[i..i + 2], 16).unwrap());
            i += 2;
        }
        let sid = stream_id_from_handshake(&data).unwrap();
        assert_eq!(sid, "publish:live/devkey");
        assert_eq!(parse_stream_id(&sid).unwrap().path, "live/devkey");
    }

    #[test]
    fn detects_shutdown() {
        let mut p = vec![0u8; 20];
        p[0] = 0x80;
        p[1] = 5;
        assert!(is_shutdown(&p));
        p[1] = 3; // NAK, не закрытие
        assert!(!is_shutdown(&p));
    }

    #[test]
    fn ignores_short_packets() {
        assert!(stream_id_from_handshake(&[]).is_none());
        assert!(stream_id_from_handshake(&[0x80, 0x00]).is_none());
        assert!(!is_shutdown(&[0x80]));
    }
}
