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

pub struct FileLogger {
    path: PathBuf,
    max_bytes: u64,
    level: LevelFilter,
    /// The open file and its current size; `None` after a failed open.
    file: Mutex<Option<(File, u64)>>,
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
            file: Mutex::new(Some((file, size))),
        })
    }

    /// Appends one line, rotating first if it would not fit.
    pub fn write_line(&self, line: &str) {
        let mut guard = self.file.lock().unwrap_or_else(|e| e.into_inner());
        let len = line.len() as u64 + 1;
        if guard
            .as_ref()
            .is_some_and(|(_, size)| size + len > self.max_bytes)
        {
            *guard = None; // close before renaming
            let _ = std::fs::rename(&self.path, rotated(&self.path));
        }
        if guard.is_none() {
            *guard = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .ok()
                .map(|f| {
                    let size = f.metadata().map_or(0, |m| m.len());
                    (f, size)
                });
        }
        if let Some((file, size)) = guard.as_mut() {
            if writeln!(file, "{line}").is_ok() {
                *size += len;
            }
        }
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
        if let Some((file, _)) = self.file.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
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
