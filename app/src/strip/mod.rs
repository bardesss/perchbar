//! The taskbar strip: model, layout, rendering and the window that shows it.

pub mod icons;
pub mod image;
pub mod layout;
pub mod model;
pub mod placement;
pub mod render;
pub mod theme;
mod uia;
mod window;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;

use crate::scheduler::SchedulerHandle;
use model::StripModel;

pub enum StripEvent {
    Model(StripModel),
    Quit,
}

/// Starts the strip thread. Send events on the channel's sender, then call
/// [`wake`] so the thread notices them. `names` maps widget folders to their
/// manifest names (for the "+N" menu); `on_quit` runs on the strip thread
/// when the user picks "Quit Perchbar", before the strip tears down.
/// `on_open_settings` runs on the strip thread to open the settings window,
/// focused on an instance or not; it must return at once, since the strip
/// never waits on anything.
pub fn spawn(
    rx: Receiver<StripEvent>,
    sched: SchedulerHandle,
    names: HashMap<PathBuf, String>,
    on_quit: impl FnOnce() + Send + 'static,
    on_open_settings: impl Fn(Option<String>) + Send + Sync + 'static,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("perchbar-strip".into())
        .spawn(move || {
            window::run(
                rx,
                sched,
                names,
                Box::new(on_quit),
                Box::new(on_open_settings),
            )
        })
        .expect("cannot spawn the strip thread")
}

/// Wakes the strip thread to read its channel. Never blocks, so it is safe
/// to call from the scheduler's publish callback.
pub fn wake() {
    window::wake();
}

/// Everything a drawn frame depends on; the strip redraws only when it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderKey {
    pub generation: u64,
    /// Sorted (slot id, shown member).
    pub groups: Vec<(String, usize)>,
    /// Sorted (instance id, shown frame).
    pub frames: Vec<(String, usize)>,
    pub hover: Option<String>,
    pub dark: bool,
    pub dpi: u32,
    /// Taskbar height in pixels.
    pub height: i32,
    /// Available width in pixels.
    pub available: i32,
}
