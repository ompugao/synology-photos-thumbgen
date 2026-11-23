use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};

pub const LEVEL_DEBUG: u8 = 1;
pub const LEVEL_INFO: u8 = 2;
pub const LEVEL_WARN: u8 = 3;
pub const LEVEL_ERROR: u8 = 4;

static LOG_LEVEL: AtomicU8 = AtomicU8::new(LEVEL_INFO);

pub fn init_logger(level: &str) {
    let parsed = match level.trim().to_ascii_uppercase().as_str() {
        "DEBUG" => LEVEL_DEBUG,
        "WARN" | "WARNING" => LEVEL_WARN,
        "ERROR" => LEVEL_ERROR,
        _ => LEVEL_INFO,
    };
    LOG_LEVEL.store(parsed, Ordering::Relaxed);
}

#[inline]
pub fn enabled(level: u8) -> bool {
    level >= LOG_LEVEL.load(Ordering::Relaxed)
}

fn format_timestamp_now() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return format!("{now}");
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

pub fn emit(level: u8, label: &str, args: fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    eprintln!("{} [{}] synology_thumbgen: {}", format_timestamp_now(), label, args);
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::LEVEL_DEBUG, "DEBUG", format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::LEVEL_INFO, "INFO", format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::LEVEL_WARN, "WARNING", format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::LEVEL_ERROR, "ERROR", format_args!($($arg)*))
    };
}
