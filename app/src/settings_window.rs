//! The settings window: one webview window labelled `settings`, created on
//! demand and destroyed when closed. Perchbar keeps running without it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tauri::window::{Effect, EffectsBuilder};
use tauri::{
    AppHandle, Emitter, Listener, Manager, PhysicalPosition, Runtime, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};
use tokio::sync::watch;

/// The window's label; capabilities/settings.json grants permissions to it.
pub const LABEL: &str = "settings";
/// Sent to the page with an instance id to show that widget's settings.
pub const EVENT_FOCUS: &str = "perchbar://focus";
/// Sent to the page (no payload) when widget state changed.
pub const EVENT_STATE: &str = "perchbar://state-changed";
/// Sent by the page once it listens for [`EVENT_FOCUS`].
pub const EVENT_READY: &str = "perchbar://ready";
/// At most one [`EVENT_STATE`] per this interval.
pub const STATE_THROTTLE: Duration = Duration::from_millis(300);

const TITLE: &str = "Perchbar";
/// Initial and minimum inner size, logical pixels.
const SIZE: (f64, f64) = (960.0, 680.0);
const MIN_SIZE: (f64, f64) = (720.0, 520.0);

/// Whether the current page said it is ready, and the instance to focus
/// once it does.
struct Readiness {
    ready: bool,
    pending: Option<String>,
}

static READINESS: Mutex<Readiness> = Mutex::new(Readiness {
    ready: false,
    pending: None,
});
/// The ready listener is registered once, on first open.
static LISTENING: AtomicBool = AtomicBool::new(false);

fn readiness() -> std::sync::MutexGuard<'static, Readiness> {
    READINESS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Opens the settings window, or brings the open one forward. With `focus`,
/// the page is told to show that instance once it is ready. Call it on the
/// main thread.
pub fn open<R: Runtime>(app: &AppHandle<R>, focus: Option<String>) {
    if !LISTENING.swap(true, Ordering::AcqRel) {
        let handle = app.clone();
        app.listen_any(EVENT_READY, move |_| {
            let pending = {
                let mut r = readiness();
                r.ready = true;
                r.pending.take()
            };
            if let Some(id) = pending {
                emit_focus(&handle, id);
            }
        });
    }

    if let Some(w) = app.get_webview_window(LABEL) {
        if w.is_minimized().unwrap_or(false) {
            let _ = w.unminimize();
        }
        let _ = w.show();
        let _ = w.set_focus();
        if let Some(id) = focus {
            let mut r = readiness();
            if r.ready {
                drop(r);
                emit_focus(app, id);
            } else {
                r.pending = Some(id);
            }
        }
        return;
    }

    {
        let mut r = readiness();
        r.ready = false;
        r.pending = focus;
    }
    match create(app) {
        Ok(w) => {
            center_on_primary(&w);
            let _ = w.show();
            let _ = w.set_focus();
            log::info!("settings window opened");
        }
        Err(e) => log::error!("cannot open the settings window: {e}"),
    }
}

fn create<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<WebviewWindow<R>> {
    WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
        .title(TITLE)
        .inner_size(SIZE.0, SIZE.1)
        .min_inner_size(MIN_SIZE.0, MIN_SIZE.1)
        .decorations(true)
        .resizable(true)
        // Mica shows through a transparent window and page.
        .transparent(true)
        .effects(EffectsBuilder::new().effect(Effect::Mica).build())
        // Shown once centered, so it does not jump.
        .visible(false)
        .build()
}

/// Centers `w` on the primary monitor's work area, or on whatever monitor
/// it opened on when there is none.
fn center_on_primary<R: Runtime>(w: &WebviewWindow<R>) {
    let (Ok(Some(mon)), Ok(size)) = (w.primary_monitor(), w.outer_size()) else {
        let _ = w.center();
        return;
    };
    let area = mon.work_area();
    let x = area.position.x + (area.size.width as i32 - size.width as i32) / 2;
    let y = area.position.y + (area.size.height as i32 - size.height as i32) / 2;
    let _ = w.set_position(PhysicalPosition::new(
        x.max(area.position.x),
        y.max(area.position.y),
    ));
}

fn emit_focus<R: Runtime>(app: &AppHandle<R>, id: String) {
    if let Err(e) = app.emit_to(LABEL, EVENT_FOCUS, id) {
        log::warn!("cannot focus the settings page: {e}");
    }
}

/// The version shown in the settings window.
#[tauri::command]
pub fn get_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The commands the settings page may call. Each needs an entry in
/// `build.rs` and an `allow-` permission in `capabilities/settings.json`.
pub fn invoke_handler<R: Runtime>() -> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static
{
    tauri::generate_handler![get_version]
}

/// What to do when Tauri asks the app to exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitDecision {
    Prevent,
    Allow,
}

/// `ExitRequested` with no code comes from the last window closing: Perchbar
/// lives on the taskbar, so it stays. An explicit code ("Quit Perchbar")
/// exits.
pub fn exit_decision(code: Option<i32>) -> ExitDecision {
    match code {
        None => ExitDecision::Prevent,
        Some(_) => ExitDecision::Allow,
    }
}

/// Tells the settings page that state changed. Cheap and non-blocking, so
/// the scheduler's publish callback can call it.
pub struct StateBridge {
    tx: watch::Sender<u64>,
}

impl StateBridge {
    /// The bridge and the receiver [`run_state_bridge`] waits on.
    pub fn new() -> (StateBridge, watch::Receiver<u64>) {
        let (tx, rx) = watch::channel(0);
        (StateBridge { tx }, rx)
    }

    pub fn notify(&self) {
        self.tx.send_modify(|n| *n = n.wrapping_add(1));
    }
}

/// Calls `emit` after each change, at most once per [`STATE_THROTTLE`]:
/// a change waits out the interval, and changes during it fold into the
/// same emit. Ends when the bridge is dropped.
pub async fn run_state_bridge(mut rx: watch::Receiver<u64>, mut emit: impl FnMut()) {
    while rx.changed().await.is_ok() {
        tokio::time::sleep(STATE_THROTTLE).await;
        rx.mark_unchanged();
        emit();
    }
}

/// Runs [`run_state_bridge`] on Tauri's runtime, emitting
/// [`EVENT_STATE`] to the settings window when it is open.
pub fn spawn_state_bridge<R: Runtime>(app: &AppHandle<R>, rx: watch::Receiver<u64>) {
    let app = app.clone();
    tauri::async_runtime::spawn(run_state_bridge(rx, move || {
        if app.get_webview_window(LABEL).is_some() {
            if let Err(e) = app.emit_to(LABEL, EVENT_STATE, ()) {
                log::debug!("cannot tell the settings page about new state: {e}");
            }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn closing_settings_does_not_exit() {
        assert_eq!(exit_decision(None), ExitDecision::Prevent);
        assert_eq!(exit_decision(Some(0)), ExitDecision::Allow);
        assert_eq!(exit_decision(Some(1)), ExitDecision::Allow);
    }

    #[tokio::test(start_paused = true)]
    async fn state_bridge_throttles() {
        let (bridge, rx) = StateBridge::new();
        let emits = Arc::new(AtomicUsize::new(0));
        let counter = emits.clone();
        let task = tokio::spawn(run_state_bridge(rx, move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));

        for _ in 0..10 {
            bridge.notify();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(emits.load(Ordering::SeqCst), 1);

        bridge.notify();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(emits.load(Ordering::SeqCst), 1, "waits out the interval");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(emits.load(Ordering::SeqCst), 2);

        drop(bridge);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("ends when the bridge is dropped")
            .unwrap();
    }
}
