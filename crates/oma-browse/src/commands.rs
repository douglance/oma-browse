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
    ("permission", "Permissions"),
    ("content", "Content blocking"),
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
        .group(permission_group(state.clone()))
        .group(content_group(state.clone()))
        .group(window_group(state))
}

// ---------------------------------------------------------------------------
// content
// ---------------------------------------------------------------------------

#[derive(JsonSchema, Serialize)]
struct Blocking {
    /// Whether blocking is switched on at all.
    on: bool,
    /// The lists compiled and applied right now. Empty while a first compile is
    /// still running, which takes a few seconds for a real blocklist.
    lists: Vec<String>,
    /// Rule files that could not be used, and why.
    problems: Vec<String>,
    /// Whether the active tab is blocking. `content off` turns this off for one
    /// tab without turning anything else off.
    #[serde(skip_serializing_if = "Option::is_none")]
    here: Option<bool>,
}

#[derive(JsonSchema, Serialize)]
struct Excused {
    /// The tab this was about.
    url: String,
    /// Whether that tab is blocking now.
    blocking: bool,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct ReloadListsOptions {
    /// Compile again even if a compiled copy is already loaded.
    force: bool,
}

/// `content off` and `content on` are the same command with the switch the
/// other way up.
fn excuse_tab(
    state: Arc<AppState>,
    name: &'static str,
    off: bool,
    description: &'static str,
) -> CommandDef {
    CommandDef::typed::<NoArgs, NoOptions, (), Excused, _, _>(
        name,
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = state.clone();
            async move {
                let Some(app) = state.app_handle() else {
                    return TypedResult::error("no_window", "the window is not up yet".to_string());
                };
                let Some(label) = state.tabs.read().await.active_label() else {
                    return TypedResult::error("no_tab", "there is no active tab".to_string());
                };
                use tauri::Manager as _;
                let Some(view) = app.get_webview(&label) else {
                    return TypedResult::error("no_tab", format!("no webview labelled {label}"));
                };
                if let Err(e) = crate::blocker::excuse(&view, off) {
                    return TypedResult::error("webview", format!("{e:#}"));
                }
                let url = here(&state).await.map(|(u, _)| u).unwrap_or_default();
                TypedResult::ok(Excused { url, blocking: !off })
            }
        },
    )
    .description(description)
    .done()
}

fn content_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let reload = CommandDef::typed::<NoArgs, ReloadListsOptions, (), Blocking, _, _>(
        "reload",
        move |ctx: TypedContext<NoArgs, ReloadListsOptions, ()>| {
            let state = s.clone();
            async move {
                let _ = ctx.options.force;
                let tab = state.tabs.read().await.active_label();
                match crate::blocker::ask(&state, true, tab).await {
                    Ok(report) => TypedResult::ok(Blocking {
                        on: state.config.content.block,
                        lists: report.lists,
                        problems: report.problems,
                        here: report.here,
                    }),
                    Err(e) => TypedResult::error("blocker", format!("{e:#}")),
                }
            }
        },
    )
    .description(
        "Read the rule lists again and apply them. Compiling a real blocklist \
         takes a few seconds and happens in the background, so `content list` \
         is how you find out it finished.",
    )
    .done();

    let s = state.clone();
    let list = CommandDef::typed::<NoArgs, NoOptions, (), Blocking, _, _>(
        "list",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let tab = state.tabs.read().await.active_label();
                match crate::blocker::ask(&state, false, tab).await {
                    Ok(report) => TypedResult::ok(Blocking {
                        on: state.config.content.block,
                        lists: report.lists,
                        problems: report.problems,
                        here: report.here,
                    }),
                    Err(e) => TypedResult::error("blocker", format!("{e:#}")),
                }
            }
        },
    )
    .description("What is blocking, and what could not be read")
    .done();

    Cli::create("content")
        .description("Block what a page tries to fetch")
        .command("list", list)
        .command("reload", reload)
        .command(
            "off",
            excuse_tab(
                state.clone(),
                "off",
                true,
                "Stop blocking in this tab. Per tab and not written down: WebKit \
                 keeps filters on the webview, so this is exactly taking this \
                 tab's filters off. Reload the page to see the difference.",
            ),
        )
        .command("on", excuse_tab(state, "on", false, "Block in this tab again"))
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
                // The fourth positional is `omarchy-webapp-install`'s
                // `custom-exec`, and without it the launcher it writes runs
                // `omarchy-launch-webapp`, whose browser allowlist is
                // Chromium-family only -- so "install this page as an app" from
                // *this* browser installed a launcher that opened Chrome.
                //
                // The absolute path, not the bare name: a `.desktop` file is
                // run by whatever has no idea what this shell's PATH was.
                let exe = std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "oma-browse".to_string());
                let exec = format!("{exe} --app {url}");
                match handoff(
                    "omarchy",
                    &[
                        "webapp",
                        "install",
                        title.as_str(),
                        url.as_str(),
                        icon.as_str(),
                        exec.as_str(),
                    ],
                )
                .await
                {
                    Ok(()) => TypedResult::ok(Shared { url, via: "omarchy webapp".into() }),
                    Err(e) => TypedResult::error("omarchy", e),
                }
            }
        },
    )
    .description(
        "Install the current page as an Omarchy web app with its own launcher, \
         opening in this browser rather than in Chrome. The window it opens has \
         no tab strip and a WM class of its own, so a Hyprland rule can target \
         it: `class:oma-browse-app-<host>`.",
    )
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
    /// How far along, as a whole percent. Only while it is still running --
    /// for anything that has ended, `state` is the answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    percent: Option<u8>,
    /// Bytes written so far. Zero for anything from a previous session.
    #[serde(skip_serializing_if = "is_zero")]
    bytes: u64,
}

/// `serde` wants a path, not a closure, and `u64::eq(&0)` is not one.
fn is_zero(n: &u64) -> bool {
    *n == 0
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
        percent: d.percent(),
        bytes: d.bytes,
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

#[derive(JsonSchema, Serialize)]
struct Found {
    /// What was searched for.
    text: String,
    /// How many matches. Omitted when WebKit did not answer in time, which is
    /// not the same as none -- the highlighting happened either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    matches: Option<u32>,
}

fn find_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let search = CommandDef::typed::<FindArgs, FindOptionsArg, (), Found, _, _>(
        "text",
        move |ctx: TypedContext<FindArgs, FindOptionsArg, ()>| {
            let state = s.clone();
            async move {
                let Some(text) = ctx.options.text.or(ctx.args.text) else {
                    return TypedResult::error("usage", "text to find is required".to_string());
                };
                match crate::tabs::find(&state, crate::tabs::FindAction::Search(text.clone())).await
                {
                    Ok(matches) => TypedResult::ok(Found { text, matches }),
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description(
        "Find text on the page, highlighting every match and saying how many \
         there were.",
    )
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
                        Ok(_) => TypedResult::ok(Acted { ok: true }),
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
// permission
// ---------------------------------------------------------------------------

#[derive(JsonSchema, Serialize)]
struct Decided {
    origin: String,
    kinds: Vec<String>,
    allowed: bool,
}

#[derive(JsonSchema, Serialize)]
struct PermissionForgotten {
    origin: String,
    /// How many decisions went. Zero is not an error: forgetting something you
    /// never decided is the state you asked for.
    forgotten: usize,
}

#[derive(JsonSchema, Serialize)]
struct GrantList {
    entries: Vec<GrantRow>,
}

#[derive(JsonSchema, Serialize)]
struct GrantRow {
    origin: String,
    kind: String,
    allowed: bool,
}

#[derive(Deserialize, incurs::Args)]
struct PermissionArgs {
    /// The site, as `https://host[:port]`.
    origin: Option<String>,
    /// What it may do: camera, microphone, screen-share, geolocation,
    /// notifications, device-info, protected-media.
    kind: Option<String>,
}

#[derive(Deserialize, incurs::Args)]
struct DecideArgs {
    /// `allow` or `deny`.
    verdict: Option<String>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct DecideOptions {
    /// Answer this once without writing the decision down.
    once: bool,
}

/// Turn what someone typed into the origin they meant.
///
/// `https://example.com/some/page` is unambiguous: it has a scheme, and the
/// path is not part of an origin. A bare `example.com` is not, and guessing
/// `https://` outright is wrong often enough to matter -- a dev server is
/// `http://localhost:3000`, and `permission forget localhost:3000` that
/// silently forgets nothing because it went looking for the https one is a
/// command that lies about having worked.
///
/// So a bare host is resolved against what has actually been decided: exactly
/// one match wins, several are reported rather than picked between, and none
/// falls back to `https://`, which is right for pre-authorising a site you have
/// not visited yet.
fn resolve_origin(known: &[String], raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("which site? Give an origin, like https://example.com".to_string());
    }
    if let Some(origin) = crate::permissions::origin_of(raw) {
        return Ok(origin);
    }

    // Not a URL, so it is a host and maybe a port. Whose is it?
    let matches: Vec<&String> = known
        .iter()
        .filter(|origin| origin.split_once("//").is_some_and(|(_, rest)| rest == raw))
        .collect();
    match matches.as_slice() {
        [only] => Ok((*only).clone()),
        [] => crate::permissions::origin_of(&format!("https://{raw}"))
            .ok_or_else(|| format!("{raw:?} is not a site; try https://{raw}")),
        several => Err(format!(
            "{raw} is ambiguous -- did you mean {}?",
            several.iter().map(|o| o.as_str()).collect::<Vec<_>>().join(" or ")
        )),
    }
}

/// The origins something has already been decided about.
fn known_origins(state: &Arc<AppState>) -> Vec<String> {
    state
        .permissions
        .lock()
        .map(|store| store.entries().iter().map(|g| g.origin.clone()).collect())
        .unwrap_or_default()
}

fn permission_group(state: Arc<AppState>) -> Cli {
    /// Both `allow` and `deny` are the same command with the verdict flipped.
    fn set_one(state: Arc<AppState>, allow: bool) -> CommandDef {
        let verb = if allow { "allow" } else { "deny" };
        CommandDef::typed::<PermissionArgs, NoOptions, (), Decided, _, _>(
            verb,
            move |ctx: TypedContext<PermissionArgs, NoOptions, ()>| {
                let state = state.clone();
                async move {
                    let known = known_origins(&state);
                    let origin = match resolve_origin(
                        &known,
                        ctx.args.origin.as_deref().unwrap_or_default(),
                    ) {
                        Ok(origin) => origin,
                        Err(e) => return TypedResult::error("usage", e),
                    };
                    let Some(kind) =
                        ctx.args.kind.as_deref().and_then(crate::permissions::Kind::parse)
                    else {
                        return TypedResult::error(
                            "usage",
                            format!("expected one of {}", kind_names()),
                        );
                    };
                    let Ok(mut store) = state.permissions.lock() else {
                        return TypedResult::error(
                            "state",
                            "the permission store is wedged".to_string(),
                        );
                    };
                    store.set(&origin, kind, allow, crate::history::now());
                    TypedResult::ok(Decided {
                        origin,
                        kinds: vec![kind.as_str().to_string()],
                        allowed: allow,
                    })
                }
            },
        )
        .description(if allow {
            "Let a site use your camera, microphone, location or screen"
        } else {
            "Refuse a site the camera, microphone, location or screen"
        })
        .done()
    }

    let allow = set_one(state.clone(), true);
    let deny = set_one(state.clone(), false);

    let s = state.clone();
    let list = CommandDef::typed::<NoArgs, NoOptions, (), GrantList, _, _>(
        "list",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let Ok(store) = state.permissions.lock() else {
                    return TypedResult::error(
                        "state",
                        "the permission store is wedged".to_string(),
                    );
                };
                TypedResult::ok(GrantList {
                    entries: store
                        .entries()
                        .iter()
                        .map(|g| GrantRow {
                            origin: g.origin.clone(),
                            kind: g.kind.as_str().to_string(),
                            allowed: g.allow,
                        })
                        .collect(),
                })
            }
        },
    )
    .description("What each site has been allowed or refused")
    .done();

    let s = state.clone();
    let forget = CommandDef::typed::<PermissionArgs, NoOptions, (), PermissionForgotten, _, _>(
        "forget",
        move |ctx: TypedContext<PermissionArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let known = known_origins(&state);
                let origin =
                    match resolve_origin(&known, ctx.args.origin.as_deref().unwrap_or_default()) {
                        Ok(origin) => origin,
                        Err(e) => return TypedResult::error("usage", e),
                    };
                // No kind means the whole site, which is what "forget about
                // them" means when you say it out loud.
                let kind = match ctx.args.kind.as_deref() {
                    None => None,
                    Some(raw) => match crate::permissions::Kind::parse(raw) {
                        Some(kind) => Some(kind),
                        None => {
                            return TypedResult::error(
                                "usage",
                                format!("expected one of {}", kind_names()),
                            );
                        }
                    },
                };
                let Ok(mut store) = state.permissions.lock() else {
                    return TypedResult::error(
                        "state",
                        "the permission store is wedged".to_string(),
                    );
                };
                let forgotten = store.forget(&origin, kind);
                TypedResult::ok(PermissionForgotten { origin, forgotten })
            }
        },
    )
    .description("Un-decide: the site will be asked about again")
    .destructive(true)
    .done();

    let s = state;
    let decide =
        CommandDef::typed::<DecideArgs, DecideOptions, (), Decided, _, _>(
            "decide",
            move |ctx: TypedContext<DecideArgs, DecideOptions, ()>| {
                let state = s.clone();
                async move {
                    decide_pending(&state, ctx.args.verdict.as_deref(), ctx.options.once).await
                }
            },
        )
        .description("Answer the site that is waiting on you")
        .done();

    Cli::create("permission")
        .description("What each site may do")
        .command("allow", allow)
        .command("deny", deny)
        .command("list", list)
        .command("forget", forget)
        .command("decide", decide)
}

fn kind_names() -> String {
    crate::permissions::Kind::ALL.map(|k| k.as_str()).join(", ")
}

/// Answer whatever is at the head of the queue.
///
/// With no verdict this returns a *usage* error rather than a failure, and that
/// is the whole mechanism behind the prompt: the palette treats a usage error as
/// "ask for this argument" and shows the message as the question (see
/// `ui::wants_argument`). So the same command both raises the question and takes
/// the answer, and there is no second code path for the GUI.
async fn decide_pending(
    state: &Arc<AppState>,
    verdict: Option<&str>,
    once: bool,
) -> TypedResult<Decided> {
    let pending = match state.asked.lock() {
        Ok(queue) => queue.front().cloned(),
        Err(_) => return TypedResult::error("state", "the permission queue is wedged".to_string()),
    };
    let Some(pending) = pending else {
        return TypedResult::error("state", "nothing is waiting on a decision".to_string());
    };

    let allow = match verdict.map(|v| v.trim().to_ascii_lowercase()) {
        None => return TypedResult::error("usage", pending.question()),
        Some(v) if ["allow", "yes", "y", "ok"].contains(&v.as_str()) => true,
        Some(v) if ["deny", "no", "n"].contains(&v.as_str()) => false,
        // Also a usage error, so a typo re-asks rather than dropping the
        // question on the floor with the site still waiting.
        Some(_) => {
            return TypedResult::error(
                "usage",
                format!("{} -- type allow or deny", pending.question()),
            );
        }
    };

    if !once && let Ok(mut store) = state.permissions.lock() {
        let now = crate::history::now();
        for kind in &pending.kinds {
            store.set(&pending.origin, *kind, allow, now);
        }
    }
    if let Ok(mut queue) = state.asked.lock() {
        queue.retain(|p| p.id != pending.id);
    }
    match crate::policy::settle(state, pending.id, allow) {
        Ok(()) => TypedResult::ok(Decided {
            origin: pending.origin,
            kinds: pending.kinds.iter().map(|k| k.as_str().to_string()).collect(),
            allowed: allow,
        }),
        Err(e) => TypedResult::error("webview", format!("{e:#}")),
    }
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

#[derive(JsonSchema, Serialize)]
struct Trusted {
    host: String,
    /// The page that was refused, now reloading.
    url: String,
}

#[derive(Deserialize, incurs::Args)]
struct TrustArgs {
    /// The host to trust. Defaults to the one that was just refused.
    host: Option<String>,
}

/// Trust the certificate of a host that was refused, and go back to the page.
///
/// Defaults to the refusal that is on screen, the way `bookmark add` defaults
/// to the page you are looking at -- typing a hostname you have just been shown
/// is work the browser can do for you, and mistyping it here would trust the
/// wrong name.
async fn trust_certificate(state: &Arc<AppState>, host: Option<String>) -> TypedResult<Trusted> {
    let refused = state.tls.lock().ok().and_then(|slot| slot.clone());
    let host = match host.map(|h| h.trim().to_string()).filter(|h| !h.is_empty()) {
        Some(host) => host,
        None => match refused.as_ref() {
            Some(refused) => refused.host.clone(),
            None => {
                return TypedResult::error(
                    "usage",
                    "no certificate has been refused, so there is nothing to trust; \
                     name a host to trust it anyway"
                        .to_string(),
                );
            }
        },
    };

    if let Err(e) = crate::policy::trust_host(state, &host) {
        return TypedResult::error("webview", format!("{e:#}"));
    }

    // Only reload if this is the page that was refused; trusting some other
    // host should not navigate the tab you are looking at.
    let url = match refused.filter(|r| r.host == host) {
        Some(refused) => {
            if let Err(e) = crate::tabs::navigate(state, &refused.uri).await {
                return TypedResult::error("webview", format!("{e:#}"));
            }
            refused.uri
        }
        None => String::new(),
    };
    TypedResult::ok(Trusted { host, url })
}

#[derive(JsonSchema, Serialize)]
struct LoggedIn {
    host: String,
    user: String,
}

#[derive(Deserialize, incurs::Args)]
struct LoginArgs {
    /// The username.
    user: Option<String>,
    /// The password.
    password: Option<String>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct LoginOptions {
    /// The username.
    user: Option<String>,
    /// The password. As an option so it can come from a variable rather than
    /// the shell history.
    password: Option<String>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct ReloadOptions {
    /// Ignore everything already cached.
    hard: bool,
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

    let s = state.clone();
    let trust = CommandDef::typed::<TrustArgs, NoOptions, (), Trusted, _, _>(
        "trust",
        move |ctx: TypedContext<TrustArgs, NoOptions, ()>| {
            let state = s.clone();
            async move { trust_certificate(&state, ctx.args.host).await }
        },
    )
    .description("Accept the certificate of the host that was just refused, and reload")
    // Not destructive -- nothing is lost -- but it is the one command here that
    // makes the browser less careful, and the flag is what puts it behind
    // `call_write_tool` for an agent rather than in the read-only set.
    .destructive(true)
    .done();

    let s = state.clone();
    let login = CommandDef::typed::<LoginArgs, LoginOptions, (), LoggedIn, _, _>(
        "login",
        move |ctx: TypedContext<LoginArgs, LoginOptions, ()>| {
            let state = s.clone();
            async move {
                let user = ctx.options.user.or(ctx.args.user).unwrap_or_default();
                let password = ctx.options.password.or(ctx.args.password).unwrap_or_default();
                if user.is_empty() {
                    return TypedResult::error("usage", "which user?".to_string());
                }
                let Some(challenge) = state.login.lock().ok().and_then(|slot| slot.clone()) else {
                    return TypedResult::error(
                        "state",
                        "nothing is asking for a login".to_string(),
                    );
                };

                // Keep it, then load the page again. The credential is *not*
                // handed to the request that is waiting: that request belongs
                // to a load this tab already navigated away from to show the
                // interstitial, so answering it would satisfy a connection
                // nobody is watching and leave the page where it is. The fresh
                // load raises its own challenge, and that one is answered from
                // here without anybody being asked twice.
                crate::policy::remember_login(&state, &challenge.key(), &user, &password);
                if let Err(e) = crate::policy::drop_challenge(&state) {
                    tracing::debug!(error = %e, "the old challenge was already gone");
                }
                if let Ok(mut slot) = state.login.lock() {
                    *slot = None;
                }
                if let Err(e) = crate::tabs::navigate(&state, &challenge.uri).await {
                    return TypedResult::error("webview", format!("{e:#}"));
                }
                TypedResult::ok(LoggedIn { host: challenge.host, user })
            }
        },
    )
    .description("Answer a site asking for a username and a password")
    .destructive(true)
    .done();

    let mut group = Cli::create("nav")
        .description("Move the active tab around")
        .command("go", go)
        .command("home", home)
        .command("trust", trust)
        .command("login", login);

    let s = state.clone();
    let reload = CommandDef::typed::<NoArgs, ReloadOptions, (), Acted, _, _>(
        "reload",
        move |ctx: TypedContext<NoArgs, ReloadOptions, ()>| {
            let state = s.clone();
            async move {
                let action = if ctx.options.hard {
                    crate::tabs::HistoryAction::HardReload
                } else {
                    crate::tabs::HistoryAction::Reload
                };
                match crate::tabs::history(&state, action).await {
                    Ok(()) => TypedResult::ok(Acted { ok: true }),
                    Err(e) => TypedResult::error("webview", format!("{e:#}")),
                }
            }
        },
    )
    .description(
        "Reload the active tab. `--hard` throws the cache away first, which is \
         the difference between seeing the bundle you just built and seeing the \
         one from four minutes ago.",
    )
    .done();
    group = group.command("reload", reload);

    for (name, action, blurb) in [
        ("back", crate::tabs::HistoryAction::Back, "Go back in history"),
        ("forward", crate::tabs::HistoryAction::Forward, "Go forward in history"),
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

// ---------------------------------------------------------------------------
// page console / page network
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct ConsoleOptions {
    /// Only lines this loud or louder: `debug`, `log`, `info`, `warn`, `error`.
    level: Option<String>,
    /// At most this many, most recent last.
    limit: Option<usize>,
    /// Only what is newer than this sequence number; pass back the `next` from
    /// the previous answer.
    since: Option<u64>,
    /// Forget what this tab has logged.
    clear: bool,
    /// Keep printing lines as the page logs them, until interrupted.
    ///
    /// Read by the CLI before the command is forwarded -- a command answers
    /// once, and following is a conversation. See `crate::follow`.
    follow: bool,
}

#[derive(JsonSchema, Serialize)]
struct Console {
    /// The tab that was asked.
    url: String,
    /// Give this back as `--since` to get only what has happened since.
    next: u64,
    lines: Vec<crate::inspect::Line>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct NetworkOptions {
    /// At most this many, most recent last.
    limit: Option<usize>,
    /// Only what is newer than this sequence number.
    since: Option<u64>,
    /// Only requests that failed or answered 400 and up.
    failed: bool,
    /// Answer with a HAR 1.2 log instead of a list.
    har: bool,
    /// Write the HAR here rather than returning it.
    path: Option<String>,
    /// Forget what this tab has fetched.
    clear: bool,
    /// Keep printing requests as the page makes them, until interrupted.
    follow: bool,
}

#[derive(JsonSchema, Serialize)]
struct Network {
    url: String,
    next: u64,
    /// Omitted when `--har` asked for a HAR instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    requests: Option<Vec<crate::inspect::Exchange>>,
    /// The HAR itself, when `--har` was given without a path.
    #[serde(skip_serializing_if = "Option::is_none")]
    har: Option<serde_json::Value>,
    /// Where the HAR was written, when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

/// Which tab a console or network question is about: the active one, by the
/// label the taps file their entries under.
async fn active_label(state: &Arc<AppState>) -> Option<String> {
    state.tabs.read().await.active_label()
}

// ---------------------------------------------------------------------------
// page markdown / page text
// ---------------------------------------------------------------------------

/// The reader, evaluated in place rather than injected into every page.
const EXTRACT: &str = include_str!("extract.js");
/// The other half of reader mode: what to do with what the reader found.
const READER: &str = include_str!("reader.js");

#[derive(Deserialize, serde::Serialize)]
struct Extracted {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    markdown: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    html: String,
}

#[derive(JsonSchema, Serialize)]
struct Reading {
    url: String,
    title: String,
    /// How many characters of prose. Zero means the reader found nothing, which
    /// on a page that is all script is the honest answer.
    chars: usize,
    /// The prose itself, omitted when it was written to a file instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

/// Run the reader and hand back what it found.
async fn read_page(state: &Arc<AppState>) -> Result<Extracted, String> {
    let raw = crate::tabs::eval(state, EXTRACT).await.map_err(|e| format!("{e:#}"))?;
    // `eval` answers with JSON, and the script's own answer is a JSON string --
    // so what arrives is a quoted, escaped document that has to come out of its
    // wrapper before it parses.
    let inner = serde_json::from_str::<String>(&raw).unwrap_or(raw);
    serde_json::from_str::<Extracted>(&inner)
        .map_err(|e| format!("the reader answered with something unreadable: {e}"))
}

/// `page markdown` and `page text` differ in one field, so they are one
/// function with a flag rather than two that drift.
fn reader(
    state: Arc<AppState>,
    name: &'static str,
    markdown: bool,
    description: &'static str,
) -> CommandDef {
    CommandDef::typed::<NoArgs, SourceOptions, (), Reading, _, _>(
        name,
        move |ctx: TypedContext<NoArgs, SourceOptions, ()>| {
            let state = state.clone();
            async move {
                let found = match read_page(&state).await {
                    Ok(found) => found,
                    Err(e) => return TypedResult::error("webview", e),
                };
                let content = if markdown { found.markdown } else { found.text };
                let chars = content.chars().count();

                let wants_file = ctx.options.open || ctx.options.path.is_some();
                if !wants_file {
                    return TypedResult::ok(Reading {
                        url: found.url,
                        title: found.title,
                        chars,
                        content: Some(content),
                        path: None,
                    });
                }

                let extension = if markdown { "md" } else { "txt" };
                let path = match crate::shot::scratch_file(ctx.options.path, name, extension) {
                    Ok(path) => path,
                    Err(e) => return TypedResult::error("path", format!("{e:#}")),
                };
                if let Err(e) = std::fs::write(&path, &content) {
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
                TypedResult::ok(Reading {
                    url: found.url,
                    title: found.title,
                    chars,
                    content: None,
                    path: Some(path.display().to_string()),
                })
            }
        },
    )
    .description(description)
    .done()
}

// ---------------------------------------------------------------------------
// page click / fill / wait
// ---------------------------------------------------------------------------

#[derive(Deserialize, incurs::Args)]
struct SelectorArgs {
    /// A CSS selector for the element to act on.
    selector: Option<String>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct SelectorOptions {
    /// Same as the positional, for callers whose transport cannot carry one.
    selector: Option<String>,
    /// Which match, when the selector finds several. Zero-based.
    nth: Option<usize>,
    /// How long to wait for the element to turn up, in milliseconds.
    timeout: Option<u64>,
}

#[derive(JsonSchema, Serialize)]
struct Interacted {
    selector: String,
    /// How many elements the selector matched.
    matched: usize,
    /// How long the element took to turn up, in milliseconds.
    waited: u64,
}

#[derive(Deserialize, incurs::Args)]
struct FillArgs {
    /// A CSS selector for the field to fill.
    selector: Option<String>,
    /// What to type into it.
    text: Option<String>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct FillOptions {
    selector: Option<String>,
    /// What to type into it.
    text: Option<String>,
    nth: Option<usize>,
    timeout: Option<u64>,
    /// Add to what is already there instead of replacing it.
    append: bool,
    /// Take the text from a password manager instead: `rbw`, `op` or `pass`.
    ///
    /// The entry is this page's host unless `--entry` says otherwise, and the
    /// secret never appears in the answer, in a log, or on a command line.
    from: Option<String>,
    /// Which entry to read, when it is not the page's host.
    entry: Option<String>,
    /// `password`, the default, or `username`.
    field: Option<String>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct WaitOptions {
    /// Wait for an element matching this selector to exist.
    selector: Option<String>,
    /// Wait for this text to appear anywhere on the page.
    text: Option<String>,
    /// Wait for the page to finish loading and stop fetching. The default when
    /// nothing else is asked for.
    idle: bool,
    /// Give up after this many milliseconds. Ten seconds by default.
    timeout: Option<u64>,
}

#[derive(JsonSchema, Serialize)]
struct Waited {
    /// What was waited for, in words.
    r#for: String,
    /// How long it took, in milliseconds.
    ms: u64,
}

/// How long a scripting verb waits for its element by default.
const WAIT_MS: u64 = 10_000;
/// How often it looks again while waiting. Short enough to feel immediate,
/// long enough not to be a spin loop against a webview.
const POLL_MS: u64 = 50;

/// Poll the page until `js` answers with something other than `null`.
///
/// The verbs are `page eval` underneath, which is what the plan said they would
/// be -- the value here is not the evaluation, it is the waiting, the retrying
/// and the one error message when it never happens.
async fn until(
    state: &Arc<AppState>,
    js: &str,
    timeout: std::time::Duration,
) -> Result<(String, u64), String> {
    let began = std::time::Instant::now();
    let mut last = String::new();
    loop {
        match crate::tabs::eval(state, js).await {
            Ok(raw) => {
                let trimmed = raw.trim();
                if trimmed != "null" && !trimmed.is_empty() {
                    return Ok((raw, began.elapsed().as_millis() as u64));
                }
                last.clear();
            }
            // A navigation in flight tears the old document down mid-question.
            // That is a reason to look again, not a reason to give up.
            Err(e) => last = format!("{e:#}"),
        }
        if began.elapsed() >= timeout {
            return Err(if last.is_empty() {
                format!("still not there after {}ms", timeout.as_millis())
            } else {
                format!("still not there after {}ms; last answer was {last}", timeout.as_millis())
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(POLL_MS)).await;
    }
}

/// A JS string literal for a value that came from a person's shell.
fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// Wait until the page has stopped fetching.
///
/// "Stopped" is two conditions, not one: nothing still in flight, and nothing
/// newly started for half a second. The second is what makes this useful on a
/// single-page application, where the document finished loading long ago and the
/// thing worth waiting for is the burst of `fetch` calls that follows a click.
///
/// A page holding a connection open forever -- an `EventSource`, a long poll --
/// never goes idle by this definition, which is why the timeout is not optional.
async fn idle(state: &Arc<AppState>, budget: std::time::Duration) -> Result<(), String> {
    const QUIET_MS: u64 = 500;
    // A request still pending after this long is not the page loading. It is a
    // beacon, a long-poll, an event stream, or a request the last navigation
    // walked away from -- and none of those ever finish, so counting them means
    // never returning.
    //
    // Both halves of that were real. `network_of` is the tab's whole history,
    // not the current document's, so a YouTube embed's telemetry POST from a
    // page we navigated away from 47 seconds ago was still "in flight" and
    // still blocking: bare `page wait` could not succeed on any tab that had
    // ever left a page with a request outstanding, which is nearly all of them.
    // The live half is the same story on one page -- analytics beacons do not
    // complete on purpose, which is why Playwright deprecated `networkidle`.
    //
    // The cost of the threshold is a genuinely slow asset: a response taking
    // longer than this stops counting as loading, and `page wait` may return
    // while it is still coming. That is the right way round -- a caller who
    // needs that asset should wait for what it produces, with `--selector` or
    // `--text`, which is exact where this is a heuristic.
    const STALL_MS: u64 = 2_000;

    let label = active_label(state).await.ok_or("there is no active tab")?;
    let began = std::time::Instant::now();
    loop {
        let now = crate::inspect::now_ms();
        let (loading, last, stalled) = {
            let Ok(inspector) = state.inspector.lock() else {
                return Err("the network log is wedged".to_string());
            };
            let requests = inspector.network_of(&label);
            let mut loading = 0usize;
            let mut stalled = 0usize;
            let mut last = 0u64;
            for e in &requests {
                let done = e.status != 0 || e.failed.is_some();
                // Finished requests count their end, so a burst that lands
                // quickly is quiet from the moment the last one lands rather
                // than from the moment the last one started.
                last = last.max(if done { e.at.saturating_add(e.ms) } else { e.at });
                if !done {
                    if now.saturating_sub(e.at) < STALL_MS {
                        loading += 1;
                    } else {
                        stalled += 1;
                    }
                }
            }
            (loading, last, stalled)
        };
        if loading == 0 && now.saturating_sub(last) >= QUIET_MS {
            return Ok(());
        }
        if began.elapsed() >= budget {
            // Says which kind, because the two want different answers: still
            // loading means raise `--timeout`, all stalled means the page has
            // something open that will never close and `--selector` is the tool.
            return Err(format!(
                "{loading} request(s) still loading after {}ms ({stalled} stalled and ignored)",
                budget.as_millis()
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(POLL_MS)).await;
    }
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
                // Before the webview is asked for anything: a path this process
                // cannot place is the caller's mistake to hear about as one,
                // rather than as a rendering failure.
                let path = match crate::shot::destination(&state, opts.path) {
                    Ok(path) => path,
                    Err(e) => return TypedResult::error("path", format!("{e:#}")),
                };
                match crate::shot::capture(&state, path, opts.full, !opts.opaque).await {
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
                // Against the *caller's* directory, not the browser's: a PDF
                // written somewhere the person who asked cannot name is lost.
                let to = match to.as_deref().map(crate::paths::resolve).transpose() {
                    Ok(to) => to,
                    Err(e) => return TypedResult::error("path", format!("{e:#}")),
                };
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
                    Err(e) => return TypedResult::error("path", format!("{e:#}")),
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

    let s = state.clone();
    let console = CommandDef::typed::<NoArgs, ConsoleOptions, (), Console, _, _>(
        "console",
        move |ctx: TypedContext<NoArgs, ConsoleOptions, ()>| {
            let state = s.clone();
            async move {
                let Some(label) = active_label(&state).await else {
                    return TypedResult::error("no_tab", "there is no active tab".to_string());
                };
                let floor = match ctx.options.level.as_deref() {
                    None => crate::inspect::Level::Debug,
                    Some(raw) => match crate::inspect::Level::parse(raw) {
                        Some(level) => level,
                        None => {
                            let known: Vec<&str> =
                                crate::inspect::Level::ALL.iter().map(|l| l.as_str()).collect();
                            return TypedResult::error(
                                "usage",
                                format!("unknown level {raw:?}; use {}", known.join(", ")),
                            );
                        }
                    },
                };
                let url = here(&state).await.map(|(u, _)| u).unwrap_or_default();
                // Collect whatever the page has piled up since the last time
                // anybody asked. `--clear` drains first too, so that clearing
                // empties what is waiting in the page rather than letting it
                // arrive a moment later.
                crate::inspect::drain(&state, &label).await;

                let Ok(mut inspector) = state.inspector.lock() else {
                    return TypedResult::error("state", "the console log is wedged".to_string());
                };
                if ctx.options.clear {
                    inspector.clear_console(&label);
                    return TypedResult::ok(Console { url, next: 0, lines: Vec::new() });
                }
                let since = ctx.options.since.unwrap_or(0);
                let mut lines: Vec<_> = inspector
                    .console_of(&label)
                    .into_iter()
                    .filter(|line| line.seq >= since && line.level >= floor)
                    .collect();
                drop(inspector);

                if let Some(limit) = ctx.options.limit
                    && lines.len() > limit
                {
                    lines.drain(..lines.len() - limit);
                }
                let next = lines.last().map_or(since, |line| line.seq + 1);
                TypedResult::ok(Console { url, next, lines })
            }
        },
    )
    .description(
        "What the active tab has logged. Every `console.*` call, every uncaught \
         error and every unhandled rejection, from the moment the tab opened -- \
         so `oma-browse page console --level error` is the whole of what F12 was \
         for. `--follow` keeps printing as the page logs.",
    )
    .done();

    let s = state.clone();
    let network = CommandDef::typed::<NoArgs, NetworkOptions, (), Network, _, _>(
        "network",
        move |ctx: TypedContext<NoArgs, NetworkOptions, ()>| {
            let state = s.clone();
            async move {
                let Some(label) = active_label(&state).await else {
                    return TypedResult::error("no_tab", "there is no active tab".to_string());
                };
                let url = here(&state).await.map(|(u, _)| u).unwrap_or_default();

                let Ok(mut inspector) = state.inspector.lock() else {
                    return TypedResult::error("state", "the network log is wedged".to_string());
                };
                if ctx.options.clear {
                    inspector.clear_network(&label);
                    return TypedResult::ok(Network {
                        url,
                        next: 0,
                        requests: Some(Vec::new()),
                        har: None,
                        path: None,
                    });
                }
                let since = ctx.options.since.unwrap_or(0);
                let mut requests: Vec<_> = inspector
                    .network_of(&label)
                    .into_iter()
                    .filter(|e| e.seq >= since)
                    .filter(|e| !ctx.options.failed || e.failed.is_some() || e.status >= 400)
                    .collect();
                drop(inspector);

                if let Some(limit) = ctx.options.limit
                    && requests.len() > limit
                {
                    requests.drain(..requests.len() - limit);
                }
                let next = requests.last().map_or(since, |e| e.seq + 1);

                if !ctx.options.har {
                    return TypedResult::ok(Network {
                        url,
                        next,
                        requests: Some(requests),
                        har: None,
                        path: None,
                    });
                }

                let har = crate::inspect::har(&url, &requests);
                let Some(asked) = ctx.options.path else {
                    return TypedResult::ok(Network {
                        url,
                        next,
                        requests: None,
                        har: Some(har),
                        path: None,
                    });
                };
                let path = match crate::shot::scratch_file(Some(asked), "network", "har") {
                    Ok(path) => path,
                    Err(e) => return TypedResult::error("path", format!("{e:#}")),
                };
                let body = serde_json::to_string_pretty(&har).unwrap_or_default();
                if let Err(e) = std::fs::write(&path, body) {
                    return TypedResult::error(
                        "io",
                        format!("could not write {}: {e}", path.display()),
                    );
                }
                TypedResult::ok(Network {
                    url,
                    next,
                    requests: None,
                    har: None,
                    path: Some(path.display().to_string()),
                })
            }
        },
    )
    .description(
        "Every request the active tab has made -- WebKit's own view, so it \
         includes the document, the stylesheets and the images, not only what \
         `fetch` was involved in. `--failed` narrows it to what went wrong; \
         `--har` writes a HAR 1.2 log for anything that reads one.",
    )
    .done();

    let s = state.clone();
    let click = CommandDef::typed::<SelectorArgs, SelectorOptions, (), Interacted, _, _>(
        "click",
        move |ctx: TypedContext<SelectorArgs, SelectorOptions, ()>| {
            let state = s.clone();
            async move {
                let Some(selector) = ctx.options.selector.or(ctx.args.selector) else {
                    return TypedResult::error("usage", "a CSS selector is required".to_string());
                };
                let nth = ctx.options.nth.unwrap_or(0);
                // The mouse events around `click()` are not ceremony: plenty of
                // menus and dropdowns open on `mousedown` and never see a
                // `click` at all, and an automation that only clicks cannot open
                // them.
                let js = format!(
                    r#"(function(){{
                        var found = document.querySelectorAll({selector});
                        if (found.length <= {nth}) return null;
                        var el = found[{nth}];
                        el.scrollIntoView({{block: "center", inline: "center"}});
                        var box = el.getBoundingClientRect();
                        var where = {{
                            bubbles: true, cancelable: true, view: window,
                            clientX: box.left + box.width / 2,
                            clientY: box.top + box.height / 2
                        }};
                        el.dispatchEvent(new MouseEvent("mousedown", where));
                        el.dispatchEvent(new MouseEvent("mouseup", where));
                        if (typeof el.click === "function") el.click();
                        else el.dispatchEvent(new MouseEvent("click", where));
                        return found.length;
                    }})()"#,
                    selector = js_string(&selector)
                );
                let timeout =
                    std::time::Duration::from_millis(ctx.options.timeout.unwrap_or(WAIT_MS).max(1));
                match until(&state, &js, timeout).await {
                    Ok((raw, waited)) => TypedResult::ok(Interacted {
                        selector,
                        matched: raw.trim().parse().unwrap_or(1),
                        waited,
                    }),
                    Err(e) => TypedResult::error("no_match", format!("{selector}: {e}")),
                }
            }
        },
    )
    .description(
        "Click what a CSS selector names, waiting up to ten seconds for it to \
         turn up. `--nth` picks among several matches.",
    )
    .done();

    let s = state.clone();
    let fill = CommandDef::typed::<FillArgs, FillOptions, (), Interacted, _, _>(
        "fill",
        move |ctx: TypedContext<FillArgs, FillOptions, ()>| {
            let state = s.clone();
            async move {
                let Some(selector) = ctx.options.selector.or(ctx.args.selector) else {
                    return TypedResult::error("usage", "a CSS selector is required".to_string());
                };
                // Either the caller typed it or a password manager knows it.
                // Never both: a command that silently preferred one over the
                // other would be a command that types the wrong secret.
                let typed = ctx.options.text.or(ctx.args.text);
                let text = match (typed, ctx.options.from.as_deref()) {
                    (Some(_), Some(_)) => {
                        return TypedResult::error(
                            "usage",
                            "give text or --from, not both".to_string(),
                        );
                    }
                    (Some(text), None) => text,
                    (None, Some(raw)) => {
                        let Some(vault) = crate::vault::Vault::parse(raw) else {
                            let known: Vec<&str> =
                                crate::vault::Vault::ALL.iter().map(|v| v.as_str()).collect();
                            return TypedResult::error(
                                "usage",
                                format!(
                                    "unknown password manager {raw:?}; use {}",
                                    known.join(", ")
                                ),
                            );
                        };
                        let field = match ctx.options.field.as_deref() {
                            None => crate::vault::Field::Password,
                            Some(raw) => match crate::vault::Field::parse(raw) {
                                Some(field) => field,
                                None => {
                                    return TypedResult::error(
                                        "usage",
                                        format!("unknown field {raw:?}; use password or username"),
                                    );
                                }
                            },
                        };
                        let entry = match ctx.options.entry {
                            Some(entry) => entry,
                            None => {
                                let url = here(&state).await.map(|(u, _)| u).unwrap_or_default();
                                match crate::vault::entry_for(&url) {
                                    Some(entry) => entry,
                                    None => {
                                        return TypedResult::error(
                                            "usage",
                                            "this page has no host to look up; name one with \
                                             --entry"
                                                .to_string(),
                                        );
                                    }
                                }
                            }
                        };
                        match crate::vault::get(vault, &entry, field).await {
                            Ok(secret) => secret,
                            Err(e) => return TypedResult::error("vault", format!("{e:#}")),
                        }
                    }
                    (None, None) => {
                        return TypedResult::error(
                            "usage",
                            format!("what should go in {selector}?"),
                        );
                    }
                };
                let nth = ctx.options.nth.unwrap_or(0);
                // Assigning `el.value` directly is invisible to React, which
                // tracks the property on the prototype and treats a value it did
                // not see set as a value that did not change. Going through the
                // native setter is what makes the framework believe it.
                let js = format!(
                    r#"(function(){{
                        var found = document.querySelectorAll({selector});
                        if (found.length <= {nth}) return null;
                        var el = found[{nth}];
                        el.focus();
                        if (el.isContentEditable) {{
                            el.textContent = {append} ? (el.textContent || "") + {text} : {text};
                        }} else {{
                            var proto = (typeof HTMLTextAreaElement !== "undefined"
                                && el instanceof HTMLTextAreaElement)
                                ? HTMLTextAreaElement.prototype
                                : HTMLInputElement.prototype;
                            var slot = Object.getOwnPropertyDescriptor(proto, "value");
                            var next = {append} ? (el.value || "") + {text} : {text};
                            if (slot && slot.set) slot.set.call(el, next);
                            else el.value = next;
                        }}
                        el.dispatchEvent(new Event("input", {{bubbles: true}}));
                        el.dispatchEvent(new Event("change", {{bubbles: true}}));
                        return found.length;
                    }})()"#,
                    selector = js_string(&selector),
                    text = js_string(&text),
                    append = ctx.options.append
                );
                let timeout =
                    std::time::Duration::from_millis(ctx.options.timeout.unwrap_or(WAIT_MS).max(1));
                match until(&state, &js, timeout).await {
                    Ok((raw, waited)) => TypedResult::ok(Interacted {
                        selector,
                        matched: raw.trim().parse().unwrap_or(1),
                        waited,
                    }),
                    Err(e) => TypedResult::error("no_match", format!("{selector}: {e}")),
                }
            }
        },
    )
    .description(
        "Type into what a CSS selector names -- an input, a textarea, or \
         anything `contenteditable` -- and tell the page it changed the way a \
         real keystroke would.",
    )
    .done();

    let s = state.clone();
    let wait = CommandDef::typed::<NoArgs, WaitOptions, (), Waited, _, _>(
        "wait",
        move |ctx: TypedContext<NoArgs, WaitOptions, ()>| {
            let state = s.clone();
            async move {
                let timeout =
                    std::time::Duration::from_millis(ctx.options.timeout.unwrap_or(WAIT_MS).max(1));
                let began = std::time::Instant::now();

                if let Some(selector) = ctx.options.selector {
                    let js = format!("document.querySelector({}) ? 1 : null", js_string(&selector));
                    return match until(&state, &js, timeout).await {
                        Ok((_, ms)) => TypedResult::ok(Waited { r#for: selector, ms }),
                        Err(e) => TypedResult::error("timeout", format!("{selector}: {e}")),
                    };
                }
                if let Some(text) = ctx.options.text {
                    let js = format!(
                        "(document.body && document.body.innerText.indexOf({}) >= 0) ? 1 : null",
                        js_string(&text)
                    );
                    return match until(&state, &js, timeout).await {
                        Ok((_, ms)) => TypedResult::ok(Waited { r#for: text, ms }),
                        Err(e) => TypedResult::error("timeout", format!("{text:?}: {e}")),
                    };
                }

                // Nothing named: wait for the load to finish and the requests to
                // stop, which is what `--idle` asks for and what somebody who
                // asked for none of the three meant.
                let js = "document.readyState === \"complete\" ? 1 : null";
                if let Err(e) = until(&state, js, timeout).await {
                    return TypedResult::error("timeout", format!("still loading: {e}"));
                }
                match idle(&state, timeout.saturating_sub(began.elapsed())).await {
                    Ok(()) => TypedResult::ok(Waited {
                        r#for: "idle".to_string(),
                        ms: began.elapsed().as_millis() as u64,
                    }),
                    Err(e) => TypedResult::error("timeout", e),
                }
            }
        },
    )
    .description(
        "Wait for the page to be ready: for `--selector` to exist, for `--text` \
         to appear, or -- given neither -- for it to finish loading and stop \
         fetching.",
    )
    .done();

    let markdown = reader(
        state.clone(),
        "markdown",
        true,
        "The article on the active page, as Markdown. Pipe it into `glow`, into \
         a file, or into a model -- it is the page without the navigation, the \
         cookie banner and the script tags.",
    );

    let text =
        reader(state.clone(), "text", false, "The article on the active page, as plain text.");

    let s = state.clone();
    let read = CommandDef::typed::<NoArgs, NoOptions, (), Reading, _, _>(
        "reader",
        move |_ctx: TypedContext<NoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let found = match read_page(&state).await {
                    Ok(found) => found,
                    Err(e) => return TypedResult::error("webview", e),
                };
                if found.html.trim().is_empty() {
                    return TypedResult::error(
                        "no_article",
                        "there is no article on this page to read".to_string(),
                    );
                }
                let chars = found.text.chars().count();
                // The markup goes in as a JSON argument rather than being
                // spliced into the script: it is somebody else's HTML, it is
                // routinely tens of kilobytes of it, and `page eval` builds a
                // JavaScript source string.
                let payload = serde_json::json!({
                    "html": found.html,
                    "title": found.title,
                    "base": found.url,
                });
                let js = format!("({READER})({payload})");
                if let Err(e) = crate::tabs::eval(&state, &js).await {
                    return TypedResult::error("webview", format!("{e:#}"));
                }
                TypedResult::ok(Reading {
                    url: found.url,
                    title: found.title,
                    chars,
                    content: None,
                    path: None,
                })
            }
        },
    )
    .description(
        "Strip the page down to its article and read it in the theme's own \
         colours. The document is replaced rather than restyled, so nothing of \
         the site's layout is left to fight with; `nav reload` puts the page \
         back.",
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
        .command("console", console)
        .command("network", network)
        .command("markdown", markdown)
        .command("text", text)
        .command("reader", read)
        .command("click", click)
        .command("fill", fill)
        .command("wait", wait)
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
    /// Open the window on this Hyprland workspace instead of the one you are
    /// standing on: a number, `name:web`, `special:magic`, `+1`, `empty`.
    /// Ignored, with a warning in the log, on anything that is not Hyprland.
    workspace: Option<String>,
}

#[derive(JsonSchema, Serialize)]
struct Spawned {
    /// The new window's process. Always present when this command succeeded:
    /// pass it straight to `--window`.
    ///
    /// `Option` because `hyprctl` answers `ok` rather than a process id, so on
    /// the `--workspace` path there is no pid to report at the moment the
    /// compositor forks the child. It is filled in by waiting for the window to
    /// answer and seeing which one is new -- so an agent gets a usable pid on
    /// both paths and never has to handle a null here.
    pid: Option<u32>,
    /// The workspace it was placed on, if it was placed at all. Absent means
    /// it opened wherever you were.
    workspace: Option<String>,
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

#[derive(Deserialize, incurs::Args)]
struct ResizeArgs {
    /// `1280x720`.
    size: Option<String>,
}

#[derive(Default, Deserialize, incurs::Options)]
#[serde(default)]
struct ResizeOptions {
    /// Same as the positional.
    size: Option<String>,
    width: Option<f64>,
    height: Option<f64>,
}

#[derive(JsonSchema, Serialize)]
struct Resized {
    /// The size the window actually is now, as the compositor reports it --
    /// not the size that was asked for.
    width: f64,
    height: f64,
    /// Whether the window is the size that was asked for. A tiled window is
    /// sized by the layout whatever anyone requests, and answering `true` there
    /// would be a lie this command used to tell.
    applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

fn window_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let resize = CommandDef::typed::<ResizeArgs, ResizeOptions, (), Resized, _, _>(
        "resize",
        move |ctx: TypedContext<ResizeArgs, ResizeOptions, ()>| {
            let state = s.clone();
            async move {
                let asked = ctx.options.size.or(ctx.args.size);
                let size = match (asked, ctx.options.width, ctx.options.height) {
                    (Some(raw), _, _) => match crate::window::parse_size(&raw) {
                        Some(size) => size,
                        None => {
                            return TypedResult::error(
                                "usage",
                                format!("{raw:?} is not a size; write it as 1280x720"),
                            );
                        }
                    },
                    (None, Some(width), Some(height)) => (width, height),
                    _ => {
                        return TypedResult::error(
                            "usage",
                            "a size is required, as 1280x720 or --width and --height".to_string(),
                        );
                    }
                };
                match crate::window::resize(&state, size.0, size.1) {
                    Ok(placed) => TypedResult::ok(Resized {
                        width: placed.width,
                        height: placed.height,
                        applied: placed.applied,
                        note: placed.note,
                    }),
                    Err(e) => TypedResult::error("window", format!("{e:#}")),
                }
            }
        },
    )
    .description(
        "Resize the window, for checking a layout at a phone's width without \
         leaving the keyboard. On Wayland a client cannot size itself, so this \
         asks the compositor -- and reads the size back, so `applied: false` \
         means it did not happen. A tiled window is sized by the layout \
         whatever anyone asks; float it first.",
    )
    .done();

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
                // No URL means Ctrl-N, which should come up asking where to go
                // rather than landing on the start page with nothing to do.
                let palette = url.is_none();
                let workspace = ctx.options.workspace.clone();
                // Taken before the window is started, so that the Hyprland path
                // -- where the compositor forks the child and there is no pid to
                // report -- can still work out which window is the new one.
                let dir = crate::control::dir_for(&state.config.control);
                let before = crate::control::live_in(&dir);

                let opened = match crate::window::spawn_on(
                    state.incognito(),
                    url,
                    palette,
                    false,
                    workspace.as_deref(),
                ) {
                    Ok(opened) => opened,
                    Err(e) => return TypedResult::error("spawn", format!("{e:#}")),
                };

                // Do not answer until the window will actually take a command.
                // Returning the moment the child is forked is what made this
                // unusable from a script: the pid was real and every command
                // sent to it for the next few seconds was told there was no
                // browser running. See `window::wait_until_ready`.
                let ready = crate::window::wait_until_ready(
                    &dir,
                    opened.pid,
                    &before,
                    crate::window::READY_TIMEOUT,
                )
                .await;
                let Some(pid) = ready else {
                    return TypedResult::error(
                        "slow_start",
                        format!(
                            "the window was started but did not answer within {}s",
                            crate::window::READY_TIMEOUT.as_secs()
                        ),
                    );
                };
                TypedResult::ok(Spawned { pid: Some(pid), workspace: opened.workspace })
            }
        },
    )
    .description(
        "Open another browser window, optionally on a given Hyprland workspace. \
         Answers only once the window will take a command, so the pid it returns \
         is usable immediately -- which is what makes `window new` scriptable.",
    )
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
        .command("resize", resize)
        .command("close", close)
}

#[derive(Deserialize, incurs::Args)]
struct HookArgs {
    /// `install`, `remove`, or `show`. Defaults to `show`.
    action: Option<String>,
}

#[derive(JsonSchema, Serialize)]
struct Hook {
    /// Where the hook lives, installed or not.
    path: String,
    /// Whether it is there now.
    installed: bool,
    /// What the hook runs, when it is installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    runs: Option<String>,
}

fn theme_group(state: Arc<AppState>) -> Cli {
    let hook = CommandDef::typed::<HookArgs, NoOptions, (), Hook, _, _>(
        "hook",
        move |ctx: TypedContext<HookArgs, NoOptions, ()>| async move {
            let path = oma_theme::watch::hook_path();
            let shown = path.display().to_string();
            match ctx.args.action.as_deref().unwrap_or("show") {
                "show" | "status" => TypedResult::ok(Hook {
                    installed: path.exists(),
                    runs: std::fs::read_to_string(&path).ok().map(|s| s.trim().to_string()),
                    path: shown,
                }),
                "install" | "add" => {
                    let exe = match std::env::current_exe() {
                        Ok(exe) => exe,
                        Err(e) => {
                            return TypedResult::error(
                                "exe",
                                format!("could not find the running binary: {e}"),
                            );
                        }
                    };
                    match oma_theme::watch::install_hook(&exe) {
                        Ok(path) => TypedResult::ok(Hook {
                            installed: true,
                            runs: std::fs::read_to_string(&path).ok().map(|s| s.trim().to_string()),
                            path: path.display().to_string(),
                        }),
                        Err(e) => TypedResult::error("hook", format!("{e}")),
                    }
                }
                "remove" | "uninstall" => match std::fs::remove_file(&path) {
                    Ok(()) => TypedResult::ok(Hook { path: shown, installed: false, runs: None }),
                    // Removing something that is not there is what was asked
                    // for, not a failure to do it.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        TypedResult::ok(Hook { path: shown, installed: false, runs: None })
                    }
                    Err(e) => TypedResult::error("hook", format!("could not remove {shown}: {e}")),
                },
                other => TypedResult::error(
                    "usage",
                    format!("unknown action {other:?}; use show, install or remove"),
                ),
            }
        },
    )
    .description(
        "Install Omarchy's theme-set hook, so a theme change reaches this \
         browser the moment it happens. The inotify watch covers the common \
         case on its own; the hook is the one that cannot miss.",
    )
    .destructive(true)
    .done();

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
        .command("hook", hook)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_url_is_reduced_to_its_origin() {
        let known: Vec<String> = vec![];
        assert_eq!(
            resolve_origin(&known, "https://example.com/some/page?q=1").unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn a_bare_host_picks_the_one_you_have_decided_about() {
        // The bug this exists for: `permission forget localhost:3000` guessing
        // https and silently forgetting nothing.
        let known = vec!["http://localhost:3000".to_string()];
        assert_eq!(resolve_origin(&known, "localhost:3000").unwrap(), "http://localhost:3000");
    }

    #[test]
    fn a_bare_host_you_have_never_decided_about_defaults_to_https() {
        let known: Vec<String> = vec![];
        assert_eq!(resolve_origin(&known, "example.com").unwrap(), "https://example.com");
    }

    #[test]
    fn a_bare_host_that_could_be_two_sites_is_refused_rather_than_guessed() {
        let known = vec!["http://localhost:3000".to_string(), "https://localhost:3000".to_string()];
        let e = resolve_origin(&known, "localhost:3000").unwrap_err();
        assert!(e.contains("ambiguous"), "{e}");
        assert!(e.contains("http://localhost:3000"), "{e}");
        assert!(e.contains("https://localhost:3000"), "{e}");
    }

    #[test]
    fn nothing_is_not_a_site() {
        let known: Vec<String> = vec![];
        assert!(resolve_origin(&known, "").is_err());
        assert!(resolve_origin(&known, "   ").is_err());
    }
}
