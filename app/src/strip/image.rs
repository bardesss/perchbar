//! Custom widget icons (SVG or PNG files inside the widget folder).

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use resvg::tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};
use resvg::usvg;

use crate::strip::render::Bitmap;

/// Largest icon file accepted.
pub const MAX_ICON_BYTES: u64 = 256 * 1024;
/// Largest decoded PNG side, in pixels.
const MAX_PNG_SIDE: u32 = 1024;
/// Decoder allocation cap for PNGs (guards against decompression bombs).
const MAX_PNG_DECODE_BYTES: usize = 8 * 1024 * 1024;
/// Entries kept before the cache is emptied (old mtimes pile up otherwise).
const MAX_ENTRIES: usize = 256;

type Key = (PathBuf, SystemTime, u32, Option<[u8; 4]>);

/// Rasterized custom icons, keyed by (canonical path, mtime, size, tint).
#[derive(Default)]
pub struct ImageCache {
    entries: HashMap<Key, Option<Arc<Bitmap>>>,
    /// Problems already logged, so a broken icon is reported once.
    logged: HashSet<String>,
}

impl ImageCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The icon `src` from `widget_dir`, scaled to `size_px` square and
    /// optionally tinted. `None` means the caller draws the fallback dot.
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
        let path = match resolve(widget_dir, src) {
            Ok(p) => p,
            Err(why) => {
                self.log_once(widget_dir, src, &why);
                return None;
            }
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
        let key = (path, mtime, size_px, tint);
        if let Some(hit) = self.entries.get(&key) {
            return hit.clone();
        }
        let made = load(&key.0, size_px).map(|mut b| {
            if let Some(t) = tint {
                apply_tint(&mut b, t);
            }
            Arc::new(b)
        });
        let made = match made {
            Ok(b) => Some(b),
            Err(why) => {
                self.log_once(widget_dir, src, &why);
                None
            }
        };
        if self.entries.len() >= MAX_ENTRIES {
            self.entries.clear();
        }
        self.entries.insert(key, made.clone());
        made
    }

    fn log_once(&mut self, widget_dir: &Path, src: &str, why: &str) {
        let id = format!("{}|{src}|{why}", widget_dir.display());
        if self.logged.insert(id) {
            log::warn!(
                "icon {src:?} in {}: {why}; drawing a dot instead",
                widget_dir.display()
            );
        }
    }
}

/// Resolves `src` inside `widget_dir`, following links, and refuses anything
/// that ends up outside the folder or is too big.
fn resolve(widget_dir: &Path, src: &str) -> Result<PathBuf, String> {
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
    Ok(file)
}

fn load(path: &Path, size_px: u32) -> Result<Bitmap, String> {
    let mut data = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_ICON_BYTES + 1).read_to_end(&mut data))
        .map_err(|e| e.to_string())?;
    if data.len() as u64 > MAX_ICON_BYTES {
        return Err("larger than 256 KB".into());
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let pixmap = match ext.as_deref() {
        Some("svg") => rasterize_svg(&data, size_px)?,
        Some("png") => scale_png(&data, size_px)?,
        _ => return Err("not .svg or .png".into()),
    };
    Ok(to_bitmap(pixmap))
}

/// Parses an SVG without touching the file system or network: no resources
/// folder, every `<image>` reference refused, and no font support compiled in.
fn svg_tree(data: &[u8]) -> Option<usvg::Tree> {
    let opt = usvg::Options {
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..usvg::Options::default()
    };
    usvg::Tree::from_data(data, &opt).ok()
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
        let b = c.get(&fixtures(), "logo.svg", 24, None).expect("svg");
        assert_eq!((b.width, b.height), (24, 24));
        assert_eq!(b.pixels.len(), 24 * 24 * 4);
        assert!(max_alpha(&b) > 0);
        let big = c.get(&fixtures(), "logo.svg", 36, None).expect("svg");
        assert_eq!((big.width, big.height), (36, 36));
    }

    #[test]
    fn png_icon_is_scaled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), png_bytes(8, 8, [0, 0, 255, 255])).unwrap();
        let mut c = ImageCache::new();
        let b = c.get(dir.path(), "a.png", 16, None).expect("png");
        assert_eq!((b.width, b.height), (16, 16));
        // BGRA: opaque blue in the middle.
        let mid = ((8 * 16 + 8) * 4) as usize;
        assert_eq!(&b.pixels[mid..mid + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn tint_replaces_color_keeps_alpha() {
        let mut c = ImageCache::new();
        let plain = c.get(&fixtures(), "logo.svg", 24, None).expect("svg");
        let red = c
            .get(&fixtures(), "logo.svg", 24, Some([255, 0, 0, 255]))
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
        assert!(c.get(&widget, "ok.svg", 16, None).is_some());
        assert!(c.get(&outside, "evil.svg", 16, None).is_some());
        assert!(c.get(&widget, "link/evil.svg", 16, None).is_none());
        assert!(c.get(&widget, "link\\evil.svg", 16, None).is_none());
        assert!(c.get(&widget, "../outside/evil.svg", 16, None).is_none());
    }

    #[test]
    fn colon_in_src_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(fixtures().join("logo.svg"), dir.path().join("a.svg")).unwrap();
        let mut c = ImageCache::new();
        assert!(c.get(dir.path(), "a.svg", 16, None).is_some());
        assert!(c.get(dir.path(), "a.svg:x", 16, None).is_none());
        assert!(c.get(dir.path(), "a.svg::$DATA", 16, None).is_none());
        assert!(c
            .get(dir.path(), "data:image/svg+xml,<svg/>", 16, None)
            .is_none());
        let abs = dir.path().join("a.svg");
        assert!(c.get(dir.path(), abs.to_str().unwrap(), 16, None).is_none());
    }

    #[test]
    fn oversized_icon_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = png_bytes(4, 4, [255, 0, 0, 255]);
        bytes.resize(300 * 1024, 0);
        std::fs::write(dir.path().join("big.png"), &bytes).unwrap();
        let mut c = ImageCache::new();
        assert!(c.get(dir.path(), "big.png", 16, None).is_none());
    }

    #[test]
    fn missing_or_bad_image_is_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.svg"), b"not svg").unwrap();
        std::fs::write(dir.path().join("bad.png"), b"not png").unwrap();
        let mut c = ImageCache::new();
        assert!(c.get(dir.path(), "missing.svg", 16, None).is_none());
        assert!(c.get(dir.path(), "bad.svg", 16, None).is_none());
        assert!(c.get(dir.path(), "bad.png", 16, None).is_none());
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
}
