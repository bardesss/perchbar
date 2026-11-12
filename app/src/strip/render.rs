//! Draws a [`StripLayout`] into a premultiplied BGRA bitmap with Direct2D.
//!
//! The target is a 32-bit top-down DIB section bound to a software
//! `ID2D1DCRenderTarget` with premultiplied alpha, which is what
//! `UpdateLayeredWindow` wants. Text uses grayscale antialiasing because
//! ClearType needs an opaque background. Everything is drawn in DIPs; the
//! target's DPI is set to `96 * scale`.

use std::path::Path;

use windows::core::{w, HSTRING};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_OPEN,
    D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1RenderTarget, ID2D1SolidColorBrush,
    ID2D1StrokeStyle, D2D1_ANTIALIAS_MODE_ALIASED, D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE,
    D2D1_ARC_SIZE_SMALL, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES,
    D2D1_CAP_STYLE_ROUND, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT, D2D1_ELLIPSE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_LINE_JOIN_ROUND,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES, D2D1_SWEEP_DIRECTION_CLOCKWISE,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection, IDWriteTextFormat,
    IDWriteTextLayout, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_METRICS, DWRITE_TEXT_RANGE, DWRITE_TRIMMING,
    DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows_numerics::Vector2;

use crate::display::{Block, Color, Output, TextSize};
use crate::strip::icons;
use crate::strip::layout::{
    block_width, px, Measure, Metrics, PlacedSlot, StripLayout, BAR_HEIGHT_DIP, BLOCK_GAP_DIP,
    ICON_DIP, NORMAL_TEXT_DIP, RING_DIP, SLOT_PAD_DIP, SMALL_TEXT_DIP, SPARK_HEIGHT_DIP,
};
use crate::strip::model::{SlotKind, SlotState, StripModel};
use crate::strip::theme::{resolve, Palette};

pub use crate::strip::image::ImageCache;

/// Premultiplied BGRA pixels, top-down rows, `width * 4` bytes per row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u8>,
}

/// Opacity of stale output and of the last output behind an error.
const STALE_OPACITY: f32 = 0.6;
/// Stack line centers as fractions of the strip height.
const STACK_TOP: f32 = 0.30;
const STACK_BOTTOM: f32 = 0.72;
const HOVER_RADIUS_DIP: f32 = 4.0;
/// Vertical inset of the hover background, like taskbar buttons.
const HOVER_INSET_DIP: f32 = 4.0;
const DOT_DIP: f32 = 4.0;
const ROTATE_DOT_DIP: f32 = 2.0;
const ROTATE_DOT_STEP_DIP: f32 = 5.0;
/// Center of the rotation dots, measured up from the bottom edge.
const ROTATE_DOTS_FROM_BOTTOM_DIP: f32 = 5.0;
const RING_STROKE_DIP: f32 = 2.0;
const SPARK_STROKE_DIP: f32 = 1.5;
/// Glyph size for built-in icons (the box is `ICON_DIP` square).
const ICON_GLYPH_DIP: f32 = 16.0;
/// Size of the warning badge over a slot that has an error and old output.
const BADGE_DIP: f32 = 10.0;
/// Slack added to a text box so rounding never triggers the ellipsis.
const TEXT_SLACK_DIP: f32 = 0.5;

/// Unbounded box for measuring.
const MEASURE_BOX_DIP: f32 = 100_000.0;

const TEXT_FONTS: &[&str] = &["Segoe UI Variable Text", "Segoe UI"];

/// The output to show for an instance, and whether it is out of date
/// (`Stale`, or an error's leftover output).
pub fn output_for<'a>(model: &'a StripModel, instance_id: &str) -> Option<(&'a Output, bool)> {
    let state = model.slots.iter().find_map(|s| match &s.kind {
        SlotKind::Single {
            instance_id: id,
            state,
            ..
        } if id == instance_id => Some(state),
        SlotKind::Group { members, .. } => members
            .iter()
            .find(|m| m.instance_id == instance_id)
            .map(|m| &m.state),
        SlotKind::Single { .. } => None,
    })?;
    match state {
        SlotState::Loading => None,
        SlotState::Ok(o) => Some((o, false)),
        SlotState::Stale(o) => Some((o, true)),
        SlotState::Error { last, .. } => last.as_ref().map(|o| (o, true)),
    }
}

/// (dot count, active index) for a rotating slot.
fn rotation_dots(model: &StripModel, slot: &PlacedSlot) -> Option<(usize, usize)> {
    let frame = slot.frame.as_ref();
    if let Some((out, _)) = frame.and_then(|f| output_for(model, &f.instance_id)) {
        if out.frames.len() > 1 {
            return Some((out.frames.len(), frame?.frame_index));
        }
    }
    let members = model.slots.iter().find_map(|s| match &s.kind {
        SlotKind::Group { members, .. } if s.id == slot.slot_id => Some(members),
        _ => None,
    })?;
    let active = frame
        .and_then(|f| members.iter().position(|m| m.instance_id == f.instance_id))
        .unwrap_or(0);
    (members.len() > 1).then_some((members.len(), active))
}

/// A memory DC with a 32-bit top-down DIB section selected into it.
struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    len: usize,
}

impl Dib {
    fn new(w: i32, h: i32) -> windows::core::Result<Self> {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: plain GDI calls; every handle is released in Drop, and
        // the DC is deleted here if the section cannot be created.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err(windows::core::Error::from_thread());
            }
            let mut bits = std::ptr::null_mut();
            let bmp = match CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(b) if !bits.is_null() => b,
                Ok(b) => {
                    let _ = DeleteObject(b.into());
                    let _ = DeleteDC(dc);
                    return Err(windows::core::Error::from_thread());
                }
                Err(e) => {
                    let _ = DeleteDC(dc);
                    return Err(e);
                }
            };
            let old = SelectObject(dc, bmp.into());
            Ok(Dib {
                dc,
                bmp,
                old,
                bits: bits.cast(),
                len: (w as usize) * (h as usize) * 4,
            })
        }
    }

    fn pixels(&self) -> Vec<u8> {
        // SAFETY: the section is `len` bytes, alive while `self` is, and GDI
        // has flushed (GdiFlush) before this is called.
        unsafe { std::slice::from_raw_parts(self.bits, self.len).to_vec() }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        // SAFETY: handles were created in `new` and are released once.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

fn color_f(c: [u8; 4], opacity: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: f32::from(c[0]) / 255.0,
        g: f32::from(c[1]) / 255.0,
        b: f32::from(c[2]) / 255.0,
        a: f32::from(c[3]) / 255.0 * opacity,
    }
}

fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left,
        top,
        right,
        bottom,
    }
}

fn v2(x: f32, y: f32) -> Vector2 {
    Vector2 { X: x, Y: y }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

pub struct Renderer {
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    normal: IDWriteTextFormat,
    small: IDWriteTextFormat,
    icon: IDWriteTextFormat,
    round: ID2D1StrokeStyle,
    target: Option<ID2D1DCRenderTarget>,
    images: ImageCache,
}

/// Per-draw state: target, palette, strip height and current opacity.
struct Ctx<'a> {
    rt: &'a ID2D1RenderTarget,
    pal: &'a Palette,
    height: f32,
    scale: f32,
    opacity: f32,
}

impl Ctx<'_> {
    fn brush(&self, c: [u8; 4]) -> windows::core::Result<ID2D1SolidColorBrush> {
        // SAFETY: valid render target inside BeginDraw/EndDraw.
        unsafe {
            self.rt
                .CreateSolidColorBrush(&color_f(c, self.opacity), None)
        }
    }

    fn dot(&self, cx: f32, cy: f32, d: f32, c: [u8; 4]) -> windows::core::Result<()> {
        let b = self.brush(c)?;
        let e = D2D1_ELLIPSE {
            point: v2(cx, cy),
            radiusX: d / 2.0,
            radiusY: d / 2.0,
        };
        // SAFETY: as above.
        unsafe { self.rt.FillEllipse(&e, &b) };
        Ok(())
    }
}

fn first_family(fonts: &IDWriteFontCollection, names: &[&str]) -> HSTRING {
    for name in names {
        let h = HSTRING::from(*name);
        let mut index = 0;
        let mut exists = windows::core::BOOL::default();
        // SAFETY: out pointers are valid locals.
        let ok = unsafe { fonts.FindFamilyName(&h, &mut index, &mut exists) };
        if ok.is_ok() && exists.as_bool() {
            return h;
        }
    }
    // DirectWrite falls back on its own if even the last name is missing.
    HSTRING::from(*names.last().unwrap_or(&"Segoe UI"))
}

fn text_format(
    dw: &IDWriteFactory,
    family: &HSTRING,
    size: f32,
) -> windows::core::Result<IDWriteTextFormat> {
    // SAFETY: plain DirectWrite factory calls with valid arguments.
    unsafe {
        let f = dw.CreateTextFormat(
            family,
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("en-us"),
        )?;
        f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        let sign = dw.CreateEllipsisTrimmingSign(&f)?;
        let trim = DWRITE_TRIMMING {
            granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
            delimiter: 0,
            delimiterCount: 0,
        };
        f.SetTrimming(&trim, &sign)?;
        f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        Ok(f)
    }
}

impl Renderer {
    /// A renderer whose custom icons are rasterized without notification
    /// (offline renders and tests call `settle_icons`).
    pub fn new() -> windows::core::Result<Self> {
        Self::with_icon_notify(std::sync::Arc::new(|| {}))
    }

    /// A renderer that calls `notify` from the icon worker thread whenever
    /// a custom icon finished rasterizing; the strip then redraws.
    pub fn with_icon_notify(notify: crate::strip::image::Notify) -> windows::core::Result<Self> {
        // SAFETY: factory creation and font lookups with valid arguments.
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let mut fonts = None;
            dwrite.GetSystemFontCollection(&mut fonts, false)?;
            let fonts = fonts.ok_or_else(windows::core::Error::empty)?;
            let text = first_family(&fonts, TEXT_FONTS);
            let icon = first_family(&fonts, &[icons::ICON_FONT, icons::ICON_FONT_FALLBACK]);
            let round = d2d.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    ..Default::default()
                },
                None,
            )?;
            Ok(Renderer {
                normal: text_format(&dwrite, &text, NORMAL_TEXT_DIP)?,
                small: text_format(&dwrite, &text, SMALL_TEXT_DIP)?,
                icon: text_format(&dwrite, &icon, ICON_GLYPH_DIP)?,
                d2d,
                dwrite,
                round,
                target: None,
                images: ImageCache::with_notify(notify),
            })
        }
    }

    /// Takes finished icons; true when a redraw would show something new.
    pub fn poll_icons(&mut self) -> bool {
        self.images.poll()
    }

    /// Re-resolves every icon on its next draw, off the strip thread.
    pub fn invalidate_icons(&mut self) {
        self.images.invalidate();
    }

    /// Waits for pending icons. Never call this on the strip thread.
    pub fn settle_icons(&mut self, timeout: std::time::Duration) -> bool {
        self.images.settle(timeout)
    }

    fn format(&self, size: TextSize) -> &IDWriteTextFormat {
        match size {
            TextSize::Normal => &self.normal,
            TextSize::Small => &self.small,
        }
    }

    fn text_layout(
        &self,
        text: &str,
        format: &IDWriteTextFormat,
        max_w: f32,
        max_h: f32,
    ) -> windows::core::Result<IDWriteTextLayout> {
        // SAFETY: valid factory and format.
        unsafe {
            self.dwrite
                .CreateTextLayout(&wide(text), format, max_w.max(0.0), max_h.max(0.0))
        }
    }

    fn dc_target(&mut self) -> windows::core::Result<ID2D1DCRenderTarget> {
        if let Some(t) = &self.target {
            return Ok(t.clone());
        }
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 0.0,
            dpiY: 0.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        // SAFETY: valid factory and properties.
        let t = unsafe { self.d2d.CreateDCRenderTarget(&props)? };
        self.target = Some(t.clone());
        Ok(t)
    }

    pub fn render(
        &mut self,
        layout: &StripLayout,
        model: &StripModel,
        metrics: &Metrics,
        palette: &Palette,
        hover: Option<&str>,
    ) -> windows::core::Result<Bitmap> {
        let w = px(layout.width_dip, metrics.scale);
        let h = px(metrics.height_dip, metrics.scale);
        if w <= 0 || h <= 0 {
            return Ok(Bitmap {
                width: w.max(0),
                height: h.max(0),
                pixels: Vec::new(),
            });
        }
        let dib = Dib::new(w, h)?;
        let target = self.dc_target()?;
        let dpi = 96.0 * metrics.scale;
        // SAFETY: the DIB outlives the draw; BeginDraw is always paired with
        // EndDraw below.
        let drawn = unsafe {
            target.BindDC(
                dib.dc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                },
            )?;
            target.SetDpi(dpi, dpi);
            target.BeginDraw();
            target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            target.Clear(Some(&color_f([0, 0, 0, 0], 1.0)));
            let ctx = Ctx {
                rt: &target,
                pal: palette,
                height: metrics.height_dip,
                scale: metrics.scale,
                opacity: 1.0,
            };
            let drawn = self.draw_strip(&ctx, layout, model, hover);
            let ended = target.EndDraw(None, None);
            if ended.is_err() {
                // Recreate the target next time (D2DERR_RECREATE_TARGET).
                self.target = None;
            }
            drawn.and(ended)
        };
        drawn?;
        // SAFETY: flushes pending GDI work on this thread.
        unsafe {
            let _ = GdiFlush();
        }
        Ok(Bitmap {
            width: w,
            height: h,
            pixels: dib.pixels(),
        })
    }

    fn draw_strip(
        &mut self,
        ctx: &Ctx,
        layout: &StripLayout,
        model: &StripModel,
        hover: Option<&str>,
    ) -> windows::core::Result<()> {
        for slot in &layout.items {
            let clip = rect(slot.x_dip, 0.0, slot.x_dip + slot.width_dip, ctx.height);
            // SAFETY: inside BeginDraw; popped below on every path.
            unsafe {
                ctx.rt
                    .PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED)
            };
            let r = self.draw_slot(ctx, slot, model, hover == Some(slot.slot_id.as_str()));
            // SAFETY: matches the push above.
            unsafe { ctx.rt.PopAxisAlignedClip() };
            r?;
        }
        if let Some(plus) = &layout.overflow_item {
            let lay = self.text_layout(&plus.text, &self.normal, plus.width_dip, ctx.height)?;
            // SAFETY: valid layout.
            unsafe { lay.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)? };
            let b = ctx.brush(ctx.pal.muted)?;
            // SAFETY: inside BeginDraw.
            unsafe {
                ctx.rt.DrawTextLayout(
                    v2(plus.x_dip, 0.0),
                    &lay,
                    &b,
                    D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                )
            };
        }
        Ok(())
    }

    fn draw_slot(
        &mut self,
        ctx: &Ctx,
        slot: &PlacedSlot,
        model: &StripModel,
        hovered: bool,
    ) -> windows::core::Result<()> {
        let left = slot.x_dip;
        let right = slot.x_dip + slot.width_dip;
        let cy = ctx.height / 2.0;
        if hovered {
            let b = ctx.brush(ctx.pal.hover_bg)?;
            let rr = D2D1_ROUNDED_RECT {
                rect: rect(left, HOVER_INSET_DIP, right, ctx.height - HOVER_INSET_DIP),
                radiusX: HOVER_RADIUS_DIP,
                radiusY: HOVER_RADIUS_DIP,
            };
            // SAFETY: inside BeginDraw.
            unsafe { ctx.rt.FillRoundedRectangle(&rr, &b) };
        }
        let shown = slot.frame.as_ref().and_then(|f| {
            let (out, stale) = output_for(model, &f.instance_id)?;
            Some((out.frames.get(f.frame_index)?, stale, f))
        });
        match shown {
            Some((frame, stale, fr)) => {
                let dim = stale || slot.stale || slot.error.is_some();
                let c = Ctx {
                    opacity: if dim { STALE_OPACITY } else { 1.0 },
                    ..*ctx
                };
                let mut x = left + SLOT_PAD_DIP;
                let max_right = right - SLOT_PAD_DIP;
                for block in &frame.blocks {
                    if x >= max_right {
                        break;
                    }
                    self.draw_block(&c, block, x, cy, max_right, &fr.widget_dir)?;
                    x += block_width(block, self) + BLOCK_GAP_DIP;
                }
                if slot.error.is_some() {
                    let half = BADGE_DIP / 2.0;
                    let at = (right - half, HOVER_INSET_DIP + half);
                    self.draw_warning(ctx, BADGE_DIP, at)?;
                }
            }
            None if slot.error.is_some() => {
                self.draw_warning(ctx, ICON_GLYPH_DIP, ((left + right) / 2.0, cy))?;
            }
            None => ctx.dot((left + right) / 2.0, cy, DOT_DIP, ctx.pal.muted)?,
        }
        if slot.rotating {
            if let Some((n, active)) = rotation_dots(model, slot) {
                let span = (n - 1) as f32 * ROTATE_DOT_STEP_DIP;
                let x0 = (left + right) / 2.0 - span / 2.0;
                let y = ctx.height - ROTATE_DOTS_FROM_BOTTOM_DIP;
                for i in 0..n {
                    let c = Ctx {
                        opacity: if i == active { 1.0 } else { 0.5 },
                        ..*ctx
                    };
                    let x = x0 + i as f32 * ROTATE_DOT_STEP_DIP;
                    c.dot(x, y, ROTATE_DOT_DIP, ctx.pal.muted)?;
                }
            }
        }
        Ok(())
    }

    fn draw_warning(&self, ctx: &Ctx, size: f32, at: (f32, f32)) -> windows::core::Result<()> {
        let g = icons::codepoint("warning").map_or_else(|| "!".to_string(), String::from);
        self.draw_centered(ctx, &g, &self.icon, size, at, ctx.pal.warn)
    }

    /// A short string (an icon glyph) at `size` DIPs, centered on `at`.
    fn draw_centered(
        &self,
        ctx: &Ctx,
        s: &str,
        format: &IDWriteTextFormat,
        size: f32,
        (cx, cy): (f32, f32),
        color: [u8; 4],
    ) -> windows::core::Result<()> {
        let lay = self.text_layout(s, format, size * 2.0, size * 2.0)?;
        let b = ctx.brush(color)?;
        // SAFETY: valid layout; inside BeginDraw.
        unsafe {
            lay.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            lay.SetFontSize(
                size,
                DWRITE_TEXT_RANGE {
                    startPosition: 0,
                    length: s.encode_utf16().count() as u32,
                },
            )?;
            ctx.rt.DrawTextLayout(
                v2(cx - size, cy - size),
                &lay,
                &b,
                D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
            );
        }
        Ok(())
    }

    fn draw_text(
        &self,
        ctx: &Ctx,
        text: &str,
        size: TextSize,
        (x, cy): (f32, f32),
        max_w: f32,
        color: [u8; 4],
    ) -> windows::core::Result<()> {
        let format = self.format(size);
        let lay = self.text_layout(text, format, max_w + TEXT_SLACK_DIP, ctx.height)?;
        let b = ctx.brush(color)?;
        // SAFETY: valid layout; inside BeginDraw.
        unsafe {
            ctx.rt.DrawTextLayout(
                v2(x, cy - ctx.height / 2.0),
                &lay,
                &b,
                D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
            );
        }
        Ok(())
    }

    fn draw_block(
        &mut self,
        ctx: &Ctx,
        block: &Block,
        x: f32,
        cy: f32,
        max_right: f32,
        widget_dir: &Path,
    ) -> windows::core::Result<()> {
        let pal = ctx.pal;
        match block {
            Block::Text { value, color, size } => self.draw_text(
                ctx,
                value,
                *size,
                (x, cy),
                max_right - x,
                resolve(color, pal),
            ),
            Block::Icon {
                name,
                src,
                tint,
                color,
            } => {
                let c = resolve(color, pal);
                let cx = x + ICON_DIP / 2.0;
                if let Some(n) = name {
                    let at = (cx, cy);
                    if let Some(g) = icons::codepoint(n) {
                        let g = String::from(g);
                        return self.draw_centered(ctx, &g, &self.icon, ICON_GLYPH_DIP, at, c);
                    }
                    if let Some(t) = icons::text_fallback(n) {
                        return self.draw_centered(ctx, t, &self.normal, ICON_GLYPH_DIP, at, c);
                    }
                } else if let Some(src) = src {
                    let size = px(ICON_DIP, ctx.scale).max(1) as u32;
                    let tint = tint.then_some(c);
                    if let Some(bmp) = self.images.get(widget_dir, src, size, tint) {
                        return draw_bitmap(
                            ctx,
                            &bmp,
                            rect(x, cy - ICON_DIP / 2.0, x + ICON_DIP, cy + ICON_DIP / 2.0),
                        );
                    }
                }
                ctx.dot(cx, cy, DOT_DIP, pal.muted)
            }
            Block::Bar {
                value,
                color,
                width,
            } => {
                let w = width.map_or(crate::strip::layout::DEFAULT_BAR_WIDTH_DIP, |w| w as f32);
                let fill = bar_color(color, pal);
                let r = BAR_HEIGHT_DIP / 2.0;
                let track = D2D1_ROUNDED_RECT {
                    rect: rect(x, cy - r, x + w, cy + r),
                    radiusX: r,
                    radiusY: r,
                };
                let tb = ctx.brush(pal.bar_track)?;
                // SAFETY: inside BeginDraw.
                unsafe { ctx.rt.FillRoundedRectangle(&track, &tb) };
                let fw = w * (*value as f32).clamp(0.0, 1.0);
                if fw > 0.0 {
                    let fr = D2D1_ROUNDED_RECT {
                        rect: rect(x, cy - r, x + fw.max(BAR_HEIGHT_DIP), cy + r),
                        radiusX: r,
                        radiusY: r,
                    };
                    let fb = ctx.brush(fill)?;
                    // SAFETY: inside BeginDraw.
                    unsafe { ctx.rt.FillRoundedRectangle(&fr, &fb) };
                }
                Ok(())
            }
            Block::Ring { value, color } => self.draw_ring(
                ctx,
                *value as f32,
                x + RING_DIP / 2.0,
                cy,
                bar_color(color, pal),
            ),
            Block::Spark { values, color } => {
                self.draw_spark(ctx, values, x, cy, resolve(color, pal))
            }
            Block::Stack { top, bottom } => {
                self.draw_block(ctx, top, x, ctx.height * STACK_TOP, max_right, widget_dir)?;
                self.draw_block(
                    ctx,
                    bottom,
                    x,
                    ctx.height * STACK_BOTTOM,
                    max_right,
                    widget_dir,
                )
            }
        }
    }

    fn draw_ring(
        &self,
        ctx: &Ctx,
        value: f32,
        cx: f32,
        cy: f32,
        color: [u8; 4],
    ) -> windows::core::Result<()> {
        let r = (RING_DIP - RING_STROKE_DIP) / 2.0;
        let track = ctx.brush(ctx.pal.bar_track)?;
        let e = D2D1_ELLIPSE {
            point: v2(cx, cy),
            radiusX: r,
            radiusY: r,
        };
        // SAFETY: inside BeginDraw.
        unsafe { ctx.rt.DrawEllipse(&e, &track, RING_STROKE_DIP, None) };
        let value = value.clamp(0.0, 1.0);
        if value <= 0.0 {
            return Ok(());
        }
        let fill = ctx.brush(color)?;
        if value >= 0.999 {
            // SAFETY: inside BeginDraw.
            unsafe { ctx.rt.DrawEllipse(&e, &fill, RING_STROKE_DIP, None) };
            return Ok(());
        }
        // Clockwise from 12 o'clock.
        let angle = value * std::f32::consts::TAU;
        let end = v2(cx + r * angle.sin(), cy - r * angle.cos());
        // SAFETY: geometry built and closed before drawing.
        unsafe {
            let geo = self.d2d.CreatePathGeometry()?;
            let sink = geo.Open()?;
            sink.BeginFigure(v2(cx, cy - r), D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: end,
                size: D2D_SIZE_F {
                    width: r,
                    height: r,
                },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: if value > 0.5 {
                    D2D1_ARC_SIZE_LARGE
                } else {
                    D2D1_ARC_SIZE_SMALL
                },
            });
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
            sink.Close()?;
            ctx.rt
                .DrawGeometry(&geo, &fill, RING_STROKE_DIP, &self.round);
        }
        Ok(())
    }

    fn draw_spark(
        &self,
        ctx: &Ctx,
        values: &[f64],
        x: f32,
        cy: f32,
        color: [u8; 4],
    ) -> windows::core::Result<()> {
        if values.len() < 2 {
            return Ok(());
        }
        let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let span = if hi > lo { hi - lo } else { 1.0 };
        // Keep the stroke inside the box.
        let inset = SPARK_STROKE_DIP / 2.0;
        let top = cy - SPARK_HEIGHT_DIP / 2.0 + inset;
        let h = SPARK_HEIGHT_DIP - 2.0 * inset;
        let w = crate::strip::layout::SPARK_WIDTH_DIP - 2.0 * inset;
        let step = w / (values.len() - 1) as f32;
        let point = |i: usize, v: f64| {
            let t = if hi > lo {
                ((v - lo) / span) as f32
            } else {
                0.5
            };
            v2(x + inset + i as f32 * step, top + h * (1.0 - t))
        };
        let b = ctx.brush(color)?;
        // SAFETY: geometry built and closed before drawing.
        unsafe {
            let geo = self.d2d.CreatePathGeometry()?;
            let sink = geo.Open()?;
            sink.BeginFigure(point(0, values[0]), D2D1_FIGURE_BEGIN_HOLLOW);
            for (i, v) in values.iter().enumerate().skip(1) {
                sink.AddLine(point(i, *v));
            }
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
            sink.Close()?;
            ctx.rt.DrawGeometry(&geo, &b, SPARK_STROKE_DIP, &self.round);
        }
        Ok(())
    }
}

/// Bars and rings use the accent color unless the widget picks one.
fn bar_color(color: &Color, pal: &Palette) -> [u8; 4] {
    match color {
        Color::Default => pal.accent,
        c => resolve(c, pal),
    }
}

fn draw_bitmap(ctx: &Ctx, bmp: &Bitmap, dest: D2D_RECT_F) -> windows::core::Result<()> {
    let props = D2D1_BITMAP_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
    };
    // SAFETY: `pixels` holds width * height * 4 bytes with the given pitch.
    unsafe {
        let b = ctx.rt.CreateBitmap(
            D2D_SIZE_U {
                width: bmp.width as u32,
                height: bmp.height as u32,
            },
            Some(bmp.pixels.as_ptr().cast()),
            (bmp.width * 4) as u32,
            &props,
        )?;
        ctx.rt.DrawBitmap(
            &b,
            Some(&dest),
            ctx.opacity,
            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
            None,
        );
    }
    Ok(())
}

impl Measure for Renderer {
    fn text_width(&self, text: &str, size: TextSize) -> f32 {
        let measure = || -> windows::core::Result<f32> {
            let lay =
                self.text_layout(text, self.format(size), MEASURE_BOX_DIP, MEASURE_BOX_DIP)?;
            let mut m = DWRITE_TEXT_METRICS::default();
            // SAFETY: valid layout and out pointer.
            unsafe { lay.GetMetrics(&mut m)? };
            Ok(m.widthIncludingTrailingWhitespace)
        };
        measure().unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{Block, Color, Frame, Output};
    use crate::strip::layout::{layout, px, RotationState};
    use crate::strip::model::{Slot, SlotKind, SlotState};
    use crate::strip::theme;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn text_model(s: &str) -> StripModel {
        let out = Output {
            frames: vec![Frame {
                blocks: vec![Block::Text {
                    value: s.into(),
                    color: Color::Default,
                    size: TextSize::Normal,
                }],
                tooltip: None,
            }],
            rotate: None,
            click: None,
        };
        StripModel {
            slots: vec![Slot {
                id: "s1".into(),
                kind: SlotKind::Single {
                    instance_id: "s1".into(),
                    widget_dir: Arc::new(PathBuf::from("w")),
                    state: SlotState::Ok(out),
                    max_width: None,
                },
            }],
            generation: 1,
        }
    }

    #[test]
    fn render_smoke_nonempty_pixels() {
        let mut r = Renderer::new().expect("renderer");
        let model = text_model("BTC $67,120");
        let pal = theme::palette(true, [0, 120, 212, 255]);
        for scale in [1.0_f32, 1.5] {
            let m = Metrics {
                height_dip: 48.0,
                scale,
                available_dip: 500.0,
            };
            let l = layout(&model, &RotationState::default(), &m, &r);
            let b = r.render(&l, &model, &m, &pal, Some("s1")).expect("render");
            assert_eq!(b.height, px(48.0, scale));
            assert_eq!(b.width, px(l.width_dip, scale));
            assert_eq!(b.pixels.len(), (b.width * b.height * 4) as usize);
            assert!(b.pixels.chunks(4).any(|p| p[3] > 0), "scale {scale}");
            // Premultiplied: no channel exceeds alpha.
            assert!(b
                .pixels
                .chunks(4)
                .all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
        }
    }

    #[test]
    fn empty_layout_renders_empty_bitmap() {
        let mut r = Renderer::new().expect("renderer");
        let m = Metrics {
            height_dip: 48.0,
            scale: 1.0,
            available_dip: 500.0,
        };
        let pal = theme::palette(false, [0, 120, 212, 255]);
        let b = r
            .render(
                &StripLayout::default(),
                &StripModel::default(),
                &m,
                &pal,
                None,
            )
            .expect("render");
        assert_eq!(b.width, 0);
        assert!(b.pixels.is_empty());
    }

    #[test]
    fn measure_is_monotonic() {
        let r = Renderer::new().expect("renderer");
        let w1 = r.text_width("W", TextSize::Normal);
        let w2 = r.text_width("WW", TextSize::Normal);
        assert!(w1 > 0.0);
        assert!(w2 > w1);
        assert!(r.text_width("WW", TextSize::Small) < w2);
    }

    #[test]
    fn output_for_finds_members_and_staleness() {
        let mut model = text_model("x");
        assert!(matches!(output_for(&model, "s1"), Some((_, false))));
        assert!(output_for(&model, "nope").is_none());
        if let SlotKind::Single { state, .. } = &mut model.slots[0].kind {
            let o = match state {
                SlotState::Ok(o) => o.clone(),
                _ => unreachable!(),
            };
            *state = SlotState::Stale(o);
        }
        assert!(matches!(output_for(&model, "s1"), Some((_, true))));
    }
}
