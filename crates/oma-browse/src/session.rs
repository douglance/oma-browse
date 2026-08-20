//! The tabs that were open last time.
//!
//! Chrome calls this "continue where you left off". It is one file of URLs,
//! rewritten whenever the tab list changes, and read back on request -- there is
//! no scroll position, no form state and no back-forward stack in it, because
//! none of those survive a process boundary in WebKit anyway and a session that
//! restores three quarters of a page is worse than one that restores the page.
//!
//! Never written by an incognito window. That is the whole promise of the flag.

use std::path::PathBuf;
use std::sync::Arc;

use crate::state::AppState;

/// Enough to reopen a morning's work; past this it is an archive, and history
/// already is one.
const CAP: usize = 100;

fn path() -> PathBuf {
    crate::history::state_dir().join("session")
}

fn lock_path() -> PathBuf {
    crate::history::state_dir().join("session.lock")
}

/// Whether this process is the one allowed to write the session file.
///
/// Every `oma-browse` launch is its own process with its own tabs, and they all
/// share one state directory -- so without this the newest window's tick
/// overwrites the session of every other window, and closing a scratch window
/// wipes the session you actually wanted. First one up owns the file; the rest
/// still *read* it, so `tab restore` works everywhere.
///
/// A PID file rather than an advisory lock, because a lock would need a
/// dependency and a PID answers the question a lock cannot: whether the last
/// owner is still running, or was killed without cleaning up.
fn claim() -> bool {
    let file = lock_path();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mine = std::process::id();
    if let Ok(raw) = std::fs::read_to_string(&file)
        && let Ok(held) = raw.trim().parse::<u32>()
        && held != mine
        && std::path::Path::new(&format!("/proc/{held}")).exists()
    {
        tracing::debug!(owner = held, "another window owns the session file");
        return false;
    }
    std::fs::write(&file, mine.to_string()).is_ok()
}

/// What is worth reopening.
///
/// The start page is where a browser goes when it has nowhere else to be, so
/// restoring it would turn one blank tab into two.
fn worth_keeping(url: &str, base: Option<&url::Url>) -> bool {
    if url.is_empty() || url == "about:blank" {
        return false;
    }
    match base {
        Some(base) => !url.starts_with(base.as_str()),
        // Before the control plane's address is known, fall back to the shape of
        // it. A user's own loopback page is a rare thing to lose; ours is a
        // certain thing to duplicate.
        None => !url.starts_with("http://127.0.0.1:") && !url.starts_with("http://localhost:"),
    }
}

/// Write the current tab list out. Cheap enough to call on a tick.
pub async fn record(state: &Arc<AppState>) {
    if state.incognito() {
        return;
    }
    let base = state.base_url();
    let urls: Vec<String> = state
        .tabs
        .read()
        .await
        .list()
        .into_iter()
        .map(|t| t.url)
        .filter(|u| worth_keeping(u, base.as_ref()))
        .take(CAP)
        .collect();
    write(&urls);
}

fn write(urls: &[String]) {
    let file = path();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // An empty tab list is still a session: closing everything and restarting
    // should not bring back yesterday.
    let _ = std::fs::write(&file, urls.join("\n"));
}

/// The session as it was when this process started.
///
/// Snapshotted rather than re-read, and that is the whole point: the ticker
/// starts writing five seconds after launch, so by the time anyone asks for
/// "the tabs from last time" the file already holds *this* window's tabs. The
/// restart that should restore the session is otherwise the thing that destroys
/// it.
static PREVIOUS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Read the session off disk before anything can overwrite it.
///
/// Idempotent, and safe to skip: [`saved`] falls back to reading the file, which
/// is right for the CLI face, where the process is short-lived and never ticks.
pub fn init() {
    let _ = PREVIOUS.set(read());
}

/// The URLs saved last time, oldest tab first.
pub fn saved() -> Vec<String> {
    PREVIOUS.get().cloned().unwrap_or_else(read)
}

fn read() -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(path()) else { return Vec::new() };
    raw.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

/// Reopen the saved tabs. Returns how many were opened.
///
/// Every one opens in the background, including the first: restoring a session
/// must not yank the user away from whatever they launched the browser to look
/// at. Anything already open is skipped, so running this twice is not a way to
/// end up with two of everything.
pub async fn restore(state: &Arc<AppState>) -> usize {
    let already: Vec<String> =
        state.tabs.read().await.list().into_iter().map(|t| t.url).collect();

    let mut opened = 0;
    for url in saved() {
        if already.iter().any(|u| u == &url) {
            continue;
        }
        match crate::tabs::open(state, &url, true).await {
            Ok(_) => opened += 1,
            Err(e) => tracing::warn!(%url, error = %e, "could not restore a tab"),
        }
    }
    if opened > 0 {
        state.notify_tabs();
    }
    opened
}

/// Keep the session file in step with the tab list.
///
/// A tick rather than a hook on every mutation: the tab list changes from six
/// places -- open, close, select, reopen, a navigation, and a title arriving --
/// and a save that has to be remembered in six places is a save that will be
/// forgotten in one. Losing at most one tick's worth of a session is not worth
/// six chances to lose all of it.
pub fn spawn(state: Arc<AppState>) {
    if !claim() {
        tracing::info!("not recording the session; another window is already doing it");
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
        // The first tick fires immediately, which would write an empty session
        // over a real one before the first tab has settled.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            record(&state).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_own_pages_are_not_a_session() {
        let base: url::Url = "http://127.0.0.1:41234/".parse().unwrap();
        assert!(!worth_keeping("http://127.0.0.1:41234/start", Some(&base)));
        assert!(!worth_keeping("about:blank", Some(&base)));
        assert!(!worth_keeping("", Some(&base)));
        assert!(worth_keeping("https://omarchy.org", Some(&base)));
    }

    #[test]
    fn a_users_own_loopback_page_survives_a_known_base() {
        // Only *our* control plane is excluded, not loopback in general: a
        // developer's `localhost:3000` is exactly the tab worth restoring.
        let base: url::Url = "http://127.0.0.1:41234/".parse().unwrap();
        assert!(worth_keeping("http://localhost:3000/app", Some(&base)));
        assert!(worth_keeping("http://127.0.0.1:8080/", Some(&base)));
    }

    #[test]
    fn without_a_base_loopback_is_assumed_to_be_ours() {
        assert!(!worth_keeping("http://127.0.0.1:41234/start", None));
        assert!(worth_keeping("https://omarchy.org", None));
    }
}
