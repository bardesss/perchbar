//! Where the strip goes on the taskbar: just left of the tray, never over
//! the app buttons.
//!
//! [`query`] reads the live taskbar (Win32 plus UI Automation); [`compute`]
//! and [`available_px`] are pure so they can be tested.

use std::cell::RefCell;
use std::time::Instant;

use windows::core::Interface;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomation2, IUIAutomationCacheRequest,
    IUIAutomationCondition, TreeScope_Descendants, UIA_BoundingRectanglePropertyId,
    UIA_ButtonControlTypeId, UIA_ControlTypePropertyId,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
};

/// Gap between the strip's right edge and the tray, in DIPs.
pub const TRAY_GAP_DIP: f32 = 2.0;
/// Gap between the app buttons and the strip, in DIPs.
pub const APP_GAP_DIP: f32 = crate::strip::layout::MIN_APP_GAP_DIP;
/// A taskbar with less than this many pixels on its monitor is auto-hidden.
const HIDDEN_THRESHOLD_PX: i32 = 4;
/// UI Automation timeouts, so a hung Explorer cannot stall the strip thread.
const UIA_CONNECTION_TIMEOUT_MS: u32 = 1000;
const UIA_TRANSACTION_TIMEOUT_MS: u32 = 2000;
/// A FindAll slower than this is logged.
const UIA_SLOW_MS: u128 = 50;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TaskbarInfo {
    /// Taskbar window rect in screen pixels.
    pub taskbar: RECT,
    /// Left edge of the tray in screen pixels.
    pub tray_left: i32,
    /// Rightmost edge of app buttons, Start, Search and Widgets, in screen pixels.
    pub buttons_right: i32,
    pub dpi: u32,
    /// Auto-hide has slid the taskbar off screen.
    pub hidden: bool,
}

impl TaskbarInfo {
    pub fn scale(&self) -> f32 {
        self.dpi.max(1) as f32 / 96.0
    }

    pub fn height(&self) -> i32 {
        self.taskbar.bottom - self.taskbar.top
    }
}

fn scaled(dip: f32, scale: f32) -> i32 {
    (dip * scale).round() as i32
}

/// The strip rect relative to the taskbar (x, y, w, h), right edge
/// `TRAY_GAP_DIP` left of the tray. `None` while the taskbar is auto-hidden.
pub fn compute(info: &TaskbarInfo, strip_width_px: i32) -> Option<(i32, i32, i32, i32)> {
    if info.hidden {
        return None;
    }
    let right = info.tray_left - info.taskbar.left - scaled(TRAY_GAP_DIP, info.scale());
    let w = strip_width_px.max(0);
    Some(((right - w).max(0), 0, w, info.height()))
}

/// Pixels the strip may use between the app buttons and the tray.
pub fn available_px(info: &TaskbarInfo) -> i32 {
    let scale = info.scale();
    let free = info.tray_left
        - info.buttons_right
        - scaled(APP_GAP_DIP, scale)
        - scaled(TRAY_GAP_DIP, scale);
    free.max(0)
}

/// True when less than `HIDDEN_THRESHOLD_PX` of the taskbar is on its monitor
/// in the taskbar's thin direction.
pub fn is_hidden(taskbar: &RECT, monitor: &RECT) -> bool {
    let w = taskbar.right.min(monitor.right) - taskbar.left.max(monitor.left);
    let h = taskbar.bottom.min(monitor.bottom) - taskbar.top.max(monitor.top);
    w.min(h) < HIDDEN_THRESHOLD_PX
}

/// Conservative guess when UI Automation fails: center plus a quarter width.
fn fallback_buttons_right(taskbar: &RECT) -> i32 {
    let width = taskbar.right - taskbar.left;
    taskbar.left + width / 2 + width / 4
}

/// The taskbar window, if Explorer is running.
pub fn taskbar_hwnd() -> Option<HWND> {
    // SAFETY: plain window lookup.
    unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }
        .ok()
        .filter(|h| !h.is_invalid())
}

/// Reads the live taskbar. Must run on a thread with COM initialized.
pub fn query() -> Option<TaskbarInfo> {
    query_reusing(None)
}

/// True when `prev`'s button edge still holds for a taskbar with this
/// horizontal extent, tray edge and DPI (auto-hide only moves it vertically).
fn same_row(prev: &TaskbarInfo, tb: &RECT, tray_left: i32, dpi: u32) -> bool {
    prev.taskbar.left == tb.left
        && prev.taskbar.right == tb.right
        && prev.tray_left == tray_left
        && prev.dpi == dpi
}

/// Like [`query`], but reuses `prev.buttons_right` instead of running the
/// UI Automation search when the taskbar row is unchanged. For the periodic
/// fallback tick, so an idle strip does not search the taskbar every time.
pub fn query_reusing(prev: Option<&TaskbarInfo>) -> Option<TaskbarInfo> {
    let taskbar = taskbar_hwnd()?;
    let mut tb = RECT::default();
    // SAFETY: valid out pointer; the window may vanish, which gives an error.
    unsafe { GetWindowRect(taskbar, &mut tb) }.ok()?;
    let tray_left = tray_left(taskbar).unwrap_or(tb.right);
    // SAFETY: plain call on a window handle.
    let dpi = unsafe { GetDpiForWindow(taskbar) }.max(96);
    let buttons_right = match prev.filter(|p| same_row(p, &tb, tray_left, dpi)) {
        Some(p) => p.buttons_right,
        None => uia_buttons_right(taskbar, tray_left, &tb)
            .unwrap_or_else(|| fallback_buttons_right(&tb))
            .min(tray_left),
    };
    Some(TaskbarInfo {
        taskbar: tb,
        tray_left,
        buttons_right,
        dpi,
        hidden: monitor_rect(taskbar).is_some_and(|m| is_hidden(&tb, &m)),
    })
}

fn tray_left(taskbar: HWND) -> Option<i32> {
    // SAFETY: plain window lookup and a valid out pointer.
    unsafe {
        let tray = FindWindowExW(Some(taskbar), None, w!("TrayNotifyWnd"), None).ok()?;
        if tray.is_invalid() {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(tray, &mut r).ok()?;
        Some(r.left)
    }
}

fn monitor_rect(taskbar: HWND) -> Option<RECT> {
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `mi` is a valid MONITORINFO with cbSize set.
    unsafe {
        let mon = MonitorFromWindow(taskbar, MONITOR_DEFAULTTONEAREST);
        GetMonitorInfoW(mon, &mut mi)
            .as_bool()
            .then_some(mi.rcMonitor)
    }
}

/// The cached UI Automation client for this thread.
struct Uia {
    automation: IUIAutomation,
    buttons: IUIAutomationCondition,
    cache: IUIAutomationCacheRequest,
}

impl Uia {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: COM is initialized on the calling thread (see `query`).
        unsafe {
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            if let Ok(a2) = automation.cast::<IUIAutomation2>() {
                let _ = a2.SetConnectionTimeout(UIA_CONNECTION_TIMEOUT_MS);
                let _ = a2.SetTransactionTimeout(UIA_TRANSACTION_TIMEOUT_MS);
            }
            let buttons = automation.CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &VARIANT::from(UIA_ButtonControlTypeId.0),
            )?;
            let cache = automation.CreateCacheRequest()?;
            cache.AddProperty(UIA_BoundingRectanglePropertyId)?;
            Ok(Uia {
                automation,
                buttons,
                cache,
            })
        }
    }
}

thread_local! {
    static UIA: RefCell<Option<Uia>> = const { RefCell::new(None) };
}

/// Drops this thread's UI Automation client; call before `CoUninitialize`.
pub fn release_uia() {
    UIA.with(|u| u.borrow_mut().take());
}

/// Rightmost button edge left of the tray, from one cached FindAll.
fn uia_buttons_right(taskbar: HWND, tray_left: i32, tb: &RECT) -> Option<i32> {
    UIA.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            match Uia::new() {
                Ok(u) => *cell = Some(u),
                Err(e) => {
                    log::warn!("UI Automation unavailable: {e}");
                    return None;
                }
            }
        }
        let uia = cell.as_ref()?;
        let started = Instant::now();
        let result = find_buttons_right(uia, taskbar, tray_left, tb);
        let took = started.elapsed().as_millis();
        if took > UIA_SLOW_MS {
            log::info!("taskbar button query took {took} ms");
        }
        match result {
            Ok(r) => r,
            Err(e) => {
                log::debug!("taskbar button query failed: {e}");
                None
            }
        }
    })
}

fn find_buttons_right(
    uia: &Uia,
    taskbar: HWND,
    tray_left: i32,
    tb: &RECT,
) -> windows::core::Result<Option<i32>> {
    let mut right: Option<i32> = None;
    let mut last_err = None;
    for child in searchable_children(taskbar) {
        // SAFETY: COM calls on live interfaces owned by this thread.
        let found = unsafe {
            uia.automation.ElementFromHandle(child).and_then(|root| {
                root.FindAllBuildCache(TreeScope_Descendants, &uia.buttons, &uia.cache)
            })
        };
        let all = match found {
            Ok(a) => a,
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        };
        // SAFETY: as above.
        let len = unsafe { all.Length() }.unwrap_or(0);
        for i in 0..len {
            // SAFETY: as above; `i` is in range.
            let rect = unsafe { all.GetElement(i).and_then(|e| e.CachedBoundingRectangle()) };
            let Ok(r) = rect else {
                continue;
            };
            // Skip empty, off-taskbar and tray buttons (the tray and its
            // descendants all sit at or right of its left edge).
            if r.right <= r.left || r.left < tb.left || r.right > tb.right || r.left >= tray_left {
                continue;
            }
            right = Some(right.map_or(r.right, |m| m.max(r.right)));
        }
    }
    match (right, last_err) {
        (None, Some(e)) => Err(e),
        (r, _) => Ok(r),
    }
}

/// Visible direct children of the taskbar owned by other processes, minus
/// the tray. Searching these one by one, instead of the taskbar itself,
/// keeps UI Automation away from the strip: the strip is a taskbar child
/// owned by the calling thread, and UIA asking it for its provider while
/// this thread waits inside FindAll deadlocks until the call times out.
fn searchable_children(taskbar: HWND) -> Vec<HWND> {
    let own_pid = std::process::id();
    let mut out = Vec::new();
    let mut after: Option<HWND> = None;
    // SAFETY: plain window enumeration; a handle that goes stale only makes
    // the later calls fail.
    unsafe {
        let tray = FindWindowExW(Some(taskbar), None, w!("TrayNotifyWnd"), PCWSTR::null()).ok();
        while let Ok(child) = FindWindowExW(Some(taskbar), after, PCWSTR::null(), PCWSTR::null()) {
            if child.is_invalid() {
                break;
            }
            after = Some(child);
            let mut pid = 0;
            GetWindowThreadProcessId(child, Some(&mut pid));
            if pid != own_pid && Some(child) != tray && IsWindowVisible(child).as_bool() {
                out.push(child);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(tray_left: i32, buttons_right: i32, dpi: u32, hidden: bool) -> TaskbarInfo {
        TaskbarInfo {
            taskbar: RECT {
                left: 0,
                top: 1032,
                right: 1920,
                bottom: 1080,
            },
            tray_left,
            buttons_right,
            dpi,
            hidden,
        }
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn strip_sits_left_of_tray() {
        let i = info(1700, 1100, 96, false);
        assert_eq!(compute(&i, 200), Some((1498, 0, 200, 48)));
    }

    #[test]
    fn available_excludes_buttons_and_gap() {
        assert_eq!(available_px(&info(1700, 1100, 96, false)), 590);
    }

    #[test]
    fn available_never_negative() {
        assert_eq!(available_px(&info(1700, 1695, 96, false)), 0);
        assert_eq!(available_px(&info(1700, 1900, 96, false)), 0);
    }

    #[test]
    fn hidden_taskbar_gives_none() {
        assert_eq!(compute(&info(1700, 1100, 96, true), 200), None);
    }

    #[test]
    fn scales_gap_with_dpi() {
        let i = info(1700, 1100, 144, false);
        // Right edge gap round(2 * 1.5) = 3; app gap round(8 * 1.5) = 12.
        assert_eq!(compute(&i, 200), Some((1700 - 3 - 200, 0, 200, 48)));
        assert_eq!(available_px(&i), 1700 - 1100 - 12 - 3);
    }

    #[test]
    fn auto_hidden_when_under_four_pixels_on_monitor() {
        let mon = rect(0, 0, 1920, 1080);
        assert!(!is_hidden(&rect(0, 1032, 1920, 1080), &mon));
        assert!(is_hidden(&rect(0, 1078, 1920, 1126), &mon));
        assert!(!is_hidden(&rect(0, 1076, 1920, 1124), &mon));
        // A taskbar on a second monitor is judged against that monitor.
        let right = rect(1920, 0, 3840, 1080);
        assert!(!is_hidden(&rect(1920, 1032, 3840, 1080), &right));
    }

    #[test]
    fn button_edge_is_reused_only_for_the_same_row() {
        let prev = info(1700, 1100, 96, false);
        let tb = prev.taskbar;
        assert!(same_row(&prev, &tb, 1700, 96));
        // Auto-hide slides the taskbar down; the row is the same.
        assert!(same_row(&prev, &rect(0, 1078, 1920, 1126), 1700, 96));
        assert!(!same_row(&prev, &tb, 1650, 96));
        assert!(!same_row(&prev, &tb, 1700, 144));
        assert!(!same_row(&prev, &rect(0, 1032, 2560, 1080), 1700, 96));
    }

    #[test]
    fn fallback_guess_is_right_of_center() {
        assert_eq!(fallback_buttons_right(&rect(0, 1032, 1920, 1080)), 1440);
    }
}
