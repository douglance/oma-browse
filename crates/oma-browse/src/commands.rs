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

/// Build the graph, capturing shared state in each handler.
pub fn command_graph(state: Arc<AppState>) -> Cli {
    Cli::create("oma-browse")
        .version(env!("CARGO_PKG_VERSION"))
        .description("An Omarchy-themed, agent-drivable browser")
        .group(tab_group(state.clone()))
        .group(nav_group(state.clone()))
        .group(page_group(state.clone()))
        .group(ui_group(state.clone()))
        .group(theme_group(state))
}

// ---------------------------------------------------------------------------
// tab
// ---------------------------------------------------------------------------

#[derive(Deserialize, incurs::Args)]
struct TabOpenArgs {
    /// A URL, a bare host, or search terms.
    url: String,
}

#[derive(Deserialize, incurs::Options)]
struct TabOpenOptions {
    /// Open without switching to it.
    background: bool,
}

#[derive(Deserialize, incurs::Args)]
struct TabIdArgs {
    /// Tab id, as reported by `tab list`. Defaults to the active tab.
    id: Option<u32>,
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
                match crate::tabs::open(&state, &ctx.args.url, ctx.options.background).await {
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
    let select = CommandDef::typed::<TabIdArgs, NoOptions, (), TabList, _, _>(
        "select",
        move |ctx: TypedContext<TabIdArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                let Some(id) = ctx.args.id else {
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
    .description("Switch to a tab by id")
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

    Cli::create("tab")
        .description("Open, close and switch tabs")
        .command("open", open)
        .command("list", list)
        .command("select", select)
        .command("close", close)
        .command("cycle", cycle)
}

#[derive(Deserialize, incurs::Args)]
struct CycleArgs {
    /// How many tabs to move; negative goes backwards. Defaults to 1.
    delta: Option<i32>,
}

// ---------------------------------------------------------------------------
// nav
// ---------------------------------------------------------------------------

#[derive(Deserialize, incurs::Args)]
struct GoArgs {
    /// A URL, a bare host, or search terms.
    url: String,
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
    let go = CommandDef::typed::<GoArgs, NoOptions, (), Navigated, _, _>(
        "go",
        move |ctx: TypedContext<GoArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                match crate::tabs::navigate(&state, &ctx.args.url).await {
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

    let mut group = Cli::create("nav").description("Move the active tab around").command("go", go);

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
    js: String,
}

#[derive(JsonSchema, Serialize)]
struct Evaluated {
    /// The expression's value, as JSON.
    result: String,
}

/// Every field is an option rather than a positional argument on purpose: over
/// HTTP incurs binds positionals to *path segments*, and a filesystem path
/// contains slashes, so `page screenshot /tmp/a.png` would arrive as `tmp`.
#[derive(Deserialize, incurs::Options)]
struct ShotOptions {
    /// Where to write the PNG. Defaults to `$XDG_RUNTIME_DIR/oma-browse/`.
    path: Option<String>,
    /// Capture the whole scrollable document instead of just the viewport.
    full: bool,
    /// Composite onto white instead of preserving the page's transparency.
    opaque: bool,
}

fn page_group(state: Arc<AppState>) -> Cli {
    let s = state.clone();
    let eval = CommandDef::typed::<EvalArgs, NoOptions, (), Evaluated, _, _>(
        "eval",
        move |ctx: TypedContext<EvalArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
                match crate::tabs::eval(&state, &ctx.args.js).await {
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

    Cli::create("page")
        .description("Inspect the active page")
        .command("eval", eval)
        .command("screenshot", screenshot)
}

// ---------------------------------------------------------------------------
// ui
// ---------------------------------------------------------------------------

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
    let palette = CommandDef::typed::<PaletteArgs, NoOptions, (), PaletteState, _, _>(
        "palette",
        move |ctx: TypedContext<PaletteArgs, NoOptions, ()>| {
            let state = s.clone();
            async move {
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
    .description("Show, hide or toggle the command palette")
    .done();

    Cli::create("ui").description("Drive the browser's own interface").command("palette", palette)
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
         colour survives — but it can still break sites, so it is off by default.",
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
