//! The command palette — the browser's entire interface.
//!
//! There is no toolbar. The page owns the window; this card is summoned over it
//! and dismissed again. Everything lives here: the URL bar, tab switching, and
//! settings (nested one level deep rather than spilling into the chrome).
//!
//! Written in Rust throughout: `signal` for the query, a `#[shard]` to re-render
//! the result list server-side as you type, and `#[procedure]` for the actions.
//! The client expression language is a deliberately small subset — `f64` only,
//! no `match`, no struct literals — so anything with real logic in it lives in a
//! procedure, where the whole language is available.

use std::sync::Arc;

use topcoat::asset::{AssetBundle, RouterBuilderAssetExt};
use topcoat::context::app_context;
use topcoat::router::{Router, RouterBuilderDiscoverExt, page};
use topcoat::runtime::{Event, procedure, shard};
use topcoat::view::{Unescaped, view};
use topcoat::{Result, context::Cx};

use crate::state::AppState;

/// Newtype because Topcoat's app context is keyed by `TypeId`, and registering
/// two values of the same type panics.
pub struct SharedState(pub Arc<AppState>);

pub fn router(state: Arc<AppState>) -> Router {
    let mut builder = Router::builder().discover().app_context(SharedState(state));

    // Anything interactive pulls in `topcoat::runtime::script`, which is itself
    // an asset, and assets resolve through a bundle built by scanning the
    // compiled binary (`topcoat asset bundle`). A missing bundle does not fail
    // loudly — it panics the render worker and returns an empty reply — so say
    // something useful here instead.
    match AssetBundle::load() {
        Ok(bundle) => builder = builder.assets(bundle),
        Err(e) => tracing::error!(
            error = %e,
            "no asset bundle next to the binary; run `topcoat asset bundle -p oma-browse`. \
             The palette will not render until you do."
        ),
    }

    builder.build()
}

fn state(cx: &Cx) -> &Arc<AppState> {
    &app_context::<SharedState>(cx).0
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

#[procedure]
async fn act_go(cx: &Cx, query: String) -> Result<()> {
    let state = state(cx).clone();
    if !query.trim().is_empty() {
        let _ = crate::tabs::navigate(&state, &query).await;
    }
    dismiss(&state);
    Ok(())
}

#[procedure]
async fn act_open_tab(cx: &Cx, query: String) -> Result<()> {
    let state = state(cx).clone();
    let target = if query.trim().is_empty() { "about:blank" } else { query.as_str() };
    let _ = crate::tabs::open(&state, target, false).await;
    dismiss(&state);
    Ok(())
}

#[procedure]
async fn act_select_tab(cx: &Cx, id: f64) -> Result<()> {
    let state = state(cx).clone();
    let _ = crate::tabs::select(&state, id as u32).await;
    dismiss(&state);
    Ok(())
}

#[procedure]
async fn act_close_tab(cx: &Cx, id: f64) -> Result<()> {
    let state = state(cx).clone();
    let _ = crate::tabs::close(&state, Some(id as u32)).await;
    state.notify_tabs();
    Ok(())
}

#[procedure]
async fn act_history(cx: &Cx, action: String) -> Result<()> {
    use crate::tabs::HistoryAction;
    let state = state(cx).clone();
    let action = match action.as_str() {
        "back" => HistoryAction::Back,
        "forward" => HistoryAction::Forward,
        "stop" => HistoryAction::Stop,
        _ => HistoryAction::Reload,
    };
    let _ = crate::tabs::history(&state, action).await;
    dismiss(&state);
    Ok(())
}

/// Toggle page recolouring and re-apply it to every open tab.
#[procedure]
async fn act_toggle_recolor(cx: &Cx) -> Result<()> {
    let state = state(cx).clone();
    state.set_recolor(!state.recolor());
    let _ = crate::window::restyle(&state).await;
    Ok(())
}

#[procedure]
async fn act_theme_reload(cx: &Cx) -> Result<()> {
    let state = state(cx).clone();
    state.reload_theme().await;
    dismiss(&state);
    Ok(())
}

#[procedure]
async fn act_dismiss(cx: &Cx) -> Result<()> {
    dismiss(state(cx));
    Ok(())
}

fn dismiss(state: &Arc<AppState>) {
    if let Err(e) = crate::window::set_palette_visible(state, false) {
        tracing::warn!(error = %e, "could not hide the palette");
    }
    state.set_palette_visible(false);
}

// ---------------------------------------------------------------------------
// The palette
// ---------------------------------------------------------------------------

#[page("/palette")]
async fn palette(cx: &Cx) -> Result {
    let theme = state(cx).theme.read().await;
    let vars = Unescaped::new_unchecked(theme.css.chrome.clone());
    let sheet = Unescaped::new_unchecked(PALETTE_CSS);

    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8">
                <title>"Palette"</title>
                <style>(vars)</style>
                <style>(sheet)</style>
            </head>
            <body>
                // Only a bool signal: the query never needs to be one, because
                // `Event` carries `target.value` straight to the handler. That
                // keeps everything inside the supported expression subset.
                signal settings = false;

                <div class="card">
                    <div class="field">
                        <input
                            id="q"
                            class="input"
                            type="text"
                            autofocus="autofocus"
                            autocomplete="off"
                            spellcheck="false"
                            placeholder="Enter a URL or search, then press enter"
                            @keydown=$(async |e: Event| {
                                if e.key == "Enter" {
                                    // The expression subset has no `||`.
                                    if e.ctrl_key {
                                        act_open_tab(e.target.value).await;
                                    } else if e.shift_key {
                                        act_open_tab(e.target.value).await;
                                    } else {
                                        act_go(e.target.value).await;
                                    }
                                } else if e.key == "Escape" {
                                    act_dismiss().await;
                                }
                            })
                        >
                    </div>

                    results(settings: $(settings.get()))

                    <div class="foot">
                        <button class="foot-toggle" @click=$(|_e| settings.toggle())>
                            $(if settings.get() { "Back" } else { "Settings" })
                        </button>
                        <span class="keys">
                            <kbd>"enter"</kbd>" open  "
                            <kbd>"ctrl+enter"</kbd>" new tab  "
                            <kbd>"esc"</kbd>" dismiss"
                        </span>
                    </div>
                </div>
                topcoat::runtime::script()
            </body>
        </html>
    }
}

/// The result list, re-rendered server-side whenever the query changes.
///
/// A shard rather than client-side filtering: the tab list lives on the server,
/// and the expression language cannot express this kind of work anyway.
/// The default page for a tab with nowhere to go yet.
///
/// Served by our own router, so it wears the live theme exactly like the palette
/// does — and it is where a bare `oma-browse` lands.
#[page("/start")]
async fn start(cx: &Cx) -> Result {
    let theme = state(cx).theme.read().await;
    let vars = Unescaped::new_unchecked(theme.css.chrome.clone());
    let sheet = Unescaped::new_unchecked(START_CSS);
    let theme_name = theme.name.clone();

    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8">
                <title>"New tab"</title>
                <style>(vars)</style>
                <style>(sheet)</style>
            </head>
            <body>
                <main>
                    <h1>"oma-browse"</h1>
                    <p class="sub">"Wearing " <strong>(theme_name)</strong></p>
                    <ul class="swatches">
                        <li style="background: var(--oma-color-red)"></li>
                        <li style="background: var(--oma-color-yellow)"></li>
                        <li style="background: var(--oma-color-green)"></li>
                        <li style="background: var(--oma-color-cyan)"></li>
                        <li style="background: var(--oma-color-blue)"></li>
                        <li style="background: var(--oma-color-magenta)"></li>
                    </ul>
                    <p class="hint"><kbd>"ctrl"</kbd>"+"<kbd>"k"</kbd>" opens the palette"</p>
                </main>
            </body>
        </html>
    }
}

const START_CSS: &str = r#"
* { box-sizing: border-box; }
html, body { margin: 0; height: 100%; background: var(--oma-veil); color: var(--oma-fg);
  font-family: system-ui, sans-serif; }
main { height: 100%; display: flex; flex-direction: column; align-items: center;
  justify-content: center; gap: var(--oma-space-2); }
h1 { margin: 0; font-size: 2.5rem; font-weight: 600; color: var(--oma-accent); letter-spacing: -0.02em; }
.sub { margin: 0; color: var(--oma-fg); }
.sub strong { color: var(--oma-accent); font-weight: 600; }
.swatches { display: flex; gap: var(--oma-space); list-style: none; padding: 0; margin: var(--oma-space-2) 0; }
.swatches li { width: 44px; height: 8px; }
.hint { margin: 0; color: var(--oma-muted); font-size: var(--oma-font-small); }
kbd { font-family: var(--oma-font-mono); color: var(--oma-fg);
  border: 1px solid var(--oma-control-normal-border); padding: 0 4px; }
"#;

/// One row of the tab list.
struct Row {
    select_id: String,
    close_id: String,
    marker: &'static str,
    title: String,
    url: String,
}

/// Dispatch a click from the delegated list handler.
///
/// One handler for the whole list, keyed off the clicked element's `id`. Doing
/// it per row would mean a closure capturing the row, and `$(…)` captures only
/// scalars — a plain `let` binding inside a `for` in view scope does not survive
/// the macro either. Delegation keeps every capture out of the loop.
#[procedure]
async fn act_row_click(cx: &Cx, target: String) -> Result<()> {
    let state = state(cx).clone();

    let Some((verb, id)) = target.split_once('-') else { return Ok(()) };
    let Ok(id) = id.parse::<u32>() else { return Ok(()) };

    match verb {
        "select" => {
            let _ = crate::tabs::select(&state, id).await;
            dismiss(&state);
        }
        "close" => {
            let _ = crate::tabs::close(&state, Some(id)).await;
            state.notify_tabs();
        }
        _ => {}
    }
    Ok(())
}

#[shard]
async fn results(cx: &Cx, settings: bool) -> Result {
    let state = state(cx);

    if settings {
        return settings_menu(cx).await;
    }

    let rows: Vec<Row> = state
        .tabs
        .read()
        .await
        .list()
        .into_iter()
        .map(|t| Row {
            select_id: format!("select-{}", t.id),
            close_id: format!("close-{}", t.id),
            marker: if t.active { "●" } else { "○" },
            title: t.title,
            url: t.url,
        })
        .collect();

    view! {
        <ul class="list" @click=$(async |e: Event| { act_row_click(e.target.id).await; })>
            if rows.is_empty() {
                <li class="empty">"No tabs open — type a URL and press enter."</li>
            } else {
                <li class="section">"Tabs"</li>
                for row in rows {
                    <li class="row">
                        <span class="row-icon">(row.marker)</span>
                        <span class="row-main" id=(row.select_id)>
                            <span class="row-title">(row.title)</span>
                            <span class="row-sub">(row.url)</span>
                        </span>
                        <button class="row-close" id=(row.close_id) title="Close tab">"×"</button>
                    </li>
                }
            }
        </ul>
    }
}

/// Settings, one level down rather than spread across a toolbar.
async fn settings_menu(cx: &Cx) -> Result {
    let state = state(cx);
    let theme = state.theme.read().await;
    let theme_name = theme.name.clone();
    let mode = theme.mode.as_str();
    let colors = theme.colors.to_string();
    let recolor_mark = if state.recolor() { "■" } else { "□" };

    // Outside a #[page]/#[component]/#[shard] the macro has no implicit context,
    // so hand it in.
    view! { cx =>
        <ul class="list">
            <li class="section">"Navigation"</li>
            <li class="row" @click=$(async |_e| { act_history("back".to_owned()).await; })>
                <span class="row-icon">"←"</span>
                <span class="row-main"><span class="row-title">"Back"</span></span>
            </li>
            <li class="row" @click=$(async |_e| { act_history("forward".to_owned()).await; })>
                <span class="row-icon">"→"</span>
                <span class="row-main"><span class="row-title">"Forward"</span></span>
            </li>
            <li class="row" @click=$(async |_e| { act_history("reload".to_owned()).await; })>
                <span class="row-icon">"⟳"</span>
                <span class="row-main"><span class="row-title">"Reload"</span></span>
            </li>

            <li class="section">"Theme"</li>
            <li class="row" @click=$(async |_e| { act_toggle_recolor().await; })>
                <span class="row-icon">(recolor_mark)</span>
                <span class="row-main">
                    <span class="row-title">"Repaint neutral page surfaces"</span>
                    <span class="row-sub">
                        "Recolours only greyscale backgrounds, leaving brand colour alone. Can break sites."
                    </span>
                </span>
            </li>
            <li class="row" @click=$(async |_e| { act_theme_reload().await; })>
                <span class="row-icon">"◐"</span>
                <span class="row-main">
                    <span class="row-title">"Re-read the Omarchy theme"</span>
                    <span class="row-sub">
                        (theme_name) " — " (mode) ", " (colors) " colours"
                    </span>
                </span>
            </li>
        </ul>
    }
}

const PALETTE_CSS: &str = r##"
* { box-sizing: border-box; }
html, body {
  margin: 0; height: 100%; background: transparent;
  /* The aesthetic contract is mono-first: dense, precise, terminal-grade. */
  font-family: var(--oma-font-mono); font-size: var(--oma-font-body);
  color: var(--oma-fg);
}
.card {
  height: 100%; display: flex; flex-direction: column;
  /* Same veil as the page, so the palette reads as part of the window. */
  background: var(--oma-veil);
  border: 1px solid var(--oma-border);
  border-radius: var(--oma-radius);
  overflow: hidden;
}
.field {
  display: flex; align-items: center;
  padding: var(--oma-space-3);
  border-bottom: 1px solid var(--oma-border);
}
.input {
  flex: 1; background: transparent; border: none; outline: none;
  color: var(--oma-fg); font: inherit; font-size: var(--oma-font-subtitle);
}
.field:focus-within { border-bottom-color: var(--oma-focus); }
.input::placeholder { color: var(--oma-muted); }
.list { flex: 1; margin: 0; padding: var(--oma-space) 0; list-style: none; overflow-y: auto; }
.section {
  padding: var(--oma-space) var(--oma-space-3);
  font-size: var(--oma-font-caption); text-transform: uppercase;
  letter-spacing: 0.08em; color: var(--oma-muted);
}
.row {
  display: flex; align-items: center; gap: var(--oma-space-2);
  padding: var(--oma-space) var(--oma-space-3);
  cursor: pointer;
}
.row:hover { background: var(--oma-selection); }
.row:hover .row-title { color: var(--oma-accent); }
.row-primary .row-title { color: var(--oma-accent); }
.row-icon { width: 1.2em; text-align: center; color: var(--oma-muted); }
.row-main { flex: 1; min-width: 0; display: flex; flex-direction: column; }
.row-main > * { pointer-events: none; }
.row-title { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.row-sub {
  font-size: var(--oma-font-caption); color: var(--oma-muted);
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.row-close {
  background: transparent; border: none; color: var(--oma-muted);
  font: inherit; cursor: pointer; padding: 0 var(--oma-space);
}
.row-close:hover { color: var(--oma-error); }
.empty { padding: var(--oma-space-3); color: var(--oma-muted); }
.foot {
  display: flex; align-items: center; justify-content: space-between;
  padding: var(--oma-space) var(--oma-space-3);
  border-top: 1px solid var(--oma-border);
  font-size: var(--oma-font-caption); color: var(--oma-muted);
}
.foot-toggle {
  background: transparent; border: none; color: var(--oma-accent);
  font: inherit; font-size: var(--oma-font-caption); cursor: pointer; padding: 0;
}
kbd {
  font-family: var(--oma-font-mono); color: var(--oma-fg);
  border: 1px solid var(--oma-border);
  border-radius: var(--oma-radius); padding: 0 4px;
}
"##;
