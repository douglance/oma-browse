//! The native window: one page filling it, one palette floating above.
//!
//! There is no toolbar. The page owns the whole window, and everything else —
//! the URL bar, tab switching, settings — is summoned into a command palette and
//! dismissed again, the way a TUI does it. See [`crate::layout`] for the GTK
//! surgery that makes a floating overlay possible on a toolkit Tauri only ever
//! stacks vertically.

use std::sync::Arc;

use anyhow::{Context, Result};
use tauri::webview::{Webview, WebviewBuilder};
use tauri::window::WindowBuilder;
use tauri::{LogicalPosition, LogicalSize, Manager, WebviewUrl};

use crate::state::AppState;

/// The palette webview's label. Reserved: it is never a tab.
pub const PALETTE_LABEL: &str = "palette";

pub struct Launch {
    pub state: Arc<AppState>,
    pub base: url::Url,
    pub start_url: url::Url,
    pub incognito: bool,
    /// Theme background, so nothing flashes white before first paint (tauri#10011).
    pub background: oma_theme::Rgb,
    /// Ghostty's `background-opacity`, applied to the window and every webview.
    pub opacity: f64,
    /// The theme's page-facing injection, read on the async side: Tauri owns the
    /// main thread from here on, so nothing may block on the runtime.
    pub page_script: String,
    /// Label for the first tab, allocated before Tauri takes the thread — for
    /// the same reason.
    pub first_tab: String,
}

pub fn run(launch: Launch) -> Result<()> {
    let Launch {
        state,
        base,
        start_url,
        incognito,
        background,
        opacity,
        page_script,
        first_tab,
    } = launch;

    // The veil is painted once, in CSS. Every native surface underneath it is
    // fully transparent, so the two never compound -- see
    // `AppState::background_color`.
    let translucent = opacity.clamp(0.0, 1.0) < 1.0;
    let alpha = if translucent { 0 } else { 255 };
    let bg = tauri::window::Color(background.r, background.g, background.b, alpha);
    let palette_url = base.join("palette")?;

    tauri::Builder::default()
        .setup(move |app| {
            state.set_app_handle(app.handle().clone());

            let window = WindowBuilder::new(app, "main")
                .title("oma-browse")
                .inner_size(1400.0, 900.0)
                // Omarchy tiles everything and paints its own themed border, so a
                // GTK titlebar is wasted rows. `SUPER + W` closes the window, as
                // with every other Omarchy app.
                .decorations(false)
                // Ghostty-style translucency: the window carries the alpha and
                // the compositor shows the wallpaper through it. Only ask for a
                // transparent window when the theme actually wants one — an
                // unnecessarily transparent surface costs a compositor pass.
                .transparent(translucent)
                .background_color(bg)
                .build()
                .context("could not create the window")?;

            // Created first so it is the widget we can find the vbox through;
            // `layout::install` then lifts it out into the overlay.
            let palette = window
                .add_child(
                    WebviewBuilder::new(PALETTE_LABEL, WebviewUrl::External(palette_url.clone()))
                        .transparent(translucent)
                        .background_color(bg),
                    LogicalPosition::new(0.0, 0.0),
                    LogicalSize::new(720.0, 420.0),
                )
                .context("could not create the palette webview")?;

            let label = first_tab.clone();
            let content = window
                .add_child(
                    instrument(
                        WebviewBuilder::new(&label, WebviewUrl::External(start_url.clone()))
                            .auto_resize()
                            .transparent(translucent)
                            .background_color(bg)
                            .initialization_script(&page_script)
                            .incognito(incognito),
                        state.clone(),
                    ),
                    LogicalPosition::new(0.0, 0.0),
                    LogicalSize::new(1400.0, 900.0),
                )
                .context("could not create the first tab")?;

            crate::layout::install(&palette)?;
            crate::layout::install_keys(&palette, state.clone())?;

            // Nothing holds keyboard focus after the surgery, so page-level key
            // handlers — which is where every shortcut lives — would never fire.
            if let Err(e) = crate::layout::focus(&content) {
                tracing::warn!(error = %e, "could not focus the first tab");
            }

            tracing::info!(palette = %palette_url, content = %start_url, "window up");
            Ok(())
        })
        .run(tauri::generate_context!())
        .context("the Tauri event loop exited with an error")?;

    Ok(())
}

/// Attach the callbacks that keep the tab model in step with what a webview is
/// actually doing. Applied to every content webview, wherever it was created.
pub fn instrument(
    builder: WebviewBuilder<tauri::Wry>,
    state: Arc<AppState>,
) -> WebviewBuilder<tauri::Wry> {
    let title_state = state.clone();
    let load_state = state;

    builder
        .on_document_title_changed(move |webview, title| {
            let state = title_state.clone();
            let label = webview.label().to_string();
            state.runtime().spawn(async move {
                state.tabs.write().await.update_title(&label, title);
                state.notify_tabs();
            });
        })
        .on_page_load(move |webview, payload| {
            let state = load_state.clone();
            let label = webview.label().to_string();
            let url = payload.url().to_string();
            state.runtime().spawn(async move {
                state.tabs.write().await.update_url(&label, url);
                state.notify_tabs();
            });
        })
}

/// Re-dress every surface for the current theme, without a restart.
///
/// Three separate things have to change, because they are themed by three
/// different mechanisms:
///
/// * loaded pages carry an injected `<style>` element, which we re-run — the
///   script keys off a sentinel id, so it updates in place rather than stacking;
/// * the palette is our own page, so a reload is enough;
/// * `prefers-color-scheme` inside WebKit comes from GTK's
///   `gtk-application-prefer-dark-theme`, a *process-wide* setting that cannot
///   be varied per webview, so it is set once for the whole app.
pub async fn restyle(state: &Arc<AppState>) -> Result<()> {
    let app = state.app_handle().context("the window is not up yet")?;

    let (script, dark, color) = {
        let theme = state.theme.read().await;
        let bg = theme.css.tint;
        // Fully transparent while the theme is translucent -- see
        // `AppState::background_color`; the veil belongs to the CSS alone.
        let alpha = if theme.css.opacity < 1.0 { 0 } else { 255 };
        (
            theme.css.page_script(state.recolor()),
            theme.mode.is_dark(),
            tauri::window::Color(bg.r, bg.g, bg.b, alpha),
        )
    };

    for tab in state.tabs.read().await.list() {
        if let Some(view) = app.get_webview(&tab.label) {
            let _ = view.set_background_color(Some(color));
            if let Err(e) = view.eval(&script) {
                tracing::debug!(tab = %tab.label, error = %e, "could not restyle tab");
            }
        }
    }

    if let Some(palette) = app.get_webview(PALETTE_LABEL) {
        let _ = palette.set_background_color(Some(color));
        let _ = palette.reload();

        // GTK settings must be touched on the main thread; ride along with the
        // palette's webview to get there.
        let _ = palette.with_webview(move |_| {
            use gtk::prelude::*;
            if let Some(settings) = gtk::Settings::default() {
                settings.set_gtk_application_prefer_dark_theme(dark);
            }
        });
    }

    if let Some(window) = app.get_window("main") {
        let _ = window.set_background_color(Some(color));
    }

    Ok(())
}

/// Show or hide the palette, and hand it focus when it appears.
pub fn set_palette_visible(state: &Arc<AppState>, visible: bool) -> Result<()> {
    let app = state.app_handle().context("the window is not up yet")?;
    let palette: Webview<tauri::Wry> =
        app.get_webview(PALETTE_LABEL).context("the palette webview is missing")?;

    if visible {
        // Re-render on every summon. The palette is a long-lived webview, so
        // without this it shows whatever was true last time — a stale tab list
        // and whatever you typed before. Reloading is cheap: it is a local page,
        // and it means the palette always opens empty and current.
        let _ = palette.reload();
        palette.show().context("could not show the palette")?;
        // Without this the keystrokes keep going to the page behind it.
        palette.set_focus().context("could not focus the palette")?;
    } else {
        palette.hide().context("could not hide the palette")?;
        // Give focus back to the page, or typing goes nowhere.
        if let Some(label) = state.tabs.try_read().ok().and_then(|t| t.active_label())
            && let Some(tab) = app.get_webview(&label)
        {
            let _ = crate::layout::focus(&tab);
        }
    }
    Ok(())
}
