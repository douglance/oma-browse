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

use std::collections::BTreeMap;

use incurs::tool::ToolCatalog;
use serde_json::Value;

use crate::state::AppState;

/// Newtype because Topcoat's app context is keyed by `TypeId`, and registering
/// two values of the same type panics.
pub struct SharedState(pub Arc<AppState>);

/// The command registry, as a second context type. Kept out of [`AppState`] on
/// purpose: every handler in the graph captures an `Arc<AppState>`, so an
/// `AppState` that owned the catalog would own a path back to itself.
pub struct SharedCatalog(pub Registry);

/// The catalog, plus a flattened index built once at startup.
///
/// The index matters: `ToolCatalog::definitions()` clones every definition and
/// its JSON Schema, and the palette re-renders on every keystroke. Paying that
/// once is the difference between a filter that keeps up with typing and one
/// that does not.
pub struct Registry {
    pub catalog: ToolCatalog,
    commands: Vec<Command>,
}

/// One command, pre-chewed into what a row needs.
struct Command {
    /// The catalog key: the command path joined with `_`.
    name: String,
    /// Display title for the group this came from. The catalog flattens groups
    /// away, so this is recovered from the name prefix via `commands::GROUPS`.
    section: String,
    title: String,
    blurb: String,
    /// What the command *is*: group and title, lowercased.
    subject: String,
    /// What it says it does, lowercased.
    blurb_lower: String,
}

impl Registry {
    fn new(catalog: ToolCatalog) -> Self {
        let mut commands: Vec<Command> = catalog
            .definitions()
            .into_iter()
            .map(|def| {
                let (group, title) = crate::dispatch::label(&def.name);
                let section = crate::commands::GROUPS
                    .iter()
                    .find(|(prefix, _)| *prefix == group)
                    .map(|(_, shown)| (*shown).to_string())
                    .unwrap_or_else(|| group.clone());
                Command {
                    subject: format!("{group} {title}").to_lowercase(),
                    blurb_lower: def.description.to_lowercase(),
                    name: def.name.clone(),
                    section,
                    title,
                    blurb: def.description,
                }
            })
            .collect();
        // Declared order, not the catalog's alphabetical one, so the list reads
        // the way the command graph is written.
        commands.sort_by_key(|c| {
            crate::commands::GROUPS
                .iter()
                .position(|(prefix, _)| c.name.starts_with(&format!("{prefix}_")))
                .unwrap_or(usize::MAX)
        });
        Self { catalog, commands }
    }
}

pub fn router(state: Arc<AppState>, catalog: ToolCatalog) -> Router {
    let mut builder = Router::builder()
        .discover()
        .app_context(SharedState(state))
        .app_context(SharedCatalog(Registry::new(catalog)));

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

pub(crate) fn state(cx: &Cx) -> &Arc<AppState> {
    &app_context::<SharedState>(cx).0
}

pub(crate) fn registry(cx: &Cx) -> &Registry {
    &app_context::<SharedCatalog>(cx).0
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Separates a staged tool name from the hint shown beside it. Signals can only
/// hold a `String`, so the two travel as one and are split server-side.
const SEP: char = '\u{1f}';

/// Everything the palette can do, in one procedure.
///
/// `spec` is the DOM id of the row that was clicked, or the row Enter landed on:
/// `do-cmd:page_zoom`, `do-tab:3`, `do-close:3`, `do-go`. Encoding the intent in
/// the id is how the click survives delegation -- Topcoat's expression subset
/// can only hand a handler scalars off the `Event`, and a per-row closure cannot
/// carry a command name out of the shard that rendered it.
///
/// Returns the new value of the page's `prompt` signal: empty for "handled", or
/// a tool name (optionally with a message after a unit separator) to ask the
/// user for an argument. A procedure cannot *write* a signal, but the page can
/// assign what it returns -- which is the whole reason this returns a String.
#[procedure]
async fn act_run(cx: &Cx, spec: String, query: String) -> Result<String> {
    let state = state(cx).clone();
    let reg = registry(cx);
    let Some(rest) = spec.strip_prefix("do-") else { return Ok(String::new()) };

    // The implicit first row: whatever is typed, treated as a URL or a search.
    if rest == "go" {
        if query.trim().is_empty() {
            return Ok(String::new());
        }
        return Ok(finish(&state, &reg.catalog, "nav_go", arg("url", &query)).await);
    }
    if rest == "newtab" {
        return Ok(finish(&state, &reg.catalog, "tab_open", arg("url", &query)).await);
    }

    let Some((verb, payload)) = rest.split_once(':') else { return Ok(String::new()) };
    match verb {
        "tab" => Ok(finish(&state, &reg.catalog, "tab_select", arg("id", payload)).await),
        "close" => Ok(finish(&state, &reg.catalog, "tab_close", arg("id", payload)).await),
        "hist" => Ok(finish(&state, &reg.catalog, "nav_go", arg("url", payload)).await),
        "down" => Ok(finish(&state, &reg.catalog, "download_open", arg("index", payload)).await),
        "cmd" => Ok(run_command(&state, reg, payload, None).await),
        _ => Ok(String::new()),
    }
}

/// Move the highlight, wrapping. The client cannot count rows -- the expression
/// subset has no DOM access -- so the server recomputes the same list the shard
/// rendered and clamps into it.
#[procedure]
async fn act_move(cx: &Cx, query: String, index: f64, delta: f64) -> Result<f64> {
    let rows = rows(state(cx), registry(cx), &query).await;
    if rows.is_empty() {
        return Ok(0.0);
    }
    let len = rows.len() as f64;
    let mut next = index + delta;
    if next < 0.0 {
        next = len - 1.0;
    } else if next >= len {
        next = 0.0;
    }
    Ok(next)
}

/// Enter: run whichever row is highlighted.
#[procedure]
async fn act_pick(cx: &Cx, prompt: String, query: String, index: f64) -> Result<String> {
    let state = state(cx).clone();
    let reg = registry(cx);

    // Prompt mode: the line is an argument for the staged command, not a filter.
    if let Some(tool) = prompt.split(SEP).next().filter(|t| !t.is_empty()) {
        return Ok(run_command(&state, reg, tool, Some(&query)).await);
    }

    let rows = rows(&state, reg, &query).await;
    let Some(row) = rows.get(index as usize).or_else(|| rows.first()) else {
        return Ok(String::new());
    };
    let spec = row.id.clone();
    let Some(rest) = spec.strip_prefix("do-") else { return Ok(String::new()) };
    if rest == "go" {
        if query.trim().is_empty() {
            return Ok(String::new());
        }
        return Ok(finish(&state, &reg.catalog, "nav_go", arg("url", &query)).await);
    }
    let Some((verb, payload)) = rest.split_once(':') else { return Ok(String::new()) };
    match verb {
        "tab" => Ok(finish(&state, &reg.catalog, "tab_select", arg("id", payload)).await),
        "close" => Ok(finish(&state, &reg.catalog, "tab_close", arg("id", payload)).await),
        "hist" => Ok(finish(&state, &reg.catalog, "nav_go", arg("url", payload)).await),
        "down" => Ok(finish(&state, &reg.catalog, "download_open", arg("index", payload)).await),
        "cmd" => Ok(run_command(&state, reg, payload, None).await),
        _ => Ok(String::new()),
    }
}

#[procedure]
async fn act_dismiss(cx: &Cx) -> Result<String> {
    dismiss(state(cx));
    Ok(String::new())
}

fn arg(name: &str, value: &str) -> BTreeMap<String, Value> {
    let mut map = BTreeMap::new();
    if !value.trim().is_empty() {
        map.insert(name.to_string(), Value::String(value.trim().to_string()));
    }
    map
}

/// Run a command, and decide whether a failure means "ask for an argument".
///
/// The palette does not inspect the schema to decide whether to prompt, because
/// the schema cannot answer it: every argument in the graph is `Option<_>`, so
/// nothing is marked required, and the real defaults live in the handlers
/// (`unwrap_or("reset")`). "Has properties" would wrongly demand input for
/// `theme recolor`, whose entire purpose is toggling with no argument.
///
/// So it just runs the thing. If the command comes back with a usage-class
/// complaint, *that* is the signal to prompt -- and the command's own message
/// becomes the hint. The registry stays the only authority on what it needs.
async fn run_command(
    state: &Arc<AppState>,
    reg: &Registry,
    tool: &str,
    line: Option<&str>,
) -> String {
    let Some(def) = reg.catalog.get(tool) else { return String::new() };

    // `None` means the command was picked out of the list, where whatever is in
    // the box was a *filter* -- passing it as an argument turned "type zoom to
    // find Zoom" into `page zoom --direction zoom`. Only prompt mode, which is
    // entered deliberately, treats the line as input.
    let mut args = BTreeMap::new();
    if let Some(line) = line
        && !line.trim().is_empty()
        && let Some((name, ty, _)) = crate::dispatch::primary_arg(def)
    {
        args.insert(name, crate::dispatch::coerce(line, &ty));
    }

    let keep = stays_open(tool, &args);
    match crate::dispatch::run(&reg.catalog, tool, args).await {
        None => {
            if !keep {
                dismiss(state);
            }
            state.notify_tabs();
            String::new()
        }
        Some(message) if wants_argument(&message) => format!("{tool}{SEP}{message}"),
        Some(message) => {
            crate::dispatch::toast(&message);
            String::new()
        }
    }
}

/// Run something that takes no deciding, and dismiss unless it changed the list.
async fn finish(
    state: &Arc<AppState>,
    catalog: &ToolCatalog,
    tool: &str,
    args: BTreeMap<String, Value>,
) -> String {
    let keep = stays_open(tool, &args);
    match crate::dispatch::run(catalog, tool, args).await {
        None => {
            if !keep {
                dismiss(state);
            }
            state.notify_tabs();
            String::new()
        }
        Some(message) => {
            crate::dispatch::toast(&message);
            String::new()
        }
    }
}

/// A failure that means "you did not give me enough", rather than "that broke".
///
/// These are the codes the graph itself returns for missing or unparseable
/// input; anything else is a real error and belongs in a toast.
fn wants_argument(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("is required") || m.contains("unknown ") || m.contains("expected")
}

/// The few commands that change what the palette is showing, so closing it would
/// hide the result. The single place UI policy diverges from the registry.
///
/// `tab_open` is the one row whose answer depends on its arguments: opening a
/// URL is done with the palette, while opening a blank tab re-summons it (see
/// `commands.rs`), and dismissing here would close what the command just opened.
fn stays_open(tool: &str, args: &BTreeMap<String, Value>) -> bool {
    match tool {
        "tab_close" | "tab_list" | "theme_recolor" | "theme_show" | "theme_css" => true,
        "tab_open" => !args.contains_key("url"),
        _ => false,
    }
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
    // Ctrl-F and friends ask for a command to be pre-selected, so the chord
    // lands straight in "type your search" rather than in a list.
    let staged = state(cx).take_stage().unwrap_or_default();
    let theme = state(cx).theme.read().await;
    let vars = Unescaped::new_unchecked(theme.css.chrome.clone());
    let mine =
        Unescaped::new_unchecked(crate::strip::chrome_vars(&state(cx).config, theme.css.opacity));
    let sheet = Unescaped::new_unchecked(PALETTE_CSS);

    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8">
                <title>"Palette"</title>
                <style>(vars)</style>
                <style>(mine)</style>
                <style>(sheet)</style>
            </head>
            <body>
                // The query has to be a signal now: filtering happens
                // server-side against the tab list and the command registry,
                // so every keystroke needs to reach a shard. It was previously
                // kept out of the signal graph, which is why typing filtered
                // nothing despite the comments promising it did.
                signal query = String::new();
                // Highlighted row. A shard argument rather than a page-only
                // signal because a shard cannot see page scope -- so moving the
                // highlight costs a re-render, which for ~25 rows is fine.
                signal sel = 0.0;
                // "" while browsing; otherwise a staged tool, optionally with a
                // hint after a unit separator.
                signal prompt = staged;

                // Delegated from the *card*, not from the shard's list. Clicks
                // bubble out of the shard into page scope, which is the only
                // scope that can read `query` and write these signals.
                <div class="card" @click=$(async |e: Event| {
                    if e.target.id.starts_with("do-") {
                        prompt.set(act_run(e.target.id, query.get()).await);
                        query.set("".to_owned());
                        sel.set(0.0);
                    }
                })>
                    <div class="field">
                        <input
                            id="q"
                            class="input"
                            type="text"
                            autofocus="autofocus"
                            autocomplete="off"
                            spellcheck="false"
                            placeholder="Search tabs and commands, or enter a URL"
                            :value=$(query.get())
                            // `input`, not `keydown`: keydown fires before the
                            // value updates, so filtering on it is always one
                            // character behind.
                            @input=$(|e: Event| { query.set(e.target.value); sel.set(0.0); })
                            @keydown=$(async |e: Event| {
                                if e.key == "Enter" {
                                    prompt.set(act_pick(prompt.get(), e.target.value, sel.get()).await);
                                    query.set("".to_owned());
                                    sel.set(0.0);
                                } else if e.key == "ArrowDown" {
                                    sel.set(act_move(e.target.value, sel.get(), 1.0).await);
                                } else if e.key == "ArrowUp" {
                                    sel.set(act_move(e.target.value, sel.get(), -1.0).await);
                                } else if e.key == "Escape" {
                                    act_dismiss().await;
                                }
                            })
                        >
                    </div>

                    results(query: $(query.get()), sel: $(sel.get()), prompt: $(prompt.get()))

                    <div class="foot">
                        <span class="keys">
                            <kbd>"enter"</kbd>" run  "
                            <kbd>"up/down"</kbd>" move  "
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
    let mine =
        Unescaped::new_unchecked(crate::strip::chrome_vars(&state(cx).config, theme.css.opacity));
    let sheet = Unescaped::new_unchecked(START_CSS);
    let theme_name = theme.name.clone();

    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8">
                <title>"New tab"</title>
                <style>(vars)</style>
                <style>(mine)</style>
                <style>(sheet)</style>
            </head>
            <body>
                <main>
                    <div class="mark">(MARK)</div>
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

/// The browser's mark: `nf-md-web_box`.
///
/// A Nerd Font glyph rather than an SVG or a bundled image, for the same reason
/// the tab strip uses one -- it inherits the theme's accent colour by being
/// text, so it re-dresses with everything else and costs no asset.
const MARK: &str = "\u{f0f94}";

const START_CSS: &str = r#"
* { box-sizing: border-box; }
html, body { margin: 0; height: 100%; background: var(--oma-veil); color: var(--oma-fg);
  font-family: system-ui, sans-serif; }
main { height: 100%; display: flex; flex-direction: column; align-items: center;
  justify-content: center; gap: var(--oma-space-2); }
.mark {
  /* Nerd Font by name, not through `--oma-font-mono`: that token is whatever
     fontconfig calls `monospace`, which is not guaranteed to be patched -- and
     an unpatched font renders the mark as tofu. Same chain as the tab strip. */
  font-family: "JetBrainsMono Nerd Font", "Symbols Nerd Font", var(--oma-font-mono), monospace;
  font-size: 4.5rem; line-height: 1; color: var(--oma-accent);
}
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
/// One line in the list. `id` carries the whole intent, because a delegated
/// click can only recover a string off the event target.
#[derive(Clone)]
struct Row {
    index: f64,
    id: String,
    close_id: String,
    icon: String,
    title: String,
    sub: String,
    /// Section heading to print above this row, or empty to continue.
    section: String,
}

/// How well a command matches, weighing what it *is* over what it says.
///
/// Both halves are searchable, but not equally: `nav back`'s description opens
/// with "Go back in history", which on a single combined haystack ties exactly
/// with `nav go`'s own name for the query "go" -- and the tie broke the wrong
/// way, so Enter ran the wrong command. Doubling the name settles it.
fn score_command(c: &Command, needle: &str) -> Option<i32> {
    let subject = crate::fuzzy::score(&c.subject, needle);
    let blurb = crate::fuzzy::score(&c.blurb_lower, needle);
    match (subject, blurb) {
        (None, None) => None,
        (s, b) => Some(s.unwrap_or(0) * 2 + b.unwrap_or(0)),
    }
}

/// The list, built in exactly one place.
///
/// This function is called from three: the shard that renders it, the arrow-key
/// procedure that clamps into it, and the Enter procedure that runs row N. If
/// they ever disagree about ordering, Enter runs the wrong command -- so they
/// share this rather than each filtering for themselves.
///
/// Three corpora are ranked against each other -- open tabs, every command, and
/// browsing history -- which is why they all go through the same matcher.
async fn rows(state: &Arc<AppState>, reg: &Registry, query: &str) -> Vec<Row> {
    let raw = query.trim();
    // A leading `>` narrows to commands, for when a query matches a page title
    // and a command equally well.
    let (needle, commands_only) = match raw.strip_prefix('>') {
        Some(rest) => (rest.trim().to_lowercase(), true),
        None => (raw.to_lowercase(), false),
    };
    let searching = !needle.is_empty();

    let mut out: Vec<Row> = Vec::new();

    // Whatever is typed, as a destination. First, so Enter keeps meaning what
    // it always meant in this box.
    if !raw.is_empty() && !commands_only {
        out.push(Row {
            index: 0.0,
            id: "do-go".to_string(),
            close_id: String::new(),
            icon: "→".to_string(),
            title: format!("Go to {raw}"),
            sub: "Open in the current tab".to_string(),
            section: String::new(),
        });
        // The old Ctrl-Enter, as a row you can see. A hidden chord for the one
        // action the box could take was never discoverable.
        out.push(Row {
            index: 0.0,
            id: "do-newtab".to_string(),
            close_id: String::new(),
            icon: "+".to_string(),
            title: format!("Open {raw} in a new tab"),
            sub: String::new(),
            section: String::new(),
        });
    }

    // Scored candidates from every corpus, ranked together. Tabs and commands
    // carry a small edge over history at equal match quality: there are a
    // handful of each and thousands of pages, and picking the tab you already
    // have open is nearly always what you meant.
    let mut scored: Vec<(i32, Row)> = Vec::new();

    if !commands_only {
        for tab in state.tabs.read().await.list() {
            let hay = format!("{} {}", tab.title, tab.url).to_lowercase();
            let Some(points) = crate::fuzzy::score(&hay, &needle) else { continue };
            scored.push((
                points + 30,
                Row {
                    index: 0.0,
                    id: format!("do-tab:{}", tab.id),
                    close_id: format!("do-close:{}", tab.id),
                    icon: if tab.active { "●" } else { "○" }.to_string(),
                    title: tab.title.clone(),
                    sub: tab.url.clone(),
                    section: "Open tabs".to_string(),
                },
            ));
        }
    }

    for c in &reg.commands {
        let Some(points) = score_command(c, &needle) else { continue };
        scored.push((
            points + 20,
            Row {
                index: 0.0,
                id: format!("do-cmd:{}", c.name),
                close_id: String::new(),
                icon: "›".to_string(),
                title: c.title.clone(),
                sub: c.blurb.clone(),
                section: c.section.clone(),
            },
        ));
    }

    if !commands_only {
        let marks = state.bookmarks.read().await;
        for mark in marks.entries() {
            let hay = format!("{} {}", mark.title, mark.url).to_lowercase();
            let Some(points) = crate::fuzzy::score(&hay, &needle) else { continue };
            // Above history at equal match quality: you kept this one on
            // purpose, and a page you chose beats one you merely passed through.
            scored.push((
                points + 25,
                Row {
                    index: 0.0,
                    id: format!("do-hist:{}", mark.url),
                    close_id: String::new(),
                    icon: "★".to_string(),
                    title: if mark.title.is_empty() { mark.url.clone() } else { mark.title.clone() },
                    sub: mark.url.clone(),
                    section: "Bookmarks".to_string(),
                },
            ));
        }
        drop(marks);

        // Saved files, ranked like anything else you might be looking for.
        // Capped low: this is "open the thing I just downloaded", and a
        // downloads *manager* is `download list`.
        if let Ok(downloads) = state.downloads.lock() {
            for (at, file) in downloads.entries().iter().enumerate().take(50) {
                let hay = format!("{} {}", file.name(), file.url).to_lowercase();
                let Some(points) = crate::fuzzy::score(&hay, &needle) else { continue };
                scored.push((
                    points + 20,
                    Row {
                        index: 0.0,
                        // 1-based, because `download open` counts from the
                        // newest as 1 and the two must agree.
                        id: format!("do-down:{}", at + 1),
                        close_id: String::new(),
                        icon: "⤓".to_string(),
                        title: file.name(),
                        sub: file.path.display().to_string(),
                        section: "Downloads".to_string(),
                    },
                ));
            }
        }

        let history = state.history.read().await;
        for visit in history.entries() {
            let hay = format!("{} {}", visit.title, visit.url).to_lowercase();
            let Some(points) = crate::fuzzy::score(&hay, &needle) else { continue };
            // A page seen often, or seen lately, edges out one seen once long
            // ago -- but only enough to break near-ties, never enough to beat a
            // better textual match.
            let familiarity = (visit.visits.min(20) as i32) / 2;
            scored.push((
                points + familiarity,
                Row {
                    index: 0.0,
                    id: format!("do-hist:{}", visit.url),
                    close_id: String::new(),
                    icon: "◷".to_string(),
                    title: if visit.title.is_empty() {
                        visit.url.clone()
                    } else {
                        visit.title.clone()
                    },
                    sub: visit.url.clone(),
                    section: "Recently visited".to_string(),
                },
            ));
        }
    }

    if searching {
        // Best first, and no section headings: they are noise once the list is
        // three rows deep, and the rows no longer come in section order anyway.
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        // A bookmarked page is usually also in history. Keep the better-scoring
        // row -- which the bookmark bonus makes the bookmark -- and drop the
        // duplicate rather than showing the same URL twice.
        let mut seen = std::collections::HashSet::new();
        scored.retain(|(_, row)| seen.insert(row.id.clone()));
        out.extend(scored.into_iter().take(state.config.chrome.palette.rows).map(|(_, mut row)| {
            row.section = String::new();
            row
        }));
    } else {
        // Idle: a menu, in declared order, with history as a short recent list
        // rather than thousands of rows.
        let mut menu: Vec<Row> = Vec::new();
        menu.extend(scored.iter().filter(|(_, r)| r.section == "Open tabs").map(|(_, r)| r.clone()));
        for (_, title) in crate::commands::GROUPS {
            menu.extend(
                scored
                    .iter()
                    .filter(|(_, r)| r.section == *title && r.id.starts_with("do-cmd:"))
                    .map(|(_, r)| r.clone()),
            );
        }
        menu.extend(
            scored
                .iter()
                .filter(|(_, r)| r.section == "Bookmarks")
                .map(|(_, r)| r.clone()),
        );
        menu.extend(
            scored
                .iter()
                .filter(|(_, r)| r.section == "Downloads")
                .take(state.config.chrome.palette.download_rows)
                .map(|(_, r)| r.clone()),
        );
        menu.extend(
            scored
                .iter()
                .filter(|(_, r)| r.section == "Recently visited")
                .take(state.config.chrome.palette.history_rows)
                .map(|(_, r)| r.clone()),
        );
        // Print a heading only where the section actually changes.
        let mut last = String::new();
        for row in &mut menu {
            if row.section == last {
                row.section = String::new();
            } else {
                last = row.section.clone();
            }
        }
        out.extend(menu);
    }

    for (i, row) in out.iter_mut().enumerate() {
        row.index = i as f64;
    }
    out
}

/// The result list: open tabs and every command, filtered together.
#[shard]
async fn results(cx: &Cx, query: String, sel: f64, prompt: String) -> Result {
    let reg = registry(cx);

    // Prompt mode: the line is an argument, so show what it is for instead of
    // a list to pick from.
    if let Some((tool, hint)) = staged(&prompt) {
        let label = reg.catalog.get(&tool).and_then(crate::dispatch::primary_arg);
        let (field, kind, describe) = match label {
            Some((name, ty, desc)) => (name, ty, desc.unwrap_or_default()),
            None => (String::new(), String::new(), String::new()),
        };
        let (_, title) = crate::dispatch::label(&tool);
        return view! {
            <ul class="list">
                <li class="section">(title)</li>
                if !hint.is_empty() {
                    <li class="empty">(hint)</li>
                }
                if !field.is_empty() {
                    <li class="row">
                        <span class="row-icon">"…"</span>
                        <span class="row-main">
                            <span class="row-title">(field)" ("(kind)")"</span>
                            <span class="row-sub">(describe)</span>
                        </span>
                    </li>
                }
            </ul>
        };
    }

    let rows = rows(state(cx), reg, &query).await;

    view! {
        <ul class="list">
            if rows.is_empty() {
                <li class="empty">"Nothing matches."</li>
            } else {
                for row in rows {
                    let idx = row.index;
                    if !row.section.is_empty() {
                        <li class="section">(row.section)</li>
                    }
                    <li class="row" :data-active=$(sel == idx)>
                        <span class="row-icon">(row.icon)</span>
                        <span class="row-main" id=(row.id)>
                            <span class="row-title">(row.title)</span>
                            <span class="row-sub">(row.sub)</span>
                        </span>
                        if !row.close_id.is_empty() {
                            <button class="row-close" id=(row.close_id) title="Close tab">"×"</button>
                        }
                    </li>
                }
            }
        </ul>
    }
}

/// Split the `prompt` signal back into the tool and its hint.
fn staged(prompt: &str) -> Option<(String, String)> {
    if prompt.is_empty() {
        return None;
    }
    match prompt.split_once(SEP) {
        Some((tool, hint)) => Some((tool.to_string(), hint.to_string())),
        None => Some((prompt.to_string(), String::new())),
    }
}

const PALETTE_CSS: &str = r##"
* { box-sizing: border-box; }
html, body {
  margin: 0; height: 100%; background: transparent;
  /* The aesthetic contract is mono-first: dense, precise, terminal-grade.
     `chrome.font` in the config file is what this resolves to. */
  font-family: var(--oma-chrome-font); font-size: var(--oma-font-body);
  color: var(--oma-fg);
}
.card {
  height: 100%; display: flex; flex-direction: column;
  /* Omarchy's own launcher surface, not the page veil. The palette is a card
     floating over content, and at the page's translucency a full list of
     commands is unreadable against whatever happens to be behind it. This is
     the same token the Omarchy launcher uses, so it matches by construction. */
  /* `chrome.veil`: the card is opaque by default, because it is a card you
     read dense text off. color-mix rather than an alpha baked into the token,
     since the token may already carry one of its own. */
  background: color-mix(in srgb, var(--oma-omnibar-bg) calc(var(--oma-chrome-alpha) * 100%), transparent);
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
/* Keyboard highlight. A data attribute rather than a class, because a `:class`
   bind replaces the whole list and would take `row` with it. */
.row[data-active] { background: var(--oma-omnibar-selected-bg); }
.row[data-active] .row-title { color: var(--oma-accent); }
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
kbd {
  font-family: var(--oma-font-mono); color: var(--oma-fg);
  border: 1px solid var(--oma-border);
  border-radius: var(--oma-radius); padding: 0 4px;
}
"##;

#[cfg(test)]
mod tests {
    /// The mark has to be a glyph a patched font actually carries, or the start
    /// page opens on a tofu box. `nf-md-web_box` lives in plane 15, where Nerd
    /// Fonts put the Material Design set -- not the BMP private use area the
    /// strip's older glyphs come from.
    #[test]
    fn the_mark_is_a_private_use_glyph() {
        let c = super::MARK.chars().next().expect("a glyph");
        assert_eq!(super::MARK.chars().count(), 1, "one codepoint, not a sequence");
        assert!(
            ('\u{e000}'..='\u{f8ff}').contains(&c) || ('\u{f0000}'..='\u{ffffd}').contains(&c),
            "{:?} is outside the private use areas",
            super::MARK
        );
    }

    /// The palette, the CLI and MCP must all see the same commands.
    ///
    /// This is the test that makes the registry worth having: add a command to
    /// the graph and it appears in every surface, or this fails. Before, a new
    /// command reached the CLI and MCP automatically and the palette not at all.
    #[tokio::test]
    async fn the_palette_sees_every_command() {
        let state = std::sync::Arc::new(crate::state::AppState::detached());
        let cli = crate::commands::command_graph(state);
        let catalog = cli.try_tool_catalog().expect("unique tool names");
        let registry = super::Registry::new(catalog.clone());

        let mut from_catalog: Vec<String> =
            catalog.definitions().into_iter().map(|d| d.name).collect();
        let mut from_palette: Vec<String> =
            registry.commands.iter().map(|c| c.name.clone()).collect();
        from_catalog.sort();
        from_palette.sort();
        assert_eq!(from_catalog, from_palette);
        assert!(!from_palette.is_empty(), "the graph produced no commands at all");
    }

    /// Every command must land in a titled section, or it renders under a raw
    /// `_`-prefix in the list.
    #[tokio::test]
    async fn every_command_has_a_group_title() {
        let state = std::sync::Arc::new(crate::state::AppState::detached());
        let cli = crate::commands::command_graph(state);
        let registry = super::Registry::new(cli.try_tool_catalog().unwrap());
        let titles: Vec<&str> = crate::commands::GROUPS.iter().map(|(_, t)| *t).collect();
        for c in &registry.commands {
            assert!(
                titles.contains(&c.section.as_str()),
                "{} has no entry in commands::GROUPS (section {:?})",
                c.name,
                c.section
            );
        }
    }

    /// The regression that started this: `nav back`'s description opens with
    /// "Go back in history", so searching "go" ranked it above `nav go` itself
    /// and Enter ran the wrong command. Ranking now runs through the shared
    /// fzf-style matcher, and these pin the cases that matter.
    #[tokio::test]
    async fn a_name_match_outranks_a_description_match() {
        let state = std::sync::Arc::new(crate::state::AppState::detached());
        let cli = crate::commands::command_graph(state);
        let reg = super::Registry::new(cli.try_tool_catalog().unwrap());

        let best = |q: &str| -> String {
            let mut hits: Vec<_> = reg
                .commands
                .iter()
                .filter_map(|c| super::score_command(c, q).map(|s| (s, c)))
                .collect();
            hits.sort_by(|a, b| b.0.cmp(&a.0));
            hits.first().map(|(_, c)| c.name.clone()).unwrap_or_default()
        };

        assert_eq!(best("go"), "nav_go");
        assert_eq!(best("zoom"), "page_zoom");
        assert_eq!(best("reload"), "nav_reload");
        assert_eq!(best("recolor"), "theme_recolor");
        assert_eq!(best("screenshot"), "page_screenshot");
    }

    /// An open tab should win over a history entry for the same page: you
    /// already have it, and switching beats reloading.
    #[tokio::test]
    async fn an_open_tab_outranks_the_same_page_in_history() {
        let state = std::sync::Arc::new(crate::state::AppState::detached());
        {
            let mut tabs = state.tabs.write().await;
            let (id, label) = tabs.allocate("https://en.wikipedia.org/wiki/Rust".to_string());
            tabs.set_active(id);
            tabs.update_title(&label, "Rust - Wikipedia".to_string());
        }
        state.history.write().await.record("https://en.wikipedia.org/wiki/Rust", 1);

        let cli = crate::commands::command_graph(state.clone());
        let reg = super::Registry::new(cli.try_tool_catalog().unwrap());
        let rows = super::rows(&state, &reg, "wikipedia").await;

        let tab_at = rows.iter().position(|r| r.id.starts_with("do-tab:"));
        let hist_at = rows.iter().position(|r| r.id.starts_with("do-hist:"));
        assert!(tab_at.is_some(), "the open tab should be offered");
        assert!(hist_at.is_some(), "history should be offered");
        assert!(tab_at < hist_at, "the open tab should rank above history");
    }

    /// A saved file is findable by name, and Enter on it opens the file rather
    /// than navigating to where it came from.
    #[tokio::test]
    async fn a_download_is_findable_by_its_name() {
        let state = std::sync::Arc::new(crate::state::AppState::detached());
        state.downloads.lock().unwrap().start(
            "https://a.example/d?id=7",
            std::path::Path::new("/tmp/quarterly-report.pdf"),
            1,
        );
        let cli = crate::commands::command_graph(state.clone());
        let reg = super::Registry::new(cli.try_tool_catalog().unwrap());

        let rows = super::rows(&state, &reg, "quarterly").await;
        let row = rows
            .iter()
            .find(|r| r.id.starts_with("do-down:"))
            .expect("the download should be offered");
        assert_eq!(row.title, "quarterly-report.pdf");
        // 1-based, because `download open` counts from the newest as 1.
        assert_eq!(row.id, "do-down:1");
    }

    /// History is searchable but must not flood the idle menu.
    #[tokio::test]
    async fn history_is_capped_when_nothing_is_typed() {
        let state = std::sync::Arc::new(crate::state::AppState::detached());
        for i in 0..40 {
            state.history.write().await.record(&format!("https://{i}.example/page"), i as u64);
        }
        let cli = crate::commands::command_graph(state.clone());
        let reg = super::Registry::new(cli.try_tool_catalog().unwrap());

        let idle = super::rows(&state, &reg, "").await;
        let shown = idle.iter().filter(|r| r.id.starts_with("do-hist:")).count();
        assert_eq!(shown, 8, "the idle menu shows a short recent list");

        let searched = super::rows(&state, &reg, "example").await;
        let found = searched.iter().filter(|r| r.id.starts_with("do-hist:")).count();
        assert!(found > 8, "searching reaches the whole history, got {found}");
    }
}
