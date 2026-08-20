//! Background opacity, taken from Ghostty.
//!
//! Omarchy's generated `ghostty.conf` carries colours but no opacity — that is a
//! personal setting, so it lives in the user's own `~/.config/ghostty/config`.
//! Reading it there means the browser is as translucent as the terminal next to
//! it, without inventing a second place to configure the same thing.

use std::path::PathBuf;

/// Fully opaque, when nothing says otherwise.
const DEFAULT: f64 = 1.0;

/// What Ghostty paints behind its text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GhosttyBackground {
    /// An explicit `background` colour, if the config overrides the theme's.
    pub color: Option<crate::Rgb>,
    pub opacity: f64,
}

impl Default for GhosttyBackground {
    fn default() -> Self {
        Self { color: None, opacity: DEFAULT }
    }
}

/// Read Ghostty's background colour and opacity.
///
/// Both matter for matching: a config that says `background = #000000` with
/// `background-opacity = 0.5` is a *black* half-veil, not a half-veil of the
/// current theme's background, and the difference is visible side by side.
pub fn ghostty_background() -> GhosttyBackground {
    for path in candidates() {
        let Ok(source) = std::fs::read_to_string(&path) else { continue };
        let opacity = parse_opacity(&source);
        let color = parse_background(&source);
        if opacity.is_some() || color.is_some() {
            return GhosttyBackground {
                color,
                opacity: opacity.unwrap_or(DEFAULT).clamp(0.0, 1.0),
            };
        }
    }
    GhosttyBackground::default()
}

/// The user's Ghostty `background-opacity`, clamped to `0.0..=1.0`.
pub fn background_opacity() -> f64 {
    ghostty_background().opacity
}

/// Pull an explicit `background` colour out of a Ghostty config.
pub fn parse_background(source: &str) -> Option<crate::Rgb> {
    let mut found = None;
    for line in source.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        if key.trim() != "background" {
            continue;
        }
        if let Ok(c) = value.trim().parse::<crate::Rgb>() {
            found = Some(c);
        }
    }
    found
}

fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        out.push(config.join("ghostty/config"));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        out.push(home.join(".config/ghostty/config"));
    }
    // A theme is free to ship one; the user's own config wins over it.
    out.push(crate::paths::theme_dir().join("ghostty.conf"));
    out
}

/// Pull `background-opacity` out of a Ghostty config.
///
/// Ghostty allows a key to be set more than once, with the last write winning,
/// so scan the whole file rather than stopping at the first hit.
pub fn parse_opacity(source: &str) -> Option<f64> {
    let mut found = None;
    for line in source.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        if key.trim() != "background-opacity" {
            continue;
        }
        if let Ok(v) = value.trim().parse::<f64>() {
            found = Some(v);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_key() {
        assert_eq!(parse_opacity("background-opacity = 0.5\n"), Some(0.5));
        assert_eq!(parse_opacity("background-opacity=0.85"), Some(0.85));
    }

    #[test]
    fn ignores_comments_and_other_keys() {
        let src = "# background-opacity = 0.1\ncursor-opacity = 0.7\nfoo = bar\n";
        assert_eq!(parse_opacity(src), None);
    }

    #[test]
    fn reads_an_explicit_background_colour() {
        // Ghostty may override the theme background outright.
        let src = "theme = Atom One Dark\nbackground = #000000\nbackground-opacity = 0.5\n";
        assert_eq!(parse_background(src), Some(crate::Rgb::new(0, 0, 0)));
        assert_eq!(parse_opacity(src), Some(0.5));
    }

    #[test]
    fn background_without_a_hash_still_parses() {
        assert_eq!(
            parse_background("background = 1a1b26\n"),
            Some(crate::Rgb::new(0x1a, 0x1b, 0x26))
        );
    }

    #[test]
    fn last_write_wins() {
        assert_eq!(
            parse_opacity("background-opacity = 0.3\nbackground-opacity = 0.9\n"),
            Some(0.9)
        );
    }
}
