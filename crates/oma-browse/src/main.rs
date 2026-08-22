//! oma-browse — an Omarchy-themed, agent-drivable browser.
// An `unwrap` in a test is the assertion, and a `panic!` in one is how a test
// fails. The workspace lints that forbid both in shipping code would otherwise
// fire on every test in the tree; the non-test build still checks the real code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod blocker;
mod bookmarks;
mod commands;
mod config;
mod control;
mod crash;
mod data;
mod dispatch;
mod downloads;
mod engine;
mod favicon;
mod follow;
mod fuzzy;
mod hints;
mod history;
mod inspect;
mod interstitial;
mod keys;
mod layout;
mod mcp;
mod paths;
mod permissions;
mod policy;
mod profile;
mod progress;
mod server;
mod session;
mod shot;
mod state;
mod strip;
mod tabs;
mod ui;
mod vault;
mod window;

use std::sync::Arc;

use anyhow::Result;

use crate::state::AppState;

/// Which face of the binary the user asked for.
///
/// One binary, two modes. A subcommand means "talk to the browser"; anything
/// else means "be the browser". `omarchy-launch-browser` passes a bare URL and
/// rewrites its own `--private` to `--incognito` for non-Firefox, non-Edge
/// browsers, so both of those have to land in GUI mode.
enum Invocation {
    Gui { url: Option<String>, incognito: bool, palette: bool, fresh: bool, app: bool },
    Command,
}

fn classify(args: &[String]) -> Invocation {
    let mut url = None;
    let mut incognito = false;
    let mut palette = false;
    let mut fresh = false;
    let mut app = false;

    for arg in args {
        match arg.as_str() {
            "--incognito" | "--private" => incognito = true,
            // One site's window: no strip, no palette on open, and a WM class of
            // its own. Both spellings, because Chrome's is `--app=<url>` and
            // that is the one people's fingers already know.
            "--app" => app = true,
            a if a.starts_with("--app=") => {
                app = true;
                if let Some(rest) = a.strip_prefix("--app=").filter(|r| !r.is_empty()) {
                    url = Some(normalize_url(rest));
                }
            }
            // Ours, and only ours: `window new` passes it when it has no URL to
            // send the new window to, so Ctrl-N comes up asking where to go
            // rather than sitting on the start page. See `window::spawn`.
            "--palette" => palette = true,
            // Also ours: `window new` sets it so Ctrl-N opens a window rather
            // than handing its URL to the window that is already up.
            "--new" => fresh = true,
            a if looks_like_url(a) => url = Some(normalize_url(a)),
            // Anything else is a subcommand or a flag for one; hand the whole
            // argv to incurs and let it do the parsing and the error messages.
            _ => return Invocation::Command,
        }
    }
    Invocation::Gui { url, incognito, palette, fresh, app }
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://") || s.starts_with("file://")
}

fn normalize_url(s: &str) -> String {
    s.to_string()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        // Logs on stderr, always. stdout is the answer -- a command's JSON, or a
        // relayed MCP frame -- and a log line in the middle of it is a parse
        // error for whatever is reading.
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "oma_browse=info,oma_theme=info".into()),
        )
        .init();

    // Before anything asks where the config file, the history or the control
    // socket lives: `--profile` moves all three, and a process that read one of
    // them first would be half in one profile and half in another.
    let (args, profile) = profile::take_flag(std::env::args().skip(1).collect());
    profile::set(profile);

    match classify(&args) {
        Invocation::Command => command(args).await,
        Invocation::Gui { url, incognito, palette, fresh, app } => {
            // A URL from a launcher, a link handler or `xdg-open` belongs in the
            // browser you already have -- opening a second whole browser for a
            // clicked link is not what any other browser does. `--new` and
            // `--incognito` say otherwise, and are obeyed.
            if let Some(url) = url.as_deref()
                && !fresh
                && !incognito
                && !app
                && join(url).await
            {
                return Ok(());
            }
            gui(url, incognito, palette, app).await
        }
    }
}

/// Ask a freshly opened window the question that was asked of this process.
///
/// The window has already done the opening -- it was launched with the URL, so
/// `tab open` would make a *second* tab and `window new` a second window. What
/// is wanted is the answer, not the action, so the two commands are re-phrased
/// as the question that describes what already happened.
async fn answer_from(dir: &std::path::Path, pid: u32, argv: &[String]) -> Result<()> {
    use std::io::Write as _;

    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
    let wants_json = argv.iter().any(|a| a == "--json");
    let mut ask: Vec<String> = match words.as_slice() {
        // The window opened the URL as its first tab, so "which tab is active"
        // is the same answer `tab open` would have given.
        ["tab", "open", ..] => vec!["tab".into(), "list".into()],
        // Nothing to ask: the answer is the pid, and this process has it. Said
        // in whichever shape the caller asked for -- a script that passed
        // `--json` and got `pid: 1234` back is a script that breaks here, which
        // is the whole class of bug this branch exists to fix.
        _ => {
            if wants_json {
                println!("{}", serde_json::json!({ "pid": pid, "workspace": null }));
            } else {
                println!("pid: {pid}");
            }
            return Ok(());
        }
    };
    // Always JSON on the wire, whatever the caller wanted: this answer is
    // reshaped below before it is printed, and reshaping a table is harder than
    // reshaping an object.
    ask.push("--json".into());

    let request = control::Request::from_process(ask);
    let reply = match control::forward(dir, control::Target::Window(pid), &request).await {
        Ok(reply) => reply,
        // The window answered a moment ago -- `wait_until_ready` proved it --
        // so this is a window that died between then and now.
        Err(e) => return Err(anyhow::anyhow!("{e}")),
    };

    // `tab list` answers with a list, and `tab open` answers with one tab. A
    // caller who asked for the second and got the first has to branch on which
    // command started the browser, which is exactly the sort of thing that makes
    // a CLI unscriptable. Narrow it back down.
    let Some(tab) = active_tab(&reply.stdout) else {
        print!("{}", reply.stdout);
        let _ = std::io::stdout().flush();
        return Ok(());
    };
    if wants_json {
        println!("{tab}");
    } else {
        print!("{}", flat(&tab));
    }
    let _ = std::io::stdout().flush();
    Ok(())
}

/// The active tab out of a `tab list --json` answer.
fn active_tab(stdout: &str) -> Option<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(stdout).ok()?;
    let tabs = parsed.get("tabs")?.as_array()?;
    tabs.iter()
        .find(|t| t.get("active").and_then(serde_json::Value::as_bool) == Some(true))
        .or_else(|| tabs.first())
        .cloned()
}

/// A flat JSON object as the `key: value` lines a terminal gets.
fn flat(value: &serde_json::Value) -> String {
    let Some(object) = value.as_object() else { return format!("{value}\n") };
    object
        .iter()
        .map(|(key, value)| match value {
            serde_json::Value::String(s) => format!("{key}: {s}\n"),
            other => format!("{key}: {other}\n"),
        })
        .collect()
}

/// Hand a URL to the window that is already open. `false` if there is none.
async fn join(url: &str) -> bool {
    let config = config::Config::load();
    let dir = control::dir_for(&config.control);
    let argv = vec!["tab".to_string(), "open".to_string(), url.to_string()];
    let request = control::Request::from_process(argv);
    match control::forward(&dir, control::Target::Current, &request).await {
        Ok(reply) if reply.exit_code.unwrap_or(0) == 0 => {
            tracing::info!(url, "opened in the window that was already up");
            true
        }
        // The window took the question and did not like it -- a URL it cannot
        // parse, say. Opening a second browser would not make that URL any
        // better, so pass the complaint on and stop here.
        Ok(reply) => {
            tracing::warn!(url, answer = %reply.stdout.trim(), "the browser would not open that");
            true
        }
        Err(control::Failure::NoWindow) => false,
        // A window is there but would not take it. Being the browser ourselves
        // is a worse answer than saying what went wrong and letting the user
        // try again -- but a link that goes nowhere is worse than both, so open
        // a window and carry on.
        Err(e) => {
            tracing::warn!(error = %e, "could not hand the URL over; opening a window instead");
            false
        }
    }
}

/// A subcommand: run it in the browser you were last looking at.
///
/// This used to build a whole `AppState` with no window attached and run the
/// command against that, which is why `oma-browse tab open <url>` answered "the
/// window is not up yet" -- there was no browser in the process it ran in, and
/// no way to reach the one on screen. Now argv goes down the control socket and
/// the window that owns the tabs runs it.
async fn command(argv: Vec<String>) -> Result<()> {
    let (argv, target) = control::take_window_flag(argv);
    // The dotfile, but not the state behind it: on the forwarding path nothing
    // here needs history, bookmarks or a theme, and reading them was the slowest
    // part of every invocation.
    let config = config::Config::load();
    let dir = control::dir_for(&config.control);

    // MCP is a protocol, not a command: it gets its own path to the window
    // rather than one argv at a time.
    if argv.iter().any(|a| a == "--mcp") {
        // An MCP client starts this binary and expects a server. Serving one
        // here, against a windowless process, is what it used to do -- and every
        // tool call then answered "the window is not up yet". Opening a browser
        // is what the client asked for in the only way it can ask.
        if control::socket_for(&dir, target).is_none() {
            // `--window` names a particular browser. Starting a different one
            // and relaying to that would answer a question nobody asked.
            if let control::Target::Window(pid) = target {
                anyhow::bail!("no browser window with pid {pid} is running");
            }
            open_a_window(&config, &dir).await?;
        }
        if mcp::relay(&dir, target).await? {
            return Ok(());
        }
    }

    // `--follow` is a conversation, not a command: it asks the same question
    // over and over and prints what is new. That loop belongs on this side of
    // the socket -- see `crate::follow`.
    if let Some(following) = follow::wanted(&argv) {
        return follow::run(&dir, target, following).await;
    }

    if !control::runs_locally(&argv) {
        let request = control::Request::from_process(argv.clone());
        match control::forward(&dir, target, &request).await {
            Ok(reply) => {
                use std::io::Write as _;
                print!("{}", reply.stdout);
                let _ = std::io::stdout().flush();
                if !reply.stderr.is_empty() {
                    eprint!("{}", reply.stderr);
                }
                match reply.exit_code {
                    Some(code) => std::process::exit(code),
                    None => return Ok(()),
                }
            }
            // Nothing to talk to, and something to open: be the thing that
            // opens it, the way `xdg-open` would.
            Err(control::Failure::NoWindow) if control::opens_a_window(&argv) => {
                let url = first_value(&argv).map(|u| tabs::resolve_input(&u, &config.search));
                // `window new` with nothing to open comes up asking where to go,
                // the way Ctrl-N does; `tab open <url>` has somewhere to be.
                let palette = url.is_none();
                let before = control::live_in(&dir);
                let pid = window::spawn(config.startup.incognito, url, palette, true)?;
                tracing::info!(pid, "no window to talk to; opened one");

                // Wait for it, then ask it the question that was asked here.
                // Without this the command printed nothing at all and exited 0:
                // a script that ran `oma-browse tab open <url> --json` as its
                // first command got no JSON, no tab id, and no way to tell
                // whether anything had happened. See `window::wait_until_ready`.
                let ready =
                    window::wait_until_ready(&dir, Some(pid), &before, window::READY_TIMEOUT).await;
                let Some(pid) = ready else {
                    anyhow::bail!(
                        "a window was started but did not answer within {}s",
                        window::READY_TIMEOUT.as_secs()
                    );
                };
                return answer_from(&dir, pid, &argv).await;
            }
            // A question about tabs, pages or the chrome, with no tabs, pages
            // or chrome to ask about. Answering it from a windowless process
            // prints `tabs[0]:` and exits 0, which reads as "the browser has
            // nothing open" -- a different fact, and one an agent will act on.
            Err(control::Failure::NoWindow) if control::needs_a_window(&argv) => {
                anyhow::bail!(
                    "no browser window is running, so there is nothing to ask. \
                     Start one with `oma-browse`, or open a page directly with \
                     `oma-browse tab open <url>`, which starts one for you."
                );
            }
            // Whatever is left reads files rather than a window -- history,
            // bookmarks, downloads, the config -- and a windowless process
            // answers those exactly as a window would. Said out loud all the
            // same, so nobody reads the answer as coming from a browser.
            Err(control::Failure::NoWindow) => {
                tracing::info!("no browser window is running; answering from this process");
            }
            // A window was there and the conversation broke. Never retried here:
            // the command may already have run, and running it twice is worse
            // than not running it at all.
            Err(e) => return Err(anyhow::anyhow!("{e}")),
        }
    }

    // Locally: metadata about the graph, incurs' own installers, and whatever
    // could not be forwarded. All of it answers with no browser running, which
    // is the point.
    let state = Arc::new(AppState::new(config));
    let cli = commands::command_graph(state);
    cli.serve_with(argv).await.map_err(|e| anyhow::anyhow!("{e}"))
}

/// The first word that is not a flag or a command name: `tab open <this>`.
fn first_value(argv: &[String]) -> Option<String> {
    argv.iter().skip(2).find(|a| !a.starts_with('-')).cloned()
}

/// How long to wait for a window we just started to answer.
///
/// WebKit and GTK take a moment, and the socket is bound before `run` is called
/// rather than after the first paint -- so this is waiting on process startup,
/// not on a browser being ready to look at.
const STARTUP: std::time::Duration = std::time::Duration::from_secs(10);

/// Start a browser and wait until it will talk to us.
///
/// For the callers that cannot do anything useful without one and cannot ask
/// the user to open it: an MCP client has a pipe and a protocol, and no way to
/// say "start your browser first".
async fn open_a_window(config: &config::Config, dir: &std::path::Path) -> Result<u32> {
    let pid = window::spawn(config.startup.incognito, None, false, true)?;
    tracing::info!(pid, "no window to talk to; opened one");

    // By pid rather than through `current.sock`, which some *other* window may
    // own: the answer has to come from the one we just started.
    let waiting_for = control::Target::Window(pid);
    let deadline = std::time::Instant::now() + STARTUP;
    while std::time::Instant::now() < deadline {
        if control::socket_for(dir, waiting_for).is_some() {
            return Ok(pid);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // Deliberately not silent: the alternative is a client that hangs, or one
    // that gets a server answering every call with "the window is not up yet".
    anyhow::bail!("started a browser window (pid {pid}) but it did not come up within {STARTUP:?}")
}

/// Be the browser.
async fn gui(url: Option<String>, incognito: bool, palette: bool, app: bool) -> Result<()> {
    // Before anything else: the window's size, the home page and the search
    // engine all come out of it, and Tauri owns the main thread from `run` on.
    let state = Arc::new(AppState::new(config::Config::load()));
    // Before the command graph and before the window: `strip_enabled` and the
    // page injection both read it, and both are consulted below.
    state.set_app_mode(app);
    // Shared rather than owned: the `/argv` route runs commands through this
    // same graph, so the router needs a handle on it that outlives `build`.
    let cli = Arc::new(commands::command_graph(state.clone()));

    // The registry every non-CLI surface dispatches through. `try_` rather than
    // `tool_catalog()`, which panics when two commands expose the same name:
    // that turns a latent panic reachable from any invocation into a startup
    // error, which is the right shape for a mistake in the graph.
    let catalog = cli.try_tool_catalog().map_err(|e| anyhow::anyhow!("{e}"))?;
    // The flag or the config file; either is enough. There is no way to
    // ask for a *public* window from a config that says otherwise,
    // which is the right way round for this particular setting.
    let incognito = incognito || state.config.startup.incognito;
    let server = server::build(cli, catalog.clone(), state.clone()).await?;
    // The browser's own pages travel with the window rather than over a
    // listener: nothing outside this process needs them, and a page that can
    // run commands has no business on an address anything can dial.
    let chrome = server.chrome.clone();
    tracing::info!(socket = %server.socket.display(), ?url, incognito, "oma-browse up");

    // Tauri takes the main thread from `run` on, so the socket has to be
    // answering before then.
    tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            tracing::error!(error = %e, "control plane stopped");
        }
    });

    let base: url::Url = format!("{}://localhost/", window::CHROME_SCHEME).parse()?;
    state.set_base_url(base.clone());
    let start_url: url::Url = match url {
        Some(u) => u.parse()?,
        // The configured home, or the browser's own start page when
        // `home` is empty. Resolved the way the palette resolves it, so
        // a bare host in the config file works.
        None => {
            let fallback = base.join("start")?.to_string();
            state.config.home_url(&fallback).parse()?
        }
    };

    // `theme.veil` has already been folded into this by
    // `ThemeState::load`, so there is one number here and not two.
    let (background, opacity) = {
        let theme = state.theme.read().await;
        (theme.css.tint, theme.css.opacity)
    };
    let page_script = state.page_script().await;

    // Allocate the first tab here: Tauri owns the main thread from the
    // moment `run` is called, and the tab model lives behind async locks.
    let first_tab = {
        let mut tabs = state.tabs.write().await;
        let (id, label) = tabs.allocate(start_url.to_string());
        tabs.set_active(id);
        label
    };

    // Omarchy emits no D-Bus signal and no broadcast on a theme change:
    // it swaps the directory, rewrites `theme.name`, and runs its hooks.
    // Watching the *parent* directory catches it without asking the user
    // to install anything. Omarchy's own `theme-set` hook can call
    // `oma-browse theme reload` for a faster path, but that is the
    // user's to install: it writes into their Omarchy config.
    spawn_theme_watcher(state.clone());

    // Keep `~/.local/state/oma-browse/session` in step with the tab
    // list, so `tab restore` has something to restore. Read the old
    // session *before* starting the ticker, which would overwrite it.
    // Before the ticker, which would otherwise overwrite the very
    // thing `tab restore` exists to read.
    session::init();
    let restore = state.config.startup.restore && !incognito;
    let previous = if restore { session::saved() } else { Vec::new() };
    crate::session::spawn(state.clone());
    if !previous.is_empty() {
        let state = state.clone();
        tokio::spawn(async move {
            // After the window exists: `tabs::open` needs an app handle,
            // and Tauri does not have one until `setup` has run.
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            let opened = session::restore(&state).await;
            tracing::info!(opened, "restored the last session");
        });
    }

    window::run(window::Launch {
        state,
        start_url,
        incognito,
        background,
        opacity,
        page_script,
        first_tab,
        catalog,
        chrome,
        // Never in app mode: a web app that opens with a command palette over
        // it is not a web app.
        open_palette: palette && !app,
    })
}

/// Watch for Omarchy theme changes and re-dress the browser in place.
fn spawn_theme_watcher(state: Arc<AppState>) {
    tokio::spawn(async move {
        let (mut rx, _watcher) = match oma_theme::watch::watch_theme_changes() {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "not watching for theme changes");
                return;
            }
        };
        tracing::info!("watching for Omarchy theme changes");

        // `_watcher` must outlive the loop: dropping it stops the watch.
        while let Some(name) = rx.recv().await {
            if state.reload_theme().await {
                tracing::info!(theme = %name, "restyling");
                if let Err(e) = crate::window::restyle(&state).await {
                    tracing::warn!(error = %e, "could not restyle");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_launch_is_gui() {
        assert!(matches!(classify(&args(&[])), Invocation::Gui { url: None, .. }));
    }

    #[test]
    fn a_url_is_gui_not_a_subcommand() {
        // `omarchy-launch-browser` passes the URL positionally.
        match classify(&args(&["https://example.com"])) {
            Invocation::Gui { url, .. } => assert_eq!(url.as_deref(), Some("https://example.com")),
            _ => panic!("a bare URL must open the GUI"),
        }
    }

    #[test]
    fn incognito_is_gui() {
        // Omarchy rewrites its own `--private` to `--incognito` for us, so both
        // spellings must reach GUI mode rather than incurs' parser.
        for flag in ["--incognito", "--private"] {
            match classify(&args(&[flag])) {
                Invocation::Gui { incognito, .. } => {
                    assert!(incognito, "{flag} should set incognito")
                }
                _ => panic!("{flag} must open the GUI"),
            }
        }
    }

    #[test]
    fn a_new_window_flag_is_gui_and_refuses_to_join() {
        // `window new` passes this. Without it, Ctrl-N with a URL would hand the
        // URL to the window it was pressed in and open nothing.
        match classify(&args(&["--new", "https://example.com"])) {
            Invocation::Gui { fresh, url, .. } => {
                assert!(fresh);
                assert_eq!(url.as_deref(), Some("https://example.com"));
            }
            _ => panic!("--new must open the GUI"),
        }
        // And a URL on its own does not set it, which is what lets a launcher's
        // link land in the window you already have.
        match classify(&args(&["https://example.com"])) {
            Invocation::Gui { fresh, .. } => assert!(!fresh),
            _ => panic!("a bare URL must open the GUI"),
        }
    }

    #[test]
    fn app_mode_is_gui_and_never_joins_a_window() {
        // `--app` opens *this* site's window. Handing the URL to whatever
        // browser window happened to be focused would be the opposite.
        match classify(&["--app".into(), "https://github.com".into()]) {
            Invocation::Gui { app, url, .. } => {
                assert!(app);
                assert_eq!(url.as_deref(), Some("https://github.com"));
            }
            Invocation::Command => panic!("--app is a window, not a subcommand"),
        }
    }

    #[test]
    fn chromes_spelling_of_the_flag_carries_the_url() {
        match classify(&["--app=https://github.com".into()]) {
            Invocation::Gui { app, url, .. } => {
                assert!(app);
                assert_eq!(url.as_deref(), Some("https://github.com"));
            }
            Invocation::Command => panic!("--app= is a window, not a subcommand"),
        }
    }

    #[test]
    fn palette_flag_is_gui() {
        // `window new` hands this to the window it opens; incurs' parser must
        // never see it, or Ctrl-N would spawn a process that prints usage.
        match classify(&args(&["--palette"])) {
            Invocation::Gui { url, palette, .. } => {
                assert!(palette);
                assert!(url.is_none());
            }
            _ => panic!("--palette must open the GUI"),
        }
    }

    #[test]
    fn incognito_with_a_url_is_still_gui() {
        match classify(&args(&["--incognito", "https://example.com"])) {
            Invocation::Gui { url, incognito, .. } => {
                assert!(incognito);
                assert_eq!(url.as_deref(), Some("https://example.com"));
            }
            _ => panic!("expected GUI"),
        }
    }

    #[test]
    fn subcommands_go_to_the_cli() {
        for a in [vec!["theme"], vec!["theme", "show"], vec!["--help"], vec!["--version"]] {
            assert!(matches!(classify(&args(&a)), Invocation::Command), "{a:?}");
        }
    }
}
