//! The commands the settings window calls (Tauri IPC).
//!
//! Each command body is a plain function over `&AppState`, so tests drive
//! them with a scheduler on a fake runner and a temporary folder. The
//! `#[tauri::command]` wrappers in [`ipc`] only forward.
//!
//! Every change to the instance data starts from `snapshot()`, edits a copy,
//! normalizes it and hands it to `apply`. Ids from the page are checked
//! against the snapshot first; an unknown id is an error.
//!
//! Secret values never appear in `get_state`, in an error or in a log line.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::display::{Block, Output};
use crate::instances::{Group, Instance, Instances};
use crate::logbook::{LogBook, LogLevel};
use crate::manifest::{Manifest, Permissions, SettingField, SettingKind};
use crate::prefs::{self, AppPrefs, Autostart};
use crate::scheduler::{SchedulerHandle, WidgetEntry};
use crate::secrets::{SecretStore, SECRET_MAX_CHARS};
use crate::strip::model::SlotState;
use crate::validate::{self, FieldError};

/// What a failed command tells the page. Never holds a secret value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandError {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<FieldError>>,
}

impl CommandError {
    fn new(message: impl Into<String>) -> CommandError {
        CommandError {
            message: message.into(),
            fields: None,
        }
    }

    fn fields(message: impl Into<String>, fields: Vec<FieldError>) -> CommandError {
        CommandError {
            message: message.into(),
            fields: Some(fields),
        }
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CommandError {}

pub type CommandResult<T> = Result<T, CommandError>;

/// Shared state behind the commands (Tauri managed state).
pub struct AppState {
    sched: SchedulerHandle,
    /// The one store on the secrets folder; the scheduler holds a clone.
    secrets: SecretStore,
    logbook: LogBook,
    autostart: Arc<dyn Autostart + Send + Sync>,
    /// The data folder: `app.json`, `storage\` and the log file live here.
    data_dir: PathBuf,
    /// Local widget folders by widget id. Filled by the local widget
    /// loading (with approvals) later; empty until then.
    pub local_folders: Mutex<HashMap<String, PathBuf>>,
}

/// One installed widget, as the page lists it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WidgetView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub author: String,
    pub version: String,
    pub settings: Vec<SettingField>,
    pub permissions: Permissions,
    pub local: bool,
    pub blocked: Option<String>,
    pub folder: Option<String>,
}

/// Where an instance is, in words the page can show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusView {
    /// `loading`, `ok`, `stale`, `error`, `hidden` or `blocked`.
    pub state: String,
    /// The first text block of the frame shown on the strip.
    pub text: Option<String>,
    pub message: Option<String>,
}

/// One instance. `settings` never holds a secret; `secrets_set` names the
/// secret keys that have a stored value.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceView {
    pub id: String,
    pub widget: String,
    pub settings: Map<String, Value>,
    pub secrets_set: Vec<String>,
    pub secrets_unreadable: bool,
    pub hidden: bool,
    pub max_width: Option<u32>,
    pub status: StatusView,
}

/// Everything the settings page shows.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateView {
    pub widgets: Vec<WidgetView>,
    pub instances: Vec<InstanceView>,
    pub groups: Vec<Group>,
    pub order: Vec<String>,
    pub autostart: bool,
    pub version: String,
    pub log_folder: String,
}

/// One log book line for the page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLineView {
    /// Local time, RFC 3339.
    pub at: String,
    /// `info`, `warn` or `error`.
    pub level: String,
    pub text: String,
}

impl StatusView {
    fn plain(state: &str, message: Option<String>) -> StatusView {
        StatusView {
            state: state.to_string(),
            text: None,
            message,
        }
    }

    /// The status of an instance from its slot state.
    pub fn from_state(state: &SlotState) -> StatusView {
        match state {
            SlotState::Loading => StatusView::plain("loading", None),
            SlotState::Ok(out) => StatusView {
                state: "ok".into(),
                text: first_text(out),
                message: None,
            },
            SlotState::Stale(out) => StatusView {
                state: "stale".into(),
                text: first_text(out),
                message: None,
            },
            SlotState::Error { message, last } => StatusView {
                state: "error".into(),
                text: last.as_ref().and_then(first_text),
                message: Some(message.clone()),
            },
        }
    }
}

/// The first text block of the first frame (the one the strip shows first).
fn first_text(out: &Output) -> Option<String> {
    fn find(b: &Block) -> Option<&str> {
        match b {
            Block::Text { value, .. } => Some(value),
            Block::Stack { top, bottom } => find(top).or_else(|| find(bottom)),
            _ => None,
        }
    }
    out.frames
        .first()?
        .blocks
        .iter()
        .find_map(find)
        .map(str::to_string)
}

fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

fn unknown_instance() -> CommandError {
    CommandError::new("This widget is no longer on the taskbar")
}

fn unknown_group() -> CommandError {
    CommandError::new("This group no longer exists")
}

fn unknown_widget() -> CommandError {
    CommandError::new("This widget is not installed")
}

fn find_instance<'a>(data: &'a Instances, id: &str) -> CommandResult<&'a Instance> {
    data.instances
        .iter()
        .find(|i| i.id == id)
        .ok_or_else(unknown_instance)
}

fn find_instance_mut<'a>(data: &'a mut Instances, id: &str) -> CommandResult<&'a mut Instance> {
    data.instances
        .iter_mut()
        .find(|i| i.id == id)
        .ok_or_else(unknown_instance)
}

/// `grp-` and 8 lowercase hex digits not used by any instance or group.
fn new_group_id(data: &Instances) -> String {
    loop {
        let id = format!("grp-{:08x}", rand::random::<u32>());
        let used =
            data.instances.iter().any(|i| i.id == id) || data.groups.iter().any(|g| g.id == id);
        if !used {
            return id;
        }
    }
}

/// Checks a group's new members: known instances, no repeats, not in a
/// group other than `own_group`.
fn check_members(
    data: &Instances,
    members: &[String],
    own_group: Option<&str>,
) -> CommandResult<()> {
    let mut seen = HashSet::new();
    for m in members {
        find_instance(data, m)?;
        if !seen.insert(m.as_str()) {
            return Err(CommandError::new("A widget can be in a group only once"));
        }
        let elsewhere = data
            .groups
            .iter()
            .any(|g| Some(g.id.as_str()) != own_group && g.members.contains(m));
        if elsewhere {
            return Err(CommandError::new("A widget can be in only one group"));
        }
    }
    Ok(())
}

impl AppState {
    pub fn new(
        sched: SchedulerHandle,
        secrets: SecretStore,
        logbook: LogBook,
        autostart: Arc<dyn Autostart + Send + Sync>,
        data_dir: PathBuf,
    ) -> AppState {
        AppState {
            sched,
            secrets,
            logbook,
            autostart,
            data_dir,
            local_folders: Mutex::default(),
        }
    }

    /// Edits the instance data under the scheduler's state lock, the one
    /// lock every edit (the strip's too) goes through. Nothing changes when
    /// `f` fails. `f` must not call into the scheduler.
    fn edit<T>(&self, f: impl FnOnce(&mut Instances) -> CommandResult<T>) -> CommandResult<T> {
        self.sched.try_edit(f)
    }

    /// The manifest of an installed widget.
    fn manifest(&self, widget: &str) -> CommandResult<Arc<Manifest>> {
        self.sched
            .widgets()
            .remove(widget)
            .map(|w| w.manifest)
            .ok_or_else(unknown_widget)
    }

    /// The instance's widget manifest, checking the instance exists.
    fn instance_manifest(&self, id: &str) -> CommandResult<Arc<Manifest>> {
        let widget = find_instance(&self.sched.snapshot(), id)?.widget.clone();
        self.manifest(&widget)
    }

    /// Checks that the instance exists and `key` is one of its secret fields.
    fn secret_field(&self, id: &str, key: &str) -> CommandResult<()> {
        let m = self.instance_manifest(id)?;
        let is_secret = m
            .settings
            .iter()
            .any(|f| f.key == key && f.kind == SettingKind::Secret);
        if is_secret {
            Ok(())
        } else {
            Err(CommandError::new("This widget has no such secret setting"))
        }
    }

    fn storage_file(&self, id: &str) -> PathBuf {
        self.data_dir.join("storage").join(format!("{id}.json"))
    }

    fn prefs_file(&self) -> PathBuf {
        self.data_dir.join("app.json")
    }

    /// The folder holding `perchbar.log` (the data folder).
    pub fn log_folder(&self) -> PathBuf {
        self.data_dir.clone()
    }

    pub fn get_state(&self) -> CommandResult<StateView> {
        let data = self.sched.snapshot();
        let entries = self.sched.widgets();
        let statuses = self.sched.statuses();
        let local = self
            .local_folders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();

        let mut widgets: Vec<WidgetView> = entries
            .iter()
            .map(|(id, e)| widget_view(id, e, local.get(id)))
            .collect();
        widgets.sort_by(|a, b| {
            let by_name = a.name.to_lowercase().cmp(&b.name.to_lowercase());
            by_name.then_with(|| a.id.cmp(&b.id))
        });

        let instances = data
            .instances
            .iter()
            .map(|inst| {
                let entry = entries.get(&inst.widget);
                let secret_keys: HashSet<&str> = entry
                    .map(|e| {
                        e.manifest
                            .settings
                            .iter()
                            .filter(|f| f.kind == SettingKind::Secret)
                            .map(|f| f.key.as_str())
                            .collect()
                    })
                    .unwrap_or_default();
                // Without its manifest nothing tells which settings are
                // secret (an unmigrated one may sit in instances.json), so
                // an instance of a widget that is not installed shows none.
                let settings: Map<String, Value> = match entry {
                    Some(_) => inst
                        .settings
                        .iter()
                        .filter(|(k, _)| !secret_keys.contains(k.as_str()))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    None => Map::new(),
                };
                // Only the key names leave this block; the values are dropped.
                let (secrets_set, secrets_unreadable) = {
                    let load = self.secrets.load(&inst.id);
                    let mut keys: Vec<String> = load.values.keys().cloned().collect();
                    keys.sort();
                    (keys, load.unreadable)
                };
                let status = if inst.hidden {
                    StatusView::plain("hidden", None)
                } else if let Some(reason) = entry.and_then(|e| e.blocked.clone()) {
                    StatusView::plain("blocked", Some(reason))
                } else if let Some(state) = statuses.get(&inst.id) {
                    StatusView::from_state(state)
                } else {
                    StatusView::plain("error", Some(unknown_widget().message))
                };
                InstanceView {
                    id: inst.id.clone(),
                    widget: inst.widget.clone(),
                    settings,
                    secrets_set,
                    secrets_unreadable,
                    hidden: inst.hidden,
                    max_width: inst.max_width,
                    status,
                }
            })
            .collect();

        Ok(StateView {
            widgets,
            instances,
            groups: data.groups,
            order: data.order,
            autostart: self.autostart.enabled(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            log_folder: self.log_folder().display().to_string(),
        })
    }

    pub fn add_instance(&self, widget: &str) -> CommandResult<String> {
        let m = self.manifest(widget)?;
        self.edit(|data| Ok(data.new_instance(&m)))
    }

    /// Removes the instance with its storage file, secrets and log book.
    pub fn remove_instance(&self, id: &str) -> CommandResult<()> {
        self.edit(|data| {
            find_instance(data, id)?;
            data.instances.retain(|i| i.id != id);
            Ok(())
        })?;
        self.secrets.remove_instance(id);
        self.logbook.remove(id);
        // A run still in flight saves its storage (and may log) when it
        // ends, so the rest waits until no run of the instance is going.
        let storage = self.storage_file(id);
        let logbook = self.logbook.clone();
        let owner = id.to_string();
        self.sched.when_idle(id, move || {
            if let Err(e) = std::fs::remove_file(&storage) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("cannot remove {}: {e}", storage.display());
                }
            }
            logbook.remove(&owner);
        });
        Ok(())
    }

    pub fn update_settings(&self, id: &str, settings: &Map<String, Value>) -> CommandResult<()> {
        let m = self.instance_manifest(id)?;
        let secrets_set = self.secrets.keys_set(id);
        let clean = validate::settings(&m, settings, &secrets_set)
            .map_err(|fields| CommandError::fields("Some settings need fixing", fields))?;
        self.edit(|data| {
            find_instance_mut(data, id)?.settings = clean;
            Ok(())
        })
    }

    /// Stores a secret and runs the instance again so it is used.
    pub fn set_secret(&self, id: &str, key: &str, value: &str) -> CommandResult<()> {
        self.secret_field(id, key)?;
        if value.is_empty() {
            let message = "Enter a value, or remove the secret".to_string();
            let field = FieldError {
                key: key.to_string(),
                message: message.clone(),
            };
            return Err(CommandError::fields(message, vec![field]));
        }
        if value.chars().count() > SECRET_MAX_CHARS {
            let message = format!("Secrets are at most {SECRET_MAX_CHARS} characters");
            let field = FieldError {
                key: key.to_string(),
                message: message.clone(),
            };
            return Err(CommandError::fields(message, vec![field]));
        }
        self.secrets.set(id, key, value).map_err(|e| {
            log::warn!("cannot save a secret of {id}: {}", e.kind());
            CommandError::new("Cannot save the secret")
        })?;
        self.sched.refresh_now(id);
        Ok(())
    }

    /// Forgets a secret and runs the instance again without it.
    pub fn clear_secret(&self, id: &str, key: &str) -> CommandResult<()> {
        self.secret_field(id, key)?;
        self.secrets.clear(id, key).map_err(|e| {
            log::warn!("cannot remove a secret of {id}: {}", e.kind());
            CommandError::new("Cannot remove the secret")
        })?;
        self.sched.refresh_now(id);
        Ok(())
    }

    pub fn set_hidden(&self, id: &str, hidden: bool) -> CommandResult<()> {
        self.edit(|data| {
            find_instance_mut(data, id)?.hidden = hidden;
            Ok(())
        })
    }

    pub fn set_max_width(&self, id: &str, max_width: Option<u32>) -> CommandResult<()> {
        let max_width = validate::max_width(max_width).map_err(CommandError::new)?;
        self.edit(|data| {
            find_instance_mut(data, id)?.max_width = max_width;
            Ok(())
        })
    }

    /// Sets the left-to-right order of instances and groups. Ids left out
    /// keep their place at the end.
    pub fn set_order(&self, order: &[String]) -> CommandResult<()> {
        self.edit(|data| {
            for id in order {
                let known = data.instances.iter().any(|i| &i.id == id)
                    || data.groups.iter().any(|g| &g.id == id);
                if !known {
                    return Err(CommandError::new("The order names a widget that is gone"));
                }
            }
            data.order = order.to_vec();
            Ok(())
        })
    }

    /// Groups the instances where the first of them stood. Returns the
    /// group id.
    pub fn create_group(&self, members: &[String], interval: u32) -> CommandResult<String> {
        let interval = validate::group_interval(interval).map_err(CommandError::new)?;
        self.edit(|data| {
            check_members(data, members, None)?;
            if members.len() < 2 {
                return Err(CommandError::new("A group needs at least two widgets"));
            }
            let id = new_group_id(data);
            let at = data
                .order
                .iter()
                .position(|o| members.contains(o))
                .unwrap_or(data.order.len());
            data.order.retain(|o| !members.contains(o));
            let at = at.min(data.order.len());
            data.order.insert(at, id.clone());
            data.groups.push(Group {
                id: id.clone(),
                members: members.to_vec(),
                interval,
            });
            Ok(id)
        })
    }

    /// Replaces a group's members and interval. Instances dropped from it
    /// go right after the group.
    pub fn update_group(&self, id: &str, members: &[String], interval: u32) -> CommandResult<()> {
        let interval = validate::group_interval(interval).map_err(CommandError::new)?;
        self.edit(|data| {
            let g = data
                .groups
                .iter()
                .position(|g| g.id == id)
                .ok_or_else(unknown_group)?;
            check_members(data, members, Some(id))?;
            if members.is_empty() {
                return Err(CommandError::new("A group needs at least one widget"));
            }
            let dropped: Vec<String> = data.groups[g]
                .members
                .iter()
                .filter(|m| !members.contains(m))
                .cloned()
                .collect();
            data.groups[g].members = members.to_vec();
            data.groups[g].interval = interval;
            data.order
                .retain(|o| !members.contains(o) && !dropped.contains(o));
            let at = data
                .order
                .iter()
                .position(|o| o == id)
                .map_or(data.order.len(), |p| p + 1);
            data.order.splice(at..at, dropped);
            Ok(())
        })
    }

    /// Dissolves a group; its members take its place, in member order.
    pub fn ungroup(&self, id: &str) -> CommandResult<()> {
        self.edit(|data| {
            let g = data
                .groups
                .iter()
                .position(|g| g.id == id)
                .ok_or_else(unknown_group)?;
            let group = data.groups.remove(g);
            data.order.retain(|o| !group.members.contains(o));
            let at = data
                .order
                .iter()
                .position(|o| o == id)
                .unwrap_or(data.order.len());
            data.order.splice(at..at, group.members);
            data.order.retain(|o| o != id);
            Ok(())
        })
    }

    pub fn refresh_now(&self, id: &str) -> CommandResult<()> {
        find_instance(&self.sched.snapshot(), id)?;
        self.sched.refresh_now(id);
        Ok(())
    }

    pub fn get_logs(&self, id: &str) -> CommandResult<Vec<LogLineView>> {
        find_instance(&self.sched.snapshot(), id)?;
        Ok(self
            .logbook
            .lines(id)
            .into_iter()
            .map(|l| LogLineView {
                at: l.at.to_rfc3339(),
                level: level_name(l.level).to_string(),
                text: l.text,
            })
            .collect())
    }

    /// Turns Start with Windows on or off, then records the choice.
    pub fn set_autostart(&self, on: bool) -> CommandResult<()> {
        self.autostart.set(on).map_err(|e| {
            log::warn!("cannot change Start with Windows: {e}");
            CommandError::new("Cannot change Start with Windows")
        })?;
        let path = self.prefs_file();
        prefs::save(&path, &AppPrefs { autostart: on }).map_err(|e| {
            log::warn!("cannot save {}: {e}", path.display());
            CommandError::new("Start with Windows changed, but the choice could not be saved")
        })
    }
}

fn widget_view(id: &str, e: &WidgetEntry, folder: Option<&PathBuf>) -> WidgetView {
    let m = &e.manifest;
    WidgetView {
        id: id.to_string(),
        name: m.name.clone(),
        description: m.description.clone(),
        author: m.author.clone(),
        version: m.version.clone(),
        settings: m.settings.clone(),
        permissions: m.permissions.clone(),
        local: folder.is_some(),
        blocked: e.blocked.clone(),
        folder: folder.map(|f| f.display().to_string()),
    }
}

/// Opens the folder holding the log file in Explorer. The only command that
/// starts a process; it passes nothing the page sent.
pub fn open_log_folder() -> CommandResult<()> {
    let log_file = crate::paths::log_file();
    let folder = log_file
        .parent()
        .ok_or_else(|| CommandError::new("No log folder"))?;
    if let Err(e) = std::fs::create_dir_all(folder) {
        log::warn!("cannot create {}: {e}", folder.display());
    }
    // A full path, so no explorer.exe planted next to Perchbar is started.
    let windows = std::env::var_os("SystemRoot")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    std::process::Command::new(windows.join("explorer.exe"))
        .arg(folder)
        .spawn()
        .map(drop)
        .map_err(|e| {
            log::warn!("cannot open {}: {e}", folder.display());
            CommandError::new("Cannot open the log folder")
        })
}

/// The `#[tauri::command]` wrappers. They only forward to `AppState`.
pub mod ipc {
    use super::*;
    use tauri::State;

    #[tauri::command(async)]
    pub fn get_state(state: State<'_, AppState>) -> CommandResult<StateView> {
        state.get_state()
    }

    #[tauri::command(async)]
    pub fn add_instance(state: State<'_, AppState>, widget: String) -> CommandResult<String> {
        state.add_instance(&widget)
    }

    #[tauri::command(async)]
    pub fn remove_instance(state: State<'_, AppState>, id: String) -> CommandResult<()> {
        state.remove_instance(&id)
    }

    #[tauri::command(async)]
    pub fn update_settings(
        state: State<'_, AppState>,
        id: String,
        settings: Map<String, Value>,
    ) -> CommandResult<()> {
        state.update_settings(&id, &settings)
    }

    #[tauri::command(async)]
    pub fn set_secret(
        state: State<'_, AppState>,
        id: String,
        key: String,
        value: String,
    ) -> CommandResult<()> {
        state.set_secret(&id, &key, &value)
    }

    #[tauri::command(async)]
    pub fn clear_secret(state: State<'_, AppState>, id: String, key: String) -> CommandResult<()> {
        state.clear_secret(&id, &key)
    }

    #[tauri::command(async)]
    pub fn set_hidden(state: State<'_, AppState>, id: String, hidden: bool) -> CommandResult<()> {
        state.set_hidden(&id, hidden)
    }

    #[tauri::command(async)]
    pub fn set_max_width(
        state: State<'_, AppState>,
        id: String,
        max_width: Option<u32>,
    ) -> CommandResult<()> {
        state.set_max_width(&id, max_width)
    }

    #[tauri::command(async)]
    pub fn set_order(state: State<'_, AppState>, order: Vec<String>) -> CommandResult<()> {
        state.set_order(&order)
    }

    #[tauri::command(async)]
    pub fn create_group(
        state: State<'_, AppState>,
        members: Vec<String>,
        interval: u32,
    ) -> CommandResult<String> {
        state.create_group(&members, interval)
    }

    #[tauri::command(async)]
    pub fn update_group(
        state: State<'_, AppState>,
        id: String,
        members: Vec<String>,
        interval: u32,
    ) -> CommandResult<()> {
        state.update_group(&id, &members, interval)
    }

    #[tauri::command(async)]
    pub fn ungroup(state: State<'_, AppState>, id: String) -> CommandResult<()> {
        state.ungroup(&id)
    }

    #[tauri::command(async)]
    pub fn refresh_now(state: State<'_, AppState>, id: String) -> CommandResult<()> {
        state.refresh_now(&id)
    }

    #[tauri::command(async)]
    pub fn get_logs(state: State<'_, AppState>, id: String) -> CommandResult<Vec<LogLineView>> {
        state.get_logs(&id)
    }

    #[tauri::command(async)]
    pub fn set_autostart(state: State<'_, AppState>, on: bool) -> CommandResult<()> {
        state.set_autostart(on)
    }

    #[tauri::command(async)]
    pub fn open_log_folder() -> CommandResult<()> {
        super::open_log_folder()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{Color, Frame, TextSize};
    use crate::sandbox::{HttpClient, HttpFuture, HttpRequest, RunError};
    use crate::scheduler::{Runner, Scheduler};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct NoHttp;
    impl HttpClient for NoHttp {
        fn send(&self, _req: HttpRequest) -> HttpFuture<'_> {
            Box::pin(std::future::pending())
        }
    }

    #[derive(Default)]
    struct FakeAutostart {
        on: AtomicBool,
    }
    impl Autostart for FakeAutostart {
        fn enabled(&self) -> bool {
            self.on.load(Ordering::SeqCst)
        }
        fn set(&self, on: bool) -> std::io::Result<()> {
            self.on.store(on, Ordering::SeqCst);
            Ok(())
        }
    }

    fn out(text: &str) -> Output {
        Output {
            frames: vec![Frame {
                blocks: vec![Block::Text {
                    value: text.into(),
                    color: Color::Default,
                    size: TextSize::Normal,
                }],
                tooltip: None,
            }],
            rotate: None,
            click: None,
        }
    }

    fn manifest() -> Arc<Manifest> {
        Arc::new(
            serde_json::from_value(json!({
                "id": "w", "name": "W", "formatVersion": 1,
                "settings": [
                    {"key": "title", "label": "Title", "kind": "text", "default": "Hi"},
                    {"key": "apiKey", "label": "API key", "kind": "secret"}
                ]
            }))
            .unwrap(),
        )
    }

    struct Rig {
        state: AppState,
        autostart: Arc<FakeAutostart>,
        dir: tempfile::TempDir,
    }

    impl Rig {
        fn start() -> Rig {
            Rig::start_with(Instances::default(), |_| {
                Arc::new(|_| Box::pin(async { Ok::<_, RunError>(out("x")) }))
            })
        }

        /// `runner` gets the data folder.
        fn start_with(data: Instances, runner: impl FnOnce(PathBuf) -> Runner) -> Rig {
            let dir = tempfile::tempdir().unwrap();
            let widgets = HashMap::from([(
                "w".to_string(),
                WidgetEntry::bundled(manifest(), dir.path().join("widgets").join("w")),
            )]);
            let secrets = SecretStore::new(dir.path().join("secrets"));
            let sched = Scheduler::start_with_runner(
                widgets,
                data,
                Arc::new(NoHttp),
                secrets.clone(),
                runner(dir.path().to_path_buf()),
                dir.path().join("instances.json"),
                |_| {},
            );
            let logbook = sched.logbook();
            let autostart = Arc::new(FakeAutostart::default());
            let state = AppState::new(
                sched,
                secrets,
                logbook,
                autostart.clone(),
                dir.path().to_path_buf(),
            );
            Rig {
                state,
                autostart,
                dir,
            }
        }
    }

    #[tokio::test]
    async fn state_reports_secret_as_set_only() {
        let rig = Rig::start();
        let id = rig.state.add_instance("w").unwrap();
        rig.state
            .set_secret(&id, "apiKey", "sk-TOP-secret-value")
            .unwrap();
        let view = rig.state.get_state().unwrap();
        let inst = view.instances.iter().find(|i| i.id == id).unwrap();
        assert_eq!(inst.secrets_set, ["apiKey"]);
        assert!(!inst.secrets_unreadable);
        assert!(!inst.settings.contains_key("apiKey"));
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"secretsSet\":[\"apiKey\"]"));
        assert!(!json.contains("sk-TOP-secret-value"));

        rig.state.clear_secret(&id, "apiKey").unwrap();
        let view = rig.state.get_state().unwrap();
        assert!(view.instances[0].secrets_set.is_empty());
    }

    #[tokio::test]
    async fn secret_errors_never_hold_the_value() {
        let rig = Rig::start();
        let id = rig.state.add_instance("w").unwrap();
        let long = "Q".repeat(SECRET_MAX_CHARS + 1);
        let e = rig.state.set_secret(&id, "apiKey", &long).unwrap_err();
        assert!(!e.message.contains("QQQQ"));
        assert!(e.message.contains(&SECRET_MAX_CHARS.to_string()));
        // Only secret fields take secrets.
        let e = rig.state.set_secret(&id, "title", "hunter2").unwrap_err();
        assert!(!format!("{e:?}").contains("hunter2"));
    }

    #[tokio::test]
    async fn remove_instance_deletes_its_files() {
        let rig = Rig::start();
        let id = rig.state.add_instance("w").unwrap();
        let storage = rig.dir.path().join("storage").join(format!("{id}.json"));
        std::fs::create_dir_all(storage.parent().unwrap()).unwrap();
        std::fs::write(&storage, "{}").unwrap();
        rig.state.set_secret(&id, "apiKey", "k").unwrap();
        let secret_file = rig.dir.path().join("secrets").join(format!("{id}.bin"));
        assert!(secret_file.exists());
        rig.state.logbook.push(&id, LogLevel::Info, "hello".into());

        rig.state.remove_instance(&id).unwrap();
        assert!(!storage.exists());
        assert!(!secret_file.exists());
        assert!(rig.state.logbook.lines(&id).is_empty());
        assert!(rig.state.get_state().unwrap().instances.is_empty());
        assert!(rig.state.remove_instance(&id).is_err());
    }

    #[tokio::test]
    async fn create_group_then_ungroup_restores_order() {
        let rig = Rig::start();
        let a = rig.state.add_instance("w").unwrap();
        let b = rig.state.add_instance("w").unwrap();
        let c = rig.state.add_instance("w").unwrap();
        let before = rig.state.get_state().unwrap().order;
        assert_eq!(before, [a.clone(), b.clone(), c.clone()]);

        let g = rig.state.create_group(&[a.clone(), b.clone()], 8).unwrap();
        assert!(g.starts_with("grp-") && g.len() == 12);
        let view = rig.state.get_state().unwrap();
        assert_eq!(view.order, [g.clone(), c.clone()]);
        assert_eq!(view.groups[0].members, [a.clone(), b.clone()]);
        assert_eq!(view.groups[0].interval, 8);

        // Bad intervals and members already grouped are refused.
        assert!(rig.state.create_group(&[c.clone(), a.clone()], 8).is_err());
        assert!(rig
            .state
            .update_group(&g, std::slice::from_ref(&a), 1)
            .is_err());
        rig.state
            .update_group(&g, &[a.clone(), b.clone()], 20)
            .unwrap();
        assert_eq!(rig.state.get_state().unwrap().groups[0].interval, 20);

        rig.state.ungroup(&g).unwrap();
        let view = rig.state.get_state().unwrap();
        assert_eq!(view.order, before);
        assert!(view.groups.is_empty());
    }

    #[tokio::test]
    async fn update_group_puts_dropped_members_after_it() {
        let rig = Rig::start();
        let a = rig.state.add_instance("w").unwrap();
        let b = rig.state.add_instance("w").unwrap();
        let c = rig.state.add_instance("w").unwrap();
        let g = rig.state.create_group(&[a.clone(), b.clone()], 8).unwrap();
        rig.state
            .update_group(&g, std::slice::from_ref(&a), 8)
            .unwrap();
        assert_eq!(rig.state.get_state().unwrap().order, [g, b, c]);
    }

    #[tokio::test]
    async fn unknown_instance_id_is_an_error() {
        let rig = Rig::start();
        let ghost = "inst-0badf00d";
        assert!(rig.state.remove_instance(ghost).is_err());
        assert!(rig.state.update_settings(ghost, &Map::new()).is_err());
        assert!(rig.state.set_secret(ghost, "apiKey", "v").is_err());
        assert!(rig.state.clear_secret(ghost, "apiKey").is_err());
        assert!(rig.state.set_hidden(ghost, true).is_err());
        assert!(rig.state.set_max_width(ghost, None).is_err());
        assert!(rig.state.refresh_now(ghost).is_err());
        assert!(rig.state.get_logs(ghost).is_err());
        assert!(rig.state.set_order(&[ghost.to_string()]).is_err());
        assert!(rig.state.create_group(&[ghost.to_string()], 8).is_err());
        assert!(rig.state.ungroup("grp-0badf00d").is_err());
        assert!(rig.state.add_instance("nope").is_err());
        // A path in place of an id never reaches the disk.
        assert!(rig.state.remove_instance("..\\..\\x").is_err());
    }

    #[tokio::test]
    async fn update_settings_validates_and_saves() {
        let rig = Rig::start();
        let id = rig.state.add_instance("w").unwrap();
        let e = rig
            .state
            .update_settings(&id, json!({"title": 5}).as_object().unwrap())
            .unwrap_err();
        assert_eq!(e.fields.unwrap()[0].key, "title");
        rig.state
            .update_settings(&id, json!({"title": "Yo"}).as_object().unwrap())
            .unwrap();
        let view = rig.state.get_state().unwrap();
        assert_eq!(view.instances[0].settings["title"], json!("Yo"));
    }

    #[test]
    fn command_error_wire_shape() {
        let plain = serde_json::to_value(CommandError::new("m")).unwrap();
        assert_eq!(plain, json!({"message": "m"}));
        let with_fields = CommandError::fields(
            "Some settings need fixing",
            vec![FieldError {
                key: "title".into(),
                message: "Must be text".into(),
            }],
        );
        assert_eq!(
            serde_json::to_value(with_fields).unwrap(),
            json!({
                "message": "Some settings need fixing",
                "fields": [{"key": "title", "message": "Must be text"}]
            })
        );
    }

    #[tokio::test]
    async fn unknown_widget_instance_sends_no_settings() {
        let orphan = Instance {
            id: "inst-0000000a".into(),
            widget: "uninstalled".into(),
            settings: json!({"apiKey": "sk-plain-leftover", "city": "Oslo"})
                .as_object()
                .unwrap()
                .clone(),
            hidden: false,
            max_width: None,
        };
        let data = Instances {
            order: vec![orphan.id.clone()],
            instances: vec![orphan],
            groups: vec![],
        };
        let rig = Rig::start_with(data, |_| {
            Arc::new(|_| Box::pin(async { Ok::<_, RunError>(out("x")) }))
        });
        let view = rig.state.get_state().unwrap();
        assert_eq!(view.instances.len(), 1);
        assert!(view.instances[0].settings.is_empty());
        assert_eq!(view.instances[0].status.state, "error");
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("sk-plain-leftover"));
    }

    #[tokio::test]
    async fn set_secret_rejects_empty_value() {
        let rig = Rig::start();
        let id = rig.state.add_instance("w").unwrap();
        let e = rig.state.set_secret(&id, "apiKey", "").unwrap_err();
        assert_eq!(e.fields.unwrap()[0].key, "apiKey");
        assert!(rig.state.get_state().unwrap().instances[0]
            .secrets_set
            .is_empty());
    }

    /// The sandbox saves storage when a run ends, even after a stop. A run
    /// in flight when its instance is removed must not leave the file.
    #[tokio::test(start_paused = true)]
    async fn run_in_flight_cannot_bring_back_removed_storage() {
        let id = "inst-0000000b".to_string();
        let inst = Instance {
            id: id.clone(),
            widget: "w".into(),
            settings: Map::new(),
            hidden: false,
            max_width: None,
        };
        let data = Instances {
            order: vec![id.clone()],
            instances: vec![inst],
            groups: vec![],
        };
        let rig = Rig::start_with(data, |dir| {
            Arc::new(move |ctx: crate::sandbox::RunContext| {
                let file = dir
                    .join("storage")
                    .join(format!("{}.json", ctx.instance_id));
                Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                    std::fs::write(&file, "{}").unwrap();
                    Ok(out("x"))
                })
            })
        });
        let storage = rig.dir.path().join("storage").join(format!("{id}.json"));
        tokio::time::sleep(std::time::Duration::from_secs(3)).await; // run in flight
        rig.state.remove_instance(&id).unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        assert!(!storage.exists(), "the run wrote storage after removal");
    }

    #[tokio::test]
    async fn hidden_width_and_order() {
        let rig = Rig::start();
        let a = rig.state.add_instance("w").unwrap();
        let b = rig.state.add_instance("w").unwrap();
        rig.state.set_hidden(&a, true).unwrap();
        assert!(rig.state.set_max_width(&a, Some(39)).is_err());
        rig.state.set_max_width(&a, Some(120)).unwrap();
        rig.state.set_order(&[b.clone(), a.clone()]).unwrap();
        let view = rig.state.get_state().unwrap();
        assert_eq!(view.order, [b, a.clone()]);
        let inst = view.instances.iter().find(|i| i.id == a).unwrap();
        assert!(inst.hidden);
        assert_eq!(inst.max_width, Some(120));
        assert_eq!(inst.status.state, "hidden");
        assert_eq!(view.widgets[0].id, "w");
        assert!(!view.widgets[0].local);
    }

    #[tokio::test]
    async fn blocked_widget_status() {
        let rig = Rig::start();
        let id = rig.state.add_instance("w").unwrap();
        let mut entry = WidgetEntry::bundled(manifest(), rig.dir.path().join("w"));
        entry.blocked = Some("Waiting for permission review".into());
        rig.state.sched.upsert_widget("w", entry);
        let view = rig.state.get_state().unwrap();
        let inst = view.instances.iter().find(|i| i.id == id).unwrap();
        assert_eq!(inst.status.state, "blocked");
        assert_eq!(
            inst.status.message.as_deref(),
            Some("Waiting for permission review")
        );
        assert_eq!(
            view.widgets[0].blocked.as_deref(),
            Some("Waiting for permission review")
        );
    }

    #[tokio::test]
    async fn set_autostart_follows_windows_and_writes_prefs() {
        let rig = Rig::start();
        rig.state.set_autostart(true).unwrap();
        assert!(rig.state.get_state().unwrap().autostart);
        let prefs = prefs::load(&rig.dir.path().join("app.json")).unwrap();
        assert!(prefs.autostart);
        // Windows turned it off: report what Windows says.
        rig.autostart.on.store(false, Ordering::SeqCst);
        assert!(!rig.state.get_state().unwrap().autostart);
    }

    #[tokio::test]
    async fn get_logs_returns_lines() {
        let rig = Rig::start();
        let id = rig.state.add_instance("w").unwrap();
        rig.state
            .logbook
            .push(&id, LogLevel::Warn, "careful".into());
        let lines = rig.state.get_logs(&id).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].level, "warn");
        assert_eq!(lines[0].text, "careful");
    }

    #[test]
    fn status_view_from_slot_state() {
        let v = StatusView::from_state(&SlotState::Ok(out("BTC $1")));
        assert_eq!(v.state, "ok");
        assert_eq!(v.text.as_deref(), Some("BTC $1"));
        assert_eq!(v.message, None);

        let v = StatusView::from_state(&SlotState::Stale(out("old")));
        assert_eq!(v.state, "stale");
        assert_eq!(v.text.as_deref(), Some("old"));

        let v = StatusView::from_state(&SlotState::Error {
            message: "boom".into(),
            last: None,
        });
        assert_eq!(v.state, "error");
        assert_eq!(v.message.as_deref(), Some("boom"));
        assert_eq!(v.text, None);

        let v = StatusView::from_state(&SlotState::Loading);
        assert_eq!(v.state, "loading");

        // A text inside a stack is found too.
        let stacked = Output {
            frames: vec![Frame {
                blocks: vec![Block::Stack {
                    top: Box::new(Block::Ring {
                        value: 0.5,
                        color: Color::Default,
                    }),
                    bottom: Box::new(Block::Text {
                        value: "50%".into(),
                        color: Color::Default,
                        size: TextSize::Small,
                    }),
                }],
                tooltip: None,
            }],
            rotate: None,
            click: None,
        };
        let v = StatusView::from_state(&SlotState::Ok(stacked));
        assert_eq!(v.text.as_deref(), Some("50%"));
    }
}
