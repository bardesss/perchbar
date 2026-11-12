//! Where the strip goes on the taskbar: just left of the tray, never over
//! the app buttons.
//!
//! [`read`] reads the live taskbar with plain Win32 calls. The app button
//! edge comes from UI Automation on a separate worker thread
//! (`strip::uia`) and is passed in. [`compute`], [`available_px`],
//! [`is_hidden`] and [`same_row`] are pure so they can be tested.

use windows::core::w;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW, GetWindowRect};

/// Gap between the strip's right edge and the tray, in DIPs.
pub const TRAY_GAP_DIP: f32 = 2.0;
/// Gap between the app buttons and the strip, in DIPs.
pub const APP_GAP_DIP: f32 = crate::strip::layout::MIN_APP_GAP_DIP;
/// A taskbar with less than this many pixels on its monitor is auto-hidden.
const HIDDEN_THRESHOLD_PX: i32 = 4;

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

/// True when two readings share the taskbar row: same horizontal extent,
/// tray edge and DPI. Auto-hide only moves the taskbar vertically, so the
/// app button edge measured for one still holds for the other.
pub fn same_row(a: &TaskbarInfo, b: &TaskbarInfo) -> bool {
    a.taskbar.left == b.taskbar.left
        && a.taskbar.right == b.taskbar.right
        && a.tray_left == b.tray_left
        && a.dpi == b.dpi
}

/// Conservative guess when no UI Automation result is available yet:
/// center plus a quarter width.
pub fn fallback_buttons_right(taskbar: &RECT) -> i32 {
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

/// The tray window, a child of the taskbar.
pub fn tray_hwnd(taskbar: HWND) -> Option<HWND> {
    // SAFETY: plain window lookup.
    unsafe { FindWindowExW(Some(taskbar), None, w!("TrayNotifyWnd"), None) }
        .ok()
        .filter(|h| !h.is_invalid())
}

/// Reads the live taskbar with Win32 calls only (no cross-process COM).
/// `buttons_right` is the last UI Automation result for this taskbar, or
/// `None` to use [`fallback_buttons_right`]. It is capped at the tray edge.
pub fn read(taskbar: HWND, buttons_right: Option<i32>) -> Option<TaskbarInfo> {
    let mut tb = RECT::default();
    // SAFETY: valid out pointer; the window may vanish, which gives an error.
    unsafe { GetWindowRect(taskbar, &mut tb) }.ok()?;
    let tray_left = tray_hwnd(taskbar)
        .and_then(|tray| {
            let mut r = RECT::default();
            // SAFETY: valid out pointer.
            unsafe { GetWindowRect(tray, &mut r) }.ok()?;
            Some(r.left)
        })
        .unwrap_or(tb.right);
    // SAFETY: plain call on a window handle.
    let dpi = unsafe { GetDpiForWindow(taskbar) }.max(96);
    Some(TaskbarInfo {
        taskbar: tb,
        tray_left,
        buttons_right: buttons_right
            .unwrap_or_else(|| fallback_buttons_right(&tb))
            .min(tray_left),
        dpi,
        hidden: monitor_rect(taskbar).is_some_and(|m| is_hidden(&tb, &m)),
    })
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
        assert!(same_row(&prev, &prev));
        // Auto-hide slides the taskbar down; the row is the same.
        let slid = TaskbarInfo {
            taskbar: rect(0, 1078, 1920, 1126),
            hidden: true,
            ..prev
        };
        assert!(same_row(&prev, &slid));
        assert!(!same_row(&prev, &info(1650, 1100, 96, false)));
        assert!(!same_row(&prev, &info(1700, 1100, 144, false)));
        let wider = TaskbarInfo {
            taskbar: rect(0, 1032, 2560, 1080),
            ..prev
        };
        assert!(!same_row(&prev, &wider));
    }

    #[test]
    fn fallback_guess_is_right_of_center() {
        assert_eq!(fallback_buttons_right(&rect(0, 1032, 1920, 1080)), 1440);
    }
}
