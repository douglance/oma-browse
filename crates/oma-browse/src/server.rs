//! The control plane: a socket for the CLI, and a loopback port for the chrome.
//!
//! Two listeners, one router, and the split between them is the point. The
//! palette, the strip and the start page are web pages, so WebKit needs an
//! origin it can load -- that is the TCP port, and it is an implementation
//! detail of how this browser draws itself, not an interface.
//!
//! Everything a person or an agent drives the browser with -- `/cmd`, MCP, and
//! the `/argv` route the CLI forwards to -- is on a Unix socket instead, where
//! the filesystem decides who may connect. A loopback port cannot: it is open to
//! every process and every account on the machine, which is a strange thing to
//! hand something that can read the pages you are logged in to.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use incurs::cli::Cli;
use incurs::tool::ToolCatalog;
use tokio::net::{TcpListener, UnixListener};
use topcoat::router::tower::TowerService;

use crate::state::AppState;

/// The browser's own mark, and the same art squared off for a favicon.
///
/// Compiled in rather than read from disk: the binary is the whole install --
/// `oma-browse` is one file you can copy onto a machine -- and a start page that
/// depends on a file beside it is a start page that breaks the moment it is not.
const MARK_PNG: &[u8] = include_bytes!("../../../assets/mark.png");
const ICON_PNG: &[u8] = include_bytes!("../../../assets/icon.png");

/// Serve compiled-in bytes as a PNG.
///
/// An hour of caching: long enough that a reload does not re-fetch the mark,
/// short enough that a rebuild during development is picked up without a hard
/// refresh. The chrome's port is ephemeral, so a fresh run is a fresh origin and
/// a cold cache anyway.
fn png(bytes: &'static [u8]) -> axum::response::Response {
    use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
    use axum::response::IntoResponse;
    ([(CONTENT_TYPE, "image/png"), (CACHE_CONTROL, "max-age=3600")], bytes).into_response()
}

pub struct Server {
    /// Where the CLI, agents and scripts reach this window.
    pub socket: PathBuf,
    /// The browser's own pages, for the window to serve to its own webviews
    /// over [`crate::window::CHROME_SCHEME`]. Not bound to anything.
    pub chrome: axum::Router,
    uds: UnixListener,
    control: axum::Router,
    /// Off unless `[control] remote_port` asked for it.
    remote: Option<(TcpListener, axum::Router)>,
}

/// Bind both, and compose the two frameworks onto them.
pub async fn build(cli: Arc<Cli>, catalog: ToolCatalog, state: Arc<AppState>) -> Result<Server> {
    let dir = crate::control::dir_for(&state.config.control);
    let remote_port = state.config.control.remote_port;
    let (chrome, control, remote_routes) = routers(cli, catalog, state)?;

    let pid = std::process::id();
    let uds = crate::control::bind_in(&dir, pid).await?;
    let socket = crate::control::socket_in(&dir, pid);

    let remote = match remote_port {
        0 => None,
        port => remote_listener(port, &dir, pid, remote_routes).await,
    };

    Ok(Server { socket, chrome, uds, control, remote })
}

/// The opt-in port, when the config file asks for one.
///
/// Best-effort on purpose: a window is a process, and with several of them the
/// second to start finds the port taken. Refusing to open a window over that
/// would be the fixed-port bug all over again, so it warns and carries on --
/// the socket is the way in that always works.
async fn remote_listener(
    port: u16,
    dir: &std::path::Path,
    pid: u32,
    routes: axum::Router,
) -> Option<(TcpListener, axum::Router)> {
    match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => {
            tracing::warn!(
                port,
                "answering commands on a loopback port: every process on this machine can drive this browser"
            );
            let router = routes.route("/json/list", windows_route(dir.to_path_buf(), pid)).layer(
                axum::middleware::from_fn({
                    let dir = dir.to_path_buf();
                    move |request, next| elsewhere(dir.clone(), request, next)
                }),
            );
            Some((listener, router))
        }
        Err(e) => {
            tracing::warn!(error = %e, port, "no remote port; the control socket is unaffected");
            None
        }
    }
}

/// The live windows, in the spirit of a debugging port's target list: enough to
/// pick one and address it with `?window=<pid>`.
fn windows_route(dir: PathBuf, mine: u32) -> axum::routing::MethodRouter {
    axum::routing::get(move || {
        let dir = dir.clone();
        async move {
            let windows: Vec<_> = crate::control::live_in(&dir)
                .into_iter()
                .map(|pid| {
                    serde_json::json!({
                        "pid": pid,
                        "socket": crate::control::socket_in(&dir, pid),
                        "answering": pid == mine,
                    })
                })
                .collect();
            axum::Json(serde_json::json!({ "windows": windows }))
        }
    })
}

/// Send a request marked `?window=<pid>` to that window instead of this one.
///
/// One port, every window: the port belongs to whichever process bound it, and
/// the others are reached over their sockets -- the same socket the CLI uses,
/// rather than a second mechanism that could disagree with it.
async fn elsewhere(
    dir: PathBuf,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    let Some(pid) = request.uri().query().and_then(window_query) else {
        return next.run(request).await;
    };
    if pid == std::process::id() {
        return next.run(request).await;
    }

    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes.to_vec(),
        Err(e) => {
            return (axum::http::StatusCode::BAD_REQUEST, format!("unreadable body: {e}"))
                .into_response();
        }
    };
    match crate::control::proxy(&dir, pid, axum::http::Request::from_parts(parts, body)).await {
        Ok(response) => {
            let (parts, body) = response.into_parts();
            axum::http::Response::from_parts(parts, axum::body::Body::from(body)).into_response()
        }
        Err(crate::control::Failure::NoWindow) => (
            axum::http::StatusCode::NOT_FOUND,
            format!("no window with pid {pid}; ask /json/list which ones are up"),
        )
            .into_response(),
        Err(e) => (axum::http::StatusCode::BAD_GATEWAY, format!("{e}")).into_response(),
    }
}

fn window_query(query: &str) -> Option<u32> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("window="))
        .and_then(|value| value.parse().ok())
}

/// The route sets, without a listener under any of them.
///
/// Three, and the difference between them is the whole security story:
///
/// * `chrome` -- our own pages, for our own webviews, over a URI scheme.
/// * `control` -- the same pages *plus* the command graph, on the Unix socket.
/// * `remote` -- the command graph alone, for the opt-in port. No palette: the
///   page that can run commands is not something to hand out over TCP.
///
/// Split out so a test can drive the control plane without binding anything a
/// machine can see.
pub fn routers(
    cli: Arc<Cli>,
    catalog: ToolCatalog,
    state: Arc<AppState>,
) -> Result<(axum::Router, axum::Router, axum::Router)> {
    let topcoat_router = crate::ui::router(state, catalog);

    // What the webviews need, and nothing else. Every route here is one of our
    // own pages or its artwork.
    let chrome = axum::Router::new()
        // The start page's mark, and the icon every page in this origin gets --
        // including `/favicon.ico`, which WebKit asks for unprompted and which
        // was answering 404 on every start-page load.
        .route("/mark.png", axum::routing::get(|| async { png(MARK_PNG) }))
        .route("/icon.png", axum::routing::get(|| async { png(ICON_PNG) }))
        .route("/favicon.ico", axum::routing::get(|| async { png(ICON_PNG) }))
        .fallback_service(TowerService::new(topcoat_router));

    // The same pages plus the command graph. `TowerService` is a handle on an
    // `Arc`, so the clone shares one topcoat router rather than building a
    // second: two listeners cost one router.
    //
    // incurs is *nested*, never merged: `build_cli_router` registers `/` and
    // `/{*path}` catch-alls that would otherwise swallow every Topcoat route --
    // which is also why `/argv` is mounted out here rather than under `/cmd`.
    let control = chrome
        .clone()
        .nest("/cmd", incurs::http::build_cli_router(&cli)?)
        .route(crate::control::ROUTE, argv_route(cli.clone()));

    let remote = axum::Router::new().nest("/cmd", incurs::http::build_cli_router(&cli)?);

    Ok((chrome, control, remote))
}

/// The route the CLI forwards to: an argv in, what it printed out.
///
/// The alternative was translating argv into `/cmd/<group>/<command>/<value>`
/// in the client, which means reimplementing incurs' parser *and* its output
/// formatter and keeping both in step with a crate we do not own. Handing the
/// argv over intact means the window runs it through exactly the code path a
/// local invocation would, so help text, validation errors, `--json` and the
/// exit code all come out right by construction.
fn argv_route(cli: Arc<Cli>) -> axum::routing::MethodRouter {
    axum::routing::post(move |body: axum::body::Bytes| {
        let cli = cli.clone();
        async move { run_argv(cli, &body).await }
    })
}

async fn run_argv(cli: Arc<Cli>, body: &[u8]) -> axum::response::Response {
    use axum::response::IntoResponse;

    let request: crate::control::Request = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(e) => {
            return (axum::http::StatusCode::BAD_REQUEST, format!("unreadable request: {e}"))
                .into_response();
        }
    };
    if request.v != crate::control::PROTOCOL {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            format!(
                "protocol {} where this window speaks {}; the binary that opened it is older or newer than yours",
                request.v,
                crate::control::PROTOCOL
            ),
        )
            .into_response();
    }

    tracing::debug!(argv = ?request.argv, human = request.human, "running a forwarded command");

    // `run_to` writes into a `&mut dyn Write` with no `Send` bound, so the future
    // it returns is not `Send` and cannot be awaited on this thread. Driving it
    // to completion inside one blocking-pool thread keeps it where it was made;
    // only the argv and the answer cross back. `Box<dyn Error>` is not `Send`
    // either, hence the `to_string` inside the closure.
    let handle = tokio::runtime::Handle::current();
    let finished = tokio::task::spawn_blocking(move || {
        let mut out: Vec<u8> = Vec::new();
        let runtime = incurs::cli::Runtime::new(
            request.display_name,
            // The graph declares no environment fields, so forwarding the
            // caller's environment would move their secrets in here for nothing.
            std::collections::HashMap::new(),
            request.human,
        );
        let code = handle
            .block_on(cli.run_to(request.argv, &mut out, runtime))
            .map_err(|e| e.to_string())?;
        Ok::<_, String>((code, String::from_utf8_lossy(&out).into_owned()))
    })
    .await;

    let reply = match finished {
        Ok(Ok((exit_code, stdout))) => crate::control::Reply {
            v: crate::control::PROTOCOL,
            exit_code,
            stdout,
            stderr: String::new(),
        },
        // A command that failed to run at all still has to come back as an
        // answer: a dropped connection would leave the CLI with nothing to
        // print and no code to exit with.
        Ok(Err(message)) => crate::control::Reply {
            v: crate::control::PROTOCOL,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: format!("{message}\n"),
        },
        Err(e) => {
            tracing::error!(error = %e, "a forwarded command panicked");
            crate::control::Reply {
                v: crate::control::PROTOCOL,
                exit_code: Some(70),
                stdout: String::new(),
                stderr: "the browser could not finish that command\n".to_string(),
            }
        }
    };
    axum::Json(reply).into_response()
}

impl Server {
    /// Answer commands until a listener goes away.
    pub async fn serve(self) -> Result<()> {
        let Server { uds, control, remote, .. } = self;
        let socket = tokio::spawn(async move { axum::serve(uds, control).await });
        let Some((listener, router)) = remote else {
            socket.await??;
            return Ok(());
        };
        let port = tokio::spawn(async move { axum::serve(listener, router).await });
        tokio::select! {
            done = socket => done??,
            done = port => done??,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A window on a socket of its own, with no GUI and nothing bound on the
    /// network. Returns the directory it is listening in.
    async fn window(name: &str) -> (std::path::PathBuf, u32) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("oma-browse-server-{name}-{stamp}"));
        let pid = std::process::id();

        let state = Arc::new(AppState::detached());
        let cli = Arc::new(crate::commands::command_graph(state.clone()));
        let catalog = cli.try_tool_catalog().unwrap();
        let (_chrome, control, _remote) = routers(cli, catalog, state).unwrap();
        let listener = crate::control::bind_in(&dir, pid).await.unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, control).await;
        });
        (dir, pid)
    }

    async fn ask(dir: &std::path::Path, argv: Vec<String>, human: bool) -> crate::control::Reply {
        let request = crate::control::Request {
            v: crate::control::PROTOCOL,
            argv,
            display_name: "oma-browse".to_string(),
            human,
            cwd: "/".to_string(),
        };
        crate::control::forward(dir, crate::control::Target::Current, &request).await.unwrap()
    }

    /// The whole point of forwarding argv rather than translating it: the
    /// window runs the same parser, formatter and exit-code path a local
    /// invocation would.
    #[tokio::test]
    async fn a_forwarded_command_comes_back_formatted_for_whoever_asked() {
        let (dir, pid) = window("format").await;

        // An output modifier is part of the command, so it has to reach the
        // window -- this is the shape every agent parses.
        let json = ask(&dir, argv(&["--json", "theme", "show"]), false).await;
        assert_eq!(json.exit_code, None, "a command that worked has nothing to exit with");
        let parsed: serde_json::Value =
            serde_json::from_str(&json.stdout).expect("--json must be JSON");
        assert!(parsed.get("name").is_some(), "{}", json.stdout);

        // `human` travels in the request because the browser's stdout is not the
        // one the answer is going to. A rejected command is where it shows: a
        // person gets a sentence, a pipe gets the envelope it can parse.
        let machine = ask(&dir, argv(&["tab", "close", "--nonsense"]), false).await;
        let person = ask(&dir, argv(&["tab", "close", "--nonsense"]), true).await;
        assert_eq!(machine.exit_code, Some(1));
        assert_eq!(person.exit_code, Some(1));
        assert!(machine.stdout.starts_with("code:"), "{}", machine.stdout);
        assert!(person.stdout.starts_with("Error ("), "{}", person.stdout);

        // A command that fails is still an answer -- never a dropped connection
        // that would leave the CLI with nothing to print.
        let closed = ask(&dir, argv(&["tab", "close", "4242"]), false).await;
        assert!(!closed.stdout.is_empty(), "a failure must explain itself");

        crate::control::unlink_in(&dir, pid);
        std::fs::remove_dir_all(&dir).ok();
    }
}
