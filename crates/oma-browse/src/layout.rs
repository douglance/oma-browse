//! GTK surgery: turn Tauri's vertical stack into a page with a floating palette.
//!
//! Tauri packs every webview into the window's single vertical `gtk::Box`, and
//! its own positioning API is inert on Linux (tauri#10420). A stacked box can
//! never draw one webview *over* another, so we rebuild the widget tree once, at
//! startup, into:
//!
//! ```text
//! vbox
//! └── gtk::Overlay
//!     ├── content stack (gtk::Box)   ← every tab; all but one hidden
//!     └── palette (overlay child)    ← centred card, hidden until summoned
//! ```
//!
//! The palette is an *overlay child* with a fixed size and centre alignment, so
//! it floats above the page without covering it and without needing webview
//! transparency. Hidden, it takes no input at all, and the page behaves normally.
//!
//! This is safe specifically because the `unstable` feature makes every webview a
//! `WindowChild`: Tauri only attaches `undecorated_resizing::attach_resize_handler`
//! to `WindowContent` webviews, and that handler is the thing that panics when an
//! unexpected widget appears in the parent chain (wry#1808).

use anyhow::{Context, Result};
use tauri::webview::Webview;

/// Width of the palette card, in pixels.
const PALETTE_WIDTH: i32 = 720;
/// Height of the palette card.
const PALETTE_HEIGHT: i32 = 420;
/// Gap between the top of the window and the palette.
const PALETTE_TOP_MARGIN: i32 = 72;

/// Name given to the content stack, so later lookups can find it again without
/// holding a (non-`Send`) reference to the widget.
const CONTENT_STACK_NAME: &str = "oma-content-stack";

/// Rebuild the widget tree. Call once, after the palette and first tab exist.
#[cfg(target_os = "linux")]
pub fn install<R: tauri::Runtime>(palette: &Webview<R>) -> Result<()> {
    // Escape hatch for bisecting rendering problems: reparenting a
    // WebKitWebView is the most invasive thing we do to the widget tree, so
    // `OMA_LAYOUT=plain` leaves Tauri's own vertical box alone.
    if std::env::var("OMA_LAYOUT").as_deref() == Ok("plain") {
        tracing::warn!("OMA_LAYOUT=plain: skipping the overlay; tabs will tile, not stack");
        return Ok(());
    }
    palette
        .with_webview(move |platform| {
            use gtk::prelude::*;

            let palette_widget = platform.inner();

            let Some(vbox) = palette_widget.parent().and_then(|p| p.downcast::<gtk::Box>().ok())
            else {
                tracing::error!("palette webview is not in a GtkBox; leaving layout alone");
                return;
            };

            let overlay = gtk::Overlay::new();
            let stack = gtk::Box::new(gtk::Orientation::Vertical, 0);
            stack.set_widget_name(CONTENT_STACK_NAME);

            // Everything already packed except the palette is page content.
            for child in vbox.children() {
                if child == palette_widget.clone().upcast::<gtk::Widget>() {
                    continue;
                }
                vbox.remove(&child);
                stack.pack_start(&child, true, true, 0);
            }

            vbox.remove(&palette_widget);
            overlay.add(&stack);
            overlay.add_overlay(&palette_widget);

            // A fixed-size, centred overlay child: a floating card, not a bar.
            palette_widget.set_size_request(PALETTE_WIDTH, PALETTE_HEIGHT);
            palette_widget.set_halign(gtk::Align::Center);
            palette_widget.set_valign(gtk::Align::Start);
            palette_widget.set_margin_top(PALETTE_TOP_MARGIN);

            vbox.pack_start(&overlay, true, true, 0);
            overlay.show_all();

            // Summoned on demand; invisible and input-transparent until then.
            palette_widget.hide();

            // Reparenting leaves the GTK focus chain pointing at nothing, and a
            // webview that never receives focus never sees a keystroke — which
            // silently kills every shortcut, since they are all page-level
            // handlers. Tauri's own `set_focus` does not recover this; the
            // widget has to grab it.
            if let Some(first) = stack.children().first() {
                first.set_can_focus(true);
                first.grab_focus();
            }

            tracing::info!("overlay layout installed");
        })
        .context("could not reach the palette webview's GTK widget")
}

/// Bind the browser's shortcuts on the GTK toplevel window.
///
/// The obvious place for these is a key handler inside the page, and that is
/// where they started — but a page-level handler only fires when that webview
/// holds keyboard focus, and after the overlay surgery none of them reliably
/// does. Worse, it would never work on a page that blocks scripts.
///
/// GTK delivers key events to the toplevel before any child, so binding here
/// catches every chord no matter what has focus, and needs nothing injected into
/// the page at all.
#[cfg(target_os = "linux")]
pub fn install_keys<R: tauri::Runtime>(
    anchor: &Webview<R>,
    state: std::sync::Arc<crate::state::AppState>,
) -> Result<()> {
    anchor
        .with_webview(move |platform| {
            use gtk::prelude::*;

            let widget = platform.inner();
            let Some(top) = widget.toplevel().and_then(|t| t.downcast::<gtk::Window>().ok()) else {
                tracing::error!("no GTK toplevel; keyboard shortcuts are not bound");
                return;
            };

            top.connect_key_press_event(move |_win, event| {
                use gtk::gdk::ModifierType;

                let mods = event.state();
                let ctrl = mods.contains(ModifierType::CONTROL_MASK);
                let shift = mods.contains(ModifierType::SHIFT_MASK);
                let alt = mods.contains(ModifierType::MOD1_MASK);
                let name = event.keyval().name().map(|n| n.to_string()).unwrap_or_default();

                let action = match (ctrl, alt, name.as_str()) {
                    // Ctrl-K, Ctrl-L and Ctrl-P all summon the palette: one is
                    // "command", one is URL-bar muscle memory, and here they are
                    // the same thing.
                    (true, false, "k" | "K" | "l" | "L" | "p" | "P") => Some(Action::Palette),
                    (true, false, "t" | "T") => Some(Action::NewTab),
                    (true, false, "w" | "W") => Some(Action::CloseTab),
                    (true, false, "r" | "R") => Some(Action::Reload),
                    (true, false, "Tab" | "ISO_Left_Tab") => {
                        Some(if shift { Action::PrevTab } else { Action::NextTab })
                    }
                    (false, true, "Left") => Some(Action::Back),
                    (false, true, "Right") => Some(Action::Forward),
                    (false, false, "Escape") => Some(Action::Dismiss),
                    _ => None,
                };

                match action {
                    Some(action) => {
                        let state = state.clone();
                        state.runtime().spawn(async move { action.run(state).await });
                        gtk::glib::Propagation::Stop
                    }
                    None => gtk::glib::Propagation::Proceed,
                }
            });

            tracing::info!("keyboard shortcuts bound on the GTK window");
        })
        .context("could not reach the GTK toplevel")
}

#[cfg(not(target_os = "linux"))]
pub fn install_keys<R: tauri::Runtime>(
    _anchor: &Webview<R>,
    _state: std::sync::Arc<crate::state::AppState>,
) -> Result<()> {
    Ok(())
}

/// What a chord does. Kept separate so the GTK handler stays a lookup table.
#[derive(Debug, Clone, Copy)]
enum Action {
    Palette,
    Dismiss,
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    Reload,
    Back,
    Forward,
}

impl Action {
    async fn run(self, state: std::sync::Arc<crate::state::AppState>) {
        use crate::tabs::HistoryAction;

        match self {
            Action::Palette | Action::Dismiss => {
                let want = match self {
                    Action::Dismiss => false,
                    _ => !state.palette_visible(),
                };
                if crate::window::set_palette_visible(&state, want).is_ok() {
                    state.set_palette_visible(want);
                }
            }
            Action::NewTab => {
                let target = state
                    .base_url()
                    .and_then(|b| b.join("start").ok())
                    .map(|u| u.to_string())
                    .unwrap_or_else(|| "about:blank".to_string());
                if crate::tabs::open(&state, &target, false).await.is_ok() {
                    state.notify_tabs();
                }
            }
            Action::CloseTab => {
                let id = state.tabs.read().await.active_id();
                if id.is_some() && crate::tabs::close(&state, id).await.is_ok() {
                    state.notify_tabs();
                }
            }
            Action::NextTab | Action::PrevTab => {
                let delta = if matches!(self, Action::NextTab) { 1 } else { -1 };
                let next = state.tabs.read().await.neighbour(delta);
                if let Some(next) = next
                    && crate::tabs::select(&state, next).await.is_ok()
                {
                    state.notify_tabs();
                }
            }
            Action::Reload => drop(crate::tabs::history(&state, HistoryAction::Reload).await),
            Action::Back => drop(crate::tabs::history(&state, HistoryAction::Back).await),
            Action::Forward => drop(crate::tabs::history(&state, HistoryAction::Forward).await),
        }
    }
}

/// Hand keyboard focus to a webview at the GTK level.
///
/// Needed because our reparenting takes the widgets out of the tree Tauri set
/// up, so `Webview::set_focus` no longer reaches them.
#[cfg(target_os = "linux")]
pub fn focus<R: tauri::Runtime>(view: &Webview<R>) -> Result<()> {
    view.with_webview(move |platform| {
        use gtk::prelude::*;
        let widget = platform.inner();
        widget.set_can_focus(true);
        widget.grab_focus();
    })
    .context("could not focus the webview")
}

#[cfg(not(target_os = "linux"))]
pub fn focus<R: tauri::Runtime>(_view: &Webview<R>) -> Result<()> {
    Ok(())
}

/// Move a newly created tab into the content stack.
///
/// Tauri packs new webviews into the window's vbox, which by now holds only the
/// overlay — so without this a new tab would split the window vertically with
/// the page instead of joining the stack.
#[cfg(target_os = "linux")]
pub fn adopt_tab<R: tauri::Runtime>(tab: &Webview<R>) -> Result<()> {
    tab.with_webview(move |platform| {
        use gtk::prelude::*;

        let widget = platform.inner();
        let Some(vbox) = widget.parent().and_then(|p| p.downcast::<gtk::Box>().ok()) else {
            return;
        };

        // Find the content stack by walking the overlay we installed earlier.
        let stack = vbox
            .children()
            .into_iter()
            .filter_map(|c| c.downcast::<gtk::Overlay>().ok())
            .filter_map(|o| o.children().into_iter().find(|c| c.widget_name() == CONTENT_STACK_NAME))
            .find_map(|c| c.downcast::<gtk::Box>().ok());

        match stack {
            Some(stack) => {
                vbox.remove(&widget);
                stack.pack_start(&widget, true, true, 0);
                widget.show_all();
                widget.set_can_focus(true);
                widget.grab_focus();
            }
            None => tracing::error!("content stack missing; new tab left in the window box"),
        }
    })
    .context("could not reach the new tab's GTK widget")
}

#[cfg(not(target_os = "linux"))]
pub fn install<R: tauri::Runtime>(_palette: &Webview<R>) -> Result<()> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn adopt_tab<R: tauri::Runtime>(_tab: &Webview<R>) -> Result<()> {
    Ok(())
}
