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
use model::{SlotKind, SlotState, StripModel};

pub enum StripEvent {
    Model(StripModel),
    Quit,
}

/// Starts the strip thread. Send events on the channel's sender, then call
/// [`wake`] so the thread notices them. `names` maps widget folders to their
/// manifest names (for the "+N" menu); `on_quit` runs on the strip thread
/// when the user picks "Quit Perchbar", before the strip tears down.
pub fn spawn(
    rx: Receiver<StripEvent>,
    sched: SchedulerHandle,
    names: HashMap<PathBuf, String>,
    on_quit: impl FnOnce() + Send + 'static,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("perchbar-strip".into())
        .spawn(move || window::run(rx, sched, names, Box::new(on_quit)))
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
    /// The slot dimmed briefly after a click.
    pub pressed: Option<String>,
    pub dark: bool,
    pub dpi: u32,
    /// Taskbar height in pixels.
    pub height: i32,
    /// Available width in pixels.
    pub available: i32,
}

/// The rotation timer period: the smallest group or frame interval among
/// slots that rotate (at least 1 s), or `None` when nothing rotates.
pub(crate) fn rotation_secs(model: &StripModel) -> Option<u32> {
    fn frames_secs(state: &SlotState) -> Option<u32> {
        let out = match state {
            SlotState::Ok(o) | SlotState::Stale(o) => Some(o),
            SlotState::Error { last, .. } => last.as_ref(),
            SlotState::Loading => None,
        }?;
        (out.frames.len() > 1).then(|| out.rotate.unwrap_or(layout::DEFAULT_ROTATE_SECS))
    }
    let mut secs: Vec<u32> = Vec::new();
    for slot in &model.slots {
        match &slot.kind {
            SlotKind::Single { state, .. } => secs.extend(frames_secs(state)),
            SlotKind::Group { interval, members } => {
                if members.len() > 1 {
                    secs.push(*interval);
                }
                secs.extend(members.iter().filter_map(|m| frames_secs(&m.state)));
            }
        }
    }
    secs.into_iter().min().map(|s| s.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{Block, Color, Frame, Output, TextSize};
    use model::{Member, Slot};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn out(n: usize, rotate: Option<u32>) -> Output {
        let frame = Frame {
            blocks: vec![Block::Text {
                value: "x".into(),
                color: Color::Default,
                size: TextSize::Normal,
            }],
            tooltip: None,
        };
        Output {
            frames: vec![frame; n],
            rotate,
            click: None,
        }
    }

    fn single(state: SlotState) -> Slot {
        Slot {
            id: "s".into(),
            kind: SlotKind::Single {
                instance_id: "s".into(),
                widget_dir: Arc::new(PathBuf::from("w")),
                state,
                max_width: None,
            },
        }
    }

    fn member(id: &str, state: SlotState) -> Member {
        Member {
            instance_id: id.into(),
            widget_dir: Arc::new(PathBuf::from("w")),
            state,
        }
    }

    fn model(slots: Vec<Slot>) -> StripModel {
        StripModel {
            slots,
            generation: 1,
        }
    }

    #[test]
    fn nothing_rotates_without_frames_or_groups() {
        let m = model(vec![
            single(SlotState::Ok(out(1, Some(2)))),
            single(SlotState::Loading),
        ]);
        assert_eq!(rotation_secs(&m), None);
    }

    #[test]
    fn smallest_interval_wins() {
        let g = Slot {
            id: "g".into(),
            kind: SlotKind::Group {
                interval: 8,
                members: vec![
                    member("a", SlotState::Ok(out(1, None))),
                    member("b", SlotState::Stale(out(3, Some(4)))),
                ],
            },
        };
        let m = model(vec![single(SlotState::Ok(out(2, None))), g]);
        assert_eq!(rotation_secs(&m), Some(4));
    }

    #[test]
    fn default_frame_period_and_floor_of_one() {
        assert_eq!(
            rotation_secs(&model(vec![single(SlotState::Ok(out(2, None)))])),
            Some(layout::DEFAULT_ROTATE_SECS)
        );
        assert_eq!(
            rotation_secs(&model(vec![single(SlotState::Ok(out(2, Some(0))))])),
            Some(1)
        );
    }
}
