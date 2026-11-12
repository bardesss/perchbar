//! What the taskbar strip shows: one slot per top-level instance or group.

use std::path::PathBuf;
use std::sync::Arc;

use crate::display::Output;

/// Where one widget instance is in its refresh cycle.
///
/// A failed run never replaces good output with an error: when a previous
/// good `Output` exists the state becomes `Stale(previous)` (render dimmed),
/// and `Error` is used only when there is nothing to show yet, so
/// `Error { last }` is always `None` in practice. The `last` field is kept so
/// renderers can treat both shapes uniformly.
#[derive(Debug, Clone, PartialEq)]
pub enum SlotState {
    Loading,
    Ok(Output),
    Stale(Output),
    Error {
        message: String,
        last: Option<Output>,
    },
}

/// A group member. `widget_dir` lets the renderer resolve custom icon files.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub instance_id: String,
    pub widget_dir: Arc<PathBuf>,
    pub state: SlotState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SlotKind {
    Single {
        instance_id: String,
        widget_dir: Arc<PathBuf>,
        state: SlotState,
        max_width: Option<u32>,
    },
    Group {
        interval: u32,
        members: Vec<Member>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    /// Instance id or group id.
    pub id: String,
    pub kind: SlotKind,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StripModel {
    /// Left to right.
    pub slots: Vec<Slot>,
    /// Bumped each time `slots` changes.
    pub generation: u64,
}
