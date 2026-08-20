//! Where Omarchy keeps live theme state.
//!
//! Note this is `~/.local/state/omarchy/current`, *not* `~/.config/omarchy/current`,
//! which does not exist on Omarchy 4.x.

use std::path::PathBuf;

/// The resolver Omarchy ships. Every consumer is expected to go through it so
/// that templates, OSC sequences, previews and third-party apps all agree on the
/// same palette. See the header of the script itself for that contract.
pub const THEME_COLOR_BIN: &str = "/usr/share/omarchy/bin/omarchy-theme-color";

/// `$XDG_STATE_HOME/omarchy/current`, falling back to `~/.local/state`.
pub fn current_dir() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"));
    state.join("omarchy/current")
}

/// The live theme directory. Rebuilt wholesale on every theme switch: Omarchy
/// stages `current/next-theme`, then `rm -rf current/theme && mv next-theme theme`.
/// Never hold an inotify watch on a path *inside* here across a switch.
pub fn theme_dir() -> PathBuf {
    current_dir().join("theme")
}

pub fn colors_toml() -> PathBuf {
    theme_dir().join("colors.toml")
}

pub fn shell_toml() -> PathBuf {
    theme_dir().join("shell.toml")
}

/// Rewritten in place (`echo >`) after the directory swap, which makes it the
/// most reliable single trigger to watch.
pub fn theme_name_file() -> PathBuf {
    current_dir().join("theme.name")
}

/// Symlink to the active background. May point at a video (`.mp4`).
pub fn background_link() -> PathBuf {
    current_dir().join("background")
}

/// The user-level shell override, layered *over* the theme's own shell.toml.
pub fn user_shell_toml() -> PathBuf {
    config_dir().join("shell.toml")
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("omarchy")
}

/// Where a `theme-set` hook must be dropped to be run by `omarchy-hook`.
/// Files ending in `.sample` are skipped by the runner.
pub fn theme_set_hook_dir() -> PathBuf {
    config_dir().join("hooks/theme-set.d")
}

pub(crate) fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}
