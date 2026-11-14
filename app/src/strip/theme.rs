//! Light and dark strip colors, following the taskbar ("system") theme.

use crate::display::Color;

/// Straight (not premultiplied) RGBA colors for the strip. Direct2D brushes
/// take straight colors and premultiply when drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub text: [u8; 4],
    pub muted: [u8; 4],
    pub up: [u8; 4],
    pub down: [u8; 4],
    pub warn: [u8; 4],
    pub accent: [u8; 4],
    pub bar_track: [u8; 4],
    pub hover_bg: [u8; 4],
    /// Background of the "+N" flyout (opaque: it floats over anything).
    pub flyout_bg: [u8; 4],
    /// The flyout's 1 px border.
    pub flyout_border: [u8; 4],
}

/// 60% alpha.
const MUTED_ALPHA: u8 = 153;
/// Windows default blue, used when the colorization color is unavailable.
const FALLBACK_ACCENT: [u8; 4] = [0x00, 0x78, 0xD4, 0xFF];

pub fn palette(dark: bool, accent: [u8; 4]) -> Palette {
    if dark {
        Palette {
            text: [0xFF, 0xFF, 0xFF, 0xFF],
            muted: [0xFF, 0xFF, 0xFF, MUTED_ALPHA],
            up: [0x6C, 0xCB, 0x5F, 0xFF],
            down: [0xFF, 0x99, 0xA4, 0xFF],
            warn: [0xFC, 0xE1, 0x00, 0xFF],
            accent,
            // White at 25%.
            bar_track: [0xFF, 0xFF, 0xFF, 64],
            // White at 8%.
            hover_bg: [0xFF, 0xFF, 0xFF, 20],
            flyout_bg: [0x2C, 0x2C, 0x2C, 0xFF],
            // White at 16%.
            flyout_border: [0xFF, 0xFF, 0xFF, 41],
        }
    } else {
        Palette {
            text: [0x1A, 0x1A, 0x1A, 0xFF],
            muted: [0x1A, 0x1A, 0x1A, MUTED_ALPHA],
            up: [0x0F, 0x7B, 0x0F, 0xFF],
            down: [0xC4, 0x2B, 0x1C, 0xFF],
            warn: [0x9D, 0x5D, 0x00, 0xFF],
            accent,
            // Black at 20%.
            bar_track: [0, 0, 0, 51],
            // Black at 6%.
            hover_bg: [0, 0, 0, 15],
            flyout_bg: [0xF9, 0xF9, 0xF9, 0xFF],
            // Black at 16%.
            flyout_border: [0, 0, 0, 41],
        }
    }
}

/// Whether the taskbar uses the dark theme, and the matching palette.
pub fn current() -> (bool, Palette) {
    let dark = !system_uses_light_theme();
    (dark, palette(dark, accent_color()))
}

pub fn resolve(color: &Color, p: &Palette) -> [u8; 4] {
    match color {
        Color::Default => p.text,
        Color::Muted => p.muted,
        Color::Up => p.up,
        Color::Down => p.down,
        Color::Warn => p.warn,
        Color::Accent => p.accent,
        Color::Rgb(r, g, b) => [*r, *g, *b, 0xFF],
    }
}

/// Reads `SystemUsesLightTheme`; a missing value means dark (the default).
fn system_uses_light_theme() -> bool {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: the out pointers are valid for the duration of the call and
    // `size` matches the buffer.
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::from_mut(&mut value).cast()),
            Some(&mut size),
        )
    };
    err.is_ok() && value != 0
}

fn accent_color() -> [u8; 4] {
    use windows::Win32::Graphics::Dwm::DwmGetColorizationColor;

    let mut argb: u32 = 0;
    let mut opaque = windows::core::BOOL::default();
    // SAFETY: both out pointers are valid locals.
    match unsafe { DwmGetColorizationColor(&mut argb, &mut opaque) } {
        Ok(()) => {
            let [_, r, g, b] = argb.to_be_bytes();
            [r, g, b, 0xFF]
        }
        Err(_) => FALLBACK_ACCENT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCENT: [u8; 4] = [0x00, 0x78, 0xD4, 0xFF];

    #[test]
    fn resolve_hex_and_semantic_colors() {
        let p = palette(true, ACCENT);
        assert_eq!(resolve(&Color::Rgb(1, 2, 3), &p), [1, 2, 3, 255]);
        assert_eq!(resolve(&Color::Default, &p), p.text);
        assert_eq!(resolve(&Color::Muted, &p), p.muted);
        assert_eq!(resolve(&Color::Up, &p), p.up);
        assert_eq!(resolve(&Color::Down, &p), p.down);
        assert_eq!(resolve(&Color::Warn, &p), p.warn);
        assert_eq!(resolve(&Color::Accent, &p), ACCENT);
    }

    #[test]
    fn dark_and_light_palettes_use_the_spec_colors() {
        let d = palette(true, ACCENT);
        assert_eq!(d.text, [0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(d.muted, [0xFF, 0xFF, 0xFF, 153]);
        assert_eq!(d.up, [0x6C, 0xCB, 0x5F, 0xFF]);
        assert_eq!(d.down, [0xFF, 0x99, 0xA4, 0xFF]);
        assert_eq!(d.warn, [0xFC, 0xE1, 0x00, 0xFF]);
        assert_eq!(d.hover_bg, [0xFF, 0xFF, 0xFF, 20]);
        let l = palette(false, ACCENT);
        assert_eq!(l.text, [0x1A, 0x1A, 0x1A, 0xFF]);
        assert_eq!(l.muted, [0x1A, 0x1A, 0x1A, 153]);
        assert_eq!(l.up, [0x0F, 0x7B, 0x0F, 0xFF]);
        assert_eq!(l.down, [0xC4, 0x2B, 0x1C, 0xFF]);
        assert_eq!(l.warn, [0x9D, 0x5D, 0x00, 0xFF]);
        assert_eq!(l.hover_bg, [0, 0, 0, 15]);
    }

    #[test]
    fn current_does_not_panic() {
        let (dark, p) = current();
        assert_eq!(p, palette(dark, p.accent));
        assert_eq!(p.accent[3], 0xFF);
    }
}
