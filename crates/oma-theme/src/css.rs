//! Turn a resolved [`Theme`] into CSS.
//!
//! Two outputs, because the browser dresses two very different surfaces:
//!
//! * [`ThemeCss::chrome`] — a `:root` custom-property block for our own chrome.
//!   A theme change is then a variable swap, not a re-render.
//! * [`ThemeCss::page_script`] — a small script injected into *loaded websites*.
//!   Deliberately conservative: scrollbars, selection, and `color-scheme` only.
//!   Recolouring arbitrary pages breaks them, so that stays opt-in.

use std::fmt::Write as _;

use crate::color::Rgb;
use crate::palette::Mode;
use crate::semantic::SemanticPalette;
use crate::shell::ShellTokens;
use crate::{Palette, Theme};

#[derive(Debug, Clone)]
pub struct ThemeCss {
    pub theme_name: String,
    pub mode: Mode,
    /// The `:root { … }` block, ready to drop inside a `<style>` element.
    pub chrome: String,
    /// The page background, also used for the webview's own background colour so
    /// a new tab doesn't flash white before first paint.
    pub background: Rgb,
    /// Cheap change detector, so a redundant theme event doesn't repaint the UI.
    pub fingerprint: u64,

    /// The semantic tokens, in the vocabulary shared with visionPTY's TUITheme
    /// and the chrome-vmux controller.
    pub semantic: SemanticPalette,
    /// Background opacity, matching the user's Ghostty setting.
    pub opacity: f64,
    /// The colour of the translucent veil, matching Ghostty's.
    pub tint: Rgb,
}

impl ThemeCss {
    pub fn build(theme: &Theme) -> Self {
        let semantic = SemanticPalette::derive(&theme.palette);
        let chrome = render_chrome(
            &theme.palette,
            &theme.shell,
            theme.mode(),
            &semantic,
            theme.tint,
            theme.opacity,
        );

        Self {
            fingerprint: fingerprint(&chrome),
            theme_name: theme.name.clone(),
            mode: theme.mode(),
            background: semantic.canvas,
            semantic,
            opacity: theme.opacity,
            tint: theme.tint,
            chrome,
        }
    }

    /// A script for `WebviewBuilder::initialization_script`.
    ///
    /// On WebKitGTK this becomes a `UserContentManager` user script, so it runs
    /// on every navigation before `window.onload`, and re-running it updates the
    /// same style element rather than stacking new ones.
    ///
    /// Two levels, ported from visionPTY's chrome-vmux content runtime:
    ///
    /// * always — publish the token block and restyle the things every site gets
    ///   wrong against a dark theme anyway: links, form controls, the caret, the
    ///   focus ring, selection and scrollbars;
    /// * opt in — walk the document and repaint *neutral* surfaces only, leaving
    ///   brand colour alone. That is the part that breaks sites, so it is off
    ///   unless asked for.
    pub fn page_script(&self, recolor: bool, max_rules: usize) -> String {
        // The script is a plain constant, substituted rather than `format!`ed.
        // It is a few hundred lines of JavaScript, and JavaScript is all braces;
        // running it through a format string means doubling every one of them,
        // which is both unreadable and a standing invitation to a compile error
        // three edits from now.
        PAGE_SCRIPT.replace("__OMA_CONFIG__", &self.page_config(recolor, max_rules))
    }

    /// The values the page runtime needs in order to map the site's own colours
    /// onto our ramp. It maps them per *stylesheet rule*, so it needs the target
    /// colours rather than a pre-baked set of attribute selectors.
    ///
    /// Hand-rolled JSON: this crate has no `serde_json` at runtime, and the
    /// shape is four strings and a bool.
    fn page_config(&self, recolor: bool, max_rules: usize) -> String {
        let s = &self.semantic;
        // The two ends of the ramp go over as raw channels, not CSS strings: the
        // runtime interpolates between them per colour rather than picking from
        // a handful of pre-baked surfaces, so it needs to do arithmetic on them.
        format!(
            "{{\"css\":{css},\"recolor\":{recolor},\"maxRules\":{max_rules},\"tint\":[{tr},{tg},{tb}],\
             \"fgRgb\":[{fr},{fg},{fb}],\"opacity\":{opacity},\
             \"accent\":{accent},\"selection\":{selection}}}",
            css = js_string(&self.page_css()),
            tr = self.tint.r,
            tg = self.tint.g,
            tb = self.tint.b,
            fr = s.foreground.r,
            fg = s.foreground.g,
            fb = s.foreground.b,
            opacity = self.opacity,
            // Hex strings rather than channels: nothing interpolates these, and
            // the one consumer -- the link-hint label -- puts them straight into
            // a `style` attribute. See `oma_browse::hints`.
            accent = js_string(&s.accent.to_hex()),
            selection = js_string(&s.selection.to_hex()),
        )
    }

    fn page_css(&self) -> String {
        let s = &self.semantic;
        // `!important` throughout: we are competing with the site's own rules,
        // and losing silently is worse than not styling at all.
        //
        // `a:any-link` is (0,1,1) on purpose: it has to outrank a site's own
        // `a { color: ... }` so links stay findable, including on the many
        // sites that paint them a neutral grey. The one thing that legitimately
        // beats it is a colour `page.js` measured against a background we chose
        // not to touch -- see `brandBg` there, which raises just those rules
        // above this one rather than lowering this one for everybody.
        format!(
            ":root {{ color-scheme: {mode} !important;\n{vars}}}\n\
             a:any-link {{ color: {accent} !important; }}\n\
             input, textarea, select {{ background-color: {raised} !important;                color: {fg} !important; caret-color: {accent} !important;                border-color: {border} !important; }}\n\
             button, [role=\"button\"], [role=\"dialog\"], [role=\"menu\"] {{                border-color: {border} !important; }}\n\
             :focus-visible {{ outline-color: {focus} !important; }}\n\
             ::selection {{ background: {selection} !important; color: {sel_fg} !important; }}\n\
             html {{ scrollbar-color: {muted} transparent; }}\n\
             ::-webkit-scrollbar {{ width: 11px; height: 11px; }}\n\
             ::-webkit-scrollbar-track {{ background: {surface}; }}\n\
             ::-webkit-scrollbar-thumb {{ background: {muted}; border: 2px solid {surface}; }}\n\
             ::-webkit-scrollbar-thumb:hover {{ background: {fg}; }}\n\
             ::-webkit-scrollbar-corner {{ background: {surface}; }}\n\
             #__oma_browse_veil {{ position: fixed !important; inset: 0 !important;                z-index: -2147483645 !important; pointer-events: none !important;                background-color: {canvas_a} !important; }}\n\
             body {{ background-color: transparent !important; }}\n\
             {float_rule}",
            mode = self.mode.as_str(),
            vars = s.to_css_vars("oma-page"),
            surface = s.surface.to_hex(),
            raised = s.raised.to_hex(),
            // The veil. It cannot come from `set_background_color` (WebKit
            // zeroes the webview's alpha in transparent mode) and it cannot come
            // from `html` either — WebKit skips the root background entirely
            // when the view is transparent. So a real element carries it, and
            // exactly one, or nested layouts would stack it into mud.
            canvas_a = self.tint.to_css(self.opacity),
            // The surface/raised fills now live in the runtime config instead:
            // the page maps them per stylesheet rule, not per element.
            float_rule = self.float_rule(),
            fg = s.foreground.to_hex(),
            muted = s.muted_foreground.to_hex(),
            accent = s.accent.to_hex(),
            border = s.border.to_hex(),
            focus = s.focus.to_hex(),
            selection = s.selection.to_hex(),
            sel_fg = s.selection_foreground().to_hex(),
        )
    }

    /// How a surface that paints over its siblings is filled.
    ///
    /// `backdrop-filter` only ever blurs an *opaque* backdrop. Verified in both
    /// this browser and stock MiniBrowser, with the backdrop at alpha 0, 0.5 and
    /// 1.0: neither blurs the first two, both blur the third. Blurring text that
    /// sits on nothing spreads the bright pixels outward instead of averaging
    /// them into anything -- which is why it reads as bloom rather than blur.
    ///
    /// A transparent page therefore cannot have glass on its own. What it can
    /// have is an opaque patch laid *under the content* in exactly the rectangle
    /// a float occupies -- see the backers in the page script. That region was
    /// never see-through anyway, since the float covers it, so nothing is lost;
    /// everywhere else the page stays transparent.
    fn float_rule(&self) -> String {
        // Omarchy's own blur is size 8 / two passes, but that only has a
        // wallpaper to soften. This has to dissolve headlines, so it runs wider.
        const BLUR_PX: u32 = 24;

        format!(
            "[data-oma-layer=\"float\"] {{ background-color: {} !important; \
             backdrop-filter: blur({BLUR_PX}px) saturate(1.35) !important; \
             -webkit-backdrop-filter: blur({BLUR_PX}px) saturate(1.35) !important; }}\n\
             .__oma_browse_backer {{ position: fixed !important; \
             z-index: -2147483640 !important; pointer-events: none !important; \
             background-color: {} !important; }}",
            // Light enough to read as glass, because the blur underneath is now
            // doing the occluding rather than the alpha alone.
            self.tint.mix(self.semantic.foreground, 0.05).to_css(0.42),
            // Fully opaque: this is the surface that makes the blur possible.
            self.semantic.canvas.to_hex()
        )
    }

    pub fn mode_str(&self) -> &'static str {
        self.mode.as_str()
    }
}

fn fingerprint(s: &str) -> u64 {
    // FNV-1a: we only need "did this change", not cryptographic strength.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A shell.toml lookup that degrades to a palette-derived value, because plenty
/// of themes ship no `shell.toml` and users may have deleted sections from theirs.
struct Surface<'a> {
    shell: &'a ShellTokens,
    palette: &'a Palette,
}

impl<'a> Surface<'a> {
    fn css(&self, section: &str, key: &str, fallback: impl FnOnce() -> String) -> String {
        self.shell.css(section, key).unwrap_or_else(fallback)
    }

    fn paint(&self, section: &str, key: &str, fallback: impl FnOnce() -> String) -> String {
        self.shell.css_paint(section, key).unwrap_or_else(fallback)
    }

    /// A colour mixed toward the theme's own "away from the background" direction,
    /// so derived shades stay right in both light and dark themes.
    fn shade(&self, base: Rgb, amount: f64) -> Rgb {
        let toward =
            if self.palette.mode().is_dark() { Rgb::new(255, 255, 255) } else { Rgb::new(0, 0, 0) };
        base.mix(toward, amount)
    }
}

fn render_chrome(
    palette: &Palette,
    shell: &ShellTokens,
    mode: Mode,
    semantic: &SemanticPalette,
    tint: Rgb,
    opacity: f64,
) -> String {
    let s = Surface { shell, palette };

    let bg = palette.background();
    let fg = palette.foreground();
    let accent = palette.accent();
    let muted = palette.muted();
    let selection = palette.selection();

    let base_size = shell.font_base_size();
    let scale = shell.spacing_scale();

    let mut css = String::with_capacity(4096);
    css.push_str(":root {\n");

    // --- identity -----------------------------------------------------------
    let _ = writeln!(css, "  color-scheme: {};", mode.as_str());

    // --- the semantic vocabulary, shared with visionPTY ----------------------
    css.push_str(&semantic.to_css_vars("oma"));

    // --- the core palette ---------------------------------------------------
    let _ = writeln!(css, "  --oma-bg: {};", bg.to_hex());
    let _ = writeln!(css, "  --oma-bg-rgb: {};", bg.to_rgb_triplet());
    let _ = writeln!(css, "  --oma-fg: {};", fg.to_hex());
    let _ = writeln!(css, "  --oma-accent: {};", accent.to_hex());
    let _ = writeln!(css, "  --oma-muted: {};", muted.to_hex());
    let _ = writeln!(css, "  --oma-selection: {};", selection.to_hex());
    // The translucent veil, matching Ghostty's background and opacity.
    let _ = writeln!(css, "  --oma-veil: {};", tint.to_css(opacity));

    // Surfaces that sit above and below the base background.
    let _ = writeln!(
        css,
        "  --oma-bg-raised: {};",
        palette.get("lighter_background").unwrap_or_else(|| s.shade(bg, 0.06)).to_hex()
    );
    let _ = writeln!(
        css,
        "  --oma-bg-sunken: {};",
        palette.get("dark_background").unwrap_or_else(|| bg.mix(Rgb::new(0, 0, 0), 0.25)).to_hex()
    );

    // Every resolved key, verbatim, so page-level styling and future chrome can
    // reach colours we did not anticipate.
    for (key, value) in palette.iter() {
        let _ = writeln!(css, "  --oma-color-{}: {};", key.replace('_', "-"), value.to_hex());
    }

    // --- typography ---------------------------------------------------------
    // Omarchy's scale: body = base, subtitle ~ base * 1.083, heading ~ base * 1.333.
    let _ = writeln!(css, "  --oma-font-base: {base_size}px;");
    let _ = writeln!(css, "  --oma-font-caption: {:.2}px;", base_size * 0.833);
    let _ = writeln!(css, "  --oma-font-small: {:.2}px;", base_size * 0.917);
    let _ = writeln!(css, "  --oma-font-body: {base_size}px;");
    let _ = writeln!(css, "  --oma-font-subtitle: {:.2}px;", base_size * 1.083);
    let _ = writeln!(css, "  --oma-font-title: {:.2}px;", base_size * 1.167);
    let _ = writeln!(css, "  --oma-font-heading: {:.2}px;", base_size * 1.333);
    let _ = writeln!(css, "  --oma-font-mono: monospace;");

    // --- spacing ------------------------------------------------------------
    let _ = writeln!(css, "  --oma-space: {:.2}px;", 4.0 * scale);
    let _ = writeln!(css, "  --oma-space-2: {:.2}px;", 8.0 * scale);
    let _ = writeln!(css, "  --oma-space-3: {:.2}px;", 12.0 * scale);
    // Omarchy's looknfeel sets rounding = 0; match its square-cornered look.
    let _ = writeln!(css, "  --oma-radius: 0px;");

    // --- control states (tab strip, buttons) --------------------------------
    for (state, css_name, default_alpha) in [
        ("normal", "normal", 0.04),
        ("hover-cursor", "hover", 0.08),
        ("focus", "focus", 0.08),
        ("selected", "selected", 0.18),
    ] {
        let color = shell.color("controls", &format!("{state}-color")).unwrap_or(fg);
        let fill_alpha =
            shell.number("controls", &format!("{state}-fill-alpha")).unwrap_or(default_alpha);
        let _ = writeln!(css, "  --oma-control-{css_name}-fg: {};", color.to_hex());
        let _ = writeln!(css, "  --oma-control-{css_name}-fill: {};", color.to_css(fill_alpha));
        let _ = writeln!(
            css,
            "  --oma-control-{css_name}-border: {};",
            s.paint("controls", &format!("{state}-border"), || color
                .to_css(shell.alpha("controls", &format!("{state}-border"))))
        );
        let _ = writeln!(
            css,
            "  --oma-control-{css_name}-border-width: {}px;",
            shell
                .number("controls", &format!("{state}-border-width"))
                .unwrap_or(if state == "selected" { 0.0 } else { 1.0 })
        );
    }
    let _ = writeln!(
        css,
        "  --oma-control-pressed-fill: {};",
        fg.to_css(shell.number("controls", "pressed-fill-alpha").unwrap_or(0.22))
    );

    // --- the omnibar, dressed as Omarchy's launcher -------------------------
    let _ =
        writeln!(css, "  --oma-omnibar-bg: {};", s.css("launcher", "background", || bg.to_hex()));
    let _ = writeln!(css, "  --oma-omnibar-fg: {};", s.css("launcher", "text", || fg.to_hex()));
    let _ =
        writeln!(css, "  --oma-omnibar-scrim: {};", s.css("launcher", "scrim", || bg.to_css(0.5)));
    let _ = writeln!(
        css,
        "  --oma-omnibar-border: {};",
        s.paint("launcher", "border", || accent.to_hex())
    );
    let _ = writeln!(
        css,
        "  --oma-omnibar-selected-bg: {};",
        s.css("launcher", "selected-background", || fg.to_css(0.08))
    );
    let _ = writeln!(
        css,
        "  --oma-omnibar-selected-fg: {};",
        s.css("launcher", "selected-text", || accent.to_hex())
    );

    // --- menus and popups ---------------------------------------------------
    let _ = writeln!(css, "  --oma-menu-bg: {};", s.css("menu", "background", || bg.to_hex()));
    let _ = writeln!(css, "  --oma-menu-fg: {};", s.css("menu", "text", || fg.to_hex()));
    let _ =
        writeln!(css, "  --oma-menu-border: {};", s.paint("menu", "border", || fg.to_css(0.25)));
    let _ = writeln!(
        css,
        "  --oma-menu-selected-bg: {};",
        s.css("menu", "selected-background", || fg.to_css(0.08))
    );
    let _ = writeln!(
        css,
        "  --oma-menu-selected-fg: {};",
        s.css("menu", "selected-text", || accent.to_hex())
    );
    let _ = writeln!(css, "  --oma-popup-bg: {};", s.css("popups", "background", || bg.to_hex()));
    let _ = writeln!(css, "  --oma-popup-fg: {};", s.css("popups", "text", || fg.to_hex()));
    let _ =
        writeln!(css, "  --oma-popup-border: {};", s.paint("popups", "border", || accent.to_hex()));
    let _ = writeln!(
        css,
        "  --oma-tooltip-bg: {};",
        s.css("tooltip", "background", || bg.to_css(0.97))
    );
    let _ = writeln!(css, "  --oma-tooltip-fg: {};", s.css("tooltip", "text", || fg.to_hex()));

    // --- the focus ring, matching the Hyprland active-window border ---------
    let _ = writeln!(
        css,
        "  --oma-focus: {};",
        s.paint("hyprland", "active-border", || accent.to_hex())
    );
    let _ = writeln!(
        css,
        "  --oma-focus-solid: {};",
        shell.color("hyprland", "active-border").unwrap_or(accent).to_hex()
    );

    css.push_str("}\n");
    css
}

/// A JavaScript string literal, quotes included.
///
/// Rust's `Debug` for `str` is nearly right but escapes non-ASCII as `\u{...}`,
/// which JavaScript does not accept outside a template literal. Do it by hand.
fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // `</script` inside a string would close an inline script element.
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The page-side runtime. See `page.js` for what it does and why.
///
/// Injected as a WebKitGTK user script, so it runs at document-start on every
/// navigation, and re-running it tears the previous instance down first.
const PAGE_SCRIPT: &str = include_str!("page.js");

#[cfg(test)]
mod tests {
    use super::*;

    /// The link accent must keep its specificity.
    ///
    /// It is `a:any-link` at (0,1,1) so that it outranks a site's own
    /// `a { color: ... }` -- links on sites that paint them neutral grey stay
    /// findable only because of that. Wrapping it in `:where()` fixes a button
    /// contrast bug at the cost of this, which is the wrong trade: `page.js`
    /// raises the handful of rules that should win instead.
    #[test]
    fn the_link_accent_keeps_its_specificity() {
        // `load` cannot fail -- it falls back to the built-in palette -- and the
        // assertion is on static rule text, so it holds under any theme.
        let css = ThemeCss::build(&crate::Theme::load()).page_css();
        assert!(css.contains("a:any-link {"), "the link rule went missing");
        assert!(
            !css.contains(":where(a:any-link)"),
            "zeroing this rule's specificity makes neutral-coloured links render as body text"
        );
    }
}
