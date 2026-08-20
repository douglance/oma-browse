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
pub mod wallpaper;
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
        let palette_fg = palette.foreground();
        let mode = palette.mode();
        let tint = veil_tint(ghostty.color, palette_bg, mode);
        let backdrop = wallpaper::backdrop_luminance();
        Self {
            name: current_theme_name(),
            palette,
            shell,
            opacity: match veil_override() {
                Some(pinned) => pinned,
                None => veil_opacity(ghostty.opacity, mode, tint, palette_fg, backdrop),
            },
            tint,
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

/// The contrast ratio the theme's own foreground has to reach against whatever
/// ends up behind it. WCAG asks 4.5:1 for body text, which on a bright wallpaper
/// would demand a nearly opaque veil; this sits between that and the 3:1 large-
/// text threshold, which in practice is where a page stops looking washed out.
const TARGET_CONTRAST: f64 = 3.6;

/// However bright the wallpaper, keep a little transparency. A window that goes
/// fully opaque the moment someone picks a white wallpaper is worse than one
/// that is merely hard to see through.
const VEIL_CEILING: f64 = 0.88;

/// How see-through the page can afford to be.
///
/// Ghostty's setting is the *floor*, not the answer. Ghostty can hold a fixed
/// 0.5 because its foreground is a few high-contrast ANSI colours; a web page is
/// mid-tone text on surfaces the site chose, and over a bright wallpaper that
/// washes out no matter how correct the theming is.
///
/// So: take Ghostty's value, then open it no further than the wallpaper allows.
/// On a dark wallpaper the constraint is inactive and this is Ghostty verbatim,
/// which is the parity worth having. On a bright one it tightens just enough to
/// keep the composite under [`DARK_BACKDROP_TARGET`].
/// A veil the user pinned by hand, via `OMA_VEIL` or `oma-browse theme veil`.
///
/// The adaptive rule below is a judgement call about a trade -- readability
/// against seeing the wallpaper -- and reasonable people land in different
/// places, especially on a wallpaper that is mostly dark with one bright
/// corner. `auto` clears it and goes back to solving for contrast.
pub fn veil_override() -> Option<f64> {
    let raw = std::env::var("OMA_VEIL").ok()?;
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("auto") {
        return None;
    }
    raw.parse::<f64>().ok().map(|v| v.clamp(0.0, 1.0))
}

fn veil_opacity(ghostty: f64, mode: Mode, tint: Rgb, fg: Rgb, backdrop: Option<f64>) -> f64 {
    let base = match mode {
        Mode::Dark => ghostty,
        Mode::Light => ghostty.max(LIGHT_VEIL_FLOOR),
    };

    // A light veil lightens *toward* the wallpaper rather than away from it, so
    // the floor above is already the whole story there.
    let (Some(wall), Mode::Dark) = (backdrop, mode) else { return base };

    let tint_l = tint.luminance();
    if wall <= tint_l {
        return base;
    }

    // Contrast is (lighter + 0.05) / (darker + 0.05); solve it for the darkest
    // the backdrop may be, then for the alpha that gets us there.
    let target = (fg.luminance() + 0.05) / TARGET_CONTRAST - 0.05;
    if wall <= target {
        return base;
    }
    // alpha * tint + (1 - alpha) * wall <= target
    let needed = (wall - target) / (wall - tint_l);
    base.max(needed).min(VEIL_CEILING)
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

    const BLACK: Rgb = Rgb { r: 0, g: 0, b: 0 };
    const LIGHT_FG: Rgb = Rgb { r: 0xa9, g: 0xb1, b: 0xd6 };

    #[test]
    fn dark_themes_keep_ghosttys_opacity_over_a_dark_wallpaper() {
        // The constraint is inactive here, so this is Ghostty verbatim — the
        // parity that matters.
        assert_eq!(veil_opacity(0.5, Mode::Dark, BLACK, LIGHT_FG, Some(0.1)), 0.5);
        // And with nothing measurable behind the window at all.
        assert_eq!(veil_opacity(0.5, Mode::Dark, BLACK, LIGHT_FG, None), 0.5);
    }

    #[test]
    fn a_bright_wallpaper_closes_the_veil_enough_to_hold_text() {
        let veil = veil_opacity(0.5, Mode::Dark, BLACK, LIGHT_FG, Some(0.55));
        assert!(veil > 0.5, "a bright wallpaper must tighten the veil, got {veil}");
        // The composite has to clear the contrast target it was solved for.
        let composite = (1.0 - veil) * 0.55;
        let contrast = (LIGHT_FG.luminance() + 0.05) / (composite + 0.05);
        assert!(contrast >= TARGET_CONTRAST - 0.01, "contrast {contrast} too low");
    }

    #[test]
    fn the_veil_never_goes_fully_opaque() {
        // Pure white behind the window is the worst case, and even that leaves
        // some transparency. A dim foreground would demand more, and gets the
        // ceiling instead.
        let white = veil_opacity(0.5, Mode::Dark, BLACK, LIGHT_FG, Some(1.0));
        assert!(white < VEIL_CEILING, "{white} should not need the ceiling");
        let dim_fg = Rgb { r: 0x50, g: 0x50, b: 0x50 };
        assert_eq!(veil_opacity(0.5, Mode::Dark, BLACK, dim_fg, Some(1.0)), VEIL_CEILING);
    }

    #[test]
    fn light_themes_get_a_readability_floor() {
        let dark_fg = Rgb { r: 0x30, g: 0x30, b: 0x30 };
        let canvas = Rgb { r: 0xfa, g: 0xf4, b: 0xed };
        assert_eq!(veil_opacity(0.5, Mode::Light, canvas, dark_fg, None), LIGHT_VEIL_FLOOR);
        // An already-opaque setting is never made *more* transparent.
        assert_eq!(veil_opacity(0.95, Mode::Light, canvas, dark_fg, Some(0.9)), 0.95);
    }

    #[test]
    fn falls_back_to_the_theme_canvas() {
        let canvas = Rgb::new(0x1a, 0x1b, 0x26);
        assert_eq!(veil_tint(None, canvas, Mode::Dark), canvas);
    }
}
