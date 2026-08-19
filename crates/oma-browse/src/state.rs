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
    fn load() -> Self {
        let theme = Theme::load();
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
    pub theme: RwLock<ThemeState>,
    pub tabs: RwLock<Tabs>,
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
    /// Whether to repaint neutral page surfaces.
    ///
    /// On by default. A browser that themes its own chrome but leaves every page
    /// glaring white has not really themed anything — and the neutral test means
    /// only greyscale surfaces are touched, so brand colour survives. Turn it off
    /// with `oma-browse theme recolor off` when a site does not survive it.
    recolor: std::sync::atomic::AtomicBool,
    /// The local control plane's base URL, used by injected scripts.
    base: OnceLock<url::Url>,
    events: broadcast::Sender<UiEvent>,
}

impl AppState {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(32);
        Self {
            theme: RwLock::new(ThemeState::load()),
            tabs: RwLock::new(Tabs::default()),
            app: OnceLock::new(),
            rt: tokio::runtime::Handle::current(),
            palette: std::sync::atomic::AtomicBool::new(false),
            recolor: std::sync::atomic::AtomicBool::new(true),
            base: OnceLock::new(),
            events,
        }
    }

    /// Called once from Tauri's `setup`.
    pub fn set_app_handle(&self, handle: tauri::AppHandle<tauri::Wry>) {
        let _ = self.app.set(handle);
    }

    pub fn app_handle(&self) -> Option<tauri::AppHandle<tauri::Wry>> {
        self.app.get().cloned()
    }

    /// What a content webview needs injected: the theme's page styling.
    ///
    /// Shortcuts are deliberately *not* here. They started as an injected key
    /// handler, but a page-level handler only fires when that webview holds
    /// focus — and it would never work on a page that blocks scripts. They are
    /// bound on the GTK toplevel instead; see [`crate::layout::install_keys`].
    pub async fn page_script(&self) -> String {
        self.theme.read().await.css.page_script(self.recolor())
    }

    pub fn recolor(&self) -> bool {
        self.recolor.load(std::sync::atomic::Ordering::Relaxed)
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
        let next = ThemeState::load();
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
        Self::new()
    }
}
