//! Обработка сигналов завершения без внешних зависимостей.
//!
//! std не умеет вешать обработчики сигналов, поэтому объявляем `signal(2)` из libc
//! напрямую: libc и так слинкована со std. Обработчик только взводит атомарный флаг
//! (это async-signal-safe), а вся уборка делается в обычном коде.

use std::sync::atomic::{AtomicBool, Ordering};

/// `false` — пора завершаться.
pub static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

const SIGINT: i32 = 2;
const SIGPIPE: i32 = 13;
const SIGTERM: i32 = 15;
const SIG_IGN: usize = 1;

extern "C" fn on_stop(_sig: i32) {
    RUNNING.store(false, Ordering::SeqCst);
}

pub fn install_handlers() {
    // Rust и сам игнорирует SIGPIPE, но для дочерних процессов это важно выставить явно:
    // иначе ffmpeg может умереть от записи в закрытый сокет.
    unsafe {
        signal(SIGINT, on_stop as usize);
        signal(SIGTERM, on_stop as usize);
        signal(SIGPIPE, SIG_IGN);
    }
}

pub fn running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}
