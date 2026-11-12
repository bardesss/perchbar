//! Display model: the JSON a widget returns, parsed and validated.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const MIN_ROTATE_SECS: u32 = 2;
const MAX_TEXT_CHARS: usize = 120;
const MAX_SPARK_POINTS: usize = 60;
const MIN_SPARK_POINTS: usize = 2;
const MAX_FRAMES: usize = 16;
const MAX_BLOCKS_PER_FRAME: usize = 8;
const MAX_TOOLTIP_CHARS: usize = 500;
const MAX_ICON_REF_CHARS: usize = 128;
const MAX_BAR_WIDTH_DIP: u32 = 200;

/// Cuts `s` to at most `max` characters, the last one an ellipsis.
fn truncate_chars(s: &mut String, max: usize) {
    if s.chars().count() > max {
        let mut cut: String = s.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        *s = cut;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DisplayError {
    #[error("invalid display output: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    Muted,
    Up,
    Down,
    Warn,
    Accent,
    Rgb(u8, u8, u8),
}

impl Color {
    fn parse(s: &str) -> Result<Color, String> {
        match s {
            "default" => Ok(Color::Default),
            "muted" => Ok(Color::Muted),
            "up" => Ok(Color::Up),
            "down" => Ok(Color::Down),
            "warn" => Ok(Color::Warn),
            "accent" => Ok(Color::Accent),
            _ => {
                let hex = s
                    .strip_prefix('#')
                    .filter(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()))
                    .ok_or_else(|| format!("invalid color {s:?}"))?;
                let v = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap();
                Ok(Color::Rgb(v(0), v(2), v(4)))
            }
        }
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Color::parse(&s).map_err(D::Error::custom)
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let text = match self {
            Color::Default => "default".to_string(),
            Color::Muted => "muted".to_string(),
            Color::Up => "up".to_string(),
            Color::Down => "down".to_string(),
            Color::Warn => "warn".to_string(),
            Color::Accent => "accent".to_string(),
            Color::Rgb(r, g, b) => format!("#{r:02X}{g:02X}{b:02X}"),
        };
        s.serialize_str(&text)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextSize {
    #[default]
    Normal,
    Small,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClickAction {
    Settings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Block {
    Text {
        value: String,
        #[serde(default)]
        color: Color,
        #[serde(default)]
        size: TextSize,
    },
    Icon {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        src: Option<String>,
        #[serde(default)]
        tint: bool,
        #[serde(default)]
        color: Color,
    },
    Bar {
        value: f64,
        #[serde(default)]
        color: Color,
        #[serde(default)]
        width: Option<u32>,
    },
    Ring {
        value: f64,
        #[serde(default)]
        color: Color,
    },
    Spark {
        values: Vec<f64>,
        #[serde(default)]
        color: Color,
    },
    Stack {
        top: Box<Block>,
        bottom: Box<Block>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Frame {
    pub blocks: Vec<Block>,
    #[serde(default)]
    pub tooltip: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Output {
    pub frames: Vec<Frame>,
    #[serde(default)]
    pub rotate: Option<u32>,
    #[serde(default)]
    pub click: Option<ClickAction>,
}

/// Parse and validate a widget's returned value. Accepts a full `Output`
/// (has `frames`) or a single `Frame` (shorthand).
pub fn parse(v: serde_json::Value) -> Result<Output, DisplayError> {
    let invalid = |e: serde_json::Error| DisplayError::Invalid(e.to_string());
    let is_output = v.as_object().is_some_and(|o| o.contains_key("frames"));
    let mut out = if is_output {
        serde_json::from_value::<Output>(v).map_err(invalid)?
    } else {
        let frame = serde_json::from_value::<Frame>(v).map_err(invalid)?;
        Output {
            frames: vec![frame],
            rotate: None,
            click: None,
        }
    };
    validate(&mut out)?;
    Ok(out)
}

fn validate(out: &mut Output) -> Result<(), DisplayError> {
    if out.frames.is_empty() {
        return Err(DisplayError::Invalid("frames must not be empty".into()));
    }
    if out.frames.len() > MAX_FRAMES {
        return Err(DisplayError::Invalid(format!(
            "at most {MAX_FRAMES} frames allowed, got {}",
            out.frames.len()
        )));
    }
    if let Some(r) = out.rotate.as_mut() {
        *r = (*r).max(MIN_ROTATE_SECS);
    }
    for frame in &mut out.frames {
        if frame.blocks.len() > MAX_BLOCKS_PER_FRAME {
            return Err(DisplayError::Invalid(format!(
                "at most {MAX_BLOCKS_PER_FRAME} blocks per frame allowed, got {}",
                frame.blocks.len()
            )));
        }
        if let Some(t) = frame.tooltip.as_mut() {
            truncate_chars(t, MAX_TOOLTIP_CHARS);
        }
        for block in &mut frame.blocks {
            validate_block(block, false)?;
        }
    }
    Ok(())
}

fn clamp_unit(v: f64, what: &str) -> Result<f64, DisplayError> {
    if v.is_finite() {
        Ok(v.clamp(0.0, 1.0))
    } else {
        Err(DisplayError::Invalid(format!(
            "{what} value must be a number"
        )))
    }
}

fn validate_block(block: &mut Block, in_stack: bool) -> Result<(), DisplayError> {
    match block {
        Block::Text { value, .. } => {
            if value.chars().count() > MAX_TEXT_CHARS {
                let mut s: String = value.chars().take(MAX_TEXT_CHARS).collect();
                s.push('…');
                *value = s;
            }
        }
        Block::Icon { name, src, .. } => match (name.as_deref(), src.as_deref()) {
            (Some(r), _) | (_, Some(r)) if r.chars().count() > MAX_ICON_REF_CHARS => {
                return Err(DisplayError::Invalid(format!(
                    "icon name or src must be at most {MAX_ICON_REF_CHARS} characters"
                )))
            }
            (Some(_), Some(_)) => {
                return Err(DisplayError::Invalid(
                    "icon takes either name or src, not both".into(),
                ))
            }
            (None, None) => return Err(DisplayError::Invalid("icon needs a name or a src".into())),
            (Some(n), None) if n.trim().is_empty() => {
                return Err(DisplayError::Invalid("icon name must not be empty".into()))
            }
            (None, Some(s)) => check_icon_src(s)?,
            _ => {}
        },
        Block::Bar { value, width, .. } => {
            *value = clamp_unit(*value, "bar")?;
            if let Some(w) = width.as_mut() {
                *w = (*w).min(MAX_BAR_WIDTH_DIP);
            }
        }
        Block::Ring { value, .. } => *value = clamp_unit(*value, "ring")?,
        Block::Spark { values, .. } => {
            if values.iter().any(|v| !v.is_finite()) {
                return Err(DisplayError::Invalid("spark values must be numbers".into()));
            }
            if values.len() < MIN_SPARK_POINTS {
                return Err(DisplayError::Invalid(
                    "spark needs at least 2 points".into(),
                ));
            }
            if values.len() > MAX_SPARK_POINTS {
                let excess = values.len() - MAX_SPARK_POINTS;
                values.drain(..excess);
            }
        }
        Block::Stack { top, bottom } => {
            if in_stack
                || matches!(**top, Block::Stack { .. })
                || matches!(**bottom, Block::Stack { .. })
            {
                return Err(DisplayError::Invalid("stacks cannot be nested".into()));
            }
            validate_block(top, true)?;
            validate_block(bottom, true)?;
        }
    }
    Ok(())
}

/// Syntactic check only; the file's existence is checked at render time.
fn check_icon_src(src: &str) -> Result<(), DisplayError> {
    let bad = |why: &str| Err(DisplayError::Invalid(format!("icon src {src:?}: {why}")));
    if src.is_empty() {
        return bad("empty");
    }
    if src.contains("://") {
        return bad("must not contain a scheme");
    }
    if src.starts_with('/') || src.starts_with('\\') {
        return bad("must be relative");
    }
    let b = src.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return bad("must not have a drive letter");
    }
    if src.split(['/', '\\']).any(|seg| seg == "..") {
        return bad("must not contain '..'");
    }
    let lower = src.to_ascii_lowercase();
    if !(lower.ends_with(".svg") || lower.ends_with(".png")) {
        return bad("must be .svg or .png");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_frame() -> serde_json::Value {
        json!({"blocks":[{"type":"text","value":"x"}]})
    }

    fn one_block(b: serde_json::Value) -> Result<Output, DisplayError> {
        parse(json!({ "blocks": [b] }))
    }

    #[test]
    fn single_frame_shorthand() {
        let o = parse(json!({"blocks":[{"type":"text","value":"BTC $1"}]})).unwrap();
        assert_eq!(o.frames.len(), 1);
        match &o.frames[0].blocks[0] {
            Block::Text { value, color, size } => {
                assert_eq!(value, "BTC $1");
                assert_eq!(*color, Color::Default);
                assert_eq!(*size, TextSize::Normal);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn full_output_with_rotate() {
        let o = parse(json!({"frames":[text_frame(), text_frame()],"rotate":5,"click":"settings"}))
            .unwrap();
        assert_eq!(o.frames.len(), 2);
        assert_eq!(o.rotate, Some(5));
        assert_eq!(o.click, Some(ClickAction::Settings));
    }

    #[test]
    fn rotate_below_two_is_raised() {
        let o = parse(json!({"frames":[text_frame()],"rotate":1})).unwrap();
        assert_eq!(o.rotate, Some(2));
    }

    #[test]
    fn bar_value_is_clamped() {
        let hi = one_block(json!({"type":"bar","value":1.7})).unwrap();
        assert!(matches!(hi.frames[0].blocks[0], Block::Bar { value, .. } if value == 1.0));
        let lo = one_block(json!({"type":"bar","value":-0.2})).unwrap();
        assert!(matches!(lo.frames[0].blocks[0], Block::Bar { value, .. } if value == 0.0));
        // json! turns NaN into null, which f64 rejects.
        assert!(one_block(json!({"type":"bar","value":f64::NAN})).is_err());
        // A real NaN that bypasses serde is still rejected by validation.
        let mut o = Output {
            frames: vec![Frame {
                blocks: vec![Block::Ring {
                    value: f64::NAN,
                    color: Color::Default,
                }],
                tooltip: None,
            }],
            rotate: None,
            click: None,
        };
        assert!(validate(&mut o).is_err());
    }

    #[test]
    fn hex_color_parses() {
        let o = one_block(json!({"type":"text","value":"a","color":"#1A2b3C"})).unwrap();
        assert!(matches!(
            o.frames[0].blocks[0],
            Block::Text {
                color: Color::Rgb(0x1A, 0x2B, 0x3C),
                ..
            }
        ));
        assert!(one_block(json!({"type":"text","value":"a","color":"#12345"})).is_err());
        assert!(one_block(json!({"type":"text","value":"a","color":"red"})).is_err());
        assert!(one_block(json!({"type":"text","value":"a","color":"up"})).is_ok());
    }

    #[test]
    fn nested_stack_rejected() {
        let t = json!({"type":"text","value":"a"});
        let ok = one_block(json!({"type":"stack","top":t,"bottom":t}));
        assert!(ok.is_ok());
        let inner = json!({"type":"stack","top":t,"bottom":t});
        assert!(one_block(json!({"type":"stack","top":inner,"bottom":t})).is_err());
        assert!(one_block(json!({"type":"stack","top":t,"bottom":inner})).is_err());
    }

    #[test]
    fn empty_frames_rejected() {
        assert!(parse(json!({"frames":[]})).is_err());
    }

    #[test]
    fn text_is_capped() {
        let long = "é".repeat(500);
        let o = one_block(json!({"type":"text","value":long})).unwrap();
        match &o.frames[0].blocks[0] {
            Block::Text { value, .. } => {
                assert_eq!(value.chars().count(), 121);
                assert!(value.ends_with('…'));
                assert!(value.starts_with("éé"));
            }
            other => panic!("unexpected {other:?}"),
        }
        let exact = "a".repeat(120);
        let o = one_block(json!({"type":"text","value":exact})).unwrap();
        assert!(matches!(&o.frames[0].blocks[0],
            Block::Text { value, .. } if value.chars().count() == 120 && !value.ends_with('…')));
    }

    #[test]
    fn spark_needs_two_points() {
        assert!(one_block(json!({"type":"spark","values":[1]})).is_err());
        let vals: Vec<f64> = (0..100).map(f64::from).collect();
        let o = one_block(json!({"type":"spark","values":vals})).unwrap();
        match &o.frames[0].blocks[0] {
            Block::Spark { values, .. } => {
                assert_eq!(values.len(), 60);
                assert_eq!(values[0], 40.0);
                assert_eq!(values[59], 99.0);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn icon_needs_exactly_one_of_name_or_src() {
        assert!(one_block(json!({"type":"icon","name":"sun","src":"a.svg"})).is_err());
        assert!(one_block(json!({"type":"icon"})).is_err());
        assert!(one_block(json!({"type":"icon","name":"sun"})).is_ok());
        assert!(one_block(json!({"type":"icon","src":"a.svg","tint":true})).is_ok());
    }

    #[test]
    fn more_than_16_frames_rejected() {
        let ok: Vec<_> = (0..16).map(|_| text_frame()).collect();
        assert_eq!(parse(json!({ "frames": ok })).unwrap().frames.len(), 16);
        let too_many: Vec<_> = (0..17).map(|_| text_frame()).collect();
        let err = parse(json!({ "frames": too_many }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("at most 16 frames"), "{err}");
    }

    #[test]
    fn more_than_8_blocks_per_frame_rejected() {
        let t = json!({"type":"text","value":"a"});
        let eight: Vec<_> = (0..8).map(|_| t.clone()).collect();
        assert_eq!(
            parse(json!({ "blocks": eight })).unwrap().frames[0]
                .blocks
                .len(),
            8
        );
        let nine: Vec<_> = (0..9).map(|_| t.clone()).collect();
        let err = parse(json!({ "blocks": nine })).unwrap_err().to_string();
        assert!(err.contains("at most 8 blocks"), "{err}");
    }

    #[test]
    fn tooltip_is_truncated_to_500_chars() {
        let long = "é".repeat(2000);
        let o = parse(json!({"blocks":[], "tooltip": long})).unwrap();
        let t = o.frames[0].tooltip.as_deref().unwrap();
        assert_eq!(t.chars().count(), 500);
        assert!(t.ends_with('…'));
        let exact = "a".repeat(500);
        let o = parse(json!({"blocks":[], "tooltip": exact})).unwrap();
        assert_eq!(o.frames[0].tooltip.as_deref(), Some(exact.as_str()));
    }

    #[test]
    fn icon_name_and_src_longer_than_128_chars_rejected() {
        let name = "a".repeat(128);
        assert!(one_block(json!({"type":"icon","name":name})).is_ok());
        let name = "a".repeat(129);
        let err = one_block(json!({"type":"icon","name":name}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("at most 128"), "{err}");
        let src = format!("{}.svg", "a".repeat(124));
        assert!(one_block(json!({"type":"icon","src":src})).is_ok());
        let src = format!("{}.svg", "a".repeat(125));
        let err = one_block(json!({"type":"icon","src":src}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("at most 128"), "{err}");
    }

    #[test]
    fn bar_width_is_clamped_to_200() {
        let o = one_block(json!({"type":"bar","value":0.5,"width":100000})).unwrap();
        assert!(matches!(
            o.frames[0].blocks[0],
            Block::Bar {
                width: Some(200),
                ..
            }
        ));
        let o = one_block(json!({"type":"bar","value":0.5,"width":40})).unwrap();
        assert!(matches!(
            o.frames[0].blocks[0],
            Block::Bar {
                width: Some(40),
                ..
            }
        ));
    }

    #[test]
    fn stacked_bar_width_is_clamped_too() {
        let bar = json!({"type":"bar","value":0.5,"width":5000});
        let t = json!({"type":"text","value":"a"});
        let o = one_block(json!({"type":"stack","top":bar,"bottom":t})).unwrap();
        match &o.frames[0].blocks[0] {
            Block::Stack { top, .. } => {
                assert!(matches!(
                    **top,
                    Block::Bar {
                        width: Some(200),
                        ..
                    }
                ))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn icon_src_must_be_relative_svg_or_png() {
        for ok in ["logo.svg", "img/logo.png", "img\\LOGO.PNG"] {
            assert!(one_block(json!({"type":"icon","src":ok})).is_ok(), "{ok}");
        }
        for bad in [
            "../x.svg",
            "a/../x.svg",
            "a\\..\\x.svg",
            "C:\\x.png",
            "c:x.png",
            "/x.png",
            "\\x.png",
            "https://x/y.png",
            "logo.gif",
            "",
        ] {
            assert!(
                one_block(json!({"type":"icon","src":bad})).is_err(),
                "{bad}"
            );
        }
    }
}
