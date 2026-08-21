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

/// The card's size for a window of the given size.
///
/// The ratio is what shapes it; the bounds keep it a card at both extremes, and
/// the margins win over the minimum, because a card wider than the window is
/// clipped by the overlay rather than centred in it. Every number is a
/// `[chrome.palette]` setting; the defaults live in [`crate::config::Palette`].
fn palette_size(p: &crate::config::Palette, window_width: i32, window_height: i32) -> (i32, i32) {
    let ideal = (f64::from(window_width) * p.width_ratio).round() as i32;
    let room = (window_width - 2 * p.side_margin).max(1);
    let width = ideal.clamp(p.min_width, p.max_width).min(room);

    let available = window_height - p.top_margin - p.bottom_margin;
    let height = available.clamp(p.min_height, p.height);

    (width, height)
}

/// Height of the tab strip, in pixels, unless the config file says otherwise.
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
pub fn install<R: tauri::Runtime>(
    palette: &Webview<R>,
    config: &crate::config::Chrome,
) -> Result<()> {
    // Escape hatch for bisecting rendering problems: reparenting a
    // WebKitWebView is the most invasive thing we do to the widget tree, so
    // `OMA_LAYOUT=plain` leaves Tauri's own vertical box alone.
    if config.plain_layout || std::env::var("OMA_LAYOUT").as_deref() == Ok("plain") {
        tracing::warn!("plain layout: skipping the overlay; tabs will tile, not stack");
        return Ok(());
    }
    // Cloned out of the config before the closure: `with_webview` runs on the
    // GTK thread whenever it gets there, and the handler it installs outlives
    // this call entirely.
    let geometry = config.palette.clone();

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
            palette_widget.set_size_request(geometry.min_width, geometry.height);
            palette_widget.set_halign(gtk::Align::Center);
            palette_widget.set_valign(gtk::Align::Start);
            palette_widget.set_margin_top(geometry.top_margin);

            // GTK hands the overlay its allocation on every layout pass, which
            // makes it the one place that knows how wide the window currently
            // is — Tauri's own resize events do not reach this far down.
            let card = palette_widget.clone();
            let last = std::cell::Cell::new((0, 0));
            overlay.connect_size_allocate(move |_, alloc| {
                let size = palette_size(&geometry, alloc.width(), alloc.height());
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
    // Resolved out here, on the async side, where complaining about the config
    // file is possible: the closure below runs on the GTK thread.
    let bound = {
        let complain = |problem: String| state.note_config_problem(problem);
        bindings(&state.config, &catalog, &complain)
    };
    tracing::debug!(count = bound.len(), "key bindings resolved");

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

                // An atomic load, not a lock: this runs on the GTK main thread
                // for every keystroke in the window, and blocking here would
                // freeze the whole UI.
                let palette_open = state.palette_visible();

                let hit = bound
                    .iter()
                    .find(|b| b.matches(ctrl, alt, shift, &name) && b.when.applies(palette_open));

                match hit {
                    Some(b) => {
                        let catalog = catalog.clone();
                        let args = b.args.clone();
                        let (tool, quiet) = (b.tool.clone(), b.propagate);
                        runtime.spawn(async move {
                            if let Some(message) = crate::dispatch::run(&catalog, &tool, args).await
                            {
                                // A propagating chord fires on presses that were
                                // meant for the page, so its failures are not
                                // news: Escape on a window with no tab would
                                // otherwise raise a notification every time.
                                if quiet {
                                    tracing::debug!(%tool, %message, "command declined");
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

/// A binding as it is actually installed: the built-in table with the config
/// file's `[keys]` merged over it.
///
/// Owned, where [`Binding`] is `&'static`, because a chord read out of a file
/// cannot be. Everything else about it is the same.
#[derive(Debug, Clone)]
pub struct Bound {
    ctrl: bool,
    alt: bool,
    shift: Option<bool>,
    keys: Vec<String>,
    tool: String,
    args: std::collections::BTreeMap<String, serde_json::Value>,
    when: When,
    propagate: bool,
}

impl Bound {
    fn matches(&self, ctrl: bool, alt: bool, shift: bool, name: &str) -> bool {
        self.ctrl == ctrl
            && self.alt == alt
            && self.shift.is_none_or(|want| want == shift)
            && self.keys.iter().any(|k| k == name)
    }

    /// Whether a chord from the config file addresses this binding.
    ///
    /// Key and modifiers, but *not* Shift when the built-in does not care about
    /// it: `Ctrl-K` is bound with `shift: None`, and someone rebinding it will
    /// write `"ctrl+k"`, not `"ctrl+shift+k"` as well.
    fn addressed_by(&self, chord: &crate::keys::Chord) -> bool {
        self.ctrl == chord.ctrl
            && self.alt == chord.alt
            && (self.shift.is_none() || self.shift == chord.shift)
            && self.keys.iter().any(|k| chord.keys.contains(k))
    }
}

/// The bindings to install: the table, plus whatever `[keys]` says.
///
/// A config chord that addresses an existing binding replaces its command; one
/// that does not is added; an empty command unbinds. Everything that goes wrong
/// -- an unparseable chord, a command the graph does not have, an argument it
/// will not accept -- is reported through `note_config_problem` and then
/// *skipped*, because a key that runs nothing is better than one that runs
/// something unintended, and both are better than refusing to start.
pub fn bindings(
    config: &crate::config::Config,
    catalog: &incurs::tool::ToolCatalog,
    complain: &impl Fn(String),
) -> Vec<Bound> {
    let mut out: Vec<Bound> = BINDINGS
        .iter()
        .map(|b| Bound {
            ctrl: b.ctrl,
            alt: b.alt,
            shift: b.shift,
            keys: b.keys.iter().map(|k| (*k).to_string()).collect(),
            tool: b.tool.to_string(),
            args: crate::dispatch::args_from_json(b.args),
            when: b.when,
            propagate: b.propagate,
        })
        .collect();

    for (spec, command) in &config.keys {
        let chord = match crate::keys::parse_chord(spec) {
            Ok(chord) => chord,
            Err(e) => {
                complain(format!("keys: {e}"));
                continue;
            }
        };

        // An empty command unbinds, which is the only way to get a key back
        // from the browser and give it to the page.
        if command.trim().is_empty() {
            out.retain(|b| !b.addressed_by(&chord));
            continue;
        }

        let tool = command.split_whitespace().next().unwrap_or_default();
        let Some(def) = catalog.get(tool) else {
            complain(format!("keys: {spec:?} names {tool:?}, which is not a command"));
            continue;
        };
        let props = def.input_schema.get("properties").and_then(|p| p.as_object());
        let (tool, args) = match crate::keys::parse_command(command, props) {
            Ok(parsed) => parsed,
            Err(e) => {
                complain(format!("keys: {spec:?}: {e}"));
                continue;
            }
        };
        if let Some(bad) = args.keys().find(|k| !props.is_some_and(|p| p.contains_key(*k))) {
            complain(format!("keys: {tool} does not take an argument named {bad:?}"));
            continue;
        }

        // Replace in place where it addresses something, so a rebound chord
        // keeps the built-in's `when` and `propagate` -- rebinding Escape
        // should not quietly stop it reaching the page.
        let mut replaced = false;
        for existing in out.iter_mut().filter(|b| b.addressed_by(&chord)) {
            existing.tool = tool.clone();
            existing.args = args.clone();
            replaced = true;
        }
        if !replaced {
            out.push(Bound {
                ctrl: chord.ctrl,
                alt: chord.alt,
                shift: chord.shift,
                keys: chord.keys.clone(),
                tool: tool.clone(),
                args: args.clone(),
                when: When::Always,
                propagate: false,
            });
        }
    }

    out
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
        ctrl: true,
        alt: false,
        shift: Some(false),
        keys,
        tool: "tab_select",
        args,
        when: When::Always,
        propagate: false,
    }
}

const BINDINGS: &[Binding] = &[
    // Ctrl-K, Ctrl-L and Ctrl-P all summon the palette: one is "command", one
    // is URL-bar muscle memory, and here they are the same thing.
    Binding {
        ctrl: true,
        alt: false,
        shift: None,
        keys: &["k", "K", "l", "L"],
        tool: "ui_palette",
        args: r#"{"action":"toggle"}"#,
        when: When::Always,
        propagate: false,
    },
    // Ctrl-P is the same summon, but Shift is excluded rather than ignored so
    // that Ctrl-Shift-P can reach `page print` below. Both letter cases stay
    // listed: Caps Lock reports `"P"` with Shift *up*.
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(false),
        keys: &["p", "P"],
        tool: "ui_palette",
        args: r#"{"action":"toggle"}"#,
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(false),
        keys: &["t", "T"],
        tool: "tab_open",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    // A window is a second process, not a second window in this one; see
    // `window::spawn` for why. Shift is excluded so that a future Ctrl-Shift-N
    // -- incognito, everywhere else -- cannot be swallowed by this row.
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(false),
        keys: &["n", "N"],
        tool: "window_new",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    // Ctrl-Shift-W closes the window; it must come *before* Ctrl-W, which would
    // otherwise close a single tab and swallow the chord.
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(true),
        keys: &["w", "W"],
        tool: "window_close",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(false),
        keys: &["w", "W"],
        tool: "tab_close",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: true,
        alt: false,
        shift: None,
        keys: &["r", "R"],
        tool: "nav_reload",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: false,
        alt: false,
        shift: None,
        keys: &["F5"],
        tool: "nav_reload",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    // Ctrl-Shift-T reopens; it must come *before* the plain Ctrl-T entry, which
    // does not care about Shift and would otherwise swallow it.
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(true),
        keys: &["t", "T"],
        tool: "tab_reopen",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    // Find. Ctrl-F stages the palette rather than opening a find bar: the
    // palette already knows how to prompt for an argument from the schema, and
    // a second text box on screen would be a second thing to learn.
    Binding {
        ctrl: true,
        alt: false,
        shift: None,
        keys: &["f", "F"],
        tool: "ui_palette",
        args: r#"{"action":"show","stage":"find_text"}"#,
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(false),
        keys: &["g", "G"],
        tool: "find_next",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(true),
        keys: &["g", "G"],
        tool: "find_previous",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: false,
        alt: false,
        shift: None,
        keys: &["F3"],
        tool: "find_next",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: true,
        alt: false,
        shift: None,
        keys: &["d", "D"],
        tool: "bookmark_add",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    // `equal` is the unshifted key people actually press for "zoom in"; `plus`
    // is what arrives once Shift is held, and the KP_ names are the numeric
    // keypad. Bind all of them, the way every other browser does.
    Binding {
        ctrl: true,
        alt: false,
        shift: None,
        keys: &["plus", "equal", "KP_Add"],
        tool: "page_zoom",
        args: r#"{"direction":"in"}"#,
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: true,
        alt: false,
        shift: None,
        keys: &["minus", "underscore", "KP_Subtract"],
        tool: "page_zoom",
        args: r#"{"direction":"out"}"#,
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: true,
        alt: false,
        shift: None,
        keys: &["0", "KP_0"],
        tool: "page_zoom",
        args: r#"{"direction":"reset"}"#,
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(false),
        keys: &["Tab", "ISO_Left_Tab"],
        tool: "tab_cycle",
        args: r#"{"delta":1}"#,
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(true),
        keys: &["Tab", "ISO_Left_Tab"],
        tool: "tab_cycle",
        args: r#"{"delta":-1}"#,
        when: When::Always,
        propagate: false,
    },
    // The other half of Ctrl-Tab's muscle memory. `Prior`/`Next` are the names
    // GDK reports on some layouts, and the KP_ pair is the keypad with NumLock
    // off, where these keys live for anyone using a compact keyboard.
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(false),
        keys: &["Page_Down", "Next", "KP_Page_Down", "KP_Next"],
        tool: "tab_cycle",
        args: r#"{"delta":1}"#,
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(false),
        keys: &["Page_Up", "Prior", "KP_Page_Up", "KP_Prior"],
        tool: "tab_cycle",
        args: r#"{"delta":-1}"#,
        when: When::Always,
        propagate: false,
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
        ctrl: false,
        alt: true,
        shift: None,
        keys: &["Left"],
        tool: "nav_back",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: false,
        alt: true,
        shift: None,
        keys: &["Right"],
        tool: "nav_forward",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: false,
        alt: true,
        shift: None,
        keys: &["Home", "KP_Home"],
        tool: "nav_home",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: false,
        alt: false,
        shift: None,
        keys: &["F11"],
        tool: "window_fullscreen",
        args: r#"{"action":"toggle"}"#,
        when: When::Always,
        propagate: false,
    },
    // Escape, twice. With the palette up it dismisses it and goes no further --
    // that is what the key was pressed for. With the palette down it stops a
    // load *and* reaches the page, because a page's own Escape closes its
    // modals, its autocomplete and its fullscreen video, and this handler runs
    // before any of them.
    Binding {
        ctrl: false,
        alt: false,
        shift: None,
        keys: &["Escape"],
        tool: "ui_palette",
        args: r#"{"action":"hide"}"#,
        when: When::PaletteOpen,
        propagate: false,
    },
    Binding {
        ctrl: false,
        alt: false,
        shift: None,
        keys: &["Escape"],
        tool: "nav_stop",
        args: "{}",
        when: When::PaletteClosed,
        propagate: true,
    },
    // Everything below reaches a command that already existed only in the
    // palette. Chrome's chord where Chrome has one, so the muscle memory
    // transfers; the two that Chrome has no answer for are noted.
    Binding {
        ctrl: false,
        alt: false,
        shift: None,
        keys: &["F12"],
        tool: "page_devtools",
        args: r#"{"action":"toggle"}"#,
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(true),
        keys: &["i", "I"],
        tool: "page_devtools",
        args: r#"{"action":"toggle"}"#,
        when: When::Always,
        propagate: false,
    },
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(false),
        keys: &["u", "U"],
        tool: "page_source",
        args: r#"{"open":true}"#,
        when: When::Always,
        propagate: false,
    },
    // Chrome prints on Ctrl-P. Here Ctrl-P is one of the three palette
    // summons and has been since before printing existed, so taking it would
    // break a chord the user reaches for daily to fix one they have never had.
    // Shift instead, and `[keys]` in the config file moves it for anyone who
    // wants the Chrome placement. No arguments, so this writes a PDF to the
    // downloads directory and says where -- deliberately not `--dialog`, which
    // wedges the window under this runtime; see `tabs::print`.
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(true),
        keys: &["p", "P"],
        tool: "page_print",
        args: "{}",
        when: When::Always,
        propagate: false,
    },
    // Chrome's Ctrl-J opens a downloads *page*; there is no such page here, and
    // the useful half of that gesture is "give me the thing I just downloaded".
    // Harmless on a fresh browser: `download open` answers `missing_id`, which
    // `dispatch::run` turns into a toast.
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(false),
        keys: &["j", "J"],
        tool: "download_open",
        args: r#"{"index":1}"#,
        when: When::Always,
        propagate: false,
    },
    // No Chrome or Firefox equivalent -- both bury muting in a tab context
    // menu. A tab that will not shut up is a daily problem and this is a
    // browser for people who would rather not reach for the mouse.
    Binding {
        ctrl: true,
        alt: false,
        shift: Some(false),
        keys: &["m", "M"],
        tool: "tab_mute",
        args: r#"{"action":"toggle"}"#,
        when: When::Always,
        propagate: false,
    },
    // Link hints (`f`, `F`) are deliberately *not* here. A toplevel accelerator
    // on a bare letter fires wherever the caret is, so it would swallow the `f`
    // in every search box on the web; only the page knows whether a keystroke
    // is text. They are bound in `hints.js` as a capturing page handler, and
    // the Escape row above is what lets the page cancel them.
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
pub fn adopt_strip<R: tauri::Runtime>(strip: &Webview<R>, height: i32) -> Result<()> {
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
                tracing::error!(
                    "no overlay; the strip would tile with the page, so leaving it out"
                );
                return;
            };

            vbox.remove(&widget);
            overlay.add_overlay(&widget);

            // Full width, pinned to the top, exactly as tall as it needs to be.
            // Added after the palette, so it sits above it in the stacking
            // order -- which costs nothing, since the card starts well below.
            widget.set_halign(gtk::Align::Fill);
            widget.set_valign(gtk::Align::Start);
            widget.set_size_request(-1, height);

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
            .filter_map(|o| {
                o.children().into_iter().find(|c| c.widget_name() == CONTENT_STACK_NAME)
            })
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
pub fn install<R: tauri::Runtime>(
    _palette: &Webview<R>,
    _config: &crate::config::Chrome,
) -> Result<()> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn adopt_tab<R: tauri::Runtime>(_tab: &Webview<R>) -> Result<()> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn adopt_strip<R: tauri::Runtime>(_strip: &Webview<R>, _height: i32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{BINDINGS, When, palette_size};

    /// The card tracks the window between the bounds, and stays inside it
    /// outside them — a card clipped by the overlay loses its right-hand edge
    /// and, on the tab rows, its close button with it.
    #[test]
    fn the_card_tracks_the_window_width() {
        let p = crate::config::Palette::default();

        assert_eq!(palette_size(&p, 1400, 900).0, 700);
        assert_eq!(palette_size(&p, 1000, 900).0, 500);

        // Bounded at both ends.
        assert_eq!(palette_size(&p, 3440, 900).0, p.max_width);
        assert_eq!(palette_size(&p, 700, 900).0, p.min_width);

        // And on a window too narrow for even the minimum, the margins win.
        let narrow = palette_size(&p, 360, 900).0;
        assert!(narrow < p.min_width, "{narrow} should have been squeezed");
        assert_eq!(narrow, 360 - 2 * p.side_margin);

        // A short window trims the card rather than overflowing the page.
        assert_eq!(palette_size(&p, 1400, 900).1, p.height);
        assert!(palette_size(&p, 1400, 500).1 < p.height);
        assert!(palette_size(&p, 1400, 100).1 > 0);
    }

    /// A card configured wider than it is allowed to be is still a card: the
    /// bounds are what keep `width_ratio = 1.0` from becoming a bar.
    #[test]
    fn the_configured_bounds_are_what_hold() {
        let wide = crate::config::Palette { width_ratio: 1.0, ..Default::default() };
        assert_eq!(palette_size(&wide, 1400, 900).0, wide.max_width);

        let roomy = crate::config::Palette { max_width: 1200, ..Default::default() };
        assert_eq!(palette_size(&roomy, 1400, 900).0, 700, "the ratio still shapes it");
    }

    /// What the config file may do to the table: replace, add, unbind.
    #[tokio::test]
    async fn the_config_file_rebinds_adds_and_unbinds() {
        use crate::config::Config;

        let state = std::sync::Arc::new(crate::state::AppState::detached());
        let catalog = crate::commands::command_graph(state)
            .try_tool_catalog()
            .expect("the graph has unique tool names");
        let quiet = |_: String| {};

        let stock = super::bindings(&Config::default(), &catalog, &quiet);
        let bound_to = |list: &[super::Bound], key: &str, ctrl: bool, shift: bool| {
            list.iter().find(|b| b.matches(ctrl, false, shift, key)).map(|b| b.tool.clone())
        };
        assert_eq!(bound_to(&stock, "t", true, false).as_deref(), Some("tab_open"));

        let mut config = Config::default();
        config.keys.insert("ctrl+t".into(), "page_hints".into());
        config.keys.insert("ctrl+shift+j".into(), "tab_cycle --delta -1".into());
        config.keys.insert("ctrl+d".into(), String::new());
        let merged = super::bindings(&config, &catalog, &quiet);

        assert_eq!(
            bound_to(&merged, "t", true, false).as_deref(),
            Some("page_hints"),
            "an addressed chord is replaced, not duplicated"
        );
        assert_eq!(
            merged.iter().filter(|b| b.matches(true, false, false, "t")).count(),
            1,
            "and exactly once"
        );
        assert_eq!(
            bound_to(&merged, "j", true, true).as_deref(),
            Some("tab_cycle"),
            "a chord nothing holds is added"
        );
        assert!(bound_to(&merged, "d", true, false).is_none(), "an empty command unbinds");
    }

    /// A broken line in the file loses that one binding and nothing else.
    #[tokio::test]
    async fn a_bad_binding_is_skipped_and_reported() {
        use crate::config::Config;

        let state = std::sync::Arc::new(crate::state::AppState::detached());
        let catalog = crate::commands::command_graph(state)
            .try_tool_catalog()
            .expect("the graph has unique tool names");

        let mut config = Config::default();
        config.keys.insert("ctrl+shift".into(), "tab_open".into()); // no key
        config.keys.insert("ctrl+y".into(), "no_such_command".into());
        config.keys.insert("ctrl+u".into(), "tab_open --nonsense 1".into());

        let complaints = std::sync::Mutex::new(Vec::new());
        let merged = super::bindings(&config, &catalog, &|p| {
            complaints.lock().unwrap().push(p);
        });

        assert_eq!(complaints.lock().unwrap().len(), 3, "each bad line says so once");
        assert!(
            merged.iter().all(|b| b.tool != "no_such_command"),
            "a command that does not exist is never installed"
        );
        assert!(
            merged.iter().any(|b| b.matches(true, false, false, "t")),
            "and the rest of the table survives"
        );
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

    /// Escape is the one key bound twice, and the split is the whole point: it
    /// dismisses the palette when there is one, and otherwise stops the load
    /// *and lets the page have the key*. A page whose modal cannot be closed is
    /// what the single unconditional binding used to cause.
    #[test]
    fn escape_is_bound_once_per_palette_state() {
        let escape: Vec<_> = BINDINGS.iter().filter(|b| b.keys.contains(&"Escape")).collect();
        assert_eq!(escape.len(), 2, "Escape needs one binding per palette state");

        let open =
            escape.iter().find(|b| b.when == When::PaletteOpen).expect("no palette-open Escape");
        let closed = escape
            .iter()
            .find(|b| b.when == When::PaletteClosed)
            .expect("no palette-closed Escape");

        assert_eq!(open.tool, "ui_palette");
        assert!(!open.propagate, "the palette's own Escape is not the page's business");
        assert_eq!(closed.tool, "nav_stop");
        assert!(closed.propagate, "the page must still see Escape");

        // Propagation is the exception, not a thing to reach for: anything else
        // the browser binds, it owns outright.
        let propagating: Vec<&str> =
            BINDINGS.iter().filter(|b| b.propagate).map(|b| b.tool).collect();
        assert_eq!(propagating, vec!["nav_stop"]);
    }

    /// Two bindings that can both fire on the same keystroke: the second is
    /// dead, because the handler takes the first match. `when` splits Escape
    /// legitimately; anything else sharing a chord is a mistake.
    #[test]
    fn no_two_bindings_claim_the_same_chord() {
        for (i, b) in BINDINGS.iter().enumerate() {
            for other in &BINDINGS[i + 1..] {
                let same_mods = b.ctrl == other.ctrl && b.alt == other.alt;
                let same_shift = match (b.shift, other.shift) {
                    (Some(x), Some(y)) => x == y,
                    _ => true,
                };
                let overlaps = [true, false]
                    .iter()
                    .any(|&open| b.when.applies(open) && other.when.applies(open));
                let shared = b.keys.iter().any(|k| other.keys.contains(k));
                assert!(
                    !(same_mods && same_shift && overlaps && shared),
                    "{} and {} both claim the same chord; the second can never fire",
                    b.tool,
                    other.tool
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

    /// Every built-in chord must name a command that exists.
    ///
    /// The config-file chords are checked against the catalog at startup and
    /// skipped with a complaint when they miss (see `bindings`), but the
    /// built-in table is mapped straight through without ever being looked up.
    /// A command renamed in `commands.rs` therefore leaves a key that presses
    /// fine, dispatches into nothing, and says nothing about it -- the one
    /// failure a keyboard-driven browser cannot afford, because the only
    /// symptom is a key that stopped working.
    #[tokio::test]
    async fn every_builtin_chord_names_a_real_command() {
        let state = std::sync::Arc::new(crate::state::AppState::detached());
        let catalog =
            crate::commands::command_graph(state).try_tool_catalog().expect("unique tool names");

        let mut dead = Vec::new();
        for b in BINDINGS {
            if catalog.get(b.tool).is_none() {
                dead.push(b.tool);
            }
        }
        assert!(dead.is_empty(), "chords bound to commands that do not exist: {dead:?}");
    }

    /// And the arguments a chord carries must be ones that command accepts.
    ///
    /// A chord whose command exists but whose arguments are wrong fails at the
    /// far end of the dispatch, where nobody is looking. `page_zoom` taking a
    /// direction and `tab_cycle` taking a delta are the two that would break
    /// quietly if either grew a required field.
    #[tokio::test]
    async fn every_builtin_chord_carries_arguments_that_command_accepts() {
        let state = std::sync::Arc::new(crate::state::AppState::detached());
        let catalog =
            crate::commands::command_graph(state).try_tool_catalog().expect("unique tool names");

        for b in BINDINGS {
            let Some(def) = catalog.get(b.tool) else { continue };
            let known: Vec<String> = def
                .input_schema
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();

            for (name, _) in crate::dispatch::args_from_json(b.args) {
                assert!(
                    known.contains(&name),
                    "{}'s chord passes {name:?}, which {} does not take (it takes {known:?})",
                    b.tool,
                    b.tool
                );
            }
        }
    }
}
