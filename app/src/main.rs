// Wired up by later tasks.
#[allow(dead_code)]
mod display;
#[allow(dead_code)]
mod instances;
#[allow(dead_code)]
mod manifest;
#[allow(dead_code)]
mod paths;
#[allow(dead_code)]
mod sandbox;
#[allow(dead_code)]
mod scheduler;
#[allow(dead_code)]
mod strip;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use manifest::Manifest;
use sandbox::ReqwestClient;
use scheduler::Scheduler;
use strip::StripEvent;

/// Installed widgets by manifest id. Folders that fail to load are skipped.
fn load_widgets(dir: &Path) -> HashMap<String, (Arc<Manifest>, PathBuf)> {
    let mut widgets = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        log::info!("no widgets folder at {}", dir.display());
        return widgets;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("perchbar=info"))
        .init();

    let widgets = load_widgets(&paths::widgets_dir());
    let known: HashSet<String> = widgets.keys().cloned().collect();
    let mut data = instances::load(&paths::instances_file());
    data.normalize(&known);
    log::info!(
        "{} widget(s), {} instance(s)",
        widgets.len(),
        data.instances.len()
    );

    let (tx, rx) = mpsc::channel();
    let publish_tx = tx.clone();
    // The scheduler spawns onto the runtime it starts in, and its runner needs
    // a multi-thread runtime; Tauri's global runtime is one.
    let sched = tauri::async_runtime::block_on(async move {
        Scheduler::start(
            widgets,
            data,
            Arc::new(ReqwestClient::new()),
            move |model| {
                // Runs on scheduler threads under the publish lock: never block.
                let _ = publish_tx.send(StripEvent::Model(model));
                strip::wake();
            },
        )
    });
    let mut strip_thread = Some(strip::spawn(rx, sched));

    tauri::Builder::default()
        .build(tauri::generate_context!())
        .expect("failed to build Perchbar")
        .run(move |_app, event| match event {
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
