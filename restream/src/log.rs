//! Логирование в stdout: одна строка на событие, удобно и для journald, и для ручного запуска.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static QUIET_FFMPEG: AtomicBool = AtomicBool::new(false);

/// UTC-время в виде `2026-11-08 05:57:23 UTC`.
fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Дни от 1970-01-01 -> (год, месяц, день). Алгоритм Говарда Хиннанта.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn emit(level: &str, msg: &str) {
    println!("{} {:5} {}", stamp(), level, msg);
}

pub fn info(msg: &str) {
    emit("INFO", msg)
}

pub fn warn(msg: &str) {
    emit("WARN", msg)
}

pub fn error(msg: &str) {
    emit("ERROR", msg)
}

/// Поток вывода ffmpeg бывает очень шумным: приглушаем его, если задано в конфиге.
pub fn set_ffmpeg_quiet(quiet: bool) {
    QUIET_FFMPEG.store(quiet, Ordering::Relaxed);
}

pub fn ffmpeg_quiet() -> bool {
    QUIET_FFMPEG.load(Ordering::Relaxed)
}

macro_rules! log_info {
    ($($t:tt)*) => { $crate::log::info(&format!($($t)*)) };
}
macro_rules! log_warn {
    ($($t:tt)*) => { $crate::log::warn(&format!($($t)*)) };
}
macro_rules! log_error {
    ($($t:tt)*) => { $crate::log::error(&format!($($t)*)) };
}

pub(crate) use {log_error, log_info, log_warn};

#[cfg(test)]
mod tests {
    use super::civil_from_days;

    #[test]
    fn epoch_is_1970() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn known_days() {
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
    }

    #[test]
    fn leap_day_2000() {
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }
}
