//! Locations of Perchbar's files under `%APPDATA%\Perchbar\`.

use std::path::PathBuf;

/// Development and test override for the data directory.
pub const DATA_DIR_ENV: &str = "PERCHBAR_DATA_DIR";

/// `%PERCHBAR_DATA_DIR%` when set, otherwise `%APPDATA%\Perchbar` (falling
/// back to `.\Perchbar` if APPDATA is unset).
pub fn data_dir() -> PathBuf {
    resolve_data_dir(std::env::var_os(DATA_DIR_ENV), std::env::var_os("APPDATA"))
}

fn resolve_data_dir(
    overridden: Option<std::ffi::OsString>,
    appdata: Option<std::ffi::OsString>,
) -> PathBuf {
    match overridden.filter(|o| !o.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => appdata
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Perchbar"),
    }
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

/// The DPAPI-encrypted secrets of one instance.
pub fn secrets_file(instance_id: &str) -> PathBuf {
    data_dir()
        .join("secrets")
        .join(format!("{instance_id}.bin"))
}

pub fn log_file() -> PathBuf {
    data_dir().join("perchbar.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins_over_appdata() {
        let r = |o: Option<&str>, a: Option<&str>| {
            resolve_data_dir(o.map(Into::into), a.map(Into::into))
        };
        assert_eq!(
            r(None, Some("C:\\A")),
            PathBuf::from("C:\\A").join("Perchbar")
        );
        assert_eq!(r(Some("D:\\t"), Some("C:\\A")), PathBuf::from("D:\\t"));
        assert_eq!(
            r(Some(""), Some("C:\\A")),
            PathBuf::from("C:\\A").join("Perchbar")
        );
        assert_eq!(r(None, None), PathBuf::from(".").join("Perchbar"));
    }

    #[test]
    fn layout_is_under_data_dir() {
        let d = data_dir();
        assert_eq!(widgets_dir(), d.join("widgets"));
        assert_eq!(instances_file(), d.join("instances.json"));
        assert_eq!(storage_file("abc"), d.join("storage").join("abc.json"));
        assert_eq!(secrets_file("abc"), d.join("secrets").join("abc.bin"));
        assert_eq!(log_file(), d.join("perchbar.log"));
    }
}
