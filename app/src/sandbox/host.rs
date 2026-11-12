//! Host functions exposed to widget code: storage, log and time.
//!
//! Each is installed as a hidden `__host_*` global that `prelude.js` captures
//! and deletes before widget code runs. Values cross the boundary as JSON text.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Datelike, Offset, TimeZone, Timelike};
use rquickjs::{Ctx, Exception, Function};
use serde_json::{json, Map, Value};

/// Maximum serialized size of one instance's storage object.
pub const STORAGE_MAX_BYTES: usize = 64 * 1024;

/// One instance's persisted key/value store (a single JSON object on disk).
#[derive(Debug)]
pub struct Storage {
    path: PathBuf,
    values: Map<String, Value>,
    dirty: bool,
}

impl Storage {
    /// Loads `path`; a missing or unreadable file starts empty.
    pub fn load(path: PathBuf) -> Storage {
        let values = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("ignoring corrupt storage {}: {e}", path.display());
                Map::new()
            }),
            Err(_) => Map::new(),
        };
        Storage {
            path,
            values,
            dirty: false,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// Stores `value`, rejecting it if the whole object would exceed the cap.
    pub fn set(&mut self, key: String, value: Value) -> Result<(), String> {
        let previous = self.values.insert(key.clone(), value);
        let size = serde_json::to_vec(&self.values).map_or(usize::MAX, |v| v.len());
        if size > STORAGE_MAX_BYTES {
            match previous {
                Some(p) => self.values.insert(key, p),
                None => self.values.remove(&key),
            };
            return Err(format!(
                "storage limit exceeded ({size} > {STORAGE_MAX_BYTES} bytes)"
            ));
        }
        self.dirty = true;
        Ok(())
    }

    /// Writes the store (tmp file + rename) if anything changed.
    pub fn save_if_dirty(&mut self) -> std::io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        write_atomic(&self.path, &serde_json::to_vec(&self.values)?)?;
        self.dirty = false;
        Ok(())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// `time.now(tz?)` fields; `None` uses the local time zone.
pub fn time_now(tz: Option<&str>) -> Result<Value, String> {
    let now = chrono::Utc::now();
    match tz {
        None => Ok(time_fields(now.with_timezone(&chrono::Local))),
        Some(name) => {
            let tz: chrono_tz::Tz = name
                .parse()
                .map_err(|_| format!("unknown time zone {name:?}"))?;
            Ok(time_fields(now.with_timezone(&tz)))
        }
    }
}

fn time_fields<Tz: TimeZone>(t: DateTime<Tz>) -> Value
where
    Tz::Offset: std::fmt::Display,
{
    json!({
        "year": t.year(),
        "month": t.month(),
        "day": t.day(),
        "hour": t.hour(),
        "minute": t.minute(),
        "second": t.second(),
        "weekday": t.weekday().to_string(),
        "hhmm": t.format("%H:%M").to_string(),
        "offsetMinutes": t.offset().fix().local_minus_utc() / 60,
    })
}

/// Installs `__host_storage_get`, `__host_storage_set`, `__host_log` and
/// `__host_time_now` on the global object.
pub fn install<'js>(
    ctx: &Ctx<'js>,
    storage: Arc<Mutex<Storage>>,
    instance_id: String,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    let store = storage.clone();
    let get = Function::new(ctx.clone(), move |key: String| -> Option<String> {
        let store = store.lock().unwrap();
        store.get(&key).map(Value::to_string)
    })?;
    globals.set("__host_storage_get", get)?;

    let set = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, key: String, json: String| -> rquickjs::Result<()> {
            let value: Value = serde_json::from_str(&json)
                .map_err(|e| Exception::throw_type(&ctx, &format!("storage.set: {e}")))?;
            storage
                .lock()
                .unwrap()
                .set(key, value)
                .map_err(|e| Exception::throw_range(&ctx, &format!("storage.set: {e}")))
        },
    )?;
    globals.set("__host_storage_set", set)?;

    let log = Function::new(ctx.clone(), move |msg: String| {
        log::info!(target: "widget", "[{instance_id}] {msg}");
    })?;
    globals.set("__host_log", log)?;

    let now = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>, tz: Option<String>| -> rquickjs::Result<String> {
            time_now(tz.as_deref())
                .map(|v| v.to_string())
                .map_err(|e| Exception::throw_type(&ctx, &e))
        },
    )?;
    globals.set("__host_time_now", now)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_cap_rejects_and_keeps_previous_value() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Storage::load(dir.path().join("s.json"));
        s.set("k".into(), json!("small")).unwrap();
        assert!(s
            .set("k".into(), json!("x".repeat(STORAGE_MAX_BYTES)))
            .is_err());
        assert_eq!(s.get("k"), Some(&json!("small")));
    }

    #[test]
    fn unchanged_storage_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        Storage::load(path.clone()).save_if_dirty().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn local_time_has_all_fields() {
        let v = time_now(None).unwrap();
        for k in [
            "year",
            "month",
            "day",
            "hour",
            "minute",
            "second",
            "weekday",
            "hhmm",
            "offsetMinutes",
        ] {
            assert!(v.get(k).is_some(), "{k}");
        }
        assert_eq!(v["hhmm"].as_str().unwrap().len(), 5);
    }
}
