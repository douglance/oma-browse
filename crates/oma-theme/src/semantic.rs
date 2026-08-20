//! A semantic palette derived from the Omarchy colours.
//!
//! The vocabulary and the mix ratios are taken from visionPTY's `SemanticPalette`
//! (`extensions/chrome-vmux/src/shared/profile-generator.ts`), so oma-browse, the
//! `TUITheme` token system, and the Chrome controller all describe a surface the
//! same way. Same names, same derivation — the only intentional divergence is
//! dark/light detection, which follows Omarchy's own rule rather than WCAG
//! relative luminance so that we always agree with `omarchy-theme-color`.
//!
//! Where Omarchy already publishes a token (it resolves `muted`, `selection` and
//! friends itself), we prefer it over the derived value: Omarchy is the source of
//! truth on this machine, and theme authors tune those deliberately.

use crate::color::Rgb;
use crate::palette::{Mode, Palette};

/// Mix ratios, lifted verbatim from the chrome-vmux profile generator.
mod ratio {
    /// A surface one step off the canvas.
    pub const SURFACE: f64 = 0.06;
    /// A surface two steps off the canvas: inputs, buttons, popovers.
    pub const RAISED: f64 = 0.12;
    /// Text that should recede without becoming unreadable.
    pub const MUTED: f64 = 0.68;
    /// Hairlines and control edges.
    pub const BORDER: f64 = 0.32;
    /// The focus ring: almost the accent, pulled slightly toward the canvas.
    pub const FOCUS: f64 = 0.88;
    /// Selection wash, when the theme does not name one.
    pub const SELECTION: f64 = 0.34;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticPalette {
    pub canvas: Rgb,
    pub surface: Rgb,
    pub raised: Rgb,
    pub foreground: Rgb,
    pub muted_foreground: Rgb,
    pub accent: Rgb,
    pub border: Rgb,
    pub focus: Rgb,
    pub selection: Rgb,
    pub error: Rgb,
    pub warning: Rgb,
    pub success: Rgb,
    pub mode: Mode,
}

impl SemanticPalette {
    pub fn derive(palette: &Palette) -> Self {
        let canvas = palette.background();
        let foreground = palette.foreground();
        let accent = palette.accent();

        // Omarchy resolves these itself; a theme author's explicit choice beats
        // anything we would compute.
        let muted_foreground = palette
            .get("muted")
            .or_else(|| palette.get("dark_foreground"))
            .unwrap_or_else(|| canvas.mix(foreground, ratio::MUTED));

        let selection = palette
            .get("selection")
            .or_else(|| palette.get("selection_background"))
            .unwrap_or_else(|| canvas.mix(accent, ratio::SELECTION));

        Self {
            canvas,
            surface: palette
                .get("lighter_background")
                .filter(|c| *c != canvas)
                .unwrap_or_else(|| canvas.mix(foreground, ratio::SURFACE)),
            raised: canvas.mix(foreground, ratio::RAISED),
            foreground,
            muted_foreground,
            accent,
            border: canvas.mix(foreground, ratio::BORDER),
            focus: canvas.mix(accent, ratio::FOCUS),
            selection,
            // ANSI 1/3 are red and yellow in every theme that ships a palette.
            error: palette.get_or("red", "color1", accent),
            warning: palette.get_or("yellow", "color3", accent),
            success: palette.get_or("green", "color2", accent),
            mode: palette.mode(),
        }
    }

    /// The colour text should take on top of [`Self::selection`].
    pub fn selection_foreground(&self) -> Rgb {
        if self.mode.is_dark() { Rgb::new(255, 255, 255) } else { Rgb::new(0, 0, 0) }
    }

    /// Emit the token block, one `--<prefix>-<name>` per line.
    pub fn to_css_vars(&self, prefix: &str) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(512);
        for (name, color) in [
            ("canvas", self.canvas),
            ("surface", self.surface),
            ("raised", self.raised),
            ("fg", self.foreground),
            ("muted", self.muted_foreground),
            ("accent", self.accent),
            ("border", self.border),
            ("focus", self.focus),
            ("selection", self.selection),
            ("error", self.error),
            ("warning", self.warning),
            ("success", self.success),
        ] {
            let _ = writeln!(out, "  --{prefix}-{name}: {};", color.to_hex());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette(extra: &str) -> Palette {
        let base = "background\t#1a1b26\nforeground\t#a9b1d6\naccent\t#7aa2f7\nmode\tdark\n";
        Palette::from_resolver_output(&format!("{base}{extra}")).unwrap()
    }

    #[test]
    fn derives_surfaces_at_the_documented_ratios() {
        let s = SemanticPalette::derive(&palette(""));
        assert_eq!(s.canvas, Rgb::new(0x1a, 0x1b, 0x26));
        // surface = mix(canvas, fg, 0.06); raised = 0.12.
        assert_eq!(s.surface, Rgb::new(0x1a, 0x1b, 0x26).mix(Rgb::new(0xa9, 0xb1, 0xd6), 0.06));
        assert_eq!(s.raised, Rgb::new(0x1a, 0x1b, 0x26).mix(Rgb::new(0xa9, 0xb1, 0xd6), 0.12));
    }

    #[test]
    fn prefers_omarchys_own_tokens_over_derived_ones() {
        // A theme author's explicit muted/selection must win.
        let s = SemanticPalette::derive(&palette("muted\t#414868\nselection\t#292e42\n"));
        assert_eq!(s.muted_foreground, Rgb::new(0x41, 0x48, 0x68));
        assert_eq!(s.selection, Rgb::new(0x29, 0x2e, 0x42));
    }

    #[test]
    fn falls_back_to_derivation_when_absent() {
        let s = SemanticPalette::derive(&palette(""));
        assert_eq!(
            s.muted_foreground,
            Rgb::new(0x1a, 0x1b, 0x26).mix(Rgb::new(0xa9, 0xb1, 0xd6), 0.68)
        );
    }

    #[test]
    fn maps_ansi_slots_to_status_colours() {
        let s = SemanticPalette::derive(&palette(
            "color1\t#f7768e\ncolor3\t#e0af68\ncolor2\t#9ece6a\n",
        ));
        assert_eq!(s.error, Rgb::new(0xf7, 0x76, 0x8e));
        assert_eq!(s.warning, Rgb::new(0xe0, 0xaf, 0x68));
        assert_eq!(s.success, Rgb::new(0x9e, 0xce, 0x6a));
    }

    #[test]
    fn emits_every_token() {
        let css = SemanticPalette::derive(&palette("")).to_css_vars("oma");
        for name in [
            "canvas",
            "surface",
            "raised",
            "fg",
            "muted",
            "accent",
            "border",
            "focus",
            "selection",
            "error",
            "warning",
            "success",
        ] {
            assert!(css.contains(&format!("--oma-{name}:")), "missing --oma-{name}");
        }
    }
}
