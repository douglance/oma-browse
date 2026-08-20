//! The tab strip: one thin row of favicons above the page.
//!
//! Deliberately not a tab *bar*. There are no labels, no numbers and no close
//! buttons on it -- naming and closing tabs is what the palette is for, and a
//! strip that grows text starts stealing rows from the page, which is the thing
//! [`crate::window`] exists to avoid. What is left is the one question a glance
//! at the top of the window should answer: which sites are open, and which of
//! them is this. Omarchy's own menubar is the same shape.
//!
//! It floats: the strip is an overlay child, and the room it needs at rest is
//! made *inside the document* by [`inset_script`] instead of being taken out of
//! the window. The two together are what gives it the behaviour a phone browser
//! has -- nothing is covered at the top of a page, because the inset is the
//! first thing on it, and the content slides under the strip the moment that
//! inset scrolls away. Taking the height out of the window instead would mean
//! the page could never reach the top of the screen at all.

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::page;
use topcoat::runtime::{Event, procedure};
use topcoat::view::{Unescaped, view};

use crate::ui::{registry, state};

/// The webview label, and the route it points at.
pub const LABEL: &str = "strip";

/// The font stack our own surfaces ask for.
///
/// Nerd Font by name rather than through `--oma-font-mono`: that token is
/// whatever fontconfig calls `monospace`, which is not guaranteed to be
/// patched -- and an unpatched font renders the strip's glyphs as tofu.
pub const DEFAULT_FONT: &str =
    "\"JetBrainsMono Nerd Font\", \"Symbols Nerd Font\", var(--oma-font-mono), monospace";

/// The `:root` block carrying what our own chrome is configured to look like.
///
/// Emitted per page rather than added to the theme's token block: these are
/// settings of the browser, not properties of the Omarchy theme, and
/// `oma-theme` has no business knowing about either.
pub fn chrome_vars(config: &crate::config::Config, page_veil: f64) -> String {
    format!(
        ":root {{ --oma-chrome-font: {font}; --oma-chrome-alpha: {alpha}; }}",
        font = config.chrome.font,
        // `auto` for chrome means "whatever the page settled on", which is the
        // only sense in which a palette card can be automatic.
        alpha = config.chrome.veil.resolve(page_veil),
    )
}

#[page("/strip")]
async fn strip(cx: &Cx) -> Result {
    let state = state(cx);
    let theme = state.theme.read().await;
    let vars = Unescaped::new_unchecked(theme.css.chrome.clone());
    let mine = Unescaped::new_unchecked(chrome_vars(&state.config, theme.css.opacity));
    let sheet = Unescaped::new_unchecked(STRIP_CSS);
    drop(theme);

    let tabs = state.tabs.read().await.list();
    // The active tab's title, which is the only text on the strip. Empty when
    // nothing is open, which is a state the window can be in for one frame at
    // startup and permanently after the last tab closes.
    let title = tabs.iter().find(|t| t.active).map(|t| t.title.clone()).unwrap_or_default();

    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8">
                <title>"Tabs"</title>
                <style>(vars)</style>
                <style>(mine)</style>
                <style>(sheet)</style>
            </head>
            <body>
                // Every click on the strip lands here and is told apart by the
                // id of what was hit, exactly as in the palette: the expression
                // subset can hand a handler scalars off the event and nothing
                // else, so the intent has to travel in the DOM.
                //
                // The signal is written but never read. A procedure's return
                // value has to go somewhere, and there is nothing on a strip
                // that changes shape in response to a click -- the reload that
                // follows a tab switch is what redraws it.
                signal ack = String::new();

                <div class="bar" @click=$(async |e: Event| { ack.set(strip_act(e.target.id).await); })>
                    <div class="tabs">
                        for tab in tabs {
                            <button
                                class=(if tab.active { "tab on" } else { "tab" })
                                id=(format!("do-tab:{}", tab.id))
                                title=(tab.title.clone())
                            >
                                if tab.icon.is_empty() {
                                    <span class="glyph">(Unescaped::new_unchecked(GLOBE))</span>
                                } else {
                                    <img class="icon" src=(tab.icon.clone()) alt="">
                                }
                            </button>
                        }
                    </div>

                    if state.config.chrome.strip.title {
                        <div class="title" title=(title.clone())>(title)</div>
                    } else {
                        // The grid wants its middle track either way, or the
                        // gear stops being pinned to the right.
                        <div class="title"></div>
                    }

                    <button class="gear" id="do-settings" title="Settings">(GEAR)</button>
                </div>
                topcoat::runtime::script()
            </body>
        </html>
    }
}

/// Act on a click, by the id of whatever was hit.
///
/// Through the command graph rather than straight into [`crate::tabs`], so the
/// strip switches tabs by the same route the palette, the keyboard, the CLI and
/// MCP all do.
#[procedure]
async fn strip_act(cx: &Cx, spec: String) -> Result<String> {
    let state = state(cx).clone();
    let catalog = &registry(cx).catalog;

    let Some(rest) = spec.strip_prefix("do-") else { return Ok(String::new()) };

    // The gear is the whole of "settings": everything configurable is a command,
    // and the palette is where commands are. A second settings surface would be
    // a second place for the same list to drift out of date.
    if rest == "settings" {
        if let Some(message) =
            crate::dispatch::run(catalog, "ui_palette", arg("action", "show")).await
        {
            crate::dispatch::toast(&message);
        }
        return Ok(String::new());
    }

    if let Some(("tab", id)) = rest.split_once(':') {
        if let Some(message) = crate::dispatch::run(catalog, "tab_select", arg("id", id)).await {
            crate::dispatch::toast(&message);
        }
        state.notify_tabs();
    }
    Ok(String::new())
}

fn arg(name: &str, value: &str) -> std::collections::BTreeMap<String, serde_json::Value> {
    let mut map = std::collections::BTreeMap::new();
    map.insert(name.to_string(), serde_json::Value::String(value.to_string()));
    map
}

/// Stand-in for a tab whose favicon has not arrived, or never will.
///
/// A blank square reads as a broken tab and a spinner would be a lie -- plenty
/// of sites simply ship no icon, and this is what they look like permanently.
///
/// Drawn rather than set in the Nerd Font, which the gear beside it still is.
/// Every icon in the patched font has ink wider than its cell and hanging off
/// to the right -- `nf-fa-globe` measures 12.0px of ink in a 7.8px advance at
/// 13px, so centring the *cell* leaves the drawing 2.1px right of centre. That
/// is invisible in a terminal, where these glyphs live in a row of other
/// glyphs, and obvious here, sitting in a row of 16px favicons with a mark
/// centred underneath. An SVG has the geometry it says it has.
const GLOBE: &str = r#"<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.7"><circle cx="12" cy="12" r="9"/><path d="M3 12h18"/><ellipse cx="12" cy="12" rx="4.6" ry="9"/></svg>"#;

/// The settings gear: `nf-fa-cog`.
const GEAR: &str = "\u{f013}";

/// The room the strip needs, made at the top of every loaded page.
///
/// Padding on `html`, so it is part of the document and scrolls away with it --
/// the whole point. A fixed spacer would stay put and the strip would never
/// come to rest over anything.
///
/// Injected alongside the theme's page script and, like it, keyed off a sentinel
/// id so that re-running it on a live document updates the rule in place rather
/// than stacking another style element behind it.
///
/// The one thing this cannot reserve room for is a site's own `position: fixed`
/// header, which by definition ignores the document's padding and will sit under
/// the strip at the top of the page. That is the cost of floating; a strip that
/// covered nothing would have to be a strip that never moved.
pub fn inset_script(height: i32) -> String {
    format!(
        r#"(function () {{
  "use strict";
  var ID = "__oma_strip_inset";
  var CSS = "html{{padding-top:{height}px !important;}}";
  function apply() {{
    var el = document.getElementById(ID);
    if (!el) {{
      el = document.createElement("style");
      el.id = ID;
      // `head` is null at document-start, which is when this first runs.
      (document.head || document.documentElement).appendChild(el);
    }}
    if (el.textContent !== CSS) el.textContent = CSS;
  }}
  apply();
  // Sites that replace their own head on load take the rule with it.
  if (document.readyState === "loading") {{
    document.addEventListener("DOMContentLoaded", apply, {{ once: true }});
  }}
}})();"#
    )
}

const STRIP_CSS: &str = r##"
* { box-sizing: border-box; }
html, body {
  margin: 0; height: 100%; overflow: hidden;
  /* See `DEFAULT_FONT`; `chrome.font` in the config file replaces it. */
  font-family: var(--oma-chrome-font);
  font-size: var(--oma-font-small);
  /* No surface of its own. The strip sits in the window's own background, and a
     bar painted here would be a second one drawn on top of it. */
  color: var(--oma-menu-fg); background: transparent;
  /* Nothing on the strip is text to select, and a drag across it selecting the
     title looks like a bug. */
  user-select: none; -webkit-user-select: none;
  cursor: default;
}

/* Three tracks, with the outer two equal, so the title sits on the window's
   centre line rather than in the middle of whatever space the tabs left. */
.bar {
  height: 100%;
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, auto) minmax(0, 1fr);
  align-items: center;
  gap: var(--oma-space);
  padding: 0 6px;
}

.tabs { display: flex; align-items: center; gap: 2px; min-width: 0; overflow: hidden; }

.tab {
  flex: none;
  position: relative;
  /* Two px taller than it needs to be for a 16px icon, and the mark lives in
     that slack. `.tabs` clips (it has to, so a long row of tabs runs off the
     end rather than shoving the title aside), so a mark hung below the button
     is simply not drawn. */
  width: 22px; height: 22px;
  display: grid; place-items: center;
  padding: 0; border: 0; border-radius: var(--oma-radius);
  background: transparent; color: inherit; font: inherit;
  /* Unvisited-looking rather than invisible: the inactive tabs are still the
     point of the strip, they just are not the one being read. */
  opacity: 0.55;
  cursor: pointer;
}
.tab:hover { opacity: 1; background: var(--oma-menu-selected-bg); }
/* No fill of its own: a filled square behind the favicon is a second shape
   competing with the icon, and it reads as the hover highlight stuck on. The
   mark below carries the whole signal. */
.tab.on { opacity: 1; }
/* The mark spans the whole button rather than sitting under the icon: at 22px
   wide there is no room for a bar to be *nearly* as wide as the thing above it
   without looking mis-set, and a full-width bar has no free edges to round
   differently from the icon on a fractionally scaled output. */
.tab.on::after {
  content: "";
  /* Inside the button, in the slack under a 16px icon centred in 22px: 3px of
     clear air between the icon and the mark, and nothing to clip. */
  position: absolute; left: 0; right: 0; bottom: 0;
  height: 2px; background: var(--oma-accent);
}

/* The icon must never be the click target: the handler reads the id off
   whatever was hit, and only the button carries one.
   The margin is what keeps clear air under the icon: the button centres its one
   child, so lifting the child by 2px puts 2px between it and the mark along the
   bottom edge. An even number on purpose -- an odd one centres on a half pixel
   and the icon blurs. */
.icon, .glyph { pointer-events: none; margin-bottom: 2px; }
.icon { width: 16px; height: 16px; display: block; }
/* The same 16px box as a favicon, so a tab with no icon lines up with the ones
   that have one -- and with the mark underneath. */
.glyph { width: 16px; height: 16px; display: block; color: var(--oma-menu-fg); }
.glyph svg { display: block; }

.title {
  min-width: 0;
  text-align: center;
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
  color: var(--oma-muted);
}

.gear {
  justify-self: end;
  width: 20px; height: 20px;
  display: grid; place-items: center;
  padding: 0; border: 0; border-radius: var(--oma-radius);
  background: transparent; color: var(--oma-muted);
  font: inherit; font-size: 13px; line-height: 1;
  opacity: 0.7; cursor: pointer;
}
.gear:hover { opacity: 1; color: var(--oma-menu-fg); background: var(--oma-menu-selected-bg); }
"##;

#[cfg(test)]
mod tests {
    use super::{GEAR, GLOBE};

    #[test]
    fn the_glyphs_are_in_the_nerd_font_private_use_area() {
        // A codepoint that fell outside it is one some unpatched font will
        // happily render as something else entirely.
        let c = GEAR.chars().next().expect("a glyph");
        assert_eq!(GEAR.chars().count(), 1, "one codepoint, not a sequence");
        assert!(('\u{e000}'..='\u{f8ff}').contains(&c), "{GEAR:?} is outside the private use area");
    }

    /// The placeholder is drawn rather than typed, and the two things that make
    /// it line up are the square viewBox and the 16px box it declares -- the
    /// same box a favicon gets. `currentColor` is what keeps it themed.
    #[test]
    fn the_placeholder_is_a_square_themed_svg() {
        assert!(GLOBE.contains(r#"viewBox="0 0 24 24""#), "not a square viewBox");
        assert!(GLOBE.contains(r#"width="16" height="16""#), "not a favicon-sized box");
        assert!(GLOBE.contains("currentColor"), "a hard-coded colour would not follow the theme");
    }
}
