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

/// Folder name of the installed widgets, in the data folder.
pub const WIDGETS_DIR: &str = "widgets";
/// File name of the instance data, in the data folder.
pub const INSTANCES_FILE: &str = "instances.json";
/// Folder name of the encrypted secrets, in the data folder.
pub const SECRETS_DIR: &str = "secrets";

pub fn widgets_dir() -> PathBuf {
    data_dir().join(WIDGETS_DIR)
}

pub fn instances_file() -> PathBuf {
    data_dir().join(INSTANCES_FILE)
}

pub fn storage_file(instance_id: &str) -> PathBuf {
    data_dir()
        .join("storage")
        .join(format!("{instance_id}.json"))
}

/// The folder of the DPAPI-encrypted secret files.
pub fn secrets_dir() -> PathBuf {
    data_dir().join(SECRETS_DIR)
}

/// The DPAPI-encrypted secrets of one instance.
pub fn secrets_file(instance_id: &str) -> PathBuf {
    secrets_dir().join(format!("{instance_id}.bin"))
}

/// The app preferences (Start with Windows).
pub fn prefs_file() -> PathBuf {
    data_dir().join("app.json")
}

pub fn log_file() -> PathBuf {
    data_dir().join("perchbar.log")
}

/// File name of the approved permissions per widget, in the data folder.
pub const APPROVALS_FILE: &str = "approvals.json";
/// File name of the local widget folders, in the data folder.
pub const LOCAL_FILE: &str = "local.json";

/// `{ "<widget-id>": Permissions }`: what the user approved.
pub fn approvals_file() -> PathBuf {
    data_dir().join(APPROVALS_FILE)
}

/// `{ "<widget-id>": "<absolute folder>" }`: widgets used in place.
pub fn local_file() -> PathBuf {
    data_dir().join(LOCAL_FILE)
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
        assert_eq!(secrets_dir(), d.join("secrets"));
        assert_eq!(secrets_file("abc"), d.join("secrets").join("abc.bin"));
        assert_eq!(prefs_file(), d.join("app.json"));
        assert_eq!(log_file(), d.join("perchbar.log"));
        assert_eq!(approvals_file(), d.join("approvals.json"));
        assert_eq!(local_file(), d.join("local.json"));
    }
}
