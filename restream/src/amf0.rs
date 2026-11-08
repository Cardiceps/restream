//! Разбор AMF0 — ровно столько, сколько нужно для командных сообщений RTMP.

const T_NUMBER: u8 = 0x00;
const T_BOOL: u8 = 0x01;
const T_STRING: u8 = 0x02;
const T_OBJECT: u8 = 0x03;
const T_NULL: u8 = 0x05;
const T_UNDEFINED: u8 = 0x06;
const T_ECMA_ARRAY: u8 = 0x08;
const T_END: u8 = 0x09;
const T_ARRAY: u8 = 0x0A;
const T_DATE: u8 = 0x0B;
const T_LONG_STRING: u8 = 0x0C;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Number(f64),
    Bool(bool),
    Str(String),
    Object(Vec<(String, Value)>),
    Array(Vec<Value>),
    Null,
    Undefined,
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8], i: usize) -> Self {
        Reader { b, i }
    }

    fn done(&self) -> bool {
        self.i >= self.b.len()
    }

    fn byte(&mut self) -> Option<u8> {
        let v = *self.b.get(self.i)?;
        self.i += 1;
        Some(v)
    }

    fn u16(&mut self) -> Option<u16> {
        let s = self.take(2)?;
        Some(u16::from_be_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        let s = self.take(4)?;
        Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn f64(&mut self) -> Option<f64> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Some(f64::from_be_bytes(a))
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.b.len().saturating_sub(self.i) {
            return None;
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Some(s)
    }
}

fn lossy(s: &[u8]) -> String {
    String::from_utf8_lossy(s).into_owned()
}

fn value(r: &mut Reader, depth: u8) -> Option<Value> {
    if depth > 8 {
        return None;
    }
    let tag = r.byte()?;
    match tag {
        T_NUMBER => r.f64().map(Value::Number),
        T_BOOL => r.byte().map(|b| Value::Bool(b != 0)),
        T_STRING => {
            let n = r.u16()? as usize;
            r.take(n).map(|s| Value::Str(lossy(s)))
        }
        T_LONG_STRING => {
            let n = r.u32()? as usize;
            r.take(n).map(|s| Value::Str(lossy(s)))
        }
        T_OBJECT | T_ECMA_ARRAY => {
            if tag == T_ECMA_ARRAY {
                r.u32()?;
            }
            let mut kv = Vec::new();
            loop {
                let n = r.u16()? as usize;
                if n == 0 {
                    // Пустое имя: либо конец объекта, либо странный пустой ключ.
                    let save = r.i;
                    if r.byte() == Some(T_END) {
                        return Some(Value::Object(kv));
                    }
                    r.i = save;
                    return Some(Value::Object(kv));
                }
                let name = lossy(r.take(n)?);
                let v = value(r, depth + 1)?;
                kv.push((name, v));
                if kv.len() > 64 {
                    return Some(Value::Object(kv));
                }
            }
        }
        T_NULL => Some(Value::Null),
        T_UNDEFINED => Some(Value::Undefined),
        T_DATE => {
            r.take(10)?;
            Some(Value::Null)
        }
        T_ARRAY => {
            let n = r.u32()? as usize;
            let mut items = Vec::new();
            for _ in 0..n.min(32) {
                match value(r, depth + 1) {
                    Some(v) => items.push(v),
                    None => break,
                }
            }
            Some(Value::Array(items))
        }
        _ => None,
    }
}

/// Разбирает последовательность AMF0-значений. Останавливается на первом непонятном
/// байте — этого достаточно: имя команды всегда идёт первым.
pub fn parse_values(payload: &[u8], amf3: bool) -> Vec<Value> {
    let mut start = 0;
    if amf3 {
        if payload.first() != Some(&0x00) {
            return Vec::new();
        }
        start = 1;
    }
    let mut r = Reader::new(payload, start);
    let mut out = Vec::new();
    while !r.done() && out.len() < 12 {
        let save = r.i;
        match value(&mut r, 0) {
            Some(v) => out.push(v),
            None => {
                r.i = save;
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect_command() {
        // "connect", 1, {app: "live"}
        let mut p = vec![T_STRING, 0x00, 0x07];
        p.extend_from_slice(b"connect");
        p.push(T_NUMBER);
        p.extend_from_slice(&1.0f64.to_be_bytes());
        p.push(T_OBJECT);
        p.extend_from_slice(&[0x00, 0x03]);
        p.extend_from_slice(b"app");
        p.push(T_STRING);
        p.extend_from_slice(&[0x00, 0x04]);
        p.extend_from_slice(b"live");
        p.extend_from_slice(&[0x00, 0x00, T_END]);

        let v = parse_values(&p, false);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].as_str(), Some("connect"));
        assert_eq!(v[1], Value::Number(1.0));
        assert_eq!(v[2].get("app").and_then(|x| x.as_str()), Some("live"));
    }

    #[test]
    fn parses_short_command() {
        // "FCUnpublish", 3
        let mut p = vec![T_STRING, 0x00, 0x0b];
        p.extend_from_slice(b"FCUnpublish");
        p.push(T_NUMBER);
        p.extend_from_slice(&3.0f64.to_be_bytes());
        let v = parse_values(&p, false);
        assert_eq!(v[0].as_str(), Some("FCUnpublish"));
        assert_eq!(v[1], Value::Number(3.0));
    }

    #[test]
    fn empty_payload_is_empty() {
        assert!(parse_values(&[], false).is_empty());
    }

    #[test]
    fn truncated_is_survivable() {
        // обрезанная строка: длина больше, чем данных
        let p = vec![T_STRING, 0x00, 0x40, b'a'];
        assert!(parse_values(&p, false).is_empty());
    }

    #[test]
    fn garbage_does_not_panic() {
        for byte in 0u8..=255 {
            let _ = parse_values(&[byte], false);
            let _ = parse_values(&[byte, 0x00, 0x00], false);
        }
    }

    #[test]
    fn amf3_skips_marker() {
        let mut p = vec![0x00, T_STRING, 0x00, 0x04];
        p.extend_from_slice(b"stop");
        let v = parse_values(&p, true);
        assert_eq!(v[0].as_str(), Some("stop"));
    }
}
