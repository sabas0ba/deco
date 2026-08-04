//! Colours in the form VS Code themes write them.

use std::fmt;
use std::str::FromStr;

/// An 8-bit-per-channel colour with alpha.
///
/// Themes routinely use translucent colours (selection and find highlights are
/// almost always `#RRGGBBAA`), so alpha is carried through rather than dropped
/// at parse time; [`Rgba::over`] does the compositing when a frontend needs an
/// opaque value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgba {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha; 255 is opaque.
    pub a: u8,
}

impl Rgba {
    /// Opaque black.
    pub const BLACK: Rgba = Rgba::rgb(0, 0, 0);
    /// Opaque white.
    pub const WHITE: Rgba = Rgba::rgb(255, 255, 255);
    /// Fully transparent.
    pub const TRANSPARENT: Rgba = Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    /// An opaque colour.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// A colour with explicit alpha.
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Whether the colour is fully opaque.
    pub const fn is_opaque(self) -> bool {
        self.a == 255
    }

    /// This colour composited over `background`, yielding an opaque result when
    /// `background` is opaque.
    pub fn over(self, background: Rgba) -> Rgba {
        if self.a == 255 {
            return self;
        }
        if self.a == 0 {
            return background;
        }
        let sa = self.a as u32;
        let ba = background.a as u32;
        // Standard source-over: out_a = sa + ba * (1 - sa)
        let out_a = sa + ba * (255 - sa) / 255;
        if out_a == 0 {
            return Rgba::TRANSPARENT;
        }
        let mix = |s: u8, b: u8| -> u8 {
            let s = s as u32 * sa;
            let b = b as u32 * ba * (255 - sa) / 255;
            ((s + b) / out_a) as u8
        };
        Rgba {
            r: mix(self.r, background.r),
            g: mix(self.g, background.g),
            b: mix(self.b, background.b),
            a: out_a.min(255) as u8,
        }
    }

    /// The same colour with a different alpha.
    pub const fn with_alpha(self, a: u8) -> Rgba {
        Rgba { a, ..self }
    }

    /// Relative luminance per WCAG, used to decide whether a theme reads as
    /// light or dark when it does not say so.
    pub fn luminance(self) -> f32 {
        fn channel(v: u8) -> f32 {
            let v = v as f32 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }

    /// The `(r, g, b, a)` channels as floats in `0.0..=1.0`, which is what the
    /// GPU frontend wants.
    pub fn to_f32(self) -> [f32; 4] {
        [
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            self.a as f32 / 255.0,
        ]
    }
}

/// A colour string that could not be parsed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("`{input}` is not a colour (expected #RGB, #RGBA, #RRGGBB or #RRGGBBAA)")]
pub struct ColorParseError {
    /// The offending text.
    pub input: String,
}

impl FromStr for Rgba {
    type Err = ColorParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ColorParseError {
            input: s.to_owned(),
        };
        let hex = s.trim().strip_prefix('#').ok_or_else(err)?;
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(err());
        }
        // The 3- and 4-digit forms repeat each digit, so #f0a is #ff00aa.
        let expand = |c: u8| -> u8 {
            let d = (c as char).to_digit(16).unwrap_or(0) as u8;
            d * 17
        };
        let bytes = hex.as_bytes();
        let pair = |i: usize| -> u8 { u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0) };
        Ok(match hex.len() {
            3 => Rgba::rgb(expand(bytes[0]), expand(bytes[1]), expand(bytes[2])),
            4 => Rgba::new(
                expand(bytes[0]),
                expand(bytes[1]),
                expand(bytes[2]),
                expand(bytes[3]),
            ),
            6 => Rgba::rgb(pair(0), pair(2), pair(4)),
            8 => Rgba::new(pair(0), pair(2), pair(4), pair(6)),
            _ => return Err(err()),
        })
    }
}

impl fmt::Display for Rgba {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_opaque() {
            write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            write!(
                f,
                "#{:02x}{:02x}{:02x}{:02x}",
                self.r, self.g, self.b, self.a
            )
        }
    }
}

impl<'de> serde::Deserialize<'de> for Rgba {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for Rgba {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_six_digit_hex() {
        assert_eq!(
            "#1e1e1e".parse::<Rgba>().unwrap(),
            Rgba::rgb(0x1e, 0x1e, 0x1e)
        );
        assert_eq!("#FF8000".parse::<Rgba>().unwrap(), Rgba::rgb(255, 128, 0));
    }

    #[test]
    fn parses_eight_digit_hex_with_alpha() {
        assert_eq!(
            "#264f7855".parse::<Rgba>().unwrap(),
            Rgba::new(0x26, 0x4f, 0x78, 0x55)
        );
    }

    #[test]
    fn parses_short_hex_by_repeating_digits() {
        assert_eq!("#f0a".parse::<Rgba>().unwrap(), Rgba::rgb(0xff, 0x00, 0xaa));
        assert_eq!(
            "#f0a8".parse::<Rgba>().unwrap(),
            Rgba::new(0xff, 0x00, 0xaa, 0x88)
        );
    }

    #[test]
    fn parsing_ignores_surrounding_whitespace() {
        assert_eq!("  #fff  ".parse::<Rgba>().unwrap(), Rgba::WHITE);
    }

    #[test]
    fn rejects_malformed_colours() {
        for input in ["1e1e1e", "#12345", "#gggggg", "", "#", "rgb(1,2,3)"] {
            assert!(input.parse::<Rgba>().is_err(), "{input} should not parse");
        }
    }

    #[test]
    fn display_round_trips() {
        for input in ["#1e1e1e", "#264f7855", "#ffffff"] {
            let parsed: Rgba = input.parse().unwrap();
            assert_eq!(parsed.to_string(), input);
        }
        // Short forms normalise to their long equivalent.
        assert_eq!("#f0a".parse::<Rgba>().unwrap().to_string(), "#ff00aa");
    }

    #[test]
    fn opaque_colours_composite_to_themselves() {
        let fg = Rgba::rgb(1, 2, 3);
        assert_eq!(fg.over(Rgba::WHITE), fg);
    }

    #[test]
    fn transparent_colours_composite_to_the_background() {
        assert_eq!(Rgba::TRANSPARENT.over(Rgba::WHITE), Rgba::WHITE);
    }

    #[test]
    fn half_alpha_white_over_black_is_grey() {
        let composited = Rgba::new(255, 255, 255, 128).over(Rgba::BLACK);
        assert!(composited.is_opaque());
        assert!(
            (120..=136).contains(&composited.r),
            "expected mid grey, got {composited}"
        );
    }

    #[test]
    fn compositing_keeps_the_backgrounds_alpha_when_both_are_translucent() {
        let out = Rgba::new(255, 0, 0, 128).over(Rgba::new(0, 0, 255, 128));
        assert!(out.a > 128, "alpha should accumulate, got {}", out.a);
        assert!(out.a < 255);
    }

    #[test]
    fn luminance_orders_black_below_white() {
        assert!(Rgba::BLACK.luminance() < Rgba::WHITE.luminance());
        assert!((Rgba::WHITE.luminance() - 1.0).abs() < 0.001);
        assert!(Rgba::BLACK.luminance().abs() < 0.001);
    }

    #[test]
    fn converts_to_normalised_floats() {
        assert_eq!(Rgba::WHITE.to_f32(), [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(Rgba::TRANSPARENT.to_f32(), [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn serde_round_trips() {
        let color: Rgba = serde_json::from_str(r##""#264f7855""##).unwrap();
        assert_eq!(color, Rgba::new(0x26, 0x4f, 0x78, 0x55));
        assert_eq!(serde_json::to_string(&color).unwrap(), r##""#264f7855""##);
    }

    #[test]
    fn serde_reports_bad_colours_as_errors() {
        assert!(serde_json::from_str::<Rgba>(r##""not a colour""##).is_err());
    }
}
