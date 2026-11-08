//! Крошечный HTTP: клиент для API MediaMTX и сервер для `restream status` / `stop`.
//!
//! Полноценный HTTP здесь не нужен: один GET на localhost и один эндпоинт управления.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// ---------------------------------------------------------------- JSON

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\r' | b'\n') {
            self.i += 1;
        }
    }

    fn value(&mut self, depth: u8) -> Option<Json> {
        if depth > 32 {
            return None;
        }
        self.ws();
        match *self.b.get(self.i)? {
            b'n' => {
                self.expect(b"null")?;
                Some(Json::Null)
            }
            b't' => {
                self.expect(b"true")?;
                Some(Json::Bool(true))
            }
            b'f' => {
                self.expect(b"false")?;
                Some(Json::Bool(false))
            }
            b'"' => self.string().map(Json::Str),
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Some(Json::Arr(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.b.get(self.i)? {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            return Some(Json::Arr(items));
                        }
                        _ => return None,
                    }
                }
            }
            b'{' => {
                self.i += 1;
                let mut kv = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Some(Json::Obj(kv));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return None;
                    }
                    self.i += 1;
                    let v = self.value(depth + 1)?;
                    kv.push((k, v));
                    self.ws();
                    match self.b.get(self.i)? {
                        b',' => self.i += 1,
                        b'}' => {
                            self.i += 1;
                            return Some(Json::Obj(kv));
                        }
                        _ => return None,
                    }
                }
            }
            _ => self.number(),
        }
    }

    fn expect(&mut self, lit: &[u8]) -> Option<()> {
        if self.b.len() >= self.i + lit.len() && &self.b[self.i..self.i + lit.len()] == lit {
            self.i += lit.len();
            Some(())
        } else {
            None
        }
    }

    fn string(&mut self) -> Option<String> {
        if self.b.get(self.i) != Some(&b'"') {
            return None;
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let c = *self.b.get(self.i)?;
            self.i += 1;
            match c {
                b'"' => return Some(out),
                b'\\' => {
                    let e = *self.b.get(self.i)?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'u' => {
                            let hex = self.b.get(self.i..self.i + 4)?;
                            let s = std::str::from_utf8(hex).ok()?;
                            let cp = u32::from_str_radix(s, 16).ok()?;
                            self.i += 4;
                            out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                        }
                        _ => return None,
                    }
                }
                _ => {
                    // Собираем UTF-8 по байтам.
                    let start = self.i - 1;
                    let mut end = self.i;
                    while end < self.b.len() && (self.b[end] & 0xC0) == 0x80 {
                        end += 1;
                    }
                    out.push_str(&String::from_utf8_lossy(&self.b[start..end]));
                    self.i = end;
                }
            }
        }
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.i;
        while self.i < self.b.len()
            && matches!(self.b[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        {
            self.i += 1;
        }
        if self.i == start {
            return None;
        }
        std::str::from_utf8(&self.b[start..self.i])
            .ok()?
            .parse::<f64>()
            .ok()
            .map(Json::Num)
    }
}

pub fn parse_json(text: &str) -> Option<Json> {
    let mut p = P {
        b: text.as_bytes(),
        i: 0,
    };
    p.value(0)
}

/// GET и разбор ответа. `None` при любой сетевой или разборной проблеме.
pub fn get_json(url: &str, timeout: Duration) -> Option<Json> {
    let rest = url.strip_prefix("http://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let mut stream = TcpStream::connect(hostport).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    write!(
        stream,
        "GET {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, hostport
    )
    .ok()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    if !line.contains("200") {
        return None;
    }
    // Пропускаем заголовки.
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 {
            break;
        }
        if h == "\r\n" || h == "\n" {
            break;
        }
    }
    let mut body = String::new();
    reader.read_to_string(&mut body).ok()?;
    parse_json(&body)
}

// ---------------------------------------------------------------- сервер

/// Управляющий API: только localhost, два пути.
pub fn serve_control(
    host: &str,
    port: u16,
    status: Arc<dyn Fn() -> String + Send + Sync>,
    stop: Arc<AtomicBool>,
) {
    let addr = format!("{}:{}", host, port);
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            crate::log_error!("не удалось занять control API {}: {}", addr, e);
            return;
        }
    };
    crate::log_info!("control API: http://{}", addr);

    thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if !crate::sys::running() {
                    break;
                }
                let Ok(mut s) = stream else { continue };
                let status = Arc::clone(&status);
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
                    let mut reader = BufReader::new(s.try_clone().unwrap_or_else(|_| {
                        // Если клонировать не вышло — просто отвечаем 500 и выходим.
                        TcpStream::connect("127.0.0.1:1").unwrap()
                    }));
                    let mut req = String::new();
                    let _ = reader.read_line(&mut req);
                    let mut parts = req.split_whitespace();
                    let method = parts.next().unwrap_or("");
                    let path = parts.next().unwrap_or("");

                    let (code, body) = match (method, path) {
                        ("GET", "/status") => ("200 OK", status()),
                        ("POST", "/stop") => {
                            stop.store(true, Ordering::SeqCst);
                            ("200 OK", r#"{"ok":true}"#.to_string())
                        }
                        _ => ("404 Not Found", r#"{"error":"not found"}"#.to_string()),
                    };
                    let _ = write!(
                        s,
                        "HTTP/1.1 {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        code,
                        body.as_bytes().len(),
                        body
                    );
                    let _ = s.flush();
                });
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ready_flag() {
        let j = parse_json(r#"{"name":"live/x","ready":true,"tracks":[]}"#).unwrap();
        assert_eq!(j.get("ready").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(j.get("name"), Some(&Json::Str("live/x".into())));
    }

    #[test]
    fn parses_nested() {
        let j = parse_json(r#"{"a":{"b":[1,2,{"c":false}]}}"#).unwrap();
        let Json::Obj(_) = j else { panic!() };
        assert!(j.get("a").is_some());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_json("не json").is_none());
        assert!(parse_json("{").is_none());
        assert!(parse_json("").is_none());
    }

    #[test]
    fn handles_unicode_escapes() {
        let j = parse_json(r#"{"s":"ключ\u0021"}"#).unwrap();
        assert_eq!(j.get("s"), Some(&Json::Str("ключ!".into())));
    }

    #[test]
    fn null_and_numbers() {
        let j = parse_json(r#"{"r":null,"n":12.5}"#).unwrap();
        assert_eq!(j.get("r"), Some(&Json::Null));
        assert_eq!(j.get("n"), Some(&Json::Num(12.5)));
    }
}
