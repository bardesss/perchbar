//! The settings window: one webview window labelled `settings`, created on
//! demand and destroyed when closed. Perchbar keeps running without it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tauri::webview::PageLoadEvent;
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
#[derive(Debug)]
struct Readiness {
    ready: bool,
    pending: Option<String>,
}

impl Readiness {
    const fn new() -> Readiness {
        Readiness {
            ready: false,
            pending: None,
        }
    }

    /// A new window: nothing is ready, `focus` waits for the page.
    fn opening(&mut self, focus: Option<String>) {
        self.ready = false;
        self.pending = focus;
    }

    /// A page (re)load started: the old page's listener is gone, so focus
    /// requests wait for the new page. A request already waiting stays.
    fn page_started(&mut self) {
        self.ready = false;
    }

    /// The page listens now. Returns the instance to focus, if one waited.
    fn mark_ready(&mut self) -> Option<String> {
        self.ready = true;
        self.pending.take()
    }

    /// Asks to focus `id`. Returns it when the page can hear it now;
    /// otherwise it waits for [`Readiness::mark_ready`].
    fn request(&mut self, id: String) -> Option<String> {
        if self.ready {
            Some(id)
        } else {
            self.pending = Some(id);
            None
        }
    }
}

static READINESS: Mutex<Readiness> = Mutex::new(Readiness::new());
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
            let pending = readiness().mark_ready();
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
            let now = readiness().request(id);
            if let Some(id) = now {
                emit_focus(app, id);
            }
        }
        return;
    }

    readiness().opening(focus);
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
    let mut builder = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()));
    if !mica_supported() {
        // The page paints its own solid background instead (theme.css).
        builder = builder.initialization_script(SOLID_BACKDROP_SCRIPT);
    }
    builder
        // F5 or Ctrl+R reloads the page; the new page says ready again.
        .on_page_load(|_, payload| {
            if payload.event() == PageLoadEvent::Started {
                readiness().page_started();
            }
        })
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

/// Tells the page that no Mica backdrop shows through it.
const SOLID_BACKDROP_SCRIPT: &str = "window.__PERCHBAR_SOLID_BACKDROP__ = true;";

/// The first Windows build with Mica.
const MICA_MIN_BUILD: u32 = 22000;

/// Whether Windows can draw Mica behind the window. When transparency
/// effects are off, Windows itself draws a solid color in its place.
fn mica_supported() -> bool {
    windows_build().is_some_and(|b| b >= MICA_MIN_BUILD)
}

/// `CurrentBuildNumber` from the registry (read only).
fn windows_build() -> Option<u32> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let key: Vec<u16> = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let value: Vec<u16> = "CurrentBuildNumber".encode_utf16().chain(Some(0)).collect();
    let mut buf = [0u16; 32];
    let mut len = std::mem::size_of_val(&buf) as u32;
    // SAFETY: the names are NUL-terminated and `len` is the size of `buf`
    // in bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let chars = (len as usize / 2).min(buf.len());
    String::from_utf16_lossy(&buf[..chars])
        .trim_end_matches('\0')
        .trim()
        .parse()
        .ok()
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
    use crate::commands::ipc;
    tauri::generate_handler![
        get_version,
        ipc::get_state,
        ipc::add_instance,
        ipc::remove_instance,
        ipc::update_settings,
        ipc::set_secret,
        ipc::clear_secret,
        ipc::set_hidden,
        ipc::set_max_width,
        ipc::set_order,
        ipc::create_group,
        ipc::update_group,
        ipc::ungroup,
        ipc::refresh_now,
        ipc::get_logs,
        ipc::set_autostart,
        ipc::open_log_folder,
    ]
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
    fn focus_waits_for_the_page() {
        let mut r = Readiness::new();
        r.opening(Some("inst-1".into()));
        assert_eq!(r.request("inst-2".into()), None, "not ready yet");
        assert_eq!(r.mark_ready(), Some("inst-2".into()));
        assert_eq!(r.request("inst-3".into()), Some("inst-3".into()));
    }

    #[test]
    fn reload_waits_for_the_new_page() {
        let mut r = Readiness::new();
        r.opening(None);
        assert_eq!(r.mark_ready(), None);
        r.page_started();
        assert_eq!(r.request("inst-1".into()), None, "old page is gone");
        assert_eq!(r.mark_ready(), Some("inst-1".into()));
    }

    #[test]
    fn this_windows_has_a_build_number() {
        assert!(windows_build().is_some_and(|b| b > 10000));
    }

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
