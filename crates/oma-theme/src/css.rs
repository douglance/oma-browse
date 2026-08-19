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
        let chrome =
            render_chrome(&theme.palette, &theme.shell, theme.mode(), &semantic, theme.tint, theme.opacity);

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
    pub fn page_script(&self, recolor: bool) -> String {
        // The script is a plain constant, substituted rather than `format!`ed.
        // It is a few hundred lines of JavaScript, and JavaScript is all braces;
        // running it through a format string means doubling every one of them,
        // which is both unreadable and a standing invitation to a compile error
        // three edits from now.
        PAGE_SCRIPT
            .replace("__OMA_CSS__", &js_string(&self.page_css()))
            .replace("__OMA_RECOLOR__", if recolor { "true" } else { "false" })
    }

    fn page_css(&self) -> String {
        let s = &self.semantic;
        // `!important` throughout: we are competing with the site's own rules,
        // and losing silently is worse than not styling at all.
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
             html {{ background-color: {canvas_a} !important; }}\n\
             body {{ background-color: transparent !important; }}\n\
             [data-oma-surface=\"canvas\"] {{ background-color: transparent !important; }}\n\
             [data-oma-surface=\"surface\"] {{ background-color: {surface_a} !important; }}\n\
             [data-oma-surface=\"raised\"] {{ background-color: {raised_a} !important; }}\n\
             {float_rule}\n\
             [data-oma-fg=\"normal\"] {{ color: {fg} !important; }}\n\
             [data-oma-fg=\"muted\"] {{ color: {muted} !important; }}\n\
             [data-oma-semantic=\"error\"] {{ color: {error} !important; }}\n\
             [data-oma-semantic=\"warning\"] {{ color: {warning} !important; }}\n\
             [data-oma-semantic=\"success\"] {{ color: {success} !important; }}",
            mode = self.mode.as_str(),
            vars = s.to_css_vars("oma-page"),
            surface = s.surface.to_hex(),
            raised = s.raised.to_hex(),
            // WebKitGTK's transparent mode zeroes the webview's own background
            // alpha, so the tint cannot come from `set_background_color` — it has
            // to be painted here. Exactly one element carries it, or nested
            // layouts would stack it into mud.
            canvas_a = self.tint.to_css(self.opacity),
            // Raised surfaces stay a touch more solid than the canvas, so cards
            // and inputs read as sitting *on* the translucent background.
            surface_a = self.tint.mix(s.foreground, 0.06).to_css((self.opacity + 0.14).min(1.0)),
            raised_a = self.tint.mix(s.foreground, 0.12).to_css((self.opacity + 0.28).min(1.0)),
            // Floating surfaces sit above content rather than beside it, so they
            // carry more alpha than a raised one — enough that text underneath
            // reads as texture rather than as words — and blur what they cover.
            // The desktop still shows through, because blurring a translucent
            // backdrop leaves it translucent; Hyprland's own blur takes it from
            // there, so the two compose instead of fighting.
            float_rule = self.float_rule(),
            fg = s.foreground.to_hex(),
            muted = s.muted_foreground.to_hex(),
            accent = s.accent.to_hex(),
            border = s.border.to_hex(),
            focus = s.focus.to_hex(),
            selection = s.selection.to_hex(),
            sel_fg = s.selection_foreground().to_hex(),
            error = s.error.to_hex(),
            warning = s.warning.to_hex(),
            success = s.success.to_hex(),
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
        let toward = if self.palette.mode().is_dark() {
            Rgb::new(255, 255, 255)
        } else {
            Rgb::new(0, 0, 0)
        };
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
        let fill_alpha = shell
            .number("controls", &format!("{state}-fill-alpha"))
            .unwrap_or(default_alpha);
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
    let _ = writeln!(css, "  --oma-omnibar-bg: {};", s.css("launcher", "background", || bg.to_hex()));
    let _ = writeln!(css, "  --oma-omnibar-fg: {};", s.css("launcher", "text", || fg.to_hex()));
    let _ = writeln!(
        css,
        "  --oma-omnibar-scrim: {};",
        s.css("launcher", "scrim", || bg.to_css(0.5))
    );
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
    let _ = writeln!(css, "  --oma-menu-border: {};", s.paint("menu", "border", || fg.to_css(0.25)));
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
    let _ = writeln!(css, "  --oma-popup-border: {};", s.paint("popups", "border", || accent.to_hex()));
    let _ = writeln!(css, "  --oma-tooltip-bg: {};", s.css("tooltip", "background", || bg.to_css(0.97)));
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

/// The page-side runtime. Injected as a WebKitGTK user script, so it runs at
/// document-start on every navigation, and re-running it updates the same style
/// element rather than stacking new ones.
const PAGE_SCRIPT: &str = r#"(function () {
  var ID = "__oma_browse_theme";
  var CSS = __OMA_CSS__;
  var RECOLOR = __OMA_RECOLOR__;

  function ensureStyle() {
    var style = document.getElementById(ID);
    if (!style) {
      style = document.createElement("style");
      style.id = ID;
      (document.head || document.documentElement).appendChild(style);
    }
    if (style.textContent !== CSS) style.textContent = CSS;
    promote(style);
    return style;
  }

  // Keep our sheet last in the document.
  //
  // At document-start there is no `<head>` yet, so the element lands directly on
  // `<html>`, ahead of everything the parser is about to insert. `!important`
  // beats a site's ordinary rules from anywhere, but an `!important` of the
  // site's own wins on document order -- and single-page apps go on appending
  // stylesheets long after load. That is the whole of the "themed on one visit,
  // untouched on the next" behaviour: nothing raced, our rules were simply
  // outranked by whatever happened to be inserted after them.
  function promote(style) {
    style = style || document.getElementById(ID);
    var head = document.head;
    if (style && head && head.lastElementChild !== style) head.appendChild(style);
  }

  function channels(value) {
    if (!value) return null;
    var m = String(value).match(/rgba?\(([^)]+)\)/i);
    if (!m) return null;
    var p = m[1].split(/[\s,\/]+/).filter(Boolean).map(Number);
    if (p.length < 3) return null;
    for (var i = 0; i < 3; i++) if (!isFinite(p[i])) return null;
    if (p.length < 4 || !isFinite(p[3])) p[3] = 1;
    return p;
  }

  function lightness(value) {
    var p = channels(value);
    if (!p || p[3] < 0.03) return null;
    return (0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]) / 255;
  }

  // Only repaint surfaces the site left grey. A channel spread this small means
  // the colour carries no brand intent, so replacing it is safe; anything more
  // saturated is the site's own identity and must survive theming.
  function isNeutral(value) {
    var p = channels(value);
    if (!p || p[3] < 0.03) return false;
    return Math.max(p[0], p[1], p[2]) - Math.min(p[0], p[1], p[2]) <= 34;
  }

  // The page's *own* base lightness, judged against nothing of ours.
  //
  // Every surface is classified by how far it sits from this value, so reading
  // it after our stylesheet has landed measures the veil we just painted and
  // reports that a plain white page is as far from its own background as it is
  // possible to be -- which is exactly how an all-white site ends up opaque
  // instead of transparent. Disable our sheet for the length of the read, and
  // cache the answer for the life of the document.
  function baseLightness() {
    if (typeof window.__omaBase === "number") return window.__omaBase;
    var style = document.getElementById(ID);
    var was = style ? style.disabled : false;
    if (style) style.disabled = true;
    var l = null;
    try {
      if (document.body) l = lightness(getComputedStyle(document.body).backgroundColor);
      if (l === null && document.documentElement) {
        l = lightness(getComputedStyle(document.documentElement).backgroundColor);
      }
    } catch (e) {}
    if (style) style.disabled = was;
    // A site that paints no background at all leaves the browser canvas showing,
    // which is white on the overwhelming majority of the web.
    if (l === null) return 1;
    window.__omaBase = l;
    return l;
  }

  var BASE = 1;

  // Give the blur something to blur.
  //
  // `backdrop-filter` samples an opaque backdrop and nothing else -- verified in
  // this engine and in stock MiniBrowser, at backdrop alpha 0, 0.5 and 1.0. On a
  // transparent page there is nothing behind a sticky header but glyphs floating
  // on air, so the filter spreads their brightness outward and the result reads
  // as bloom rather than blur.
  //
  // The fix is an opaque patch the exact size of the float, laid *under* the
  // page content on a negative z-index. Negative-z children paint before in-flow
  // block backgrounds, so content still draws on top of it and the float's
  // backdrop becomes opaque-plus-text: blurrable. That rectangle was already
  // hidden behind the float, so no transparency is lost; the rest of the page is
  // untouched.
  var backerSeq = 0;

  function syncBackers() {
    if (!document.body) return;
    var floats = document.querySelectorAll("[data-oma-layer=\"float\"]");
    var live = {};
    for (var i = 0; i < floats.length; i++) {
      var el = floats[i];
      var id = el.getAttribute("data-oma-float");
      if (!id) {
        id = String(++backerSeq);
        el.setAttribute("data-oma-float", id);
      }
      live[id] = 1;
      var r = el.getBoundingClientRect();
      var b = document.querySelector("[data-oma-backer=\"" + id + "\"]");
      if (r.width < 2 || r.height < 2) {
        if (b) b.style.display = "none";
        continue;
      }
      if (!b) {
        b = document.createElement("div");
        b.className = "__oma_browse_backer";
        b.setAttribute("data-oma-backer", id);
        b.setAttribute("data-oma-seen", "");
        b.setAttribute("aria-hidden", "true");
        document.body.appendChild(b);
      }
      b.style.display = "block";
      b.style.left = r.left + "px";
      b.style.top = r.top + "px";
      b.style.width = r.width + "px";
      b.style.height = r.height + "px";
    }
    var known = document.querySelectorAll("[data-oma-backer]");
    for (var j = 0; j < known.length; j++) {
      if (!live[known[j].getAttribute("data-oma-backer")]) {
        known[j].parentNode.removeChild(known[j]);
      }
    }
  }

  var backerPending = false;
  function queueBackers() {
    if (backerPending) return;
    backerPending = true;
    requestAnimationFrame(function () {
      backerPending = false;
      syncBackers();
    });
  }

  // Does this element paint over its siblings rather than beside them?
  //
  // A surface we make transparent stops occluding whatever scrolls beneath it,
  // and for an ordinary block that is the entire point. For a sticky header it
  // is a bug you can read straight off the screen: the headlines slide up
  // *through* the site's own navigation and the two render on top of each
  // other. Those surfaces have to keep hiding what is behind them.
  //
  // True overlap testing would mean hit-testing every painted rect on every
  // scroll, which is both expensive and stale the moment it finishes. Being out
  // of the normal flow is a cheap, static stand-in, and it is very nearly the
  // same question: a box the flow does not reserve room for is a box that lands
  // on top of something.
  //
  // `relative` is deliberately not on the list even when it carries a z-index.
  // It stays in flow, so it displaces its siblings rather than covering them --
  // and sites lean on it heavily for plain layout. Google News wraps its entire
  // 4938px article column in one, so treating it as an overlay turns the whole
  // page into a single opaque slab.
  function floats(el, cs) {
    var p = cs.position;
    if (p === "fixed" || p === "sticky" || p === "-webkit-sticky") return true;
    if (p !== "absolute") return false;
    // An absolute box as tall as the document is a layout wrapper wearing an
    // overlay's positioning. Overlays are viewport-sized at most.
    var r;
    try { r = el.getBoundingClientRect(); } catch (e) { return false; }
    return r.height > 0 && r.height <= window.innerHeight * 1.2;
  }

  function classify(el) {
    if (el.nodeType !== 1 || el.id === ID) return;
    var cs;
    try { cs = getComputedStyle(el); } catch (e) { return; }
    el.setAttribute("data-oma-seen", "");

    if (isNeutral(cs.backgroundColor)) {
      // Map the site's depth ordering onto our ramp instead of flattening every
      // neutral surface to one colour.
      var d = Math.abs((lightness(cs.backgroundColor) || 0) - BASE);
      el.setAttribute("data-oma-surface", d < 0.1 ? "canvas" : d < 0.3 ? "surface" : "raised");
      // Only neutral surfaces need this. One in the site's own brand colour is
      // still opaque after theming, so nothing bleeds through it anyway.
      if (floats(el, cs)) el.setAttribute("data-oma-layer", "float");
    }
    if (isNeutral(cs.color)) {
      // Dim greys stay dim: keep the site's own hierarchy of emphasis.
      var lc = lightness(cs.color);
      var muted = lc !== null && Math.abs(lc - BASE) < 0.45;
      el.setAttribute("data-oma-fg", muted ? "muted" : "normal");
    }
  }

  // Walk the whole document, but in slices.
  //
  // A single pass is both slow and, if it is capped, wrong: Wikipedia runs to
  // ~10k elements, and stopping early leaves everything past the cap painted in
  // the site's own colours -- visible as a white block halfway down the page.
  // Tagging is idempotent and marked, so each slice only looks at elements it
  // has not seen, and the work spreads across frames instead of blocking paint.
  var SLICE = 1500;
  var queue = [];
  var draining = false;

  function drain() {
    var budget = SLICE;
    while (queue.length && budget-- > 0) classify(queue.pop());
    if (queue.length) {
      requestAnimationFrame(drain);
    } else {
      draining = false;
      // Floats are only known once the slice that classified them has run.
      queueBackers();
    }
  }

  function scan(root) {
    if (!root || !root.querySelectorAll) return;
    BASE = baseLightness();
    if (root.nodeType === 1 && !root.hasAttribute("data-oma-seen")) queue.push(root);
    var fresh = root.querySelectorAll("*:not([data-oma-seen])");
    for (var i = 0; i < fresh.length; i++) queue.push(fresh[i]);
    if (!draining && queue.length) {
      draining = true;
      requestAnimationFrame(drain);
    }
  }

  function apply() {
    if (!document.documentElement) return;
    // Measured before the sheet exists on the first pass, cached thereafter.
    BASE = baseLightness();
    ensureStyle();
    if (RECOLOR) {
      scan(document.body || document.documentElement);
      queueBackers();
    }
  }

  apply();
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", apply, { once: true });
  }
  window.addEventListener("load", apply, { once: true });

  if (RECOLOR && !window.__omaBackerBound) {
    window.__omaBackerBound = true;
    // Capture phase, so scrolling inside a nested scroller moves the patches
    // too -- a sticky header inside one is exactly the case that needs them.
    window.addEventListener("scroll", queueBackers, true);
    window.addEventListener("resize", queueBackers);
  }

  if (RECOLOR && window.MutationObserver && !window.__omaObserving) {
    window.__omaObserving = true;
    var pending = false;
    new MutationObserver(function () {
      if (pending) return;
      pending = true;
      requestAnimationFrame(function () {
        pending = false;
        // Re-appending is itself a mutation, so this settles after one extra
        // pass rather than looping: the second time round it is already last.
        promote();
        scan(document.body || document.documentElement);
      });
    }).observe(document.documentElement, { childList: true, subtree: true });
  }
})();"#;
