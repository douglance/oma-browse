//! Shared application state.
//!
//! Held behind an `Arc` and captured by every command handler, so the CLI, HTTP
//! and MCP transports all mutate exactly the same browser.

use std::sync::OnceLock;

use tokio::sync::{RwLock, broadcast};

use oma_theme::{Mode, Theme, ThemeCss};

use crate::tabs::Tabs;

/// The theme, flattened into the shape the UI and the commands actually consume.
#[derive(Debug, Clone)]
pub struct ThemeState {
    pub name: String,
    pub mode: Mode,
    pub colors: usize,
    pub css: ThemeCss,
}

impl ThemeState {
    /// Read the live theme, with the config file's say over how see-through a
    /// page ends up.
    ///
    /// `theme.veil` is applied here rather than inside `oma-theme`, which has no
    /// business knowing this crate's config exists: `Theme::opacity` is a plain
    /// field, and everything downstream -- `--oma-veil`, the page's veil
    /// element, the window's alpha -- is derived from it by `css()`.
    ///
    /// `OMA_VEIL` still wins. It is the bisecting hatch, and a hatch that a
    /// config file could shut is not one.
    fn load(config: &crate::config::Config) -> Self {
        let mut theme = Theme::load();
        if oma_theme::veil_override().is_none()
            && let Some(pinned) = config.theme.veil.fixed()
        {
            theme.opacity = pinned;
        }
        Self {
            name: theme.name.clone(),
            mode: theme.mode(),
            colors: theme.palette.len(),
            css: theme.css(),
        }
    }
}

/// Something the UI needs to hear about without having asked.
///
/// Topcoat's reactivity runtime is pull-only — its browser bundle contains no
/// `EventSource` and no `WebSocket` — so anything the server originates has to be
/// pushed over SSE. This is the channel that feeds it.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// The Omarchy theme changed; the chrome should restyle.
    ThemeChanged { name: String, mode: Mode },
    /// A tab was opened, closed, selected, or changed its title or URL.
    TabsChanged,
}

pub struct AppState {
    /// The dotfile, read once at startup. Everything in it is a setting the
    /// browser reads rather than a value it changes, so it needs no lock.
    pub config: crate::config::Config,
    pub theme: RwLock<ThemeState>,
    pub tabs: RwLock<Tabs>,
    /// Where the browser has been. Empty and never written in an incognito
    /// window -- see [`AppState::set_incognito`].
    pub history: RwLock<crate::history::History>,
    pub bookmarks: RwLock<crate::bookmarks::Bookmarks>,
    /// What has been saved to disk.
    ///
    /// A `std::sync::Mutex`, not tokio's, and deliberately: WebKit's download
    /// callbacks arrive on the GTK main thread with no runtime under them, and
    /// an async lock cannot be taken there.
    pub downloads: std::sync::Mutex<crate::downloads::Downloads>,
    /// Which sites may use the camera, the microphone, your location.
    ///
    /// A `std::sync::Mutex` for the same reason as `downloads`: WebKit asks on
    /// the GTK main thread, with no runtime under it, and it wants the answer
    /// there and then.
    pub permissions: std::sync::Mutex<crate::permissions::Permissions>,
    /// What every tab has logged and fetched.
    ///
    /// A `std::sync::Mutex` for the same reason as `downloads` and
    /// `permissions`: both taps fire on the GTK main thread, one from a script
    /// message and one from a WebKit signal, and neither has a runtime under it.
    pub inspector: std::sync::Mutex<crate::inspect::Inspector>,
    /// Requests waiting on a person, oldest first.
    ///
    /// Only what a command needs to word the question and name the answer; the
    /// WebKit request object itself cannot leave the main thread and stays in
    /// [`crate::policy`].
    pub asked: std::sync::Mutex<std::collections::VecDeque<crate::permissions::Pending>>,
    /// The last page refused for its certificate, for the interstitial to
    /// explain and for `nav trust` to act on.
    pub tls: std::sync::Mutex<Option<crate::policy::Refused>>,
    /// The site currently asking for a username and a password.
    pub login: std::sync::Mutex<Option<crate::policy::Challenge>>,
    /// Logins given this session, by host and port. Memory only -- see
    /// [`crate::policy::remember_login`].
    pub logins: std::sync::Mutex<std::collections::HashMap<String, (String, String)>>,
    /// Set once, after Tauri's `setup` runs. Everything that touches a webview
    /// goes through here, so commands work identically from the GUI, the CLI
    /// and MCP.
    app: OnceLock<tauri::AppHandle<tauri::Wry>>,
    /// Webview callbacks fire on the GTK main thread, outside any tokio context,
    /// but the state they update lives behind async locks. Keep a runtime handle
    /// so those callbacks can hand work back to the runtime.
    rt: tokio::runtime::Handle,
    /// Whether the palette is currently up. Plain atomic rather than an async
    /// lock: it is read from key handlers on the GTK thread.
    palette: std::sync::atomic::AtomicBool,
    /// A command the palette should open already asking for its argument.
    ///
    /// Ctrl-F is "find", which is a command that needs text, so the chord opens
    /// the palette *staged* into that command rather than dropping the user in
    /// a list to search for it. Taken and cleared when the palette renders.
    stage: std::sync::Mutex<Option<String>>,
    /// Whether this window forgets where it has been.
    incognito: std::sync::atomic::AtomicBool,
    /// Whether this window is one site's window rather than a browser.
    app_mode: std::sync::atomic::AtomicBool,
    /// Whether WebKit's shared download signal has been hooked yet.
    download_hook: std::sync::atomic::AtomicBool,
    /// Whether to repaint neutral page surfaces.
    ///
    /// On by default. A browser that themes its own chrome but leaves every page
    /// glaring white has not really themed anything — and the neutral test means
    /// only greyscale surfaces are touched, so brand colour survives. Turn it off
    /// with `oma-browse theme recolor off` when a site does not survive it.
    recolor: std::sync::atomic::AtomicBool,
    /// Set once the cookie policy has been applied to the shared web context.
    cookie_policy: std::sync::atomic::AtomicBool,
    /// A complaint about the configuration raised after startup.
    late_problem: std::sync::Mutex<Option<String>>,
    /// The local control plane's base URL, used by injected scripts.
    base: OnceLock<url::Url>,
    events: broadcast::Sender<UiEvent>,
}

impl AppState {
    pub fn new(config: crate::config::Config) -> Self {
        let (events, _) = broadcast::channel(32);
        Self {
            theme: RwLock::new(ThemeState::load(&config)),
            tabs: RwLock::new(Tabs::with_reopen_depth(config.tabs.reopen_depth)),
            history: RwLock::new(crate::history::History::load_with(config.history.limit)),
            bookmarks: RwLock::new(crate::bookmarks::Bookmarks::load()),
            downloads: std::sync::Mutex::new(crate::downloads::Downloads::load()),
            permissions: std::sync::Mutex::new(crate::permissions::Permissions::load()),
            inspector: std::sync::Mutex::new(crate::inspect::Inspector::default()),
            asked: std::sync::Mutex::new(std::collections::VecDeque::new()),
            tls: std::sync::Mutex::new(None),
            login: std::sync::Mutex::new(None),
            logins: std::sync::Mutex::new(std::collections::HashMap::new()),
            incognito: std::sync::atomic::AtomicBool::new(false),
            app_mode: std::sync::atomic::AtomicBool::new(false),
            download_hook: std::sync::atomic::AtomicBool::new(false),
            stage: std::sync::Mutex::new(None),
            app: OnceLock::new(),
            rt: tokio::runtime::Handle::current(),
            palette: std::sync::atomic::AtomicBool::new(false),
            recolor: std::sync::atomic::AtomicBool::new(config.theme.recolor),
            base: OnceLock::new(),
            cookie_policy: std::sync::atomic::AtomicBool::new(false),
            late_problem: std::sync::Mutex::new(None),
            events,
            config,
        }
    }

    /// Whether a visit should be written down.
    ///
    /// Two ways to answer no, and they are different questions: this window is
    /// incognito, or this browser never keeps history at all.
    pub fn keeps_history(&self) -> bool {
        !self.incognito() && self.config.history.enabled
    }

    /// An app state whose history and bookmarks are detached from disk.
    ///
    /// `new` loads both stores from the user's real state directory, which makes
    /// any test that asserts on their contents depend on the developer's own
    /// browsing. The palette tests count rows; one stray bookmark was enough to
    /// fail them.
    #[cfg(test)]
    pub fn detached() -> Self {
        let mut state = Self::new(crate::config::Config::default());
        *state.history.get_mut() = crate::history::History::default();
        *state.bookmarks.get_mut() = crate::bookmarks::Bookmarks::default();
        *state.downloads.get_mut().unwrap() = crate::downloads::Downloads::default();
        *state.permissions.get_mut().unwrap() = crate::permissions::Permissions::default();
        state
    }

    /// Called once from Tauri's `setup`.
    pub fn set_app_handle(&self, handle: tauri::AppHandle<tauri::Wry>) {
        let _ = self.app.set(handle);
    }

    pub fn app_handle(&self) -> Option<tauri::AppHandle<tauri::Wry>> {
        self.app.get().cloned()
    }

    /// What a content webview needs injected: the theme's page styling, the tab
    /// strip's inset, and link hints.
    ///
    /// Shortcuts are otherwise deliberately *not* here. They started as an
    /// injected key handler, but a page-level handler only fires when that
    /// webview holds focus — and it would never work on a page that blocks
    /// scripts. They are bound on the GTK toplevel instead; see
    /// [`crate::layout::install_keys`].
    ///
    /// Link hints are the one exception, and for a reason that only applies to
    /// them: their key is a bare `f`, which is a letter, and a toplevel
    /// accelerator would eat it in every search box on the web. Only the page
    /// knows where the caret is. See [`crate::hints`].
    pub async fn page_script(&self) -> String {
        let theme =
            self.theme.read().await.css.page_script(self.recolor(), self.recolor_max_rules());
        // These ride along with the theme's injection rather than being further
        // `initialization_script`s: all of them have to be re-applied on every
        // navigation, and there is only one hook for that.
        format!(
            "{theme}\n{}\n{}\n{}",
            self.inset_script(),
            crate::hints::script(),
            crate::inspect::script()
        )
    }

    /// Take the right to configure the shared web context, once.
    ///
    /// Same reason as the download hook below: the cookie policy, the proxy and
    /// the spell checker all belong to the `WebContext` every tab shares, not to
    /// a tab. Doing them per tab would be repeated work at best and, for the
    /// proxy, a settings object rebuilt under a live connection at worst.
    pub fn claim_shared_context(&self) -> bool {
        !self.cookie_policy.swap(true, std::sync::atomic::Ordering::Relaxed)
    }

    /// Something the browser noticed about its own configuration after the file
    /// was read -- a value that parsed but does not mean anything.
    ///
    /// Separate from `Config::problem` because the config itself is immutable
    /// once loaded; `config show` reports whichever of the two happened.
    pub fn note_config_problem(&self, problem: String) {
        tracing::warn!(%problem, "config");
        if let Ok(mut slot) = self.late_problem.lock()
            && slot.is_none()
        {
            *slot = Some(problem);
        }
    }

    /// What is wrong with the configuration, if anything: the parse failure
    /// first, since a file that did not load explains everything after it.
    pub fn config_problem(&self) -> Option<String> {
        self.config
            .problem
            .clone()
            .or_else(|| self.late_problem.lock().ok().and_then(|slot| slot.clone()))
    }

    /// Take the right to hook WebKit's downloads, once.
    ///
    /// The signal lives on the `WebContext`, which every tab shares, so hooking
    /// it per tab would report each download once per open tab.
    pub fn claim_download_hook(&self) -> bool {
        !self.download_hook.swap(true, std::sync::atomic::Ordering::Relaxed)
    }

    /// Where a download should land, and under what name.
    ///
    /// Resolved per download rather than once at startup: the directory can be
    /// created, removed or filled between two saves, and `unique` has to see the
    /// disk as it is at the moment the file is written.
    pub fn download_path(&self, suggested: &str) -> std::path::PathBuf {
        let configured = self.config.downloads.dir.trim();
        let dir = if configured.is_empty() {
            crate::downloads::download_dir()
        } else {
            std::path::PathBuf::from(crate::paths::shellexpand(configured))
        };
        // A download that cannot be placed is a download that is lost, so make
        // the directory rather than letting WebKit fail on it.
        let _ = std::fs::create_dir_all(&dir);
        crate::downloads::unique(&dir, suggested)
    }

    /// Ask the palette to open staged into a command.
    pub fn set_stage(&self, tool: Option<String>) {
        if let Ok(mut slot) = self.stage.lock() {
            *slot = tool;
        }
    }

    /// Read and clear the staged command; the next summon starts clean.
    pub fn take_stage(&self) -> Option<String> {
        self.stage.lock().ok().and_then(|mut slot| slot.take())
    }

    pub fn incognito(&self) -> bool {
        self.incognito.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether this window is a single site's window rather than a browser.
    ///
    /// `--app <url>` is the Omarchy web-app case: one page, no strip, no
    /// palette in your face when it opens, and a WM class of its own so a
    /// Hyprland rule can target it. The keys still work -- a window you cannot
    /// reload or close from the keyboard is not chromeless, it is crippled.
    pub fn app_mode(&self) -> bool {
        self.app_mode.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_app_mode(&self, on: bool) {
        self.app_mode.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether the tab strip should exist at all.
    ///
    /// Two ways to answer no, and they are different questions: the config file
    /// turned it off, or this window is one site's window and a tab strip would
    /// be a browser sitting on top of an app.
    pub fn strip_enabled(&self) -> bool {
        self.config.chrome.strip.enabled && !self.app_mode()
    }

    /// Set once, from the launch flags. Checked before anything is written to
    /// history, so an incognito window leaves no trace on disk.
    pub fn set_incognito(&self, on: bool) {
        self.incognito.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// The strip's top inset, or nothing at all when the strip is off -- the
    /// room is only worth taking for something that is going to be there.
    pub fn inset_script(&self) -> String {
        if self.strip_enabled() {
            crate::strip::inset_script(self.config.chrome.strip.height)
        } else {
            String::new()
        }
    }

    pub fn recolor(&self) -> bool {
        self.recolor.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The rule count above which a page is left in its own colours; see
    /// [`crate::config::Theme::recolor_max_rules`].
    pub fn recolor_max_rules(&self) -> usize {
        self.config.theme.recolor_max_rules
    }

    pub fn set_recolor(&self, on: bool) {
        self.recolor.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn base_url(&self) -> Option<url::Url> {
        self.base.get().cloned()
    }

    /// Record the control-plane URL, once it is known.
    pub fn set_base_url(&self, base: url::Url) {
        let _ = self.base.set(base);
    }

    /// The window/webview background.
    ///
    /// When the theme is translucent this is *fully* transparent, not the
    /// Ghostty alpha. The veil is painted exactly once, in CSS on `html`, and
    /// the webview underneath must contribute nothing: WebKit composites the
    /// document background over the webview background, so carrying the alpha
    /// in both places squares it -- 0.5 twice reads as 0.75, and the window
    /// visibly fails to match the terminal beside it.
    ///
    /// An opaque theme still gets its colour here, to kill the white flash a
    /// fresh webview shows before first paint (tauri#10011).
    pub async fn background_color(&self) -> tauri::window::Color {
        let theme = self.theme.read().await;
        let bg = theme.css.tint;
        if theme.css.opacity < 1.0 {
            tauri::window::Color(bg.r, bg.g, bg.b, 0)
        } else {
            tauri::window::Color(bg.r, bg.g, bg.b, 255)
        }
    }

    /// Tell the UI that the tab strip is stale.
    pub fn palette_visible(&self) -> bool {
        self.palette.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_palette_visible(&self, visible: bool) {
        self.palette.store(visible, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn runtime(&self) -> tokio::runtime::Handle {
        self.rt.clone()
    }

    pub fn notify_tabs(&self) {
        let _ = self.events.send(UiEvent::TabsChanged);
    }

    /// Subscribe to UI events. Late subscribers may miss events that fired before
    /// they joined; that is fine, since every event is a "re-read the state" hint
    /// rather than a delta.
    pub fn subscribe(&self) -> broadcast::Receiver<UiEvent> {
        self.events.subscribe()
    }

    /// Re-read the live Omarchy theme. Returns whether anything actually changed.
    ///
    /// The fingerprint check matters: the hook and the inotify fallback both fire
    /// for a single `omarchy theme set`, and repainting twice is visible.
    pub async fn reload_theme(&self) -> bool {
        // The config's veil survives a theme change: it is a statement about
        // this browser, not about the theme that happens to be on.
        let next = ThemeState::load(&self.config);
        let mut current = self.theme.write().await;
        if current.css.fingerprint == next.css.fingerprint {
            tracing::debug!(theme = %next.name, "theme reload was a no-op");
            return false;
        }

        tracing::info!(theme = %next.name, mode = next.mode.as_str(), "theme changed");
        let event = UiEvent::ThemeChanged { name: next.name.clone(), mode: next.mode };
        *current = next;
        drop(current);

        // No subscribers simply means no UI is up yet.
        let _ = self.events.send(event);
        true
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(crate::config::Config::default())
    }
}
