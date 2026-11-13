// Release builds are GUI programs: no console window when started from
// Explorer or at sign-in. Debug builds keep the console for log output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use perchbar::approvals::{self, Approvals};
use perchbar::commands::AppState;
use perchbar::prefs::{self, Autostart, NoAutostart, RegistryAutostart};
use perchbar::sandbox::ReqwestClient;
use perchbar::scheduler::{Scheduler, WidgetEntry};
use perchbar::secrets::{self, SecretStore};
use perchbar::settings_window::{self, ExitDecision, StateBridge};
use perchbar::strip::{self, StripEvent};
use perchbar::{instances, logger, paths, setup};

/// Takes the session-wide `Local\Perchbar` mutex. Returns false when another
/// Perchbar already holds it. The handle is never closed: the mutex lives
/// until the process exits.
fn first_instance() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    // SAFETY: plain Win32 calls with a static, NUL-terminated name.
    unsafe {
        match CreateMutexW(None, false, w!("Local\\Perchbar")) {
            Ok(_) => GetLastError() != ERROR_ALREADY_EXISTS,
            // Without the mutex we cannot tell; running is the safer guess.
            Err(_) => true,
        }
    }
}

/// How long exit waits for the strip thread to tear down.
const STRIP_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Joins `t` if it finishes within `timeout`; otherwise leaves it running
/// (the process is exiting anyway) so a hung Explorer cannot hang exit.
fn join_for(t: JoinHandle<()>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !t.is_finished() {
        if Instant::now() >= deadline {
            log::warn!("strip thread did not stop within {timeout:?}");
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = t.join();
}

fn main() {
    if !first_instance() {
        return;
    }
    if let Err(e) = logger::init(&paths::log_file()) {
        eprintln!("cannot open {}: {e}", paths::log_file().display());
    }
    log::info!(
        "Perchbar {} starting, data in {}",
        env!("CARGO_PKG_VERSION"),
        paths::data_dir().display()
    );

    // Dev and test runs (PERCHBAR_DATA_DIR set) must not register themselves
    // in the real Run key.
    let dev_mode = std::env::var_os(paths::DATA_DIR_ENV).is_some_and(|v| !v.is_empty());
    let exe = std::env::current_exe();
    let autostart: Arc<dyn Autostart + Send + Sync> = match &exe {
        Ok(exe) if !dev_mode => Arc::new(RegistryAutostart { exe: exe.clone() }),
        _ => Arc::new(NoAutostart),
    };
    match &exe {
        Ok(exe) => {
            if dev_mode {
                log::info!("autostart skipped in dev mode");
            }
            prefs::startup(&paths::prefs_file(), autostart.as_ref(), exe);
        }
        Err(e) => log::warn!("cannot find own exe, Start with Windows unchanged: {e}"),
    }

    let widgets_dir = paths::widgets_dir();
    let copied = match setup::install_bundled(&widgets_dir) {
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
    let mut approvals = Approvals::load(&paths::approvals_file());
    if setup::approve_bundled(&mut approvals, &copied) {
        if let Err(e) = approvals.save(&paths::approvals_file()) {
            log::warn!("cannot save {}: {e}", paths::approvals_file().display());
        }
    }

    // Installed widgets first; a local one with a taken id is skipped and
    // dropped from local.json, so it is never mistaken for the installed one.
    let mut widgets = setup::load_widgets(&widgets_dir);
    match approvals::load_local(&paths::local_file()) {
        Ok(mut local) => {
            let shadowed = setup::add_local(&mut widgets, &local);
            if !shadowed.is_empty() {
                if setup::drop_shadowed(&mut local, &mut approvals, &shadowed) {
                    if let Err(e) = approvals.save(&paths::approvals_file()) {
                        log::warn!("cannot save {}: {e}", paths::approvals_file().display());
                    }
                }
                if let Err(e) = approvals::save_local(&paths::local_file(), &local) {
                    log::warn!("cannot save {}: {e}", paths::local_file().display());
                }
            }
        }
        Err(e) => log::warn!(
            "cannot read {}: {e}; local widgets are left out",
            paths::local_file().display()
        ),
    }
    let known: HashSet<String> = widgets.keys().cloned().collect();
    let names: HashMap<PathBuf, String> = widgets
        .values()
        .map(|(m, dir)| (dir.clone(), m.name.clone()))
        .collect();
    let mut data = instances::load(&paths::instances_file());
    // No file after loading: a first run, or a corrupt file that `load` moved
    // aside. A saved empty strip (the user removed everything) stays empty.
    let first_run = !paths::instances_file().exists();
    data.normalize(&known);
    if first_run && setup::seed_first_run(&mut data, &widgets) {
        log::info!("first run: added the default widgets");
        if let Err(e) = instances::save(&paths::instances_file(), &data) {
            log::warn!("cannot save {}: {e}", paths::instances_file().display());
        }
    }
    log::info!(
        "{} widget(s), {} instance(s)",
        widgets.len(),
        data.instances.len()
    );

    // Secret settings never stay in instances.json: move any found there
    // (hand edits, older builds) into the encrypted store.
    let secret_store = SecretStore::new(paths::secrets_dir());
    let manifests: HashMap<String, Arc<_>> = widgets
        .iter()
        .map(|(id, (m, _))| (id.clone(), m.clone()))
        .collect();
    if secrets::migrate(&mut data, &manifests, &secret_store) {
        log::info!("moved secret settings out of instances.json");
        if let Err(e) = instances::save(&paths::instances_file(), &data) {
            log::warn!("cannot save {}: {e}", paths::instances_file().display());
        }
    }

    // Each widget runs with the permissions the user approved; one that
    // asks for more waits for review in the settings window.
    let widgets: HashMap<String, WidgetEntry> = setup::entries(widgets, &approvals);

    let (tx, rx) = mpsc::channel();
    let publish_tx = tx.clone();
    let (bridge, bridge_rx) = StateBridge::new();
    // The same store, shared with the settings commands.
    let sched_secrets = secret_store.clone();
    // The scheduler spawns onto the runtime it starts in, and its runner needs
    // a multi-thread runtime; Tauri's global runtime is one.
    let sched = tauri::async_runtime::block_on(async move {
        Scheduler::start(
            widgets,
            data,
            Arc::new(ReqwestClient::new()),
            sched_secrets,
            move |model| {
                // Runs on scheduler threads under the publish lock: never block.
                let _ = publish_tx.send(StripEvent::Model(model));
                strip::wake();
                bridge.notify();
            },
        )
    });
    let state = AppState::new(
        sched.clone(),
        secret_store,
        sched.logbook(),
        autostart,
        paths::data_dir(),
    );
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(settings_window::invoke_handler())
        .build(tauri::generate_context!())
        .expect("failed to build Perchbar");
    settings_window::spawn_state_bridge(app.handle(), bridge_rx);
    let quit_handle = app.handle().clone();
    let open_handle = app.handle().clone();
    // "Quit Perchbar" in the strip's menu: an explicit exit code, which the
    // ExitRequested handler below lets through.
    let mut strip_thread = Some(strip::spawn(
        rx,
        sched,
        names,
        move || quit_handle.exit(0),
        // Queued for the main thread; the strip never waits.
        move |focus| {
            let app = open_handle.clone();
            let queued = open_handle.run_on_main_thread(move || settings_window::open(&app, focus));
            if let Err(e) = queued {
                log::warn!("cannot open the settings window: {e}");
            }
        },
    ));

    app.run(move |_app, event| match event {
        // Closing the settings window must not end Perchbar; only an
        // explicit exit (code Some) quits.
        tauri::RunEvent::ExitRequested { api, code, .. }
            if settings_window::exit_decision(code) == ExitDecision::Prevent =>
        {
            api.prevent_exit();
        }
        tauri::RunEvent::Exit => {
            let _ = tx.send(StripEvent::Quit);
            strip::wake();
            if let Some(t) = strip_thread.take() {
                join_for(t, STRIP_JOIN_TIMEOUT);
            }
        }
        _ => {}
    });
}
