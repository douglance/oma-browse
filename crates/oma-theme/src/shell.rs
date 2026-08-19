//! Omarchy's *surface* language, from `shell.toml`.
//!
//! `colors.toml` gives sixteen-odd colours; `shell.toml` gives the tokens the
//! Omarchy shell actually dresses its own widgets with — control states, the
//! launcher card, menu rows, popup borders, the type scale. Consuming these is
//! what makes a third-party app look like it belongs next to the Omarchy bar
//! rather than merely sharing a palette.
//!
//! Two things make it more than a flat TOML read:
//!
//! * values may be **cross-section references**, e.g. `border = "hyprland.active-border"`;
//! * colour and opacity live in **separate keys** (`background` + `background-alpha`).
//!
//! A user-level `~/.config/omarchy/shell.toml` is layered over the theme's own
//! copy, with user keys winning — the same precedence the shell itself applies.

use std::collections::BTreeMap;
use std::path::Path;

use toml::Value as Toml;

use crate::color::Rgb;
use crate::{Error, paths};

/// How deep a chain of `section.key` references may go before we call it a cycle.
const MAX_REF_DEPTH: usize = 8;

/// A resolved shell.toml value.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Color(Rgb),
    /// A Hyprland-style gradient, e.g. `"rgba(aaaaaaee) rgba(bbbbbbee) 45deg"`.
    /// Kept whole so a focus ring can render the real gradient instead of
    /// silently flattening it.
    Gradient { stops: Vec<Rgb>, angle_deg: f64 },
    Number(f64),
    Bool(bool),
    Text(String),
}

impl Token {
    /// The single colour to use where a gradient cannot be expressed.
    pub fn solid(&self) -> Option<Rgb> {
        match self {
            Token::Color(c) => Some(*c),
            Token::Gradient { stops, .. } => stops.first().copied(),
            _ => None,
        }
    }

    pub fn number(&self) -> Option<f64> {
        match self {
            Token::Number(n) => Some(*n),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ShellTokens {
    /// `section` -> `key` -> raw TOML value, user layered over theme.
    sections: BTreeMap<String, BTreeMap<String, Toml>>,
}

impl ShellTokens {
    /// Load the theme's `shell.toml`, then layer `~/.config/omarchy/shell.toml`
    /// on top. A missing file at either level is not an error — plenty of themes
    /// ship no shell.toml at all, and most users have no override.
    pub fn load() -> Result<Self, Error> {
        let mut me = Self::default();
        me.merge_file(&paths::shell_toml())?;
        me.merge_file(&paths::user_shell_toml())?;
        Ok(me)
    }

    pub fn parse(src: &str) -> Result<Self, Error> {
        let mut me = Self::default();
        me.merge_str(src)?;
        Ok(me)
    }

    fn merge_file(&mut self, path: &Path) -> Result<(), Error> {
        match std::fs::read_to_string(path) {
            Ok(src) => self.merge_str(&src),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Read(path.to_path_buf(), e)),
        }
    }

    fn merge_str(&mut self, src: &str) -> Result<(), Error> {
        // Parse as a *document*, not a value: `Value`'s `FromStr` would read a
        // leading `[section]` header as an array literal.
        let table: toml::Table = toml::from_str(src)?;
        for (section, body) in &table {
            let Some(body) = body.as_table() else { continue };
            let entry = self.sections.entry(section.clone()).or_default();
            for (key, value) in body {
                entry.insert(key.clone(), value.clone());
            }
        }
        Ok(())
    }

    pub fn has_section(&self, section: &str) -> bool {
        self.sections.contains_key(section)
    }

    fn raw(&self, section: &str, key: &str) -> Option<&Toml> {
        self.sections.get(section)?.get(key)
    }

    /// Resolve one token, following `section.key` references.
    pub fn get(&self, section: &str, key: &str) -> Option<Token> {
        self.get_depth(section, key, 0)
    }

    fn get_depth(&self, section: &str, key: &str, depth: usize) -> Option<Token> {
        if depth > MAX_REF_DEPTH {
            tracing::warn!(section, key, "shell.toml reference chain too deep; giving up");
            return None;
        }
        match self.raw(section, key)? {
            Toml::Float(f) => Some(Token::Number(*f)),
            Toml::Integer(i) => Some(Token::Number(*i as f64)),
            Toml::Boolean(b) => Some(Token::Bool(*b)),
            Toml::String(s) => self.interpret_str(s, depth),
            _ => None,
        }
    }

    fn interpret_str(&self, s: &str, depth: usize) -> Option<Token> {
        let s = s.trim();

        // A cross-section reference, e.g. "hyprland.active-border". If it names a
        // key that exists, its resolution *is* the answer — falling back to
        // literal text would quietly turn an unresolvable reference (a typo, or a
        // cycle) into a bogus token that callers would then try to paint with.
        if let Some((sec, key)) = s.split_once('.')
            && !sec.is_empty()
            && !key.contains(' ')
            && self.raw(sec, key).is_some()
        {
            return self.get_depth(sec, key, depth + 1);
        }

        if let Some(g) = parse_gradient(s) {
            return Some(g);
        }
        if let Ok(c) = s.parse::<Rgb>() {
            return Some(Token::Color(c));
        }
        Some(Token::Text(s.to_string()))
    }

    /// A colour, following references and flattening gradients.
    pub fn color(&self, section: &str, key: &str) -> Option<Rgb> {
        self.get(section, key)?.solid()
    }

    /// The `<key>-alpha` companion, defaulting to fully opaque.
    pub fn alpha(&self, section: &str, key: &str) -> f64 {
        self.get(section, &format!("{key}-alpha"))
            .and_then(|t| t.number())
            .unwrap_or(1.0)
            .clamp(0.0, 1.0)
    }

    /// A ready-to-emit CSS colour with its alpha companion folded in — the form
    /// almost every caller wants.
    pub fn css(&self, section: &str, key: &str) -> Option<String> {
        Some(self.color(section, key)?.to_css(self.alpha(section, key)))
    }

    /// Like [`Self::css`], but renders a real `linear-gradient()` when the token
    /// is a Hyprland gradient. Useful for the focus ring, which should match the
    /// window border.
    pub fn css_paint(&self, section: &str, key: &str) -> Option<String> {
        let alpha = self.alpha(section, key);
        match self.get(section, key)? {
            Token::Gradient { stops, angle_deg } if stops.len() > 1 => {
                let stops: Vec<String> = stops.iter().map(|c| c.to_css(alpha)).collect();
                // Hyprland measures its gradient angle counter-clockwise from the
                // +x axis; CSS measures clockwise from "up". Convert so a themed
                // 45deg border reads the same way in the chrome.
                let css_angle = (90.0 - angle_deg).rem_euclid(360.0);
                Some(format!("linear-gradient({css_angle}deg, {})", stops.join(", ")))
            }
            other => other.solid().map(|c| c.to_css(alpha)),
        }
    }

    pub fn number(&self, section: &str, key: &str) -> Option<f64> {
        self.get(section, key)?.number()
    }

    /// `[font].base-size`, the rem root for Omarchy's whole type scale.
    pub fn font_base_size(&self) -> f64 {
        self.number("font", "base-size").unwrap_or(12.0).max(1.0)
    }

    /// `[spacing].scale`, a global multiplier on padding and gaps.
    pub fn spacing_scale(&self) -> f64 {
        self.number("spacing", "scale").unwrap_or(1.0).max(0.0)
    }
}

/// Parse a Hyprland-style gradient: two or more `rgba(...)`/`rgb(...)`/hex stops
/// followed by an optional `<n>deg`.
fn parse_gradient(s: &str) -> Option<Token> {
    if !s.contains(' ') {
        return None;
    }
    let mut stops = Vec::new();
    let mut angle = 0.0_f64;
    let mut saw_angle = false;

    for tok in s.split_whitespace() {
        let t = tok.trim_end_matches(',');
        if let Some(rest) = t.strip_suffix("deg")
            && let Ok(a) = rest.parse::<f64>()
        {
            angle = a;
            saw_angle = true;
            continue;
        }
        let inner = t
            .strip_prefix("rgba(")
            .or_else(|| t.strip_prefix("rgb("))
            .map(|v| v.trim_end_matches(')'))
            .unwrap_or(t);
        if let Ok(c) = inner.parse::<Rgb>() {
            stops.push(c);
        }
    }

    if stops.len() > 1 || (stops.len() == 1 && saw_angle) {
        Some(Token::Gradient { stops, angle_deg: angle })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"
[hyprland]
active-border = "#61afef"
active-border-foreground = "#abb2bf"

[popups]
background = "#080a0e"
background-alpha = 1.0
border = "hyprland.active-border"
border-alpha = 0.5

[menu]
background = "#080a0e"
selected-background = "#abb2bf"
selected-background-alpha = 0.08
border = "hyprland.active-border-foreground"

[font]
base-size = 12

[spacing]
scale = 1.5

[general]
gradient = "rgba(33ccffee) rgba(00ff99ee) 45deg"
loop-a = "general.loop-b"
loop-b = "general.loop-a"
"##;

    fn tokens() -> ShellTokens {
        ShellTokens::parse(SAMPLE).unwrap()
    }

    #[test]
    fn resolves_cross_section_references() {
        let t = tokens();
        assert_eq!(t.color("popups", "border"), Some(Rgb::new(0x61, 0xaf, 0xef)));
        assert_eq!(t.color("menu", "border"), Some(Rgb::new(0xab, 0xb2, 0xbf)));
    }

    #[test]
    fn composes_alpha_companions() {
        let t = tokens();
        assert_eq!(t.css("popups", "background").as_deref(), Some("#080a0e"));
        assert_eq!(t.css("popups", "border").as_deref(), Some("rgba(97, 175, 239, 0.5)"));
        assert_eq!(
            t.css("menu", "selected-background").as_deref(),
            Some("rgba(171, 178, 191, 0.08)")
        );
    }

    #[test]
    fn parses_hyprland_gradients_into_css() {
        let t = tokens();
        let paint = t.css_paint("general", "gradient").unwrap();
        assert_eq!(paint, "linear-gradient(45deg, #33ccff, #00ff99)");
        // Flattening still yields a usable solid colour.
        assert_eq!(t.color("general", "gradient"), Some(Rgb::new(0x33, 0xcc, 0xff)));
    }

    #[test]
    fn reference_cycles_terminate_without_yielding_a_colour() {
        // Must not hang or overflow the stack, and must not hand back something
        // a caller would try to paint with.
        let t = tokens();
        assert!(t.get("general", "loop-a").is_none());
        assert!(t.color("general", "loop-a").is_none());
        assert!(t.css("general", "loop-a").is_none());
    }

    #[test]
    fn dangling_references_resolve_to_nothing() {
        let t = ShellTokens::parse("[menu]\nborder = \"hyprland.nope\"\n").unwrap();
        // The section does not exist at all, so this is ordinary text, not a
        // reference — but it must still never be mistaken for a colour.
        assert!(t.color("menu", "border").is_none());
    }

    #[test]
    fn reads_scale_tokens_with_defaults() {
        let t = tokens();
        assert_eq!(t.font_base_size(), 12.0);
        assert_eq!(t.spacing_scale(), 1.5);
        assert_eq!(ShellTokens::default().font_base_size(), 12.0);
        assert_eq!(ShellTokens::default().spacing_scale(), 1.0);
    }

    #[test]
    fn user_layer_wins_over_theme_layer() {
        let mut t = ShellTokens::parse("[menu]\nbackground = \"#111111\"\ntext = \"#eeeeee\"\n").unwrap();
        t.merge_str("[menu]\nbackground = \"#222222\"\n").unwrap();
        assert_eq!(t.color("menu", "background"), Some(Rgb::new(0x22, 0x22, 0x22)));
        // Keys the user did not override survive.
        assert_eq!(t.color("menu", "text"), Some(Rgb::new(0xee, 0xee, 0xee)));
    }
}
