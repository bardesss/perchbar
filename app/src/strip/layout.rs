//! Pure strip layout: where each slot goes, how wide it is, which frame or
//! group member is showing, which slots overflow into "+N", and where the
//! "+N" flyout and its rows go.
//! No Win32 calls here; text measurement is injected through [`Measure`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::RECT;

use crate::display::{Block, Frame, Output, TextSize};
use crate::strip::model::{Slot, SlotKind, SlotState, StripModel};

pub const SLOT_GAP_DIP: f32 = 12.0;
pub const SLOT_PAD_DIP: f32 = 4.0;
pub const BLOCK_GAP_DIP: f32 = 4.0;
pub const ICON_DIP: f32 = 16.0;
pub const RING_DIP: f32 = 16.0;
pub const DEFAULT_BAR_WIDTH_DIP: f32 = 32.0;
pub const BAR_HEIGHT_DIP: f32 = 4.0;
pub const SPARK_WIDTH_DIP: f32 = 32.0;
pub const SPARK_HEIGHT_DIP: f32 = 14.0;
pub const NORMAL_TEXT_DIP: f32 = 12.0;
pub const SMALL_TEXT_DIP: f32 = 9.0;
pub const PLUS_N_PAD_DIP: f32 = 8.0;
/// Minimum gap between the strip and the app buttons; applied by the caller
/// when it computes `Metrics::available_dip`.
pub const MIN_APP_GAP_DIP: f32 = 8.0;
/// Width of a slot with no output yet (loading dot, or error icon plus padding).
pub const EMPTY_SLOT_DIP: f32 = 24.0;
/// Frame rotation period when a widget does not set `rotate`.
pub const DEFAULT_ROTATE_SECS: u32 = 6;

/// Inner padding of the "+N" flyout around its rows.
pub const FLYOUT_PAD_DIP: f32 = 8.0;
/// Space between two flyout rows.
pub const FLYOUT_SPACING_DIP: f32 = 4.0;
/// The flyout is never narrower than this.
pub const FLYOUT_MIN_WIDTH_DIP: f32 = 160.0;
/// Rows shown at once; more scroll with the wheel.
pub const FLYOUT_MAX_ROWS: usize = 12;
/// Height of one flyout row.
pub const FLYOUT_ROW_DIP: f32 = 40.0;
/// Space between the flyout and the "+N" item.
pub const FLYOUT_GAP_DIP: f32 = 8.0;

/// DIPs to device pixels.
pub fn px(dip: f32, scale: f32) -> i32 {
    (dip * scale).round() as i32
}

/// Text measurement in DIPs. Task 8 implements it with DirectWrite.
pub trait Measure {
    fn text_width(&self, text: &str, size: TextSize) -> f32;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    /// Taskbar height in DIPs.
    pub height_dip: f32,
    /// dpi / 96.
    pub scale: f32,
    /// Free width in DIPs.
    pub available_dip: f32,
}

/// Which instance and which of its frames to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameRef {
    pub instance_id: String,
    pub widget_dir: Arc<PathBuf>,
    pub frame_index: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlacedSlot {
    pub slot_id: String,
    pub x_dip: f32,
    pub width_dip: f32,
    /// `None` while loading or when an error has nothing to show.
    pub frame: Option<FrameRef>,
    pub rotating: bool,
    pub stale: bool,
    pub error: Option<String>,
}

/// The trailing "+N" item that stands for hidden slots.
#[derive(Debug, Clone, PartialEq)]
pub struct OverflowItem {
    pub x_dip: f32,
    pub width_dip: f32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct StripLayout {
    pub width_dip: f32,
    pub items: Vec<PlacedSlot>,
    /// Slot ids hidden for lack of space.
    pub overflow: Vec<String>,
    /// Present exactly when `overflow` is not empty.
    pub overflow_item: Option<OverflowItem>,
}

/// Rotation positions plus the timers that drive them.
#[derive(Debug, Clone, Default)]
pub struct RotationState {
    /// Slot id to shown member index.
    pub group_index: HashMap<String, usize>,
    /// Instance id to shown frame index.
    pub frame_index: HashMap<String, usize>,
    /// The hovered slot, which does not rotate.
    pub paused_slot: Option<String>,
    group_at: HashMap<String, Instant>,
    frame_at: HashMap<String, Instant>,
}

/// Returns true when `secs` have passed since the key's last advance, and
/// restarts the timer. A paused key never fires and keeps its timer fresh so
/// it does not jump the moment the pause ends.
fn due(
    at: &mut HashMap<String, Instant>,
    key: &str,
    now: Instant,
    secs: u32,
    paused: bool,
) -> bool {
    if paused {
        at.insert(key.to_string(), now);
        return false;
    }
    match at.get(key) {
        None => {
            at.insert(key.to_string(), now);
            false
        }
        Some(&t) => {
            if now.saturating_duration_since(t) >= Duration::from_secs(u64::from(secs)) {
                at.insert(key.to_string(), now);
                true
            } else {
                false
            }
        }
    }
}

impl RotationState {
    pub fn tick(&mut self, model: &StripModel, now: Instant) {
        for slot in &model.slots {
            let paused = self.paused_slot.as_deref() == Some(slot.id.as_str());
            match &slot.kind {
                SlotKind::Single {
                    instance_id, state, ..
                } => {
                    self.tick_frames(instance_id, state_output(state), now, paused);
                }
                SlotKind::Group { interval, members } => {
                    let n = members.len();
                    if n == 0 {
                        self.group_index.remove(&slot.id);
                        continue;
                    }
                    let mut idx = self.group_index.get(&slot.id).copied().unwrap_or(0) % n;
                    if n > 1
                        && due(
                            &mut self.group_at,
                            &slot.id,
                            now,
                            (*interval).max(1),
                            paused,
                        )
                    {
                        idx = (idx + 1) % n;
                    }
                    self.group_index.insert(slot.id.clone(), idx);
                    let m = &members[idx];
                    self.tick_frames(&m.instance_id, state_output(&m.state), now, paused);
                }
            }
        }
        self.prune(model);
    }

    /// Steps a slot by `delta` (negative: backwards): a group to another
    /// member, otherwise the shown instance to another frame. Returns false
    /// when there is nothing to step to.
    pub fn advance(&mut self, slot_id: &str, delta: i32, model: &StripModel) -> bool {
        if delta == 0 {
            return false;
        }
        let Some(slot) = model.slots.iter().find(|s| s.id == slot_id) else {
            return false;
        };
        let step =
            |idx: usize, n: usize| (idx as i64 + i64::from(delta)).rem_euclid(n as i64) as usize;
        let (id, out) = match &slot.kind {
            SlotKind::Single {
                instance_id, state, ..
            } => (instance_id, state_output(state)),
            SlotKind::Group { members, .. } if members.len() > 1 => {
                let n = members.len();
                let idx = self.group_index.get(slot_id).copied().unwrap_or(0) % n;
                self.group_index.insert(slot_id.to_string(), step(idx, n));
                return true;
            }
            SlotKind::Group { members, .. } => match members.first() {
                Some(m) => (&m.instance_id, state_output(&m.state)),
                None => return false,
            },
        };
        let n = out.map_or(0, |o| o.frames.len());
        if n < 2 {
            return false;
        }
        let idx = self.frame_index.get(id).copied().unwrap_or(0) % n;
        self.frame_index.insert(id.clone(), step(idx, n));
        true
    }

    /// When the next rotation is due: the earliest of every rotating key's
    /// last advance plus its period, as of the last `tick`. `None` when
    /// nothing rotates. Arming one timer for this instant lets an 8 s group
    /// next to a 6 s clock rotate at 8 s, not at the clock's 12 s tick.
    pub fn next_due(&self, model: &StripModel) -> Option<Instant> {
        let mut due: Option<Instant> = None;
        let mut note = |at: Option<&Instant>, secs: u32| {
            if let Some(t) = at {
                let d = *t + Duration::from_secs(u64::from(secs.max(1)));
                due = Some(due.map_or(d, |cur| cur.min(d)));
            }
        };
        let frames_secs = |out: Option<&Output>| {
            out.filter(|o| o.frames.len() > 1)
                .map(|o| o.rotate.unwrap_or(DEFAULT_ROTATE_SECS))
        };
        for slot in &model.slots {
            match &slot.kind {
                SlotKind::Single {
                    instance_id, state, ..
                } => {
                    if let Some(secs) = frames_secs(state_output(state)) {
                        note(self.frame_at.get(instance_id), secs);
                    }
                }
                SlotKind::Group { interval, members } if !members.is_empty() => {
                    if members.len() > 1 {
                        note(self.group_at.get(&slot.id), *interval);
                    }
                    let idx = self.group_index.get(&slot.id).copied().unwrap_or(0) % members.len();
                    let m = &members[idx];
                    if let Some(secs) = frames_secs(state_output(&m.state)) {
                        note(self.frame_at.get(&m.instance_id), secs);
                    }
                }
                SlotKind::Group { .. } => {}
            }
        }
        due
    }

    fn tick_frames(&mut self, id: &str, out: Option<&Output>, now: Instant, paused: bool) {
        let n = out.map_or(0, |o| o.frames.len());
        if n < 2 {
            self.frame_index.remove(id);
            return;
        }
        let secs = out
            .and_then(|o| o.rotate)
            .unwrap_or(DEFAULT_ROTATE_SECS)
            .max(1);
        let mut idx = self.frame_index.get(id).copied().unwrap_or(0) % n;
        if due(&mut self.frame_at, id, now, secs, paused) {
            idx = (idx + 1) % n;
        }
        self.frame_index.insert(id.to_string(), idx);
    }

    /// Drop positions and timers for slots and instances that are gone.
    fn prune(&mut self, model: &StripModel) {
        let mut slots: Vec<&str> = Vec::new();
        let mut instances: Vec<&str> = Vec::new();
        for slot in &model.slots {
            slots.push(&slot.id);
            match &slot.kind {
                SlotKind::Single { instance_id, .. } => instances.push(instance_id),
                SlotKind::Group { members, .. } => {
                    instances.extend(members.iter().map(|m| m.instance_id.as_str()));
                }
            }
        }
        self.group_index.retain(|k, _| slots.contains(&k.as_str()));
        self.group_at.retain(|k, _| slots.contains(&k.as_str()));
        self.frame_index
            .retain(|k, _| instances.contains(&k.as_str()));
        self.frame_at.retain(|k, _| instances.contains(&k.as_str()));
    }
}

/// The output a state can show: `Ok`, `Stale`, or an error's leftover output.
fn state_output(state: &SlotState) -> Option<&Output> {
    let out = match state {
        SlotState::Loading => None,
        SlotState::Ok(o) | SlotState::Stale(o) => Some(o),
        SlotState::Error { last, .. } => last.as_ref(),
    };
    out.filter(|o| !o.frames.is_empty())
}

pub(crate) fn block_width(b: &Block, m: &dyn Measure) -> f32 {
    match b {
        Block::Text { value, size, .. } => m.text_width(value, *size),
        Block::Icon { .. } => ICON_DIP,
        Block::Bar { width, .. } => width.map_or(DEFAULT_BAR_WIDTH_DIP, |w| w as f32),
        Block::Ring { .. } => RING_DIP,
        Block::Spark { .. } => SPARK_WIDTH_DIP,
        Block::Stack { top, bottom } => block_width(top, m).max(block_width(bottom, m)),
    }
}

fn frame_content_width(f: &Frame, m: &dyn Measure) -> f32 {
    let sum: f32 = f.blocks.iter().map(|b| block_width(b, m)).sum();
    let gaps = f.blocks.len().saturating_sub(1) as f32 * BLOCK_GAP_DIP;
    sum + gaps
}

/// Widest frame plus inner padding, so rotation does not jitter the strip.
fn output_width(o: &Output, m: &dyn Measure) -> f32 {
    let widest = o
        .frames
        .iter()
        .map(|f| frame_content_width(f, m))
        .fold(0.0, f32::max);
    widest + 2.0 * SLOT_PAD_DIP
}

fn slot_width(slot: &Slot, m: &dyn Measure) -> f32 {
    match &slot.kind {
        SlotKind::Single {
            state, max_width, ..
        } => {
            let w = state_output(state).map_or(EMPTY_SLOT_DIP, |o| output_width(o, m));
            max_width.map_or(w, |cap| w.min(cap as f32))
        }
        SlotKind::Group { members, .. } => members
            .iter()
            .filter_map(|mem| state_output(&mem.state))
            .map(|o| output_width(o, m))
            .reduce(f32::max)
            .unwrap_or(EMPTY_SLOT_DIP),
    }
}

fn place(slot: &Slot, x: f32, width: f32, rot: &RotationState) -> PlacedSlot {
    // The instance currently on show and whether the slot cycles members.
    let (shown, group_rotates) = match &slot.kind {
        SlotKind::Single {
            instance_id,
            widget_dir,
            state,
            ..
        } => (Some((instance_id, widget_dir, state)), false),
        SlotKind::Group { members, .. } if !members.is_empty() => {
            let idx = rot.group_index.get(&slot.id).copied().unwrap_or(0) % members.len();
            let m = &members[idx];
            (
                Some((&m.instance_id, &m.widget_dir, &m.state)),
                members.len() > 1,
            )
        }
        SlotKind::Group { .. } => (None, false),
    };
    let out = shown.and_then(|(_, _, state)| state_output(state));
    let frame = shown.and_then(|(id, dir, state)| {
        let o = state_output(state)?;
        Some(FrameRef {
            instance_id: id.clone(),
            widget_dir: Arc::clone(dir),
            frame_index: rot.frame_index.get(id).copied().unwrap_or(0) % o.frames.len(),
        })
    });
    let state = shown.map(|(_, _, s)| s);
    PlacedSlot {
        slot_id: slot.id.clone(),
        x_dip: x,
        width_dip: width,
        frame,
        rotating: group_rotates || out.is_some_and(|o| o.frames.len() > 1),
        stale: matches!(state, Some(SlotState::Stale(_))),
        error: match state {
            Some(SlotState::Error { message, .. }) => Some(message.clone()),
            _ => None,
        },
    }
}

fn plus_text(count: usize) -> String {
    format!("+{count}")
}

fn plus_width(count: usize, m: &dyn Measure) -> f32 {
    m.text_width(&plus_text(count), TextSize::Normal) + PLUS_N_PAD_DIP
}

pub fn layout(
    model: &StripModel,
    rot: &RotationState,
    metrics: &Metrics,
    m: &dyn Measure,
) -> StripLayout {
    let n = model.slots.len();
    let mut items: Vec<PlacedSlot> = Vec::new();
    let mut overflow: Vec<String> = Vec::new();
    let mut used = 0.0_f32;

    for (i, slot) in model.slots.iter().enumerate() {
        let w = slot_width(slot, m);
        let gap = if items.is_empty() { 0.0 } else { SLOT_GAP_DIP };
        let remaining = n - i - 1;
        // Keep room for "+N" when slots would still follow this one.
        let reserve = if remaining > 0 {
            SLOT_GAP_DIP + plus_width(remaining, m)
        } else {
            0.0
        };
        if used + gap + w + reserve <= metrics.available_dip {
            items.push(place(slot, used + gap, w, rot));
            used += gap + w;
        } else {
            overflow = model.slots[i..].iter().map(|s| s.id.clone()).collect();
            break;
        }
    }

    let overflow_item = if overflow.is_empty() {
        None
    } else {
        let gap = if items.is_empty() { 0.0 } else { SLOT_GAP_DIP };
        let width = plus_width(overflow.len(), m);
        let item = OverflowItem {
            x_dip: used + gap,
            width_dip: width,
            text: plus_text(overflow.len()),
        };
        used += gap + width;
        Some(item)
    };

    // Not even "+N" fits: show nothing (the strip hides at width 0) rather
    // than draw over the app buttons. The slack absorbs float rounding.
    if used > metrics.available_dip + 0.01 {
        return StripLayout::default();
    }

    StripLayout {
        width_dip: used,
        items,
        overflow,
        overflow_item,
    }
}

/// What the pointer is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    Slot(String),
    Overflow,
}

/// The slot or "+N" item at `x_dip` from the strip's left edge; `None` in
/// the gaps between them and outside the strip.
pub fn hit_test(layout: &StripLayout, x_dip: f32) -> Option<Hit> {
    let inside = |x: f32, w: f32| x_dip >= x && x_dip < x + w;
    if let Some(p) = layout.items.iter().find(|p| inside(p.x_dip, p.width_dip)) {
        return Some(Hit::Slot(p.slot_id.clone()));
    }
    layout
        .overflow_item
        .as_ref()
        .filter(|o| inside(o.x_dip, o.width_dip))
        .map(|_| Hit::Overflow)
}

/// The screen edge the taskbar sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
}

/// The edge of `monitor` that `taskbar` is closer to.
pub fn taskbar_edge(taskbar: RECT, monitor: RECT) -> Edge {
    if taskbar.top - monitor.top < monitor.bottom - taskbar.bottom {
        Edge::Top
    } else {
        Edge::Bottom
    }
}

/// The flyout's width for rows `rows` DIPs wide: the widest row plus
/// padding, at least [`FLYOUT_MIN_WIDTH_DIP`].
pub fn flyout_width_dip(rows: &[f32]) -> f32 {
    let widest = rows.iter().copied().fold(0.0, f32::max);
    (widest + 2.0 * FLYOUT_PAD_DIP).max(FLYOUT_MIN_WIDTH_DIP)
}

/// Where the "+N" flyout goes.
#[derive(Debug, Clone, PartialEq)]
pub struct FlyoutGeometry {
    /// The window in screen pixels.
    pub rect: RECT,
    /// The visible rows in client pixels, top to bottom; at most
    /// [`FLYOUT_MAX_ROWS`].
    pub rows: Vec<RECT>,
}

/// Lays out the flyout for rows `rows` DIPs wide: stacked with
/// [`FLYOUT_PAD_DIP`] around them and [`FLYOUT_SPACING_DIP`] between them,
/// as wide as the widest row plus padding (at least
/// [`FLYOUT_MIN_WIDTH_DIP`]), [`FLYOUT_GAP_DIP`] above the anchor for a
/// bottom taskbar (below it for a top one), right-aligned to it and kept
/// inside `work`. `anchor` is the "+N" item in screen pixels.
pub fn flyout_geometry(
    rows: &[f32],
    row_height_dip: f32,
    scale: f32,
    anchor: RECT,
    work: RECT,
    taskbar_edge: Edge,
) -> FlyoutGeometry {
    let shown = rows.len().min(FLYOUT_MAX_ROWS);
    let width_dip = flyout_width_dip(rows);
    let height_dip = 2.0 * FLYOUT_PAD_DIP
        + shown as f32 * row_height_dip
        + shown.saturating_sub(1) as f32 * FLYOUT_SPACING_DIP;
    let (w, h) = (px(width_dip, scale), px(height_dip, scale));
    let gap = px(FLYOUT_GAP_DIP, scale);

    // Right-aligned to the anchor, then shifted inside the work area (its
    // left edge wins when the flyout is wider than the work area).
    let left = (anchor.right - w).min(work.right - w).max(work.left);
    let top = match taskbar_edge {
        Edge::Bottom => (anchor.top - gap - h).max(work.top),
        Edge::Top => (anchor.bottom + gap).min(work.bottom - h).max(work.top),
    };
    let row_rects = (0..shown)
        .map(|i| {
            let y = FLYOUT_PAD_DIP + i as f32 * (row_height_dip + FLYOUT_SPACING_DIP);
            RECT {
                left: px(FLYOUT_PAD_DIP, scale),
                top: px(y, scale),
                right: px(width_dip - FLYOUT_PAD_DIP, scale),
                bottom: px(y + row_height_dip, scale),
            }
        })
        .collect();
    FlyoutGeometry {
        rect: RECT {
            left,
            top,
            right: left + w,
            bottom: top + h,
        },
        rows: row_rects,
    }
}

/// The hidden slots `overflow` as flyout rows, in that order: placed like
/// strip slots, at x 0 with their strip width.
pub fn flyout_rows(
    model: &StripModel,
    rot: &RotationState,
    overflow: &[String],
    m: &dyn Measure,
) -> Vec<PlacedSlot> {
    overflow
        .iter()
        .filter_map(|id| model.slots.iter().find(|s| &s.id == id))
        .map(|slot| place(slot, 0.0, slot_width(slot, m), rot))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::Color;
    use crate::strip::model::Member;

    struct Fake;
    impl Measure for Fake {
        fn text_width(&self, text: &str, _size: TextSize) -> f32 {
            text.chars().count() as f32 * 7.0
        }
    }

    fn text(s: &str) -> Block {
        Block::Text {
            value: s.into(),
            color: Color::Default,
            size: TextSize::Normal,
        }
    }

    fn frame(blocks: Vec<Block>) -> Frame {
        Frame {
            blocks,
            tooltip: None,
        }
    }

    fn out(texts: &[&str]) -> Output {
        Output {
            frames: texts.iter().map(|t| frame(vec![text(t)])).collect(),
            rotate: None,
            click: None,
        }
    }

    fn dir() -> Arc<PathBuf> {
        Arc::new(PathBuf::from("w"))
    }

    fn single(id: &str, state: SlotState, max_width: Option<u32>) -> Slot {
        Slot {
            id: id.into(),
            kind: SlotKind::Single {
                instance_id: id.into(),
                widget_dir: dir(),
                state,
                max_width,
            },
        }
    }

    fn member(id: &str, state: SlotState) -> Member {
        Member {
            instance_id: id.into(),
            widget_dir: dir(),
            state,
        }
    }

    fn group(id: &str, interval: u32, members: Vec<Member>) -> Slot {
        Slot {
            id: id.into(),
            kind: SlotKind::Group { interval, members },
        }
    }

    fn model(slots: Vec<Slot>) -> StripModel {
        StripModel {
            slots,
            generation: 1,
        }
    }

    fn metrics(available: f32) -> Metrics {
        Metrics {
            height_dip: 48.0,
            scale: 1.0,
            available_dip: available,
        }
    }

    fn run(m: &StripModel, available: f32) -> StripLayout {
        layout(m, &RotationState::default(), &metrics(available), &Fake)
    }

    fn btc(id: &str) -> Slot {
        single(id, SlotState::Ok(out(&["BTC $67,120"])), None)
    }

    #[test]
    fn single_text_slot_width() {
        let l = run(&model(vec![btc("s1")]), 500.0);
        assert_eq!(l.items.len(), 1);
        assert_eq!(l.items[0].width_dip, 85.0);
        assert_eq!(l.items[0].x_dip, 0.0);
        assert_eq!(l.width_dip, 85.0);
        assert!(l.overflow.is_empty());
    }

    #[test]
    fn blocks_icons_bars_and_stacks_add_up() {
        let f = frame(vec![
            Block::Icon {
                name: Some("sun".into()),
                src: None,
                tint: false,
                color: Color::Default,
            },
            Block::Bar {
                value: 0.5,
                color: Color::Default,
                width: None,
            },
            Block::Stack {
                top: Box::new(text("ab")),
                bottom: Box::new(text("abcd")),
            },
        ]);
        let o = Output {
            frames: vec![f],
            rotate: None,
            click: None,
        };
        // 16 + 32 + 28 (widest of stack) + 2 gaps of 4 + padding 8.
        let l = run(&model(vec![single("a", SlotState::Ok(o), None)]), 500.0);
        assert_eq!(l.items[0].width_dip, 92.0);
    }

    #[test]
    fn empty_states_are_24_wide() {
        let m = model(vec![
            single("l", SlotState::Loading, None),
            single(
                "e",
                SlotState::Error {
                    message: "boom".into(),
                    last: None,
                },
                None,
            ),
        ]);
        let l = run(&m, 500.0);
        assert_eq!(l.items[0].width_dip, 24.0);
        assert!(l.items[0].frame.is_none());
        assert_eq!(l.items[1].width_dip, 24.0);
        assert_eq!(l.items[1].error.as_deref(), Some("boom"));
        assert!(l.items[1].frame.is_none());
        assert_eq!(l.items[1].x_dip, 36.0);
    }

    #[test]
    fn stale_flag_is_set() {
        let m = model(vec![single("s", SlotState::Stale(out(&["x"])), None)]);
        let l = run(&m, 500.0);
        assert!(l.items[0].stale);
        assert!(l.items[0].frame.is_some());
    }

    #[test]
    fn group_width_is_widest_member() {
        let g = group(
            "g",
            5,
            vec![
                member("a", SlotState::Ok(out(&["ab"]))),
                member("b", SlotState::Ok(out(&["abcdefghij"]))),
                member("c", SlotState::Loading),
            ],
        );
        let l = run(&model(vec![g]), 500.0);
        assert_eq!(l.items[0].width_dip, 78.0);
        assert!(l.items[0].rotating);
    }

    #[test]
    fn group_without_output_is_loading_width() {
        let g = group("g", 5, vec![member("a", SlotState::Loading)]);
        let l = run(&model(vec![g]), 500.0);
        assert_eq!(l.items[0].width_dip, 24.0);
        assert!(l.items[0].frame.is_none());
    }

    #[test]
    fn multi_frame_width_is_widest_frame() {
        let s = single("s", SlotState::Ok(out(&["ab", "abcdef", "abc"])), None);
        let l = run(&model(vec![s]), 500.0);
        assert_eq!(l.items[0].width_dip, 50.0);
        assert!(l.items[0].rotating);
    }

    #[test]
    fn overflow_hides_from_end_of_order() {
        let m = model(vec![btc("s1"), btc("s2"), btc("s3")]);
        let l = run(&m, 150.0);
        let ids: Vec<_> = l.items.iter().map(|p| p.slot_id.as_str()).collect();
        assert_eq!(ids, ["s1"]);
        assert_eq!(l.overflow, ["s2", "s3"]);
        let plus = l.overflow_item.expect("plus item");
        assert_eq!(plus.text, "+2");
        assert_eq!(plus.width_dip, 22.0);
        assert_eq!(plus.x_dip, 97.0);
        assert_eq!(l.width_dip, 119.0);
    }

    #[test]
    fn no_overflow_item_when_all_fit() {
        let m = model(vec![btc("s1"), btc("s2")]);
        let l = run(&m, 182.0);
        assert_eq!(l.items.len(), 2);
        assert!(l.overflow.is_empty());
        assert!(l.overflow_item.is_none());
        assert_eq!(l.width_dip, 182.0);
    }

    #[test]
    fn plus_n_space_is_reserved() {
        // Two slots fit alone (182) but a third would need "+1" too.
        let m = model(vec![btc("s1"), btc("s2"), btc("s3")]);
        let l = run(&m, 190.0);
        assert_eq!(l.items.len(), 1);
        assert_eq!(l.overflow, ["s2", "s3"]);
    }

    #[test]
    fn zero_available_space_shows_nothing() {
        // Not even "+N" fits: the strip must not cover the app buttons.
        let m = model(vec![btc("s1"), btc("s2")]);
        let l = run(&m, 0.0);
        assert_eq!(l, StripLayout::default());
        assert_eq!(l.width_dip, 0.0);
    }

    #[test]
    fn plus_n_alone_shows_only_when_it_fits() {
        // "+2" is 22 wide with the fake measure.
        let m = model(vec![btc("s1"), btc("s2")]);
        assert_eq!(run(&m, 21.0), StripLayout::default());
        let l = run(&m, 22.0);
        assert!(l.items.is_empty());
        assert_eq!(l.overflow, ["s1", "s2"]);
        let plus = l.overflow_item.expect("plus item");
        assert_eq!(plus.x_dip, 0.0);
        assert_eq!(plus.text, "+2");
        assert!(l.width_dip <= 22.0);
    }

    #[test]
    fn max_width_ellipsizes() {
        let s = single("s", SlotState::Ok(out(&["BTC $67,120"])), Some(40));
        let l = run(&model(vec![s]), 500.0);
        assert_eq!(l.items[0].width_dip, 40.0);
    }

    #[test]
    fn layout_scales_with_dpi() {
        let m = model(vec![btc("s1")]);
        let a = layout(
            &m,
            &RotationState::default(),
            &Metrics {
                scale: 1.5,
                ..metrics(500.0)
            },
            &Fake,
        );
        let b = run(&m, 500.0);
        assert_eq!(a.width_dip, b.width_dip);
        assert_eq!(px(a.width_dip, 1.5), 128);
        assert_eq!(px(a.items[0].width_dip, 1.0), 85);
    }

    #[test]
    fn rotation_advances_and_pauses_on_hover() {
        let g = group(
            "g",
            5,
            vec![
                member("a", SlotState::Ok(out(&["one"]))),
                member("b", SlotState::Ok(out(&["two"]))),
            ],
        );
        let m = model(vec![g]);
        let mut rot = RotationState::default();
        let t0 = Instant::now();
        let secs = |s: u64| t0 + Duration::from_secs(s);
        let shown = |rot: &RotationState| {
            layout(&m, rot, &metrics(500.0), &Fake).items[0]
                .frame
                .clone()
                .unwrap()
                .instance_id
        };

        rot.tick(&m, t0);
        assert_eq!(shown(&rot), "a");
        rot.tick(&m, secs(4));
        assert_eq!(shown(&rot), "a");
        rot.tick(&m, secs(5));
        assert_eq!(shown(&rot), "b");

        rot.paused_slot = Some("g".into());
        rot.tick(&m, secs(10));
        rot.tick(&m, secs(20));
        assert_eq!(shown(&rot), "b");

        // The timer restarts on unpause instead of firing at once.
        rot.paused_slot = None;
        rot.tick(&m, secs(21));
        assert_eq!(shown(&rot), "b");
        rot.tick(&m, secs(25));
        assert_eq!(shown(&rot), "a");
    }

    #[test]
    fn frames_rotate_with_default_and_custom_period() {
        let mut slow = out(&["a", "b", "c"]);
        let mut fast = out(&["x", "y"]);
        fast.rotate = Some(2);
        slow.rotate = None;
        let m = model(vec![
            single("slow", SlotState::Ok(slow), None),
            single("fast", SlotState::Ok(fast), None),
        ]);
        let mut rot = RotationState::default();
        let t0 = Instant::now();
        rot.tick(&m, t0);
        rot.tick(&m, t0 + Duration::from_secs(2));
        assert_eq!(rot.frame_index["fast"], 1);
        assert_eq!(rot.frame_index["slow"], 0);
        rot.tick(&m, t0 + Duration::from_secs(6));
        assert_eq!(rot.frame_index["slow"], 1);
        let l = layout(&m, &rot, &metrics(500.0), &Fake);
        assert_eq!(l.items[0].frame.as_ref().unwrap().frame_index, 1);
    }

    #[test]
    fn next_due_is_the_earliest_rotation_across_keys() {
        // A 6 s two-frame clock next to an 8 s group.
        let clock = single("clock", SlotState::Ok(out(&["a", "b"])), None);
        let g = group(
            "g",
            8,
            vec![
                member("m1", SlotState::Ok(out(&["one"]))),
                member("m2", SlotState::Ok(out(&["two"]))),
            ],
        );
        let m = model(vec![clock, g]);
        let mut rot = RotationState::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        rot.tick(&m, t0);
        assert_eq!(rot.next_due(&m), Some(at(6)));
        rot.tick(&m, at(6));
        assert_eq!(rot.frame_index["clock"], 1);
        // The group is due at 8 s, not at the clock's next tick (12 s).
        assert_eq!(rot.next_due(&m), Some(at(8)));
        rot.tick(&m, at(8));
        assert_eq!(rot.group_index["g"], 1);
        assert_eq!(rot.next_due(&m), Some(at(12)));
    }

    #[test]
    fn next_due_is_none_when_nothing_rotates() {
        let m = model(vec![
            btc("s1"),
            group("solo", 5, vec![member("a", SlotState::Ok(out(&["x"])))]),
        ]);
        let mut rot = RotationState::default();
        rot.tick(&m, Instant::now());
        assert_eq!(rot.next_due(&m), None);
    }

    #[test]
    fn frames_rotate_inside_the_shown_member() {
        let mut o = out(&["a", "b"]);
        o.rotate = Some(2);
        let g = group(
            "g",
            60,
            vec![
                member("m1", SlotState::Ok(o)),
                member("m2", SlotState::Ok(out(&["z"]))),
            ],
        );
        let m = model(vec![g]);
        let mut rot = RotationState::default();
        let t0 = Instant::now();
        rot.tick(&m, t0);
        rot.tick(&m, t0 + Duration::from_secs(2));
        let l = layout(&m, &rot, &metrics(500.0), &Fake);
        let f = l.items[0].frame.as_ref().unwrap();
        assert_eq!((f.instance_id.as_str(), f.frame_index), ("m1", 1));
    }

    #[test]
    fn indices_are_clamped_when_model_shrinks() {
        let mut rot = RotationState::default();
        rot.group_index.insert("g".into(), 5);
        rot.frame_index.insert("a".into(), 9);
        let g = group("g", 5, vec![member("a", SlotState::Ok(out(&["p", "q"])))]);
        let m = model(vec![g]);
        let l = layout(&m, &rot, &metrics(500.0), &Fake);
        let f = l.items[0].frame.as_ref().unwrap();
        assert_eq!(f.instance_id, "a");
        assert!(f.frame_index < 2);
        rot.tick(&m, Instant::now());
        assert_eq!(rot.group_index["g"], 0);
        assert!(rot.frame_index["a"] < 2);
    }

    #[test]
    fn hit_test_finds_slot() {
        // s1 at [0, 85), s2 at [97, 182).
        let l = run(&model(vec![btc("s1"), btc("s2")]), 500.0);
        assert_eq!(hit_test(&l, 0.0), Some(Hit::Slot("s1".into())));
        assert_eq!(hit_test(&l, 84.9), Some(Hit::Slot("s1".into())));
        assert_eq!(hit_test(&l, 97.0), Some(Hit::Slot("s2".into())));
        assert_eq!(hit_test(&l, 181.0), Some(Hit::Slot("s2".into())));
    }

    #[test]
    fn hit_test_overflow_item() {
        // s1 at [0, 85), "+2" at [97, 119).
        let l = run(&model(vec![btc("s1"), btc("s2"), btc("s3")]), 150.0);
        assert_eq!(hit_test(&l, 97.0), Some(Hit::Overflow));
        assert_eq!(hit_test(&l, 118.0), Some(Hit::Overflow));
        assert_eq!(hit_test(&l, 119.0), None);
    }

    #[test]
    fn hit_test_gap_is_none() {
        let l = run(&model(vec![btc("s1"), btc("s2")]), 500.0);
        assert_eq!(hit_test(&l, 85.0), None);
        assert_eq!(hit_test(&l, 90.0), None);
        assert_eq!(hit_test(&l, -1.0), None);
        assert_eq!(hit_test(&l, 182.0), None);
        assert_eq!(hit_test(&StripLayout::default(), 0.0), None);
    }

    #[test]
    fn advance_moves_group_members_both_ways() {
        let g = group(
            "g",
            5,
            vec![
                member("a", SlotState::Ok(out(&["one"]))),
                member("b", SlotState::Ok(out(&["two"]))),
                member("c", SlotState::Ok(out(&["three"]))),
            ],
        );
        let m = model(vec![g]);
        let mut rot = RotationState::default();
        assert!(rot.advance("g", 1, &m));
        assert_eq!(rot.group_index["g"], 1);
        assert!(rot.advance("g", 1, &m));
        assert!(rot.advance("g", 1, &m));
        assert_eq!(rot.group_index["g"], 0);
        assert!(rot.advance("g", -1, &m));
        assert_eq!(rot.group_index["g"], 2);
    }

    #[test]
    fn advance_moves_frames_of_a_single_slot() {
        let m = model(vec![single("s", SlotState::Ok(out(&["a", "b"])), None)]);
        let mut rot = RotationState::default();
        assert!(rot.advance("s", -1, &m));
        assert_eq!(rot.frame_index["s"], 1);
        assert!(rot.advance("s", 1, &m));
        assert_eq!(rot.frame_index["s"], 0);
    }

    #[test]
    fn advance_does_nothing_without_alternatives() {
        let m = model(vec![
            btc("one-frame"),
            single("loading", SlotState::Loading, None),
            group("solo", 5, vec![member("a", SlotState::Ok(out(&["x"])))]),
        ]);
        let mut rot = RotationState::default();
        for id in ["one-frame", "loading", "solo", "missing"] {
            assert!(!rot.advance(id, 1, &m), "{id}");
        }
        assert!(!rot.advance("one-frame", 0, &m));
        assert!(rot.frame_index.is_empty());
    }

    #[test]
    fn tick_prunes_removed_slots() {
        let mut rot = RotationState::default();
        rot.group_index.insert("gone".into(), 1);
        rot.frame_index.insert("gone".into(), 1);
        rot.tick(&model(vec![btc("s1")]), Instant::now());
        assert!(rot.group_index.is_empty());
        assert!(rot.frame_index.is_empty());
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
    fn flyout_above_bottom_taskbar() {
        let work = rect(0, 0, 2560, 1552);
        let anchor = rect(2300, 1552, 2330, 1600);
        let g = flyout_geometry(&[100.0, 120.0], 40.0, 1.0, anchor, work, Edge::Bottom);
        assert_eq!(g.rect.bottom, anchor.top - px(FLYOUT_GAP_DIP, 1.0));
        assert_eq!(g.rect.right, anchor.right);
        // 8 + 40 + 4 + 40 + 8; 160 wide (the minimum).
        assert_eq!(g.rect.bottom - g.rect.top, 100);
        assert_eq!(g.rect.right - g.rect.left, 160);
        assert_eq!(g.rows, [rect(8, 8, 152, 48), rect(8, 52, 152, 92)]);
    }

    #[test]
    fn flyout_below_top_taskbar() {
        let work = rect(0, 48, 1920, 1080);
        let anchor = rect(1500, 0, 1530, 48);
        let g = flyout_geometry(&[100.0], 40.0, 1.5, anchor, work, Edge::Top);
        assert_eq!(g.rect.top, anchor.bottom + px(FLYOUT_GAP_DIP, 1.5));
        assert_eq!(g.rect.right, anchor.right);
        assert_eq!(g.rect.bottom - g.rect.top, px(8.0 + 40.0 + 8.0, 1.5));
    }

    #[test]
    fn flyout_clamped_to_work_area() {
        let work = rect(0, 0, 1920, 1032);
        let anchor = rect(10, 1032, 40, 1080);
        let g = flyout_geometry(&[200.0], 40.0, 1.0, anchor, work, Edge::Bottom);
        assert!(g.rect.left >= work.left);
        assert_eq!(g.rect.left, 0);
        assert_eq!(g.rect.right, 216);
        // A secondary monitor left of the primary one has negative x.
        let work = rect(-1920, 0, 0, 1040);
        let anchor = rect(-1915, 1040, -1890, 1080);
        let g = flyout_geometry(&[200.0], 40.0, 1.0, anchor, work, Edge::Bottom);
        assert_eq!((g.rect.left, g.rect.right), (-1920, -1704));
        assert_eq!(g.rect.bottom, 1040 - px(FLYOUT_GAP_DIP, 1.0));
        // Too tall for the space above: kept inside the top of the work area.
        let work = rect(0, 900, 1920, 1032);
        let g = flyout_geometry(
            &[200.0; 5],
            40.0,
            1.0,
            rect(500, 1032, 530, 1080),
            work,
            Edge::Bottom,
        );
        assert_eq!(g.rect.top, 900);
    }

    #[test]
    fn flyout_width_is_widest_row() {
        let work = rect(0, 0, 2560, 1552);
        let anchor = rect(2300, 1552, 2330, 1600);
        let g = flyout_geometry(&[80.0, 150.0], 40.0, 1.5, anchor, work, Edge::Bottom);
        assert_eq!(
            g.rect.right - g.rect.left,
            ((150.0_f32 + 16.0) * 1.5).round() as i32
        );
        // Rows span the inner width.
        assert!(g
            .rows
            .iter()
            .all(|r| r.left == px(8.0, 1.5) && r.right == px(158.0, 1.5)));
        // Narrow rows still get the 160 DIP minimum.
        let g = flyout_geometry(&[80.0, 140.0], 40.0, 1.5, anchor, work, Edge::Bottom);
        assert_eq!(g.rect.right - g.rect.left, px(160.0, 1.5));
    }

    #[test]
    fn flyout_caps_visible_rows() {
        let work = rect(0, 0, 2560, 1552);
        let anchor = rect(2300, 1552, 2330, 1600);
        let g = flyout_geometry(&[100.0; 20], 40.0, 1.0, anchor, work, Edge::Bottom);
        assert_eq!(g.rows.len(), FLYOUT_MAX_ROWS);
        assert_eq!(FLYOUT_MAX_ROWS, 12);
        assert_eq!(g.rect.bottom - g.rect.top, 16 + 12 * 40 + 11 * 4);
    }

    #[test]
    fn taskbar_edge_from_its_monitor() {
        let mon = rect(0, 0, 1920, 1080);
        assert_eq!(taskbar_edge(rect(0, 1032, 1920, 1080), mon), Edge::Bottom);
        assert_eq!(taskbar_edge(rect(0, 0, 1920, 48), mon), Edge::Top);
    }

    #[test]
    fn flyout_rows_are_the_hidden_slots_in_order() {
        let m = model(vec![btc("a"), btc("b"), btc("c")]);
        let rows = flyout_rows(
            &m,
            &RotationState::default(),
            &["c".into(), "b".into()],
            &Fake,
        );
        let ids: Vec<&str> = rows.iter().map(|p| p.slot_id.as_str()).collect();
        assert_eq!(ids, ["c", "b"]);
        assert_eq!(rows[0].width_dip, 11.0 * 7.0 + 2.0 * SLOT_PAD_DIP);
        assert_eq!(
            rows[0].frame.as_ref().map(|f| f.instance_id.as_str()),
            Some("c")
        );
    }
}
