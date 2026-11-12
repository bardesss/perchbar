//! Dev tool: renders a sample strip in dark and light mode and writes PNGs
//! composited on a taskbar-like background, to check glyphs and alignment.
//!
//! Usage: `cargo run --example render_png [-- <output dir>] [--scale 1.5]`
//! (the output dir defaults to the system temp dir).

#![allow(dead_code)]

#[path = "../src/display.rs"]
mod display;
/// The drawing half of `src/strip` (the window half needs the scheduler).
mod strip {
    #[path = "../../src/strip/icons.rs"]
    pub mod icons;
    #[path = "../../src/strip/image.rs"]
    pub mod image;
    #[path = "../../src/strip/layout.rs"]
    pub mod layout;
    #[path = "../../src/strip/model.rs"]
    pub mod model;
    #[path = "../../src/strip/render.rs"]
    pub mod render;
    #[path = "../../src/strip/theme.rs"]
    pub mod theme;
}

use std::path::{Path, PathBuf};
use std::sync::Arc;

use display::{Block, Color, Frame, Output, TextSize};
use strip::layout::{layout, Metrics, RotationState};
use strip::model::{Member, Slot, SlotKind, SlotState, StripModel};
use strip::render::{Bitmap, Renderer};
use strip::theme;

/// Approximate Windows 11 taskbar backgrounds.
const DARK_BG: [u8; 3] = [0x1C, 0x1C, 0x1C];
const LIGHT_BG: [u8; 3] = [0xEE, 0xEE, 0xEE];

fn text(v: &str, color: Color, size: TextSize) -> Block {
    Block::Text {
        value: v.into(),
        color,
        size,
    }
}

fn t(v: &str) -> Block {
    text(v, Color::Default, TextSize::Normal)
}

fn icon(name: &str, color: Color) -> Block {
    Block::Icon {
        name: Some(name.into()),
        src: None,
        tint: false,
        color,
    }
}

fn out(frames: Vec<Vec<Block>>) -> Output {
    Output {
        frames: frames
            .into_iter()
            .map(|blocks| Frame {
                blocks,
                tooltip: None,
            })
            .collect(),
        rotate: None,
        click: None,
    }
}

fn single(id: &str, dir: &Arc<PathBuf>, state: SlotState) -> Slot {
    Slot {
        id: id.into(),
        kind: SlotKind::Single {
            instance_id: id.into(),
            widget_dir: Arc::clone(dir),
            state,
            max_width: None,
        },
    }
}

fn sample(dir: &Arc<PathBuf>) -> StripModel {
    let ok = |blocks: Vec<Block>| SlotState::Ok(out(vec![blocks]));
    let gallery: Vec<Block> = [
        "bitcoin", "clock", "globe", "up", "down", "warning", "battery", "music", "mail", "sun",
        "cloud", "rain", "unknown",
    ]
    .iter()
    .map(|n| icon(n, Color::Default))
    .collect();
    let slots = vec![
        single(
            "btc",
            dir,
            ok(vec![
                icon("bitcoin", Color::Accent),
                t("$67,120"),
                icon("up", Color::Up),
                text("2.4%", Color::Up, TextSize::Normal),
            ]),
        ),
        single(
            "weather",
            dir,
            ok(vec![
                icon("sun", Color::Warn),
                Block::Stack {
                    top: Box::new(t("21\u{00B0}C")),
                    bottom: Box::new(text("Sunny", Color::Muted, TextSize::Small)),
                },
            ]),
        ),
        single(
            "cpu",
            dir,
            ok(vec![
                text("CPU", Color::Muted, TextSize::Small),
                Block::Bar {
                    value: 0.62,
                    color: Color::Default,
                    width: None,
                },
                Block::Ring {
                    value: 0.7,
                    color: Color::Down,
                },
                Block::Spark {
                    values: vec![3.0, 5.0, 4.0, 8.0, 6.0, 9.0, 7.0, 10.0],
                    color: Color::Up,
                },
            ]),
        ),
        single(
            "logo",
            dir,
            ok(vec![
                Block::Icon {
                    name: None,
                    src: Some("logo.svg".into()),
                    tint: false,
                    color: Color::Default,
                },
                Block::Icon {
                    name: None,
                    src: Some("logo.svg".into()),
                    tint: true,
                    color: Color::Default,
                },
                t("Logo"),
            ]),
        ),
        single(
            "stale",
            dir,
            SlotState::Stale(out(vec![vec![icon("mail", Color::Default), t("3")]])),
        ),
        Slot {
            id: "group".into(),
            kind: SlotKind::Group {
                interval: 5,
                members: vec![
                    Member {
                        instance_id: "g1".into(),
                        widget_dir: Arc::clone(dir),
                        state: ok(vec![icon("clock", Color::Default), t("14:05")]),
                    },
                    Member {
                        instance_id: "g2".into(),
                        widget_dir: Arc::clone(dir),
                        state: ok(vec![icon("globe", Color::Default), t("NYC 08:05")]),
                    },
                    Member {
                        instance_id: "g3".into(),
                        widget_dir: Arc::clone(dir),
                        state: SlotState::Loading,
                    },
                ],
            },
        },
        single("loading", dir, SlotState::Loading),
        single(
            "error",
            dir,
            SlotState::Error {
                message: "boom".into(),
                last: None,
            },
        ),
        single("gallery", dir, ok(gallery)),
        single("hidden1", dir, ok(vec![t("hidden")])),
        single("hidden2", dir, ok(vec![t("hidden")])),
    ];
    StripModel {
        slots,
        generation: 1,
    }
}

/// Premultiplied BGRA over an opaque background, as RGB.
fn composite(b: &Bitmap, bg: [u8; 3]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(b.pixels.len() / 4 * 3);
    for p in b.pixels.as_chunks::<4>().0 {
        let inv = 255 - u16::from(p[3]);
        for (c, back) in [p[2], p[1], p[0]].into_iter().zip(bg) {
            let v = u16::from(c) + (u16::from(back) * inv + 127) / 255;
            rgb.push(v.min(255) as u8);
        }
    }
    rgb
}

fn write_png(path: &Path, w: u32, h: u32, rgb: &[u8]) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().expect("png header");
    wr.write_image_data(rgb).expect("png data");
}

fn main() {
    let mut out_dir = std::env::temp_dir();
    let mut scale = 1.0_f32;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--scale" {
            scale = args.next().and_then(|s| s.parse().ok()).unwrap_or(1.0);
        } else {
            out_dir = PathBuf::from(a);
        }
    }
    let fixtures = Arc::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"));
    let model = sample(&fixtures);
    let mut r = Renderer::new().expect("renderer");
    let mut rot = RotationState::default();
    rot.tick(&model, std::time::Instant::now());
    let mut metrics = Metrics {
        height_dip: 48.0,
        scale,
        available_dip: 10_000.0,
    };
    // Squeeze out the last slots so the "+N" item shows.
    metrics.available_dip = layout(&model, &rot, &metrics, &r).width_dip - 60.0;
    let l = layout(&model, &rot, &metrics, &r);
    for (name, dark, bg) in [("dark", true, DARK_BG), ("light", false, LIGHT_BG)] {
        let (_, sys) = theme::current();
        let pal = theme::palette(dark, sys.accent);
        let b = r
            .render(&l, &model, &metrics, &pal, Some("cpu"))
            .expect("render");
        let path = out_dir.join(format!("perchbar-render-{name}.png"));
        write_png(&path, b.width as u32, b.height as u32, &composite(&b, bg));
        println!("{}", path.display());
    }
}
