//! Finds the app buttons' right edge with UI Automation, on a worker thread.
//!
//! UI Automation calls are cross-process and can take hundreds of ms, or
//! until a timeout when Explorer is busy. The strip thread owns a child of
//! Explorer's taskbar, so it must never block on Explorer: a blocked strip
//! thread stalls taskbar input and any synchronous message Explorer sends
//! it. So one MTA worker that owns no windows does all UI Automation work.
//! The strip sends requests through [`UiaWorker::request`] (coalesced: at
//! most one in flight and one pending) and gets results through a shared
//! slot plus a notify callback that posts a message to the strip thread.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationCacheRequest,
    IUIAutomationCondition, TreeScope_Descendants, UIA_BoundingRectanglePropertyId,
    UIA_ButtonControlTypeId, UIA_ControlTypePropertyId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetWindowThreadProcessId, IsWindowVisible,
};

use crate::strip::placement;

/// UI Automation timeouts, so a hung Explorer cannot stall the worker for long.
const UIA_CONNECTION_TIMEOUT_MS: u32 = 2000;
const UIA_TRANSACTION_TIMEOUT_MS: u32 = 2000;
/// A search slower than this is logged.
const UIA_SLOW_MS: u128 = 50;

/// What to search: the taskbar, plus its rect and tray edge for filtering.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ButtonQuery {
    /// Taskbar HWND as an integer, so the query can cross threads.
    pub taskbar: isize,
    pub taskbar_rect: RECT,
    pub tray_left: i32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ButtonResult {
    pub query: ButtonQuery,
    /// `None` when no button was found or UI Automation failed.
    pub buttons_right: Option<i32>,
}

/// At most one request in flight and one pending; a newer pending request
/// replaces an older one.
#[derive(Debug)]
struct Coalescer<T> {
    in_flight: bool,
    pending: Option<T>,
}

impl<T> Default for Coalescer<T> {
    fn default() -> Self {
        Coalescer {
            in_flight: false,
            pending: None,
        }
    }
}

impl<T> Coalescer<T> {
    /// Returns the request to send now, or `None` if it was parked.
    fn request(&mut self, q: T) -> Option<T> {
        if self.in_flight {
            self.pending = Some(q);
            None
        } else {
            self.in_flight = true;
            Some(q)
        }
    }

    /// The in-flight request finished. Returns the pending one to send next.
    fn complete(&mut self) -> Option<T> {
        let next = self.pending.take();
        self.in_flight = next.is_some();
        next
    }
}

/// The strip thread's handle on the worker.
pub struct UiaWorker {
    tx: Option<Sender<ButtonQuery>>,
    slot: Arc<Mutex<Option<ButtonResult>>>,
    queue: Coalescer<ButtonQuery>,
}

impl UiaWorker {
    /// Starts the worker. `notify` runs on the worker after each result is
    /// stored; it must not block (post a message).
    pub fn start(notify: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = channel();
        let slot = Arc::new(Mutex::new(None));
        let worker_slot = Arc::clone(&slot);
        let spawned = std::thread::Builder::new()
            .name("perchbar-uia".into())
            .spawn(move || worker(rx, worker_slot, notify));
        if let Err(e) = spawned {
            log::warn!("cannot start the UI Automation worker: {e}");
        }
        UiaWorker {
            tx: Some(tx),
            slot,
            queue: Coalescer::default(),
        }
    }

    /// Asks for a search; coalesced with any search already running.
    pub fn request(&mut self, q: ButtonQuery) {
        if let Some(q) = self.queue.request(q) {
            self.send(q);
        }
    }

    fn send(&mut self, q: ButtonQuery) {
        let sent = self.tx.as_ref().is_some_and(|tx| tx.send(q).is_ok());
        if !sent {
            // The worker is gone; nothing will ever complete.
            self.queue = Coalescer::default();
        }
    }

    /// Takes the finished result (call when notified) and sends the pending
    /// request, if any.
    pub fn take_result(&mut self) -> Option<ButtonResult> {
        let result = self.slot.lock().unwrap_or_else(|p| p.into_inner()).take();
        if result.is_some() {
            if let Some(next) = self.queue.complete() {
                self.send(next);
            }
        }
        result
    }

    /// Stops the worker once its current search ends. Does not wait for it:
    /// a search can take up to the UI Automation timeouts.
    pub fn shutdown(&mut self) {
        self.tx = None;
    }
}

fn worker(rx: Receiver<ButtonQuery>, slot: Arc<Mutex<Option<ButtonResult>>>, notify: impl Fn()) {
    // SAFETY: thread-level COM setup, paired with CoUninitialize below.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if let Err(e) = com.ok() {
        log::warn!("UI Automation worker: COM init failed: {e}");
    }
    let uia = match Uia::new() {
        Ok(u) => Some(u),
        Err(e) => {
            log::warn!("UI Automation unavailable: {e}");
            None
        }
    };
    while let Ok(q) = rx.recv() {
        let buttons_right = uia.as_ref().and_then(|u| u.buttons_right(&q));
        *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(ButtonResult {
            query: q,
            buttons_right,
        });
        notify();
    }
    drop(uia);
    if com.is_ok() {
        // SAFETY: pairs the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

/// The cached UI Automation client.
struct Uia {
    automation: IUIAutomation,
    buttons: IUIAutomationCondition,
    cache: IUIAutomationCacheRequest,
}

impl Uia {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: COM is initialized on the worker thread.
        unsafe {
            // CUIAutomation8 implements IUIAutomation2 (timeouts); the older
            // CUIAutomation class does not.
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
            match automation.cast::<IUIAutomation2>() {
                Ok(a2) => {
                    if let Err(e) = a2.SetConnectionTimeout(UIA_CONNECTION_TIMEOUT_MS) {
                        log::warn!("cannot set the UI Automation connection timeout: {e}");
                    }
                    if let Err(e) = a2.SetTransactionTimeout(UIA_TRANSACTION_TIMEOUT_MS) {
                        log::warn!("cannot set the UI Automation transaction timeout: {e}");
                    }
                }
                Err(e) => log::warn!("UI Automation has no IUIAutomation2, no timeouts: {e}"),
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

    fn buttons_right(&self, q: &ButtonQuery) -> Option<i32> {
        let started = Instant::now();
        let result = self.search(q);
        let took = started.elapsed().as_millis();
        if took > UIA_SLOW_MS {
            log::debug!("taskbar button search took {took} ms");
        }
        match result {
            Ok(r) => r,
            Err(e) => {
                log::debug!("taskbar button search failed: {e}");
                None
            }
        }
    }

    fn search(&self, q: &ButtonQuery) -> windows::core::Result<Option<i32>> {
        let mut rects = Vec::new();
        let mut last_err = None;
        for child in searchable_children(HWND(q.taskbar as _)) {
            // SAFETY: COM calls on live interfaces owned by this thread.
            let found = unsafe {
                self.automation.ElementFromHandle(child).and_then(|root| {
                    root.FindAllBuildCache(TreeScope_Descendants, &self.buttons, &self.cache)
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
                if let Ok(r) = rect {
                    rects.push(r);
                }
            }
        }
        match (
            buttons_right(&rects, &q.taskbar_rect, q.tray_left),
            last_err,
        ) {
            (None, Some(e)) => Err(e),
            (r, _) => Ok(r),
        }
    }
}

/// The rightmost edge among button rects that count as app buttons: not
/// empty, inside the taskbar, left of the tray (the tray and its descendants
/// sit at or right of its left edge), and not wider than half the taskbar
/// (a "button" that wide is a container, not a taskbar button).
fn buttons_right(rects: &[RECT], taskbar: &RECT, tray_left: i32) -> Option<i32> {
    let max_width = (taskbar.right - taskbar.left) / 2;
    rects
        .iter()
        .filter(|r| {
            let w = r.right - r.left;
            w > 0
                && w <= max_width
                && r.left >= taskbar.left
                && r.right <= taskbar.right
                && r.left < tray_left
        })
        .map(|r| r.right)
        .max()
}

/// Visible direct children of the taskbar owned by other processes, minus
/// the tray. Searching these one by one, instead of the taskbar itself,
/// keeps UI Automation away from our own strip window (and the worker away
/// from the strip thread), which would otherwise be asked for its provider.
fn searchable_children(taskbar: HWND) -> Vec<HWND> {
    let own_pid = std::process::id();
    let tray = placement::tray_hwnd(taskbar);
    let mut out = Vec::new();
    let mut after: Option<HWND> = None;
    // SAFETY: plain window enumeration; a handle that goes stale only makes
    // the later calls fail.
    unsafe {
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

    fn rect(left: i32, right: i32) -> RECT {
        RECT {
            left,
            top: 1032,
            right,
            bottom: 1080,
        }
    }

    #[test]
    fn coalescer_keeps_one_in_flight_and_the_newest_pending() {
        let mut c = Coalescer::default();
        assert_eq!(c.request(1), Some(1));
        assert_eq!(c.request(2), None);
        assert_eq!(c.request(3), None);
        assert_eq!(c.complete(), Some(3));
        assert_eq!(c.complete(), None);
        // Idle again: the next request goes straight out.
        assert_eq!(c.request(4), Some(4));
        assert_eq!(c.complete(), None);
    }

    #[test]
    fn button_edge_skips_wide_empty_outside_and_tray_rects() {
        let tb = rect(0, 1920);
        let rects = [
            rect(900, 950),
            rect(950, 1000),
            // A container wider than half the taskbar.
            rect(100, 1500),
            // Empty.
            rect(1200, 1200),
            // Outside the taskbar.
            rect(1900, 1960),
            // In the tray.
            rect(1720, 1760),
        ];
        assert_eq!(buttons_right(&rects, &tb, 1700), Some(1000));
        assert_eq!(buttons_right(&[], &tb, 1700), None);
        assert_eq!(buttons_right(&[rect(100, 1500)], &tb, 1700), None);
    }
}
