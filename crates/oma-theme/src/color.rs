//! A minimal sRGB colour, enough to compose Omarchy's separate colour and alpha
//! keys into a single CSS value.

use std::fmt;
use std::str::FromStr;

use crate::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// `#rrggbb`, the form every Omarchy palette key uses.
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// `r, g, b` — for composing into `rgb()` / `rgba()`.
    pub fn to_rgb_triplet(self) -> String {
        format!("{}, {}, {}", self.r, self.g, self.b)
    }

    /// Omarchy keeps colour and opacity in separate keys (`background` +
    /// `background-alpha`); CSS wants them together.
    pub fn to_css(self, alpha: f64) -> String {
        if alpha >= 1.0 {
            self.to_hex()
        } else {
            let a = (alpha.clamp(0.0, 1.0) * 1000.0).round() / 1000.0;
            format!("rgba({}, {}, {}, {})", self.r, self.g, self.b, a)
        }
    }

    /// Linear mix towards `other`. Mirrors the `mix` used by Omarchy's own
    /// template functions and colour derivations.
    pub fn mix(self, other: Rgb, t: f64) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let lerp = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
        Rgb::new(lerp(self.r, other.r), lerp(self.g, other.g), lerp(self.b, other.b))
    }

    /// The same crude luminance sum Omarchy uses to auto-detect light themes
    /// when no `mode` key is present (`r + g + b > 382` means light).
    /// Relative luminance, 0..1. Used to decide how much veil a wallpaper needs.
    pub fn luminance(self) -> f64 {
        (0.2126 * f64::from(self.r) + 0.7152 * f64::from(self.g) + 0.0722 * f64::from(self.b))
            / 255.0
    }

    pub fn channel_sum(self) -> u16 {
        u16::from(self.r) + u16::from(self.g) + u16::from(self.b)
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for Rgb {
    type Err = Error;

    /// Accepts `#rgb`, `#rrggbb`, `#rrggbbaa` (alpha dropped), and bare forms
    /// without the leading `#`. Omarchy themes are hand-written, so all of these
    /// turn up in practice.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let raw = s.trim().trim_start_matches('#');
        let bad = || Error::BadColor(s.to_string());

        let expand = |c: char| -> Result<u8, Error> {
            let d = c.to_digit(16).ok_or_else(bad)? as u8;
            Ok(d * 17)
        };
        let byte = |a: char, b: char| -> Result<u8, Error> {
            let hi = a.to_digit(16).ok_or_else(bad)? as u8;
            let lo = b.to_digit(16).ok_or_else(bad)? as u8;
            Ok(hi * 16 + lo)
        };

        let c: Vec<char> = raw.chars().collect();
        match c.len() {
            3 => Ok(Rgb::new(expand(c[0])?, expand(c[1])?, expand(c[2])?)),
            6 | 8 => Ok(Rgb::new(byte(c[0], c[1])?, byte(c[2], c[3])?, byte(c[4], c[5])?)),
            _ => Err(bad()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_forms_themes_actually_use() {
        assert_eq!("#61afef".parse::<Rgb>().unwrap(), Rgb::new(0x61, 0xaf, 0xef));
        assert_eq!("61afef".parse::<Rgb>().unwrap(), Rgb::new(0x61, 0xaf, 0xef));
        assert_eq!("#fff".parse::<Rgb>().unwrap(), Rgb::new(255, 255, 255));
        // Hyprland-style 8-digit values appear in theme files; alpha is dropped.
        assert_eq!("#595959aa".parse::<Rgb>().unwrap(), Rgb::new(0x59, 0x59, 0x59));
        assert!("#zzz".parse::<Rgb>().is_err());
        assert!("".parse::<Rgb>().is_err());
    }

    #[test]
    fn composes_alpha_into_css() {
        let c = Rgb::new(8, 10, 14);
        assert_eq!(c.to_css(1.0), "#080a0e");
        assert_eq!(c.to_css(0.5), "rgba(8, 10, 14, 0.5)");
    }

    #[test]
    fn mixes_towards_black_and_white() {
        let bg = Rgb::new(100, 100, 100);
        assert_eq!(bg.mix(Rgb::new(0, 0, 0), 0.25), Rgb::new(75, 75, 75));
        assert_eq!(bg.mix(Rgb::new(255, 255, 255), 0.2), Rgb::new(131, 131, 131));
    }
}
