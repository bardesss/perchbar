//! The strip window: a per-pixel-alpha layered child of `Shell_TrayWnd`.
//!
//! Everything runs on one thread with a Win32 message loop. A hidden
//! top-level control window owns the timers and receives broadcasts
//! (`TaskbarCreated`, `WM_SETTINGCHANGE`) that a taskbar child would not.
//! State lives in a thread-local and is only touched from window procedures
//! dispatched by the loop; nested messages (sent while the state is in use)
//! never touch it, and any that would are reposted or dropped.

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, GetWindow,
    GetWindowThreadProcessId, IsWindow, IsWindowVisible, KillTimer, LoadCursorW, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetTimer, SetWindowPos, ShowWindow,
    TranslateMessage, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_LOCATIONCHANGE,
    GW_CHILD, HWND_TOP, IDC_ARROW, MA_NOACTIVATE, MSG, OBJID_CARET, OBJID_CURSOR, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, SW_HIDE, ULW_ALPHA, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP,
    WM_DPICHANGED, WM_DPICHANGED_AFTERPARENT, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_SETTINGCHANGE,
    WM_TIMER, WNDCLASSW, WS_CHILD, WS_CLIPSIBLINGS, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::scheduler::SchedulerHandle;
use crate::strip::layout::{self, Metrics, RotationState, StripLayout};
use crate::strip::model::StripModel;
use crate::strip::placement::{self, TaskbarInfo};
use crate::strip::render::{Bitmap, Renderer};
use crate::strip::theme::{self, Palette};
use crate::strip::{rotation_secs, RenderKey, StripEvent};

/// Drain the event channel.
const WM_APP_EVENTS: u32 = WM_APP;
/// Something on the taskbar may have moved: run a debounced placement tick.
const WM_APP_TICK: u32 = WM_APP + 1;
/// Not exported by the `windows` features in use.
const WM_MOUSELEAVE: u32 = 0x02A3;

const T_FALLBACK: usize = 1;
const T_DEBOUNCE: usize = 2;
const T_ROTATE: usize = 3;
const FALLBACK_MS: u32 = 5000;
const DEBOUNCE_MS: u32 = 250;
/// A WM_TIMER can fire a little early; this keeps such a tick from missing
/// a rotation that is due.
const ROTATE_SLACK: Duration = Duration::from_millis(100);

const CTL_CLASS: PCWSTR = w!("PerchbarStripControl");
const STRIP_CLASS: PCWSTR = w!("PerchbarStrip");

/// The control window, for [`wake`] from other threads. 0 until created.
static CTL: AtomicIsize = AtomicIsize::new(0);
/// The registered `TaskbarCreated` message.
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

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
pub(super) fn run(rx: Receiver<StripEvent>, sched: SchedulerHandle) {
    // SAFETY: thread-level setup calls with constant arguments. MTA keeps UI
    // Automation calls from depending on this thread's message pumping.
    let com = unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_MULTITHREADED)
    };
    match setup(rx, sched) {
        Ok(()) => message_loop(),
        Err(e) => log::error!("cannot start the taskbar strip: {e}"),
    }
    CTL.store(0, Ordering::Release);
    // COM objects (UI Automation, Direct2D) go before COM does.
    placement::release_uia();
    STATE.with(|s| s.borrow_mut().take());
    if com.is_ok() {
        // SAFETY: pairs the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

fn setup(rx: Receiver<StripEvent>, sched: SchedulerHandle) -> windows::core::Result<()> {
    // SAFETY: class registration and window creation with valid arguments.
    let ctl = unsafe {
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
        });
    });
    CTL.store(ctl.0 as isize, Ordering::Release);
    // SAFETY: timer on our own window.
    unsafe { SetTimer(Some(ctl), T_FALLBACK, FALLBACK_MS, None) };
    with_state(|s| {
        s.placement_tick();
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
            self.model = m;
            self.rot.tick(&self.model, Instant::now() + ROTATE_SLACK);
            self.update_rotation_timer();
            self.refresh();
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

    /// Re-reads the taskbar, recreating the strip or hooks when Explorer
    /// restarted, then repositions and redraws as needed.
    fn placement_tick(&mut self) {
        self.tick(true);
    }

    /// The periodic fallback: the same, but the UI Automation button search
    /// only runs when the taskbar row changed or the strip was recreated.
    fn fallback_tick(&mut self) {
        self.tick(false);
    }

    fn tick(&mut self, full: bool) {
        let Some(taskbar) = placement::taskbar_hwnd() else {
            self.info = None;
            return;
        };
        let mut full = full;
        if self.taskbar != Some(taskbar) || !self.strip_alive() {
            self.recreate(taskbar);
            full = true;
        }
        let mut pid = 0;
        // SAFETY: valid out pointer.
        unsafe { GetWindowThreadProcessId(taskbar, Some(&mut pid)) };
        if pid != 0 && pid != self.explorer_pid {
            self.rehook(pid);
        }
        self.info = if full {
            placement::query()
        } else {
            placement::query_reusing(self.info.as_ref())
        };
        log::debug!("taskbar: {:?}", self.info);
        self.refresh();
    }

    fn recreate(&mut self, taskbar: HWND) {
        if self.strip_alive() {
            if let Some(old) = self.strip.take() {
                // SAFETY: our own window.
                let _ = unsafe { DestroyWindow(old) };
            }
        }
        // SAFETY: creates a child of the taskbar with our registered class.
        let created = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
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
                log::warn!("cannot create the strip window: {e}");
                self.strip = None;
            }
        }
        self.taskbar = Some(taskbar);
        self.key = None;
        self.pos = None;
        self.hover = None;
        self.tracking_mouse = false;
        self.rot.paused_slot = None;
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
                Ok(bmp) => {
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
        let scale = self.info.map_or(1.0, |i| i.scale());
        let x = x_px as f32 / scale;
        let hit = self
            .layout
            .items
            .iter()
            .find(|p| x >= p.x_dip && x < p.x_dip + p.width_dip)
            .map(|p| p.slot_id.clone());
        self.set_hover(hit);
    }

    fn set_hover(&mut self, hover: Option<String>) {
        if hover != self.hover {
            self.rot.paused_slot.clone_from(&hover);
            self.hover = hover;
            self.refresh();
        }
    }

    fn quit(&mut self) {
        log::info!("strip shutting down");
        // SAFETY: our own timers, hooks and windows.
        unsafe {
            for t in [T_FALLBACK, T_DEBOUNCE, T_ROTATE] {
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
        CTL.store(0, Ordering::Release);
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

fn post_to_ctl(msg: u32) {
    let ctl = CTL.load(Ordering::Acquire);
    if ctl != 0 {
        // SAFETY: posting is safe for any handle.
        unsafe {
            let _ = PostMessageW(Some(HWND(ctl as _)), msg, WPARAM(0), LPARAM(0));
        }
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
    // The cursor and caret move constantly and say nothing about layout.
    if id_object == OBJID_CURSOR.0 || id_object == OBJID_CARET.0 {
        return;
    }
    log::trace!(
        "win event {event:#x} hwnd {:?} object {id_object} child {id_child}",
        hwnd.0
    );
    post_to_ctl(WM_APP_TICK);
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
                        s.placement_tick();
                    });
                }
                T_FALLBACK => {
                    with_state(|s| {
                        s.reload_theme();
                        s.fallback_tick();
                    });
                }
                T_ROTATE => {
                    with_state(State::rotate);
                }
                _ => {}
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
            with_state(State::placement_tick);
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
            // Signed low word: x in client pixels.
            let x = i32::from(lparam.0 as u16 as i16);
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
        WM_DPICHANGED_AFTERPARENT => {
            post_to_ctl(WM_APP_TICK);
            LRESULT(0)
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
