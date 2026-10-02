//! Minimal, dependency-free logger with an in-memory ring buffer.
//!
//! `shellctl logs` reads the ring buffer over IPC rather than tailing a file, so
//! the shell can keep its log small and only persist it when something goes wrong.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        Some(match s.to_ascii_lowercase().as_str() {
            "trace" => Level::Trace,
            "debug" => Level::Debug,
            "info" => Level::Info,
            "warn" | "warning" => Level::Warn,
            "error" => Level::Error,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Record {
    pub level: Level,
    pub target: String,
    pub message: String,
    /// Milliseconds since the Unix epoch, so `shellctl logs` can interleave runs.
    pub ts_ms: u64,
}

impl Record {
    pub fn format(&self) -> String {
        format!("[{}] {:<5} {}: {}", self.stamp(), self.level.label(), self.target, self.message)
    }

    /// `HH:MM:SS.mmm` in local time.
    fn stamp(&self) -> String {
        let secs = self.ts_ms / 1000;
        let ms = self.ts_ms % 1000;
        let days = secs / 86_400;
        let tod = secs % 86_400;
        let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
        format!("{:>3}d {:02}:{:02}:{:02}.{:03}", days, h, m, s, ms)
    }
}

struct State {
    level: Level,
    ring: VecDeque<Record>,
    capacity: usize,
    /// Mirror to stderr. Off by default once the shell owns the screen.
    console: bool,
    /// Mirror to a file, enabled by `SLOWSHELL_LOG_FILE`.
    file: Option<std::fs::File>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State {
            level: std::env::var("SLOWSHELL_LOG")
                .ok()
                .and_then(|v| Level::parse(&v))
                .unwrap_or(Level::Info),
            ring: VecDeque::with_capacity(2048),
            capacity: 4096,
            console: std::env::var("SLOWSHELL_LOG").is_ok(),
            file: std::env::var("SLOWSHELL_LOG_FILE").ok().and_then(|p| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .ok()
            }),
        })
    })
}

/// Set the minimum level at runtime, used by `shellctl logs --level`.
pub fn set_level(level: Level) {
    if let Ok(mut s) = state().lock() {
        s.level = level;
    }
}

pub fn level() -> Level {
    state().lock().map(|s| s.level).unwrap_or(Level::Info)
}

pub fn log(level: Level, target: &str, message: impl Into<String>) {
    let record = Record {
        level,
        target: target.to_string(),
        message: message.into(),
        ts_ms: now_ms(),
    };
    let mut s = match state().lock() {
        Ok(s) => s,
        // A poisoned log mutex must never take down the shell.
        Err(poisoned) => poisoned.into_inner(),
    };
    if record.level < s.level {
        return;
    }
    if s.console {
        let _ = writeln!(std::io::stderr(), "{}", record.format());
    }
    if let Some(f) = s.file.as_mut() {
        let _ = writeln!(f, "{}", record.format());
    }
    s.ring.push_back(record);
    while s.ring.len() > s.capacity {
        s.ring.pop_front();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Most recent records, newest last. `limit` caps the returned count.
pub fn recent(limit: usize, min_level: Level) -> Vec<Record> {
    let Ok(s) = state().lock() else { return Vec::new() };
    s.ring
        .iter()
        .filter(|r| r.level >= min_level)
        .rev()
        .take(limit)
        .cloned()
        .collect::<VecDeque<_>>()
        .into_iter()
        .rev()
        .collect()
}

#[macro_export]
macro_rules! log_at {
    ($lvl:expr, $($arg:tt)*) => {
        $crate::log::log($lvl, module_path!(), format!($($arg)*))
    };
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Trace, $($arg)*) };
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Debug, $($arg)*) };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Info, $($arg)*) };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Warn, $($arg)*) };
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Error, $($arg)*) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_order_correctly() {
        assert!(Level::Error > Level::Warn);
        assert!(Level::Trace < Level::Info);
    }

    #[test]
    fn round_trips_and_filters() {
        set_level(Level::Trace);
        log(Level::Debug, "test", "a debug line");
        log(Level::Error, "test", "an error line");
        let r = recent(10, Level::Error);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].message, "an error line");
        let all = recent(10, Level::Trace);
        assert!(all.len() >= 2);
        set_level(Level::Info);
    }
}
