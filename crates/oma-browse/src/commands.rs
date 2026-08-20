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
    ("config", "Config"),
    ("nav", "Navigation"),
    ("tab", "Tabs"),
    ("page", "Page"),
    ("theme", "Theme"),
    ("ui", "Interface"),
    ("history", "History"),
    ("bookmark", "Bookmarks"),
    ("find", "Find"),
    ("share", "Share"),
    ("download", "Downloads"),
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
        .group(download_group(state.clone()))
        .group(share_group(state.clone()))
        .group(config_group(state.clone()))
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
// download
// ---------------------------------------------------------------------------

#[derive(JsonSchema, Serialize)]
struct Saved {
    /// The file's own name, which is the part a human recognises.
    name: String,
    /// Where it went.
    path: String,
    url: String,
    /// `running`, `done` or `failed`.
    state: String,
}

#[derive(JsonSchema, Serialize)]
struct SavedList {
    /// Newest first, so position 1 is the download just finished.
    entries: Vec<Saved>,
    /// Where new downloads land.
    directory: String,
}

#[derive(Deserialize, incurs::Args)]
struct DownloadArgs {
    /// Which download, counting from the newest as 1. Defaults to the newest.
    index: Option<usize>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct DownloadOptions {
    /// Which download, counting from the newest as 1. See [`ToggleOptions`].
    index: Option<usize>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct DownloadListOptions {
    /// How many to return. Defaults to 50.
    limit: Option<u32>,
}

fn saved(d: &crate::downloads::Download) -> Saved {
    Saved {
        name: d.name(),
        path: d.path.display().to_string(),
        url: d.url.clone(),
        state: d.state().to_string(),
    }
}

/// Pick one download out of the list, by position or "the newest".
///
/// Cloned out under the lock rather than returning a borrow: the store is behind
/// a `std::sync::Mutex` and the caller is in an async fn, so the guard must not
/// live across an await.
fn pick(state: &Arc<AppState>, index: Option<usize>) -> std::result::Result<Saved, String> {
    let list = state.downloads.lock().map_err(|_| "the download list is poisoned".to_string())?;
    let entry = match index {
        Some(n) => list.nth(n).ok_or_else(|| format!("there is no download {n}")),
        None => list.entries().first().ok_or_else(|| "nothing has been downloaded".to_string()),
    }?;
    Ok(saved(entry))
}

fn download_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let list = CommandDef::typed::<NoArgs, DownloadListOptions, (), SavedList, _, _>(
        "list",
        move |ctx: TypedContext<NoArgs, DownloadListOptions, ()>| {
            let state = s.clone();
            async move {
                let limit = ctx.options.limit.unwrap_or(50) as usize;
                let directory = state.download_path("x").parent().map(|p| p.display().to_string());
                let Ok(downloads) = state.downloads.lock() else {
                    return TypedResult::error("poisoned", "the download list is gone".to_string());
                };
                TypedResult::ok(SavedList {
                    entries: downloads.entries().iter().take(limit).map(saved).collect(),
                    directory: directory.unwrap_or_default(),
                })
            }
        },
    )
    .description("List downloaded files, newest first")
    .done();

    let s = state.clone();
    let open = CommandDef::typed::<DownloadArgs, DownloadOptions, (), Saved, _, _>(
        "open",
        move |ctx: TypedContext<DownloadArgs, DownloadOptions, ()>| {
            let state = s.clone();
            async move {
                let entry = match pick(&state, ctx.options.index.or(ctx.args.index)) {
                    Ok(entry) => entry,
                    Err(e) => return TypedResult::error("missing_id", e),
                };
                if !std::path::Path::new(&entry.path).exists() {
                    return TypedResult::error(
                        "gone",
                        format!("{} is no longer on disk", entry.path),
                    );
                }
                // `xdg-open`, not a guess at the application: the desktop
                // already knows what opens a `.pdf`, and disagreeing with it is
                // how a browser ends up with its own half-working file manager.
                match handoff("xdg-open", &[entry.path.as_str()]).await {
                    Ok(()) => TypedResult::ok(entry),
                    Err(e) => TypedResult::error("desktop", e),
                }
            }
        },
    )
    .description("Open a downloaded file with the desktop's own handler")
    .done();

    let s = state.clone();
    let reveal = CommandDef::typed::<DownloadArgs, DownloadOptions, (), Saved, _, _>(
        "reveal",
        move |ctx: TypedContext<DownloadArgs, DownloadOptions, ()>| {
            let state = s.clone();
            async move {
                let entry = match pick(&state, ctx.options.index.or(ctx.args.index)) {
                    Ok(entry) => entry,
                    Err(e) => return TypedResult::error("missing_id", e),
                };
                let Some(dir) = std::path::Path::new(&entry.path).parent() else {
                    return TypedResult::error("gone", "that file has no folder".to_string());
                };
                match handoff("xdg-open", &[&dir.display().to_string()]).await {
                    Ok(()) => TypedResult::ok(entry),
                    Err(e) => TypedResult::error("desktop", e),
                }
            }
        },
    )
    .description("Open the folder a downloaded file is in")
    .done();

    let s = state.clone();
    let copy = CommandDef::typed::<DownloadArgs, DownloadOptions, (), Saved, _, _>(
        "copy",
        move |ctx: TypedContext<DownloadArgs, DownloadOptions, ()>| {
            let state = s.clone();
            async move {
                let entry = match pick(&state, ctx.options.index.or(ctx.args.index)) {
                    Ok(entry) => entry,
                    Err(e) => return TypedResult::error("missing_id", e),
                };
                match handoff("wl-copy", &[entry.path.as_str()]).await {
                    Ok(()) => TypedResult::ok(entry),
                    Err(e) => TypedResult::error("clipboard", e),
                }
            }
        },
    )
    .description("Copy a downloaded file's path to the Wayland clipboard")
    .done();

    let s = state;
    let clear = CommandDef::typed::<NoArgs, NoOptions, (), Forgotten, _, _>(
        "clear",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let Ok(mut downloads) = state.downloads.lock() else {
                    return TypedResult::error("poisoned", "the download list is gone".to_string());
                };
                let cleared = downloads.entries().len();
                downloads.clear();
                // The list, not the files. Deleting somebody's files because
                // they tidied a list is not a thing a browser gets to do.
                TypedResult::ok(Forgotten { cleared })
            }
        },
    )
    .description("Forget the download list. The files themselves are left alone")
    .destructive(true)
    .done();

    Cli::create("download")
        .description("Files this browser has saved")
        .command("list", list)
        .command("open", open)
        .command("reveal", reveal)
        .command("copy", copy)
        .command("clear", clear)
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
                let title =
                    ctx.options.title.or_else(|| current.map(|(_, t)| t)).unwrap_or_default();
                let added = state.bookmarks.write().await.add(&url, &title, crate::history::now());
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
                        .map(|b| Visited { url: b.url.clone(), title: b.title.clone(), visits: 0 })
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

#[derive(JsonSchema, Serialize)]
struct Restored {
    /// How many tabs were reopened.
    opened: usize,
    /// How many the last session held, whether or not they were already open.
    saved: usize,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct MuteOptions {
    /// Which tab, as reported by `tab list`. Defaults to the active tab.
    id: Option<u32>,
    /// `on`, `off` or `toggle`. Defaults to `toggle`. See [`ToggleOptions`].
    action: Option<String>,
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
    let built_in = state
        .base_url()
        .and_then(|b| b.join("start").ok())
        .map(|u| u.to_string())
        .unwrap_or_else(|| "about:blank".to_string());
    state.config.home_url(&built_in)
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
                let (url, blank) = match ctx.options.url.or(ctx.args.url) {
                    Some(url) => (url, false),
                    None => (start_page(&state), true),
                };
                match crate::tabs::open(&state, &url, ctx.options.background).await {
                    Ok(tab) => {
                        state.notify_tabs();
                        // A tab opened with nowhere to go is a question, so ask
                        // it: the palette is this browser's URL bar, and Ctrl-T
                        // landing on the start page with nothing focused left
                        // you a keystroke short of anywhere. A tab opened *at* a
                        // URL already has its answer, and a background tab is
                        // not the one you are looking at.
                        if blank && !ctx.options.background {
                            match crate::window::set_palette_visible(&state, true) {
                                Ok(()) => state.set_palette_visible(true),
                                Err(e) => tracing::warn!(
                                    error = %e,
                                    "opened a blank tab but could not summon the palette"
                                ),
                            }
                        }
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
                            None => {
                                return TypedResult::ok(TabList {
                                    tabs: state.tabs.read().await.list(),
                                });
                            }
                        },
                        None => None,
                    },
                };
                let Some(id) = id else {
                    return TypedResult::error(
                        "missing_id",
                        "which tab? pass an id from `tab list`",
                    );
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

    let s = state.clone();
    let mute = CommandDef::typed::<ToggleArgs, MuteOptions, (), Toggled, _, _>(
        "mute",
        move |ctx: TypedContext<ToggleArgs, MuteOptions, ()>| {
            let state = s.clone();
            async move {
                let action = ctx.options.action.or(ctx.args.action);
                let raw = action.as_deref().unwrap_or("toggle");
                let Some(action) = crate::tabs::Toggle::parse(raw) else {
                    return TypedResult::error(
                        "bad_action",
                        format!("unknown action {raw:?}; expected on, off or toggle"),
                    );
                };
                match crate::tabs::mute(&state, ctx.options.id, action).await {
                    Ok(on) => TypedResult::ok(Toggled { on }),
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description("Silence a tab, or let it speak again. Per tab, like zoom")
    .done();

    let s = state.clone();
    let restore = CommandDef::typed::<NoArgs, NoOptions, (), Restored, _, _>(
        "restore",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let saved = crate::session::saved().len();
                let opened = crate::session::restore(&state).await;
                TypedResult::ok(Restored { opened, saved })
            }
        },
    )
    .description(
        "Reopen the tabs from the last session, in the background. Anything \
         already open is skipped, so running it twice changes nothing.",
    )
    .done();

    Cli::create("tab")
        .description("Open, close and switch tabs")
        .command("mute", mute)
        .command("restore", restore)
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
    /// Flatten the shot onto the theme's background colour instead of
    /// leaving it transparent. A translucent PNG looks fine on a white
    /// viewer and hides dark-on-dark text; this shows what is on screen.
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

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct SourceOptions {
    /// Open the source in a new tab instead of returning it.
    open: bool,
    /// Write it here. Implied by `--open`, which needs a file to point at.
    path: Option<String>,
}

#[derive(JsonSchema, Serialize)]
struct Source {
    url: String,
    /// How many bytes of markup, so a caller can decide whether to print it.
    bytes: usize,
    /// The markup itself, omitted when it was written to a file instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    html: Option<String>,
    /// Where it was written, when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Deserialize, incurs::Args)]
struct ToggleArgs {
    /// `show`, `hide` or `toggle`. Defaults to `toggle`.
    action: Option<String>,
}

/// Same reason as `nav go`'s `url`: over HTTP a positional argument is a path
/// segment, so a JSON body carrying one is silently ignored and the command
/// runs with its default. An option of the same name, which wins, is what makes
/// the HTTP face agree with the CLI.
#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct ToggleOptions {
    /// `show`, `hide` or `toggle`. Defaults to `toggle`.
    action: Option<String>,
}

#[derive(JsonSchema, Serialize)]
struct Toggled {
    /// Where the thing ended up.
    on: bool,
}

#[derive(Deserialize, incurs::Args)]
struct PrintArgs {
    // Same reason as `nav go`: over HTTP a positional is a path segment, and a
    // path is all slashes.
    /// Write a PDF here instead of opening the print dialog.
    path: Option<String>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct PrintOptions {
    /// Write a PDF here instead of opening the print dialog.
    path: Option<String>,
    /// Open GTK's print dialog instead of writing a PDF. Currently broken
    /// under this runtime -- see `tabs::print` -- so it is opt-in.
    dialog: bool,
}

#[derive(JsonSchema, Serialize)]
struct Printed {
    /// The PDF written, or null when the print dialog was opened instead.
    path: Option<String>,
}

/// Where a PDF goes when the caller did not name a file.
///
/// The downloads directory under the page's own title, because a PDF of a page
/// is a file the user went looking for, and `$XDG_RUNTIME_DIR` -- where the
/// screenshots go -- is wiped at reboot.
async fn default_pdf(state: &std::sync::Arc<crate::state::AppState>) -> std::path::PathBuf {
    let title = {
        let tabs = state.tabs.read().await;
        tabs.list().iter().find(|t| t.active).map(|t| t.title.clone()).unwrap_or_default()
    };
    let title = title.trim();
    let stem = if title.is_empty() { "page" } else { title };
    let dir = crate::downloads::download_dir();
    crate::downloads::unique(&dir, &format!("{stem}.pdf"))
}

#[derive(Deserialize, incurs::Args)]
struct HintsArgs {
    /// `click`, `newtab`, or `clear`. Defaults to `click`.
    mode: Option<String>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct HintsOptions {
    /// `click`, `newtab`, or `clear`. Defaults to `click`. See [`ToggleOptions`].
    mode: Option<String>,
}

#[derive(JsonSchema, Serialize)]
struct Hinted {
    /// How many hints were drawn. Zero means nothing clickable is on screen.
    shown: u32,
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

    let s = state.clone();
    let hints = CommandDef::typed::<HintsArgs, HintsOptions, (), Hinted, _, _>(
        "hints",
        move |ctx: TypedContext<HintsArgs, HintsOptions, ()>| {
            let state = s.clone();
            async move {
                let asked = ctx.options.mode.or(ctx.args.mode);
                let mode = match asked.as_deref().unwrap_or("click") {
                    "click" | "open" | "show" => "click",
                    "newtab" | "tab" | "background" => "newtab",
                    "clear" | "hide" | "off" => "clear",
                    other => {
                        return TypedResult::error(
                            "usage",
                            format!("unknown mode {other:?}; use click, newtab or clear"),
                        );
                    }
                };
                // The script answers with the number of hints it drew, or is
                // absent entirely on a page that runs no scripts of ours.
                let js = format!(
                    "window.__omaHints ? String(window.__omaHints(\"{mode}\")) : \"absent\""
                );
                match crate::tabs::eval(&state, &js).await {
                    Ok(raw) => match raw.trim().trim_matches('"').parse::<u32>() {
                        Ok(shown) => TypedResult::ok(Hinted { shown }),
                        Err(_) => TypedResult::error(
                            "no_script",
                            "this page does not run our scripts, so it has no hints".to_string(),
                        ),
                    },
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description(
        "Label every clickable thing in the viewport and activate the one you \
         type. Also on a bare `f` (and `F` to open links in a new tab) whenever \
         the caret is not in a text field.",
    )
    .done();

    let s = state.clone();
    let devtools = CommandDef::typed::<ToggleArgs, ToggleOptions, (), Toggled, _, _>(
        "devtools",
        move |ctx: TypedContext<ToggleArgs, ToggleOptions, ()>| {
            let state = s.clone();
            async move {
                let action = ctx.options.action.or(ctx.args.action);
                let raw = action.as_deref().unwrap_or("toggle");
                let Some(action) = crate::tabs::Toggle::parse(raw) else {
                    return TypedResult::error(
                        "bad_action",
                        format!("unknown action {raw:?}; expected show, hide or toggle"),
                    );
                };
                match crate::tabs::devtools(&state, action).await {
                    Ok(on) => TypedResult::ok(Toggled { on }),
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description(
        "Open WebKit's inspector on the active tab. It is a WebKit window of \
         its own, so it is not themed and does not tile with the page.",
    )
    .done();

    let s = state.clone();
    let print = CommandDef::typed::<PrintArgs, PrintOptions, (), Printed, _, _>(
        "print",
        move |ctx: TypedContext<PrintArgs, PrintOptions, ()>| {
            let state = s.clone();
            async move {
                let to = ctx.options.path.or(ctx.args.path).filter(|p| !p.is_empty());
                let to = to.map(|p| {
                    let expanded = match p.strip_prefix("~/") {
                        Some(rest) => {
                            format!("{}/{rest}", std::env::var("HOME").unwrap_or_default())
                        }
                        None => p,
                    };
                    std::path::PathBuf::from(expanded)
                });
                let dialog = ctx.options.dialog;
                // No path and no `--dialog` writes a PDF where the user's other
                // downloads go. Defaulting to the dialog would be the obvious
                // choice and is the wrong one: it is unusable here and takes the
                // whole window down with it for two minutes.
                let to = match to {
                    Some(path) => Some(path),
                    None if dialog => None,
                    None => Some(default_pdf(&state).await),
                };
                match crate::tabs::print(&state, to, dialog).await {
                    Ok(path) => TypedResult::ok(Printed { path }),
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description(
        "Print the active tab to a PDF, in the downloads directory under the \
         page's own title unless a path says otherwise. Never opens a dialog, \
         which is what makes it usable from the CLI, from an agent and from a \
         key; `--dialog` asks for GTK's printer chooser instead.",
    )
    .done();

    let s = state.clone();
    let source = CommandDef::typed::<NoArgs, SourceOptions, (), Source, _, _>(
        "source",
        move |ctx: TypedContext<NoArgs, SourceOptions, ()>| {
            let state = s.clone();
            async move {
                // The live DOM, with this browser's own injections taken back
                // out. Not a re-fetch: a re-fetch gets a different page on
                // anything behind a login, and the question "what is on my
                // screen made of" is about the document that is on screen.
                let js = r#"(function(){
                    var root = document.documentElement.cloneNode(true);
                    // Every id this browser injects, not just the veil's: the
                    // strip's inset is `__oma_strip_inset`, and leaving it in
                    // makes the source of every page look like it ships a
                    // stylesheet it does not.
                    var mine = root.querySelectorAll('[id^="__oma_"],.__oma_browse_backer');
                    for (var i = 0; i < mine.length; i++) mine[i].remove();
                    return "<!DOCTYPE html>\n" + root.outerHTML;
                })()"#;
                let raw = match crate::tabs::eval(&state, js).await {
                    Ok(raw) => raw,
                    Err(e) => return TypedResult::error("webview", format!("{e:#}")),
                };
                // `eval` answers with JSON, so the markup arrives as a quoted,
                // escaped string.
                let html = match serde_json::from_str::<String>(&raw) {
                    Ok(html) => html,
                    Err(_) => raw,
                };
                let url = here(&state).await.map(|(u, _)| u).unwrap_or_default();
                let bytes = html.len();

                let wants_file = ctx.options.open || ctx.options.path.is_some();
                if !wants_file {
                    return TypedResult::ok(Source { url, bytes, html: Some(html), path: None });
                }

                // `.txt`, not `.html`: the point is to read the markup, and a
                // file WebKit recognises as HTML is one it renders instead.
                let path = match crate::shot::scratch_file(ctx.options.path, "source", "txt") {
                    Ok(path) => path,
                    Err(e) => return TypedResult::error("io", format!("{e:#}")),
                };
                if let Err(e) = std::fs::write(&path, &html) {
                    return TypedResult::error(
                        "io",
                        format!("could not write {}: {e}", path.display()),
                    );
                }
                if ctx.options.open {
                    let target = format!("file://{}", path.display());
                    if let Err(e) = crate::tabs::open(&state, &target, false).await {
                        return TypedResult::error("webview", format!("{e:#}"));
                    }
                }
                TypedResult::ok(Source {
                    url,
                    bytes,
                    html: None,
                    path: Some(path.display().to_string()),
                })
            }
        },
    )
    .description(
        "The active page's markup, with this browser's own injections removed. \
         `--open` puts it in a new tab as plain text.",
    )
    .done();

    Cli::create("page")
        .description("Inspect the active page")
        .command("source", source)
        .command("devtools", devtools)
        .command("print", print)
        .command("eval", eval)
        .command("screenshot", screenshot)
        .command("zoom", zoom)
        .command("hints", hints)
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

#[derive(JsonSchema, Serialize)]
struct Dismissed {
    /// Whether the palette was up and has now been put away.
    palette: bool,
    /// Whether the page was reachable to clear hints on.
    hints: bool,
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

    let s = state.clone();
    let dismiss = CommandDef::typed::<NoArgs, NoOptions, (), Dismissed, _, _>(
        "dismiss",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let palette = state.palette_visible();
                if palette {
                    if let Err(e) = crate::window::set_palette_visible(&state, false) {
                        return TypedResult::error("webview", format!("{e:#}"));
                    }
                    state.set_palette_visible(false);
                }
                // Best effort: a page with no scripts of ours has no hints to
                // clear, and Escape must not report that as a failure.
                let hints = crate::tabs::eval(
                    &state,
                    "window.__omaHints ? String(window.__omaHints(\"clear\")) : \"absent\"",
                )
                .await
                .is_ok();
                TypedResult::ok(Dismissed { palette, hints })
            }
        },
    )
    .description(
        "Put away whatever is open: the command palette, and any link hints on \
         the page. This is what Escape runs.",
    )
    .done();

    Cli::create("ui")
        .description("Drive the browser's own interface")
        .command("palette", palette)
        .command("dismiss", dismiss)
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

#[derive(Deserialize, incurs::Args)]
struct WindowNewArgs {
    /// A URL, a bare host, or search terms. Omitted, the new window lands on
    /// the start page with its palette already up.
    url: Option<String>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct WindowNewOptions {
    // Same as the positional, for callers whose transport cannot carry slashes.
    /// A URL, a bare host, or search terms.
    url: Option<String>,
}

#[derive(JsonSchema, Serialize)]
struct Spawned {
    /// The new window's process.
    pid: u32,
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

#[derive(JsonSchema, Serialize)]
struct ConfigState {
    /// Where the file is looked for, whether or not it exists.
    path: String,
    exists: bool,
    /// Every setting as the browser resolved it: the file over the defaults.
    settings: crate::config::Config,
}

#[derive(JsonSchema, Serialize)]
struct ConfigWritten {
    path: String,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct ConfigInitOptions {
    /// Overwrite a config file that is already there.
    force: Option<bool>,
}

/// Read the config file, and write a commented one to start from.
///
/// No `set` command, deliberately: the file is a dotfile people keep in a
/// dotfiles repository, and a command that rewrote it would have to give up its
/// comments and its ordering to do so.
fn config_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let show = CommandDef::typed::<NoArgs, NoOptions, (), ConfigState, _, _>(
        "show",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let path = crate::config::Config::path();
                // Whatever is *running*, which is the file only if it parsed --
                // and `problem` is how you find out it did not, since the
                // warning went to a log nobody is reading. A complaint raised
                // later, by a value that parsed but means nothing, lands here
                // too; see `AppState::note_config_problem`.
                let mut settings = state.config.clone();
                settings.problem = state.config_problem();
                TypedResult::ok(ConfigState {
                    exists: path.exists(),
                    path: path.display().to_string(),
                    settings,
                })
            }
        },
    )
    .description("Show the config file's path and every setting as resolved")
    .done();

    let init = CommandDef::typed::<NoArgs, ConfigInitOptions, (), ConfigWritten, _, _>(
        "init",
        move |ctx: TypedContext<NoArgs, ConfigInitOptions, ()>| async move {
            match crate::config::Config::write_template(ctx.options.force.unwrap_or(false)) {
                Ok(path) => TypedResult::ok(ConfigWritten { path: path.display().to_string() }),
                Err(e) => TypedResult::error("config", format!("{e:#}")),
            }
        },
    )
    .description("Write a commented config file with every setting at its default")
    .done();

    Cli::create("config")
        .description("The dotfile every setting comes from")
        .command("show", show)
        .command("init", init)
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

    let s = state.clone();
    let new = CommandDef::typed::<WindowNewArgs, WindowNewOptions, (), Spawned, _, _>(
        "new",
        move |ctx: TypedContext<WindowNewArgs, WindowNewOptions, ()>| {
            let state = s.clone();
            async move {
                // Resolved here rather than in the child, so that what you type
                // at Ctrl-N means the same thing as what you type at Ctrl-T: a
                // bare host becomes https, and anything else becomes a search.
                let url = ctx
                    .options
                    .url
                    .or(ctx.args.url)
                    .map(|input| crate::tabs::resolve_input(&input, &state.config.search));
                match crate::window::spawn(state.incognito(), url, false) {
                    Ok(pid) => TypedResult::ok(Spawned { pid }),
                    Err(e) => TypedResult::error("spawn", format!("{e:#}")),
                }
            }
        },
    )
    .description("Open another browser window")
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
        .command("new", new)
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
    .description(
        "Re-read the live Omarchy theme and restyle. This is what the theme-set hook calls.",
    )
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
