//! Where the CLI finds the browser.
//!
//! A browser window is a process (see [`crate::window::spawn`]), and each one
//! listens on its own Unix socket under `$XDG_RUNTIME_DIR/oma-browse`. That is
//! the whole address book: no port to discover, no port file to go stale, and
//! filesystem permissions doing the access control a loopback port cannot.
//!
//! `current.sock` is a symlink to whichever window was focused last, so a bare
//! `oma-browse tab open <url>` means "the window I am looking at". A socket
//! whose process has gone refuses `connect()` immediately, which is how both
//! sides tell a live window from a leftover inode -- cheaper and more truthful
//! than a lock file, and impossible to fool with a reused pid.

use std::io::IsTerminal;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use axum::http;
use serde::{Deserialize, Serialize};

/// Bumped when [`Request`] or [`Reply`] change shape. A client and a browser
/// from different builds can be alive at once -- an upgrade does not close the
/// windows you already had -- so both sides check rather than assume.
pub const PROTOCOL: u32 = 1;

/// The route the browser answers argv on. Mounted at the top level rather than
/// under `/cmd`, whose catch-all would swallow it.
pub const ROUTE: &str = "/argv";

const PREFIX: &str = "window-";
const SUFFIX: &str = ".sock";
const CURRENT: &str = "current.sock";

/// One CLI invocation, on its way to the window that will run it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    /// argv *without* the binary name, exactly as incurs wants it.
    pub argv: Vec<String>,
    /// What to call the binary in help and error text: whatever the user typed.
    pub display_name: String,
    /// Whether the caller's stdout is a terminal. Sent rather than sniffed,
    /// because the browser's stdout is not the one the answer is going to.
    pub human: bool,
    /// The caller's working directory. Carried for the day a command resolves a
    /// relative path; nothing reads it yet.
    pub cwd: String,
}

/// What the window made of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub v: u32,
    /// `None` means "finished, no particular code" -- what incurs returns when a
    /// command succeeds.
    pub exit_code: Option<i32>,
    pub stdout: String,
    /// Reserved: command output, errors included, comes back on `stdout` the way
    /// it does locally. Kept so this can change without a protocol break.
    #[serde(default)]
    pub stderr: String,
}

impl Request {
    /// Everything the browser needs to run this argv as if it had been typed at
    /// the browser's own process.
    pub fn from_process(argv: Vec<String>) -> Self {
        let display_name = std::env::args()
            .next()
            .and_then(|path| Path::new(&path).file_name()?.to_str().map(ToString::to_string))
            .unwrap_or_else(|| "oma-browse".to_string());
        Request {
            v: PROTOCOL,
            argv,
            display_name,
            human: std::io::stdout().is_terminal(),
            cwd: std::env::current_dir().unwrap_or_default().to_string_lossy().into_owned(),
        }
    }
}

/// Which window a command is addressed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The one focused most recently.
    Current,
    /// A particular process, from `--window`.
    Window(u32),
}

/// Why a command did not reach a window.
#[derive(Debug)]
pub enum Failure {
    /// Nothing is listening. The caller decides what that means: launch a
    /// browser, or run the command here.
    NoWindow,
    /// A window was there and the conversation failed. Never retried locally --
    /// the command may already have run.
    Transport(anyhow::Error),
    /// A window answered with something this build does not understand.
    Protocol(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::NoWindow => write!(f, "no browser window is running"),
            Failure::Transport(e) => write!(f, "could not talk to the browser: {e:#}"),
            Failure::Protocol(m) => write!(f, "the browser answered with {m}"),
        }
    }
}

impl std::error::Error for Failure {}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// The directory this window's socket lives in.
///
/// `$XDG_RUNTIME_DIR` is per-user, `0700` and wiped at reboot, which is exactly
/// the lifetime a socket wants. Without it, `/tmp` is shared, so the fallback is
/// keyed by uid: a bare `/tmp/oma-browse` is a name another user could create
/// first.
pub fn dir_for(control: &crate::config::Control) -> PathBuf {
    let configured = control.socket.trim();
    if configured.is_empty() {
        runtime_dir()
    } else {
        // The same `~` expansion `[screenshot] dir` gets: one answer to "what
        // does a path in the dotfile mean".
        PathBuf::from(crate::shot::shellexpand(configured))
    }
}

/// The default home for every window's socket.
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("oma-browse"),
        None => {
            let uid = std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0);
            std::env::temp_dir().join(format!("oma-browse-{uid}"))
        }
    }
}

/// This window's socket.
pub fn socket_in(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{PREFIX}{pid}{SUFFIX}"))
}

/// The symlink pointing at the window that was focused last.
pub fn current_in(dir: &Path) -> PathBuf {
    dir.join(CURRENT)
}

/// Make the directory, and refuse to use one somebody else owns.
pub fn ensure_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("could not make {}", dir.display()))?;
    let meta =
        std::fs::metadata(dir).with_context(|| format!("could not stat {}", dir.display()))?;
    let mine = std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(meta.uid());
    anyhow::ensure!(
        meta.uid() == mine,
        "{} belongs to uid {} rather than {mine}",
        dir.display(),
        meta.uid()
    );
    // The socket is chmodded too, but the directory is what closes the window
    // between `bind` and that call: connecting needs traversal here as well.
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

// ---------------------------------------------------------------------------
// Liveness
// ---------------------------------------------------------------------------

/// Whether something is actually listening.
///
/// A socket file outlives the process that made it, so its existence proves
/// nothing. Connecting proves everything, and costs nothing: an unattended inode
/// answers `ECONNREFUSED` without waiting.
fn alive(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Delete every socket in the directory whose window has gone.
///
/// A live window's socket is never touched, so two browsers starting at the same
/// moment cannot sweep each other away.
pub fn sweep_in(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if pid_of(&path).is_some() && !alive(&path) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The windows that answer, newest socket first.
pub fn live_in(dir: &Path) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<(std::time::SystemTime, u32)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let pid = pid_of(&path)?;
            alive(&path).then(|| {
                let when =
                    entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                (when, pid)
            })
        })
        .collect();
    // Newest first: the most recently focused window is the one a bare
    // `oma-browse tab open <url>` should mean.
    found.sort_by_key(|(when, _)| std::cmp::Reverse(*when));
    found.into_iter().map(|(_, pid)| pid).collect()
}

/// `window-1234.sock` -> `1234`, and `None` for anything else in the directory.
fn pid_of(path: &Path) -> Option<u32> {
    path.file_name()?.to_str()?.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?.parse().ok()
}

// ---------------------------------------------------------------------------
// The window side
// ---------------------------------------------------------------------------

/// Listen for this window, and become the one a bare command means.
pub async fn bind_in(dir: &Path, pid: u32) -> Result<tokio::net::UnixListener> {
    ensure_dir(dir)?;
    // Before binding, not after: a crash leaves an inode behind, and `bind`
    // fails on a path that exists whether or not anything is listening.
    sweep_in(dir);
    let path = socket_in(dir, pid);
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)
        .with_context(|| format!("could not listen on {}", path.display()))?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    point_current_in(dir, pid);
    Ok(listener)
}

/// Point `current.sock` at this window.
///
/// Called on every focus, so it must be atomic: a symlink to a temporary name
/// followed by a rename replaces the link in one step, and never leaves a moment
/// where `current.sock` does not exist.
pub fn point_current_in(dir: &Path, pid: u32) {
    // Relative, so the link keeps working wherever the directory is reached from.
    let target = format!("{PREFIX}{pid}{SUFFIX}");
    let staging = dir.join(format!(".{CURRENT}.{pid}"));
    let _ = std::fs::remove_file(&staging);
    if std::os::unix::fs::symlink(&target, &staging).is_ok()
        && std::fs::rename(&staging, current_in(dir)).is_err()
    {
        let _ = std::fs::remove_file(&staging);
    }
}

/// Best-effort tidy-up on the way out.
///
/// Best-effort by construction: Tauri's event loop exits the process directly,
/// so nothing on the stack unwinds, and a kill -9 never gets here at all. The
/// connect-probe is what has to be correct; this only keeps the directory neat.
pub fn unlink_in(dir: &Path, pid: u32) {
    let _ = std::fs::remove_file(socket_in(dir, pid));
    // Only if it was ours: another window may have taken it since.
    if std::fs::read_link(current_in(dir))
        .map(|t| t == Path::new(&format!("{PREFIX}{pid}{SUFFIX}")))
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(current_in(dir));
    }
}

// ---------------------------------------------------------------------------
// The client side
// ---------------------------------------------------------------------------

/// Find the socket a target names, mending `current.sock` when it dangles.
pub fn socket_for(dir: &Path, target: Target) -> Option<PathBuf> {
    match target {
        Target::Window(pid) => {
            let path = socket_in(dir, pid);
            alive(&path).then_some(path)
        }
        Target::Current => {
            let current = current_in(dir);
            if alive(&current) {
                return Some(current);
            }
            // Dangling, or never written: the newest live window is the best
            // answer available, and re-pointing means the next call is cheap.
            let pid = live_in(dir).first().copied()?;
            point_current_in(dir, pid);
            Some(socket_in(dir, pid))
        }
    }
}

/// Run an argv in a live window and bring back what it printed.
pub async fn forward(dir: &Path, target: Target, request: &Request) -> Result<Reply, Failure> {
    talk(connect(dir, target).await?, request).await
}

async fn talk(stream: tokio::net::UnixStream, request: &Request) -> Result<Reply, Failure> {
    let body = serde_json::to_vec(request).map_err(|e| Failure::Transport(e.into()))?;
    let http = http::Request::builder()
        .method("POST")
        .uri(ROUTE)
        .header("host", HOST)
        .header("content-type", "application/json")
        .body(body)
        .map_err(|e| Failure::Transport(e.into()))?;

    let response = send(stream, http).await?;
    let status = response.status();
    let bytes = response.into_body();
    if !status.is_success() {
        return Err(Failure::Protocol(format!(
            "{status}: {}",
            String::from_utf8_lossy(&bytes).trim()
        )));
    }
    let reply: Reply = serde_json::from_slice(&bytes)
        .map_err(|e| Failure::Protocol(format!("an unreadable answer ({e})")))?;
    if reply.v != PROTOCOL {
        return Err(Failure::Protocol(format!(
            "protocol {} where this build speaks {PROTOCOL}; restart the browser",
            reply.v
        )));
    }
    Ok(reply)
}

/// The Host every request to a window carries.
///
/// There is no host -- the address is a path on disk -- but HTTP/1.1 insists on
/// the header, and the MCP service refuses names it does not know as
/// DNS-rebinding protection. `localhost` is the one both are happy with.
pub const HOST: &str = "localhost";

/// Open a window's socket, ready to send it a request.
pub async fn connect(dir: &Path, target: Target) -> Result<tokio::net::UnixStream, Failure> {
    let Some(path) = socket_for(dir, target) else { return Err(Failure::NoWindow) };
    match tokio::net::UnixStream::connect(&path).await {
        Ok(stream) => Ok(stream),
        // It was alive a moment ago and is not now: that is a window closing
        // between the probe and the call, not a broken one.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(Failure::NoWindow)
        }
        Err(e) => Err(Failure::Transport(e.into())),
    }
}

/// One HTTP request over an open socket, and the whole answer back.
///
/// Every conversation with a window goes through here -- the CLI's argv, the
/// remote port's proxying, and the MCP relay -- so there is one place that knows
/// how this browser speaks HTTP to itself.
pub async fn send(
    stream: tokio::net::UnixStream,
    request: http::Request<Vec<u8>>,
) -> Result<http::Response<Vec<u8>>, Failure> {
    use http_body_util::BodyExt as _;

    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| Failure::Transport(e.into()))?;
    // The connection future is what actually moves bytes; without driving it the
    // request never leaves.
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::debug!(error = %e, "control connection ended");
        }
    });

    let (parts, body) = request.into_parts();
    let request =
        http::Request::from_parts(parts, http_body_util::Full::new(hyper::body::Bytes::from(body)));
    let response = sender.send_request(request).await.map_err(|e| Failure::Transport(e.into()))?;
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.map_err(|e| Failure::Transport(e.into()))?.to_bytes();
    Ok(http::Response::from_parts(parts, bytes.to_vec()))
}

/// Hand an HTTP request to another window and bring its answer back.
///
/// What the remote port uses to answer `?window=<pid>`: one port can then reach
/// every window, without every window needing a port.
pub async fn proxy(
    dir: &Path,
    pid: u32,
    request: http::Request<Vec<u8>>,
) -> Result<http::Response<Vec<u8>>, Failure> {
    let stream = connect(dir, Target::Window(pid)).await?;
    let (mut parts, body) = request.into_parts();
    // Whatever the caller dialled, the window on the other end knows itself by
    // one name.
    parts.headers.insert("host", http::HeaderValue::from_static(HOST));
    send(stream, http::Request::from_parts(parts, body)).await
}

// ---------------------------------------------------------------------------
// argv, before it is anyone's problem
// ---------------------------------------------------------------------------

/// Pull `--window <pid>` out of the argv, leaving what incurs should parse.
///
/// Malformed spellings are left in place on purpose: incurs already knows how to
/// say "unknown option", and eating the next word would turn `--window` alone
/// into a command that quietly ran somewhere else.
pub fn take_window_flag(argv: Vec<String>) -> (Vec<String>, Target) {
    let mut out = Vec::with_capacity(argv.len());
    let mut target = Target::Current;
    let mut rest = argv.into_iter();
    while let Some(arg) = rest.next() {
        if let Some(value) = arg.strip_prefix("--window=") {
            match value.parse() {
                Ok(pid) => target = Target::Window(pid),
                Err(_) => out.push(arg.clone()),
            }
            continue;
        }
        if arg == "--window" {
            match rest.next() {
                Some(value) => match value.parse() {
                    Ok(pid) => target = Target::Window(pid),
                    Err(_) => {
                        out.push(arg);
                        out.push(value);
                    }
                },
                None => out.push(arg),
            }
            continue;
        }
        out.push(arg);
    }
    (out, target)
}

/// Whether this argv is about the caller's machine rather than the browser.
///
/// Metadata about the graph must answer with no browser running, and anything
/// that writes into the caller's shell or home has to happen in the caller's
/// process with the caller's umask. Everything else -- including `--json` and
/// the other output modifiers, which are how agents ask for their format --
/// belongs in the window.
pub fn runs_locally(argv: &[String]) -> bool {
    const FLAGS: &[&str] = &[
        "--help",
        "-h",
        "--version",
        "--llms",
        "--llms-full",
        "--schema",
        "--mcp",
        "--config-schema",
    ];
    if argv.iter().any(|a| FLAGS.contains(&a.as_str())) {
        return true;
    }
    let words: Vec<&str> =
        argv.iter().map(String::as_str).filter(|a| !a.starts_with('-')).collect();
    match words.as_slice() {
        // incurs' own builtins: they install completions, register MCP servers
        // and write skill files, all of it beside the caller.
        ["completions", ..] | ["mcp", ..] | ["skills", ..] | ["agent", ..] => true,
        // `config init` writes the caller's dotfile; `config show` is a question
        // about the running browser, and belongs there.
        ["config", "init", ..] => true,
        _ => false,
    }
}

/// The commands that mean "put something on screen", and can therefore answer
/// "no browser running" by starting one.
pub fn opens_a_window(argv: &[String]) -> bool {
    let words: Vec<&str> =
        argv.iter().map(String::as_str).filter(|a| !a.starts_with('-')).collect();
    matches!(words.as_slice(), ["tab", "open", ..] | ["window", "new", ..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A directory of our own, without touching the process environment: the
    /// tests run in one process and `set_var` is global to all of them.
    fn scratch(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("oma-browse-test-{name}-{stamp}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_window_flag_is_taken_out_of_the_argv() {
        for spelling in
            [argv(&["--window", "42", "tab", "list"]), argv(&["--window=42", "tab", "list"])]
        {
            let (rest, target) = take_window_flag(spelling);
            assert_eq!(rest, argv(&["tab", "list"]));
            assert_eq!(target, Target::Window(42));
        }
    }

    #[test]
    fn a_malformed_window_flag_is_left_for_incurs() {
        // The failure to avoid is `--window` swallowing `tab` and the command
        // running somewhere the user did not ask for.
        let (rest, target) = take_window_flag(argv(&["--window", "tab", "list"]));
        assert_eq!(rest, argv(&["--window", "tab", "list"]));
        assert_eq!(target, Target::Current);

        let (rest, target) = take_window_flag(argv(&["--window"]));
        assert_eq!(rest, argv(&["--window"]));
        assert_eq!(target, Target::Current);
    }

    #[test]
    fn no_window_flag_means_the_focused_one() {
        let (rest, target) = take_window_flag(argv(&["tab", "open", "https://example.com"]));
        assert_eq!(rest, argv(&["tab", "open", "https://example.com"]));
        assert_eq!(target, Target::Current);
    }

    #[test]
    fn metadata_and_installers_run_locally() {
        for a in [
            argv(&["--help"]),
            argv(&["tab", "open", "--help"]),
            argv(&["--version"]),
            argv(&["--schema"]),
            argv(&["completions", "bash"]),
            argv(&["mcp", "add"]),
            argv(&["skills", "list"]),
            argv(&["config", "init"]),
        ] {
            assert!(runs_locally(&a), "{a:?} should run in this process");
        }
    }

    #[test]
    fn everything_about_the_browser_is_forwarded() {
        for a in [
            argv(&["tab", "list"]),
            argv(&["tab", "list", "--json"]),
            argv(&["tab", "list", "--format", "json"]),
            argv(&["page", "eval", "--js", "1"]),
            argv(&["theme", "show"]),
            // The running browser's resolved config is the truthful answer, and
            // it is the one that knows about problems found at startup.
            argv(&["config", "show"]),
        ] {
            assert!(!runs_locally(&a), "{a:?} should reach the window");
        }
    }

    #[test]
    fn only_the_opening_commands_may_start_a_browser() {
        assert!(opens_a_window(&argv(&["tab", "open", "https://example.com"])));
        assert!(opens_a_window(&argv(&["window", "new"])));
        assert!(!opens_a_window(&argv(&["tab", "list"])));
        assert!(!opens_a_window(&argv(&["window", "close"])));
    }

    #[test]
    fn a_socket_path_names_its_window() {
        let dir = Path::new("/run/user/1000/oma-browse");
        let path = socket_in(dir, 4321);
        assert_eq!(path, dir.join("window-4321.sock"));
        assert_eq!(pid_of(&path), Some(4321));
        assert_eq!(pid_of(&dir.join("current.sock")), None);
        // `sun_path` is 108 bytes, and a socket that does not fit is a runtime
        // failure with a baffling message.
        assert!(path.as_os_str().len() < 100, "{} is too long to bind", path.display());
    }

    #[test]
    fn a_reply_without_stderr_still_parses() {
        // Forward compatibility of the envelope: an older window answering a
        // newer client must not fail to deserialise.
        let reply: Reply =
            serde_json::from_str(r#"{"v":1,"exit_code":null,"stdout":"ok"}"#).unwrap();
        assert_eq!(reply.stdout, "ok");
        assert!(reply.stderr.is_empty());
    }

    #[tokio::test]
    async fn a_dead_socket_is_swept_and_rebound() {
        let dir = scratch("rebind");
        let stale = socket_in(&dir, 999_999);
        std::fs::write(&stale, "").unwrap();

        let listener = bind_in(&dir, 999_999).await.unwrap();
        assert!(socket_in(&dir, 999_999).exists());
        assert_eq!(live_in(&dir), vec![999_999]);
        assert_eq!(socket_for(&dir, Target::Current), Some(current_in(&dir)));

        drop(listener);
        // Nothing listening: the file may remain, but it must not be mistaken
        // for a window.
        assert!(live_in(&dir).is_empty());
        assert_eq!(socket_for(&dir, Target::Current), None);
        sweep_in(&dir);
        assert!(!socket_in(&dir, 999_999).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn current_follows_the_last_window_to_ask() {
        let dir = scratch("current");
        let first = bind_in(&dir, 111_111).await.unwrap();
        let second = bind_in(&dir, 222_222).await.unwrap();
        assert_eq!(std::fs::read_link(current_in(&dir)).unwrap(), Path::new("window-222222.sock"));

        point_current_in(&dir, 111_111);
        assert_eq!(std::fs::read_link(current_in(&dir)).unwrap(), Path::new("window-111111.sock"));

        // The focused window closing leaves the link dangling; the next caller
        // must find the other window rather than give up.
        drop(first);
        std::fs::remove_file(socket_in(&dir, 111_111)).unwrap();
        assert_eq!(socket_for(&dir, Target::Current), Some(socket_in(&dir, 222_222)));

        drop(second);
        std::fs::remove_dir_all(&dir).ok();
    }
}
