//! A compact, allocation-light RGBA color used across the language, theming and renderer.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Straight (non-premultiplied) sRGB color with 8 bits per channel.
///
/// The renderer converts to premultiplied float at draw time; keeping the stored
/// form straight means theme files and user-facing hex values stay predictable.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Default for Color {
    /// Fully transparent, so a default-constructed style paints nothing rather
    /// than an unexpected black.
    fn default() -> Self {
        Color::TRANSPARENT
    }
}

impl Color {
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const WHITE: Color = Color::rgb(255, 255, 255);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r, g, b, a }
    }

    /// Linear interpolation in sRGB space.
    pub fn lerp(self, other: Color, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        let f = |a: u8, b: u8| {
            (a as f32 + (b as f32 - a as f32) * t).round().clamp(0.0, 255.0) as u8
        };
        Color {
            r: f(self.r, other.r),
            g: f(self.g, other.g),
            b: f(self.b, other.b),
            a: f(self.a, other.a),
        }
    }

    /// Multiply the alpha channel, used for opacity without a second color value.
    pub fn with_alpha(self, alpha: f32) -> Color {
        Color { a: (self.a as f32 * alpha.clamp(0.0, 1.0)).round() as u8, ..self }
    }

    pub fn with_opacity(self, opacity: f32) -> Color {
        self.with_alpha(opacity.clamp(0.0, 1.0) * 255.0)
    }

    /// Perceived luminance, used to pick readable foreground colors automatically.
    pub fn luminance(self) -> f32 {
        let l = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * l(self.r) + 0.7152 * l(self.g) + 0.0722 * l(self.b)
    }

    /// Blend `self` over `dst` using source alpha (straight-alpha compositing).
    pub fn over(self, dst: Color) -> Color {
        if self.a == 255 {
            return self;
        }
        if self.a == 0 {
            return dst;
        }
        let sa = self.a as f32 / 255.0;
        let da = dst.a as f32 / 255.0;
        let oa = sa + da * (1.0 - sa);
        if oa <= 0.0 {
            return Color::TRANSPARENT;
        }
        let ch = |s: u8, d: u8| {
            let s = s as f32 / 255.0;
            let d = d as f32 / 255.0;
            (((s * sa + d * da * (1.0 - sa)) / oa) * 255.0).round().clamp(0.0, 255.0) as u8
        };
        Color { r: ch(self.r, dst.r), g: ch(self.g, dst.g), b: ch(self.b, dst.b), a: (oa * 255.0).round() as u8 }
    }

    /// Parse `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`, or a small set of CSS names.
    pub fn parse(input: &str) -> Option<Color> {
        let s = input.trim();
        if let Some(hex) = s.strip_prefix('#') {
            let h = hex.trim();
            if !h.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let d = |i: usize| {
                // In the short form each digit is doubled: `#f0f` is `#ff00ff`.
                u8::from_str_radix(&h[i..i + 1], 16).ok().map(|n| n * 17)
            };
            let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
            return match h.len() {
                3 => Some(Color::rgba(d(0)?, d(1)?, d(2)?, 255)),
                4 => Some(Color::rgba(d(0)?, d(1)?, d(2)?, d(3)?)),
                6 => Some(Color::rgba(p(0)?, p(2)?, p(4)?, 255)),
                8 => Some(Color::rgba(p(0)?, p(2)?, p(4)?, p(6)?)),
                _ => None,
            };
        }
        let lower = s.to_ascii_lowercase();
        Some(match lower.as_str() {
            "transparent" | "none" => Color::TRANSPARENT,
            "black" => Color::rgb(0, 0, 0),
            "white" => Color::rgb(255, 255, 255),
            "red" => Color::rgb(0xef, 0x44, 0x44),
            "green" => Color::rgb(0x4c, 0xc9, 0x5a),
            "blue" => Color::rgb(0x45, 0x8b, 0xe0),
            "orange" => Color::rgb(0xf5, 0x9e, 0x2b),
            "yellow" => Color::rgb(0xf2, 0xcc, 0x4c),
            "purple" => Color::rgb(0xa8, 0x77, 0xe0),
            "grey" | "gray" => Color::rgb(0x8a, 0x8a, 0x8a),
            _ => return None,
        })
    }
}

impl fmt::Debug for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.a == 255 {
            write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            write!(f, "#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_forms() {
        assert_eq!(Color::parse("#f0f"), Some(Color::rgb(0xff, 0x00, 0xff)));
        assert_eq!(Color::parse("#89b4fa"), Some(Color::rgb(0x89, 0xb4, 0xfa)));
        assert_eq!(Color::parse("#89b4fa80"), Some(Color::rgba(0x89, 0xb4, 0xfa, 0x80)));
        // Short and long forms agree.
        assert_eq!(Color::parse("#abc"), Color::parse("#aabbcc"));
        assert_eq!(Color::parse("#abcd"), Color::parse("#aabbccdd"));
        // Wrong digit counts and non-hex characters are rejected.
        assert_eq!(Color::parse("#abcde"), None);
        assert_eq!(Color::parse("#xyz"), None);
        assert_eq!(Color::parse("nope"), None);
    }

    #[test]
    fn compositing_is_correct() {
        let bg = Color::rgb(0, 0, 0);
        let fg = Color::rgba(255, 255, 255, 128);
        let out = fg.over(bg);
        assert_eq!(out.r, 128);
        assert_eq!(out.a, 255);
    }

    #[test]
    fn transparent_is_identity() {
        let bg = Color::rgb(10, 20, 30);
        assert_eq!(Color::TRANSPARENT.over(bg), bg);
    }
}
