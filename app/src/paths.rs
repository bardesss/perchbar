//! Locations of Perchbar's files under `%APPDATA%\Perchbar\`.

use std::path::PathBuf;

/// `%APPDATA%\Perchbar` (falls back to `.\Perchbar` if APPDATA is unset).
pub fn data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Perchbar")
}

pub fn widgets_dir() -> PathBuf {
    data_dir().join("widgets")
}

pub fn instances_file() -> PathBuf {
    data_dir().join("instances.json")
}

pub fn storage_file(instance_id: &str) -> PathBuf {
    data_dir()
        .join("storage")
        .join(format!("{instance_id}.json"))
}

pub fn log_file() -> PathBuf {
    data_dir().join("perchbar.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_under_data_dir() {
        let d = data_dir();
        assert!(d.ends_with("Perchbar"));
        assert_eq!(widgets_dir(), d.join("widgets"));
        assert_eq!(instances_file(), d.join("instances.json"));
        assert_eq!(storage_file("abc"), d.join("storage").join("abc.json"));
        assert_eq!(log_file(), d.join("perchbar.log"));
    }
}
