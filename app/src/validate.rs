//! Checks what the settings window sends before it reaches the scheduler.
//!
//! Error messages never repeat the value they reject, so a secret pasted
//! into the wrong field cannot end up in an error or a log line.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::instances::MIN_GROUP_INTERVAL_SECS;
use crate::manifest::{Manifest, SettingKind};

/// Longest `text` value, in characters.
pub const TEXT_MAX_CHARS: usize = 2000;
/// Longest `location` value and longest `list` entry, in characters.
pub const SHORT_MAX_CHARS: usize = 200;
/// Most entries in a `list` value.
pub const LIST_MAX_ENTRIES: usize = 50;
/// Most entries a `list` value may arrive with, empty ones included.
pub const LIST_MAX_RAW_ENTRIES: usize = 200;
/// Largest serialized settings object, in bytes.
pub const SETTINGS_MAX_BYTES: usize = 16 * 1024;
/// Allowed `maxWidth` range, in pixels.
pub const MAX_WIDTH_RANGE: std::ops::RangeInclusive<u32> = 40..=600;
/// Longest group rotation interval, in seconds.
pub const GROUP_INTERVAL_MAX_SECS: u32 = 3600;

/// A problem with one setting, shown next to its field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldError {
    pub key: String,
    pub message: String,
}

impl FieldError {
    fn new(key: &str, message: impl Into<String>) -> FieldError {
        FieldError {
            key: key.to_string(),
            message: message.into(),
        }
    }
}

/// Checks `input` against the manifest and returns the settings to store.
///
/// `secrets_set` lists the secret keys the store already holds: a required
/// `secret` field counts as filled when its key is there. Secret values are
/// never accepted here; they go through `set_secret`.
///
/// A `null` value clears the setting, so the manifest default applies.
pub fn settings(
    m: &Manifest,
    input: &Map<String, Value>,
    secrets_set: &[String],
) -> Result<Map<String, Value>, Vec<FieldError>> {
    let mut out = Map::new();
    let mut errors = Vec::new();

    for (key, value) in input {
        let Some(field) = m.settings.iter().find(|f| &f.key == key) else {
            errors.push(FieldError::new(key, "Unknown setting"));
            continue;
        };
        if field.kind == SettingKind::Secret {
            errors.push(FieldError::new(key, "Secrets are saved separately"));
            continue;
        }
        if value.is_null() {
            continue;
        }
        match check(field.kind, &field.options, value) {
            Ok(v) => {
                out.insert(key.clone(), v);
            }
            Err(message) => errors.push(FieldError::new(key, message)),
        }
    }

    for field in m.settings.iter().filter(|f| f.required) {
        if errors.iter().any(|e| e.key == field.key) {
            continue;
        }
        let filled = if field.kind == SettingKind::Secret {
            secrets_set.contains(&field.key)
        } else {
            out.get(&field.key)
                .or(field.default.as_ref())
                .is_some_and(is_filled)
        };
        if !filled {
            errors.push(FieldError::new(&field.key, "Required"));
        }
    }

    if errors.is_empty() {
        let size = serde_json::to_vec(&out).map_or(usize::MAX, |b| b.len());
        if size > SETTINGS_MAX_BYTES {
            errors.push(FieldError::new(
                "",
                format!(
                    "Settings are at most {} KB in total",
                    SETTINGS_MAX_BYTES / 1024
                ),
            ));
        }
    }

    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

/// Checks one value against its kind and returns the value to store.
fn check(kind: SettingKind, options: &[String], value: &Value) -> Result<Value, String> {
    let string = |max: usize| match value {
        Value::String(s) if s.chars().count() <= max => Ok(value.clone()),
        Value::String(_) => Err(format!("At most {max} characters")),
        _ => Err("Must be text".to_string()),
    };
    match kind {
        SettingKind::Text => string(TEXT_MAX_CHARS),
        SettingKind::Location => string(SHORT_MAX_CHARS),
        SettingKind::Number => match value.as_f64() {
            Some(n) if n.is_finite() => Ok(value.clone()),
            _ => Err("Must be a number".to_string()),
        },
        SettingKind::Select => match value {
            Value::String(s) if options.contains(s) => Ok(value.clone()),
            _ => Err("Must be one of the listed choices".to_string()),
        },
        SettingKind::Toggle => match value {
            Value::Bool(_) => Ok(value.clone()),
            _ => Err("Must be on or off".to_string()),
        },
        SettingKind::List => {
            let Value::Array(items) = value else {
                return Err("Must be a list".to_string());
            };
            // Bounds the work before empty entries are dropped.
            if items.len() > LIST_MAX_RAW_ENTRIES {
                return Err(format!("At most {LIST_MAX_ENTRIES} entries"));
            }
            let mut kept = Vec::new();
            for item in items {
                let Value::String(s) = item else {
                    return Err("Each entry must be text".to_string());
                };
                let s = s.trim();
                if s.chars().count() > SHORT_MAX_CHARS {
                    return Err(format!(
                        "Each entry is at most {SHORT_MAX_CHARS} characters"
                    ));
                }
                if !s.is_empty() {
                    kept.push(Value::String(s.to_string()));
                }
            }
            if kept.len() > LIST_MAX_ENTRIES {
                return Err(format!("At most {LIST_MAX_ENTRIES} entries"));
            }
            Ok(Value::Array(kept))
        }
        SettingKind::Color => match value {
            Value::String(s) if is_hex_color(s) => Ok(value.clone()),
            _ => Err("Must be a color like #1a2b3c".to_string()),
        },
        SettingKind::Secret => Err("Secrets are saved separately".to_string()),
    }
}

/// `^#[0-9a-fA-F]{6}$`
fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s.bytes().skip(1).all(|b| b.is_ascii_hexdigit())
}

/// Whether a stored value counts as filled for a required field.
fn is_filled(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::String(s) => !s.trim().is_empty(),
        Value::Array(a) => !a.is_empty(),
        _ => true,
    }
}

/// A maximum width: none (automatic) or 40 to 600 pixels.
pub fn max_width(v: Option<u32>) -> Result<Option<u32>, String> {
    match v {
        Some(w) if !MAX_WIDTH_RANGE.contains(&w) => Err(format!(
            "Maximum width is {} to {} pixels",
            MAX_WIDTH_RANGE.start(),
            MAX_WIDTH_RANGE.end()
        )),
        _ => Ok(v),
    }
}

/// A group rotation interval in seconds.
pub fn group_interval(v: u32) -> Result<u32, String> {
    if (MIN_GROUP_INTERVAL_SECS..=GROUP_INTERVAL_MAX_SECS).contains(&v) {
        Ok(v)
    } else {
        Err(format!(
            "Rotation interval is {MIN_GROUP_INTERVAL_SECS} to {GROUP_INTERVAL_MAX_SECS} seconds"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest() -> Manifest {
        serde_json::from_value(json!({
            "id": "w", "name": "W", "formatVersion": 1,
            "settings": [
                {"key": "title", "label": "Title", "kind": "text"},
                {"key": "place", "label": "Place", "kind": "location"},
                {"key": "count", "label": "Count", "kind": "number"},
                {"key": "unit", "label": "Unit", "kind": "select", "options": ["C", "F"]},
                {"key": "on", "label": "On", "kind": "toggle"},
                {"key": "tickers", "label": "Tickers", "kind": "list"},
                {"key": "tint", "label": "Tint", "kind": "color"},
                {"key": "apiKey", "label": "API key", "kind": "secret", "required": true}
            ]
        }))
        .unwrap()
    }

    fn input(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn has_key() -> Vec<String> {
        vec!["apiKey".to_string()]
    }

    fn errors_for(v: Value) -> Vec<FieldError> {
        settings(&manifest(), &input(v), &has_key()).unwrap_err()
    }

    fn keys(errors: &[FieldError]) -> Vec<&str> {
        errors.iter().map(|e| e.key.as_str()).collect()
    }

    #[test]
    fn validate_rejects_unknown_key() {
        let e = errors_for(json!({"nope": 1}));
        assert_eq!(keys(&e), ["nope"]);
    }

    #[test]
    fn validate_number_must_be_finite() {
        let e = errors_for(json!({"count": "12"}));
        assert_eq!(keys(&e), ["count"]);
        let ok = settings(&manifest(), &input(json!({"count": 12.5})), &has_key()).unwrap();
        assert_eq!(ok["count"], json!(12.5));
    }

    #[test]
    fn validate_select_must_be_an_option() {
        assert_eq!(keys(&errors_for(json!({"unit": "K"}))), ["unit"]);
        assert_eq!(keys(&errors_for(json!({"unit": 1}))), ["unit"]);
        let ok = settings(&manifest(), &input(json!({"unit": "F"})), &has_key()).unwrap();
        assert_eq!(ok["unit"], json!("F"));
    }

    #[test]
    fn validate_text_length_capped() {
        let long = "x".repeat(TEXT_MAX_CHARS + 1);
        assert_eq!(keys(&errors_for(json!({ "title": long }))), ["title"]);
        let fits = "x".repeat(TEXT_MAX_CHARS);
        assert!(settings(&manifest(), &input(json!({ "title": fits })), &has_key()).is_ok());
        let place = "y".repeat(SHORT_MAX_CHARS + 1);
        assert_eq!(keys(&errors_for(json!({ "place": place }))), ["place"]);
    }

    #[test]
    fn validate_list_caps_entries_and_drops_empty() {
        let many: Vec<String> = (0..51).map(|n| format!("t{n}")).collect();
        assert_eq!(keys(&errors_for(json!({ "tickers": many }))), ["tickers"]);
        let long = "z".repeat(SHORT_MAX_CHARS + 1);
        assert_eq!(keys(&errors_for(json!({ "tickers": [long] }))), ["tickers"]);
        let ok = settings(
            &manifest(),
            &input(json!({"tickers": ["a", "", " b "]})),
            &has_key(),
        )
        .unwrap();
        assert_eq!(ok["tickers"], json!(["a", "b"]));
        // Mostly empty entries: fine up to the raw cap, refused past it.
        let mut padded = vec![""; LIST_MAX_RAW_ENTRIES - 1];
        padded.push("x");
        assert!(settings(
            &manifest(),
            &input(json!({ "tickers": padded })),
            &has_key()
        )
        .is_ok());
        padded.push("");
        assert_eq!(keys(&errors_for(json!({ "tickers": padded }))), ["tickers"]);
    }

    #[test]
    fn validate_color_hex() {
        for bad in ["#12345", "123456", "#12345g", "#1234567", "red"] {
            assert_eq!(keys(&errors_for(json!({ "tint": bad }))), ["tint"], "{bad}");
        }
        for good in ["#1a2B3c", "#000000", "#FFFFFF"] {
            assert!(
                settings(&manifest(), &input(json!({ "tint": good })), &has_key()).is_ok(),
                "{good}"
            );
        }
    }

    #[test]
    fn validate_required_secret_uses_store() {
        let e = settings(&manifest(), &Map::new(), &[]).unwrap_err();
        assert_eq!(keys(&e), ["apiKey"]);
        assert!(settings(&manifest(), &Map::new(), &has_key()).is_ok());
    }

    #[test]
    fn validate_required_plain_field() {
        let m: Manifest = serde_json::from_value(json!({
            "id": "w", "name": "W", "formatVersion": 1,
            "settings": [
                {"key": "city", "label": "City", "kind": "location", "required": true},
                {"key": "unit", "label": "Unit", "kind": "text", "required": true, "default": "C"}
            ]
        }))
        .unwrap();
        let e = settings(&m, &input(json!({"city": "  "})), &[]).unwrap_err();
        assert_eq!(keys(&e), ["city"]);
        // A default fills a required field the input leaves out.
        assert!(settings(&m, &input(json!({"city": "Oslo"})), &[]).is_ok());
    }

    #[test]
    fn validate_rejects_secret_field_in_settings() {
        let e = errors_for(json!({"apiKey": "sk-very-secret"}));
        assert_eq!(keys(&e), ["apiKey"]);
        assert!(!format!("{e:?}").contains("sk-very-secret"));
    }

    #[test]
    fn validate_total_size_capped() {
        let m: Manifest = serde_json::from_value(json!({
            "id": "w", "name": "W", "formatVersion": 1,
            "settings": (0..10).map(|n| json!({"key": format!("t{n}"), "label": "T", "kind": "text"})).collect::<Vec<_>>()
        }))
        .unwrap();
        let mut big = Map::new();
        for n in 0..10 {
            big.insert(format!("t{n}"), json!("x".repeat(TEXT_MAX_CHARS)));
        }
        let e = settings(&m, &big, &[]).unwrap_err();
        assert_eq!(e.len(), 1);
        assert!(e[0].message.contains("16 KB"));
        big.retain(|k, _| k == "t0");
        assert!(settings(&m, &big, &[]).is_ok());
    }

    #[test]
    fn validate_null_clears_a_setting() {
        let ok = settings(&manifest(), &input(json!({"unit": null})), &has_key()).unwrap();
        assert!(ok.is_empty());
    }

    #[test]
    fn max_width_bounds() {
        assert!(max_width(Some(39)).is_err());
        assert_eq!(max_width(Some(40)), Ok(Some(40)));
        assert_eq!(max_width(Some(600)), Ok(Some(600)));
        assert!(max_width(Some(601)).is_err());
        assert_eq!(max_width(None), Ok(None));
    }

    #[test]
    fn group_interval_bounds() {
        assert!(group_interval(MIN_GROUP_INTERVAL_SECS - 1).is_err());
        assert_eq!(
            group_interval(MIN_GROUP_INTERVAL_SECS),
            Ok(MIN_GROUP_INTERVAL_SECS)
        );
        assert_eq!(group_interval(3600), Ok(3600));
        assert!(group_interval(3601).is_err());
    }
}
