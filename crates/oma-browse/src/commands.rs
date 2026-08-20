//! The command graph — the core of the application.
//!
//! Every browser action is defined here exactly once. incurs then projects each
//! one through the CLI, HTTP, MCP, OpenAPI, skills and shell completions, so the
//! GUI, a shell user, and an agent all reach identical, identically-validated
//! behaviour. Handlers reach shared state by closure capture: `TypedContext`
//! carries no typed user-state slot, and its `vars` is JSON, so it cannot hold an
//! `Arc`.

use std::sync::Arc;

use incurs::cli::Cli;
use incurs::command::{CommandDef, TypedContext, TypedResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Deserialize, incurs::Args)]
struct NoArgs {}

#[derive(Deserialize, incurs::Options)]
struct NoOptions {}

#[derive(JsonSchema, Serialize)]
struct ThemeInfo {
    /// The slug Omarchy knows this theme by.
    name: String,
    /// `dark` or `light`, as Omarchy resolves it.
    mode: String,
    /// How many colour keys the resolver produced.
    colors: usize,
}

#[derive(JsonSchema, Serialize)]
struct ThemeReloaded {
    name: String,
    mode: String,
    /// False when the newly-resolved theme was byte-identical to the old one, so
    /// callers can tell a real change from a redundant hook firing.
    changed: bool,
}

#[derive(Deserialize, incurs::Args)]
struct ThemeReloadArgs {
    /// Theme slug, as passed by Omarchy's `theme-set` hook. Advisory only: the
    /// live theme is always re-read from disk.
    name: Option<String>,
}

/// Display titles for the command groups.
///
/// The tool catalog every other surface reads flattens groups away and keeps
/// only the `_`-joined name, so the description passed to `Cli::create` inside
/// each group function never reaches it. This lives beside `command_graph` so
/// that adding a group and forgetting its title is a one-file mistake.
pub const GROUPS: &[(&str, &str)] = &[
    ("nav", "Navigation"),
    ("tab", "Tabs"),
    ("page", "Page"),
    ("theme", "Theme"),
    ("ui", "Interface"),
    ("history", "History"),
    ("bookmark", "Bookmarks"),
    ("find", "Find"),
    ("share", "Share"),
    ("window", "Window"),
];

/// Build the graph, capturing shared state in each handler.
pub fn command_graph(state: Arc<AppState>) -> Cli {
    Cli::create("oma-browse")
        .version(env!("CARGO_PKG_VERSION"))
        .description("An Omarchy-themed, agent-drivable browser")
        .group(tab_group(state.clone()))
        .group(nav_group(state.clone()))
        .group(page_group(state.clone()))
        .group(ui_group(state.clone()))
        .group(theme_group(state.clone()))
        .group(history_group(state.clone()))
        .group(bookmark_group(state.clone()))
        .group(find_group(state.clone()))
        .group(share_group(state.clone()))
        .group(window_group(state))
}

// ---------------------------------------------------------------------------
// share -- the things a browser on *this* desktop can do that others cannot
// ---------------------------------------------------------------------------

#[derive(JsonSchema, Serialize)]
struct Shared {
    url: String,
    /// What the page was handed to, for the record.
    via: String,
}

/// Run a desktop helper without blocking the browser on it.
///
/// Every one of these shells out to something Omarchy owns, so failures are
/// reported rather than swallowed -- but the caller is a keystroke, and a slow
/// helper must not wedge the UI.
async fn handoff(program: &str, args: &[&str]) -> std::result::Result<(), String> {
    match tokio::process::Command::new(program).args(args).status().await {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("{program} exited with {status}")),
        Err(e) => Err(format!("could not run {program}: {e}")),
    }
}

fn share_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let copy = CommandDef::typed::<NoArgs, NoOptions, (), Shared, _, _>(
        "copy",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let Some((url, _)) = here(&state).await else {
                    return TypedResult::error("usage", "there is no page".to_string());
                };
                match handoff("wl-copy", &[url.as_str()]).await {
                    Ok(()) => TypedResult::ok(Shared { url, via: "wl-copy".into() }),
                    Err(e) => TypedResult::error("clipboard", e),
                }
            }
        },
    )
    .description("Copy the current page's URL to the Wayland clipboard")
    .done();

    let s = state.clone();
    let webapp = CommandDef::typed::<NoArgs, BookmarkOptions, (), Shared, _, _>(
        "webapp",
        move |ctx: TypedContext<NoArgs, BookmarkOptions, ()>| {
            let state = s.clone();
            async move {
                let current = here(&state).await;
                let url = ctx.options.url.or_else(|| current.as_ref().map(|(u, _)| u.clone()));
                let Some(url) = url.filter(|u| !u.is_empty()) else {
                    return TypedResult::error("usage", "there is no page".to_string());
                };
                // The launcher wants a display name and an icon; the page's own
                // title and favicon host are the only sensible defaults, and
                // Omarchy resolves an icon name it does not recognise itself.
                let title = ctx
                    .options
                    .title
                    .or_else(|| current.map(|(_, t)| t))
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| url.clone());
                let icon = url
                    .split('/')
                    .nth(2)
                    .and_then(|host| host.split('.').rev().nth(1))
                    .unwrap_or("web")
                    .to_string();
                match handoff(
                    "omarchy",
                    &["webapp", "install", title.as_str(), url.as_str(), icon.as_str()],
                )
                .await
                {
                    Ok(()) => TypedResult::ok(Shared { url, via: "omarchy webapp".into() }),
                    Err(e) => TypedResult::error("omarchy", e),
                }
            }
        },
    )
    .description("Install the current page as an Omarchy web app with its own launcher")
    .done();

    let s = state;
    let terminal = CommandDef::typed::<NoArgs, NoOptions, (), Shared, _, _>(
        "terminal",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let Some((url, _)) = here(&state).await else {
                    return TypedResult::error("usage", "there is no page".to_string());
                };
                // Ghostty holding the shell open is the point: this is for
                // piping a URL into curl or yt-dlp, not for viewing it.
                let command = format!("printf %s {url:?} | wl-copy; echo {url:?}; exec $SHELL");
                match handoff("ghostty", &["-e", "bash", "-lc", command.as_str()]).await {
                    Ok(()) => TypedResult::ok(Shared { url, via: "ghostty".into() }),
                    Err(e) => TypedResult::error("terminal", e),
                }
            }
        },
    )
    .description("Open a Ghostty terminal with this page's URL on the clipboard")
    .done();

    Cli::create("share")
        .description("Hand the current page to the rest of the desktop")
        .command("copy", copy)
        .command("webapp", webapp)
        .command("terminal", terminal)
}

// ---------------------------------------------------------------------------
// find
// ---------------------------------------------------------------------------

#[derive(Deserialize, incurs::Args)]
struct FindArgs {
    // Same reason as `nav go`: over HTTP a positional is a path segment.
    /// Text to search for on the page.
    text: Option<String>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct FindOptionsArg {
    /// Text to search for on the page.
    text: Option<String>,
}

fn find_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let search = CommandDef::typed::<FindArgs, FindOptionsArg, (), Acted, _, _>(
        "text",
        move |ctx: TypedContext<FindArgs, FindOptionsArg, ()>| {
            let state = s.clone();
            async move {
                let Some(text) = ctx.options.text.or(ctx.args.text) else {
                    return TypedResult::error("usage", "text to find is required".to_string());
                };
                match crate::tabs::find(&state, crate::tabs::FindAction::Search(text)).await {
                    Ok(()) => TypedResult::ok(Acted { ok: true }),
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description("Find text on the page, highlighting every match")
    .done();

    let mut group =
        Cli::create("find").description("Search within the current page").command("text", search);

    for (name, action, blurb) in [
        ("next", crate::tabs::FindAction::Next, "Jump to the next match"),
        ("previous", crate::tabs::FindAction::Previous, "Jump to the previous match"),
        ("clear", crate::tabs::FindAction::Stop, "Clear the search highlight"),
    ] {
        let s = state.clone();
        let action = action.clone();
        let cmd = CommandDef::typed::<NoArgs, NoOptions, (), Acted, _, _>(
            name,
            move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
                let (state, action) = (s.clone(), action.clone());
                async move {
                    match crate::tabs::find(&state, action).await {
                        Ok(()) => TypedResult::ok(Acted { ok: true }),
                        Err(e) => TypedResult::error("webview", format!("{e:#}")),
                    }
                }
            },
        )
        .description(blurb)
        .done();
        group = group.command(name, cmd);
    }
    group
}

// ---------------------------------------------------------------------------
// bookmark
// ---------------------------------------------------------------------------

#[derive(JsonSchema, Serialize)]
struct Bookmarked {
    url: String,
    title: String,
    /// False when the page was already kept and only its title was refreshed.
    added: bool,
}

#[derive(JsonSchema, Serialize)]
struct BookmarkList {
    entries: Vec<Visited>,
}

#[derive(Deserialize, incurs::Args)]
struct BookmarkArgs {
    // A URL is full of slashes, so the option below is the usable route.
    /// The page to keep. Defaults to the active tab.
    url: Option<String>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct BookmarkOptions {
    /// The page to keep. Defaults to the active tab.
    url: Option<String>,
    /// Title to file it under. Defaults to the page's own.
    title: Option<String>,
}

/// The active tab's URL and title, for commands that default to "here".
async fn here(state: &Arc<AppState>) -> Option<(String, String)> {
    let tabs = state.tabs.read().await;
    tabs.list().into_iter().find(|t| t.active).map(|t| (t.url, t.title))
}

fn bookmark_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let add = CommandDef::typed::<BookmarkArgs, BookmarkOptions, (), Bookmarked, _, _>(
        "add",
        move |ctx: TypedContext<BookmarkArgs, BookmarkOptions, ()>| {
            let state = s.clone();
            async move {
                let current = here(&state).await;
                let url = ctx
                    .options
                    .url
                    .or(ctx.args.url)
                    .or_else(|| current.as_ref().map(|(u, _)| u.clone()));
                let Some(url) = url.filter(|u| !u.is_empty()) else {
                    return TypedResult::error("usage", "there is no page to bookmark".to_string());
                };
                let title = ctx
                    .options
                    .title
                    .or_else(|| current.map(|(_, t)| t))
                    .unwrap_or_default();
                let added =
                    state.bookmarks.write().await.add(&url, &title, crate::history::now());
                TypedResult::ok(Bookmarked { url, title, added })
            }
        },
    )
    .description("Keep the current page, or a given URL")
    .done();

    let s = state.clone();
    let remove = CommandDef::typed::<BookmarkArgs, BookmarkOptions, (), Bookmarked, _, _>(
        "remove",
        move |ctx: TypedContext<BookmarkArgs, BookmarkOptions, ()>| {
            let state = s.clone();
            async move {
                let url = match ctx.options.url.or(ctx.args.url) {
                    Some(url) => url,
                    None => match here(&state).await {
                        Some((url, _)) => url,
                        None => {
                            return TypedResult::error("usage", "no page to forget".to_string());
                        }
                    },
                };
                let gone = state.bookmarks.write().await.remove(&url);
                if gone {
                    TypedResult::ok(Bookmarked { url, title: String::new(), added: false })
                } else {
                    TypedResult::error("not_found", format!("{url} was not bookmarked"))
                }
            }
        },
    )
    .description("Forget a bookmarked page, defaulting to the current one")
    .destructive(true)
    .done();

    let s = state;
    let list = CommandDef::typed::<NoArgs, NoOptions, (), BookmarkList, _, _>(
        "list",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let marks = state.bookmarks.read().await;
                TypedResult::ok(BookmarkList {
                    entries: marks
                        .entries()
                        .iter()
                        .map(|b| Visited {
                            url: b.url.clone(),
                            title: b.title.clone(),
                            visits: 0,
                        })
                        .collect(),
                })
            }
        },
    )
    .description("List kept pages, newest first")
    .done();

    Cli::create("bookmark")
        .description("Pages worth keeping")
        .command("add", add)
        .command("remove", remove)
        .command("list", list)
}

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

#[derive(JsonSchema, Serialize)]
struct Visited {
    url: String,
    title: String,
    visits: u32,
}

#[derive(JsonSchema, Serialize)]
struct HistoryList {
    entries: Vec<Visited>,
}

#[derive(JsonSchema, Serialize)]
struct Forgotten {
    cleared: usize,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct HistoryListOptions {
    /// How many entries to return, newest first. Defaults to 50.
    limit: Option<u32>,
}

fn history_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let list = CommandDef::typed::<NoArgs, HistoryListOptions, (), HistoryList, _, _>(
        "list",
        move |ctx: TypedContext<NoArgs, HistoryListOptions, ()>| {
            let state = s.clone();
            async move {
                let limit = ctx.options.limit.unwrap_or(50) as usize;
                let history = state.history.read().await;
                TypedResult::ok(HistoryList {
                    entries: history
                        .entries()
                        .iter()
                        .take(limit)
                        .map(|v| Visited {
                            url: v.url.clone(),
                            title: v.title.clone(),
                            visits: v.visits,
                        })
                        .collect(),
                })
            }
        },
    )
    .description("List recently visited pages, newest first")
    .done();

    let s = state;
    let clear = CommandDef::typed::<NoArgs, NoOptions, (), Forgotten, _, _>(
        "clear",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let mut history = state.history.write().await;
                let cleared = history.entries().len();
                history.clear();
                TypedResult::ok(Forgotten { cleared })
            }
        },
    )
    .description("Forget every page this browser has visited")
    .destructive(true)
    .done();

    Cli::create("history")
        .description("Where the browser has been")
        .command("list", list)
        .command("clear", clear)
}

// ---------------------------------------------------------------------------
// tab
// ---------------------------------------------------------------------------

#[derive(Deserialize, incurs::Args)]
struct TabOpenArgs {
    /// A URL, a bare host, or search terms.
    url: Option<String>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct TabOpenOptions {
    // Same as the positional, for callers whose transport cannot carry slashes.
    // Not a doc comment: incurs merges args and options into one schema with the
    // options last, so this text would be what the palette shows for the field.
    /// A URL, a bare host, or search terms.
    url: Option<String>,
    /// Open without switching to it.
    background: bool,
}

#[derive(Deserialize, incurs::Args)]
struct TabIdArgs {
    /// Tab id, as reported by `tab list`. Defaults to the active tab.
    id: Option<u32>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct TabSelectOptions {
    /// Position in the strip, counting from 1. Negative counts from the end,
    /// so -1 is the last tab.
    index: Option<i32>,
}

/// Where a tab with nowhere to go should land: our own start page, or
/// `about:blank` before the control plane is up.
///
/// Shared by `tab open` and `nav home` so a new tab and Alt-Home cannot
/// disagree about what "home" is.
fn start_page(state: &AppState) -> String {
    state
        .base_url()
        .and_then(|b| b.join("start").ok())
        .map(|u| u.to_string())
        .unwrap_or_else(|| "about:blank".to_string())
}

#[derive(JsonSchema, Serialize)]
struct TabList {
    tabs: Vec<crate::tabs::Tab>,
}

#[derive(JsonSchema, Serialize)]
struct Closed {
    closed: u32,
    active: Option<u32>,
}

fn tab_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let open = CommandDef::typed::<TabOpenArgs, TabOpenOptions, (), crate::tabs::Tab, _, _>(
        "open",
        move |ctx: TypedContext<TabOpenArgs, TabOpenOptions, ()>| {
            let state = s.clone();
            async move {
                // A new tab with nowhere to go lands on our own start page --
                // the same answer a bare launch gives (`main.rs`). The keyboard
                // used to compute this itself and the palette picked
                // `about:blank`, so the three surfaces disagreed; it belongs
                // here, where every caller gets it.
                let url = match ctx.options.url.or(ctx.args.url) {
                    Some(url) => url,
                    None => start_page(&state),
                };
                match crate::tabs::open(&state, &url, ctx.options.background).await {
                    Ok(tab) => {
                        state.notify_tabs();
                        TypedResult::ok(tab)
                    }
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description("Open a URL in a new tab")
    .done();

    let s = state.clone();
    let list = CommandDef::typed::<NoArgs, NoOptions, (), TabList, _, _>(
        "list",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move { TypedResult::ok(TabList { tabs: state.tabs.read().await.list() }) }
        },
    )
    .description("List open tabs")
    .done();

    let s = state.clone();
    let select = CommandDef::typed::<TabIdArgs, TabSelectOptions, (), TabList, _, _>(
        "select",
        move |ctx: TypedContext<TabIdArgs, TabSelectOptions, ()>| {
            let state = s.clone();
            async move {
                // An id names a tab for as long as it lives; a position names
                // whatever is sitting there now. Ctrl-1..Ctrl-9 want the
                // second, everything else wants the first, so both are here
                // rather than in a second command that would have to repeat
                // the selection and the notify.
                let id = match ctx.args.id {
                    Some(id) => Some(id),
                    None => match ctx.options.index {
                        Some(pos) => match state.tabs.read().await.by_position(pos) {
                            Some(id) => Some(id),
                            // Ctrl-5 with four tabs open is a miss, not an
                            // error worth a notification.
                            None => return TypedResult::ok(TabList {
                                tabs: state.tabs.read().await.list(),
                            }),
                        },
                        None => None,
                    },
                };
                let Some(id) = id else {
                    return TypedResult::error("missing_id", "which tab? pass an id from `tab list`");
                };
                match crate::tabs::select(&state, id).await {
                    Ok(()) => {
                        state.notify_tabs();
                        TypedResult::ok(TabList { tabs: state.tabs.read().await.list() })
                    }
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description("Switch to a tab by id, or by position with --index")
    .done();

    let s = state.clone();
    let close = CommandDef::typed::<TabIdArgs, NoOptions, (), Closed, _, _>(
        "close",
        move |ctx: TypedContext<TabIdArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let requested = ctx.args.id;
                let target = match requested {
                    Some(id) => Some(id),
                    None => state.tabs.read().await.active_id(),
                };
                let Some(target) = target else {
                    return TypedResult::error("no_tabs", "there are no tabs to close");
                };
                match crate::tabs::close(&state, Some(target)).await {
                    Ok(active) => {
                        state.notify_tabs();
                        TypedResult::ok(Closed { closed: target, active })
                    }
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description("Close a tab, defaulting to the active one")
    .done();

    let s = state.clone();
    let cycle = CommandDef::typed::<CycleArgs, NoOptions, (), TabList, _, _>(
        "cycle",
        move |ctx: TypedContext<CycleArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let next = state.tabs.read().await.neighbour(ctx.args.delta.unwrap_or(1));
                match next {
                    Some(id) => match crate::tabs::select(&state, id).await {
                        Ok(()) => {
                            state.notify_tabs();
                            TypedResult::ok(TabList { tabs: state.tabs.read().await.list() })
                        }
                        Err(e) => TypedResult::error("webview", format!("{e:#}")),
                    },
                    None => TypedResult::error("no_tabs", "there are no tabs"),
                }
            }
        },
    )
    .description("Move to the next or previous tab, wrapping")
    .done();

    let s = state.clone();
    let reopen = CommandDef::typed::<NoArgs, NoOptions, (), crate::tabs::Tab, _, _>(
        "reopen",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let url = state.tabs.write().await.take_closed();
                let Some(url) = url else {
                    return TypedResult::error("no_tabs", "nothing has been closed".to_string());
                };
                match crate::tabs::open(&state, &url, false).await {
                    Ok(tab) => {
                        state.notify_tabs();
                        TypedResult::ok(tab)
                    }
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description("Reopen the most recently closed tab")
    .done();

    Cli::create("tab")
        .description("Open, close and switch tabs")
        .command("open", open)
        .command("list", list)
        .command("select", select)
        .command("close", close)
        .command("cycle", cycle)
        .command("reopen", reopen)
}

#[derive(Deserialize, incurs::Args)]
struct CycleArgs {
    /// How many tabs to move; negative goes backwards. Defaults to 1.
    delta: Option<i32>,
}

// ---------------------------------------------------------------------------
// nav
// ---------------------------------------------------------------------------

/// The URL is reachable two ways on purpose. Over HTTP incurs binds positionals
/// to *path segments*, so `https://a/b` arrives split and truncated; the option
/// carries it in the body instead. On the CLI the positional is what anyone
/// would type, so both are kept and the option wins when present.
#[derive(Deserialize, incurs::Args)]
struct GoArgs {
    /// A URL, a bare host, or search terms.
    url: Option<String>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct GoOptions {
    // Same as the positional, for callers whose transport cannot carry slashes.
    // Not a doc comment: incurs merges args and options into one schema with the
    // options last, so this text would be what the palette shows for the field.
    /// A URL, a bare host, or search terms.
    url: Option<String>,
}

#[derive(JsonSchema, Serialize)]
struct Navigated {
    url: String,
}

#[derive(JsonSchema, Serialize)]
struct Acted {
    ok: bool,
}

fn nav_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let go = CommandDef::typed::<GoArgs, GoOptions, (), Navigated, _, _>(
        "go",
        move |ctx: TypedContext<GoArgs, GoOptions, ()>| {
            let state = s.clone();
            async move {
                let Some(url) = ctx.options.url.or(ctx.args.url) else {
                    return TypedResult::error("usage", "a url is required".to_string());
                };
                match crate::tabs::navigate(&state, &url).await {
                    Ok(url) => {
                        state.notify_tabs();
                        TypedResult::ok(Navigated { url })
                    }
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description("Navigate the active tab")
    .done();

    let s = state.clone();
    let home = CommandDef::typed::<NoArgs, NoOptions, (), Navigated, _, _>(
        "home",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let url = start_page(&state);
                match crate::tabs::navigate(&state, &url).await {
                    Ok(url) => {
                        state.notify_tabs();
                        TypedResult::ok(Navigated { url })
                    }
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description("Send the active tab to the start page")
    .done();

    let mut group = Cli::create("nav")
        .description("Move the active tab around")
        .command("go", go)
        .command("home", home);

    for (name, action, blurb) in [
        ("back", crate::tabs::HistoryAction::Back, "Go back in history"),
        ("forward", crate::tabs::HistoryAction::Forward, "Go forward in history"),
        ("reload", crate::tabs::HistoryAction::Reload, "Reload the active tab"),
        ("stop", crate::tabs::HistoryAction::Stop, "Stop loading"),
    ] {
        let s = state.clone();
        let cmd = CommandDef::typed::<NoArgs, NoOptions, (), Acted, _, _>(
            name,
            move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
                let state = s.clone();
                async move {
                    match crate::tabs::history(&state, action).await {
                        Ok(()) => TypedResult::ok(Acted { ok: true }),
                        Err(e) => TypedResult::error("webview", format!("{e:#}")),
                    }
                }
            },
        )
        .description(blurb)
        .done();
        group = group.command(name, cmd);
    }
    group
}

// ---------------------------------------------------------------------------
// page
// ---------------------------------------------------------------------------

#[derive(Deserialize, incurs::Args)]
struct EvalArgs {
    /// JavaScript to run in the active tab.
    js: Option<String>,
}

/// See [`GoOptions`]: any script with a `/` in it is unroutable as a positional.
#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct EvalOptions {
    // Same as the positional, for callers whose transport cannot carry slashes.
    // Not a doc comment: incurs merges args and options into one schema with the
    // options last, so this text would be what the palette shows for the field.
    /// JavaScript to run in the active tab.
    js: Option<String>,
}

#[derive(JsonSchema, Serialize)]
struct Evaluated {
    /// The expression's value, as JSON.
    result: String,
}

/// Every field is an option rather than a positional argument on purpose: over
/// HTTP incurs binds positionals to *path segments*, and a filesystem path
/// contains slashes, so `page screenshot /tmp/a.png` would arrive as `tmp`.
#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct ShotOptions {
    /// Where to write the PNG. Defaults to `$XDG_RUNTIME_DIR/oma-browse/`.
    path: Option<String>,
    /// Capture the whole scrollable document instead of just the viewport.
    full: bool,
    /// Composite onto white instead of preserving the page's transparency.
    opaque: bool,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct ZoomOptions {
    /// Set an exact level, e.g. 1.5. Overrides the direction.
    level: Option<f64>,
}

#[derive(Deserialize, incurs::Args)]
struct ZoomArgs {
    /// `in`, `out`, or `reset`. Defaults to `reset`.
    direction: Option<String>,
}

#[derive(JsonSchema, Serialize)]
struct Zoomed {
    level: f64,
}

fn page_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let eval = CommandDef::typed::<EvalArgs, EvalOptions, (), Evaluated, _, _>(
        "eval",
        move |ctx: TypedContext<EvalArgs, EvalOptions, ()>| {
            let state = s.clone();
            async move {
                let Some(js) = ctx.options.js.or(ctx.args.js) else {
                    return TypedResult::error("usage", "a script is required".to_string());
                };
                match crate::tabs::eval(&state, &js).await {
                    Ok(result) => TypedResult::ok(Evaluated { result }),
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description("Evaluate JavaScript in the active tab and return the result")
    .done();

    let s = state.clone();
    let screenshot = CommandDef::typed::<NoArgs, ShotOptions, (), crate::shot::Shot, _, _>(
        "screenshot",
        move |ctx: TypedContext<NoArgs, ShotOptions, ()>| {
            let state = s.clone();
            async move {
                let opts = ctx.options;
                match crate::shot::capture(&state, opts.path, opts.full, !opts.opaque).await {
                    Ok(shot) => TypedResult::ok(shot),
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description(
        "Capture the active tab as a PNG and return its path. Uses WebKit's own \
         snapshot, so it works while the window is on another workspace -- but \
         that path re-renders without compositor effects, so backdrop-filter \
         blur is absent from the image while being present on screen.",
    )
    .done();

    let s = state.clone();
    let zoom = CommandDef::typed::<ZoomArgs, ZoomOptions, (), Zoomed, _, _>(
        "zoom",
        move |ctx: TypedContext<ZoomArgs, ZoomOptions, ()>| {
            let state = s.clone();
            async move {
                let change = match ctx.options.level {
                    Some(level) => crate::tabs::ZoomChange::Set(level),
                    None => match ctx.args.direction.as_deref().unwrap_or("reset") {
                        "in" => crate::tabs::ZoomChange::In,
                        "out" => crate::tabs::ZoomChange::Out,
                        "reset" => crate::tabs::ZoomChange::Reset,
                        other => {
                            return TypedResult::error(
                                "usage",
                                format!("unknown direction {other:?}; use in, out or reset"),
                            );
                        }
                    },
                };
                match crate::tabs::zoom(&state, change).await {
                    Ok(level) => TypedResult::ok(Zoomed { level }),
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description(
        "Zoom the active tab. Steps along Chrome's zoom ladder; also bound to \
         Ctrl+= / Ctrl+- / Ctrl+0 on the window. Zoom is per tab, not per site.",
    )
    .done();

    Cli::create("page")
        .description("Inspect the active page")
        .command("eval", eval)
        .command("screenshot", screenshot)
        .command("zoom", zoom)
}

// ---------------------------------------------------------------------------
// ui
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct PaletteOptions {
    /// Open already asking for this command's argument, e.g. `find_text`.
    stage: Option<String>,
}

#[derive(Deserialize, incurs::Args)]
struct PaletteArgs {
    /// `show`, `hide`, or `toggle`. Defaults to `toggle`.
    action: Option<String>,
}

#[derive(JsonSchema, Serialize)]
struct PaletteState {
    visible: bool,
}

fn ui_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let palette = CommandDef::typed::<PaletteArgs, PaletteOptions, (), PaletteState, _, _>(
        "palette",
        move |ctx: TypedContext<PaletteArgs, PaletteOptions, ()>| {
            let state = s.clone();
            async move {
                // Staged before showing: the palette reads and clears this as
                // it renders, and it reloads on every summon.
                state.set_stage(ctx.options.stage.filter(|t| !t.is_empty()));
                let want = match ctx.args.action.as_deref().unwrap_or("toggle") {
                    "show" => true,
                    "hide" => false,
                    "toggle" => !state.palette_visible(),
                    other => {
                        return TypedResult::error(
                            "bad_action",
                            format!("unknown action {other:?}; expected show, hide or toggle"),
                        );
                    }
                };
                match crate::window::set_palette_visible(&state, want) {
                    Ok(()) => {
                        state.set_palette_visible(want);
                        TypedResult::ok(PaletteState { visible: want })
                    }
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description("Show, hide or toggle the command palette, optionally staged into a command")
    .done();

    Cli::create("ui").description("Drive the browser's own interface").command("palette", palette)
}

// ---------------------------------------------------------------------------
// window
// ---------------------------------------------------------------------------

#[derive(Deserialize, incurs::Args)]
struct FullscreenArgs {
    /// `toggle`, `on`, or `off`. Defaults to `toggle`.
    action: Option<String>,
}

#[derive(JsonSchema, Serialize)]
struct WindowState {
    fullscreen: bool,
}

fn window_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let fullscreen = CommandDef::typed::<FullscreenArgs, NoOptions, (), WindowState, _, _>(
        "fullscreen",
        move |ctx: TypedContext<FullscreenArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let want = match ctx.args.action.as_deref().unwrap_or("toggle") {
                    "on" => Some(true),
                    "off" => Some(false),
                    "toggle" => None,
                    other => {
                        return TypedResult::error(
                            "bad_action",
                            format!("unknown action {other:?}; expected toggle, on or off"),
                        );
                    }
                };
                match crate::window::set_fullscreen(&state, want) {
                    Ok(fullscreen) => TypedResult::ok(WindowState { fullscreen }),
                    Err(e) => TypedResult::error("window", format!("{e:#}")),
                }
            }
        },
    )
    .description("Take the window fullscreen, or bring it back")
    .done();

    let s = state;
    let close = CommandDef::typed::<NoArgs, NoOptions, (), Acted, _, _>(
        "close",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                match crate::window::close(&state) {
                    Ok(()) => TypedResult::ok(Acted { ok: true }),
                    Err(e) => TypedResult::error("window", format!("{e:#}")),
                }
            }
        },
    )
    .description("Close the window, and with it the browser")
    .done();

    Cli::create("window")
        .description("The native window itself")
        .command("fullscreen", fullscreen)
        .command("close", close)
}

fn theme_group(state: Arc<AppState>) -> Cli {
    let show_state = state.clone();
    let show = CommandDef::typed::<NoArgs, NoOptions, (), ThemeInfo, _, _>(
        "show",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = show_state.clone();
            async move {
                let theme = state.theme.read().await;
                TypedResult::ok(ThemeInfo {
                    name: theme.name.clone(),
                    mode: theme.mode.as_str().to_string(),
                    colors: theme.colors,
                })
            }
        },
    )
    .description("Show the Omarchy theme the browser is currently wearing")
    .done();

    let reload_state = state.clone();
    let reload = CommandDef::typed::<ThemeReloadArgs, NoOptions, (), ThemeReloaded, _, _>(
        "reload",
        move |ctx: TypedContext<ThemeReloadArgs, NoOptions, ()>| {
            let state = reload_state.clone();
            async move {
                let _ = ctx.args.name;
                let changed = state.reload_theme().await;
                // Re-reading is not restyling. The description has always
                // promised both, and the file watcher does both, but this
                // handler only ever did the first -- so `theme reload` from the
                // CLI or the theme-set hook left the window wearing the old
                // colours until something else repainted it.
                if changed {
                    let _ = crate::window::restyle(&state).await;
                }
                let theme = state.theme.read().await;
                TypedResult::ok(ThemeReloaded {
                    name: theme.name.clone(),
                    mode: theme.mode.as_str().to_string(),
                    changed,
                })
            }
        },
    )
    .description("Re-read the live Omarchy theme and restyle. This is what the theme-set hook calls.")
    .done();

    let css_state = state.clone();
    let css = CommandDef::typed::<NoArgs, NoOptions, (), CssDump, _, _>(
        "css",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = css_state.clone();
            async move {
                let theme = state.theme.read().await;
                TypedResult::ok(CssDump { css: theme.css.chrome.clone() })
            }
        },
    )
    .description("Print the CSS custom properties derived from the current theme")
    .done();

    let s = state.clone();
    let recolor = CommandDef::typed::<RecolorArgs, NoOptions, (), RecolorState, _, _>(
        "recolor",
        move |ctx: TypedContext<RecolorArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let want = match ctx.args.mode.as_deref().unwrap_or("toggle") {
                    "on" => true,
                    "off" => false,
                    "toggle" => !state.recolor(),
                    other => {
                        return TypedResult::error(
                            "bad_mode",
                            format!("unknown mode {other:?}; expected on, off or toggle"),
                        );
                    }
                };
                state.set_recolor(want);
                let _ = crate::window::restyle(&state).await;
                TypedResult::ok(RecolorState { recolor: want })
            }
        },
    )
    .description(
        "Repaint neutral page surfaces. Only greyscale backgrounds are replaced, so brand \
         colour survives. On by default; turn it off for a site that does not survive it.",
    )
    .done();

    Cli::create("theme")
        .description("Inspect and reload Omarchy theming")
        .command("recolor", recolor)
        .command("show", show)
        .command("reload", reload)
        .command("css", css)
}

#[derive(Deserialize, incurs::Args)]
struct RecolorArgs {
    /// `on`, `off`, or `toggle`. Defaults to `toggle`.
    mode: Option<String>,
}

#[derive(JsonSchema, Serialize)]
struct RecolorState {
    recolor: bool,
}

#[derive(JsonSchema, Serialize)]
struct CssDump {
    css: String,
}
