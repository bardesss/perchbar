//! The permissions the user approved per widget (`approvals.json`) and the
//! folders of local widgets (`local.json`).
//!
//! A widget runs only with permissions the user approved. One whose manifest
//! asks for more than its approval is registered blocked until the user
//! reviews the new permissions.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::manifest::{Manifest, Permissions};
use crate::scheduler::WidgetEntry;

/// Why a widget whose permissions grew does not run.
pub const REVIEW_REASON: &str = "Waiting for permission review";

/// Local widget folders by widget id. Paths are absolute.
pub type LocalFolders = BTreeMap<String, PathBuf>;

fn host_in(host: &str, list: &[String]) -> bool {
    list.iter().any(|h| h.eq_ignore_ascii_case(host))
}

fn exact_in(item: &str, list: &[String]) -> bool {
    list.iter().any(|x| x == item)
}

/// Whether every host, file and source in `asked` is in `approved`. Hosts
/// compare ASCII case-insensitively, files and sources exactly.
pub fn is_within(asked: &Permissions, approved: &Permissions) -> bool {
    asked.network.iter().all(|h| host_in(h, &approved.network))
        && asked.files.iter().all(|f| exact_in(f, &approved.files))
        && asked.sources.iter().all(|s| exact_in(s, &approved.sources))
}

/// What `asked` has that `approved` does not.
pub fn added(asked: &Permissions, approved: &Permissions) -> Permissions {
    let keep = |list: &[String], f: &dyn Fn(&str) -> bool| -> Vec<String> {
        list.iter().filter(|x| !f(x)).cloned().collect()
    };
    Permissions {
        network: keep(&asked.network, &|h| host_in(h, &approved.network)),
        files: keep(&asked.files, &|f| exact_in(f, &approved.files)),
        sources: keep(&asked.sources, &|s| exact_in(s, &approved.sources)),
    }
}

/// The scheduler entry for a widget given its approval. Within the approval
/// it runs with what its manifest asks (never more than approved);
/// otherwise it is blocked and keeps only the old approval. A widget that
/// asks for nothing is within any approval, even none.
pub fn entry(manifest: Arc<Manifest>, dir: PathBuf, approved: Option<&Permissions>) -> WidgetEntry {
    let approved = approved.cloned().unwrap_or_default();
    if is_within(&manifest.permissions, &approved) {
        WidgetEntry {
            approved: manifest.permissions.clone(),
            manifest,
            dir,
            blocked: None,
        }
    } else {
        WidgetEntry {
            approved,
            manifest,
            dir,
            blocked: Some(REVIEW_REASON.to_string()),
        }
    }
}

/// The approved permissions per widget id.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Approvals {
    by_widget: BTreeMap<String, Permissions>,
    /// The file could not be read: it is never overwritten.
    unreadable: bool,
}

impl Approvals {
    /// Reads `approvals.json`. Missing gives none; a corrupt file is moved
    /// aside, so its widgets wait for review again. A file that cannot be
    /// read (or moved aside) gives none and is never saved over.
    pub fn load(path: &Path) -> Approvals {
        match read_json(path) {
            Ok(by_widget) => Approvals {
                by_widget,
                unreadable: false,
            },
            Err(e) => {
                log::warn!(
                    "cannot read {}: {e}; approvals will not be saved",
                    path.display()
                );
                Approvals {
                    by_widget: BTreeMap::new(),
                    unreadable: true,
                }
            }
        }
    }

    /// Whether `save` may write the file.
    pub fn is_readable(&self) -> bool {
        !self.unreadable
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if self.unreadable {
            return Err(unreadable_error(path));
        }
        write_json(path, &self.by_widget)
    }

    pub fn get(&self, id: &str) -> Option<Permissions> {
        self.by_widget.get(id).cloned()
    }

    pub fn set(&mut self, id: &str, permissions: Permissions) {
        self.by_widget.insert(id.to_string(), permissions);
    }

    pub fn remove(&mut self, id: &str) {
        self.by_widget.remove(id);
    }
}

pub(crate) fn unreadable_error(path: &Path) -> io::Error {
    io::Error::other(format!(
        "{} could not be read at startup, so it is not overwritten",
        path.display()
    ))
}

/// Reads `local.json`. Entries that are not absolute paths are dropped.
/// An error means the file exists but cannot be read; it must not be
/// saved over.
pub fn load_local(path: &Path) -> io::Result<LocalFolders> {
    let mut local: LocalFolders = read_json(path)?;
    local.retain(|id, dir| {
        let ok = dir.is_absolute();
        if !ok {
            log::warn!(
                "{}: ignoring {id}, its folder is not absolute",
                path.display()
            );
        }
        ok
    });
    Ok(local)
}

pub fn save_local(path: &Path, local: &LocalFolders) -> io::Result<()> {
    write_json(path, local)
}

/// Missing gives the default; a corrupt file is renamed to
/// `<name>.bak-<unix seconds>` and gives the default. Any other read
/// error, or a corrupt file that cannot be moved aside, is an error.
fn read_json<T: DeserializeOwned + Default>(path: &Path) -> io::Result<T> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(T::default()),
        Err(e) => return Err(e),
    };
    let json = text.strip_prefix('\u{feff}').unwrap_or(&text);
    match serde_json::from_str(json) {
        Ok(v) => Ok(v),
        Err(e) => {
            log::warn!("{} is not valid ({e}); starting without it", path.display());
            let unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let mut name = path.file_name().unwrap_or_default().to_os_string();
            name.push(format!(".bak-{unix}"));
            std::fs::rename(path, path.with_file_name(name))?;
            Ok(T::default())
        }
    }
}

/// Writes `<path>.tmp` next to the target, then renames it over the target.
fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perms(network: &[&str], files: &[&str], sources: &[&str]) -> Permissions {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect();
        Permissions {
            network: v(network),
            files: v(files),
            sources: v(sources),
        }
    }

    #[test]
    fn within_when_subset() {
        let ab = perms(&["a.com", "b.com"], &[], &[]);
        assert!(is_within(&perms(&["a.com"], &[], &[]), &ab));
        assert!(!is_within(&perms(&["c.com"], &[], &[]), &ab));
        assert!(is_within(&perms(&["A.COM"], &[], &[]), &ab));
        assert!(is_within(&Permissions::default(), &Permissions::default()));
        // Files and sources count too, compared exactly.
        assert!(!is_within(&perms(&[], &["x.txt"], &[]), &ab));
        assert!(!is_within(&perms(&[], &[], &["cpu"]), &ab));
        let all = perms(&["a.com"], &["x.txt"], &["cpu"]);
        assert!(is_within(&perms(&[], &["x.txt"], &["cpu"]), &all));
        assert!(!is_within(&perms(&[], &["X.txt"], &[]), &all));
    }

    #[test]
    fn unreadable_files_are_never_overwritten() {
        // A folder where the file goes: reading fails with something other
        // than "not found".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), "x").unwrap();
        let mut a = Approvals::load(&path);
        assert!(!a.is_readable());
        a.set("x", Permissions::default());
        assert!(a.save(&path).is_err());
        assert!(path.join("keep").is_file());
        assert!(load_local(&path).is_err());
        // Missing is fine: nothing to lose.
        let missing = dir.path().join("none.json");
        assert!(Approvals::load(&missing).is_readable());
        assert!(load_local(&missing).unwrap().is_empty());
    }

    #[test]
    fn empty_permissions_are_never_blocked() {
        let m: Manifest = serde_json::from_value(serde_json::json!(
            {"id": "x", "name": "X", "formatVersion": 1}
        ))
        .unwrap();
        let e = entry(Arc::new(m.clone()), PathBuf::from("x"), None);
        assert_eq!(e.blocked, None);
        let mut asks = m;
        asks.permissions.network = vec!["a.com".into()];
        let e = entry(Arc::new(asks), PathBuf::from("x"), None);
        assert_eq!(e.blocked.as_deref(), Some(REVIEW_REASON));
        assert_eq!(e.approved, Permissions::default());
    }

    #[test]
    fn added_lists_only_what_is_new() {
        let asked = perms(&["a.com", "B.com", "c.com"], &["f"], &["cpu"]);
        let approved = perms(&["A.com", "b.com"], &["f"], &[]);
        assert_eq!(added(&asked, &approved), perms(&["c.com"], &[], &["cpu"]));
    }

    #[test]
    fn approvals_round_trip_and_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("approvals.json");
        let mut a = Approvals::load(&path);
        assert_eq!(a.get("x"), None);
        a.set("x", perms(&["a.com"], &[], &[]));
        a.save(&path).unwrap();
        let back = Approvals::load(&path);
        assert_eq!(back.get("x"), Some(perms(&["a.com"], &[], &[])));

        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Approvals::load(&path).get("x"), None);
        // The broken file is kept aside, not overwritten.
        let kept = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("approvals.json.bak-")
            });
        assert!(kept);
    }

    #[test]
    fn local_folders_drop_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local.json");
        let abs = dir.path().join("w");
        let json = serde_json::json!({ "good": abs, "bad": "relative\\w" });
        std::fs::write(&path, json.to_string()).unwrap();
        let local = load_local(&path).unwrap();
        assert_eq!(local.len(), 1);
        assert_eq!(local["good"], abs);
        save_local(&path, &local).unwrap();
        assert_eq!(load_local(&path).unwrap(), local);
    }
}
