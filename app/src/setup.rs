//! First-run setup: the bundled widget templates and the default instances.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::approvals::{self, Approvals, LocalFolders};
use crate::instances::{self, Instances};
use crate::manifest::{self, Manifest};
use crate::paths;
use crate::scheduler::WidgetEntry;
use crate::secrets::{self, SecretStore};

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
/// place, so an interrupted copy is redone on the next start. A template
/// that cannot be copied is logged and skipped; the others still install.
pub fn install_bundled(widgets_dir: &Path) -> io::Result<Vec<&'static str>> {
    std::fs::create_dir_all(widgets_dir)?;
    let mut copied = Vec::new();
    for t in BUNDLED {
        let target = widgets_dir.join(t.id);
        if target.exists() {
            continue;
        }
        match install_one(widgets_dir, t, &target) {
            Ok(()) => copied.push(t.id),
            Err(e) => log::warn!("cannot install the {} template: {e}", t.id),
        }
    }
    Ok(copied)
}

fn install_one(widgets_dir: &Path, t: &Template, target: &Path) -> io::Result<()> {
    let partial: PathBuf = widgets_dir.join(format!(".{}.partial", t.id));
    if partial.exists() {
        std::fs::remove_dir_all(&partial)?;
    }
    std::fs::create_dir(&partial)?;
    for (name, contents) in t.files {
        std::fs::write(partial.join(name), contents)?;
    }
    std::fs::rename(&partial, target)
}

/// Installed widgets by manifest id. Folders that fail to load, and folders
/// whose name starts with '.', are skipped.
pub fn load_widgets(dir: &Path) -> HashMap<String, (Arc<Manifest>, PathBuf)> {
    let mut widgets = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        log::info!("no widgets folder at {}", dir.display());
        return widgets;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Dot folders are partial template copies or hidden user folders.
        if !path.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        match manifest::load(&path) {
            Ok(m) if widgets.contains_key(&m.id) => {
                log::warn!("skipping {}: duplicate widget id {}", path.display(), m.id);
            }
            Ok(m) => {
                widgets.insert(m.id.clone(), (Arc::new(m), path));
            }
            Err(e) => log::warn!("skipping widget {}: {e}", path.display()),
        }
    }
    widgets
}

/// Records the bundled templates' own permissions as approved: for the
/// templates just `copied`, and for any template with no approval yet (an
/// install from before approvals existed). Returns whether anything changed.
pub fn approve_bundled(approvals: &mut Approvals, copied: &[&str]) -> bool {
    let mut changed = false;
    for t in BUNDLED {
        if !copied.contains(&t.id) && approvals.get(t.id).is_some() {
            continue;
        }
        let Some((_, json)) = t.files.iter().find(|(name, _)| *name == "widget.json") else {
            continue;
        };
        match serde_json::from_str::<Manifest>(json) {
            Ok(m) => {
                if approvals.get(t.id).as_ref() != Some(&m.permissions) {
                    approvals.set(t.id, m.permissions);
                    changed = true;
                }
            }
            Err(e) => log::warn!("the {} template does not parse: {e}", t.id),
        }
    }
    changed
}

/// Adds the local widgets in `local` to `widgets`. One whose folder is
/// missing or broken, or whose manifest has another id, is logged and left
/// out; its instances stay in the data and its entry stays, so the folder
/// can come back. Returns the ids already taken by an installed widget (a
/// newer bundled template, or the folder copied into `widgets\`): those
/// entries no longer name the widget with that id and should be dropped.
pub fn add_local(
    widgets: &mut HashMap<String, (Arc<Manifest>, PathBuf)>,
    local: &LocalFolders,
) -> Vec<String> {
    let mut shadowed = Vec::new();
    for (id, dir) in local {
        if widgets.contains_key(id) {
            log::warn!(
                "local widget {id} in {} is shadowed by the installed widget with that id; dropping it from local.json",
                dir.display()
            );
            shadowed.push(id.clone());
            continue;
        }
        match manifest::load(dir) {
            Ok(m) if &m.id != id => log::warn!(
                "skipping local widget {id}: {} now has the id {}",
                dir.display(),
                m.id
            ),
            Ok(m) => {
                widgets.insert(id.clone(), (Arc::new(m), dir.clone()));
            }
            Err(e) => log::warn!("skipping local widget {id} in {}: {e}", dir.display()),
        }
    }
    shadowed
}

/// Drops the `shadowed` ids (from `add_local`) from `local`. For an id
/// that is not a bundled template, the approval is dropped too: it was
/// given to the local folder, so the widget now loaded under that id gets
/// a first review. Templates keep theirs (see `approve_bundled`). Returns
/// whether the approvals changed.
pub fn drop_shadowed(
    local: &mut LocalFolders,
    approvals: &mut Approvals,
    shadowed: &[String],
) -> bool {
    let mut changed = false;
    for id in shadowed {
        local.remove(id);
        let bundled = BUNDLED.iter().any(|t| t.id == id);
        if !bundled && approvals.get(id).is_some() {
            approvals.remove(id);
            changed = true;
        }
    }
    changed
}

/// The scheduler entries: each widget runs with its approved permissions,
/// or waits for review when it asks for more.
pub fn entries(
    widgets: HashMap<String, (Arc<Manifest>, PathBuf)>,
    approvals: &Approvals,
) -> HashMap<String, WidgetEntry> {
    widgets
        .into_iter()
        .map(|(id, (m, dir))| {
            let approved = approvals.get(&id);
            let e = approvals::entry(m, dir, approved.as_ref());
            if e.blocked.is_some() {
                log::info!("{id} asks for permissions not approved yet; waiting for review");
            }
            (id, e)
        })
        .collect()
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

/// What Perchbar starts with: the scheduler entries, the instance data and
/// the secret store.
pub struct Boot {
    pub widgets: HashMap<String, WidgetEntry>,
    pub data: Instances,
    pub secrets: SecretStore,
}

/// Prepares a start over `data_dir`: installs missing templates (approving
/// their permissions), loads the installed and local widgets, loads the
/// instances (seeding a first run) and moves any secret out of
/// `instances.json`. Problems are logged; a start always gets a `Boot`.
pub fn boot(data_dir: &Path) -> Boot {
    let widgets_dir = data_dir.join(paths::WIDGETS_DIR);
    let approvals_path = data_dir.join(paths::APPROVALS_FILE);
    let local_path = data_dir.join(paths::LOCAL_FILE);
    let instances_path = data_dir.join(paths::INSTANCES_FILE);
    let save_approvals = |approvals: &Approvals| {
        if let Err(e) = approvals.save(&approvals_path) {
            log::warn!("cannot save {}: {e}", approvals_path.display());
        }
    };
    let save_instances = |data: &Instances| {
        if let Err(e) = instances::save(&instances_path, data) {
            log::warn!("cannot save {}: {e}", instances_path.display());
        }
    };

    let copied = match install_bundled(&widgets_dir) {
        Ok(copied) => {
            if !copied.is_empty() {
                log::info!("installed templates: {copied:?}");
            }
            copied
        }
        Err(e) => {
            log::warn!("cannot install bundled templates: {e}");
            Vec::new()
        }
    };
    let mut approvals = Approvals::load(&approvals_path);
    if approve_bundled(&mut approvals, &copied) {
        save_approvals(&approvals);
    }

    // Installed widgets first; a local one with a taken id is skipped and
    // dropped from local.json, so it is never mistaken for the installed one.
    let mut widgets = load_widgets(&widgets_dir);
    match approvals::load_local(&local_path) {
        Ok(mut local) => {
            let shadowed = add_local(&mut widgets, &local);
            if !shadowed.is_empty() {
                if drop_shadowed(&mut local, &mut approvals, &shadowed) {
                    save_approvals(&approvals);
                }
                if let Err(e) = approvals::save_local(&local_path, &local) {
                    log::warn!("cannot save {}: {e}", local_path.display());
                }
            }
        }
        Err(e) => log::warn!(
            "cannot read {}: {e}; local widgets are left out",
            local_path.display()
        ),
    }
    let known: HashSet<String> = widgets.keys().cloned().collect();
    let mut data = instances::load(&instances_path);
    // No file after loading: a first run, or a corrupt file that `load` moved
    // aside. A saved empty strip (the user removed everything) stays empty.
    let first_run = !instances_path.exists();
    data.normalize(&known);
    if first_run && seed_first_run(&mut data, &widgets) {
        log::info!("first run: added the default widgets");
        save_instances(&data);
    }
    log::info!(
        "{} widget(s), {} instance(s)",
        widgets.len(),
        data.instances.len()
    );

    // Secret settings never stay in instances.json: move any found there
    // (hand edits, older builds) into the encrypted store.
    let secrets = SecretStore::new(data_dir.join(paths::SECRETS_DIR));
    let manifests: HashMap<String, Arc<Manifest>> = widgets
        .iter()
        .map(|(id, (m, _))| (id.clone(), m.clone()))
        .collect();
    if secrets::migrate(&mut data, &manifests, &secrets) {
        log::info!("moved secret settings out of instances.json");
        save_instances(&data);
    }

    // Each widget runs with the permissions the user approved; one that
    // asks for more waits for review in the settings window.
    Boot {
        widgets: entries(widgets, &approvals),
        data,
        secrets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Permissions;
    use serde_json::json;
    use std::collections::HashSet;

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
    fn install_logs_a_failing_template_and_installs_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the partial folder goes: removing it as a folder fails.
        std::fs::write(dir.path().join(".bitcoin-price.partial"), "x").unwrap();
        assert_eq!(install_bundled(dir.path()).unwrap(), ["world-clock"]);
        assert!(dir.path().join("world-clock").join("widget.js").is_file());
        assert!(!dir.path().join("bitcoin-price").exists());
    }

    #[test]
    fn load_widgets_skips_dot_folders() {
        let dir = tempfile::tempdir().unwrap();
        install_bundled(dir.path()).unwrap();
        let hidden = dir.path().join(".leftover");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(
            hidden.join("widget.json"),
            r#"{"id":"leftover","name":"L","formatVersion":1}"#,
        )
        .unwrap();
        std::fs::write(
            hidden.join("widget.js"),
            "export default async function update() {}",
        )
        .unwrap();
        let mut ids: Vec<String> = load_widgets(dir.path()).into_keys().collect();
        ids.sort();
        assert_eq!(ids, ["bitcoin-price", "world-clock"]);
    }

    #[test]
    fn bundled_templates_are_approved() {
        let dir = tempfile::tempdir().unwrap();
        let copied = install_bundled(dir.path()).unwrap();
        let mut approvals = Approvals::default();
        assert!(approve_bundled(&mut approvals, &copied));
        let btc = approvals.get("bitcoin-price").unwrap();
        assert_eq!(btc.network, ["api.coinbase.com"]);
        assert!(approvals.get("world-clock").is_some());
        // Nothing new the second time.
        assert!(!approve_bundled(&mut approvals, &[]));

        // Installed with their approvals, the templates run unblocked.
        let entries = entries(load_widgets(dir.path()), &approvals);
        assert_eq!(entries["bitcoin-price"].blocked, None);
        assert_eq!(entries["bitcoin-price"].approved, btc);

        // A template edited to ask for more waits for review.
        let json = dir.path().join("bitcoin-price").join("widget.json");
        let text = std::fs::read_to_string(&json)
            .unwrap()
            .replace("api.coinbase.com", "evil.example.com");
        std::fs::write(&json, text).unwrap();
        let entries = super::entries(load_widgets(dir.path()), &approvals);
        assert!(entries["bitcoin-price"].blocked.is_some());
        assert!(entries["bitcoin-price"]
            .approved
            .network
            .iter()
            .all(|h| h != "evil.example.com"));
    }

    #[test]
    fn missing_local_folder_keeps_instances() {
        let dir = tempfile::tempdir().unwrap();
        let widgets_dir = dir.path().join("widgets");
        install_bundled(&widgets_dir).unwrap();
        let gone = dir.path().join("gone");
        let mut local = LocalFolders::new();
        local.insert("gone-widget".into(), gone);
        // A local folder with the id of an installed widget is skipped too.
        let clash = dir.path().join("clash");
        std::fs::create_dir_all(&clash).unwrap();
        std::fs::write(
            clash.join("widget.json"),
            r#"{"id":"world-clock","name":"Fake","formatVersion":1,"permissions":{"network":["evil.example.com"]}}"#,
        )
        .unwrap();
        std::fs::write(clash.join("widget.js"), "").unwrap();
        local.insert("world-clock".into(), clash);

        let mut widgets = load_widgets(&widgets_dir);
        // Only the collision is reported for dropping; a missing folder is not.
        assert_eq!(add_local(&mut widgets, &local), ["world-clock"]);
        assert!(!widgets.contains_key("gone-widget"));
        assert_eq!(widgets["world-clock"].1, widgets_dir.join("world-clock"));

        let mut data = Instances::default();
        let m = manifest("gone-widget");
        let id = data.new_instance(&m);
        let known: HashSet<String> = widgets.keys().cloned().collect();
        data.normalize(&known);
        assert!(data.instances.iter().any(|i| i.id == id));
    }

    fn user_folder(parent: &Path, name: &str, id: &str, hosts: &[&str]) -> PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let json = json!({"id": id, "name": "Mine", "formatVersion": 1, "permissions": {"network": hosts}});
        std::fs::write(dir.join("widget.json"), json.to_string()).unwrap();
        std::fs::write(dir.join("widget.js"), "").unwrap();
        dir
    }

    #[test]
    fn local_widget_shadowed_after_a_template_upgrade() {
        // The user loaded their own "world-clock" before Perchbar shipped one.
        let dir = tempfile::tempdir().unwrap();
        let widgets_dir = dir.path().join("widgets");
        let mine = user_folder(dir.path(), "mine", "world-clock", &["evil.example.com"]);
        let mut local = LocalFolders::from([("world-clock".to_string(), mine)]);
        // The local widget's approval is wider than the template's.
        let mut approvals = Approvals::default();
        approvals.set(
            "world-clock",
            Permissions {
                network: vec!["evil.example.com".into()],
                ..Permissions::default()
            },
        );
        let copied = install_bundled(&widgets_dir).unwrap();
        assert!(copied.contains(&"world-clock"));
        approve_bundled(&mut approvals, &copied);
        let mut widgets = load_widgets(&widgets_dir);
        let shadowed = add_local(&mut widgets, &local);
        assert_eq!(shadowed, ["world-clock"]);
        assert_eq!(widgets["world-clock"].1, widgets_dir.join("world-clock"));
        drop_shadowed(&mut local, &mut approvals, &shadowed);
        assert!(local.is_empty());
        // approve_bundled replaced the wider approval with the template's.
        assert_eq!(approvals.get("world-clock"), Some(Permissions::default()));
        let entries = entries(widgets, &approvals);
        assert!(entries["world-clock"].approved.network.is_empty());
        assert_eq!(entries["world-clock"].blocked, None);
    }

    #[test]
    fn local_widget_copied_into_widgets_dir() {
        let dir = tempfile::tempdir().unwrap();
        let widgets_dir = dir.path().join("widgets");
        let mine = user_folder(dir.path(), "mine", "mine", &["a.example.com"]);
        user_folder(&widgets_dir, "mine", "mine", &["a.example.com"]);
        let mut local = LocalFolders::from([("mine".to_string(), mine)]);
        let mut approvals = Approvals::default();
        approvals.set(
            "mine",
            Permissions {
                network: vec!["a.example.com".into()],
                ..Permissions::default()
            },
        );
        let mut widgets = load_widgets(&widgets_dir);
        let shadowed = add_local(&mut widgets, &local);
        assert_eq!(shadowed, ["mine"]);
        assert_eq!(widgets["mine"].1, widgets_dir.join("mine"));
        // Not a template: the copy gets a first review of its own.
        assert!(drop_shadowed(&mut local, &mut approvals, &shadowed));
        assert!(local.is_empty());
        assert_eq!(approvals.get("mine"), None);
        let entries = entries(widgets, &approvals);
        assert_eq!(
            entries["mine"].blocked.as_deref(),
            Some(crate::approvals::REVIEW_REASON)
        );
    }

    #[test]
    fn local_widget_without_approval_is_blocked() {
        // approvals.json is corrupt: it is moved aside and nothing is approved.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.json");
        std::fs::write(&path, "{ broken").unwrap();
        let approvals = Approvals::load(&path);
        assert!(approvals.is_readable());
        let mine = user_folder(dir.path(), "mine", "mine", &["a.example.com"]);
        let local = LocalFolders::from([("mine".to_string(), mine)]);
        let mut widgets = HashMap::new();
        assert!(add_local(&mut widgets, &local).is_empty());
        let entries = entries(widgets, &approvals);
        assert_eq!(
            entries["mine"].blocked.as_deref(),
            Some(crate::approvals::REVIEW_REASON)
        );
        assert!(entries["mine"].approved.network.is_empty());
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
