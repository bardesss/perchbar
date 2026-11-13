//! The "+N" flyout: the widgets that do not fit on the strip, one per row.
//!
//! A top-level layered `WS_POPUP` owned by the strip's control window (never
//! a child of Explorer), on the strip thread. It is drawn with the strip
//! renderer; its window procedure and drawing live with the strip's state in
//! `strip::window`. This module holds what the flyout remembers between
//! messages and the window it shows.

use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, IsWindow, IsWindowVisible, ShowWindow, SW_HIDE, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::strip::layout::{Edge, FlyoutGeometry, FLYOUT_MAX_ROWS};
use crate::strip::theme::Palette;

pub(super) const FLYOUT_CLASS: PCWSTR = w!("PerchbarFlyout");

/// A click on "+N" this soon after the flyout lost the focus is the click
/// that took the focus away, so it does not open the flyout again.
const REOPEN_GUARD: Duration = Duration::from_millis(300);

/// Everything a drawn flyout depends on; it redraws only when this changes.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct FlyoutKey {
    pub generation: u64,
    /// Sorted (slot id, shown member).
    pub groups: Vec<(String, usize)>,
    /// Sorted (instance id, shown frame).
    pub frames: Vec<(String, usize)>,
    /// The hidden slot ids, in row order.
    pub rows: Vec<String>,
    pub hover: Option<String>,
    pub scroll: usize,
    pub palette: Palette,
    pub dpi: u32,
    /// The "+N" item in screen pixels.
    pub anchor: RECT,
    pub work: RECT,
    pub edge: Edge,
}

/// Remembers that a mouse button went down on a window, so its button-up
/// acts only then (not for a press that started elsewhere).
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct ButtonLatch(bool);

impl ButtonLatch {
    pub fn down(&mut self) {
        self.0 = true;
    }

    /// True when this release ends a press that began on the window.
    pub fn up(&mut self) -> bool {
        std::mem::take(&mut self.0)
    }

    pub fn clear(&mut self) {
        self.0 = false;
    }
}

#[derive(Debug, Default)]
pub(super) struct Flyout {
    /// Created on first open, hidden while closed, destroyed with the strip.
    hwnd: Option<HWND>,
    pub open: bool,
    /// The hidden slot id under the pointer.
    pub hover: Option<String>,
    /// The first row shown.
    pub scroll: usize,
    pub tracking_mouse: bool,
    pub left: ButtonLatch,
    pub right: ButtonLatch,
    /// Wheel movement not yet worth a whole step.
    pub wheel_acc: i32,
    /// What is on screen: its key, where it is, and the slot id of each
    /// row (all of them, scrolled or not).
    pub key: Option<FlyoutKey>,
    geometry: Option<FlyoutGeometry>,
    rows: Vec<String>,
    /// When it last closed because it lost the focus.
    deactivated_at: Option<Instant>,
}

impl Flyout {
    /// The live window, if any.
    pub fn hwnd(&self) -> Option<HWND> {
        // SAFETY: IsWindow accepts any handle.
        self.hwnd
            .filter(|h| unsafe { IsWindow(Some(*h)) }.as_bool())
    }

    /// The window, created on first use and owned by `owner`.
    pub fn window(&mut self, owner: HWND) -> Option<HWND> {
        if let Some(h) = self.hwnd() {
            return Some(h);
        }
        match create_window(owner) {
            Ok(h) => {
                self.hwnd = Some(h);
                Some(h)
            }
            Err(e) => {
                log::warn!("cannot create the flyout: {e}");
                None
            }
        }
    }

    /// Records what was just drawn.
    pub fn drawn(&mut self, key: FlyoutKey, geometry: FlyoutGeometry) {
        self.rows.clone_from(&key.rows);
        self.key = Some(key);
        self.geometry = Some(geometry);
    }

    /// Hides the window and forgets the pointer, scroll and drawing.
    /// `deactivated`: it closes because it lost the focus.
    pub fn close(&mut self, deactivated: bool) {
        if deactivated {
            self.deactivated_at = Some(Instant::now());
        }
        if !self.open {
            return;
        }
        self.open = false;
        if let Some(h) = self.hwnd() {
            // SAFETY: our own window.
            unsafe {
                if IsWindowVisible(h).as_bool() {
                    let _ = ShowWindow(h, SW_HIDE);
                }
            }
        }
        self.hover = None;
        self.scroll = 0;
        self.tracking_mouse = false;
        self.left.clear();
        self.right.clear();
        self.wheel_acc = 0;
        self.key = None;
        self.geometry = None;
        self.rows.clear();
    }

    /// Closes it and destroys the window.
    pub fn destroy(&mut self) {
        self.close(false);
        if let Some(h) = self.hwnd.take() {
            // SAFETY: our own window; DestroyWindow fails harmlessly if it
            // is already gone.
            let _ = unsafe { DestroyWindow(h) };
        }
    }

    /// True right after it closed because it lost the focus.
    pub fn recently_deactivated(&self, now: Instant) -> bool {
        self.deactivated_at
            .is_some_and(|t| now.saturating_duration_since(t) < REOPEN_GUARD)
    }

    /// The slot id of the row at client pixel (`x`, `y`).
    pub fn row_at(&self, x: i32, y: i32) -> Option<String> {
        let geo = self.geometry.as_ref()?;
        let i = geo
            .rows
            .iter()
            .position(|r| x >= r.left && x < r.right && y >= r.top && y < r.bottom)?;
        self.rows.get(self.scroll + i).cloned()
    }

    /// The screen rect of the row showing `slot_id`, when it is visible.
    pub fn row_screen_rect(&self, slot_id: &str) -> Option<RECT> {
        let geo = self.geometry.as_ref()?;
        let i = self.rows.iter().position(|r| r == slot_id)?;
        let r = geo.rows.get(i.checked_sub(self.scroll)?)?;
        Some(RECT {
            left: geo.rect.left + r.left,
            top: geo.rect.top + r.top,
            right: geo.rect.left + r.right,
            bottom: geo.rect.top + r.bottom,
        })
    }

    /// True when there are more rows than fit, so the wheel scrolls.
    pub fn scrolls(&self, rows: usize) -> bool {
        rows > FLYOUT_MAX_ROWS
    }

    /// Keeps `scroll` valid for `rows` rows.
    pub fn clamp_scroll(&mut self, rows: usize) {
        self.scroll = self.scroll.min(max_scroll(rows));
    }

    /// Scrolls by `steps` rows (negative: up). Returns true when it moved.
    pub fn scroll_by(&mut self, steps: i32, rows: usize) -> bool {
        let to = (self.scroll as i64 + i64::from(steps)).clamp(0, max_scroll(rows) as i64) as usize;
        let moved = to != self.scroll;
        self.scroll = to;
        moved
    }
}

/// The largest first row for `rows` rows.
fn max_scroll(rows: usize) -> usize {
    rows.saturating_sub(FLYOUT_MAX_ROWS)
}

/// A hidden layered popup with DWM rounded corners. The pixels come from
/// `UpdateLayeredWindow`, which DWM does not round, so the renderer draws
/// the rounded panel itself; the attribute is set for the shadow and in
/// case a later Windows rounds layered windows too.
fn create_window(owner: HWND) -> windows::core::Result<HWND> {
    // SAFETY: creates a popup of our registered class owned by our own
    // window; the attribute pointer is valid for the call.
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            FLYOUT_CLASS,
            w!("Perchbar"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            Some(owner),
            None,
            GetModuleHandleW(None).ok().map(Into::into),
            None,
        )?;
        let round: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND;
        if let Err(e) = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            std::ptr::from_ref(&round).cast(),
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        ) {
            log::debug!("cannot round the flyout corners: {e}");
        }
        Ok(hwnd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    fn drawn(rows: usize, scroll: usize) -> Flyout {
        let ids: Vec<String> = (0..rows).map(|i| format!("s{i}")).collect();
        let shown = rows.min(FLYOUT_MAX_ROWS);
        let geometry = FlyoutGeometry {
            rect: rect(100, 200, 260, 300),
            rows: (0..shown as i32)
                .map(|i| rect(8, 8 + i * 44, 152, 48 + i * 44))
                .collect(),
        };
        let mut f = Flyout {
            scroll,
            ..Flyout::default()
        };
        f.rows = ids;
        f.geometry = Some(geometry);
        f
    }

    #[test]
    fn rows_are_found_under_the_pointer_after_scrolling() {
        let f = drawn(20, 0);
        assert_eq!(f.row_at(10, 10).as_deref(), Some("s0"));
        assert_eq!(f.row_at(10, 60).as_deref(), Some("s1"));
        // Padding and the gap between rows hit nothing.
        assert_eq!(f.row_at(2, 10), None);
        assert_eq!(f.row_at(10, 50), None);
        let f = drawn(20, 3);
        assert_eq!(f.row_at(10, 10).as_deref(), Some("s3"));
        assert_eq!(
            f.row_screen_rect("s4"),
            Some(rect(108, 252, 252, 292)),
            "second visible row, in screen pixels"
        );
        // Scrolled out of view.
        assert_eq!(f.row_screen_rect("s1"), None);
        assert_eq!(f.row_screen_rect("s15"), None);
    }

    #[test]
    fn scrolling_stays_within_the_rows() {
        let mut f = drawn(20, 0);
        assert!(f.scrolls(20));
        assert!(!f.scrolls(12));
        assert!(!f.scroll_by(-1, 20), "already at the top");
        assert!(f.scroll_by(5, 20));
        assert_eq!(f.scroll, 5);
        assert!(f.scroll_by(10, 20));
        assert_eq!(f.scroll, 8, "the last 12 rows");
        // Rows went away: the scroll follows.
        f.clamp_scroll(14);
        assert_eq!(f.scroll, 2);
        f.clamp_scroll(3);
        assert_eq!(f.scroll, 0);
    }

    #[test]
    fn a_close_by_deactivation_guards_the_next_click_briefly() {
        let mut f = Flyout::default();
        let now = Instant::now();
        assert!(!f.recently_deactivated(now));
        f.close(true);
        assert!(f.recently_deactivated(Instant::now()));
        assert!(!f.recently_deactivated(Instant::now() + REOPEN_GUARD));
        // An explicit close (Esc, "+N") does not.
        let mut f = Flyout::default();
        f.close(false);
        assert!(!f.recently_deactivated(Instant::now()));
    }
}
