//! A small file logger for the `log` facade. When the file would pass
//! `max_bytes` it is renamed to `<name>.1` (replacing the previous one) and a
//! new file is started, so at most one old file is kept.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use log::{LevelFilter, Log, Metadata, Record};

/// Rotate at 1 MB.
pub const MAX_BYTES: u64 = 1024 * 1024;
/// Optional level override, e.g. `PERCHBAR_LOG=debug`.
pub const LEVEL_ENV: &str = "PERCHBAR_LOG";
/// After a failed rotation, write this much more before trying again.
const RETRY_AFTER_BYTES: u64 = 64 * 1024;

pub struct FileLogger {
    path: PathBuf,
    max_bytes: u64,
    level: LevelFilter,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The open file and its current size; `None` after a failed open.
    file: Option<(File, u64)>,
    /// After a failed rotation, the size the file must reach before the
    /// next attempt, so a stuck rename is not retried on every line.
    retry_at: u64,
    attempts: u32,
}

impl FileLogger {
    pub fn open(path: &Path, max_bytes: u64, level: LevelFilter) -> io::Result<FileLogger> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(FileLogger {
            path: path.to_path_buf(),
            max_bytes,
            level,
            state: Mutex::new(State {
                file: Some((file, size)),
                ..State::default()
            }),
        })
    }

    /// Appends one line, rotating first if it would not fit.
    pub fn write_line(&self, line: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let len = line.len() as u64 + 1;
        let size = state.file.as_ref().map(|(_, size)| *size);
        if let Some(size) = size.filter(|s| s + len > self.max_bytes && *s >= state.retry_at) {
            state.file = None; // close before renaming
            state.attempts += 1;
            state.retry_at = match std::fs::rename(&self.path, rotated(&self.path)) {
                Ok(()) => 0,
                Err(_) => size + RETRY_AFTER_BYTES,
            };
        }
        if state.file.is_none() {
            state.file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .ok()
                .map(|f| {
                    let size = f.metadata().map_or(0, |m| m.len());
                    (f, size)
                });
        }
        if let Some((file, size)) = state.file.as_mut() {
            if writeln!(file, "{line}").is_ok() {
                *size += len;
            }
        }
    }
}

impl FileLogger {
    /// How many times rotation was tried (for tests and diagnostics).
    pub fn rotation_attempts(&self) -> u32 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).attempts
    }
}

/// `perchbar.log` -> `perchbar.log.1`
pub fn rotated(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        // Perchbar's own messages at the chosen level; dependencies only warn+.
        let limit = if metadata.target().starts_with("perchbar") {
            self.level
        } else {
            self.level.min(LevelFilter::Warn)
        };
        metadata.level() <= limit
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {:<5} {}: {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            record.level(),
            record.target(),
            record.args()
        );
        if cfg!(debug_assertions) {
            eprintln!("{line}");
        }
        self.write_line(&line);
    }

    fn flush(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((file, _)) = state.file.as_mut() {
            let _ = file.flush();
        }
    }
}

/// Installs the global logger writing to `path`. The level is `info` unless
/// `PERCHBAR_LOG` names another one.
pub fn init(path: &Path) -> io::Result<()> {
    let level = std::env::var(LEVEL_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(LevelFilter::Info);
    let logger = FileLogger::open(path, MAX_BYTES, level)?;
    log::set_boxed_logger(Box::new(logger)).map_err(io::Error::other)?;
    log::set_max_level(level);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_and_keeps_one_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("perchbar.log");
        let logger = FileLogger::open(&path, 100, LevelFilter::Info).unwrap();
        for i in 0..30 {
            logger.write_line(&format!("line {i:02} xxxxxxxxxx"));
        }
        let current = std::fs::read_to_string(&path).unwrap();
        let old = std::fs::read_to_string(rotated(&path)).unwrap();
        assert!(current.len() <= 100, "{}", current.len());
        assert!(old.len() <= 100, "{}", old.len());
        assert!(current.ends_with("line 29 xxxxxxxxxx\n"), "{current}");
        let files = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(files, 2);
    }

    #[test]
    fn appends_to_an_existing_file_and_counts_its_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("perchbar.log");
        std::fs::write(&path, "x".repeat(97)).unwrap();
        let logger = FileLogger::open(&path, 100, LevelFilter::Info).unwrap();
        logger.write_line("next");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "next\n");
        assert_eq!(std::fs::read_to_string(rotated(&path)).unwrap().len(), 97);
    }

    #[test]
    fn failed_rotation_waits_for_another_64kb_before_retrying() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("perchbar.log");
        // A non-empty folder where the old file goes: the rename fails.
        let blocker = rotated(&path);
        std::fs::create_dir(&blocker).unwrap();
        std::fs::write(blocker.join("x"), "x").unwrap();
        let logger = FileLogger::open(&path, 100, LevelFilter::Info).unwrap();
        let line = "y".repeat(99); // 100 bytes with the newline
        logger.write_line(&line);
        logger.write_line(&line); // over the limit: the first attempt fails
        assert_eq!(logger.rotation_attempts(), 1);
        // Below another 64 KB: no new attempt, and lines still land.
        for _ in 0..600 {
            logger.write_line(&line);
        }
        assert_eq!(logger.rotation_attempts(), 1);
        let written = std::fs::metadata(&path).unwrap().len();
        assert_eq!(written, 602 * 100);
        // Past 64 KB more: one more attempt.
        for _ in 0..60 {
            logger.write_line(&line);
        }
        assert_eq!(logger.rotation_attempts(), 2);
    }

    #[test]
    fn dependencies_log_warnings_only() {
        let dir = tempfile::tempdir().unwrap();
        let logger =
            FileLogger::open(&dir.path().join("a.log"), MAX_BYTES, LevelFilter::Info).unwrap();
        let meta = |target, level| Metadata::builder().target(target).level(level).build();
        assert!(logger.enabled(&meta("perchbar::scheduler", log::Level::Info)));
        assert!(!logger.enabled(&meta("perchbar::scheduler", log::Level::Debug)));
        assert!(!logger.enabled(&meta("reqwest::connect", log::Level::Info)));
        assert!(logger.enabled(&meta("tao::platform", log::Level::Warn)));
    }

    #[test]
    fn widget_log_records_pass_at_the_default_level() {
        let dir = tempfile::tempdir().unwrap();
        let logger =
            FileLogger::open(&dir.path().join("a.log"), MAX_BYTES, LevelFilter::Info).unwrap();
        let meta = Metadata::builder()
            .target(crate::sandbox::WIDGET_LOG_TARGET)
            .level(log::Level::Info)
            .build();
        assert!(logger.enabled(&meta));
    }
}
