//! The tab model, and the webview operations behind it.
//!
//! Tab switching is `hide()`/`show()`, not repositioning. On Linux Tauri's
//! positioning API is inert, but visibility genuinely works: a hidden
//! `GtkWidget` drops out of `GtkBox` layout entirely, so the one visible content
//! webview expands to fill whatever the fixed-height chrome leaves.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use tauri::{AppHandle, Manager, WebviewUrl, Wry};
use tauri::webview::{Webview, WebviewBuilder};

use crate::state::AppState;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Tab {
    /// Stable identifier, also used to address the tab from the CLI and MCP.
    pub id: u32,
    /// The Tauri webview label backing this tab.
    pub label: String,
    pub url: String,
    pub title: String,
    pub active: bool,
}

#[derive(Debug, Default)]
pub struct Tabs {
    entries: Vec<TabEntry>,
    active: Option<u32>,
    next_id: u32,
}

#[derive(Debug, Clone)]
struct TabEntry {
    id: u32,
    label: String,
    url: String,
    title: String,
}

impl Tabs {
    pub fn allocate(&mut self, url: String) -> (u32, String) {
        let id = self.next_id;
        self.next_id += 1;
        let label = format!("tab-{id}");
        self.entries.push(TabEntry { id, label: label.clone(), url, title: String::new() });
        (id, label)
    }

    pub fn set_active(&mut self, id: u32) {
        if self.entries.iter().any(|t| t.id == id) {
            self.active = Some(id);
        }
    }

    pub fn active_id(&self) -> Option<u32> {
        self.active
    }

    pub fn active_label(&self) -> Option<String> {
        let id = self.active?;
        self.entries.iter().find(|t| t.id == id).map(|t| t.label.clone())
    }

    pub fn label_of(&self, id: u32) -> Option<String> {
        self.entries.iter().find(|t| t.id == id).map(|t| t.label.clone())
    }

    pub fn remove(&mut self, id: u32) -> Option<String> {
        let index = self.entries.iter().position(|t| t.id == id)?;
        let removed = self.entries.remove(index);

        if self.active == Some(id) {
            // Prefer the tab that slid into this slot, else the one before it —
            // the behaviour every other browser has trained people to expect.
            self.active = self
                .entries
                .get(index)
                .or_else(|| index.checked_sub(1).and_then(|i| self.entries.get(i)))
                .map(|t| t.id);
        }
        Some(removed.label)
    }

    pub fn update_title(&mut self, label: &str, title: String) {
        if let Some(t) = self.entries.iter_mut().find(|t| t.label == label) {
            t.title = title;
        }
    }

    pub fn update_url(&mut self, label: &str, url: String) {
        if let Some(t) = self.entries.iter_mut().find(|t| t.label == label) {
            t.url = url;
        }
    }

    pub fn list(&self) -> Vec<Tab> {
        self.entries
            .iter()
            .map(|t| Tab {
                id: t.id,
                label: t.label.clone(),
                url: t.url.clone(),
                title: if t.title.is_empty() { t.url.clone() } else { t.title.clone() },
                active: self.active == Some(t.id),
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The neighbour `delta` steps away, wrapping. Drives Ctrl-Tab.
    pub fn neighbour(&self, delta: i32) -> Option<u32> {
        if self.entries.is_empty() {
            return None;
        }
        let current = self
            .active
            .and_then(|id| self.entries.iter().position(|t| t.id == id))
            .unwrap_or(0) as i32;
        let len = self.entries.len() as i32;
        let next = (current + delta).rem_euclid(len) as usize;
        Some(self.entries[next].id)
    }

    pub fn nth(&self, index: usize) -> Option<u32> {
        self.entries.get(index).map(|t| t.id)
    }
}

/// Turn whatever the user typed into something navigable.
///
/// A bare host becomes https, anything that cannot be a host becomes a search.
pub fn resolve_input(input: &str) -> String {
    let raw = input.trim();
    if raw.is_empty() {
        return "about:blank".to_string();
    }
    if raw.starts_with("http://") || raw.starts_with("https://") || raw.starts_with("file://") {
        return raw.to_string();
    }

    // Looks like a host if it has no spaces and either a dot with a plausible
    // TLD, or is localhost (with optional port and path).
    let host = raw.split(['/', '?', '#']).next().unwrap_or(raw);
    let hostless_port = host.split(':').next().unwrap_or(host);
    let looks_like_host = !raw.contains(char::is_whitespace)
        && (hostless_port == "localhost"
            || (hostless_port.contains('.')
                && hostless_port
                    .rsplit('.')
                    .next()
                    .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))));

    if looks_like_host {
        format!("https://{raw}")
    } else {
        format!("https://duckduckgo.com/?q={}", urlencode(raw))
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Operations that touch real webviews
// ---------------------------------------------------------------------------

fn window(app: &AppHandle<Wry>) -> Result<tauri::window::Window<Wry>> {
    app.get_window("main").context("the main window has gone away")
}

fn webview(app: &AppHandle<Wry>, label: &str) -> Result<Webview<Wry>> {
    app.get_webview(label).with_context(|| format!("no webview labelled {label}"))
}

/// Open a URL in a new tab.
pub async fn open(state: &Arc<AppState>, input: &str, background: bool) -> Result<Tab> {
    let app = state.app_handle().context("the window is not up yet")?;
    let url = resolve_input(input);
    let parsed: url::Url = url.parse().with_context(|| format!("{url} is not a URL"))?;

    let (id, label) = {
        let mut tabs = state.tabs.write().await;
        tabs.allocate(url.clone())
    };

    let win = window(&app)?;
    let script = state.page_script().await;
    let bg = state.background_color().await;

    let builder = WebviewBuilder::new(&label, WebviewUrl::External(parsed))
        .auto_resize()
        .transparent(bg.3 < 255)
        .background_color(bg)
        .initialization_script(&script);

    let view = win
        .add_child(
            crate::window::instrument(builder, state.clone()),
            tauri::LogicalPosition::new(0.0, 0.0),
            tauri::LogicalSize::new(800.0, 600.0),
        )
        .context("could not create the tab's webview")?;

    // Tauri packs new webviews into the window box, which by now holds only the
    // overlay; move it into the content stack so it stacks with the other tabs.
    crate::layout::adopt_tab(&view)?;

    if background {
        // Newly packed webviews are visible and expanding, which would split the
        // content area with the active tab. Hide it immediately.
        webview(&app, &label)?.hide().ok();
    } else {
        select(state, id).await?;
    }

    let tabs = state.tabs.read().await;
    tabs.list()
        .into_iter()
        .find(|t| t.id == id)
        .ok_or_else(|| anyhow!("tab {id} vanished during creation"))
}

/// Make one tab visible and hide the rest.
pub async fn select(state: &Arc<AppState>, id: u32) -> Result<()> {
    let app = state.app_handle().context("the window is not up yet")?;
    let mut tabs = state.tabs.write().await;
    let target = tabs.label_of(id).ok_or_else(|| anyhow!("no tab with id {id}"))?;

    for tab in tabs.list() {
        if let Ok(view) = webview(&app, &tab.label) {
            let _ = if tab.label == target { view.show() } else { view.hide() };
        }
    }
    tabs.set_active(id);
    Ok(())
}

pub async fn close(state: &Arc<AppState>, id: Option<u32>) -> Result<Option<u32>> {
    let app = state.app_handle().context("the window is not up yet")?;
    let id = match id.or_else(|| futures_active(state)) {
        Some(id) => id,
        None => return Ok(None),
    };

    let (label, next) = {
        let mut tabs = state.tabs.write().await;
        let label = tabs.remove(id).ok_or_else(|| anyhow!("no tab with id {id}"))?;
        (label, tabs.active_id())
    };

    if let Ok(view) = webview(&app, &label) {
        let _ = view.close();
    }
    if let Some(next) = next {
        select(state, next).await?;
    }
    Ok(next)
}

/// Read the active id without awaiting, for the common `close(None)` path.
fn futures_active(state: &Arc<AppState>) -> Option<u32> {
    state.tabs.try_read().ok().and_then(|t| t.active_id())
}

/// Navigate the active tab.
pub async fn navigate(state: &Arc<AppState>, input: &str) -> Result<String> {
    let app = state.app_handle().context("the window is not up yet")?;
    let url = resolve_input(input);
    let parsed: url::Url = url.parse().with_context(|| format!("{url} is not a URL"))?;

    let label = {
        let tabs = state.tabs.read().await;
        tabs.active_label()
    };

    match label {
        Some(label) => {
            webview(&app, &label)?.navigate(parsed).context("navigation failed")?;
            state.tabs.write().await.update_url(&label, url.clone());
            Ok(url)
        }
        // No tabs left: navigating should open one rather than fail.
        None => {
            let tab = open(state, input, false).await?;
            Ok(tab.url)
        }
    }
}

/// History and reload, via the raw WebKitGTK view.
///
/// Tauri exposes `reload` but not back/forward or stop; wry 0.56 has them and
/// does not re-export them either. `with_webview` is the way through.
pub async fn history(state: &Arc<AppState>, action: HistoryAction) -> Result<()> {
    let app = state.app_handle().context("the window is not up yet")?;
    let label = state
        .tabs
        .read()
        .await
        .active_label()
        .ok_or_else(|| anyhow!("there is no active tab"))?;
    let view = webview(&app, &label)?;

    match action {
        HistoryAction::Reload => view.reload().context("reload failed")?,
        #[cfg(target_os = "linux")]
        other => {
            view.with_webview(move |platform| {
                use webkit2gtk::WebViewExt;
                let w = platform.inner();
                match other {
                    HistoryAction::Back => w.go_back(),
                    HistoryAction::Forward => w.go_forward(),
                    HistoryAction::Stop => w.stop_loading(),
                    HistoryAction::Reload => w.reload(),
                }
            })
            .context("could not reach the webview")?;
        }
        #[cfg(not(target_os = "linux"))]
        _ => return Err(anyhow!("unsupported on this platform")),
    }
    Ok(())
}

/// Evaluate JavaScript in the active tab and return its result.
///
/// `eval_with_callback` landed in Tauri 2.11; before that there was no way to
/// get a value back out of a webview. The result arrives as a JSON string.
pub async fn eval(state: &Arc<AppState>, js: &str) -> Result<String> {
    let app = state.app_handle().context("the window is not up yet")?;
    let label = state
        .tabs
        .read()
        .await
        .active_label()
        .ok_or_else(|| anyhow!("there is no active tab"))?;
    let view = webview(&app, &label)?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let tx = std::sync::Mutex::new(Some(tx));

    view.eval_with_callback(js, move |value| {
        if let Ok(mut slot) = tx.lock()
            && let Some(tx) = slot.take()
        {
            let _ = tx.send(value);
        }
    })
    .context("could not evaluate in the webview")?;

    tokio::time::timeout(std::time::Duration::from_secs(5), rx)
        .await
        .context("the page did not answer within 5s")?
        .context("the webview dropped the evaluation")
}

#[derive(Debug, Clone, Copy)]
pub enum HistoryAction {
    Back,
    Forward,
    Reload,
    Stop,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_hosts_become_https() {
        assert_eq!(resolve_input("github.com"), "https://github.com");
        assert_eq!(resolve_input("github.com/rust-lang"), "https://github.com/rust-lang");
        assert_eq!(resolve_input("localhost:3000"), "https://localhost:3000");
    }

    #[test]
    fn explicit_schemes_pass_through() {
        assert_eq!(resolve_input("http://example.com"), "http://example.com");
        assert_eq!(resolve_input("https://example.com"), "https://example.com");
    }

    #[test]
    fn prose_becomes_a_search() {
        assert_eq!(
            resolve_input("how do i tile windows"),
            "https://duckduckgo.com/?q=how+do+i+tile+windows"
        );
        // A single word with no dot is a search, not a host.
        assert!(resolve_input("omarchy").starts_with("https://duckduckgo.com/?q="));
    }

    #[test]
    fn empty_input_is_blank_not_a_search() {
        assert_eq!(resolve_input("   "), "about:blank");
    }

    #[test]
    fn tab_cycling_wraps_both_ways() {
        let mut tabs = Tabs::default();
        let (a, _) = tabs.allocate("a".into());
        let (_b, _) = tabs.allocate("b".into());
        let (c, _) = tabs.allocate("c".into());
        tabs.set_active(a);
        assert_eq!(tabs.neighbour(-1), Some(c), "wraps backwards from the first");
        tabs.set_active(c);
        assert_eq!(tabs.neighbour(1), Some(a), "wraps forwards from the last");
    }

    #[test]
    fn closing_the_active_tab_picks_a_neighbour() {
        let mut tabs = Tabs::default();
        let (a, _) = tabs.allocate("a".into());
        let (b, _) = tabs.allocate("b".into());
        let (c, _) = tabs.allocate("c".into());

        tabs.set_active(b);
        tabs.remove(b);
        assert_eq!(tabs.active_id(), Some(c), "falls through to the tab that slid in");

        tabs.set_active(c);
        tabs.remove(c);
        assert_eq!(tabs.active_id(), Some(a), "falls back to the previous tab at the end");
    }
}
