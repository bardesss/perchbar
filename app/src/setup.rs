//! First-run setup: the bundled widget templates and the default instances.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::instances::Instances;
use crate::manifest::Manifest;

/// A widget folder compiled into the binary.
pub struct Template {
    pub id: &'static str,
    /// (file name, contents)
    pub files: &'static [(&'static str, &'static str)],
}

macro_rules! template {
    ($id:literal) => {
        Template {
            id: $id,
            files: &[
                (
                    "widget.json",
                    include_str!(concat!("../../widgets/", $id, "/widget.json")),
                ),
                (
                    "widget.js",
                    include_str!(concat!("../../widgets/", $id, "/widget.js")),
                ),
                (
                    "README.md",
                    include_str!(concat!("../../widgets/", $id, "/README.md")),
                ),
            ],
        }
    };
}

/// The templates shipped with Perchbar, from the repo's `widgets/` folder.
pub const BUNDLED: &[Template] = &[template!("bitcoin-price"), template!("world-clock")];

/// Widgets a first run starts with, in strip order.
const FIRST_RUN: &[&str] = &["bitcoin-price", "world-clock"];

/// Copies each bundled template into `widgets_dir/<id>` unless that folder
/// already exists, so user edits are never overwritten. Returns the ids
/// copied. Each folder is written under a temporary name and renamed into
/// place, so an interrupted copy is redone on the next start.
pub fn install_bundled(widgets_dir: &Path) -> io::Result<Vec<&'static str>> {
    std::fs::create_dir_all(widgets_dir)?;
    let mut copied = Vec::new();
    for t in BUNDLED {
        let target = widgets_dir.join(t.id);
        if target.exists() {
            continue;
        }
        let partial: PathBuf = widgets_dir.join(format!(".{}.partial", t.id));
        if partial.exists() {
            std::fs::remove_dir_all(&partial)?;
        }
        std::fs::create_dir(&partial)?;
        for (name, contents) in t.files {
            std::fs::write(partial.join(name), contents)?;
        }
        std::fs::rename(&partial, &target)?;
        copied.push(t.id);
    }
    Ok(copied)
}

/// On a first run (no instances yet) adds one instance of each first-run
/// widget that is installed. Returns whether anything was added.
pub fn seed_first_run(
    data: &mut Instances,
    widgets: &HashMap<String, (Arc<Manifest>, PathBuf)>,
) -> bool {
    if !data.instances.is_empty() {
        return false;
    }
    let mut added = false;
    for id in FIRST_RUN {
        if let Some((m, _)) = widgets.get(*id) {
            data.new_instance(m);
            added = true;
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(id: &str) -> Arc<Manifest> {
        Arc::new(serde_json::from_value(json!({"id": id, "name": id, "formatVersion": 1})).unwrap())
    }

    fn widgets(ids: &[&str]) -> HashMap<String, (Arc<Manifest>, PathBuf)> {
        ids.iter()
            .map(|id| (id.to_string(), (manifest(id), PathBuf::from(id))))
            .collect()
    }

    #[test]
    fn install_copies_missing_templates_only() {
        let dir = tempfile::tempdir().unwrap();
        let widgets = dir.path().join("widgets");
        std::fs::create_dir_all(widgets.join("world-clock")).unwrap();
        std::fs::write(widgets.join("world-clock").join("widget.js"), "mine").unwrap();

        assert_eq!(install_bundled(&widgets).unwrap(), ["bitcoin-price"]);
        assert!(widgets.join("bitcoin-price").join("widget.json").is_file());
        assert_eq!(
            std::fs::read_to_string(widgets.join("world-clock").join("widget.js")).unwrap(),
            "mine"
        );
        assert!(install_bundled(&widgets).unwrap().is_empty());
    }

    #[test]
    fn install_redoes_an_interrupted_copy() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join(".bitcoin-price.partial");
        std::fs::create_dir_all(&partial).unwrap();
        std::fs::write(partial.join("junk"), "x").unwrap();
        install_bundled(dir.path()).unwrap();
        assert!(!partial.exists());
        assert!(!dir.path().join("bitcoin-price").join("junk").exists());
    }

    #[test]
    fn first_run_adds_bitcoin_then_clock() {
        let mut data = Instances::default();
        assert!(seed_first_run(
            &mut data,
            &widgets(&["world-clock", "bitcoin-price"])
        ));
        let kinds: Vec<&str> = data.instances.iter().map(|i| i.widget.as_str()).collect();
        assert_eq!(kinds, ["bitcoin-price", "world-clock"]);
        assert_eq!(data.order.len(), 2);
    }

    #[test]
    fn existing_instances_are_left_alone() {
        let w = widgets(&["bitcoin-price", "world-clock"]);
        let mut data = Instances::default();
        data.new_instance(&w["world-clock"].0);
        assert!(!seed_first_run(&mut data, &w));
        assert_eq!(data.instances.len(), 1);
    }

    #[test]
    fn missing_template_is_skipped() {
        let mut data = Instances::default();
        assert!(seed_first_run(&mut data, &widgets(&["world-clock"])));
        assert_eq!(data.instances.len(), 1);
        assert!(!seed_first_run(&mut Instances::default(), &widgets(&[])));
    }
}
