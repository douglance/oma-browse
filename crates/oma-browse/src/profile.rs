//! A second browser, in the same binary.
//!
//! Work and personal. A logged-out window for reproducing a bug a user reported.
//! A throwaway identity for a staging environment that will not let you have two
//! sessions at once. Every browser solves this eventually, and most solve it
//! with a profile picker; here it is a flag, because a window is already a
//! process (see [`crate::window::spawn`]) and a process is already the isolation
//! boundary.
//!
//! `--profile <name>` moves four things at once, and it has to be all four or
//! none:
//!
//! * the config file, so a profile can have its own home page and keys;
//! * the state directory -- history, bookmarks, downloads, permissions;
//! * the control socket directory, so `oma-browse --profile work tab list` talks
//!   to the work window and not to whichever window was focused last;
//! * WebKit's own data directory, which is the one that actually holds the
//!   cookies. Without it two profiles would share every login, which is the
//!   thing somebody asking for a profile is asking to avoid.
//!
//! Read once, before anything that consults a path, and never changed after:
//! half the process on one profile and half on another would be worse than no
//! profiles at all.

use std::path::PathBuf;
use std::sync::OnceLock;

static NAME: OnceLock<Option<String>> = OnceLock::new();

/// The profile this process is running as, if any.
pub fn name() -> Option<&'static str> {
    NAME.get().and_then(Option::as_deref)
}

/// Fix the profile for the life of the process. Later calls are ignored, which
/// is what makes [`name`] safe to call from anywhere.
pub fn set(profile: Option<String>) {
    let _ = NAME.set(profile.filter(|p| !p.trim().is_empty()));
}

/// Take `--profile <name>` out of an argv, the way
/// [`crate::control::take_window_flag`] takes `--window`.
///
/// It is taken out rather than passed on because it is a question about *which*
/// browser, answered here, before anything is sent. A browser receiving it would
/// have nothing to do with it.
pub fn take_flag(argv: Vec<String>) -> (Vec<String>, Option<String>) {
    let mut kept = Vec::with_capacity(argv.len());
    let mut found = None;
    let mut iter = argv.into_iter();
    while let Some(word) = iter.next() {
        if let Some(rest) = word.strip_prefix("--profile=") {
            found = Some(rest.to_string());
            continue;
        }
        if word == "--profile" {
            match iter.next() {
                Some(value) => found = Some(value),
                // A trailing `--profile` with nothing after it is a typo, and
                // incurs writes a better message about it than this could.
                None => kept.push(word),
            }
            continue;
        }
        kept.push(word);
    }
    (kept, found)
}

/// A name safe to put in a path.
///
/// Profiles are named by whoever types the flag, and `--profile ../../etc` must
/// name a directory called `.._..etc` rather than escaping into one. Anything
/// that is not a letter, a digit, a dash or an underscore becomes a dash.
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() { "profile".to_string() } else { trimmed }
}

/// Add this profile's subdirectory to a base directory, or hand it back
/// unchanged when there is no profile.
///
/// The default profile keeps the old paths exactly, which is not tidiness: a
/// browser that moved everybody's history the day profiles landed would be a
/// browser that lost everybody's history.
pub fn within(base: PathBuf) -> PathBuf {
    match name() {
        Some(profile) => base.join("profiles").join(sanitize(profile)),
        None => base,
    }
}

/// Where WebKit keeps this profile's cookies and local storage.
///
/// `None` for the default profile, where the answer is "wherever wry already
/// put them" -- naming it would move an existing browser's logins.
pub fn data_dir() -> Option<PathBuf> {
    name().map(|_| crate::history::state_dir().join("webkit"))
}

/// Point a webview at this profile's cookie jar.
///
/// A no-op for the default profile, deliberately: naming the directory that wry
/// already chose would be harmless, and naming a *different* one would log
/// everybody out on upgrade. Not naming it at all is the only version of this
/// that cannot go wrong.
pub fn in_profile<R: tauri::Runtime>(
    builder: tauri::webview::WebviewBuilder<R>,
) -> tauri::webview::WebviewBuilder<R> {
    match data_dir() {
        Some(dir) => builder.data_directory(dir),
        None => builder,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    #[test]
    fn the_flag_is_taken_out_in_both_spellings() {
        let (kept, found) = take_flag(argv(&["--profile", "work", "tab", "list"]));
        assert_eq!(found.as_deref(), Some("work"));
        assert_eq!(kept, argv(&["tab", "list"]));

        let (kept, found) = take_flag(argv(&["tab", "list", "--profile=work"]));
        assert_eq!(found.as_deref(), Some("work"));
        assert_eq!(kept, argv(&["tab", "list"]));
    }

    #[test]
    fn an_argv_without_the_flag_is_untouched() {
        let (kept, found) = take_flag(argv(&["page", "console", "--level", "warn"]));
        assert!(found.is_none());
        assert_eq!(kept, argv(&["page", "console", "--level", "warn"]));
    }

    #[test]
    fn a_trailing_flag_is_left_for_incurs_to_complain_about() {
        let (kept, found) = take_flag(argv(&["tab", "list", "--profile"]));
        assert!(found.is_none());
        assert_eq!(kept, argv(&["tab", "list", "--profile"]));
    }

    #[test]
    fn a_profile_name_cannot_leave_its_directory() {
        assert_eq!(sanitize("work"), "work");
        assert_eq!(sanitize("../../etc"), "etc");
        assert_eq!(sanitize("a/b"), "a-b");
        assert_eq!(sanitize("  spaced name "), "spaced-name");
        assert_eq!(sanitize("..."), "profile");
        assert_eq!(sanitize(""), "profile");
    }
}
