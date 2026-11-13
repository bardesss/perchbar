//! Widget manifest (`widget.json`) parsing and validation.

use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Lowest allowed refresh interval in seconds; smaller values are raised.
pub const MIN_REFRESH_SECS: u32 = 10;
const DEFAULT_REFRESH_SECS: u32 = 60;
const SUPPORTED_FORMAT_VERSION: u32 = 1;
/// Largest `widget.json` read.
pub const MANIFEST_MAX_BYTES: u64 = 64 * 1024;
/// Largest `widget.js` accepted when a widget is loaded.
pub const SCRIPT_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON: {0}")]
    Json(String),
    #[error("widget.js is missing")]
    MissingWidgetJs,
    #[error("{0} is larger than {1} bytes")]
    TooLarge(&'static str, u64),
    #[error("{0} must be a file in the widget folder, not a link")]
    NotAFile(&'static str),
    #[error("invalid manifest: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingKind {
    Text,
    Number,
    Select,
    Toggle,
    List,
    Secret,
    Location,
    Color,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingField {
    pub key: String,
    pub label: String,
    pub kind: SettingKind,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    /// Choices for `Select` fields.
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default = "default_version")]
    pub version: String,
    pub format_version: u32,
    #[serde(default = "default_refresh")]
    pub refresh: u32,
    #[serde(default)]
    pub settings: Vec<SettingField>,
    #[serde(default)]
    pub permissions: Permissions,
    /// Editor hint (`"$schema"`); ignored.
    #[serde(default, rename = "$schema", skip_serializing)]
    schema: Option<String>,
}

fn default_version() -> String {
    "0.0.0".to_string()
}

fn default_refresh() -> u32 {
    DEFAULT_REFRESH_SECS
}

/// Reads `dir/widget.json`, validates it and requires `dir/widget.js`.
/// Both must be plain files (links are refused, so nothing outside the
/// folder is read) within the size limits. Nothing is run.
pub fn load(dir: &Path) -> Result<Manifest, ManifestError> {
    let json = dir.join("widget.json");
    plain_file(&json, "widget.json", MANIFEST_MAX_BYTES)?;
    let mut bytes = Vec::new();
    std::fs::File::open(&json)?
        .take(MANIFEST_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MANIFEST_MAX_BYTES {
        return Err(ManifestError::TooLarge("widget.json", MANIFEST_MAX_BYTES));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ManifestError::Json("widget.json is not UTF-8 text".into()))?;
    // Some Windows editors save UTF-8 with a byte order mark.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut manifest: Manifest =
        serde_json::from_str(text).map_err(|e| ManifestError::Json(e.to_string()))?;
    match plain_file(&dir.join("widget.js"), "widget.js", SCRIPT_MAX_BYTES) {
        Err(ManifestError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ManifestError::MissingWidgetJs)
        }
        other => other?,
    }
    validate(&mut manifest)?;
    Ok(manifest)
}

/// Checks that `path` is a regular file (not a link) of at most `max` bytes.
fn plain_file(path: &Path, name: &'static str, max: u64) -> Result<(), ManifestError> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(ManifestError::NotAFile(name));
    }
    if meta.len() > max {
        return Err(ManifestError::TooLarge(name, max));
    }
    Ok(())
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, ManifestError> {
    Err(ManifestError::Invalid(msg.into()))
}

fn validate(m: &mut Manifest) -> Result<(), ManifestError> {
    if m.format_version != SUPPORTED_FORMAT_VERSION {
        return invalid(format!("unsupported formatVersion {}", m.format_version));
    }
    if !is_kebab_case(&m.id) {
        return invalid(format!(
            "id {:?} must be kebab-case (a-z, 0-9, hyphens)",
            m.id
        ));
    }
    if m.name.trim().is_empty() {
        return invalid("name must not be empty");
    }
    m.refresh = m.refresh.max(MIN_REFRESH_SECS);
    for host in &m.permissions.network {
        if !is_bare_hostname(host) {
            return invalid(format!("network entry {host:?} must be a bare hostname"));
        }
    }
    for s in &m.settings {
        if s.key.is_empty() {
            return invalid("setting key must not be empty");
        }
        if s.kind == SettingKind::Select && s.options.is_empty() {
            return invalid(format!("select setting {:?} needs options", s.key));
        }
    }
    Ok(())
}

/// `^[a-z0-9]+(-[a-z0-9]+)*$`
fn is_kebab_case(id: &str) -> bool {
    !id.is_empty()
        && id.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// Dot-separated labels of ASCII letters, digits and inner hyphens: no scheme,
/// port, path, wildcard or whitespace.
fn is_bare_hostname(host: &str) -> bool {
    !host.is_empty()
        && host.split('.').all(|label| {
            let b = label.as_bytes();
            !b.is_empty()
                && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
                && b[0] != b'-'
                && b[b.len() - 1] != b'-'
        })
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn fixture(json: serde_json::Value, with_js: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("widget.json"), json.to_string()).unwrap();
        if with_js {
            fs::write(dir.path().join("widget.js"), "").unwrap();
        }
        dir
    }

    fn base() -> serde_json::Value {
        json!({"id": "btc", "name": "BTC", "formatVersion": 1, "refresh": 60})
    }

    #[test]
    fn loads_valid_manifest() {
        let mut j = base();
        j["permissions"] = json!({"network": ["api.coinbase.com"]});
        let dir = fixture(j, true);
        let m = load(dir.path()).unwrap();
        assert_eq!(m.id, "btc");
        assert_eq!(m.refresh, 60);
        assert_eq!(m.permissions.network, vec!["api.coinbase.com"]);
        assert!(m.permissions.files.is_empty());
    }

    #[test]
    fn leading_byte_order_mark_is_accepted() {
        let dir = fixture(base(), true);
        fs::write(
            dir.path().join("widget.json"),
            format!("\u{feff}{}", base()),
        )
        .unwrap();
        assert_eq!(load(dir.path()).unwrap().id, "btc");
    }

    #[test]
    fn refresh_below_minimum_is_raised() {
        let mut j = base();
        j["refresh"] = json!(3);
        let dir = fixture(j, true);
        assert_eq!(load(dir.path()).unwrap().refresh, 10);
    }

    #[test]
    fn missing_widget_js_is_error() {
        let dir = fixture(base(), false);
        assert!(matches!(
            load(dir.path()),
            Err(ManifestError::MissingWidgetJs)
        ));
    }

    #[test]
    fn id_must_be_kebab_case() {
        let mut j = base();
        j["id"] = json!("My Widget");
        let dir = fixture(j, true);
        assert!(matches!(load(dir.path()), Err(ManifestError::Invalid(_))));
        let mut j = base();
        j["id"] = json!("my-widget-2");
        let dir = fixture(j, true);
        assert!(load(dir.path()).is_ok());
    }

    #[test]
    fn oversized_files_are_refused() {
        let dir = fixture(base(), true);
        let mut big = base();
        big["description"] = json!("x".repeat(MANIFEST_MAX_BYTES as usize));
        fs::write(dir.path().join("widget.json"), big.to_string()).unwrap();
        assert!(matches!(
            load(dir.path()),
            Err(ManifestError::TooLarge("widget.json", _))
        ));

        let dir = fixture(base(), true);
        let js = vec![b' '; SCRIPT_MAX_BYTES as usize + 1];
        fs::write(dir.path().join("widget.js"), js).unwrap();
        assert!(matches!(
            load(dir.path()),
            Err(ManifestError::TooLarge("widget.js", _))
        ));
    }

    #[test]
    fn widget_js_folder_is_not_a_file() {
        let dir = fixture(base(), false);
        fs::create_dir(dir.path().join("widget.js")).unwrap();
        assert!(matches!(
            load(dir.path()),
            Err(ManifestError::NotAFile("widget.js"))
        ));
    }

    #[test]
    fn unknown_format_version_is_error() {
        let mut j = base();
        j["formatVersion"] = json!(2);
        let dir = fixture(j, true);
        assert!(matches!(load(dir.path()), Err(ManifestError::Invalid(_))));
    }

    #[test]
    fn network_entries_must_be_bare_hostnames() {
        for bad in ["https://x.com/path", "x.com:8080"] {
            let mut j = base();
            j["permissions"] = json!({"network": [bad]});
            let dir = fixture(j, true);
            assert!(
                matches!(load(dir.path()), Err(ManifestError::Invalid(_))),
                "{bad}"
            );
        }
        let mut j = base();
        j["permissions"] = json!({"network": ["api.x.com"]});
        let dir = fixture(j, true);
        assert!(load(dir.path()).is_ok());
    }

    #[test]
    fn select_requires_options() {
        let mut j = base();
        j["settings"] = json!([{"key": "unit", "label": "Unit", "kind": "select", "options": []}]);
        let dir = fixture(j, true);
        assert!(matches!(load(dir.path()), Err(ManifestError::Invalid(_))));
    }
}
