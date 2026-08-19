//! Resolve the live Omarchy theme into something a web UI can wear.
//!
//! Omarchy retints every app it ships on `omarchy theme set`. This crate is the
//! browser's side of that contract: read the live theme, turn it into CSS custom
//! properties, and notice when it changes.
//!
//! The two inputs are deliberately different in kind:
//!
//! * [`Palette`] — the colours, resolved by shelling out to Omarchy's own
//!   `omarchy-theme-color` so we inherit its alias/derivation cascade verbatim.
//! * [`ShellTokens`] — the surface language from `shell.toml`: control states,
//!   the launcher card, menu rows, the type scale.

pub mod color;
pub mod css;
pub mod palette;
pub mod opacity;
pub mod paths;
pub mod semantic;
pub mod shell;
pub mod watch;

pub use color::Rgb;
pub use css::ThemeCss;
pub use palette::{Mode, Palette};
pub use semantic::SemanticPalette;
pub use shell::{ShellTokens, Token};

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Omarchy's colour resolver is not present at {0}; is Omarchy installed?")]
    ResolverMissing(&'static str),

    #[error("could not run Omarchy's colour resolver")]
    ResolverSpawn(#[source] std::io::Error),

    #[error("Omarchy's colour resolver failed: {0}")]
    ResolverFailed(String),

    #[error("the resolver returned no colours")]
    EmptyPalette,

    #[error("{0} is not a colour")]
    BadColor(String),

    #[error("could not read {0}")]
    Read(PathBuf, #[source] std::io::Error),

    #[error("malformed TOML")]
    Toml(#[from] toml::de::Error),

    #[error("could not watch the Omarchy theme directory")]
    Watch(#[from] notify::Error),
}

/// Everything the browser needs to dress itself for the current theme.
#[derive(Debug, Clone)]
pub struct Theme {
    /// The slug Omarchy knows this theme by, e.g. `one-dark-pro-deep`.
    pub name: String,
    pub palette: Palette,
    pub shell: ShellTokens,
    /// Background opacity, matching the user's Ghostty setting.
    pub opacity: f64,
    /// The colour of the translucent veil. Ghostty's explicit `background` when
    /// it sets one, so the browser and the terminal tint identically; otherwise
    /// the Omarchy theme's own canvas.
    pub tint: Rgb,
}

impl Theme {
    /// Read the live theme. Falls back to a built-in dark palette rather than
    /// refusing to start when Omarchy is absent, so the browser stays runnable
    /// off an Omarchy box.
    pub fn load() -> Self {
        let palette = match Palette::resolve() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "falling back to the built-in palette");
                Palette::fallback()
            }
        };
        let shell = ShellTokens::load().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not read shell.toml; using defaults");
            ShellTokens::default()
        });
        let ghostty = opacity::ghostty_background();
        let palette_bg = palette.background();
        let mode = palette.mode();
        Self {
            name: current_theme_name(),
            palette,
            shell,
            opacity: veil_opacity(ghostty.opacity, mode),
            tint: veil_tint(ghostty.color, palette_bg, mode),
        }
    }

    pub fn mode(&self) -> Mode {
        self.palette.mode()
    }

    /// The CSS custom-property block this theme renders to.
    pub fn css(&self) -> ThemeCss {
        ThemeCss::build(self)
    }
}

/// Pick the colour of the translucent veil.
///
/// Ghostty's own `background` wins when it agrees with the Omarchy theme about
/// dark vs light — that is the case worth matching pixel for pixel, and it is
/// what a `background = #000000` override is for. But that override is a
/// personal choice made against a dark setup: honouring it under a light theme
/// would drape a black sheet over a light desktop. There, the theme's own canvas
/// is the right veil.
fn veil_tint(ghostty: Option<Rgb>, canvas: Rgb, mode: Mode) -> Rgb {
    match ghostty {
        Some(color) => {
            let ghostty_is_dark = color.channel_sum() <= 382;
            if ghostty_is_dark == mode.is_dark() { color } else { canvas }
        }
        None => canvas,
    }
}

/// A light theme cannot be as transparent as a dark one and stay readable.
///
/// A dark veil always darkens whatever is behind it, so contrast against light
/// text survives almost any wallpaper. A light veil only lightens *toward* the
/// wallpaper: over a bright one it barely covers, the wallpaper's own variation
/// bleeds through, and dark text loses its background. Ghostty never hits this
/// because its veil is black.
///
/// So dark themes get the Ghostty opacity verbatim, and light themes get a floor
/// under it — still translucent, but opaque enough to hold text.
const LIGHT_VEIL_FLOOR: f64 = 0.82;

fn veil_opacity(ghostty: f64, mode: Mode) -> f64 {
    match mode {
        Mode::Dark => ghostty,
        Mode::Light => ghostty.max(LIGHT_VEIL_FLOOR),
    }
}

/// The active theme slug, or `"unknown"` when Omarchy is not present.
pub fn current_theme_name() -> String {
    std::fs::read_to_string(paths::theme_name_file())
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ghostty_background_wins_when_it_agrees_with_the_theme() {
        let black = Rgb::new(0, 0, 0);
        let canvas = Rgb::new(0x08, 0x0a, 0x0e);
        assert_eq!(veil_tint(Some(black), canvas, Mode::Dark), black);
    }

    #[test]
    fn a_light_theme_refuses_a_dark_veil() {
        // `background = #000000` is a choice made against a dark desktop; under a
        // light theme it would be a black sheet over a light page.
        let canvas = Rgb::new(0xfa, 0xf4, 0xed);
        assert_eq!(veil_tint(Some(Rgb::new(0, 0, 0)), canvas, Mode::Light), canvas);
    }

    #[test]
    fn dark_themes_keep_ghosttys_opacity_exactly() {
        assert_eq!(veil_opacity(0.5, Mode::Dark), 0.5);
    }

    #[test]
    fn light_themes_get_a_readability_floor() {
        assert_eq!(veil_opacity(0.5, Mode::Light), LIGHT_VEIL_FLOOR);
        // An already-opaque setting is never made *more* transparent.
        assert_eq!(veil_opacity(0.95, Mode::Light), 0.95);
    }

    #[test]
    fn falls_back_to_the_theme_canvas() {
        let canvas = Rgb::new(0x1a, 0x1b, 0x26);
        assert_eq!(veil_tint(None, canvas, Mode::Dark), canvas);
    }
}
