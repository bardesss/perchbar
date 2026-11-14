//! The settings commands end to end: a first start over an empty data
//! folder, a round of edits through the commands, then a restart over the
//! same folder. Widget runs go to a fake runner; secrets use the real
//! DPAPI store.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};

use perchbar::commands::{AppState, StateView};
use perchbar::display::{Block, Color, Frame, Output, TextSize};
use perchbar::manifest::Permissions;
use perchbar::prefs::{self, Autostart};
use perchbar::sandbox::{HttpClient, HttpFuture, HttpRequest, RunError};
use perchbar::scheduler::{Runner, Scheduler, SchedulerHandle};
use perchbar::setup;

const SECRET: &str = "test-key-6f1d93a0c4";

/// Start with Windows as a flag. One instance stands in for the registry,
/// so it keeps its value across the restart.
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

/// Widgets never reach the network here.
struct NoHttp;

impl HttpClient for NoHttp {
    fn send(&self, _req: HttpRequest) -> HttpFuture<'_> {
        Box::pin(std::future::pending())
    }
}

/// Settings each run was given, by instance id.
type Seen = Arc<Mutex<Vec<(String, Map<String, Value>)>>>;

fn fake_runner(seen: Seen) -> Runner {
    Arc::new(move |ctx| {
        seen.lock()
            .unwrap()
            .push((ctx.instance_id.clone(), ctx.settings.clone()));
        Box::pin(async {
            Ok::<_, RunError>(Output {
                frames: vec![Frame {
                    blocks: vec![Block::Text {
                        value: "ok".into(),
                        color: Color::Default,
                        size: TextSize::Normal,
                    }],
                    tooltip: None,
                }],
                rotate: None,
                click: None,
            })
        })
    })
}

/// One run of Perchbar over `data_dir`: the same startup as the app, with
/// a fake runner and a fake Start with Windows.
struct App {
    state: AppState,
    sched: SchedulerHandle,
    seen: Seen,
}

impl App {
    fn start(data_dir: &Path, autostart: &Arc<FakeAutostart>) -> App {
        prefs::startup(
            &data_dir.join("app.json"),
            autostart.as_ref(),
            Path::new(r"C:\Perchbar\perchbar.exe"),
        );
        let boot = setup::boot(data_dir);
        let seen: Seen = Arc::default();
        let sched = Scheduler::start_with_runner(
            boot.widgets,
            boot.data,
            Arc::new(NoHttp),
            boot.secrets.clone(),
            fake_runner(seen.clone()),
            data_dir.join("instances.json"),
            |_| {},
        );
        let state = AppState::new(
            sched.clone(),
            boot.secrets,
            sched.logbook(),
            autostart.clone(),
            data_dir.to_path_buf(),
        );
        App { state, sched, seen }
    }

    fn stop(self) {
        drop(self.state);
        self.sched.shutdown();
    }

    /// Waits for a run of `id` and returns the settings it was given.
    async fn run_settings(&self, id: &str) -> Map<String, Value> {
        for _ in 0..200 {
            let found = self
                .seen
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(i, _)| i == id)
                .map(|(_, s)| s.clone());
            if let Some(s) = found {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{id} never ran");
    }
}

/// A widget with a secret setting and no permissions, outside the data
/// folder, as a user would load it with "Load from folder".
fn secret_widget(dir: &Path) -> PathBuf {
    let folder = dir.join("secret-test");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("widget.json"),
        json!({
            "id": "secret-test",
            "name": "Secret test",
            "formatVersion": 1,
            "settings": [
                {"key": "label", "label": "Label", "kind": "text", "default": "Key"},
                {"key": "apiKey", "label": "API key", "kind": "secret"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        folder.join("widget.js"),
        "export default async function update() { return { blocks: [] }; }",
    )
    .unwrap();
    folder
}

fn ids_of(view: &StateView, widget: &str) -> Vec<String> {
    view.instances
        .iter()
        .filter(|i| i.widget == widget)
        .map(|i| i.id.clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_flow_end_to_end() {
    let data = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let autostart = Arc::new(FakeAutostart::default());

    // First start: the two default widgets, Start with Windows on.
    let app = App::start(data.path(), &autostart);
    let view = app.state.get_state().unwrap();
    let mut widgets: Vec<&str> = view.instances.iter().map(|i| i.widget.as_str()).collect();
    widgets.sort();
    assert_eq!(widgets, ["bitcoin-price", "world-clock"]);
    assert!(view.autostart);
    assert!(autostart.enabled());
    assert!(
        view.widgets.iter().all(|w| w.blocked.is_none()),
        "templates run without a review"
    );

    // A second world clock with its own cities.
    let bitcoin = ids_of(&view, "bitcoin-price")[0].clone();
    let first_clock = ids_of(&view, "world-clock")[0].clone();
    let clock = app.state.add_instance("world-clock").unwrap();
    let cities = json!(["Lima|America/Lima", "Oslo|Europe/Oslo"]);
    app.state
        .update_settings(
            &clock,
            json!({"cities": cities, "rotate": 4}).as_object().unwrap(),
        )
        .unwrap();

    // A widget loaded from a folder, with a secret.
    let folder = secret_widget(elsewhere.path());
    let report = app
        .state
        .inspect_folder(&folder.display().to_string())
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let widget = app
        .state
        .install_local(
            &folder.display().to_string(),
            &Permissions::default(),
            false,
        )
        .unwrap();
    let tester = app.state.add_instance(&widget).unwrap();
    app.state.set_secret(&tester, "apiKey", SECRET).unwrap();
    assert_eq!(app.run_settings(&tester).await["apiKey"], json!(SECRET));

    // Both clocks take turns in one spot; the price is hidden.
    let group = app
        .state
        .create_group(&[first_clock.clone(), clock.clone()], 5)
        .unwrap();
    app.state.set_hidden(&bitcoin, true).unwrap();
    app.state.set_max_width(&clock, Some(120)).unwrap();

    let before = app.state.get_state().unwrap();
    app.stop();

    // instances.json on disk never holds a secret value.
    let raw = std::fs::read_to_string(data.path().join("instances.json")).unwrap();
    assert!(!raw.contains(SECRET), "instances.json holds the secret");
    assert!(data
        .path()
        .join("secrets")
        .join(format!("{tester}.bin"))
        .is_file());

    // Restart over the same folder: everything comes back.
    let app = App::start(data.path(), &autostart);
    let after = app.state.get_state().unwrap();
    assert!(after.autostart);
    assert_eq!(after.order, before.order);
    assert_eq!(after.groups, before.groups);
    assert_eq!(after.groups[0].id, group);
    assert_eq!(after.groups[0].members, [first_clock, clock.clone()]);
    assert_eq!(after.groups[0].interval, 5);
    // Everything but the run status, which starts over.
    let summary = |v: &StateView| -> Vec<Value> {
        v.instances
            .iter()
            .map(|i| {
                json!({
                    "id": i.id,
                    "widget": i.widget,
                    "settings": i.settings,
                    "hidden": i.hidden,
                    "maxWidth": i.max_width,
                    "secretsSet": i.secrets_set,
                })
            })
            .collect()
    };
    assert_eq!(summary(&after), summary(&before));
    let clock_view = after.instances.iter().find(|i| i.id == clock).unwrap();
    assert_eq!(clock_view.settings["cities"], cities);
    assert_eq!(clock_view.max_width, Some(120));
    assert!(after.instances.iter().any(|i| i.id == bitcoin && i.hidden));

    // The local widget is still approved and its secret loads.
    let local = after.widgets.iter().find(|w| w.id == widget).unwrap();
    assert!(local.local);
    assert_eq!(local.blocked, None);
    let tester_view = after.instances.iter().find(|i| i.id == tester).unwrap();
    assert_eq!(tester_view.secrets_set, ["apiKey"]);
    assert!(!tester_view.secrets_unreadable);
    assert!(!tester_view.settings.contains_key("apiKey"));
    app.state.refresh_now(&tester).unwrap();
    assert_eq!(app.run_settings(&tester).await["apiKey"], json!(SECRET));

    let raw = std::fs::read_to_string(data.path().join("instances.json")).unwrap();
    assert!(!raw.contains(SECRET), "instances.json holds the secret");
    app.stop();
}
