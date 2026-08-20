//! Notice when the Omarchy theme changes.
//!
//! `omarchy-theme-set` emits no D-Bus signal and no inotify broadcast. It stages
//! `current/next-theme`, swaps it in with `rm -rf current/theme && mv next-theme
//! theme`, writes `current/theme.name`, pushes a payload to its own Quickshell
//! bar over private IPC, and finally runs `omarchy-hook theme-set "$NAME"`.
//!
//! So there are exactly two integration points available to a third-party app:
//!
//! 1. a **`theme-set.d` hook**, which is the idiomatic one and gets the name; and
//! 2. **inotify**, as a fallback for when the hook isn't installed.
//!
//! The subtlety that bites people: the whole `theme` directory is deleted and
//! replaced, so a watch on `current/theme/colors.toml` dies on the first switch.
//! Watch the *parent* directory instead and react to `theme.name`, which is
//! rewritten in place after the swap — meaning `colors.toml` is already correct
//! by the time we see it.

use std::path::Path;
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::{Error, paths};

/// Theme changes can arrive twice — once from the hook, once from inotify — and
/// inotify itself fires several events per swap. Collapse anything inside this
/// window into a single notification.
const DEBOUNCE: Duration = Duration::from_millis(120);

/// Watch for theme changes, emitting the new theme slug.
///
/// The returned watcher must be kept alive; dropping it stops the watch.
pub fn watch_theme_changes() -> Result<(mpsc::Receiver<String>, impl Watcher), Error> {
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<()>();
    let (tx, rx) = mpsc::channel::<String>(4);

    let current = paths::current_dir();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let Ok(event) = res else { return };
        if is_theme_swap(&event) {
            let _ = raw_tx.send(());
        }
    })?;

    // The parent directory, never the theme directory itself.
    if current.exists() {
        watcher.watch(&current, RecursiveMode::NonRecursive)?;
    } else {
        tracing::warn!(path = %current.display(), "no Omarchy state directory; theme watching is inert");
    }

    tokio::spawn(async move {
        while raw_rx.recv().await.is_some() {
            // Drain the burst that a single swap produces.
            tokio::time::sleep(DEBOUNCE).await;
            while raw_rx.try_recv().is_ok() {}

            let name = crate::current_theme_name();
            if tx.send(name).await.is_err() {
                break;
            }
        }
    });

    Ok((rx, watcher))
}

/// Does this event mean the theme was swapped?
///
/// `theme.name` is rewritten in place, and `theme` itself arrives via `mv`, which
/// inotify reports as a rename into the directory.
fn is_theme_swap(event: &Event) -> bool {
    let touches = |name: &str| {
        event.paths.iter().any(|p| p.file_name().and_then(|n| n.to_str()) == Some(name))
    };

    match event.kind {
        EventKind::Create(_) | EventKind::Modify(_) => touches("theme.name") || touches("theme"),
        _ => false,
    }
}

/// The hook script that Omarchy runs on every theme change.
///
/// It is intentionally a one-liner back into our own CLI: the theme reload is an
/// ordinary command, so the hook, the shell, and an agent all reach it the same way.
pub fn hook_script(exe: &Path) -> String {
    format!(
        "#!/bin/bash\n\
         # Installed by oma-browse. Runs after every `omarchy theme set`.\n\
         # $1 is the new theme slug.\n\
         exec {} theme reload \"$1\"\n",
        shell_quote(&exe.display().to_string())
    )
}

/// Where the hook belongs. `omarchy-hook` runs every file in this directory,
/// sorted, skipping anything ending in `.sample`.
pub fn hook_path() -> std::path::PathBuf {
    paths::theme_set_hook_dir().join("oma-browse")
}

/// Install (or refresh) the theme-set hook. Idempotent.
pub fn install_hook(exe: &Path) -> Result<std::path::PathBuf, Error> {
    use std::os::unix::fs::PermissionsExt;

    let dir = paths::theme_set_hook_dir();
    std::fs::create_dir_all(&dir).map_err(|e| Error::Read(dir.clone(), e))?;

    let path = hook_path();
    let script = hook_script(exe);

    // Avoid rewriting an identical hook so we don't churn mtimes on every launch.
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == script) {
        return Ok(path);
    }

    std::fs::write(&path, &script).map_err(|e| Error::Read(path.clone(), e))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| Error::Read(path.clone(), e))?;
    Ok(path)
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn hook_invokes_our_own_cli() {
        let script = hook_script(&PathBuf::from("/usr/bin/oma-browse"));
        assert!(script.starts_with("#!/bin/bash\n"));
        assert!(script.contains("/usr/bin/oma-browse theme reload \"$1\""));
    }

    #[test]
    fn hook_quotes_awkward_paths() {
        let script = hook_script(&PathBuf::from("/home/some one/oma browse"));
        assert!(script.contains(r"'/home/some one/oma browse' theme reload"));
    }

    #[test]
    fn hook_is_not_a_sample_file() {
        // `omarchy-hook` skips anything ending in `.sample`.
        assert!(!hook_path().to_string_lossy().ends_with(".sample"));
    }
}
