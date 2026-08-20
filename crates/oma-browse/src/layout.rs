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

/// Share of the window's width the palette card takes, before the bounds below.
const PALETTE_WIDTH_RATIO: f64 = 0.5;
/// Narrowest the card gets. Below this the two-line rows start wrapping.
const PALETTE_MIN_WIDTH: i32 = 420;
/// Widest it gets. A launcher stretched across an ultrawide is a bar, not a card.
const PALETTE_MAX_WIDTH: i32 = 900;
/// Tallest the card gets, and its height on any window with room for it.
const PALETTE_HEIGHT: i32 = 420;
/// Shortest it gets before it would rather be clipped than useless.
const PALETTE_MIN_HEIGHT: i32 = 200;
/// Gap between the top of the window and the palette.
const PALETTE_TOP_MARGIN: i32 = 72;
/// Page left visible either side of the card, and below it.
const PALETTE_SIDE_MARGIN: i32 = 24;
const PALETTE_BOTTOM_MARGIN: i32 = 72;

/// The card's size for a window of the given size.
///
/// The ratio is what shapes it; the bounds keep it a card at both extremes, and
/// the margins win over the minimum, because a card wider than the window is
/// clipped by the overlay rather than centred in it.
fn palette_size(window_width: i32, window_height: i32) -> (i32, i32) {
    let ideal = (f64::from(window_width) * PALETTE_WIDTH_RATIO).round() as i32;
    let room = (window_width - 2 * PALETTE_SIDE_MARGIN).max(1);
    let width = ideal.clamp(PALETTE_MIN_WIDTH, PALETTE_MAX_WIDTH).min(room);

    let available = window_height - PALETTE_TOP_MARGIN - PALETTE_BOTTOM_MARGIN;
    let height = available.clamp(PALETTE_MIN_HEIGHT, PALETTE_HEIGHT);

    (width, height)
}

/// Height of the tab strip, in pixels.
///
/// Two rows of nothing is what a browser normally spends here; this is one row
/// of 16px favicons and the padding that keeps them off the page. Anything
/// taller stops being a strip.
///
/// The page is told the same number -- see [`crate::strip::inset_script`] --
/// because the strip floats and the room for it is made inside the document
/// rather than taken out of the window.
pub const STRIP_HEIGHT: i32 = 26;

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

            // A centred overlay child sized as a share of the window: a floating
            // card, not a bar. The request here is only what the widget asks for
            // before the first layout pass; the handler below is what actually
            // sizes it, on that pass and on every resize after it.
            palette_widget.set_size_request(PALETTE_MIN_WIDTH, PALETTE_HEIGHT);
            palette_widget.set_halign(gtk::Align::Center);
            palette_widget.set_valign(gtk::Align::Start);
            palette_widget.set_margin_top(PALETTE_TOP_MARGIN);

            // GTK hands the overlay its allocation on every layout pass, which
            // makes it the one place that knows how wide the window currently
            // is — Tauri's own resize events do not reach this far down.
            let card = palette_widget.clone();
            let last = std::cell::Cell::new((0, 0));
            overlay.connect_size_allocate(move |_, alloc| {
                let size = palette_size(alloc.width(), alloc.height());
                // `set_size_request` queues another allocation, so setting it
                // unconditionally here would spin the layout loop.
                if last.replace(size) != size {
                    card.set_size_request(size.0, size.1);
                    tracing::debug!(
                        window = alloc.width(),
                        width = size.0,
                        height = size.1,
                        "palette resized"
                    );
                }
            });

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
    catalog: incurs::tool::ToolCatalog,
    runtime: tokio::runtime::Handle,
) -> Result<()> {
    anchor
        .with_webview(move |platform| {
            use gtk::prelude::*;

            let widget = platform.inner();
            let Some(top) = widget.toplevel().and_then(|t| t.downcast::<gtk::Window>().ok()) else {
                tracing::error!("no GTK toplevel; keyboard shortcuts are not bound");
                return;
            };

            // A binding that names a command the graph does not have is a
            // typo, and a silently dead key is a miserable way to find out.
            for b in BINDINGS {
                if catalog.get(b.tool).is_none() {
                    tracing::error!(tool = b.tool, "key binding names a command that does not exist");
                }
            }

            top.connect_key_press_event(move |_win, event| {
                use gtk::gdk::ModifierType;

                let mods = event.state();
                let ctrl = mods.contains(ModifierType::CONTROL_MASK);
                let shift = mods.contains(ModifierType::SHIFT_MASK);
                let alt = mods.contains(ModifierType::MOD1_MASK);
                let name = event.keyval().name().map(|n| n.to_string()).unwrap_or_default();

                // An atomic load, not a lock: this runs on the GTK main thread
                // for every keystroke in the window, and blocking here would
                // freeze the whole UI.
                let palette_open = state.palette_visible();

                let hit = BINDINGS.iter().find(|b| {
                    b.ctrl == ctrl
                        && b.alt == alt
                        && b.shift.is_none_or(|want| want == shift)
                        && b.keys.contains(&name.as_str())
                        && b.when.applies(palette_open)
                });

                match hit {
                    Some(b) => {
                        let catalog = catalog.clone();
                        let args = crate::dispatch::args_from_json(b.args);
                        let (tool, quiet) = (b.tool, b.propagate);
                        runtime.spawn(async move {
                            if let Some(message) = crate::dispatch::run(&catalog, tool, args).await {
                                // A propagating chord fires on presses that were
                                // meant for the page, so its failures are not
                                // news: Escape on a window with no tab would
                                // otherwise raise a notification every time.
                                if quiet {
                                    tracing::debug!(tool, %message, "command declined");
                                } else {
                                    crate::dispatch::toast(&message);
                                }
                            }
                        });
                        if b.propagate {
                            gtk::glib::Propagation::Proceed
                        } else {
                            gtk::glib::Propagation::Stop
                        }
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
    _catalog: incurs::tool::ToolCatalog,
    _runtime: tokio::runtime::Handle,
) -> Result<()> {
    Ok(())
}

/// One chord, and the command it runs.
///
/// The whole point of the table is that the *only* thing a key knows is a tool
/// name and its arguments. Before this, `layout.rs` held an `Action` enum whose
/// twelve arms re-implemented what the command graph already did, so adding
/// `page zoom` meant writing it twice and the palette still could not reach it.
struct Binding {
    ctrl: bool,
    alt: bool,
    /// `None` when Shift is not part of the chord. Only Ctrl-Tab cares.
    shift: Option<bool>,
    /// GDK keyval names. Both letter cases are listed because GTK reports the
    /// shifted name (`"T"`) when Shift is down, and some of these chords are
    /// reachable with it held.
    keys: &'static [&'static str],
    /// A tool name from the catalog: the command path joined with `_`.
    tool: &'static str,
    /// Arguments as JSON, parsed by the same function the palette uses, so
    /// there is one encoding of "call this command with these arguments".
    args: &'static str,
    /// What has to be on screen for this chord to mean anything.
    when: When,
    /// Whether the page sees the key as well.
    ///
    /// Off for everything the browser owns outright: a page must not also act
    /// on Ctrl-W. On only for Escape with the palette down — Escape means
    /// "close your modal" or "leave fullscreen" to a page at least as often as
    /// it means "stop loading" to us, and swallowing it broke both.
    propagate: bool,
}

/// What has to be true for a chord to fire.
///
/// Escape is the only key whose meaning depends on what is on screen, but the
/// condition belongs in the table rather than in the handler: the point of the
/// table is that reading it tells you the whole keyboard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum When {
    Always,
    PaletteOpen,
    PaletteClosed,
}

impl When {
    const fn applies(self, palette_open: bool) -> bool {
        match self {
            When::Always => true,
            When::PaletteOpen => palette_open,
            When::PaletteClosed => !palette_open,
        }
    }
}

/// One of the nine "jump to that tab" chords.
///
/// The same row nine times otherwise: only the digit and the position differ,
/// and `tab select` does the rest.
const fn tab_at(keys: &'static [&'static str], args: &'static str) -> Binding {
    Binding {
        ctrl: true, alt: false, shift: Some(false),
        keys, tool: "tab_select", args,
        when: When::Always, propagate: false,
    }
}

const BINDINGS: &[Binding] = &[
    // Ctrl-K, Ctrl-L and Ctrl-P all summon the palette: one is "command", one
    // is URL-bar muscle memory, and here they are the same thing.
    Binding {
        ctrl: true, alt: false, shift: None,
        keys: &["k", "K", "l", "L", "p", "P"],
        tool: "ui_palette", args: r#"{"action":"toggle"}"#,
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: true, alt: false, shift: Some(false),
        keys: &["t", "T"], tool: "tab_open", args: "{}",
        when: When::Always, propagate: false,
    },
    // Ctrl-Shift-W closes the window; it must come *before* Ctrl-W, which would
    // otherwise close a single tab and swallow the chord.
    Binding {
        ctrl: true, alt: false, shift: Some(true),
        keys: &["w", "W"], tool: "window_close", args: "{}",
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: true, alt: false, shift: Some(false),
        keys: &["w", "W"], tool: "tab_close", args: "{}",
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: true, alt: false, shift: None,
        keys: &["r", "R"], tool: "nav_reload", args: "{}",
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: false, alt: false, shift: None,
        keys: &["F5"], tool: "nav_reload", args: "{}",
        when: When::Always, propagate: false,
    },
    // Ctrl-Shift-T reopens; it must come *before* the plain Ctrl-T entry, which
    // does not care about Shift and would otherwise swallow it.
    Binding {
        ctrl: true, alt: false, shift: Some(true),
        keys: &["t", "T"], tool: "tab_reopen", args: "{}",
        when: When::Always, propagate: false,
    },
    // Find. Ctrl-F stages the palette rather than opening a find bar: the
    // palette already knows how to prompt for an argument from the schema, and
    // a second text box on screen would be a second thing to learn.
    Binding {
        ctrl: true, alt: false, shift: None,
        keys: &["f", "F"],
        tool: "ui_palette", args: r#"{"action":"show","stage":"find_text"}"#,
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: true, alt: false, shift: Some(false),
        keys: &["g", "G"], tool: "find_next", args: "{}",
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: true, alt: false, shift: Some(true),
        keys: &["g", "G"], tool: "find_previous", args: "{}",
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: false, alt: false, shift: None,
        keys: &["F3"], tool: "find_next", args: "{}",
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: true, alt: false, shift: None,
        keys: &["d", "D"], tool: "bookmark_add", args: "{}",
        when: When::Always, propagate: false,
    },
    // `equal` is the unshifted key people actually press for "zoom in"; `plus`
    // is what arrives once Shift is held, and the KP_ names are the numeric
    // keypad. Bind all of them, the way every other browser does.
    Binding {
        ctrl: true, alt: false, shift: None,
        keys: &["plus", "equal", "KP_Add"],
        tool: "page_zoom", args: r#"{"direction":"in"}"#,
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: true, alt: false, shift: None,
        keys: &["minus", "underscore", "KP_Subtract"],
        tool: "page_zoom", args: r#"{"direction":"out"}"#,
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: true, alt: false, shift: None,
        keys: &["0", "KP_0"],
        tool: "page_zoom", args: r#"{"direction":"reset"}"#,
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: true, alt: false, shift: Some(false),
        keys: &["Tab", "ISO_Left_Tab"],
        tool: "tab_cycle", args: r#"{"delta":1}"#,
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: true, alt: false, shift: Some(true),
        keys: &["Tab", "ISO_Left_Tab"],
        tool: "tab_cycle", args: r#"{"delta":-1}"#,
        when: When::Always, propagate: false,
    },
    // The other half of Ctrl-Tab's muscle memory. `Prior`/`Next` are the names
    // GDK reports on some layouts, and the KP_ pair is the keypad with NumLock
    // off, where these keys live for anyone using a compact keyboard.
    Binding {
        ctrl: true, alt: false, shift: Some(false),
        keys: &["Page_Down", "Next", "KP_Page_Down", "KP_Next"],
        tool: "tab_cycle", args: r#"{"delta":1}"#,
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: true, alt: false, shift: Some(false),
        keys: &["Page_Up", "Prior", "KP_Page_Up", "KP_Prior"],
        tool: "tab_cycle", args: r#"{"delta":-1}"#,
        when: When::Always, propagate: false,
    },
    // Ctrl-1..Ctrl-8 count from the left; Ctrl-9 is the last tab however many
    // there are, which is what every other browser does with it. Shift is
    // excluded for form's sake only: GDK reports `exclam`, not `1`, when it is
    // held, so a shifted digit cannot reach these rows anyway.
    tab_at(&["1", "KP_1"], r#"{"index":1}"#),
    tab_at(&["2", "KP_2"], r#"{"index":2}"#),
    tab_at(&["3", "KP_3"], r#"{"index":3}"#),
    tab_at(&["4", "KP_4"], r#"{"index":4}"#),
    tab_at(&["5", "KP_5"], r#"{"index":5}"#),
    tab_at(&["6", "KP_6"], r#"{"index":6}"#),
    tab_at(&["7", "KP_7"], r#"{"index":7}"#),
    tab_at(&["8", "KP_8"], r#"{"index":8}"#),
    tab_at(&["9", "KP_9"], r#"{"index":-1}"#),
    Binding {
        ctrl: false, alt: true, shift: None,
        keys: &["Left"], tool: "nav_back", args: "{}",
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: false, alt: true, shift: None,
        keys: &["Right"], tool: "nav_forward", args: "{}",
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: false, alt: true, shift: None,
        keys: &["Home", "KP_Home"], tool: "nav_home", args: "{}",
        when: When::Always, propagate: false,
    },
    Binding {
        ctrl: false, alt: false, shift: None,
        keys: &["F11"], tool: "window_fullscreen", args: r#"{"action":"toggle"}"#,
        when: When::Always, propagate: false,
    },
    // Escape, twice. With the palette up it dismisses it and goes no further --
    // that is what the key was pressed for. With the palette down it stops a
    // load *and* reaches the page, because a page's own Escape closes its
    // modals, its autocomplete and its fullscreen video, and this handler runs
    // before any of them.
    Binding {
        ctrl: false, alt: false, shift: None,
        keys: &["Escape"],
        tool: "ui_palette", args: r#"{"action":"hide"}"#,
        when: When::PaletteOpen, propagate: false,
    },
    Binding {
        ctrl: false, alt: false, shift: None,
        keys: &["Escape"], tool: "nav_stop", args: "{}",
        when: When::PaletteClosed, propagate: true,
    },
];

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

/// Float the tab strip over the top of the page.
///
/// An overlay child rather than another row in the window's box, which is what
/// makes the page scroll *under* it: a packed strip would take its height out of
/// the window permanently, so the page could never reach the top of the screen.
/// Floating it and giving the document a matching top inset instead means the
/// strip obscures nothing at rest -- the inset is the first thing on the page --
/// and the content passes beneath it as soon as that inset scrolls away.
#[cfg(target_os = "linux")]
pub fn adopt_strip<R: tauri::Runtime>(strip: &Webview<R>) -> Result<()> {
    strip
        .with_webview(move |platform| {
            use gtk::prelude::*;

            let widget = platform.inner();
            let Some(vbox) = widget.parent().and_then(|p| p.downcast::<gtk::Box>().ok()) else {
                tracing::error!("the strip is not in a GtkBox; leaving it where Tauri put it");
                return;
            };
            let Some(overlay) =
                vbox.children().into_iter().find_map(|c| c.downcast::<gtk::Overlay>().ok())
            else {
                tracing::error!("no overlay; the strip would tile with the page, so leaving it out");
                return;
            };

            vbox.remove(&widget);
            overlay.add_overlay(&widget);

            // Full width, pinned to the top, exactly as tall as it needs to be.
            // Added after the palette, so it sits above it in the stacking
            // order -- which costs nothing, since the card starts well below.
            widget.set_halign(gtk::Align::Fill);
            widget.set_valign(gtk::Align::Start);
            widget.set_size_request(-1, STRIP_HEIGHT);

            // A strip that can take focus is a strip that swallows the next
            // keystroke after a click on it -- and every shortcut is bound on
            // the toplevel, so it has no reason to want focus at all.
            widget.set_can_focus(false);
            widget.show_all();
        })
        .context("could not reach the strip's GTK widget")
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

#[cfg(not(target_os = "linux"))]
pub fn adopt_strip<R: tauri::Runtime>(_strip: &Webview<R>) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        BINDINGS, PALETTE_HEIGHT, PALETTE_MAX_WIDTH, PALETTE_MIN_WIDTH, PALETTE_SIDE_MARGIN,
        palette_size,
    };

    /// The card tracks the window between the bounds, and stays inside it
    /// outside them — a card clipped by the overlay loses its right-hand edge
    /// and, on the tab rows, its close button with it.
    #[test]
    fn the_card_tracks_the_window_width() {
        assert_eq!(palette_size(1400, 900).0, 700);
        assert_eq!(palette_size(1000, 900).0, 500);

        // Bounded at both ends.
        assert_eq!(palette_size(3440, 900).0, PALETTE_MAX_WIDTH);
        assert_eq!(palette_size(700, 900).0, PALETTE_MIN_WIDTH);

        // And on a window too narrow for even the minimum, the margins win.
        let narrow = palette_size(360, 900).0;
        assert!(narrow < PALETTE_MIN_WIDTH, "{narrow} should have been squeezed");
        assert_eq!(narrow, 360 - 2 * PALETTE_SIDE_MARGIN);

        // A short window trims the card rather than overflowing the page.
        assert_eq!(palette_size(1400, 900).1, PALETTE_HEIGHT);
        assert!(palette_size(1400, 500).1 < PALETTE_HEIGHT);
        assert!(palette_size(1400, 100).1 > 0);
    }

    /// Every chord must name a command that exists, with arguments that command
    /// will accept. Without this the failure mode is a key that silently does
    /// nothing, which is exactly what the old `Action` enum could not suffer
    /// from and the table can.
    #[tokio::test]
    async fn every_binding_resolves_against_the_graph() {
        let state = std::sync::Arc::new(crate::state::AppState::detached());
        let cli = crate::commands::command_graph(state);
        let catalog = cli.try_tool_catalog().expect("the graph has unique tool names");

        for b in BINDINGS {
            let def = catalog
                .get(b.tool)
                .unwrap_or_else(|| panic!("binding names a missing command: {}", b.tool));

            let args = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(b.args)
                .unwrap_or_else(|e| panic!("{} has unparseable args {:?}: {e}", b.tool, b.args));

            let props = def.input_schema.get("properties").and_then(|p| p.as_object());
            for key in args.keys() {
                assert!(
                    props.is_some_and(|p| p.contains_key(key)),
                    "{} does not accept an argument named {key:?}",
                    b.tool
                );
            }
        }
    }

    /// Two chords that differ only by Shift must both be reachable: a `None`
    /// shift earlier in the table would shadow the `Some(true)` entry after it.
    #[test]
    fn shift_variants_are_not_shadowed() {
        for (i, b) in BINDINGS.iter().enumerate() {
            let Some(_) = b.shift else { continue };
            let shadowed = BINDINGS[..i].iter().any(|earlier| {
                earlier.ctrl == b.ctrl
                    && earlier.alt == b.alt
                    && earlier.shift.is_none()
                    && earlier.keys.iter().any(|k| b.keys.contains(k))
            });
            assert!(!shadowed, "{} is unreachable behind an earlier binding", b.tool);
        }
    }
}
