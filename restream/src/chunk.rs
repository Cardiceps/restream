//! Разбор потока RTMP-чанков клиент→сервер.
//!
//! Задача узкая: вытащить командные сообщения (`connect`, `publish`, `FCUnpublish` …),
//! чтобы отличить штатное завершение публикации от обрыва. Тела аудио и видео не
//! копируются — они только пропускаются.

use crate::amf0::{self, Value};
use std::collections::HashMap;

/// Рукопожатие: C0 (1) + C1 (1536) + C2 (1536).
pub const CLIENT_HANDSHAKE: usize = 1 + 1536 + 1536;

const MSG_SET_CHUNK_SIZE: u8 = 1;
const MSG_COMMAND_AMF3: u8 = 17;
const MSG_COMMAND_AMF0: u8 = 20;
/// Разумный предел на одно сообщение: защита от мусора в заголовке.
const MAX_MSG: usize = 4 << 20;

#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub name: String,
    pub values: Vec<Value>,
}

impl Command {
    /// Путь публикации: `app` из `connect` + значение из `publish`.
    pub fn publish_path(&self, app: &str) -> String {
        let raw = self
            .values
            .iter()
            .skip(1)
            .find_map(|v| v.as_str().filter(|s| *s != "publish"))
            .unwrap_or("");
        if raw.contains('/') {
            raw.trim_matches('/').to_string()
        } else if app.is_empty() {
            raw.trim_matches('/').to_string()
        } else {
            format!("{}/{}", app.trim_matches('/'), raw.trim_matches('/'))
        }
    }

    pub fn app(&self) -> Option<&str> {
        self.values
            .iter()
            .find_map(|v| v.get("app").and_then(|a| a.as_str()))
    }
}

#[derive(Default, Clone)]
struct Stream {
    ts: u32,
    len: usize,
    msg_type: u8,
    ext: bool,
    done: bool,
    recv: usize,
    data: Vec<u8>,
}

enum Step {
    /// Разобран чанк, курсор сдвинулся на n.
    Done(usize),
    /// Данных пока не хватает.
    Need,
    /// Поток не разобрать: отключаем детект, пересылка продолжается.
    Broken,
}

pub struct TapParser {
    buf: Vec<u8>,
    skip: usize,
    chunk_size: usize,
    streams: HashMap<u32, Stream>,
    broken: bool,
    warned: bool,
}

impl TapParser {
    #[cfg(test)]
    pub fn broken(&self) -> bool {
        self.broken
    }

    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(64 * 1024),
            skip: CLIENT_HANDSHAKE,
            chunk_size: 128,
            streams: HashMap::new(),
            broken: false,
            warned: false,
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<Command> {
        if self.broken {
            return Vec::new();
        }
        self.buf.extend_from_slice(data);

        if self.skip > 0 {
            if self.buf.len() < self.skip {
                return Vec::new();
            }
            self.buf.drain(..self.skip);
            self.skip = 0;
        }

        // Забираем буфер локально: так поля `buf` и `streams` не конфликтуют по заимствованию.
        let mut buf = std::mem::take(&mut self.buf);
        let mut out = Vec::new();
        let mut i = 0usize;
        loop {
            match self.step(&buf, i, &mut out) {
                Step::Done(n) => i = n,
                Step::Need => break,
                Step::Broken => {
                    self.broken = true;
                    break;
                }
            }
        }
        if i > 0 {
            buf.drain(..i);
        }
        self.buf = buf;
        if self.broken && !self.warned {
            self.warned = true;
            crate::log_warn!("не удалось разобрать RTMP-поток: опознание завершения отключено");
        }
        out
    }

    fn step(&mut self, buf: &[u8], at: usize, out: &mut Vec<Command>) -> Step {
        let avail = buf.len() - at;
        if avail == 0 {
            return Step::Need;
        }
        let b0 = buf[at];
        let fmt = b0 >> 6;
        let mut csid = (b0 & 0x3f) as u32;
        let mut i = at + 1;

        if csid == 0 {
            if i - at > avail - 1 + 1 && avail < 2 {
                return Step::Need;
            }
            if i >= buf.len() {
                return Step::Need;
            }
            csid = 64 + buf[i] as u32;
            i += 1;
        } else if csid == 1 {
            if i + 1 >= buf.len() + 1 && avail < 3 {
                return Step::Need;
            }
            if i + 1 >= buf.len() {
                return Step::Need;
            }
            csid = 64 + buf[i] as u32 + (buf[i + 1] as u32) * 256;
            i += 2;
        }

        // Новое сообщение начинается, если это первый чанк потока, чанк с новым
        // заголовком (fmt < 3) или предыдущее сообщение уже собрано.
        let known = self.streams.get(&csid);
        // Чанк типа 3 продолжает уже начатое сообщение: без предыдущего состояния
        // он бессмыслен, а значит поток не разобрать.
        if known.is_none() && fmt == 3 {
            return Step::Broken;
        }
        let new_msg = match known {
            None => true,
            Some(s) => fmt < 3 || s.done,
        };
        if new_msg {
            let prev = self.streams.get(&csid).cloned().unwrap_or_default();
            self.streams.insert(
                csid,
                Stream {
                    ts: prev.ts,
                    len: prev.len,
                    msg_type: prev.msg_type,
                    ext: prev.ext,
                    done: false,
                    recv: 0,
                    data: Vec::new(),
                },
            );
        }
        let Some(st) = self.streams.get_mut(&csid) else {
            return Step::Broken;
        };

        // ---- заголовок чанка
        match fmt {
            0 => {
                if i + 11 > buf.len() {
                    return Step::Need;
                }
                let ts = be24(&buf[i..i + 3]);
                st.len = be24(&buf[i + 3..i + 6]);
                st.msg_type = buf[i + 6];
                i += 11;
                st.ext = ts == 0xff_ffff;
                if st.ext {
                    if i + 4 > buf.len() {
                        return Step::Need;
                    }
                    i += 4;
                }
                st.ts = ts as u32;
            }
            1 => {
                if i + 7 > buf.len() {
                    return Step::Need;
                }
                let ts = be24(&buf[i..i + 3]);
                st.len = be24(&buf[i + 3..i + 6]);
                st.msg_type = buf[i + 6];
                i += 7;
                st.ext = ts == 0xff_ffff;
                if st.ext {
                    if i + 4 > buf.len() {
                        return Step::Need;
                    }
                    i += 4;
                }
                st.ts = ts as u32;
            }
            2 => {
                if i + 3 > buf.len() {
                    return Step::Need;
                }
                let ts = be24(&buf[i..i + 3]);
                i += 3;
                st.ext = ts == 0xff_ffff;
                if st.ext {
                    if i + 4 > buf.len() {
                        return Step::Need;
                    }
                    i += 4;
                }
            }
            _ => {
                if st.ext {
                    if i + 4 > buf.len() {
                        return Step::Need;
                    }
                    i += 4;
                }
            }
        }

        if st.len > MAX_MSG {
            return Step::Broken;
        }

        let keep = st.msg_type == MSG_COMMAND_AMF0
            || st.msg_type == MSG_COMMAND_AMF3
            || st.msg_type == MSG_SET_CHUNK_SIZE;
        let remaining = st.len.saturating_sub(st.recv);
        let take = remaining.min(self.chunk_size);
        if i + take > buf.len() {
            return Step::Need;
        }
        if take > 0 && keep {
            st.data.extend_from_slice(&buf[i..i + take]);
        }
        i += take;
        st.recv += take;

        if st.recv < st.len {
            return Step::Done(i);
        }

        // ---- сообщение собрано
        st.done = true;
        let payload = std::mem::take(&mut st.data);
        let msg_type = st.msg_type;

        if msg_type == MSG_SET_CHUNK_SIZE && payload.len() >= 4 {
            let size = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                & 0x7fff_ffff;
            if (128..=0xff_ffff).contains(&size) {
                self.chunk_size = size as usize;
            }
        } else if msg_type == MSG_COMMAND_AMF0 || msg_type == MSG_COMMAND_AMF3 {
            if let Some(cmd) = parse_command(&payload, msg_type == MSG_COMMAND_AMF3) {
                out.push(cmd);
            }
        }

        Step::Done(i)
    }
}

fn parse_command(payload: &[u8], amf3: bool) -> Option<Command> {
    let values = amf0::parse_values(payload, amf3);
    let name = values.first()?.as_str()?.to_string();
    Some(Command { name, values })
}

fn be24(s: &[u8]) -> usize {
    ((s[0] as usize) << 16) | ((s[1] as usize) << 8) | s[2] as usize
}

impl Default for TapParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Собирает чанк с полным заголовком (fmt=0) для одного сообщения.
    fn full_chunk(csid: u8, msg_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![csid];
        v.extend_from_slice(&[0, 0, 0]); // timestamp
        v.extend_from_slice(&[
            (payload.len() >> 16) as u8,
            (payload.len() >> 8) as u8,
            payload.len() as u8,
        ]);
        v.push(msg_type);
        v.extend_from_slice(&[0, 0, 0, 0]); // stream id
        v.extend_from_slice(payload);
        v
    }

    fn cmd_payload(name: &str) -> Vec<u8> {
        let mut p = vec![0x02, 0x00, name.len() as u8];
        p.extend_from_slice(name.as_bytes());
        p.push(0x00);
        p.extend_from_slice(&3.0f64.to_be_bytes());
        p
    }

    #[test]
    fn skips_handshake_then_reads_command() {
        let mut parser = TapParser::new();
        let mut data = vec![0u8; CLIENT_HANDSHAKE];
        data.extend(full_chunk(3, MSG_COMMAND_AMF0, &cmd_payload("FCUnpublish")));
        let cmds = parser.feed(&data);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "FCUnpublish");
    }

    #[test]
    fn partial_feed_is_reassembled() {
        let mut parser = TapParser::new();
        let mut data = vec![0u8; CLIENT_HANDSHAKE];
        data.extend(full_chunk(3, MSG_COMMAND_AMF0, &cmd_payload("deleteStream")));
        let mid = data.len() / 2;
        assert!(parser.feed(&data[..mid]).is_empty());
        let cmds = parser.feed(&data[mid..]);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "deleteStream");
    }

    #[test]
    fn chunk_size_change_is_honoured() {
        let mut parser = TapParser::new();
        let mut data = vec![0u8; CLIENT_HANDSHAKE];
        // SetChunkSize = 4096
        data.extend(full_chunk(2, MSG_SET_CHUNK_SIZE, &4096u32.to_be_bytes()));
        // одно большое командное сообщение (больше 128 байт) — должно собраться без
        // дополнительных чанков, раз размер чанка уже 4096
        let mut big = cmd_payload("publish");
        big.resize(1000, 0);
        data.extend(full_chunk(3, MSG_COMMAND_AMF0, &big));
        let cmds = parser.feed(&data);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "publish");
    }

    #[test]
    fn publish_path_joins_app() {
        let cmd = Command {
            name: "publish".into(),
            values: vec![
                Value::Str("publish".into()),
                Value::Str("devkey".into()),
            ],
        };
        assert_eq!(cmd.publish_path("live"), "live/devkey");
        // если путь уже полный — не дублируем app
        let cmd2 = Command {
            name: "publish".into(),
            values: vec![
                Value::Str("publish".into()),
                Value::Str("live/devkey".into()),
            ],
        };
        assert_eq!(cmd2.publish_path("live"), "live/devkey");
    }

    #[test]
    fn command_app_extracted() {
        let cmd = Command {
            name: "connect".into(),
            values: vec![
                Value::Str("connect".into()),
                Value::Number(1.0),
                Value::Object(vec![("app".into(), Value::Str("live".into()))]),
            ],
        };
        assert_eq!(cmd.app(), Some("live"));
    }

    #[test]
    fn garbage_is_bounded() {
        let mut parser = TapParser::new();
        let mut data = vec![0u8; CLIENT_HANDSHAKE];
        data.extend(std::iter::repeat(0xffu8).take(4096));
        let _ = parser.feed(&data);
        assert!(parser.broken());
    }
}
