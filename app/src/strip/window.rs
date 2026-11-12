//! The strip window: a per-pixel-alpha layered child of `Shell_TrayWnd`.
//!
//! Everything runs on one thread with a Win32 message loop. A hidden
//! top-level control window owns the timers and receives broadcasts
//! (`TaskbarCreated`, `WM_SETTINGCHANGE`) that a taskbar child would not.
//! State lives in a thread-local and is only touched from window procedures
//! dispatched by the loop; nested messages (sent while the state is in use)
//! never touch it, and any that would are reposted or dropped.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, ScreenToClient, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX, TOOLTIPS_CLASSW, TTF_ABSOLUTE,
    TTF_TRACK, TTM_ADDTOOLW, TTM_GETBUBBLESIZE, TTM_SETMAXTIPWIDTH, TTM_TRACKACTIVATE,
    TTM_TRACKPOSITION, TTM_UPDATETIPTEXTW, TTS_ALWAYSTIP, TTS_NOPREFIX, TTTOOLINFOW,
};
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetWindow, GetWindowRect,
    GetWindowThreadProcessId, IsChild, IsWindow, IsWindowVisible, KillTimer, LoadCursorW,
    PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SendMessageW, SetCursor,
    SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow, TrackPopupMenuEx, TranslateMessage,
    CW_USEDEFAULT, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_LOCATIONCHANGE,
    GW_CHILD, HWND_TOP, IDC_ARROW, MA_NOACTIVATE, MF_SEPARATOR, MF_STRING, MSG, OBJID_CLIENT,
    OBJID_WINDOW, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, ULW_ALPHA, WINDOW_STYLE, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_APP, WM_COMMAND, WM_DPICHANGED, WM_DPICHANGED_AFTERPARENT,
    WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NULL,
    WM_RBUTTONUP, WM_SETCURSOR, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_CHILD, WS_CLIPSIBLINGS,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_NOPARENTNOTIFY, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};

use crate::display::Block;
use crate::scheduler::SchedulerHandle;
use crate::strip::layout::{self, Hit, Metrics, PlacedSlot, RotationState, StripLayout};
use crate::strip::model::{SlotKind, SlotState, StripModel};
use crate::strip::placement::{self, TaskbarInfo};
use crate::strip::render::{Bitmap, Renderer};
use crate::strip::theme::{self, Palette};
use crate::strip::uia::{ButtonQuery, UiaWorker};
use crate::strip::{rotation_secs, RenderKey, StripEvent};

/// Drain the event channel.
const WM_APP_EVENTS: u32 = WM_APP;
/// Something on the taskbar may have moved: run a debounced placement tick.
const WM_APP_TICK: u32 = WM_APP + 1;
/// The UI Automation worker stored a result.
const WM_APP_BUTTONS: u32 = WM_APP + 2;
/// The strip-creation warning is logged at most this often.
const CREATE_WARN_INTERVAL: Duration = Duration::from_secs(60);
/// Not exported by the `windows` features in use.
const WM_MOUSELEAVE: u32 = 0x02A3;

const T_FALLBACK: usize = 1;
const T_DEBOUNCE: usize = 2;
const T_ROTATE: usize = 3;
const T_TOOLTIP: usize = 4;
const T_PRESS: usize = 5;
const FALLBACK_MS: u32 = 5000;
const DEBOUNCE_MS: u32 = 250;
/// Hover delay before the tooltip shows.
const TOOLTIP_MS: u32 = 400;
/// How long a clicked slot stays dimmed.
const PRESS_MS: u32 = 200;
/// Tooltip lines wrap at this width (DIPs); it also enables `\n` breaks.
const TOOLTIP_MAX_WIDTH_DIP: f32 = 320.0;
/// Space between the strip and its tooltip (DIPs).
const TOOLTIP_GAP_DIP: f32 = 4.0;
/// One wheel notch.
const WHEEL_DELTA: i32 = 120;

/// Menu command ids. Hidden-widget entries of the "+N" menu start at
/// `ID_OVERFLOW_BASE`. `ID_QUIT` is also accepted as a `WM_COMMAND` posted
/// to the control window.
const ID_REFRESH: u32 = 1;
const ID_HIDE: u32 = 2;
const ID_QUIT: u32 = 3;
const ID_OVERFLOW_BASE: u32 = 100;
/// A WM_TIMER can fire a little early; this keeps such a tick from missing
/// a rotation that is due.
const ROTATE_SLACK: Duration = Duration::from_millis(100);

const CTL_CLASS: PCWSTR = w!("PerchbarStripControl");
const STRIP_CLASS: PCWSTR = w!("PerchbarStrip");

/// The control window, for [`wake`] from other threads. 0 until created.
static CTL: AtomicIsize = AtomicIsize::new(0);
/// The registered `TaskbarCreated` message.
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
/// The current taskbar HWND, for the WinEvent filter. 0 when unknown.
static TASKBAR: AtomicIsize = AtomicIsize::new(0);

/// Wakes the strip thread so it drains the channel. Non-blocking.
pub fn wake() {
    let ctl = CTL.load(Ordering::Acquire);
    if ctl != 0 {
        // SAFETY: posting to a window handle is safe even if it was destroyed.
        unsafe {
            let _ = PostMessageW(Some(HWND(ctl as _)), WM_APP_EVENTS, WPARAM(0), LPARAM(0));
        }
    }
}

struct State {
    rx: Receiver<StripEvent>,
    sched: Option<SchedulerHandle>,
    ctl: HWND,
    strip: Option<HWND>,
    taskbar: Option<HWND>,
    explorer_pid: u32,
    hooks: Vec<HWINEVENTHOOK>,
    renderer: Renderer,
    model: StripModel,
    rot: RotationState,
    hover: Option<String>,
    tracking_mouse: bool,
    dark: bool,
    palette: Palette,
    info: Option<TaskbarInfo>,
    layout: StripLayout,
    width_px: i32,
    key: Option<RenderKey>,
    pos: Option<(i32, i32, i32, i32)>,
    rotate_secs: Option<u32>,
    debounce_pending: bool,
    uia: UiaWorker,
    /// Last UI Automation result for the current taskbar; `None` until the
    /// first one arrives (the placement fallback guess applies meanwhile).
    buttons_right: Option<i32>,
    last_create_warn: Option<Instant>,
    /// Widget folder to manifest name, for the "+N" menu.
    names: HashMap<PathBuf, String>,
    /// Ends the whole app; taken when "Quit Perchbar" runs.
    on_quit: Option<Box<dyn FnOnce() + Send>>,
    quitting: bool,
    /// When each instance's output last changed, for stale tooltips.
    updated: HashMap<String, DateTime<Local>>,
    /// The slot dimmed after a click, until `T_PRESS` fires.
    pressed: Option<String>,
    /// Wheel movement not yet worth a whole step.
    wheel_acc: i32,
    tooltip: Option<HWND>,
    /// Text and position of the tooltip on screen; `None` while hidden.
    tooltip_shown: Option<(String, i32, i32)>,
}

/// One entry of a popup menu.
#[derive(Debug, Clone, PartialEq)]
enum MenuItem {
    Command(u32, String),
    Separator,
}

/// A popup menu to show, plus what its commands act on. Built under the
/// state, shown with the state released (the menu runs a modal loop that
/// dispatches our other messages), then handed back with the chosen id.
#[derive(Debug, Default)]
struct MenuSpec {
    items: Vec<MenuItem>,
    /// The instance "Refresh now" and "Hide this widget" act on.
    target: Option<String>,
    /// Instance ids of the "+N" entries, by index from `ID_OVERFLOW_BASE`.
    overflow: Vec<String>,
}

/// A hidden widget as listed in the "+N" menu.
#[derive(Debug, Clone, PartialEq)]
struct OverflowEntry {
    instance_id: String,
    label: String,
}

/// How much of the UI Automation search a tick asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Search {
    /// Always re-search the app buttons.
    Always,
    /// Only when the taskbar row changed (auto-hide moves are vertical).
    IfRowChanged,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Runs `f` on the state unless it is missing or already in use.
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| {
        let mut guard = s.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

/// The strip thread's body.
pub(super) fn run(
    rx: Receiver<StripEvent>,
    sched: SchedulerHandle,
    names: HashMap<PathBuf, String>,
    on_quit: Box<dyn FnOnce() + Send>,
) {
    // Explorer's taskbar is per-monitor v2 aware; a child must match it.
    // SAFETY: thread-level setting with a constant argument.
    let previous =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    if previous.0.is_null() {
        log::warn!("cannot make the strip thread per-monitor DPI aware");
    }
    match setup(rx, sched, names, on_quit) {
        Ok(()) => message_loop(),
        Err(e) => log::error!("cannot start the taskbar strip: {e}"),
    }
    CTL.store(0, Ordering::Release);
    TASKBAR.store(0, Ordering::Release);
    STATE.with(|s| s.borrow_mut().take());
}

fn setup(
    rx: Receiver<StripEvent>,
    sched: SchedulerHandle,
    names: HashMap<PathBuf, String>,
    on_quit: Box<dyn FnOnce() + Send>,
) -> windows::core::Result<()> {
    // SAFETY: class registration and window creation with valid arguments.
    let ctl = unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_WIN95_CLASSES,
        };
        if !InitCommonControlsEx(&icc).as_bool() {
            log::warn!("cannot load common controls; tooltips may not show");
        }
        let hinst = GetModuleHandleW(None)?.into();
        let cursor = LoadCursorW(None, IDC_ARROW)?;
        for (name, proc_) in [
            (
                CTL_CLASS,
                ctl_proc as unsafe extern "system" fn(_, _, _, _) -> _,
            ),
            (STRIP_CLASS, strip_proc),
        ] {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc_),
                hInstance: hinst,
                lpszClassName: name,
                hCursor: cursor,
                ..Default::default()
            };
            // Fails harmlessly if the class already exists.
            RegisterClassW(&wc);
        }
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(w!("TaskbarCreated")),
            Ordering::Release,
        );
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            CTL_CLASS,
            w!("Perchbar"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst),
            None,
        )?
    };
    let renderer = Renderer::new()?;
    let (dark, palette) = theme::current();
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            rx,
            sched: Some(sched),
            ctl,
            strip: None,
            taskbar: None,
            explorer_pid: 0,
            hooks: Vec::new(),
            renderer,
            model: StripModel::default(),
            rot: RotationState::default(),
            hover: None,
            tracking_mouse: false,
            dark,
            palette,
            info: None,
            layout: StripLayout::default(),
            width_px: 0,
            key: None,
            pos: None,
            rotate_secs: None,
            debounce_pending: false,
            uia: UiaWorker::start(|| post_to_ctl(WM_APP_BUTTONS)),
            buttons_right: None,
            last_create_warn: None,
            names,
            on_quit: Some(on_quit),
            quitting: false,
            updated: HashMap::new(),
            pressed: None,
            wheel_acc: 0,
            tooltip: None,
            tooltip_shown: None,
        });
    });
    CTL.store(ctl.0 as isize, Ordering::Release);
    // SAFETY: timer on our own window.
    unsafe { SetTimer(Some(ctl), T_FALLBACK, FALLBACK_MS, None) };
    with_state(|s| {
        s.tick(Search::Always);
        // Models published before the window existed are waiting.
        s.drain();
    });
    Ok(())
}

fn message_loop() {
    let mut msg = MSG::default();
    // SAFETY: standard message loop; GetMessageW returns -1 on error, 0 on quit.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

impl State {
    fn drain(&mut self) {
        let mut latest = None;
        loop {
            match self.rx.try_recv() {
                Ok(StripEvent::Model(m)) => latest = Some(m),
                Ok(StripEvent::Quit) | Err(TryRecvError::Disconnected) => {
                    self.quit();
                    return;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if let Some(m) = latest {
            let old = std::mem::replace(&mut self.model, m);
            note_updates(&old, &self.model, Local::now(), &mut self.updated);
            self.rot.tick(&self.model, Instant::now() + ROTATE_SLACK);
            self.update_rotation_timer();
            self.refresh();
            self.sync_tooltip();
        }
    }

    fn update_rotation_timer(&mut self) {
        let secs = rotation_secs(&self.model);
        if secs == self.rotate_secs {
            return;
        }
        self.rotate_secs = secs;
        // SAFETY: timers on our own window; SetTimer replaces an existing one.
        unsafe {
            match secs {
                Some(s) => {
                    SetTimer(Some(self.ctl), T_ROTATE, s.saturating_mul(1000), None);
                }
                None => {
                    let _ = KillTimer(Some(self.ctl), T_ROTATE);
                }
            }
        }
    }

    fn rotate(&mut self) {
        self.rot.tick(&self.model, Instant::now() + ROTATE_SLACK);
        self.refresh();
    }

    fn schedule_tick(&mut self) {
        if !self.debounce_pending {
            self.debounce_pending = true;
            // SAFETY: timer on our own window.
            unsafe { SetTimer(Some(self.ctl), T_DEBOUNCE, DEBOUNCE_MS, None) };
        }
    }

    fn reload_theme(&mut self) {
        let (dark, palette) = theme::current();
        if (dark, palette) != (self.dark, self.palette) {
            log::info!("taskbar theme changed (dark: {dark})");
            self.dark = dark;
            self.palette = palette;
            // The key holds `dark` only; the accent may change on its own.
            self.key = None;
            self.refresh();
        }
    }

    fn strip_alive(&self) -> bool {
        // SAFETY: IsWindow accepts any handle.
        self.strip
            .is_some_and(|h| unsafe { IsWindow(Some(h)) }.as_bool())
    }

    /// Re-reads the taskbar (Win32 only, never blocking on Explorer),
    /// recreating the strip or hooks when Explorer restarted, then
    /// repositions and redraws as needed. The app button search is handed to
    /// the UI Automation worker; its result arrives as `WM_APP_BUTTONS`.
    fn tick(&mut self, search: Search) {
        let Some(taskbar) = placement::taskbar_hwnd() else {
            self.info = None;
            TASKBAR.store(0, Ordering::Release);
            return;
        };
        let mut search = search;
        if self.taskbar != Some(taskbar) || !self.strip_alive() {
            self.recreate(taskbar);
            search = Search::Always;
        }
        TASKBAR.store(taskbar.0 as isize, Ordering::Release);
        let mut pid = 0;
        // SAFETY: valid out pointer.
        unsafe { GetWindowThreadProcessId(taskbar, Some(&mut pid)) };
        if pid != 0 && pid != self.explorer_pid {
            self.rehook(pid);
        }
        let info = placement::read(taskbar, self.buttons_right);
        let row_changed = match (&self.info, &info) {
            (Some(a), Some(b)) => !placement::same_row(a, b),
            _ => true,
        };
        self.info = info;
        log::debug!("taskbar: {:?}", self.info);
        if let Some(i) = info {
            if search == Search::Always || row_changed {
                self.uia.request(ButtonQuery {
                    taskbar: taskbar.0 as isize,
                    taskbar_rect: i.taskbar,
                    tray_left: i.tray_left,
                });
            }
        }
        self.refresh();
    }

    /// Applies a finished UI Automation search.
    fn on_buttons(&mut self) {
        let Some(result) = self.uia.take_result() else {
            return;
        };
        let current = self.taskbar.map(|h| h.0 as isize);
        if Some(result.query.taskbar) != current {
            return;
        }
        log::debug!("app buttons end at {:?}", result.buttons_right);
        // A failed search keeps the last good edge.
        if let Some(right) = result.buttons_right {
            self.buttons_right = Some(right);
            if let Some(info) = self.info.as_mut() {
                info.buttons_right = right.min(info.tray_left);
            }
            self.refresh();
        }
    }

    fn recreate(&mut self, taskbar: HWND) {
        if self.strip_alive() {
            if let Some(old) = self.strip.take() {
                // SAFETY: our own window.
                let _ = unsafe { DestroyWindow(old) };
            }
        }
        if self.taskbar != Some(taskbar) {
            // A new taskbar: the old button edge means nothing.
            self.buttons_right = None;
            self.info = None;
        }
        // SAFETY: creates a child of the taskbar with our registered class.
        // WS_EX_NOPARENTNOTIFY: creating or destroying the strip must not send
        // WM_PARENTNOTIFY to Explorer, which blocks if Explorer is hung.
        let created = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOPARENTNOTIFY,
                STRIP_CLASS,
                w!("Perchbar"),
                WS_CHILD | WS_CLIPSIBLINGS,
                0,
                0,
                1,
                1,
                Some(taskbar),
                None,
                GetModuleHandleW(None).ok().map(Into::into),
                None,
            )
        };
        match created {
            Ok(h) => {
                log::info!("strip attached to taskbar {:?}", taskbar.0);
                self.strip = Some(h);
            }
            Err(e) => {
                let now = Instant::now();
                let quiet = self
                    .last_create_warn
                    .is_some_and(|t| now.duration_since(t) < CREATE_WARN_INTERVAL);
                if quiet {
                    log::debug!("cannot create the strip window: {e}");
                } else {
                    log::warn!("cannot create the strip window: {e}");
                    self.last_create_warn = Some(now);
                }
                self.strip = None;
            }
        }
        self.taskbar = Some(taskbar);
        self.key = None;
        self.pos = None;
        self.hover = None;
        self.pressed = None;
        self.tracking_mouse = false;
        self.rot.paused_slot = None;
        self.hide_tooltip();
    }

    fn unhook(&mut self) {
        for h in self.hooks.drain(..) {
            // SAFETY: hooks we installed.
            let _ = unsafe { UnhookWinEvent(h) };
        }
    }

    fn rehook(&mut self, pid: u32) {
        self.unhook();
        for (min, max) in [
            (EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY),
            (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
        ] {
            // SAFETY: out-of-context hook delivered to this thread's loop.
            let h = unsafe {
                SetWinEventHook(
                    min,
                    max,
                    None,
                    Some(win_event),
                    pid,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
            if h.is_invalid() {
                log::warn!("cannot hook taskbar events for process {pid}");
            } else {
                self.hooks.push(h);
            }
        }
        self.explorer_pid = pid;
    }

    fn render_key(&self, info: &TaskbarInfo) -> RenderKey {
        let mut groups: Vec<(String, usize)> = self
            .rot
            .group_index
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        groups.sort();
        let mut frames: Vec<(String, usize)> = self
            .rot
            .frame_index
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        frames.sort();
        RenderKey {
            generation: self.model.generation,
            groups,
            frames,
            hover: self.hover.clone(),
            pressed: self.pressed.clone(),
            dark: self.dark,
            dpi: info.dpi,
            height: info.height(),
            available: placement::available_px(info),
        }
    }

    /// Redraws when the key changed, then moves, shows or hides the strip.
    fn refresh(&mut self) {
        let (Some(strip), Some(info), Some(taskbar)) = (self.strip, self.info, self.taskbar) else {
            return;
        };
        let key = self.render_key(&info);
        let mut fresh: Option<Bitmap> = None;
        if self.key.as_ref() != Some(&key) {
            let scale = info.scale();
            let metrics = Metrics {
                height_dip: info.height() as f32 / scale,
                scale,
                available_dip: key.available as f32 / scale,
            };
            let lay = layout::layout(&self.model, &self.rot, &metrics, &self.renderer);
            match self.renderer.render(
                &lay,
                &self.model,
                &metrics,
                &self.palette,
                self.hover.as_deref(),
            ) {
                Ok(mut bmp) => {
                    if let Some(p) = lay
                        .items
                        .iter()
                        .find(|p| Some(&p.slot_id) == self.pressed.as_ref())
                    {
                        let x0 = layout::px(p.x_dip, scale);
                        let x1 = layout::px(p.x_dip + p.width_dip, scale);
                        dim_columns(&mut bmp.pixels, bmp.width, x0, x1);
                    }
                    self.width_px = bmp.width;
                    self.layout = lay;
                    self.key = Some(key);
                    fresh = Some(bmp);
                }
                // Keep showing the previous frame.
                Err(e) => log::warn!("strip render failed: {e}"),
            }
        }
        // SAFETY: window calls on our own strip and the live taskbar handle.
        unsafe {
            let visible = IsWindowVisible(strip).as_bool();
            match placement::compute(&info, self.width_px) {
                Some(pos) if self.width_px > 0 => {
                    let on_top = GetWindow(taskbar, GW_CHILD).ok() == Some(strip);
                    if self.pos != Some(pos) || !on_top || !visible {
                        let (x, y, w, h) = pos;
                        if let Err(e) = SetWindowPos(
                            strip,
                            Some(HWND_TOP),
                            x,
                            y,
                            w,
                            h,
                            SWP_NOACTIVATE | SWP_SHOWWINDOW,
                        ) {
                            log::debug!("cannot position the strip: {e}");
                        }
                        self.pos = Some(pos);
                    }
                }
                _ => {
                    if visible {
                        let _ = ShowWindow(strip, SW_HIDE);
                    }
                    self.pos = None;
                }
            }
        }
        if let Some(bmp) = fresh.filter(|b| b.width > 0 && b.height > 0) {
            if let Err(e) = present(strip, &bmp) {
                log::warn!("cannot update the strip: {e}");
            }
        }
    }

    fn on_mouse_move(&mut self, x_px: i32) {
        if !self.tracking_mouse {
            if let Some(strip) = self.strip {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: strip,
                    dwHoverTime: 0,
                };
                // SAFETY: valid struct for our own window.
                self.tracking_mouse = unsafe { TrackMouseEvent(&mut tme) }.is_ok();
            }
        }
        let hover = match self.hit(x_px) {
            Some(Hit::Slot(id)) => Some(id),
            _ => None,
        };
        self.set_hover(hover);
    }

    /// What is under client x `x_px` of the strip.
    fn hit(&self, x_px: i32) -> Option<Hit> {
        let scale = self.info.map_or(1.0, |i| i.scale());
        layout::hit_test(&self.layout, x_px as f32 / scale)
    }

    fn set_hover(&mut self, hover: Option<String>) {
        if hover != self.hover {
            log::debug!("hover {hover:?}");
            self.rot.paused_slot.clone_from(&hover);
            self.hover = hover;
            self.wheel_acc = 0;
            self.hide_tooltip();
            // SAFETY: timers on our own window.
            unsafe {
                if self.hover.is_some() {
                    SetTimer(Some(self.ctl), T_TOOLTIP, TOOLTIP_MS, None);
                } else {
                    let _ = KillTimer(Some(self.ctl), T_TOOLTIP);
                }
            }
            self.refresh();
        }
    }

    /// Left click: a group steps to its next member; any other slot
    /// refreshes now (R5: a widget's `click` action waits for Plan 2) and
    /// dims briefly; "+N" returns the hidden-widget menu to show.
    fn on_left_click(&mut self, x_px: i32) -> Option<MenuSpec> {
        let hit = self.hit(x_px);
        log::debug!("left click on {hit:?}");
        match hit? {
            Hit::Slot(id) if is_multi_member_group(&self.model, &id) => {
                if self.rot.advance(&id, 1, &self.model) {
                    self.refresh();
                    self.sync_tooltip();
                }
                None
            }
            Hit::Slot(id) => {
                if let Some(inst) = shown_instance(&self.model, &self.rot, &id) {
                    if let Some(s) = &self.sched {
                        s.refresh_now(&inst);
                    }
                }
                self.pressed = Some(id);
                // SAFETY: timer on our own window.
                unsafe { SetTimer(Some(self.ctl), T_PRESS, PRESS_MS, None) };
                self.refresh();
                None
            }
            Hit::Overflow => {
                let entries = overflow_entries(&self.layout, &self.model, &self.names);
                if entries.is_empty() {
                    return None;
                }
                self.hide_tooltip();
                let mut spec = MenuSpec::default();
                for (i, e) in entries.into_iter().enumerate() {
                    spec.items
                        .push(MenuItem::Command(ID_OVERFLOW_BASE + i as u32, e.label));
                    spec.overflow.push(e.instance_id);
                }
                Some(spec)
            }
        }
    }

    /// Right click: slot commands over a slot, otherwise only Quit.
    fn context_menu(&mut self, x_px: i32) -> MenuSpec {
        self.hide_tooltip();
        let mut spec = MenuSpec::default();
        if let Some(Hit::Slot(id)) = self.hit(x_px) {
            spec.target = shown_instance(&self.model, &self.rot, &id);
        }
        if spec.target.is_some() {
            spec.items.extend([
                MenuItem::Command(ID_REFRESH, "Refresh now".into()),
                MenuItem::Command(ID_HIDE, "Hide this widget".into()),
                MenuItem::Separator,
            ]);
        }
        spec.items
            .push(MenuItem::Command(ID_QUIT, "Quit Perchbar".into()));
        spec
    }

    /// Carries out a command picked from `spec`.
    fn on_menu(&mut self, cmd: u32, spec: &MenuSpec) {
        log::debug!("menu command {cmd}");
        let target = spec.target.as_deref();
        match cmd {
            ID_REFRESH => {
                if let (Some(s), Some(t)) = (&self.sched, target) {
                    s.refresh_now(t);
                }
            }
            ID_HIDE => {
                if let (Some(s), Some(t)) = (&self.sched, target) {
                    log::info!("hiding {t}");
                    s.set_hidden(t, true);
                }
            }
            ID_QUIT => self.quit_app(),
            n if n >= ID_OVERFLOW_BASE => {
                let picked = spec.overflow.get((n - ID_OVERFLOW_BASE) as usize);
                if let (Some(s), Some(id)) = (&self.sched, picked) {
                    s.refresh_now(id);
                }
            }
            _ => {}
        }
    }

    fn on_wheel(&mut self, x_px: i32, delta: i16) {
        let Some(Hit::Slot(id)) = self.hit(x_px) else {
            return;
        };
        let steps = wheel_steps(&mut self.wheel_acc, delta);
        log::debug!("wheel {delta} on {id}: {steps} step(s)");
        if steps != 0 && self.rot.advance(&id, steps, &self.model) {
            self.refresh();
            self.sync_tooltip();
        }
    }

    fn end_press(&mut self) {
        // SAFETY: our own timer.
        let _ = unsafe { KillTimer(Some(self.ctl), T_PRESS) };
        if self.pressed.take().is_some() {
            self.refresh();
        }
    }

    /// The tooltip text and screen position for the hovered slot, if any.
    fn tooltip_target(&self) -> Option<(String, i32, i32)> {
        let id = self.hover.as_deref()?;
        let slot = self.layout.items.iter().find(|p| p.slot_id == id)?;
        let text = tooltip_text(slot, &self.model, &self.updated)?;
        let strip = self.strip?;
        let scale = self.info.map_or(1.0, |i| i.scale());
        let mut r = RECT::default();
        // SAFETY: our own window; valid out pointer.
        unsafe { GetWindowRect(strip, &mut r) }.ok()?;
        let cx = r.left + layout::px(slot.x_dip + slot.width_dip / 2.0, scale);
        Some((text, cx, r.top))
    }

    fn show_tooltip(&mut self) {
        let Some((text, cx, top)) = self.tooltip_target() else {
            self.hide_tooltip();
            return;
        };
        if let Some((t, x, y)) = &self.tooltip_shown {
            if (t, *x, *y) == (&text, cx, top) {
                return;
            }
        }
        let Some(tip) = self.tooltip_window() else {
            return;
        };
        let scale = self.info.map_or(1.0, |i| i.scale());
        let mut wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let mut ti = tool_info(self.ctl);
        ti.lpszText = PWSTR(wide.as_mut_ptr());
        // SAFETY: our own tooltip on this thread; `ti` and `wide` outlive
        // the calls, and the tooltip copies the text.
        unsafe {
            let lp = LPARAM(&ti as *const TTTOOLINFOW as isize);
            SendMessageW(tip, TTM_UPDATETIPTEXTW, None, Some(lp));
            let size = SendMessageW(tip, TTM_GETBUBBLESIZE, None, Some(lp)).0;
            let (w, h) = ((size & 0xFFFF) as i32, ((size >> 16) & 0xFFFF) as i32);
            let gap = layout::px(TOOLTIP_GAP_DIP, scale);
            let x = cx - w / 2;
            let y = (top - gap - h).max(0);
            let pos = ((y as u16 as u32) << 16) | (x as u16 as u32);
            SendMessageW(tip, TTM_TRACKPOSITION, None, Some(LPARAM(pos as isize)));
            SendMessageW(tip, TTM_TRACKACTIVATE, Some(WPARAM(1)), Some(lp));
        }
        log::debug!("tooltip shown: {text:?}");
        self.tooltip_shown = Some((text, cx, top));
    }

    /// Updates a visible tooltip after the hovered slot changed content.
    fn sync_tooltip(&mut self) {
        if self.tooltip_shown.is_some() {
            self.show_tooltip();
        }
    }

    fn hide_tooltip(&mut self) {
        if self.tooltip_shown.take().is_none() {
            return;
        }
        if let Some(tip) = self.tooltip {
            let ti = tool_info(self.ctl);
            // SAFETY: our own tooltip; `ti` outlives the call.
            unsafe {
                SendMessageW(
                    tip,
                    TTM_TRACKACTIVATE,
                    Some(WPARAM(0)),
                    Some(LPARAM(&ti as *const TTTOOLINFOW as isize)),
                );
            }
        }
    }

    /// The tracking tooltip, created on first use. It is owned by the
    /// control window, so it goes away with it.
    fn tooltip_window(&mut self) -> Option<HWND> {
        // SAFETY: IsWindow accepts any handle.
        if let Some(t) = self
            .tooltip
            .filter(|t| unsafe { IsWindow(Some(*t)) }.as_bool())
        {
            return Some(t);
        }
        let scale = self.info.map_or(1.0, |i| i.scale());
        // SAFETY: creates a common-controls tooltip owned by our control
        // window, then registers one tracking tool; `ti` outlives the calls.
        unsafe {
            let tip = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                TOOLTIPS_CLASSW,
                PCWSTR::null(),
                WS_POPUP | WINDOW_STYLE(TTS_NOPREFIX | TTS_ALWAYSTIP),
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                Some(self.ctl),
                None,
                GetModuleHandleW(None).ok().map(Into::into),
                None,
            );
            let tip = match tip {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("cannot create the tooltip: {e}");
                    return None;
                }
            };
            let mut empty = [0u16];
            let mut ti = tool_info(self.ctl);
            ti.lpszText = PWSTR(empty.as_mut_ptr());
            let lp = LPARAM(&ti as *const TTTOOLINFOW as isize);
            if SendMessageW(tip, TTM_ADDTOOLW, None, Some(lp)).0 == 0 {
                log::warn!("cannot register the tooltip tool");
            }
            let max_w = layout::px(TOOLTIP_MAX_WIDTH_DIP, scale);
            SendMessageW(tip, TTM_SETMAXTIPWIDTH, None, Some(LPARAM(max_w as isize)));
            self.tooltip = Some(tip);
            Some(tip)
        }
    }

    /// "Quit Perchbar": asks the app to exit, then tears the strip down.
    fn quit_app(&mut self) {
        if self.quitting {
            return;
        }
        log::info!("quit requested from the strip");
        if let Some(f) = self.on_quit.take() {
            f();
        }
        self.quit();
    }

    fn quit(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        log::info!("strip shutting down");
        self.hide_tooltip();
        // SAFETY: our own timers, hooks and windows.
        unsafe {
            for t in [T_FALLBACK, T_DEBOUNCE, T_ROTATE, T_TOOLTIP, T_PRESS] {
                let _ = KillTimer(Some(self.ctl), t);
            }
        }
        self.unhook();
        if self.strip_alive() {
            if let Some(s) = self.strip.take() {
                // SAFETY: our own window.
                let _ = unsafe { DestroyWindow(s) };
            }
        }
        self.strip = None;
        if let Some(sched) = self.sched.take() {
            sched.shutdown();
        }
        self.uia.shutdown();
        CTL.store(0, Ordering::Release);
        TASKBAR.store(0, Ordering::Release);
        // SAFETY: our own window; the loop ends on WM_QUIT.
        unsafe {
            let _ = DestroyWindow(self.ctl);
            PostQuitMessage(0);
        }
    }
}

/// Copies the premultiplied bitmap into a DIB and hands it to the window.
fn present(strip: HWND, bmp: &Bitmap) -> windows::core::Result<()> {
    let len = (bmp.width as usize) * (bmp.height as usize) * 4;
    if bmp.pixels.len() != len {
        return Err(windows::core::Error::empty());
    }
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bmp.width,
            biHeight: -bmp.height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: the section holds `len` bytes and is released on every path.
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return Err(windows::core::Error::from_thread());
        }
        let mut bits = std::ptr::null_mut();
        let section = match CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(s) if !bits.is_null() => s,
            Ok(s) => {
                let _ = DeleteObject(s.into());
                let _ = DeleteDC(dc);
                return Err(windows::core::Error::from_thread());
            }
            Err(e) => {
                let _ = DeleteDC(dc);
                return Err(e);
            }
        };
        std::ptr::copy_nonoverlapping(bmp.pixels.as_ptr(), bits.cast::<u8>(), len);
        let old = SelectObject(dc, section.into());
        let size = SIZE {
            cx: bmp.width,
            cy: bmp.height,
        };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let r = windows::Win32::UI::WindowsAndMessaging::UpdateLayeredWindow(
            strip,
            None,
            None,
            Some(&size),
            Some(dc),
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        SelectObject(dc, old);
        let _ = DeleteObject(section.into());
        let _ = DeleteDC(dc);
        r
    }
}

/// The one tracking tool of our tooltip, identified by (owner, 1).
fn tool_info(owner: HWND) -> TTTOOLINFOW {
    TTTOOLINFOW {
        cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
        uFlags: TTF_TRACK | TTF_ABSOLUTE,
        hwnd: owner,
        uId: 1,
        ..Default::default()
    }
}

/// Shows a popup menu at the cursor and returns the picked id (0: none).
/// Uses the tray-menu pattern: our hidden top-level window owns the menu
/// and is made foreground first, so clicking elsewhere dismisses it.
fn track_menu(owner: HWND, items: &[MenuItem]) -> u32 {
    // SAFETY: a menu we create and destroy here; strings outlive AppendMenuW,
    // which copies them.
    unsafe {
        let menu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(e) => {
                log::warn!("cannot create a menu: {e}");
                return 0;
            }
        };
        for item in items {
            let r = match item {
                MenuItem::Command(id, label) => {
                    let wide: Vec<u16> = menu_label(label).encode_utf16().chain(Some(0)).collect();
                    AppendMenuW(menu, MF_STRING, *id as usize, PCWSTR(wide.as_ptr()))
                }
                MenuItem::Separator => AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()),
            };
            if let Err(e) = r {
                log::debug!("cannot add a menu item: {e}");
            }
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(owner);
        let flags =
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN;
        let picked = TrackPopupMenuEx(menu, flags.0, pt.x, pt.y, owner, None).0;
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        u32::try_from(picked).unwrap_or(0)
    }
}

/// A slot's current state for one instance, wherever it sits in the model.
fn instance_state<'a>(model: &'a StripModel, id: &str) -> Option<&'a SlotState> {
    model.slots.iter().find_map(|slot| match &slot.kind {
        SlotKind::Single {
            instance_id, state, ..
        } => (instance_id == id).then_some(state),
        SlotKind::Group { members, .. } => members
            .iter()
            .find(|m| m.instance_id == id)
            .map(|m| &m.state),
    })
}

/// Every (instance id, state) in the model.
fn instances(model: &StripModel) -> impl Iterator<Item = (&String, &PathBuf, &SlotState)> {
    model.slots.iter().flat_map(|slot| {
        let v: Vec<_> = match &slot.kind {
            SlotKind::Single {
                instance_id,
                widget_dir,
                state,
                ..
            } => vec![(instance_id, widget_dir.as_ref(), state)],
            SlotKind::Group { members, .. } => members
                .iter()
                .map(|m| (&m.instance_id, m.widget_dir.as_ref(), &m.state))
                .collect(),
        };
        v
    })
}

/// The tooltip for a placed slot: the error message for an error slot, the
/// shown frame's tooltip, and for a stale slot that tooltip plus when the
/// output last changed.
fn tooltip_text(
    slot: &PlacedSlot,
    model: &StripModel,
    updated: &HashMap<String, DateTime<Local>>,
) -> Option<String> {
    if let Some(e) = &slot.error {
        return Some(e.clone());
    }
    let fr = slot.frame.as_ref()?;
    let tip = match instance_state(model, &fr.instance_id)? {
        SlotState::Ok(o) | SlotState::Stale(o) => o.frames.get(fr.frame_index)?.tooltip.clone(),
        _ => None,
    }
    .filter(|t| !t.is_empty());
    if !slot.stale {
        return tip;
    }
    let since = match updated.get(&fr.instance_id) {
        Some(t) => format!("Not updated since {}", t.format("%H:%M")),
        None => "Not updated recently".to_string(),
    };
    Some(match tip {
        Some(t) => format!("{t}\n{since}"),
        None => since,
    })
}

/// Records `now` for each instance whose good output is new in `new`, and
/// forgets instances that are gone. Stale states keep their time.
fn note_updates(
    old: &StripModel,
    new: &StripModel,
    now: DateTime<Local>,
    updated: &mut HashMap<String, DateTime<Local>>,
) {
    let mut present: Vec<&str> = Vec::new();
    for (id, _, state) in instances(new) {
        present.push(id);
        if let SlotState::Ok(o) = state {
            let same = matches!(instance_state(old, id), Some(SlotState::Ok(p)) if p == o);
            if !same || !updated.contains_key(id) {
                updated.insert(id.clone(), now);
            }
        }
    }
    updated.retain(|k, _| present.contains(&k.as_str()));
}

/// The first text in a block, looking inside stacks top first.
fn first_text(b: &Block) -> Option<&str> {
    match b {
        Block::Text { value, .. } => Some(value),
        Block::Stack { top, bottom } => first_text(top).or_else(|| first_text(bottom)),
        _ => None,
    }
}

/// The hidden widgets as "+N" menu entries: manifest name (instance id when
/// unknown), plus ": text" when its first frame has text.
fn overflow_entries(
    layout: &StripLayout,
    model: &StripModel,
    names: &HashMap<PathBuf, String>,
) -> Vec<OverflowEntry> {
    let hidden: Vec<&str> = layout.overflow.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for slot in model
        .slots
        .iter()
        .filter(|s| hidden.contains(&s.id.as_str()))
    {
        let single = StripModel {
            slots: vec![slot.clone()],
            generation: 0,
        };
        for (id, dir, state) in instances(&single) {
            let name = names.get(dir).cloned().unwrap_or_else(|| id.clone());
            let output = match state {
                SlotState::Ok(o) | SlotState::Stale(o) => Some(o),
                SlotState::Error { last, .. } => last.as_ref(),
                SlotState::Loading => None,
            };
            let text = output
                .and_then(|o| o.frames.first())
                .and_then(|f| f.blocks.iter().find_map(first_text))
                .filter(|t| !t.is_empty());
            let label = match text {
                Some(t) => format!("{name}: {t}"),
                None => name,
            };
            out.push(OverflowEntry {
                instance_id: id.clone(),
                label,
            });
        }
    }
    out
}

/// Menu text with `&` doubled, so it is not read as a mnemonic marker.
fn menu_label(s: &str) -> String {
    s.replace('&', "&&")
}

/// The instance a slot is showing: the instance itself, or the group's
/// current member.
fn shown_instance(model: &StripModel, rot: &RotationState, slot_id: &str) -> Option<String> {
    let slot = model.slots.iter().find(|s| s.id == slot_id)?;
    match &slot.kind {
        SlotKind::Single { instance_id, .. } => Some(instance_id.clone()),
        SlotKind::Group { members, .. } if !members.is_empty() => {
            let idx = rot.group_index.get(slot_id).copied().unwrap_or(0) % members.len();
            Some(members[idx].instance_id.clone())
        }
        SlotKind::Group { .. } => None,
    }
}

fn is_multi_member_group(model: &StripModel, slot_id: &str) -> bool {
    model.slots.iter().any(|s| {
        s.id == slot_id && matches!(&s.kind, SlotKind::Group { members, .. } if members.len() > 1)
    })
}

/// Adds a wheel delta and returns whole steps to move: rolling away from
/// the user (positive) goes back, towards the user goes forward.
fn wheel_steps(acc: &mut i32, delta: i16) -> i32 {
    *acc += i32::from(delta);
    let notches = *acc / WHEEL_DELTA;
    *acc -= notches * WHEEL_DELTA;
    -notches
}

/// Halves premultiplied BGRA pixels in columns `x0..x1` of a bitmap
/// `width` pixels wide, which halves the opacity of that slot.
fn dim_columns(pixels: &mut [u8], width: i32, x0: i32, x1: i32) {
    if width <= 0 {
        return;
    }
    let (x0, x1) = (x0.clamp(0, width) as usize, x1.clamp(0, width) as usize);
    if x0 >= x1 {
        return;
    }
    for row in pixels.chunks_exact_mut(width as usize * 4) {
        for b in &mut row[x0 * 4..x1 * 4] {
            *b /= 2;
        }
    }
}

fn post_to_ctl(msg: u32) {
    post_to_ctl_with(msg, WPARAM(0));
}

fn post_to_ctl_with(msg: u32, wparam: WPARAM) {
    let ctl = CTL.load(Ordering::Acquire);
    if ctl != 0 {
        // SAFETY: posting is safe for any handle.
        unsafe {
            let _ = PostMessageW(Some(HWND(ctl as _)), msg, wparam, LPARAM(0));
        }
    }
}

/// Signed client x from a mouse message's lParam.
fn lparam_x(lparam: LPARAM) -> i32 {
    i32::from(lparam.0 as u16 as i16)
}

/// Shows a menu built under the state, then applies the pick under it again.
/// The state is free while the menu's modal loop runs.
fn run_menu(spec: Option<MenuSpec>) {
    let Some(spec) = spec.filter(|s| !s.items.is_empty()) else {
        return;
    };
    let ctl = CTL.load(Ordering::Acquire);
    if ctl == 0 {
        return;
    }
    let cmd = track_menu(HWND(ctl as _), &spec.items);
    if cmd != 0 {
        with_state(|s| s.on_menu(cmd, &spec));
    }
}

unsafe extern "system" fn win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    let taskbar = TASKBAR.load(Ordering::Acquire);
    let relevant = is_taskbar_event(id_object, hwnd.0 as isize, taskbar, |parent, child| {
        // SAFETY: IsChild accepts any handles.
        unsafe { IsChild(HWND(parent as _), HWND(child as _)) }.as_bool()
    });
    if !relevant {
        return;
    }
    log::trace!(
        "win event {event:#x} hwnd {:?} object {id_object} child {id_child}",
        hwnd.0
    );
    post_to_ctl(WM_APP_TICK);
}

/// Keeps only window-level events on the taskbar or one of its descendants.
/// Explorer also hosts File Explorer windows and the desktop, whose moves
/// say nothing about the taskbar; the cursor and caret are not windows.
fn is_taskbar_event(
    id_object: i32,
    hwnd: isize,
    taskbar: isize,
    is_child: impl Fn(isize, isize) -> bool,
) -> bool {
    taskbar != 0
        && hwnd != 0
        && (id_object == OBJID_WINDOW.0 || id_object == OBJID_CLIENT.0)
        && (hwnd == taskbar || is_child(taskbar, hwnd))
}

/// True when a WM_SETTINGCHANGE names the color scheme.
///
/// # Safety
/// `lparam` must be the message's lParam: null or a NUL-terminated string.
unsafe fn is_color_set_change(lparam: LPARAM) -> bool {
    let p = PCWSTR(lparam.0 as *const u16);
    // SAFETY: guaranteed by the caller.
    !p.is_null() && unsafe { p.to_string() }.is_ok_and(|s| s == "ImmersiveColorSet")
}

unsafe extern "system" fn ctl_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_APP_EVENTS => {
            if with_state(State::drain).is_none() {
                // Busy: try again from the loop.
                post_to_ctl(WM_APP_EVENTS);
            }
            LRESULT(0)
        }
        WM_APP_TICK | WM_DPICHANGED => {
            with_state(State::schedule_tick);
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                T_DEBOUNCE => {
                    // SAFETY: our own timer.
                    let _ = unsafe { KillTimer(Some(hwnd), T_DEBOUNCE) };
                    with_state(|s| {
                        s.debounce_pending = false;
                        s.tick(Search::IfRowChanged);
                    });
                }
                T_FALLBACK => {
                    // A full search here picks up app buttons that grew the
                    // XAML row without any window event within FALLBACK_MS.
                    with_state(|s| {
                        s.reload_theme();
                        s.tick(Search::Always);
                    });
                }
                T_ROTATE => {
                    with_state(State::rotate);
                }
                T_TOOLTIP => {
                    // SAFETY: our own timer.
                    let _ = unsafe { KillTimer(Some(hwnd), T_TOOLTIP) };
                    with_state(State::show_tooltip);
                }
                T_PRESS => {
                    with_state(State::end_press);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND if (wparam.0 & 0xFFFF) as u32 == ID_QUIT => {
            if with_state(State::quit_app).is_none() {
                post_to_ctl_with(WM_COMMAND, wparam);
            }
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            // SAFETY: lParam of WM_SETTINGCHANGE is null or a string.
            let color = unsafe { is_color_set_change(lparam) };
            with_state(|s| {
                if color {
                    s.reload_theme();
                }
                s.schedule_tick();
            });
            LRESULT(0)
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Acquire) => {
            log::info!("taskbar recreated");
            with_state(|s| s.tick(Search::Always));
            LRESULT(0)
        }
        WM_APP_BUTTONS => {
            if with_state(State::on_buttons).is_none() {
                // Busy: try again from the loop.
                post_to_ctl(WM_APP_BUTTONS);
            }
            LRESULT(0)
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe extern "system" fn strip_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE => {
            let x = lparam_x(lparam);
            with_state(|s| s.on_mouse_move(x));
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            with_state(|s| {
                s.tracking_mouse = false;
                s.set_hover(None);
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = lparam_x(lparam);
            run_menu(with_state(|s| s.on_left_click(x)).flatten());
            LRESULT(0)
        }
        // Handled here so DefWindowProc does not turn it into WM_CONTEXTMENU,
        // which a child window forwards to Explorer with SendMessage.
        WM_RBUTTONUP => {
            let x = lparam_x(lparam);
            run_menu(with_state(|s| s.context_menu(x)));
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            // Screen coordinates; the high word of wParam is the delta.
            let mut pt = POINT {
                x: lparam_x(lparam),
                y: i32::from((lparam.0 >> 16) as u16 as i16),
            };
            let delta = (wparam.0 >> 16) as u16 as i16;
            // SAFETY: our own window; valid pointer.
            if unsafe { ScreenToClient(hwnd, &mut pt) }.as_bool() {
                with_state(|s| s.on_wheel(pt.x, delta));
            }
            LRESULT(0)
        }
        // DefWindowProc forwards these to the parent (Explorer) with
        // SendMessage; the strip must never block on Explorer.
        WM_MOUSEHWHEEL => LRESULT(0),
        WM_SETCURSOR => {
            // SAFETY: a shared system cursor.
            unsafe {
                if let Ok(c) = LoadCursorW(None, IDC_ARROW) {
                    SetCursor(Some(c));
                }
            }
            LRESULT(1)
        }
        WM_DPICHANGED_AFTERPARENT => {
            post_to_ctl(WM_APP_TICK);
            LRESULT(0)
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{Block, Color, Frame, Output, TextSize};
    use crate::strip::layout::{FrameRef, PlacedSlot};
    use crate::strip::model::{Member, Slot, SlotKind, SlotState};
    use chrono::TimeZone;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn text(s: &str) -> Block {
        Block::Text {
            value: s.into(),
            color: Color::Default,
            size: TextSize::Normal,
        }
    }

    fn out(frames: &[(&str, Option<&str>)]) -> Output {
        Output {
            frames: frames
                .iter()
                .map(|(t, tip)| Frame {
                    blocks: vec![text(t)],
                    tooltip: tip.map(String::from),
                })
                .collect(),
            rotate: None,
            click: None,
        }
    }

    fn dir(name: &str) -> Arc<PathBuf> {
        Arc::new(PathBuf::from(name))
    }

    fn single(id: &str, widget: &str, state: SlotState) -> Slot {
        Slot {
            id: id.into(),
            kind: SlotKind::Single {
                instance_id: id.into(),
                widget_dir: dir(widget),
                state,
                max_width: None,
            },
        }
    }

    fn group(id: &str, members: Vec<(&str, SlotState)>) -> Slot {
        Slot {
            id: id.into(),
            kind: SlotKind::Group {
                interval: 5,
                members: members
                    .into_iter()
                    .map(|(m, state)| Member {
                        instance_id: m.into(),
                        widget_dir: dir(m),
                        state,
                    })
                    .collect(),
            },
        }
    }

    fn model(slots: Vec<Slot>) -> StripModel {
        StripModel {
            slots,
            generation: 1,
        }
    }

    fn placed(id: &str, instance: Option<(&str, usize)>) -> PlacedSlot {
        PlacedSlot {
            slot_id: id.into(),
            x_dip: 0.0,
            width_dip: 50.0,
            frame: instance.map(|(i, f)| FrameRef {
                instance_id: i.into(),
                widget_dir: dir(i),
                frame_index: f,
            }),
            rotating: false,
            stale: false,
            error: None,
        }
    }

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 11, 12, h, m, 0).unwrap()
    }

    #[test]
    fn tooltip_shows_the_shown_frames_tooltip() {
        let m = model(vec![single(
            "a",
            "w",
            SlotState::Ok(out(&[("x", Some("first")), ("y", Some("second"))])),
        )]);
        let none = HashMap::new();
        assert_eq!(
            tooltip_text(&placed("a", Some(("a", 1))), &m, &none).as_deref(),
            Some("second")
        );
        let m = model(vec![single("a", "w", SlotState::Ok(out(&[("x", None)])))]);
        assert_eq!(tooltip_text(&placed("a", Some(("a", 0))), &m, &none), None);
    }

    #[test]
    fn tooltip_shows_the_error_message() {
        let m = model(vec![single(
            "a",
            "w",
            SlotState::Error {
                message: "boom".into(),
                last: None,
            },
        )]);
        let mut p = placed("a", None);
        p.error = Some("boom".into());
        assert_eq!(
            tooltip_text(&p, &m, &HashMap::new()).as_deref(),
            Some("boom")
        );
    }

    #[test]
    fn tooltip_of_a_stale_slot_says_since_when() {
        let m = model(vec![single(
            "a",
            "w",
            SlotState::Stale(out(&[("x", Some("BTC $67,120"))])),
        )]);
        let mut p = placed("a", Some(("a", 0)));
        p.stale = true;
        let mut seen = HashMap::new();
        seen.insert("a".to_string(), at(14, 5));
        assert_eq!(
            tooltip_text(&p, &m, &seen).as_deref(),
            Some("BTC $67,120\nNot updated since 14:05")
        );
        let m = model(vec![single(
            "a",
            "w",
            SlotState::Stale(out(&[("x", None)])),
        )]);
        assert_eq!(
            tooltip_text(&p, &m, &seen).as_deref(),
            Some("Not updated since 14:05")
        );
        assert_eq!(
            tooltip_text(&p, &m, &HashMap::new()).as_deref(),
            Some("Not updated recently")
        );
    }

    #[test]
    fn updates_are_timed_when_output_changes() {
        let ok = |t: &str| SlotState::Ok(out(&[(t, None)]));
        let m1 = model(vec![
            single("a", "w", ok("1")),
            single("b", "w", SlotState::Loading),
        ]);
        let mut seen = HashMap::new();
        note_updates(&StripModel::default(), &m1, at(10, 0), &mut seen);
        assert_eq!(seen.get("a"), Some(&at(10, 0)));
        assert!(!seen.contains_key("b"));

        // Same output again: the time stays; going stale keeps it too.
        let m2 = model(vec![
            single("a", "w", ok("1")),
            group("g", vec![("b", ok("2"))]),
        ]);
        note_updates(&m1, &m2, at(10, 5), &mut seen);
        assert_eq!(seen.get("a"), Some(&at(10, 0)));
        assert_eq!(seen.get("b"), Some(&at(10, 5)));
        let m3 = model(vec![
            single("a", "w", SlotState::Stale(out(&[("1", None)]))),
            group("g", vec![("b", ok("3"))]),
        ]);
        note_updates(&m2, &m3, at(10, 9), &mut seen);
        assert_eq!(seen.get("a"), Some(&at(10, 0)));
        assert_eq!(seen.get("b"), Some(&at(10, 9)));

        // Gone instances are forgotten.
        note_updates(&m3, &model(vec![]), at(11, 0), &mut seen);
        assert!(seen.is_empty());
    }

    #[test]
    fn overflow_entries_name_hidden_widgets_with_their_text() {
        let m = model(vec![
            single("s1", "clock", SlotState::Ok(out(&[("12:00", None)]))),
            single(
                "s2",
                "btc",
                SlotState::Ok(out(&[("$67,120", None), ("later", None)])),
            ),
            single("s3", "unnamed", SlotState::Loading),
            group(
                "g",
                vec![
                    ("m1", SlotState::Stale(out(&[("A & B", None)]))),
                    ("m2", SlotState::Loading),
                ],
            ),
        ]);
        let lay = StripLayout {
            overflow: vec!["s2".into(), "s3".into(), "g".into()],
            ..StripLayout::default()
        };
        let names: HashMap<PathBuf, String> = [
            ("btc", "Bitcoin price"),
            ("clock", "World clock"),
            ("m1", "Pairs"),
            ("m2", "Weather"),
        ]
        .into_iter()
        .map(|(d, n)| (PathBuf::from(d), n.to_string()))
        .collect();
        let entries = overflow_entries(&lay, &m, &names);
        let got: Vec<(&str, &str)> = entries
            .iter()
            .map(|e| (e.instance_id.as_str(), e.label.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("s2", "Bitcoin price: $67,120"),
                ("s3", "s3"),
                ("m1", "Pairs: A & B"),
                ("m2", "Weather"),
            ]
        );
    }

    #[test]
    fn menu_labels_escape_ampersands() {
        assert_eq!(menu_label("Pairs: A & B"), "Pairs: A && B");
        assert_eq!(menu_label("plain"), "plain");
    }

    #[test]
    fn shown_instance_follows_the_group_index() {
        let m = model(vec![
            single("s", "w", SlotState::Loading),
            group(
                "g",
                vec![("a", SlotState::Loading), ("b", SlotState::Loading)],
            ),
        ]);
        let mut rot = RotationState::default();
        assert_eq!(shown_instance(&m, &rot, "s").as_deref(), Some("s"));
        assert_eq!(shown_instance(&m, &rot, "g").as_deref(), Some("a"));
        rot.group_index.insert("g".into(), 3);
        assert_eq!(shown_instance(&m, &rot, "g").as_deref(), Some("b"));
        assert_eq!(shown_instance(&m, &rot, "missing"), None);
        assert!(is_multi_member_group(&m, "g"));
        assert!(!is_multi_member_group(&m, "s"));
    }

    #[test]
    fn wheel_notches_step_backwards_when_rolled_up() {
        let mut acc = 0;
        assert_eq!(wheel_steps(&mut acc, 120), -1);
        assert_eq!(wheel_steps(&mut acc, -240), 2);
        // Touchpads send small deltas that add up.
        assert_eq!(wheel_steps(&mut acc, -60), 0);
        assert_eq!(wheel_steps(&mut acc, -60), 1);
        assert_eq!(acc, 0);
        assert_eq!(wheel_steps(&mut acc, 30), 0);
        assert_eq!(acc, 30);
    }

    #[test]
    fn dimming_halves_only_the_given_columns() {
        // 3 x 2 premultiplied BGRA, all 200.
        let mut px = vec![200u8; 3 * 2 * 4];
        dim_columns(&mut px, 3, 1, 2);
        for row in 0..2 {
            for col in 0..3 {
                let i = (row * 3 + col) * 4;
                let want = if col == 1 { 100 } else { 200 };
                assert_eq!(&px[i..i + 4], &[want; 4], "row {row} col {col}");
            }
        }
        // Out-of-range columns are clamped.
        dim_columns(&mut px, 3, -5, 99);
        assert_eq!(px[0], 100);
    }

    const TASKBAR_HWND: isize = 100;
    const BRIDGE: isize = 101;
    const FOLDER_WINDOW: isize = 500;

    fn is_child(parent: isize, child: isize) -> bool {
        parent == TASKBAR_HWND && child == BRIDGE
    }

    #[test]
    fn keeps_window_events_on_the_taskbar_and_its_children() {
        let ev = |obj: i32, hwnd: isize| is_taskbar_event(obj, hwnd, TASKBAR_HWND, is_child);
        assert!(ev(OBJID_WINDOW.0, TASKBAR_HWND));
        assert!(ev(OBJID_WINDOW.0, BRIDGE));
        assert!(ev(OBJID_CLIENT.0, BRIDGE));
    }

    #[test]
    fn drops_other_explorer_windows_and_non_window_objects() {
        let ev = |obj: i32, hwnd: isize| is_taskbar_event(obj, hwnd, TASKBAR_HWND, is_child);
        // A File Explorer window being dragged.
        assert!(!ev(OBJID_WINDOW.0, FOLDER_WINDOW));
        // Cursor, caret and accessible child objects.
        assert!(!ev(-9, TASKBAR_HWND));
        assert!(!ev(-8, TASKBAR_HWND));
        assert!(!ev(5, BRIDGE));
        assert!(!ev(OBJID_WINDOW.0, 0));
        // No taskbar known yet.
        assert!(!is_taskbar_event(OBJID_WINDOW.0, BRIDGE, 0, is_child));
    }
}
