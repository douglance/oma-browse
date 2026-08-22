//! The tab model, and the webview operations behind it.
//!
//! Tab switching is `hide()`/`show()`, not repositioning. On Linux Tauri's
//! positioning API is inert, but visibility genuinely works: a hidden
//! `GtkWidget` drops out of `GtkBox` layout entirely, so the one visible content
//! webview expands to fill whatever the fixed-height chrome leaves.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use tauri::webview::{Webview, WebviewBuilder};
use tauri::{AppHandle, Manager, WebviewUrl, Wry};

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
    /// The site's favicon as a `data:` URL, or empty until one arrives.
    ///
    /// Skipped everywhere the tab is serialised: the strip renders it inline,
    /// but a few hundred bytes of base64 per tab is noise in `tab list` output
    /// and worse in an MCP reply, where it would be spent tokens.
    #[serde(skip)]
    #[schemars(skip)]
    pub icon: String,
}

/// How many closed tabs are worth remembering, unless the config says otherwise.
pub const CLOSED_STACK: usize = 32;

#[derive(Debug)]
pub struct Tabs {
    entries: Vec<TabEntry>,
    active: Option<u32>,
    next_id: u32,
    /// URLs of recently closed tabs, most recent last.
    closed: Vec<String>,
    /// How deep that stack goes; see [`crate::config::Tabs::reopen_depth`].
    reopen_depth: usize,
}

impl Default for Tabs {
    fn default() -> Self {
        Self::with_reopen_depth(CLOSED_STACK)
    }
}

impl Tabs {
    pub fn with_reopen_depth(reopen_depth: usize) -> Self {
        Self { entries: Vec::new(), active: None, next_id: 0, closed: Vec::new(), reopen_depth }
    }
}

#[derive(Debug, Clone)]
struct TabEntry {
    id: u32,
    label: String,
    url: String,
    title: String,
    /// See [`Tab::icon`]. Kept per tab rather than per origin: WebKit already
    /// has the origin-keyed cache, and this is only what to paint right now.
    icon: String,
    /// How far WebKit says this tab's load has got, or `None` when it is not
    /// loading -- which is the resting state and the great majority of the time.
    ///
    /// WebKit's own estimate rather than anything we count: it is the only
    /// number that knows how many subresources are still outstanding, and
    /// counting them ourselves would mean a second resource-load listener
    /// arriving at a worse answer. See [`crate::progress`].
    progress: Option<f64>,
}

impl Tabs {
    pub fn allocate(&mut self, url: String) -> (u32, String) {
        let id = self.next_id;
        self.next_id += 1;
        let label = format!("tab-{id}");
        self.entries.push(TabEntry {
            id,
            label: label.clone(),
            url,
            title: String::new(),
            icon: String::new(),
            progress: None,
        });
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

        // Remember where it was pointing, so Ctrl-Shift-T can bring it back.
        // Only the URL: a webview cannot be resurrected, and reloading the page
        // is what every other browser does here too.
        if !removed.url.is_empty() && removed.url != "about:blank" {
            self.closed.push(removed.url.clone());
            // A bounded stack. Nobody reopens the fortieth tab back, and this
            // is the only thing in the model that would otherwise grow forever.
            while self.closed.len() > self.reopen_depth {
                self.closed.remove(0);
            }
        }

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

    /// The most recently closed tab's URL, taken off the stack.
    pub fn take_closed(&mut self) -> Option<String> {
        self.closed.pop()
    }

    /// The URL a webview is currently showing, for attaching a title to the
    /// history entry the page load created.
    pub fn url_for(&self, label: &str) -> Option<String> {
        self.entries.iter().find(|t| t.label == label).map(|t| t.url.clone())
    }

    pub fn update_title(&mut self, label: &str, title: String) {
        if let Some(t) = self.entries.iter_mut().find(|t| t.label == label) {
            t.title = title;
        }
    }

    pub fn update_url(&mut self, label: &str, url: String) {
        if let Some(t) = self.entries.iter_mut().find(|t| t.label == label) {
            // A new page means the old page's icon is wrong. Clearing it here
            // rather than waiting for the next one to arrive is the difference
            // between a stale favicon and a blank slot for a moment.
            if t.url != url {
                t.icon.clear();
            }
            t.url = url;
        }
    }

    /// Record a favicon, reporting whether it actually changed.
    ///
    /// WebKit re-notifies on every load, including reloads and same-icon
    /// navigations within a site, and every change reloads the strip -- so the
    /// answer here is what keeps a page of redirects from thrashing it.
    pub fn update_icon(&mut self, label: &str, icon: String) -> bool {
        match self.entries.iter_mut().find(|t| t.label == label) {
            Some(t) if t.icon != icon => {
                t.icon = icon;
                true
            }
            _ => false,
        }
    }

    /// Record how far a tab's load has got, reporting whether the strip's bar
    /// has to be repainted for it.
    ///
    /// Two filters, because this is called from a GObject notification that
    /// fires freely and the answer costs a webview `eval` every time it is yes:
    /// only the active tab is drawn, and only movement of a whole percent
    /// counts. A 2px bar across a 1600px window cannot show anything finer, so
    /// the ones dropped here are repaints nobody could have seen.
    pub fn set_progress(&mut self, label: &str, value: Option<f64>) -> bool {
        let active = self.active_label().as_deref() == Some(label);
        let Some(entry) = self.entries.iter_mut().find(|t| t.label == label) else {
            return false;
        };

        let value = match (entry.progress, value) {
            (None, Some(fraction)) => Some(fraction.min(OPENING)),
            (_, other) => other,
        };
        let moved = match (entry.progress, value) {
            (Some(was), Some(now)) => percent(was) != percent(now),
            (was, now) => was.is_some() != now.is_some(),
        };
        entry.progress = value;
        moved && active
    }

    /// What the strip's load bar should be showing: the active tab's progress,
    /// or `None` when nothing is loading. Background tabs are deliberately not
    /// in it -- one bar cannot answer for several loads at once, and the load
    /// worth watching is the one whose page is on screen.
    pub fn active_progress(&self) -> Option<f64> {
        let id = self.active?;
        self.entries.iter().find(|t| t.id == id).and_then(|t| t.progress)
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
                icon: t.icon.clone(),
            })
            .collect()
    }

    /// The neighbour `delta` steps away, wrapping. Drives Ctrl-Tab.
    pub fn neighbour(&self, delta: i32) -> Option<u32> {
        if self.entries.is_empty() {
            return None;
        }
        let current =
            self.active.and_then(|id| self.entries.iter().position(|t| t.id == id)).unwrap_or(0)
                as i32;
        let len = self.entries.len() as i32;
        let next = (current + delta).rem_euclid(len) as usize;
        Some(self.entries[next].id)
    }

    pub fn nth(&self, index: usize) -> Option<u32> {
        self.entries.get(index).map(|t| t.id)
    }

    /// The tab at a 1-based position, counting from the end when `pos` is
    /// negative. Drives Ctrl-1..Ctrl-9, where every browser makes the last
    /// chord mean "the last tab" rather than "the ninth".
    ///
    /// Out of range is `None` rather than a clamp: Ctrl-5 with four tabs open
    /// should do nothing, not land somewhere the user did not aim for.
    pub fn by_position(&self, pos: i32) -> Option<u32> {
        let len = self.entries.len() as i32;
        let index = if pos < 0 { len + pos } else { pos - 1 };
        if index < 0 {
            return None;
        }
        self.nth(index as usize)
    }
}

/// Turn whatever the user typed into something navigable.
///
/// A bare host becomes https, anything that cannot be a host becomes a search.
pub fn resolve_input(input: &str, search: &str) -> String {
    let raw = input.trim();
    if raw.is_empty() {
        return "about:blank".to_string();
    }
    if raw.starts_with("http://") || raw.starts_with("https://") || raw.starts_with("file://") {
        return raw.to_string();
    }
    // The browser's own pages. `start_page` hands this straight back to
    // `tab open`, and without it a new tab searched the web for the address of
    // its own start page. A tab that asks for the *palette* still gets nowhere:
    // `window::may_see` only serves the chrome to the chrome.
    if raw.starts_with(&format!("{}://", crate::window::CHROME_SCHEME)) {
        return raw.to_string();
    }

    // `:3000` is the dev server. Nobody has ever typed a bare colon-and-port
    // into an address bar meaning to search the web for it, and everybody who
    // runs a dev server types it several times a day.
    //
    // `http`, not `https`: a dev server that speaks TLS is the exception, and
    // guessing `https` here would turn the shortcut into an error page.
    if let Some(rest) = raw.strip_prefix(':') {
        let port = rest.split(['/', '?', '#']).next().unwrap_or("");
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            return format!("http://localhost:{rest}");
        }
    }

    // Looks like a host if it has no spaces and either a dot with a plausible
    // TLD, or is localhost (with optional port and path).
    let host = raw.split(['/', '?', '#']).next().unwrap_or(raw);
    let hostless_port = host.split(':').next().unwrap_or(host);
    let looks_like_host = !raw.contains(char::is_whitespace)
        && (hostless_port == "localhost"
            || (hostless_port.contains('.')
                && hostless_port.rsplit('.').next().is_some_and(|tld| {
                    tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
                })));

    if looks_like_host {
        return format!("https://{raw}");
    }

    // `{query}` rather than a bare append, because plenty of engines want the
    // terms in the middle of the URL rather than at the end of it. A template
    // without the placeholder still works: the terms go on the end, which is
    // what a half-written `?q=` in the config file was reaching for anyway.
    let encoded = urlencode(raw);
    if search.contains("{query}") {
        search.replace("{query}", &encoded)
    } else {
        format!("{search}{encoded}")
    }
}

/// The most a load may claim on its first reading.
///
/// WebKit does not clear `estimated-load-progress` at the moment a load
/// *starts*; it clears it a beat later. So the first notification of a new load
/// still carries the last one's number, which is 1.0 -- and taken at face value
/// that is a full bar flashed across the window at the instant a page begins to
/// load, saying the opposite of what it means. Nothing has arrived yet whatever
/// the property still holds, so the opening reading is capped rather than
/// trusted.
const OPENING: f64 = 0.1;

/// A load fraction rounded to what a progress bar can actually draw.
fn percent(fraction: f64) -> i32 {
    (fraction.clamp(0.0, 1.0) * 100.0).round() as i32
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
    let url = resolve_input(input, &state.config.search);
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
        .initialization_script(&script)
        // Every tab, not just the first one the window was built with.
        //
        // Without this an incognito window is private for exactly one tab:
        // wry gives an `incognito` webview its own ephemeral web context, and
        // one built without the flag gets the shared, persistent one instead.
        // Measured before the flag was added here -- a cookie set in the second
        // tab of an incognito window was still there in an ordinary window
        // after a restart, which is the opposite of what the word promises.
        //
        // The cost is that each incognito tab has its *own* ephemeral context,
        // so a login does not carry from one to the next the way it does in
        // Chrome. Tauri offers no way to share one context between webviews,
        // and of the two failures -- "logged out in the new tab" against
        // "not actually private" -- only one of them is a lie.
        .incognito(state.incognito());
    let builder = crate::profile::in_profile(builder);

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

    // The download signal is on the shared web context, so this registers
    // once however many tabs ask; see `downloads::watch`.
    if let Err(e) = crate::downloads::watch(&view, state.clone()) {
        tracing::warn!(error = %e, "not watching downloads");
    }
    if let Err(e) = crate::favicon::watch(&view, state.clone()) {
        tracing::warn!(error = %e, tab = %label, "not watching this tab's favicon");
    }
    if let Err(e) = crate::progress::watch(&view, state.clone()) {
        tracing::warn!(error = %e, tab = %label, "this tab loads without a progress bar");
    }
    if let Err(e) = crate::crash::watch(&view, state.clone()) {
        tracing::warn!(error = %e, tab = %label, "this tab will die silently if its process does");
    }
    if let Err(e) = crate::engine::configure(&view, state.clone()) {
        tracing::warn!(error = %e, tab = %label, "this tab kept WebKit's own settings");
    }
    if let Err(e) = crate::policy::install(&view, state.clone()) {
        tracing::warn!(error = %e, tab = %label, "this tab answers pages with WebKit's defaults");
    }
    if let Err(e) = crate::inspect::install(&view, state.clone()) {
        tracing::warn!(error = %e, tab = %label, "this tab keeps no console or network log");
    }
    if let Err(e) = crate::blocker::install(&view, state.clone()) {
        tracing::warn!(error = %e, tab = %label, "this tab blocks nothing");
    }
    if let Err(e) = watch_url(&view, state.clone()) {
        tracing::warn!(error = %e, tab = %label, "this tab will not notice in-page navigation");
    }

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

/// Keep the tab model in step with where the page has actually gone.
///
/// Tauri's `on_page_load` is a *document* event, so a single-page application
/// that navigates with `history.pushState` is invisible to it -- and this
/// browser then reports the wrong page from every command that asks the tab
/// model where it is. Measured: clicking through from a search page left
/// `tab list` showing the search URL while the page was on the video, and the
/// *video's* title was recorded into history against the *search* URL. Not a
/// stale field: a wrong pair, written down.
///
/// WebKit's `uri` property is the honest answer, because it changes for a
/// pushState as well as for a load. It also changes several times *during* an
/// ordinary load -- provisional, then committed, then any redirect -- which is
/// why `on_page_load` was the natural place to record history from and why this
/// does not take that job away from it. `is_loading()` is what tells the two
/// apart: a pushState happens with nothing in flight.
#[cfg(target_os = "linux")]
pub fn watch_url<R: tauri::Runtime>(view: &Webview<R>, state: Arc<AppState>) -> Result<()> {
    let label = view.label().to_string();
    view.with_webview(move |platform| {
        use webkit2gtk::WebViewExt;

        platform.inner().connect_uri_notify(move |webview| {
            let Some(uri) = webview.uri() else { return };
            let url = uri.to_string();
            // The browser's own pages are not somewhere the user went.
            if url.is_empty()
                || url == "about:blank"
                || url.starts_with(&format!("{}://", crate::window::CHROME_SCHEME))
            {
                return;
            }
            // Mid-load the URI churns; `on_page_load` records the one that
            // sticks. A URI that changes with nothing loading is a pushState,
            // and it is the only kind nothing else will hear about.
            let quiet = !webview.is_loading();

            let state = state.clone();
            let label = label.clone();
            state.runtime().spawn(async move {
                let moved = {
                    let mut tabs = state.tabs.write().await;
                    let before = tabs.url_for(&label);
                    if before.as_deref() == Some(url.as_str()) {
                        false
                    } else {
                        tabs.update_url(&label, url.clone());
                        true
                    }
                };
                if !moved {
                    return;
                }
                if quiet && state.keeps_history() {
                    let mut history = state.history.write().await;
                    history.record(&url, crate::history::now());
                    history.flush();
                }
                state.notify_tabs();
            });
        });
    })
    .context("could not reach the webview to follow its URL")
}

#[cfg(not(target_os = "linux"))]
pub fn watch_url<R: tauri::Runtime>(_view: &Webview<R>, _state: Arc<AppState>) -> Result<()> {
    Ok(())
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
    // The tab's console and network log go with it. Keeping them would mean a
    // browser open for a day remembered every page it had ever closed.
    if let Ok(mut inspector) = state.inspector.lock() {
        inspector.clear(Some(&label));
    }
    // Likewise anything recorded about its web process dying.
    state.forget_crash(&label);
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
    let url = resolve_input(input, &state.config.search);
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
/// Chrome's zoom ladder, so that stepping feels like every other browser rather
/// than like a slider. Anything WebKit already holds that is off the ladder --
/// set through `page zoom --level` -- steps to the next rung in the direction
/// asked for.
pub const ZOOM_STEPS: [f64; 16] =
    [0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0];

#[derive(Clone, Copy, Debug)]
pub enum ZoomChange {
    In,
    Out,
    Reset,
    Set(f64),
}

/// The next rung in the direction asked for, or the end of the ladder.
///
/// The ladder is `tabs.zoom_steps`, which need not be Chrome's and need not be
/// sorted by the person who wrote it -- so it is sorted here rather than
/// trusting the file. An empty one leaves zoom alone rather than dividing by a
/// ladder with no rungs.
fn stepped(steps: &[f64], current: f64, up: bool) -> f64 {
    let mut ladder: Vec<f64> = steps.iter().copied().filter(|s| *s > 0.0).collect();
    ladder.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let Some((&first, &last)) = ladder.first().zip(ladder.last()) else { return current };

    if up {
        ladder.iter().copied().find(|s| *s > current + 1e-6).unwrap_or(last)
    } else {
        ladder.iter().copied().rev().find(|s| *s < current - 1e-6).unwrap_or(first)
    }
}

/// Zoom the active tab, returning the level it settled on.
///
/// Per tab, because that is where WebKit keeps it. Chrome remembers zoom per
/// origin and reapplies it on the next visit; that needs somewhere to persist a
/// map, and is a separate thing from the keys doing what the keys should do.
/// What to do with the find controller.
#[derive(Clone, Debug)]
pub enum FindAction {
    /// Start a fresh search, highlighting every hit and jumping to the first.
    Search(String),
    Next,
    Previous,
    /// Drop the highlight and forget the term.
    Stop,
}

/// Search the active page.
///
/// WebKit keeps the search state on the webview's own `FindController`, which
/// is why "next" needs no argument: the controller still holds the term from
/// the last `search`. That also means stopping matters -- the highlight
/// survives navigation otherwise.
/// Search the active page, and say how many matches there were.
///
/// `None` when the number is not knowable: a `next`/`previous`/`clear`, a
/// platform without WebKit, or a page that did not answer in time. A counter
/// that guesses zero when it simply has not heard back is worse than one that
/// says nothing.
pub async fn find(state: &Arc<AppState>, action: FindAction) -> Result<Option<u32>> {
    let app = state.app_handle().context("the window is not up yet")?;
    let label =
        state.tabs.read().await.active_label().ok_or_else(|| anyhow!("there is no active tab"))?;
    let view = webview(&app, &label)?;

    #[cfg(target_os = "linux")]
    {
        let counting = matches!(action, FindAction::Search(_));
        let (tx, rx) = tokio::sync::oneshot::channel::<u32>();
        let tx = std::sync::Mutex::new(Some(tx));

        view.with_webview(move |platform| {
            use gtk::glib::prelude::*;
            use webkit2gtk::{FindController, FindControllerExt, FindOptions, WebViewExt};

            let Some(finder): Option<FindController> = platform.inner().find_controller() else {
                tracing::warn!("this webview has no find controller");
                return;
            };
            match action {
                FindAction::Search(text) => {
                    // Case-insensitive and wrapping, which is what every
                    // browser's Ctrl-F does and what anyone expects.
                    let options = FindOptions::CASE_INSENSITIVE | FindOptions::WRAP_AROUND;

                    // The count arrives on a signal rather than from the call,
                    // so the handler is connected for exactly one answer and
                    // then taken off again. Connecting once per webview instead
                    // would leave a handler firing into a channel nobody is
                    // listening on for the rest of the tab's life.
                    let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
                    let mine = slot.clone();
                    let id = finder.connect_counted_matches(move |finder, count| {
                        if let Ok(mut held) = tx.lock()
                            && let Some(tx) = held.take()
                        {
                            let _ = tx.send(count);
                        }
                        if let Some(id) = mine.borrow_mut().take() {
                            finder.disconnect(id);
                        }
                    });
                    // Cannot fire before this line: the signal is emitted from
                    // the same main loop this closure is running on.
                    *slot.borrow_mut() = Some(id);

                    finder.count_matches(&text, options.bits(), u32::MAX);
                    finder.search(&text, options.bits(), u32::MAX);
                }
                FindAction::Next => finder.search_next(),
                FindAction::Previous => finder.search_previous(),
                FindAction::Stop => finder.search_finish(),
            }
        })
        .context("could not reach the webview")?;

        if !counting {
            return Ok(None);
        }
        // A page that never answers is a page with a find controller that did
        // not emit -- reported as "no number", not as a failed search, because
        // the highlighting has happened either way.
        Ok(tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .ok()
            .and_then(Result::ok))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (view, action);
        Err(anyhow!("unsupported on this platform"))
    }
}

/// Show, hide or toggle WebKit's own inspector for the active tab.
///
/// The inspector is a real WebKit surface, not something this browser draws, so
/// there is nothing to theme and nothing to keep in step -- the price is that it
/// opens in its own window rather than in the tab, which on a tiling compositor
/// is arguably the better shape anyway.
///
/// Developer extras are switched on here rather than at webview creation: it is
/// a per-webview setting with a cost, and a browser that is not being debugged
/// should not be carrying an inspector's worth of instrumentation.
pub async fn devtools(state: &Arc<AppState>, action: Toggle) -> Result<bool> {
    let (view, _) = active(state).await?;

    #[cfg(target_os = "linux")]
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        view.with_webview(move |platform| {
            use webkit2gtk::{SettingsExt, WebInspectorExt, WebViewExt};
            let w = platform.inner();
            if let Some(settings) = w.settings() {
                settings.set_enable_developer_extras(true);
            }
            // Only available once developer extras are on, which is why the
            // settings line above is not an optimisation.
            let Some(inspector) = w.inspector() else {
                let _ = tx.send(false);
                return;
            };
            let open = inspector.is_attached() || inspector.attached_height() > 0;
            let want = action.resolve(open);
            if want {
                inspector.show();
                // Docked, the inspector takes its half out of the *window*,
                // which includes the strip: the title is cut in half and the
                // gear ends up underneath the Elements pane. Undocking it costs
                // nothing on a tiling compositor, which will place the second
                // window better than WebKit's splitter does anyway, and the
                // inspector's own dock button puts it back for anyone who
                // prefers it attached.
                if inspector.is_attached() {
                    inspector.detach();
                }
            } else {
                inspector.close();
            }
            let _ = tx.send(want);
        })
        .context("could not reach the webview")?;
        rx.await.context("the webview did not answer about its inspector")
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (view, action);
        Err(anyhow!("unsupported on this platform"))
    }
}

/// Silence a tab, or let it speak again.
///
/// Per tab and not per site, which is the same choice `zoom` makes: a tab is the
/// thing on screen, and "which of these is making noise" is a question about
/// tabs.
pub async fn mute(state: &Arc<AppState>, id: Option<u32>, action: Toggle) -> Result<bool> {
    let app = state.app_handle().context("the window is not up yet")?;
    let label = {
        let tabs = state.tabs.read().await;
        match id {
            Some(id) => tabs.label_of(id).ok_or_else(|| anyhow!("no tab with id {id}"))?,
            None => tabs.active_label().ok_or_else(|| anyhow!("there is no active tab"))?,
        }
    };
    let view = webview(&app, &label)?;

    #[cfg(target_os = "linux")]
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        view.with_webview(move |platform| {
            use webkit2gtk::WebViewExt;
            let w = platform.inner();
            let want = action.resolve(w.is_muted());
            w.set_is_muted(want);
            let _ = tx.send(want);
        })
        .context("could not reach the webview")?;
        rx.await.context("the webview did not report whether it is muted")
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (view, action);
        Err(anyhow!("unsupported on this platform"))
    }
}

/// Print the active tab, or write it to a PDF.
///
/// A path writes a PDF and never shows a dialog: "save this page as a PDF" is a
/// thing an agent asks for as often as a person does, and a modal dialog on the
/// CLI path would hang whoever called it.
///
/// `dialog` asks for GTK's print dialog instead, which is the only place the
/// user's real printers live. It is opt-in rather than the default because
/// under this runtime it does not work: the dialog renders, takes the keyboard
/// focus, and then accepts no input at all -- not Escape, not Alt-F4 -- while
/// GTK's grab keeps every key away from the browser too, so the window takes no
/// keys until the dialog goes away. `run_dialog` runs a nested GTK main loop,
/// and tao pumps GTK by hand from its own loop, so the nested one never gets
/// the events.
///
/// It is only GTK's input path that is dead, which is the way out: the
/// compositor can still act on the window even though GTK cannot, so SUPER+W --
/// close-window on Omarchy -- dismisses it. The control plane keeps answering
/// throughout, so `oma-browse --window <pid> tab list` works while it is up.
/// Closing the dialog that way leaves the job in limbo, though: neither
/// `connect_finished` nor `connect_failed` fires, and the caller waits out the
/// two-minute timeout below.
///
/// Fixing it properly means building the dialog ourselves from
/// `gtk_print_unix_dialog_*` and driving it with `connect_response` instead of
/// a nested loop -- there are no gtk-rs bindings for that, so it is raw FFI and
/// a job of its own. Note `unsafe_code` is denied workspace-wide; that site is
/// the case the lint's own comment says may lift it.
pub async fn print(
    state: &Arc<AppState>,
    to: Option<std::path::PathBuf>,
    dialog: bool,
) -> Result<Option<String>> {
    let (view, _) = active(state).await?;

    #[cfg(target_os = "linux")]
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        view.with_webview(move |platform| {
            use gtk::prelude::*;
            use std::cell::RefCell;
            use std::rc::Rc;
            use webkit2gtk::{PrintOperation, PrintOperationExt};

            let op = PrintOperation::new(&platform.inner());

            // A `WebKitPrintOperation` runs asynchronously and holds no
            // reference to itself. Dropping it at the end of this closure --
            // which is what the obvious code does -- cancels the job before a
            // byte is written, silently: `print()` returns, the command reports
            // success, and no file appears. Hold a reference until the operation
            // says it is done with itself.
            let keep: Rc<RefCell<Option<PrintOperation>>> = Rc::new(RefCell::new(None));
            let answer = Rc::new(RefCell::new(Some(tx)));

            let done = answer.clone();
            let alive = keep.clone();
            let wrote = to.as_ref().filter(|_| !dialog).map(|p| p.display().to_string());
            op.connect_finished(move |_| {
                alive.borrow_mut().take();
                if let Some(tx) = done.borrow_mut().take() {
                    let _ = tx.send(Ok(wrote.clone()));
                }
            });

            let bad = answer.clone();
            let alive = keep.clone();
            op.connect_failed(move |_, error| {
                alive.borrow_mut().take();
                if let Some(tx) = bad.borrow_mut().take() {
                    let _ = tx.send(Err(error.to_string()));
                }
            });

            *keep.borrow_mut() = Some(op.clone());

            match to.as_ref().filter(|_| !dialog) {
                Some(path) => {
                    let settings = gtk::PrintSettings::new();
                    // The destination is a *printer*, not a setting. Without
                    // this GTK sends the job to the default CUPS queue and, on
                    // a machine with no printer configured, that surfaces as
                    // "Broken pipe" -- which is what this looked like before.
                    // "Print to File" is GTK's own file backend, the same entry
                    // the print dialog lists first, and the name Epiphany uses.
                    settings.set_printer(FILE_PRINTER);
                    // Its output takes a URI, and only a URI: a bare path
                    // prints nowhere and says nothing.
                    settings.set("output-uri", Some(format!("file://{}", path.display()).as_str()));
                    settings.set("output-file-format", Some("pdf"));
                    op.set_print_settings(&settings);
                    op.print();
                }
                None => {
                    // Passed because a dialog should have a parent, not because
                    // it fixes anything: it was measured, and the dialog is just
                    // as unusable with it as without. See this function's doc
                    // comment for what actually happens and why. Leaving the
                    // parent out as well would only add a second defect on top
                    // of the one that matters.
                    let parent =
                        platform.inner().toplevel().and_then(|w| w.downcast::<gtk::Window>().ok());
                    op.run_dialog(parent.as_ref());
                }
            }
        })
        .context("could not reach the webview")?;

        // A print job that never answers must not hold the caller forever --
        // the dialog path in particular waits on a human.
        match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await {
            Ok(Ok(Ok(path))) => Ok(path),
            Ok(Ok(Err(e))) => Err(anyhow!("printing failed: {e}")),
            Ok(Err(_)) => Err(anyhow!("the webview dropped the print job")),
            Err(_) => Err(anyhow!("the print job did not finish within two minutes")),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (view, to);
        Err(anyhow!("unsupported on this platform"))
    }
}

/// GTK's built-in "save it to a file instead" printer.
///
/// Not a CUPS queue: GTK's file backend registers it itself, so it exists on a
/// machine with no printers at all -- which is most machines this will run on.
#[cfg(target_os = "linux")]
const FILE_PRINTER: &str = "Print to File";

/// The active tab's webview and label, which four commands here all want first.
async fn active(state: &Arc<AppState>) -> Result<(tauri::webview::Webview<tauri::Wry>, String)> {
    let app = state.app_handle().context("the window is not up yet")?;
    let label =
        state.tabs.read().await.active_label().ok_or_else(|| anyhow!("there is no active tab"))?;
    let view = webview(&app, &label)?;
    Ok((view, label))
}

/// `show`, `hide` or `toggle`, resolved against what is currently true.
///
/// Three commands take exactly this argument, and a shared type is what stops
/// one of them growing a fourth spelling nobody else accepts.
#[derive(Debug, Clone, Copy)]
pub enum Toggle {
    On,
    Off,
    Flip,
}

impl Toggle {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "show" | "on" | "open" | "true" => Some(Toggle::On),
            "hide" | "off" | "close" | "false" => Some(Toggle::Off),
            "toggle" | "flip" => Some(Toggle::Flip),
            _ => None,
        }
    }

    pub fn resolve(self, current: bool) -> bool {
        match self {
            Toggle::On => true,
            Toggle::Off => false,
            Toggle::Flip => !current,
        }
    }
}

pub async fn zoom(state: &Arc<AppState>, change: ZoomChange) -> Result<f64> {
    let app = state.app_handle().context("the window is not up yet")?;
    let label =
        state.tabs.read().await.active_label().ok_or_else(|| anyhow!("there is no active tab"))?;
    let view = webview(&app, &label)?;

    #[cfg(target_os = "linux")]
    {
        // Sorted once, here, rather than inside the GTK closure: the config is
        // not `Send` to borrow across it, and this runs on every step.
        let mut steps = state.config.tabs.zoom_steps.clone();
        steps.retain(|s| *s > 0.0);
        steps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let (tx, rx) = tokio::sync::oneshot::channel();
        view.with_webview(move |platform| {
            use webkit2gtk::WebViewExt;
            let w = platform.inner();
            let current = w.zoom_level();
            let next = match change {
                ZoomChange::In => stepped(&steps, current, true),
                ZoomChange::Out => stepped(&steps, current, false),
                ZoomChange::Reset => 1.0,
                ZoomChange::Set(v) => {
                    let low = steps.first().copied().unwrap_or(ZOOM_STEPS[0]);
                    let high = steps.last().copied().unwrap_or(ZOOM_STEPS[ZOOM_STEPS.len() - 1]);
                    v.clamp(low.min(high), high.max(low))
                }
            };
            w.set_zoom_level(next);
            let _ = tx.send(next);
        })
        .context("could not reach the webview")?;
        rx.await.context("the webview did not report a zoom level")
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (view, change);
        Err(anyhow!("unsupported on this platform"))
    }
}

pub async fn history(state: &Arc<AppState>, action: HistoryAction) -> Result<()> {
    let app = state.app_handle().context("the window is not up yet")?;
    let label =
        state.tabs.read().await.active_label().ok_or_else(|| anyhow!("there is no active tab"))?;
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
                    HistoryAction::HardReload => w.reload_bypass_cache(),
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
    let label =
        state.tabs.read().await.active_label().ok_or_else(|| anyhow!("there is no active tab"))?;

    // A webview whose web process has died still accepts an evaluation and
    // still calls back -- with an empty string, immediately, for any script at
    // all. That is indistinguishable from a page that genuinely answered with
    // nothing, so every command built on this one would report a confident
    // wrong answer. Refuse instead, and say what to do about it.
    if let Some(crash) = state.crash_of(&label) {
        return Err(anyhow!(
            "this tab's web process died -- {} -- so there is no page to ask. `nav reload` \
             brings {} back",
            crash.reason,
            if crash.uri.is_empty() { "it" } else { crash.uri.as_str() }
        ));
    }

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
    /// Reload, ignoring everything already cached.
    ///
    /// The one an engineer actually wants: a plain reload happily serves the
    /// bundle the dev server built four minutes ago, and no amount of saving the
    /// file changes that.
    HardReload,
    Stop,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stock template, so a test reads as what it is testing.
    const SEARCH: &str = "https://duckduckgo.com/?q={query}";

    /// Two tabs, the first of them active.
    fn pair() -> Tabs {
        let mut tabs = Tabs::default();
        let (id, _) = tabs.allocate("https://one.example".into());
        tabs.allocate("https://two.example".into());
        tabs.set_active(id);
        tabs
    }

    /// A tab with a load already under way, past the capped opening reading —
    /// which is the state every assertion about *movement* wants to start from.
    fn loading(tabs: &mut Tabs, label: &str, fraction: f64) {
        tabs.set_progress(label, Some(0.0));
        tabs.set_progress(label, Some(fraction));
    }

    #[test]
    fn only_the_active_tab_repaints_the_load_bar() {
        let mut tabs = pair();
        loading(&mut tabs, "tab-0", 0.4);
        assert!(tabs.set_progress("tab-0", Some(0.6)), "the active tab is the one drawn");
        loading(&mut tabs, "tab-1", 0.4);
        assert!(!tabs.set_progress("tab-1", Some(0.6)), "a background load has nowhere to go");
        // Recorded either way: it is only the *painting* that is skipped, so
        // switching to that tab still finds its bar where it should be.
        tabs.set_active(1);
        assert_eq!(tabs.active_progress(), Some(0.6));
    }

    #[test]
    fn movement_under_a_percent_is_not_worth_an_eval() {
        let mut tabs = pair();
        loading(&mut tabs, "tab-0", 0.40);
        assert!(!tabs.set_progress("tab-0", Some(0.402)), "invisible on a 2px bar");
        assert!(tabs.set_progress("tab-0", Some(0.41)), "a whole percent shows");
    }

    #[test]
    fn starting_and_finishing_always_repaint() {
        let mut tabs = pair();
        // Both edges matter however still the number is either side of them:
        // these are the frames the bar appears and disappears on.
        assert!(tabs.set_progress("tab-0", Some(0.0)), "a load beginning at nothing");
        assert!(tabs.set_progress("tab-0", None), "and the same load ending");
        assert_eq!(tabs.active_progress(), None, "a tab at rest has no bar");
        assert!(!tabs.set_progress("tab-0", None), "but not twice");
    }

    #[test]
    fn a_load_does_not_open_on_the_last_ones_number() {
        // WebKit hands over the *previous* load's 1.0 on the first
        // notification of the next one; see `OPENING`. Uncapped, that is a full
        // bar flashed across the window every time a page starts loading.
        let mut tabs = pair();
        tabs.set_progress("tab-0", Some(1.0));
        assert_eq!(tabs.active_progress(), Some(OPENING));
        // Only the opening reading. Once a load is under way its numbers are
        // its own, and 1.0 then means what it says.
        tabs.set_progress("tab-0", Some(0.5));
        tabs.set_progress("tab-0", Some(1.0));
        assert_eq!(tabs.active_progress(), Some(1.0));
    }

    #[test]
    fn a_tab_that_is_gone_reports_nothing() {
        // The notification is asynchronous, so a tab can be closed between
        // WebKit reading the value and the runtime getting to it.
        let mut tabs = pair();
        assert!(!tabs.set_progress("tab-9", Some(0.5)));
        assert_eq!(tabs.active_progress(), None);
    }

    #[test]
    fn a_toggle_reads_every_spelling_and_nothing_else() {
        for (raw, from_false, from_true) in [
            ("show", true, true),
            ("on", true, true),
            ("hide", false, false),
            ("off", false, false),
            ("toggle", true, false),
        ] {
            let t = Toggle::parse(raw).unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(t.resolve(false), from_false, "{raw} from off");
            assert_eq!(t.resolve(true), from_true, "{raw} from on");
        }
        assert!(Toggle::parse("").is_none());
        assert!(
            Toggle::parse("yes").is_none(),
            "an unknown spelling must be rejected, not guessed"
        );
    }

    #[test]
    fn a_bare_port_is_the_dev_server() {
        assert_eq!(resolve_input(":3000", SEARCH), "http://localhost:3000");
        assert_eq!(resolve_input(":8080/health", SEARCH), "http://localhost:8080/health");
        // Not a port, so not a shortcut: this is a search, and it must stay one.
        assert!(resolve_input(":not-a-port", SEARCH).starts_with("https://duckduckgo.com/"));
        assert!(resolve_input(":", SEARCH).starts_with("https://duckduckgo.com/"));
    }

    #[test]
    fn bare_hosts_become_https() {
        assert_eq!(resolve_input("github.com", SEARCH), "https://github.com");
        // Our own chrome is navigable, not searchable: a blank tab is sent to
        // `oma-chrome://localhost/start` by name.
        assert_eq!(
            resolve_input("oma-chrome://localhost/start", SEARCH),
            "oma-chrome://localhost/start"
        );
        assert_eq!(resolve_input("github.com/rust-lang", SEARCH), "https://github.com/rust-lang");
        assert_eq!(resolve_input("localhost:3000", SEARCH), "https://localhost:3000");
    }

    #[test]
    fn explicit_schemes_pass_through() {
        assert_eq!(resolve_input("http://example.com", SEARCH), "http://example.com");
        assert_eq!(resolve_input("https://example.com", SEARCH), "https://example.com");
    }

    #[test]
    fn prose_becomes_a_search() {
        assert_eq!(
            resolve_input("how do i tile windows", SEARCH),
            "https://duckduckgo.com/?q=how+do+i+tile+windows"
        );
        // A single word with no dot is a search, not a host.
        assert!(resolve_input("omarchy", SEARCH).starts_with("https://duckduckgo.com/?q="));
    }

    #[test]
    fn empty_input_is_blank_not_a_search() {
        assert_eq!(resolve_input("   ", SEARCH), "about:blank");
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

    #[test]
    fn positions_count_from_one_and_from_the_end() {
        let mut tabs = Tabs::default();
        assert_eq!(tabs.by_position(1), None, "no tabs, nowhere to go");

        let (a, _) = tabs.allocate("a".into());
        let (b, _) = tabs.allocate("b".into());
        let (c, _) = tabs.allocate("c".into());

        assert_eq!(tabs.by_position(1), Some(a), "Ctrl-1 is the first tab, not the second");
        assert_eq!(tabs.by_position(2), Some(b));
        assert_eq!(tabs.by_position(-1), Some(c), "Ctrl-9 means the last tab");
        assert_eq!(tabs.by_position(-3), Some(a));

        // Aiming past either end does nothing rather than clamping onto a tab
        // the chord did not name.
        assert_eq!(tabs.by_position(4), None);
        assert_eq!(tabs.by_position(-4), None);
        assert_eq!(tabs.by_position(0), None, "there is no zeroth tab");
    }
}
