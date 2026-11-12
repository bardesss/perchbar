//! Built-in icon names mapped to Segoe Fluent Icons glyphs.
//!
//! The renderer tries [`codepoint`] in the icon font first, then
//! [`text_fallback`] in the text font, then draws a small dot.

/// Icon font, with the Windows 10 font as the fallback family.
pub const ICON_FONT: &str = "Segoe Fluent Icons";
pub const ICON_FONT_FALLBACK: &str = "Segoe MDL2 Assets";

const GLYPHS: &[(&str, char)] = &[
    ("clock", '\u{E917}'),
    ("globe", '\u{E774}'),
    ("up", '\u{E70E}'),
    ("down", '\u{E70D}'),
    ("warning", '\u{E7BA}'),
    ("battery", '\u{E83F}'),
    ("music", '\u{EC4F}'),
    ("mail", '\u{E715}'),
    ("sun", '\u{E706}'),
    ("cloud", '\u{E753}'),
    // Segoe Fluent Icons has no rain cloud; E9C4 is empty, so a water drop.
    ("rain", '\u{EB42}'),
];

pub fn codepoint(name: &str) -> Option<char> {
    GLYPHS.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

/// Text drawn in the text font for names the icon font has no glyph for.
pub fn text_fallback(name: &str) -> Option<&'static str> {
    match name {
        "bitcoin" => Some("\u{20BF}"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_icons_resolve() {
        assert_eq!(codepoint("clock"), Some('\u{E917}'));
        assert_eq!(codepoint("warning"), Some('\u{E7BA}'));
        assert_eq!(codepoint("globe"), Some('\u{E774}'));
    }

    #[test]
    fn unknown_icon_is_none() {
        assert_eq!(codepoint("nope"), None);
        assert_eq!(text_fallback("nope"), None);
    }

    #[test]
    fn bitcoin_falls_back_to_text() {
        assert_eq!(codepoint("bitcoin"), None);
        assert_eq!(text_fallback("bitcoin"), Some("\u{20BF}"));
    }
}
