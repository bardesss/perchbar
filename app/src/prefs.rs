//! App preferences (`app.json`) and the Start with Windows registration.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Preferences of the app itself, as opposed to its widgets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppPrefs {
    pub autostart: bool,
}

/// Reads `app.json`. None when it is missing or corrupt.
pub fn load(path: &Path) -> Option<AppPrefs> {
    let text = std::fs::read_to_string(path).ok()?;
    let json = text.strip_prefix('\u{feff}').unwrap_or(&text);
    serde_json::from_str(json).ok()
}

/// Writes `<path>.tmp` next to the target, then renames it over the target.
pub fn save(path: &Path, prefs: &AppPrefs) -> io::Result<()> {
    let json = serde_json::to_string_pretty(prefs).map_err(io::Error::other)?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// Start with Windows.
pub trait Autostart {
    /// True when Perchbar is registered for this exe and Windows has not
    /// turned it off.
    fn enabled(&self) -> bool;
    fn set(&self, on: bool) -> io::Result<()>;
}

/// Reports off and ignores `set`. Used for dev and test runs, so they never
/// touch the real Run key.
pub struct NoAutostart;

impl Autostart for NoAutostart {
    fn enabled(&self) -> bool {
        false
    }
    fn set(&self, _on: bool) -> io::Result<()> {
        Ok(())
    }
}

/// Applies the Start with Windows policy at launch and returns the prefs in
/// effect. First run (no usable `app.json`) turns autostart on; a user's off
/// stays off; an on whose registration is missing or points at another exe
/// (a moved portable exe) is rewritten. Registry failures are only logged.
pub fn startup(prefs_path: &Path, autostart: &dyn Autostart, exe: &Path) -> AppPrefs {
    match load(prefs_path) {
        None => {
            let prefs = AppPrefs { autostart: true };
            apply_on(autostart, exe);
            if let Err(e) = save(prefs_path, &prefs) {
                log::warn!("cannot save {}: {e}", prefs_path.display());
            }
            prefs
        }
        Some(prefs) => {
            if prefs.autostart && !autostart.enabled() {
                apply_on(autostart, exe);
            }
            prefs
        }
    }
}

fn apply_on(autostart: &dyn Autostart, exe: &Path) {
    if let Err(e) = autostart.set(true) {
        log::warn!(
            "cannot register {} to start with Windows: {e}",
            exe.display()
        );
    }
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const VALUE_NAME: &str = "Perchbar";

/// Start with Windows through `HKCU\...\Run`. Kept thin: the policy lives in
/// `startup`, and tests never touch the real registry.
pub struct RegistryAutostart {
    pub exe: PathBuf,
}

/// The Run value is the exe path in quotes.
fn run_data(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// Compares a stored Run value with the exe: case-insensitive, ignoring the
/// surrounding quotes.
fn same_exe(stored: &str, exe: &Path) -> bool {
    let stored = stored.trim().trim_matches('"');
    stored.eq_ignore_ascii_case(&exe.to_string_lossy())
}

/// Windows' Startup Apps page writes a binary value whose first byte is odd
/// (0x03) when the user turned the entry off.
fn approved_is_off(data: &[u8]) -> bool {
    data.first().is_some_and(|b| b & 1 == 1)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

impl RegistryAutostart {
    fn read_run(&self) -> Option<String> {
        use windows::core::PCWSTR;
        use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

        let key = wide(RUN_KEY);
        let name = wide(VALUE_NAME);
        let mut buf = vec![0u16; 2048];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: NUL-terminated names; `buf` is `bytes` bytes long.
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if rc.0 != 0 {
            return None;
        }
        let units = (bytes as usize / 2).min(buf.len());
        let text = String::from_utf16_lossy(&buf[..units]);
        Some(text.trim_end_matches('\0').to_string())
    }

    /// Missing value or a read failure counts as not disabled.
    fn user_disabled(&self) -> bool {
        use windows::core::PCWSTR;
        use windows::Win32::System::Registry::{
            RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_BINARY,
        };

        let key = wide(APPROVED_KEY);
        let name = wide(VALUE_NAME);
        let mut buf = [0u8; 64];
        let mut bytes = buf.len() as u32;
        // SAFETY: NUL-terminated names; `buf` is `bytes` bytes long.
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_BINARY,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        // ERROR_MORE_DATA (234) still filled the first bytes.
        if rc.0 != 0 && rc.0 != 234 {
            return false;
        }
        approved_is_off(&buf[..(bytes as usize).min(buf.len())])
    }
}

impl Autostart for RegistryAutostart {
    fn enabled(&self) -> bool {
        match self.read_run() {
            Some(stored) => same_exe(&stored, &self.exe) && !self.user_disabled(),
            None => false,
        }
    }

    fn set(&self, on: bool) -> io::Result<()> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
        use windows::Win32::System::Registry::{
            RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ,
        };

        let key = wide(RUN_KEY);
        let name = wide(VALUE_NAME);
        if on {
            let data = wide(&run_data(&self.exe));
            // SAFETY: NUL-terminated strings; the size counts the bytes of `data`.
            let rc = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    PCWSTR(key.as_ptr()),
                    PCWSTR(name.as_ptr()),
                    REG_SZ.0,
                    Some(data.as_ptr().cast()),
                    (data.len() * 2) as u32,
                )
            };
            if rc != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(rc.0 as i32));
            }
        } else {
            // SAFETY: NUL-terminated names. Only the Perchbar value is removed.
            let rc = unsafe {
                RegDeleteKeyValueW(
                    HKEY_CURRENT_USER,
                    PCWSTR(key.as_ptr()),
                    PCWSTR(name.as_ptr()),
                )
            };
            if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
                return Err(io::Error::from_raw_os_error(rc.0 as i32));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// An in-memory Run value.
    struct Fake {
        /// The exe the fake Run value points at, if any.
        registered: RefCell<Option<PathBuf>>,
        current: PathBuf,
        calls: RefCell<Vec<bool>>,
    }

    impl Fake {
        fn new(registered: Option<&str>) -> Self {
            Fake {
                registered: RefCell::new(registered.map(PathBuf::from)),
                current: exe(),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Autostart for Fake {
        fn enabled(&self) -> bool {
            self.registered.borrow().as_deref() == Some(self.current.as_path())
        }
        fn set(&self, on: bool) -> io::Result<()> {
            self.calls.borrow_mut().push(on);
            *self.registered.borrow_mut() = on.then(|| self.current.clone());
            Ok(())
        }
    }

    fn exe() -> PathBuf {
        PathBuf::from(r"C:\apps\perchbar.exe")
    }

    #[test]
    fn first_run_turns_autostart_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.json");
        let fake = Fake::new(None);
        let prefs = startup(&path, &fake, &exe());
        assert_eq!(prefs, AppPrefs { autostart: true });
        assert_eq!(*fake.calls.borrow(), vec![true]);
        assert_eq!(load(&path), Some(AppPrefs { autostart: true }));
    }

    #[test]
    fn user_off_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.json");
        save(&path, &AppPrefs { autostart: false }).unwrap();
        let fake = Fake::new(Some(r"D:\old\perchbar.exe"));
        let prefs = startup(&path, &fake, &exe());
        assert!(!prefs.autostart);
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn moved_exe_is_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.json");
        save(&path, &AppPrefs { autostart: true }).unwrap();
        let fake = Fake::new(Some(r"D:\old\perchbar.exe"));
        startup(&path, &fake, &exe());
        assert_eq!(*fake.calls.borrow(), vec![true]);
        assert!(fake.enabled());
    }

    #[test]
    fn matching_registration_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.json");
        save(&path, &AppPrefs { autostart: true }).unwrap();
        let fake = Fake::new(Some(r"C:\apps\perchbar.exe"));
        startup(&path, &fake, &exe());
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn corrupt_prefs_count_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load(&path), None);
        let fake = Fake::new(None);
        let prefs = startup(&path, &fake, &exe());
        assert!(prefs.autostart);
        assert_eq!(*fake.calls.borrow(), vec![true]);
        assert_eq!(load(&path), Some(AppPrefs { autostart: true }));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.json");
        std::fs::write(&path, r#"{"autostart": true, "future": 1}"#).unwrap();
        assert_eq!(load(&path), Some(AppPrefs { autostart: true }));
    }

    #[test]
    fn run_value_helpers() {
        let exe = Path::new(r"C:\Apps\Perchbar.exe");
        assert_eq!(run_data(exe), r#""C:\Apps\Perchbar.exe""#);
        assert!(same_exe(r#""c:\apps\perchbar.EXE""#, exe));
        assert!(same_exe(r"C:\Apps\Perchbar.exe", exe));
        assert!(!same_exe(r#""D:\Apps\Perchbar.exe""#, exe));
        assert!(approved_is_off(&[0x03, 0, 0]));
        assert!(approved_is_off(&[0x01]));
        assert!(!approved_is_off(&[0x02, 0, 0]));
        assert!(!approved_is_off(&[]));
    }
}
