//! Persisted widget instances and rotation groups (`instances.json`).

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::manifest::Manifest;

/// Lowest allowed group rotation interval in seconds; smaller values are raised.
pub const MIN_GROUP_INTERVAL_SECS: u32 = 3;
const DEFAULT_GROUP_INTERVAL_SECS: u32 = 8;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    /// `inst-` followed by 8 lowercase hex digits.
    pub id: String,
    /// Manifest id of the widget this is an instance of.
    pub widget: String,
    #[serde(default)]
    pub settings: Map<String, Value>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub max_width: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    /// `grp-` followed by 8 lowercase hex digits.
    pub id: String,
    /// Instance ids.
    #[serde(default)]
    pub members: Vec<String>,
    /// Seconds between rotations.
    #[serde(default = "default_interval")]
    pub interval: u32,
}

fn default_interval() -> u32 {
    DEFAULT_GROUP_INTERVAL_SECS
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Instances {
    #[serde(default)]
    pub instances: Vec<Instance>,
    #[serde(default)]
    pub groups: Vec<Group>,
    /// Instance or group ids, left to right.
    #[serde(default)]
    pub order: Vec<String>,
}

/// `<prefix>` followed by exactly 8 lowercase hex digits.
fn has_id_shape(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix).is_some_and(|hex| {
        hex.len() == 8
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn random_id(prefix: &str) -> String {
    format!("{prefix}{:08x}", rand::random::<u32>())
}

fn manifest_defaults(m: &Manifest) -> Map<String, Value> {
    m.settings
        .iter()
        .filter_map(|f| f.default.clone().map(|d| (f.key.clone(), d)))
        .collect()
}

/// Reads the file. A missing file gives the default; a corrupt one is renamed
/// to `<name>.bak-<unix seconds>` and the default is returned.
pub fn load(path: &Path) -> Instances {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Instances::default();
    };
    // Hand-edited files may start with a UTF-8 byte order mark.
    let json = text.strip_prefix('\u{feff}').unwrap_or(&text);
    match serde_json::from_str::<Instances>(json) {
        Ok(mut loaded) => {
            loaded.drop_malformed_ids();
            loaded
        }
        Err(_) => {
            let unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let mut name = path.file_name().unwrap_or_default().to_os_string();
            name.push(format!(".bak-{unix}"));
            let _ = std::fs::rename(path, path.with_file_name(name));
            Instances::default()
        }
    }
}

/// Writes `<path>.tmp` next to the target, then renames it over the target.
pub fn save(path: &Path, data: &Instances) -> io::Result<()> {
    let json = serde_json::to_string_pretty(data).map_err(io::Error::other)?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp: PathBuf = path.with_file_name(tmp_name);
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

impl Instances {
    /// Creates an instance of `m` with its default settings, appends it to
    /// `order` and returns its id.
    pub fn new_instance(&mut self, m: &Manifest) -> String {
        let id = loop {
            let candidate = random_id("inst-");
            if !self.id_in_use(&candidate) {
                break candidate;
            }
        };
        self.instances.push(Instance {
            id: id.clone(),
            widget: m.id.clone(),
            settings: manifest_defaults(m),
            hidden: false,
            max_width: None,
        });
        self.order.push(id.clone());
        id
    }

    fn id_in_use(&self, id: &str) -> bool {
        self.instances.iter().any(|i| i.id == id) || self.groups.iter().any(|g| g.id == id)
    }

    /// Manifest defaults overlaid with the instance's own settings.
    pub fn effective_settings(&self, inst: &Instance, m: &Manifest) -> Map<String, Value> {
        let mut out = manifest_defaults(m);
        for (k, v) in &inst.settings {
            out.insert(k.clone(), v.clone());
        }
        out
    }

    /// Drops instances and groups whose ids are malformed or repeated (first
    /// occurrence wins). Ids end up in file paths, so they are never trusted.
    fn drop_malformed_ids(&mut self) {
        let mut seen = HashSet::new();
        self.instances
            .retain(|i| has_id_shape(&i.id, "inst-") && seen.insert(i.id.clone()));
        self.groups
            .retain(|g| has_id_shape(&g.id, "grp-") && seen.insert(g.id.clone()));
    }

    /// Repairs the data against itself. Instances whose widget is not
    /// installed (or failed to load) are kept, so a broken widget folder
    /// never loses their settings on the next save; the scheduler leaves
    /// them out of the strip and does not run them.
    pub fn normalize(&mut self, known_widgets: &HashSet<String>) {
        self.drop_malformed_ids();
        for i in self
            .instances
            .iter()
            .filter(|i| !known_widgets.contains(&i.widget))
        {
            log::info!(
                "instance {} uses widget {:?}, which is not loaded; keeping it hidden from the strip",
                i.id,
                i.widget
            );
        }

        let instance_ids: HashSet<String> = self.instances.iter().map(|i| i.id.clone()).collect();
        let mut grouped: HashSet<String> = HashSet::new();
        for g in &mut self.groups {
            g.members
                .retain(|m| instance_ids.contains(m) && grouped.insert(m.clone()));
            g.interval = g.interval.max(MIN_GROUP_INTERVAL_SECS);
        }
        self.groups.retain(|g| !g.members.is_empty());

        let top_level: Vec<&str> = self
            .instances
            .iter()
            .map(|i| i.id.as_str())
            .filter(|id| !grouped.contains(*id))
            .chain(self.groups.iter().map(|g| g.id.as_str()))
            .collect();
        let valid: HashSet<&str> = top_level.iter().copied().collect();
        let mut in_order = HashSet::new();
        self.order
            .retain(|id| valid.contains(id.as_str()) && in_order.insert(id.clone()));
        for id in top_level {
            if in_order.insert(id.to_string()) {
                self.order.push(id.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(id: &str) -> Manifest {
        serde_json::from_value(json!({
            "id": id, "name": "W", "formatVersion": 1,
            "settings": [
                {"key": "unit", "label": "Unit", "kind": "text", "default": "C"},
                {"key": "city", "label": "City", "kind": "text"}
            ]
        }))
        .unwrap()
    }

    fn known(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn inst(id: &str, widget: &str) -> Instance {
        Instance {
            id: id.into(),
            widget: widget.into(),
            settings: Map::new(),
            hidden: false,
            max_width: None,
        }
    }

    fn group(id: &str, members: &[&str], interval: u32) -> Group {
        Group {
            id: id.into(),
            members: members.iter().map(|s| s.to_string()).collect(),
            interval,
        }
    }

    #[test]
    fn two_instances_of_same_widget_have_independent_settings() {
        let m = manifest("weather");
        let mut data = Instances::default();
        let a = data.new_instance(&m);
        let b = data.new_instance(&m);
        assert_ne!(a, b);
        data.instances[0].settings.insert("unit".into(), json!("F"));
        let ia = data.instances[0].clone();
        let ib = data.instances[1].clone();
        assert_eq!(data.effective_settings(&ia, &m)["unit"], json!("F"));
        assert_eq!(data.effective_settings(&ib, &m)["unit"], json!("C"));
    }

    #[test]
    fn new_instance_uses_manifest_defaults() {
        let m = manifest("weather");
        let mut data = Instances::default();
        let id = data.new_instance(&m);
        assert!(has_id_shape(&id, "inst-"));
        assert_eq!(data.order, vec![id.clone()]);
        let i = &data.instances[0];
        assert_eq!(i.widget, "weather");
        assert_eq!(i.settings["unit"], json!("C"));
        assert!(!i.settings.contains_key("city"));
    }

    #[test]
    fn normalize_keeps_unknown_widget_instances_and_drops_empty_groups() {
        let mut data = Instances {
            instances: vec![
                inst("inst-00000001", "weather"),
                inst("inst-00000002", "gone"),
            ],
            groups: vec![
                group("grp-00000001", &["inst-00000002", "inst-ffffffff"], 8),
                group("grp-00000002", &[], 8),
            ],
            order: vec!["grp-00000001".into()],
        };
        data.normalize(&known(&["weather"]));
        assert_eq!(data.instances.len(), 2);
        assert_eq!(data.groups.len(), 1);
        assert_eq!(data.groups[0].members, ["inst-00000002"]);
        assert_eq!(
            data.order,
            vec!["grp-00000001".to_string(), "inst-00000001".to_string()]
        );
    }

    #[test]
    fn unknown_widget_instance_survives_load_save_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instances.json");
        let mut data = Instances::default();
        let kept = data.new_instance(&manifest("weather"));
        let orphan = data.new_instance(&manifest("uninstalled"));
        data.instances[1].settings.insert("city".into(), json!("Oslo"));
        save(&path, &data).unwrap();

        let mut loaded = load(&path);
        loaded.normalize(&known(&["weather"]));
        save(&path, &loaded).unwrap();

        let again = load(&path);
        let ids: Vec<&str> = again.instances.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, [kept.as_str(), orphan.as_str()]);
        assert_eq!(again.instances[1].settings["city"], json!("Oslo"));
        assert_eq!(again.order, vec![kept, orphan]);
    }

    #[test]
    fn grouped_instance_not_in_top_level_order() {
        let mut data = Instances {
            instances: vec![
                inst("inst-00000001", "w"),
                inst("inst-00000002", "w"),
                inst("inst-00000003", "w"),
            ],
            groups: vec![group(
                "grp-00000001",
                &["inst-00000001", "inst-00000002"],
                8,
            )],
            order: vec!["inst-00000001".into(), "inst-00000003".into()],
        };
        data.normalize(&known(&["w"]));
        assert_eq!(
            data.order,
            vec!["inst-00000003".to_string(), "grp-00000001".to_string()]
        );
    }

    #[test]
    fn normalize_drops_malformed_ids() {
        let mut data = Instances {
            instances: vec![
                inst("../../evil", "w"),
                inst("inst-ABCDEF12", "w"),
                inst("inst-0000000", "w"),
                inst("inst-0000000a", "w"),
            ],
            groups: vec![
                group("grp-../../x", &["inst-0000000a"], 8),
                group("grp-0000000b", &["inst-0000000a"], 8),
            ],
            order: vec!["../../evil".into()],
        };
        data.normalize(&known(&["w"]));
        assert_eq!(data.instances.len(), 1);
        assert_eq!(data.groups.len(), 1);
        assert_eq!(data.groups[0].id, "grp-0000000b");
        assert_eq!(data.order, vec!["grp-0000000b".to_string()]);
    }

    #[test]
    fn normalize_dedupes_ids_and_clamps_interval() {
        let mut data = Instances {
            instances: vec![inst("inst-00000001", "w"), inst("inst-00000001", "w")],
            groups: vec![group("grp-00000001", &["inst-00000001"], 1)],
            order: vec!["grp-00000001".into(), "grp-00000001".into()],
        };
        data.normalize(&known(&["w"]));
        assert_eq!(data.instances.len(), 1);
        assert_eq!(data.groups[0].interval, 3);
        assert_eq!(data.order, vec!["grp-00000001".to_string()]);
    }

    #[test]
    fn group_interval_defaults_to_eight() {
        let data: Instances =
            serde_json::from_str(r#"{"groups":[{"id":"grp-00000001","members":[]}]}"#).unwrap();
        assert_eq!(data.groups[0].interval, 8);
    }

    #[test]
    fn corrupt_file_backed_up_and_defaulted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instances.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load(&path), Instances::default());
        assert!(!path.exists());
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("instances.json.bak-"))
            .collect();
        assert_eq!(backups.len(), 1);
    }

    #[test]
    fn hand_edited_file_with_byte_order_mark_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instances.json");
        let mut data = Instances::default();
        data.new_instance(&manifest("weather"));
        let json = serde_json::to_string(&data).unwrap();
        std::fs::write(&path, format!("\u{feff}{json}")).unwrap();
        assert_eq!(load(&path), data);
        assert!(path.exists());
    }

    #[test]
    fn missing_file_gives_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&dir.path().join("nope.json")), Instances::default());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instances.json");
        let mut data = Instances::default();
        let id = data.new_instance(&manifest("weather"));
        data.instances[0].max_width = Some(120);
        data.groups.push(group("grp-00000001", &[&id], 5));
        save(&path, &data).unwrap();
        assert!(!dir.path().join("instances.json.tmp").exists());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"maxWidth\""));
        assert_eq!(load(&path), data);
    }
}
