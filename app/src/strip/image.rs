//! Custom widget icons (SVG or PNG files inside the widget folder).
//!
//! The strip thread never touches the disk or rasterizes here. [`ImageCache`]
//! hands each (widget folder, src, pixel size) to one background worker
//! thread, draws the placeholder dot while it is pending, and picks the
//! result up on a later draw; the worker calls the `notify` callback (the
//! strip posts itself a message) when a result is ready.
//!
//! The worker resolves the file (canonical path, mtime, size) and keeps the
//! rasters keyed by (canonical path, mtime, file size, pixel size). Tint is
//! never part of a key: it is applied to a copy of the cached raster. Failed
//! rasterizations (errors and timeouts) are remembered per key apart from
//! the rasters, so trimming the rasters never retries a broken file.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use resvg::tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};
use resvg::usvg;

use crate::strip::render::Bitmap;

/// Largest icon file accepted.
pub const MAX_ICON_BYTES: u64 = 256 * 1024;
/// Largest decoded PNG side, in pixels.
const MAX_PNG_SIDE: u32 = 1024;
/// Decoder allocation cap for PNGs (guards against decompression bombs).
const MAX_PNG_DECODE_BYTES: usize = 8 * 1024 * 1024;
/// usvg nesting limit; its converter recurses once per level.
const SVG_MAX_DEPTH: u32 = 32;
/// Most elements an icon may have once `<use>` references are expanded.
const SVG_MAX_ELEMENTS: u64 = 10_000;
/// Stack for the SVG rasterizer thread.
const SVG_THREAD_STACK: usize = 16 * 1024 * 1024;
/// An SVG that takes longer than this counts as failed.
const SVG_TIMEOUT: Duration = Duration::from_secs(3);
/// Rasters (and strip-side entries) kept; the least recently used goes first.
const MAX_ENTRIES: usize = 256;
/// Failed keys kept. They are tiny, and only new files or sizes add one.
const MAX_FAILURES: usize = 4096;
/// A shown icon is re-resolved (off the strip thread) at most this often,
/// so an edited file shows up without disk I/O on every draw.
const REVALIDATE: Duration = Duration::from_secs(5);

/// Turns a resolved icon file into a `size` x `size` bitmap. Injectable so
/// tests can count rasterizations.
pub type Rasterizer = Arc<dyn Fn(&Path, u32) -> Result<Bitmap, RasterError> + Send + Sync>;

/// Why an icon could not be rasterized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RasterError {
    /// Broken or unsupported file; cached as failed for this file version.
    Failed(String),
    /// Took longer than the limit; that widget folder and src stay failed
    /// for the rest of the process, whatever later edits to the file.
    TimedOut,
    /// Another SVG rasterization (possibly a timed-out one) still runs;
    /// not cached, the icon is tried again on a later recheck.
    Busy,
}

impl std::fmt::Display for RasterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RasterError::Failed(why) => f.write_str(why),
            RasterError::TimedOut => f.write_str("took too long to draw"),
            RasterError::Busy => f.write_str("another icon is still being drawn"),
        }
    }
}

/// Allows one isolated rasterization thread alive at a time. A thread that
/// timed out keeps its slot until it really ends, so a widget cannot pile
/// up runaway threads by changing its src or file.
pub struct Slot {
    busy: Mutex<bool>,
    cv: Condvar,
}

impl Slot {
    pub const fn new() -> Slot {
        Slot {
            busy: Mutex::new(false),
            cv: Condvar::new(),
        }
    }
}

impl Default for Slot {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide slot for SVG rasterization threads.
static SVG_SLOT: Slot = Slot::new();

/// Frees the slot when the rasterization thread ends, however it ends.
struct SlotGuard(&'static Slot);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        *self.0.busy.lock().unwrap_or_else(|p| p.into_inner()) = false;
        self.0.cv.notify_all();
    }
}

/// Runs `job` on a fresh thread with a large stack, holding `slot`. Waits
/// up to `wait` for the slot (else `Busy`, without spawning) and up to
/// `timeout` for the result (else `TimedOut`; the thread keeps the slot
/// until it finishes).
fn run_limited<T: Send + 'static>(
    slot: &'static Slot,
    wait: Duration,
    timeout: Duration,
    job: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, RasterError> {
    {
        let deadline = Instant::now() + wait;
        let mut busy = slot.busy.lock().unwrap_or_else(|p| p.into_inner());
        while *busy {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(RasterError::Busy);
            }
            busy = slot
                .cv
                .wait_timeout(busy, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        *busy = true;
    }
    let guard = SlotGuard(slot);
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("svg-icon".into())
        .stack_size(SVG_THREAD_STACK)
        .spawn(move || {
            let _guard = guard;
            let _ = tx.send(job());
        });
    if let Err(e) = spawned {
        // The closure (and the guard in it) was dropped: the slot is free.
        return Err(RasterError::Failed(format!("cannot start SVG thread: {e}")));
    }
    match rx.recv_timeout(timeout) {
        Ok(r) => r.map_err(RasterError::Failed),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(RasterError::TimedOut),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(RasterError::Failed("SVG rasterizer panicked".into()))
        }
    }
}
/// Called on the worker thread whenever a result is ready.
pub type Notify = Arc<dyn Fn() + Send + Sync>;

/// What the strip asks for: (widget folder, src, pixel size).
type Request = (PathBuf, String, u32);
/// What a raster depends on: (canonical path, mtime, file size, pixel size).
type RasterKey = (PathBuf, SystemTime, u64, u32);
/// A finished request; `None` means draw the fallback dot.
type Done = (Request, Option<Arc<Bitmap>>);

/// What the strip last got for a request.
struct Shown {
    bitmap: Option<Arc<Bitmap>>,
    /// When the worker last resolved it; `None` forces a recheck.
    checked: Option<Instant>,
    used: u64,
}

#[derive(Default)]
struct QueueState {
    jobs: VecDeque<Request>,
    closed: bool,
}

#[derive(Default)]
struct Queue {
    state: Mutex<QueueState>,
    cv: Condvar,
}

impl Queue {
    /// Adds `req` unless it is already waiting.
    fn push(&self, req: Request) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !s.jobs.contains(&req) {
            s.jobs.push_back(req);
            self.cv.notify_one();
        }
    }

    /// The next job, or `None` once closed.
    fn pop(&self) -> Option<Request> {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if s.closed {
                return None;
            }
            if let Some(j) = s.jobs.pop_front() {
                return Some(j);
            }
            s = self.cv.wait(s).unwrap_or_else(|p| p.into_inner());
        }
    }

    fn close(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).closed = true;
        self.cv.notify_all();
    }
}

/// The strip-side icon cache. Never blocks: `get` returns what is ready and
/// queues the rest for the worker.
pub struct ImageCache {
    shown: HashMap<Request, Shown>,
    /// Requests sent to the worker and not answered yet.
    pending: HashSet<Request>,
    queue: Arc<Queue>,
    results: Receiver<Done>,
    clock: u64,
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageCache {
    /// A cache with the real rasterizer and no notification.
    pub fn new() -> Self {
        Self::with(Arc::new(load), Arc::new(|| {}))
    }

    /// A cache whose worker calls `notify` after each result.
    pub fn with_notify(notify: Notify) -> Self {
        Self::with(Arc::new(load), notify)
    }

    pub fn with(rasterize: Rasterizer, notify: Notify) -> Self {
        let queue = Arc::new(Queue::default());
        let (tx, results) = mpsc::channel();
        let q = queue.clone();
        let spawned = std::thread::Builder::new()
            .name("perchbar-icons".into())
            .spawn(move || worker(&q, &tx, &rasterize, &notify));
        if let Err(e) = spawned {
            // Without the worker every icon stays a dot; nothing blocks.
            log::warn!("cannot start the icon worker ({e}); custom icons show as dots");
        }
        ImageCache {
            shown: HashMap::new(),
            pending: HashSet::new(),
            queue,
            results,
            clock: 0,
        }
    }

    /// The icon `src` from `widget_dir`, `size_px` square and optionally
    /// tinted, if it is ready. `None` means the caller draws the fallback
    /// dot: the icon is broken, or still being rasterized (a later draw,
    /// after `notify`, picks it up).
    pub fn get(
        &mut self,
        widget_dir: &Path,
        src: &str,
        size_px: u32,
        tint: Option<[u8; 4]>,
    ) -> Option<Arc<Bitmap>> {
        if size_px == 0 {
            return None;
        }
        self.poll();
        self.clock += 1;
        let req: Request = (widget_dir.to_path_buf(), src.to_string(), size_px);
        let (bitmap, recheck) = match self.shown.get_mut(&req) {
            Some(e) => {
                e.used = self.clock;
                let due = e.checked.is_none_or(|t| t.elapsed() >= REVALIDATE);
                (e.bitmap.clone(), due)
            }
            None => (None, true),
        };
        if recheck && self.pending.insert(req.clone()) {
            self.queue.push(req);
        }
        let bitmap = bitmap?;
        Some(match tint {
            Some(t) => {
                let mut copy = (*bitmap).clone();
                apply_tint(&mut copy, t);
                Arc::new(copy)
            }
            None => bitmap,
        })
    }

    /// Takes the worker's finished results. Returns whether any of them
    /// changes what a draw would show.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(done) = self.results.try_recv() {
            changed |= self.accept(done);
        }
        changed
    }

    /// Makes every icon re-resolve on its next draw (a widget published new
    /// output, so its files may have changed).
    pub fn invalidate(&mut self) {
        for e in self.shown.values_mut() {
            e.checked = None;
        }
    }

    /// Waits up to `timeout` for every pending request. For tests and
    /// offline renders only: the strip thread must never call it.
    pub fn settle(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.pending.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.results.recv_timeout(left) {
                Ok(done) => {
                    self.accept(done);
                }
                Err(_) => return false,
            }
        }
        true
    }

    fn accept(&mut self, (req, bitmap): Done) -> bool {
        self.pending.remove(&req);
        let changed = match self.shown.get(&req) {
            None => true,
            Some(old) => match (&old.bitmap, &bitmap) {
                (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                (None, None) => false,
                _ => true,
            },
        };
        if !self.shown.contains_key(&req) && self.shown.len() >= MAX_ENTRIES {
            if let Some(oldest) = self
                .shown
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            {
                self.shown.remove(&oldest);
            }
        }
        self.shown.insert(
            req,
            Shown {
                bitmap,
                checked: Some(Instant::now()),
                used: self.clock,
            },
        );
        changed
    }
}

impl Drop for ImageCache {
    fn drop(&mut self) {
        self.queue.close();
    }
}

/// Rasters by key, least recently used evicted first, and failed keys kept
/// apart so trimming never brings a broken file back.
#[derive(Default)]
struct RasterCache {
    ok: HashMap<RasterKey, (Arc<Bitmap>, u64)>,
    failed: HashSet<RasterKey>,
    clock: u64,
}

impl RasterCache {
    /// `Some(None)` for a known failure, `None` when not cached.
    fn get(&mut self, key: &RasterKey) -> Option<Option<Arc<Bitmap>>> {
        if self.failed.contains(key) {
            return Some(None);
        }
        self.clock += 1;
        let clock = self.clock;
        self.ok.get_mut(key).map(|(b, used)| {
            *used = clock;
            Some(b.clone())
        })
    }

    fn insert(&mut self, key: RasterKey, result: Option<Arc<Bitmap>>) {
        match result {
            Some(b) => {
                if self.ok.len() >= MAX_ENTRIES {
                    if let Some(oldest) = self
                        .ok
                        .iter()
                        .min_by_key(|(_, (_, used))| *used)
                        .map(|(k, _)| k.clone())
                    {
                        self.ok.remove(&oldest);
                    }
                }
                self.clock += 1;
                self.ok.insert(key, (b, self.clock));
            }
            None => {
                if self.failed.len() >= MAX_FAILURES {
                    self.failed.clear();
                }
                self.failed.insert(key);
            }
        }
    }
}

/// The worker loop: resolve, look up or rasterize, send, notify.
fn worker(queue: &Queue, tx: &Sender<Done>, rasterize: &Rasterizer, notify: &Notify) {
    let mut cache = RasterCache::default();
    // (widget folder, src) that timed out once: failed for good.
    let mut timed_out: HashSet<(PathBuf, String)> = HashSet::new();
    let mut logged: HashSet<String> = HashSet::new();
    let mut log_once = |dir: &Path, src: &str, why: &str| {
        if logged.insert(format!("{}|{src}|{why}", dir.display())) {
            log::warn!(
                "icon {src:?} in {}: {why}; drawing a dot instead",
                dir.display()
            );
        }
    };
    while let Some(req) = queue.pop() {
        let (dir, src, size) = (&req.0, req.1.as_str(), req.2);
        let gave_up = timed_out.contains(&(dir.clone(), src.to_string()));
        let bitmap = match resolve(dir, src) {
            _ if gave_up => None,
            Err(why) => {
                log_once(dir, src, &why);
                None
            }
            Ok((path, mtime, len)) => {
                let key: RasterKey = (path, mtime, len, size);
                match cache.get(&key) {
                    Some(hit) => hit,
                    None => match rasterize(&key.0, size) {
                        Ok(b) => {
                            let made = Some(Arc::new(b));
                            cache.insert(key, made.clone());
                            made
                        }
                        Err(RasterError::Busy) => None,
                        Err(e) => {
                            log_once(dir, src, &e.to_string());
                            if e == RasterError::TimedOut {
                                timed_out.insert((dir.clone(), src.to_string()));
                            }
                            cache.insert(key, None);
                            None
                        }
                    },
                }
            }
        };
        if tx.send((req, bitmap)).is_err() {
            return;
        }
        notify();
    }
}

/// Resolves `src` inside `widget_dir`, following links, and refuses anything
/// that ends up outside the folder or is too big. Returns the canonical
/// path, its mtime and its size.
fn resolve(widget_dir: &Path, src: &str) -> Result<(PathBuf, SystemTime, u64), String> {
    // ':' covers drive letters, NTFS alternate data streams and `data:` URLs.
    if src.is_empty() || src.contains(':') {
        return Err("invalid path".into());
    }
    let dir = std::fs::canonicalize(widget_dir).map_err(|e| format!("widget folder: {e}"))?;
    let file = std::fs::canonicalize(widget_dir.join(src)).map_err(|e| e.to_string())?;
    if !file.starts_with(&dir) {
        return Err("resolves outside the widget folder".into());
    }
    let meta = std::fs::metadata(&file).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a file".into());
    }
    if meta.len() > MAX_ICON_BYTES {
        return Err("larger than 256 KB".into());
    }
    let mtime = meta.modified().map_err(|e| e.to_string())?;
    Ok((file, mtime, meta.len()))
}

fn load(path: &Path, size_px: u32) -> Result<Bitmap, RasterError> {
    let failed = RasterError::Failed;
    let mut data = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_ICON_BYTES + 1).read_to_end(&mut data))
        .map_err(|e| failed(e.to_string()))?;
    if data.len() as u64 > MAX_ICON_BYTES {
        return Err(failed("larger than 256 KB".into()));
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let pixmap = match ext.as_deref() {
        Some("svg") => rasterize_svg_isolated(data, size_px)?,
        Some("png") => scale_png(&data, size_px).map_err(failed)?,
        _ => return Err(failed("not .svg or .png".into())),
    };
    Ok(to_bitmap(pixmap))
}

/// Parses an SVG without touching the file system or network: no resources
/// folder, every `<image>` reference refused, and no font support compiled in.
fn svg_tree(data: &[u8]) -> Option<usvg::Tree> {
    if expanded_elements(data)? > SVG_MAX_ELEMENTS {
        return None;
    }
    let opt = usvg::Options {
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        max_depth: SVG_MAX_DEPTH,
        ..usvg::Options::default()
    };
    usvg::Tree::from_data(data, &opt).ok()
}

/// Element count after expanding every `<use>`, so a fan-out of references
/// is refused up front instead of being expanded by usvg (which only stops
/// at a million elements, seconds of CPU). Iterative, so nesting depth does
/// not matter. `None` when the XML does not parse.
fn expanded_elements(data: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(data).ok()?;
    let opt = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = roxmltree::Document::parse_with_options(text, opt).ok()?;
    let by_id: HashMap<&str, roxmltree::NodeId> = doc
        .descendants()
        .filter_map(|n| Some((n.attribute("id")?, n.id())))
        .collect();
    let use_target = |n: roxmltree::Node| -> Option<roxmltree::NodeId> {
        if n.tag_name().name() != "use" {
            return None;
        }
        let href = n
            .attribute(("http://www.w3.org/1999/xlink", "href"))
            .or_else(|| n.attribute("href"))?;
        by_id.get(href.strip_prefix('#')?).copied()
    };

    // Post-order walk with an explicit stack. `None` in `count` means the
    // node is being counted; a reference back to it (a cycle) adds nothing.
    let mut count: HashMap<roxmltree::NodeId, Option<u64>> = HashMap::new();
    let root = doc.root_element().id();
    let mut stack = vec![(root, false)];
    while let Some((id, done)) = stack.pop() {
        let node = doc.get_node(id)?;
        let deps = node
            .children()
            .filter(|c| c.is_element())
            .map(|c| c.id())
            .chain(use_target(node));
        if done {
            let total = deps.fold(1_u64, |acc, d| {
                acc.saturating_add(count.get(&d).copied().flatten().unwrap_or(0))
            });
            if total > SVG_MAX_ELEMENTS {
                return Some(total);
            }
            count.insert(id, Some(total));
        } else if let std::collections::hash_map::Entry::Vacant(e) = count.entry(id) {
            e.insert(None);
            stack.push((id, true));
            stack.extend(deps.filter(|d| !count.contains_key(d)).map(|d| (d, false)));
        }
    }
    count.get(&root).copied().flatten()
}

/// Parses and rasterizes on a fresh thread with a large stack, so a deeply
/// nested file can never overflow the caller's stack (an overflow aborts the
/// process and cannot be caught). Runs on the icon worker, never the strip.
fn rasterize_svg_isolated(data: Vec<u8>, size: u32) -> Result<Pixmap, RasterError> {
    run_limited(&SVG_SLOT, SVG_TIMEOUT, SVG_TIMEOUT, move || {
        rasterize_svg(&data, size)
    })
}

/// Uniform scale and offset that fit `w` x `h` centered in a square.
fn fit(w: f32, h: f32, size: u32) -> Transform {
    let s = size as f32 / w.max(h);
    let dx = (size as f32 - w * s) / 2.0;
    let dy = (size as f32 - h * s) / 2.0;
    Transform::from_row(s, 0.0, 0.0, s, dx, dy)
}

fn rasterize_svg(data: &[u8], size: u32) -> Result<Pixmap, String> {
    let tree = svg_tree(data).ok_or("not a valid SVG")?;
    let sz = tree.size();
    let mut pm = Pixmap::new(size, size).ok_or("bad size")?;
    resvg::render(&tree, fit(sz.width(), sz.height(), size), &mut pm.as_mut());
    Ok(pm)
}

fn scale_png(data: &[u8], size: u32) -> Result<Pixmap, String> {
    let bad = |e: png::DecodingError| format!("bad PNG: {e}");
    let mut dec = png::Decoder::new_with_limits(
        std::io::Cursor::new(data),
        png::Limits {
            bytes: MAX_PNG_DECODE_BYTES,
        },
    );
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().map_err(bad)?;
    let (w, h) = {
        let info = reader.info();
        (info.width, info.height)
    };
    if w == 0 || h == 0 || w > MAX_PNG_SIDE || h > MAX_PNG_SIDE {
        return Err("PNG dimensions out of range".into());
    }
    let mut buf = vec![0; reader.output_buffer_size().ok_or("PNG too large")?];
    let frame = reader.next_frame(&mut buf).map_err(bad)?;
    let buf = &buf[..frame.buffer_size()];
    let mut src = Pixmap::new(w, h).ok_or("bad size")?;
    let rgba: fn(&[u8]) -> [u8; 4] = match frame.color_type {
        png::ColorType::Rgba => |p| [p[0], p[1], p[2], p[3]],
        png::ColorType::Rgb => |p| [p[0], p[1], p[2], 255],
        png::ColorType::GrayscaleAlpha => |p| [p[0], p[0], p[0], p[1]],
        png::ColorType::Grayscale => |p| [p[0], p[0], p[0], 255],
        png::ColorType::Indexed => return Err("unexpanded palette PNG".into()),
    };
    let channels = frame.color_type.samples();
    for (dst, px) in src
        .data_mut()
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(buf.chunks_exact(channels))
    {
        let [r, g, b, a] = rgba(px);
        *dst = [mul(r, a), mul(g, a), mul(b, a), a];
    }
    let mut out = Pixmap::new(size, size).ok_or("bad size")?;
    let paint = PixmapPaint {
        quality: FilterQuality::Bicubic,
        ..PixmapPaint::default()
    };
    out.draw_pixmap(
        0,
        0,
        src.as_ref(),
        &paint,
        fit(w as f32, h as f32, size),
        None,
    );
    Ok(out)
}

fn mul(c: u8, a: u8) -> u8 {
    ((u16::from(c) * u16::from(a) + 127) / 255) as u8
}

/// tiny-skia premultiplied RGBA to premultiplied BGRA.
fn to_bitmap(pm: Pixmap) -> Bitmap {
    let (w, h) = (pm.width() as i32, pm.height() as i32);
    let mut pixels = pm.take();
    for p in pixels.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
    }
    Bitmap {
        width: w,
        height: h,
        pixels,
    }
}

/// Replaces every pixel's color with `tint`, keeping the shape's alpha
/// (scaled by the tint's own alpha).
pub fn apply_tint(b: &mut Bitmap, tint: [u8; 4]) {
    let [r, g, bl, ta] = tint;
    for p in b.pixels.as_chunks_mut::<4>().0 {
        let a = mul(p[3], ta);
        *p = [mul(bl, a), mul(g, a), mul(r, a), a];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    impl ImageCache {
        /// `get`, waiting for the worker if the icon is not ready yet.
        fn get_wait(
            &mut self,
            dir: &Path,
            src: &str,
            size: u32,
            tint: Option<[u8; 4]>,
        ) -> Option<Arc<Bitmap>> {
            self.get(dir, src, size, tint);
            assert!(self.settle(Duration::from_secs(20)), "icon worker stuck");
            self.get(dir, src, size, tint)
        }
    }

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
    }

    fn png_bytes(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut wr = enc.write_header().unwrap();
            let data: Vec<u8> = (0..w * h).flat_map(|_| rgba).collect();
            wr.write_image_data(&data).unwrap();
        }
        out
    }

    fn has_image(g: &usvg::Group) -> bool {
        g.children().iter().any(|n| match n {
            usvg::Node::Image(_) => true,
            usvg::Node::Group(g) => has_image(g),
            _ => false,
        })
    }

    fn max_alpha(b: &Bitmap) -> u8 {
        b.pixels.chunks(4).map(|p| p[3]).max().unwrap_or(0)
    }

    #[test]
    fn svg_icon_rasterizes_at_scale() {
        let mut c = ImageCache::new();
        let b = c.get_wait(&fixtures(), "logo.svg", 24, None).expect("svg");
        assert_eq!((b.width, b.height), (24, 24));
        assert_eq!(b.pixels.len(), 24 * 24 * 4);
        assert!(max_alpha(&b) > 0);
        let big = c.get_wait(&fixtures(), "logo.svg", 36, None).expect("svg");
        assert_eq!((big.width, big.height), (36, 36));
    }

    #[test]
    fn png_icon_is_scaled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), png_bytes(8, 8, [0, 0, 255, 255])).unwrap();
        let mut c = ImageCache::new();
        let b = c.get_wait(dir.path(), "a.png", 16, None).expect("png");
        assert_eq!((b.width, b.height), (16, 16));
        // BGRA: opaque blue in the middle.
        let mid = ((8 * 16 + 8) * 4) as usize;
        assert_eq!(&b.pixels[mid..mid + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn tint_replaces_color_keeps_alpha() {
        let mut c = ImageCache::new();
        let plain = c.get_wait(&fixtures(), "logo.svg", 24, None).expect("svg");
        let red = c
            .get_wait(&fixtures(), "logo.svg", 24, Some([255, 0, 0, 255]))
            .expect("tinted");
        assert_eq!(plain.pixels.len(), red.pixels.len());
        for (p, t) in plain.pixels.chunks(4).zip(red.pixels.chunks(4)) {
            assert_eq!(t[3], p[3], "alpha kept");
            // Premultiplied BGRA red: B = G = 0, R = A.
            assert_eq!((t[0], t[1], t[2]), (0, 0, t[3]));
        }
    }

    #[test]
    fn icon_outside_widget_dir_rejected() {
        let root = tempfile::tempdir().unwrap();
        let widget = root.path().join("widget");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&widget).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::copy(fixtures().join("logo.svg"), outside.join("evil.svg")).unwrap();
        std::fs::copy(fixtures().join("logo.svg"), widget.join("ok.svg")).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(widget.join("link"))
            .arg(&outside)
            .output()
            .unwrap();
        assert!(status.status.success(), "mklink /J failed: {status:?}");
        assert!(widget.join("link").join("evil.svg").exists());

        let mut c = ImageCache::new();
        assert!(c.get_wait(&widget, "ok.svg", 16, None).is_some());
        assert!(c.get_wait(&outside, "evil.svg", 16, None).is_some());
        assert!(c.get_wait(&widget, "link/evil.svg", 16, None).is_none());
        assert!(c.get_wait(&widget, "link\\evil.svg", 16, None).is_none());
        assert!(c
            .get_wait(&widget, "../outside/evil.svg", 16, None)
            .is_none());
    }

    #[test]
    fn colon_in_src_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(fixtures().join("logo.svg"), dir.path().join("a.svg")).unwrap();
        let mut c = ImageCache::new();
        assert!(c.get_wait(dir.path(), "a.svg", 16, None).is_some());
        assert!(c.get_wait(dir.path(), "a.svg:x", 16, None).is_none());
        assert!(c.get_wait(dir.path(), "a.svg::$DATA", 16, None).is_none());
        assert!(c
            .get_wait(dir.path(), "data:image/svg+xml,<svg/>", 16, None)
            .is_none());
        let abs = dir.path().join("a.svg");
        assert!(c
            .get_wait(dir.path(), abs.to_str().unwrap(), 16, None)
            .is_none());
    }

    #[test]
    fn oversized_icon_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = png_bytes(4, 4, [255, 0, 0, 255]);
        bytes.resize(300 * 1024, 0);
        std::fs::write(dir.path().join("big.png"), &bytes).unwrap();
        let mut c = ImageCache::new();
        assert!(c.get_wait(dir.path(), "big.png", 16, None).is_none());
    }

    #[test]
    fn missing_or_bad_image_is_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.svg"), b"not svg").unwrap();
        std::fs::write(dir.path().join("bad.png"), b"not png").unwrap();
        let mut c = ImageCache::new();
        assert!(c.get_wait(dir.path(), "missing.svg", 16, None).is_none());
        assert!(c.get_wait(dir.path(), "bad.svg", 16, None).is_none());
        assert!(c.get_wait(dir.path(), "bad.png", 16, None).is_none());
    }

    #[test]
    fn deeply_nested_svg_is_rejected_without_crashing() {
        // A stack overflow on the icon worker would abort the whole test
        // binary, so passing proves its stack is safe.
        let depth = 1000;
        let mut svg = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 4 4">"#);
        svg.push_str(&r#"<g opacity=".9">"#.repeat(depth));
        svg.push_str(r#"<rect width="4" height="4"/>"#);
        svg.push_str(&"</g>".repeat(depth));
        svg.push_str("</svg>");
        assert!(svg.len() < MAX_ICON_BYTES as usize);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("deep.svg"), svg).unwrap();
        let mut c = ImageCache::new();
        if let Some(b) = c.get_wait(dir.path(), "deep.svg", 16, None) {
            assert_eq!((b.width, b.height), (16, 16));
        }
    }

    #[test]
    fn use_fan_out_svg_finishes() {
        // Each level references the previous one ten times: 10^8 rects if
        // expanded naively.
        let mut svg = String::from(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 4 4"><defs><rect id="l0" width="1" height="1"/>"#,
        );
        for level in 1..=8 {
            svg.push_str(&format!(r#"<g id="l{level}">"#));
            for _ in 0..10 {
                svg.push_str(&format!(r##"<use xlink:href="#l{}"/>"##, level - 1));
            }
            svg.push_str("</g>");
        }
        svg.push_str(r##"</defs><use xlink:href="#l8"/></svg>"##);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fan.svg"), svg).unwrap();
        let start = std::time::Instant::now();
        let mut c = ImageCache::new();
        assert!(c.get_wait(dir.path(), "fan.svg", 16, None).is_none());
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn expanded_element_count() {
        let logo = std::fs::read(fixtures().join("logo.svg")).unwrap();
        assert_eq!(expanded_elements(&logo), Some(3));
        let used = br##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><g id="a"><rect/><rect/></g><use href="#a"/><use xlink:href="#a"/></svg>"##;
        // svg, the g subtree (3), and two uses of 1 + 3 each.
        assert_eq!(expanded_elements(used), Some(12));
        let cycle =
            br##"<svg xmlns="http://www.w3.org/2000/svg"><g id="a"><use href="#a"/></g></svg>"##;
        assert_eq!(expanded_elements(cycle), Some(3));
        assert_eq!(expanded_elements(b"not xml"), None);
    }

    #[test]
    fn svg_external_image_is_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pic.png"),
            png_bytes(4, 4, [255, 0, 0, 255]),
        )
        .unwrap();
        let abs = dir.path().join("pic.png");
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";
        for href in [
            abs.display().to_string(),
            "pic.png".to_string(),
            format!("data:image/png;base64,{b64}"),
        ] {
            let svg = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 4 4"><image width="4" height="4" xlink:href="{href}"/></svg>"#
            );
            let tree = svg_tree(svg.as_bytes()).expect("svg parses");
            assert!(!has_image(tree.root()), "image {href} must not be loaded");
        }
    }

    /// A rasterizer that counts its calls and fails when `fail` is set.
    fn counting(fail: bool) -> (Rasterizer, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let r: Rasterizer = Arc::new(move |_path: &Path, size: u32| {
            c.fetch_add(1, Ordering::SeqCst);
            if fail {
                return Err(RasterError::Failed("fake failure".into()));
            }
            let n = size as usize;
            Ok(Bitmap {
                width: size as i32,
                height: size as i32,
                pixels: [0u8, 0, 0, 255].repeat(n * n),
            })
        });
        (r, calls)
    }

    #[test]
    fn tint_change_does_not_rasterize_again() {
        let (r, calls) = counting(false);
        let mut c = ImageCache::with(r, Arc::new(|| {}));
        let plain = c
            .get_wait(&fixtures(), "logo.svg", 24, None)
            .expect("plain");
        let red = c
            .get(&fixtures(), "logo.svg", 24, Some([255, 0, 0, 255]))
            .expect("tinted copy is ready at once");
        let blue = c
            .get(&fixtures(), "logo.svg", 24, Some([0, 0, 255, 255]))
            .expect("tinted copy is ready at once");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(&red.pixels[..4], &[0, 0, 255, 255]);
        assert_eq!(&blue.pixels[..4], &[255, 0, 0, 255]);
        // The cached raster itself is untouched by tinting.
        assert_eq!(&plain.pixels[..4], &[0, 0, 0, 255]);
        let again = c.get(&fixtures(), "logo.svg", 24, None).unwrap();
        assert!(Arc::ptr_eq(&plain, &again));
    }

    #[test]
    fn failure_is_cached_and_not_retried() {
        let (r, calls) = counting(true);
        let mut c = ImageCache::with(r, Arc::new(|| {}));
        assert!(c.get_wait(&fixtures(), "logo.svg", 24, None).is_none());
        assert!(c.get(&fixtures(), "logo.svg", 24, None).is_none());
        // A publish makes it re-resolve; the same file is still known bad.
        c.invalidate();
        assert!(c.get_wait(&fixtures(), "logo.svg", 24, None).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn get_never_waits_and_notify_fires_when_ready() {
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let gate = Mutex::new(gate_rx);
        let r: Rasterizer = Arc::new(move |_: &Path, size: u32| {
            // Blocks until the test lets it finish.
            let _ = gate.lock().unwrap().recv();
            Ok(Bitmap {
                width: size as i32,
                height: size as i32,
                pixels: vec![0; (size * size * 4) as usize],
            })
        });
        let notified = Arc::new(AtomicUsize::new(0));
        let n = notified.clone();
        let mut c = ImageCache::with(
            r,
            Arc::new(move || {
                n.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let t0 = Instant::now();
        for _ in 0..5 {
            // Pending: the placeholder is drawn, and no duplicate requests.
            assert!(c.get(&fixtures(), "logo.svg", 16, None).is_none());
        }
        assert!(t0.elapsed() < Duration::from_secs(1));
        assert_eq!(c.pending.len(), 1);
        gate_tx.send(()).unwrap();
        assert!(c.settle(Duration::from_secs(20)));
        assert_eq!(notified.load(Ordering::SeqCst), 1);
        assert!(c.get(&fixtures(), "logo.svg", 16, None).is_some());
    }

    fn key(n: u32) -> RasterKey {
        (
            PathBuf::from(format!("f{n}.svg")),
            SystemTime::UNIX_EPOCH,
            1,
            16,
        )
    }

    fn tiny() -> Option<Arc<Bitmap>> {
        Some(Arc::new(Bitmap {
            width: 1,
            height: 1,
            pixels: vec![0; 4],
        }))
    }

    #[test]
    fn raster_cache_evicts_least_recently_used_and_keeps_failures() {
        let mut rc = RasterCache::default();
        rc.insert(key(9999), None);
        for n in 0..MAX_ENTRIES as u32 {
            rc.insert(key(n), tiny());
        }
        // Touch the oldest so it is recent.
        assert!(rc.get(&key(0)).is_some());
        rc.insert(key(5000), tiny());
        assert_eq!(rc.ok.len(), MAX_ENTRIES);
        assert!(rc.get(&key(0)).is_some(), "recently used kept");
        assert!(rc.get(&key(1)).is_none(), "least recently used evicted");
        assert!(rc.get(&key(5000)).is_some());
        for n in 6000..6000 + 2 * MAX_ENTRIES as u32 {
            rc.insert(key(n), tiny());
        }
        assert!(matches!(rc.get(&key(9999)), Some(None)), "failure survives");
    }

    #[test]
    fn no_new_thread_while_a_timed_out_one_still_runs() {
        static TEST_SLOT: Slot = Slot::new();
        let started = Arc::new(AtomicUsize::new(0));
        let (s1, s2, s3) = (started.clone(), started.clone(), started.clone());
        let slow = run_limited(
            &TEST_SLOT,
            Duration::ZERO,
            Duration::from_millis(50),
            move || {
                s1.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(600));
                Ok(())
            },
        );
        assert_eq!(slow, Err(RasterError::TimedOut));
        // The timed-out thread still holds the slot: no second thread.
        let second = run_limited(
            &TEST_SLOT,
            Duration::from_millis(50),
            Duration::from_secs(5),
            move || {
                s2.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );
        assert_eq!(second, Err(RasterError::Busy));
        assert_eq!(started.load(Ordering::SeqCst), 1);
        // Once it ends, the next job runs.
        let third = run_limited(
            &TEST_SLOT,
            Duration::from_secs(5),
            Duration::from_secs(5),
            move || {
                s3.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );
        assert_eq!(third, Ok(()));
        assert_eq!(started.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn timed_out_src_stays_failed_after_the_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("slow.svg");
        std::fs::copy(fixtures().join("logo.svg"), &file).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let r: Rasterizer = Arc::new(move |_: &Path, _: u32| {
            c.fetch_add(1, Ordering::SeqCst);
            Err(RasterError::TimedOut)
        });
        let mut cache = ImageCache::with(r, Arc::new(|| {}));
        assert!(cache.get_wait(dir.path(), "slow.svg", 16, None).is_none());
        // New contents, size and mtime: a new raster key.
        let mut bigger = std::fs::read(&file).unwrap();
        bigger.extend_from_slice(b"<!-- edited -->");
        std::fs::write(&file, bigger).unwrap();
        let later = SystemTime::now() + Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(later)
            .unwrap();
        cache.invalidate();
        assert!(cache.get_wait(dir.path(), "slow.svg", 16, None).is_none());
        // Another size is a new key too, and still not tried.
        assert!(cache.get_wait(dir.path(), "slow.svg", 24, None).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn busy_is_not_cached_as_a_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let r: Rasterizer = Arc::new(move |_: &Path, size: u32| {
            if c.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(RasterError::Busy);
            }
            Ok(Bitmap {
                width: size as i32,
                height: size as i32,
                pixels: vec![0; (size * size * 4) as usize],
            })
        });
        let mut cache = ImageCache::with(r, Arc::new(|| {}));
        assert!(cache.get_wait(&fixtures(), "logo.svg", 16, None).is_none());
        cache.invalidate();
        assert!(cache.get_wait(&fixtures(), "logo.svg", 16, None).is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
