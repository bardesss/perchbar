//! Secret settings, kept out of `instances.json` and encrypted with Windows
//! DPAPI (user scope), one file per instance. Also the `Redactor`, which keeps
//! secret values out of log lines and error messages.
//!
//! Nothing here puts a secret value or a decrypted buffer into `Debug`
//! output, log lines or error messages.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{Map, Value};

use crate::instances::Instances;
use crate::manifest::{Manifest, SettingKind};

/// Longest secret value, in characters.
pub const SECRET_MAX_CHARS: usize = 4096;
/// Secret values shorter than this are not redacted (they would mangle
/// ordinary text).
pub const REDACT_MIN_CHARS: usize = 4;
/// What a redacted secret value is replaced with.
pub const REDACTED: &str = "[secret]";
/// Extra entropy mixed into every DPAPI blob.
const ENTROPY: &[u8] = b"Perchbar secrets v1";

/// Which DPAPI call to make.
enum Direction {
    Protect,
    Unprotect,
}

/// Runs `CryptProtectData` or `CryptUnprotectData` (user scope, no UI, with
/// `ENTROPY`) over `input` and copies the result out of the DPAPI buffer,
/// which is then freed with `LocalFree`.
fn dpapi(input: &[u8], direction: Direction) -> io::Result<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let too_big = || io::Error::new(io::ErrorKind::InvalidInput, "data too large");
    let data_in = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(input.len()).map_err(|_| too_big())?,
        pbData: input.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr().cast_mut(),
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: the input blobs point at live slices that DPAPI only reads; on
    // success `out` holds a LocalAlloc'd buffer of `cbData` bytes, copied and
    // then freed exactly once below.
    unsafe {
        let result = match direction {
            Direction::Protect => CryptProtectData(
                &data_in,
                windows::core::PCWSTR::null(),
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            ),
            Direction::Unprotect => CryptUnprotectData(
                &data_in,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            ),
        };
        // The Win32 error says why (bad data, wrong user); it never holds data.
        result.map_err(|e| io::Error::other(format!("DPAPI: {}", e.message())))?;
        if out.pbData.is_null() {
            return Ok(Vec::new());
        }
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        // Wipe the plain text before handing the buffer back to the system.
        std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
        let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
        Ok(bytes)
    }
}

/// Encrypts `plain` for the current Windows user.
pub fn protect(plain: &[u8]) -> io::Result<Vec<u8>> {
    dpapi(plain, Direction::Protect)
}

/// Decrypts a blob made by `protect` for the current Windows user. Fails for
/// blobs from another user or PC, and for corrupt ones.
pub fn unprotect(blob: &[u8]) -> io::Result<Vec<u8>> {
    dpapi(blob, Direction::Unprotect)
}

/// One instance's secrets as read from disk.
pub struct SecretLoad {
    pub values: Map<String, Value>,
    /// The file exists but cannot be decrypted or parsed (copied from another
    /// PC or user, or corrupt). `values` is then empty and the file is kept.
    pub unreadable: bool,
}

impl fmt::Debug for SecretLoad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretLoad")
            .field("keys", &self.values.keys().collect::<Vec<_>>())
            .field("unreadable", &self.unreadable)
            .finish()
    }
}

/// The encrypted per-instance secret files in one folder
/// (`<dir>\<instance-id>.bin`). Cheap to clone; clones share a lock, so
/// read-modify-write updates never interleave.
#[derive(Clone)]
pub struct SecretStore {
    dir: Arc<PathBuf>,
    lock: Arc<Mutex<()>>,
}

impl fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretStore")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// Instance ids end up in file names: only letters, digits and `-`.
fn valid_id(instance_id: &str) -> bool {
    !instance_id.is_empty()
        && instance_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn bad_id() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid instance id")
}

enum Read {
    Missing,
    Values(Map<String, Value>),
    Unreadable,
}

impl SecretStore {
    pub fn new(dir: PathBuf) -> SecretStore {
        SecretStore {
            dir: Arc::new(dir),
            lock: Arc::default(),
        }
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn file(&self, instance_id: &str) -> PathBuf {
        self.dir.join(format!("{instance_id}.bin"))
    }

    fn read(&self, instance_id: &str) -> Read {
        if !valid_id(instance_id) {
            return Read::Missing;
        }
        let blob = match std::fs::read(self.file(instance_id)) {
            Ok(blob) => blob,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Read::Missing,
            Err(_) => return Read::Unreadable,
        };
        let Ok(plain) = unprotect(&blob) else {
            return Read::Unreadable;
        };
        let parsed = serde_json::from_slice::<Map<String, Value>>(&plain);
        drop(plain);
        match parsed {
            Ok(values) if values.values().all(Value::is_string) => Read::Values(values),
            _ => Read::Unreadable,
        }
    }

    fn write(&self, instance_id: &str, values: &Map<String, Value>) -> io::Result<()> {
        let path = self.file(instance_id);
        if values.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let plain = serde_json::to_vec(values).map_err(|_| io::Error::other("cannot encode"))?;
        let blob = protect(&plain)?;
        drop(plain);
        write_atomic(&path, &blob)
    }

    /// The instance's secrets. A missing file gives none.
    pub fn load(&self, instance_id: &str) -> SecretLoad {
        let _g = self.guard();
        match self.read(instance_id) {
            Read::Missing => SecretLoad {
                values: Map::new(),
                unreadable: false,
            },
            Read::Values(values) => SecretLoad {
                values,
                unreadable: false,
            },
            Read::Unreadable => SecretLoad {
                values: Map::new(),
                unreadable: true,
            },
        }
    }

    /// Stores one secret. Values over `SECRET_MAX_CHARS` characters are
    /// rejected with `InvalidInput`. An unreadable file is replaced.
    pub fn set(&self, instance_id: &str, key: &str, value: &str) -> io::Result<()> {
        if !valid_id(instance_id) {
            return Err(bad_id());
        }
        if value.chars().count() > SECRET_MAX_CHARS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("secret values are at most {SECRET_MAX_CHARS} characters"),
            ));
        }
        let _g = self.guard();
        let mut values = match self.read(instance_id) {
            Read::Values(values) => values,
            Read::Missing | Read::Unreadable => Map::new(),
        };
        values.insert(key.to_string(), Value::String(value.to_string()));
        self.write(instance_id, &values)
    }

    /// Forgets one secret. Removes the file once it holds none. An
    /// unreadable file is left alone.
    pub fn clear(&self, instance_id: &str, key: &str) -> io::Result<()> {
        if !valid_id(instance_id) {
            return Err(bad_id());
        }
        let _g = self.guard();
        let Read::Values(mut values) = self.read(instance_id) else {
            return Ok(());
        };
        if values.remove(key).is_none() {
            return Ok(());
        }
        self.write(instance_id, &values)
    }

    /// Deletes all of the instance's secrets.
    pub fn remove_instance(&self, instance_id: &str) {
        if !valid_id(instance_id) {
            return;
        }
        let _g = self.guard();
        let path = self.file(instance_id);
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != io::ErrorKind::NotFound {
                log::warn!("cannot remove {}: {e}", path.display());
            }
        }
    }

    /// The keys that have a stored secret, sorted.
    pub fn keys_set(&self, instance_id: &str) -> Vec<String> {
        let mut keys: Vec<String> = self.load(instance_id).values.keys().cloned().collect();
        keys.sort();
        keys
    }
}

/// Writes `<path>.tmp`, then renames it over `path`.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Replaces secret values in text with `[secret]`.
#[derive(Default)]
pub struct Redactor {
    /// Longest first, so a secret containing another is replaced whole.
    values: Vec<String>,
}

impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redactor")
            .field("values", &self.values.len())
            .finish()
    }
}

impl Redactor {
    /// Values shorter than `REDACT_MIN_CHARS` characters are ignored.
    pub fn new(values: impl IntoIterator<Item = String>) -> Redactor {
        let mut values: Vec<String> = values
            .into_iter()
            .filter(|v| v.chars().count() >= REDACT_MIN_CHARS)
            .collect();
        values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        values.dedup();
        Redactor { values }
    }

    /// A redactor for the string values of a settings map.
    pub fn from_values(values: &Map<String, Value>) -> Redactor {
        Redactor::new(
            values
                .values()
                .filter_map(|v| v.as_str().map(str::to_string)),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// `text` with every occurrence of every secret value replaced.
    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for v in &self.values {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), REDACTED);
            }
        }
        out
    }
}

/// Moves every `secret`-kind setting found in `data` into `store` and removes
/// it from `data`. A value moves only when the store write succeeded; empty
/// strings and nulls are just dropped. Instances of widgets not in
/// `manifests` are left alone. Returns whether `data` changed.
pub fn migrate(
    data: &mut Instances,
    manifests: &HashMap<String, Arc<Manifest>>,
    store: &SecretStore,
) -> bool {
    let mut changed = false;
    for inst in &mut data.instances {
        let Some(m) = manifests.get(&inst.widget) else {
            continue;
        };
        for field in m.settings.iter().filter(|f| f.kind == SettingKind::Secret) {
            let Some(value) = inst.settings.get(&field.key) else {
                continue;
            };
            let text = match value {
                Value::Null => String::new(),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let moved = text.is_empty() || {
                match store.set(&inst.id, &field.key, &text) {
                    Ok(()) => true,
                    Err(e) => {
                        log::warn!(
                            "cannot move secret {:?} of {} out of instances.json: {e}",
                            field.key,
                            inst.id
                        );
                        false
                    }
                }
            };
            if moved {
                inst.settings.remove(&field.key);
                changed = true;
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::{self, Instance};
    use serde_json::json;

    fn store() -> (SecretStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (SecretStore::new(dir.path().join("secrets")), dir)
    }

    const ID: &str = "inst-00000001";

    #[test]
    fn protect_round_trips() {
        let blob = protect(b"abc").unwrap();
        assert_ne!(blob, b"abc");
        assert_eq!(unprotect(&blob).unwrap(), b"abc");
    }

    #[test]
    fn store_set_load_clear() {
        let (s, _dir) = store();
        assert!(s.keys_set(ID).is_empty());
        s.set(ID, "apiKey", "v").unwrap();
        s.set(ID, "token", "t0k3n").unwrap();
        let loaded = s.load(ID);
        assert!(!loaded.unreadable);
        assert_eq!(loaded.values["apiKey"], json!("v"));
        assert_eq!(s.keys_set(ID), vec!["apiKey", "token"]);
        // The file on disk holds no plain text.
        let raw = std::fs::read(s.file(ID)).unwrap();
        assert!(!raw.windows(5).any(|w| w == b"t0k3n"));

        s.clear(ID, "apiKey").unwrap();
        let loaded = s.load(ID);
        assert!(!loaded.values.contains_key("apiKey"));
        assert_eq!(s.keys_set(ID), vec!["token"]);
        // Other instances are separate.
        assert!(s.load("inst-00000002").values.is_empty());

        s.remove_instance(ID);
        assert!(!s.file(ID).exists());
        assert!(s.keys_set(ID).is_empty());
    }

    #[test]
    fn corrupt_file_is_unreadable_not_error() {
        let (s, _dir) = store();
        std::fs::create_dir_all(s.file(ID).parent().unwrap()).unwrap();
        std::fs::write(s.file(ID), b"garbage").unwrap();
        let loaded = s.load(ID);
        assert!(loaded.unreadable);
        assert!(loaded.values.is_empty());
        assert!(s.file(ID).exists());
        // A missing file is simply empty.
        let none = s.load("inst-00000002");
        assert!(!none.unreadable);
        assert!(none.values.is_empty());
    }

    #[test]
    fn secret_value_length_capped() {
        let (s, _dir) = store();
        s.set(ID, "k", &"x".repeat(SECRET_MAX_CHARS)).unwrap();
        let err = s
            .set(ID, "k", &"x".repeat(SECRET_MAX_CHARS + 1))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!err.to_string().contains("xxxx"), "{err}");
    }

    #[test]
    fn redactor_replaces_values() {
        let r = Redactor::new(["sk-12345".to_string(), "abc".to_string()]);
        assert_eq!(r.apply("key sk-12345 ok"), "key [secret] ok");
        assert_eq!(r.apply("sk-12345sk-12345"), "[secret][secret]");
        // Short values are left alone.
        assert_eq!(r.apply("abc def"), "abc def");
        // A longer secret that contains a shorter one is replaced whole.
        let r = Redactor::new(["pass".to_string(), "password1".to_string()]);
        assert_eq!(r.apply("x password1 pass"), "x [secret] [secret]");
        assert_eq!(Redactor::default().apply("plain"), "plain");
    }

    fn secret_manifest() -> Arc<Manifest> {
        Arc::new(
            serde_json::from_value(json!({
                "id": "w", "name": "W", "formatVersion": 1,
                "settings": [
                    {"key": "apiKey", "label": "API key", "kind": "secret"},
                    {"key": "city", "label": "City", "kind": "text"}
                ]
            }))
            .unwrap(),
        )
    }

    #[test]
    fn secret_never_reaches_instances_json() {
        let (s, dir) = store();
        let mut settings = Map::new();
        settings.insert("apiKey".into(), json!("sk-live-123456"));
        settings.insert("city".into(), json!("Oslo"));
        let mut data = Instances {
            instances: vec![Instance {
                id: ID.into(),
                widget: "w".into(),
                settings,
                hidden: false,
                max_width: None,
            }],
            groups: vec![],
            order: vec![ID.into()],
        };
        let manifests = HashMap::from([("w".to_string(), secret_manifest())]);
        assert!(migrate(&mut data, &manifests, &s));
        let path = dir.path().join("instances.json");
        instances::save(&path, &data).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("apiKey"), "{text}");
        assert!(!text.contains("sk-live-123456"), "{text}");
        assert!(text.contains("Oslo"));
        assert_eq!(s.load(ID).values["apiKey"], json!("sk-live-123456"));
        // Nothing left to move the second time.
        assert!(!migrate(&mut data, &manifests, &s));
    }

    #[test]
    fn migration_keeps_value_when_the_store_rejects_it() {
        let (s, _dir) = store();
        let too_long = "x".repeat(SECRET_MAX_CHARS + 1);
        let mut settings = Map::new();
        settings.insert("apiKey".into(), json!(too_long));
        let mut data = Instances {
            instances: vec![Instance {
                id: ID.into(),
                widget: "w".into(),
                settings,
                hidden: false,
                max_width: None,
            }],
            groups: vec![],
            order: vec![ID.into()],
        };
        let manifests = HashMap::from([("w".to_string(), secret_manifest())]);
        assert!(!migrate(&mut data, &manifests, &s));
        assert!(data.instances[0].settings.contains_key("apiKey"));
    }
}
