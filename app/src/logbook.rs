//! A small in-memory log per widget instance, for the settings window.
//!
//! Widget `log` / `console.log` lines and run failures land here (as well as
//! in the log file). Each instance keeps its newest `MAX_LINES` lines.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Local};

/// Lines kept per instance; older ones are dropped.
pub const MAX_LINES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    pub at: DateTime<Local>,
    pub level: LogLevel,
    pub text: String,
}

/// Shared, cheap to clone. Safe to use from any thread.
#[derive(Clone, Default)]
pub struct LogBook {
    lines: Arc<Mutex<HashMap<String, VecDeque<LogLine>>>>,
}

impl LogBook {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, VecDeque<LogLine>>> {
        self.lines.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Appends a line, dropping the oldest once the instance has `MAX_LINES`.
    pub fn push(&self, instance_id: &str, level: LogLevel, line: String) {
        let mut all = self.lock();
        let book = all.entry(instance_id.to_string()).or_default();
        while book.len() >= MAX_LINES {
            book.pop_front();
        }
        book.push_back(LogLine {
            at: Local::now(),
            level,
            text: line,
        });
    }

    /// The instance's lines, oldest first.
    pub fn lines(&self, instance_id: &str) -> Vec<LogLine> {
        self.lock()
            .get(instance_id)
            .map(|b| b.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Forgets everything logged for the instance.
    pub fn remove(&self, instance_id: &str) {
        self.lock().remove(instance_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logbook_keeps_last_200_lines() {
        let book = LogBook::default();
        for n in 0..250 {
            book.push("inst-00000001", LogLevel::Info, format!("line {n}"));
        }
        book.push("inst-00000002", LogLevel::Warn, "other".into());
        let lines = book.lines("inst-00000001");
        assert_eq!(lines.len(), 200);
        assert_eq!(lines[0].text, "line 50");
        assert_eq!(lines[199].text, "line 249");
        assert_eq!(lines[199].level, LogLevel::Info);
        let other = book.clone().lines("inst-00000002");
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].level, LogLevel::Warn);
    }

    #[test]
    fn logbook_remove_forgets_instance() {
        let book = LogBook::default();
        book.push("inst-00000001", LogLevel::Error, "boom".into());
        book.push("inst-00000002", LogLevel::Info, "keep".into());
        book.remove("inst-00000001");
        assert!(book.lines("inst-00000001").is_empty());
        assert_eq!(book.lines("inst-00000002").len(), 1);
    }
}
