// Release builds are GUI programs: no console window when started from
// Explorer or at sign-in. Debug builds keep the console for log output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use perchbar::sandbox::ReqwestClient;
use perchbar::scheduler::{Scheduler, WidgetEntry};
use perchbar::secrets::{self, SecretStore};
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

    let widgets_dir = paths::widgets_dir();
    match setup::install_bundled(&widgets_dir) {
        Ok(copied) if !copied.is_empty() => log::info!("installed templates: {copied:?}"),
        Ok(_) => {}
        Err(e) => log::warn!("cannot install bundled templates: {e}"),
    }

    let widgets = setup::load_widgets(&widgets_dir);
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
    let secret_store = SecretStore::new(paths::data_dir().join("secrets"));
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

    // Every installed widget runs with what its manifest declares, as in
    // Plan 1; permission review comes with the settings window.
    let widgets: HashMap<String, WidgetEntry> = widgets
        .into_iter()
        .map(|(id, (m, dir))| (id, WidgetEntry::bundled(m, dir)))
        .collect();

    let (tx, rx) = mpsc::channel();
    let publish_tx = tx.clone();
    // The scheduler spawns onto the runtime it starts in, and its runner needs
    // a multi-thread runtime; Tauri's global runtime is one.
    let sched = tauri::async_runtime::block_on(async move {
        Scheduler::start(
            widgets,
            data,
            Arc::new(ReqwestClient::new()),
            secret_store,
            move |model| {
                // Runs on scheduler threads under the publish lock: never block.
                let _ = publish_tx.send(StripEvent::Model(model));
                strip::wake();
            },
        )
    });
    let app = tauri::Builder::default()
        .build(tauri::generate_context!())
        .expect("failed to build Perchbar");
    let handle = app.handle().clone();
    // "Quit Perchbar" in the strip's menu: an explicit exit code, which the
    // ExitRequested handler below lets through.
    let mut strip_thread = Some(strip::spawn(rx, sched, names, move || handle.exit(0)));

    app.run(move |_app, event| match event {
        // Perchbar has no windows; only an explicit exit (code Some) quits.
        tauri::RunEvent::ExitRequested { api, code, .. } if code.is_none() => {
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
