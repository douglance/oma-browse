//! The resolved Omarchy colour palette.
//!
//! `colors.toml` is deliberately sparse — a theme may define only ANSI `colorN`,
//! only semantic names, or legacy short names like `bg`/`fg`. The full palette is
//! produced by an alias/fallback/derivation cascade that Omarchy implements once,
//! in `omarchy-theme-color`. We shell out to it rather than reimplementing that
//! cascade, because its own header states the contract: *"every consumer
//! (templates, OSC sequences, tmux, GNOME, previews) resolves the exact same
//! palette."* Reimplementing it is the main way third-party apps drift.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use crate::color::Rgb;
use crate::{Error, paths};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Dark => "dark",
            Mode::Light => "light",
        }
    }

    pub fn is_dark(self) -> bool {
        matches!(self, Mode::Dark)
    }
}

#[derive(Debug, Clone)]
pub struct Palette {
    /// Every key the resolver emitted, colours parsed. Non-colour keys such as
    /// `mode`/`theme_type` are kept out of here and surfaced via [`Palette::mode`].
    colors: BTreeMap<String, Rgb>,
    mode: Mode,
}

impl Palette {
    /// Run Omarchy's resolver against the live theme.
    ///
    /// Always passes `--file` explicitly rather than letting the script pick its
    /// own default: the script hardcodes `$HOME/.local/state/...`, while we honour
    /// `XDG_STATE_HOME`. Left implicit, the two disagree the moment that variable
    /// is set, and we would watch one directory while reading another.
    pub fn resolve() -> Result<Self, Error> {
        Self::resolve_file(&crate::paths::colors_toml())
    }

    /// Run the resolver against a specific `colors.toml`. Used by tests and by
    /// theme previews.
    pub fn resolve_file(colors_toml: &Path) -> Result<Self, Error> {
        Self::resolve_with(Some(colors_toml))
    }

    fn resolve_with(file: Option<&Path>) -> Result<Self, Error> {
        if !Path::new(paths::THEME_COLOR_BIN).exists() {
            return Err(Error::ResolverMissing(paths::THEME_COLOR_BIN));
        }
        let mut cmd = Command::new(paths::THEME_COLOR_BIN);
        if let Some(f) = file {
            cmd.arg("--file").arg(f);
        }
        cmd.arg("--all");

        let out = cmd.output().map_err(Error::ResolverSpawn)?;
        if !out.status.success() {
            return Err(Error::ResolverFailed(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Self::from_resolver_output(&String::from_utf8_lossy(&out.stdout))
    }

    /// Parse the `key<TAB>value` lines that `omarchy-theme-color --all` prints.
    /// Pure, so the cascade's output can be pinned in tests without Omarchy present.
    pub fn from_resolver_output(stdout: &str) -> Result<Self, Error> {
        let mut colors = BTreeMap::new();
        let mut mode = None;

        for line in stdout.lines() {
            let Some((key, value)) = line.split_once('\t') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            if value.is_empty() {
                continue;
            }
            match key {
                // The resolver exposes the same answer under both names.
                "mode" | "theme_type" => {
                    if mode.is_none() {
                        mode = match value {
                            "light" => Some(Mode::Light),
                            _ => Some(Mode::Dark),
                        };
                    }
                }
                _ => {
                    // Non-colour keys are skipped rather than fatal: the resolver
                    // is free to grow new metadata without breaking us.
                    if let Ok(c) = value.parse::<Rgb>() {
                        colors.insert(key.to_string(), c);
                    }
                }
            }
        }

        if colors.is_empty() {
            return Err(Error::EmptyPalette);
        }

        // Belt and braces: if the resolver ever omits `mode`, fall back to the
        // same luminance rule it uses internally.
        let mode = mode.unwrap_or_else(|| {
            colors
                .get("background")
                .map(|bg| if bg.channel_sum() > 382 { Mode::Light } else { Mode::Dark })
                .unwrap_or(Mode::Dark)
        });

        Ok(Self { colors, mode })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn get(&self, key: &str) -> Option<Rgb> {
        self.colors.get(key).copied()
    }

    /// Look `key` up, falling back to `fallback` and finally to a hard default.
    /// Mirrors the resolver's own `<key> [fallback]` behaviour.
    pub fn get_or(&self, key: &str, fallback: &str, default: Rgb) -> Rgb {
        self.get(key).or_else(|| self.get(fallback)).unwrap_or(default)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, Rgb)> {
        self.colors.iter().map(|(k, v)| (k.as_str(), *v))
    }

    pub fn len(&self) -> usize {
        self.colors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.colors.is_empty()
    }

    // --- the handful of keys the browser chrome actually leans on ---

    pub fn background(&self) -> Rgb {
        self.get_or("background", "bg", Rgb::new(0x18, 0x18, 0x18))
    }

    pub fn foreground(&self) -> Rgb {
        self.get_or("foreground", "fg", Rgb::new(0xd0, 0xd0, 0xd0))
    }

    pub fn accent(&self) -> Rgb {
        self.get_or("accent", "blue", self.foreground())
    }

    pub fn selection(&self) -> Rgb {
        self.get_or("selection", "selection_background", self.accent())
    }

    pub fn muted(&self) -> Rgb {
        self.get_or("muted", "color8", self.foreground())
    }

    /// A last-resort palette for systems without Omarchy, so the browser still
    /// starts rather than refusing to launch.
    pub fn fallback() -> Self {
        let mut colors = BTreeMap::new();
        for (k, v) in [
            ("background", Rgb::new(0x18, 0x18, 0x18)),
            ("foreground", Rgb::new(0xd0, 0xd0, 0xd0)),
            ("accent", Rgb::new(0x61, 0xaf, 0xef)),
            ("selection", Rgb::new(0x3e, 0x44, 0x51)),
            ("muted", Rgb::new(0x5c, 0x63, 0x70)),
        ] {
            colors.insert(k.to_string(), v);
        }
        Self { colors, mode: Mode::Dark }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A trimmed capture of real `omarchy-theme-color --all` output.
    const SAMPLE: &str = "\
accent\t#61afef
background\t#080a0e
bg\t#080a0e
blue\t#61afef
color0\t#080a0e
color1\t#e06c75
foreground\t#abb2bf
mode\tdark
muted\t#5c6370
selection\t#3e4451
theme_type\tdark
";

    #[test]
    fn parses_resolver_output() {
        let p = Palette::from_resolver_output(SAMPLE).unwrap();
        assert_eq!(p.mode(), Mode::Dark);
        assert_eq!(p.background(), Rgb::new(0x08, 0x0a, 0x0e));
        assert_eq!(p.accent(), Rgb::new(0x61, 0xaf, 0xef));
        // `mode`/`theme_type` must not leak into the colour map.
        assert!(p.get("mode").is_none());
        assert!(p.get("theme_type").is_none());
    }

    #[test]
    fn detects_light_mode_by_luminance_when_absent() {
        let p =
            Palette::from_resolver_output("background\t#eff1f5\nforeground\t#4c4f69\n").unwrap();
        assert_eq!(p.mode(), Mode::Light);
    }

    #[test]
    fn honours_explicit_light_mode() {
        let p = Palette::from_resolver_output("background\t#000000\nmode\tlight\n").unwrap();
        assert_eq!(p.mode(), Mode::Light);
    }

    #[test]
    fn empty_output_is_an_error() {
        assert!(Palette::from_resolver_output("").is_err());
    }
}
