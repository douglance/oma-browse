//! A real browser, started and driven the way a person or an agent drives one.
//!
//! Nothing here reaches into the crate. The suite runs the shipped binary,
//! sends it argv, and reads what comes back on stdout -- so what it proves is
//! what a user would find, including the parts of the path that unit tests
//! cannot see: the control socket, argv parsing, the window, WebKit, and the
//! JSON on the other end.
//!
//! Three things have to be true for that to be safe to run on a machine
//! somebody is using:
//!
//! * It must not touch the browser they have open. Every path the binary
//!   consults is moved -- config, state, the control socket directory, the
//!   cookie jar -- and `--profile` moves the last of those.
//! * It must not reach the network. The home page, the search engine and every
//!   URL the suite visits come from [`fixtures`].
//! * It must not hand anything to the desktop. `wl-copy`, `ghostty`,
//!   `xdg-open` and `omarchy` are shadowed by recording stubs on `PATH`, so the
//!   commands that shell out are exercised for real and the desktop stays as it
//!   was.
//!
//! `XDG_RUNTIME_DIR` is deliberately *not* moved. It is where Hyprland's own
//! socket lives, and a suite that moved it turned every `window resize` into
//! "could not connect to the compositor" -- a failure of the test rig reported
//! as a failure of the browser. The control socket is moved by naming it in the
//! config file instead, which is a supported setting rather than a trick.

pub mod fixtures;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long to wait for a freshly started window to answer.
const READY: Duration = Duration::from_secs(30);

/// How long any one command may take before the suite calls it hung. Generous:
/// `page wait` is allowed ten seconds of its own, and a debug build starting a
/// second window is not fast.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// A running browser and everything that was moved out of the way for it.
pub struct Browser {
    binary: PathBuf,
    root: PathBuf,
    config: PathBuf,
    sockets: PathBuf,
    downloads: PathBuf,
    screenshots: PathBuf,
    shims: PathBuf,
    shim_log: PathBuf,
    state: PathBuf,
    profile: String,
    child: Child,
    /// Every `<group> <command>` the suite has actually run. The coverage gate
    /// reads this rather than a hand-written list, because a hand-written list
    /// is a claim and this is a record.
    covered: Mutex<BTreeSet<String>>,
    pub web: fixtures::Fixtures,
}

impl Browser {
    /// Start a browser, and wait until it will take a command.
    pub fn start() -> Browser {
        // Said here rather than found out thirty seconds later, when the window
        // has failed to answer and the only clue is a GTK complaint at the
        // bottom of a log.
        assert!(
            std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some(),
            "this suite drives a real browser window and there is no display to put one on. \
             Run it from a desktop session, or headless with \
             `xvfb-run -a dbus-run-session -- cargo test`."
        );

        let web = fixtures::Fixtures::start().expect("could not start the fixture web server");

        // Short, because a Unix socket path has 108 bytes to live in and the
        // directory cargo hands a test is nowhere near short enough. The first
        // attempt at this put the socket under the scratch directory and the
        // browser answered `path must be shorter than SUN_LEN`.
        let root = std::env::temp_dir().join(format!("ob-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sockets = root.join("s");
        let downloads = root.join("dl");
        let screenshots = root.join("shot");
        let shims = root.join("bin");
        let state = root.join("state");
        for dir in [&root, &sockets, &downloads, &screenshots, &shims, &state] {
            std::fs::create_dir_all(dir).expect("could not make a test directory");
        }
        let shim_log = root.join("handoffs.log");
        write_shims(&shims, &shim_log);

        let config = root.join("config.toml");
        write_config(&config, &web, &sockets, &downloads, &screenshots);

        let binary = pin_binary();
        let profile = format!("e2e-{}", std::process::id());

        let log = std::fs::File::create(root.join("browser.log")).expect("could not make a log");
        let child = Command::new(&binary)
            .arg("--profile")
            .arg(&profile)
            .envs(environment(&config, &state, &shims, &shim_log))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().expect("could not share the log")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("could not start the browser");

        // If this process dies without running its teardown -- a panic that
        // aborts, a `kill -9`, a cancelled `cargo test` -- the browser would
        // otherwise sit on the desktop for ever. This waits for us to go and
        // then tidies up, and costs one sleeping shell.
        reap_on_exit(child.id(), &root, &binary);

        let browser = Browser {
            binary,
            root,
            config,
            sockets,
            downloads,
            screenshots,
            shims,
            shim_log,
            state,
            profile,
            child,
            covered: Mutex::new(BTreeSet::new()),
            web,
        };
        browser.wait_until_ready();
        browser
    }

    fn wait_until_ready(&self) {
        let deadline = Instant::now() + READY;
        let mut last = String::new();
        while Instant::now() < deadline {
            // A parsed answer to `tab list` is not readiness, which is what the
            // first version of this assumed and what CI caught: the control
            // server starts answering before the window exists, so `tab list`
            // comes back as an empty list while the very next command is told
            // "the window is not up yet". Readiness is a *tab*, because a tab
            // only exists once there is a webview to hold it.
            let output = self.try_run(&["tab", "list", "--json"]);
            if let Some(output) = output
                && output.status.success()
                && let Ok(value) =
                    serde_json::from_str::<Value>(&String::from_utf8_lossy(&output.stdout))
                && value.get("tabs").and_then(Value::as_array).is_some_and(|t| !t.is_empty())
            {
                return;
            }
            last = self.log_tail();
            std::thread::sleep(Duration::from_millis(250));
        }
        panic!("the browser did not answer within {}s.\n--- its log ---\n{last}", READY.as_secs());
    }

    /// Run a command against the browser and hand back whatever it said.
    ///
    /// The first two words are recorded as covered. That is the only bookkeeping
    /// the coverage gate does, and it is done here so that a command cannot be
    /// counted as tested without having actually been run.
    pub fn run(&self, args: &[&str]) -> Answer {
        if let (Some(group), Some(command)) = (args.first(), args.get(1))
            && !group.starts_with('-')
            && !command.starts_with('-')
        {
            let mut covered = self.covered.lock().expect("coverage set");
            covered.insert(format!("{group} {command}"));
        }
        let output = self
            .try_run(args)
            .unwrap_or_else(|| panic!("`{}` did not finish in time", args.join(" ")));
        Answer {
            command: args.join(" "),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Run a command and parse its JSON answer, failing loudly if it is not
    /// JSON -- which is itself a bug worth failing on, since every one of these
    /// commands is documented to answer an agent.
    pub fn json(&self, args: &[&str]) -> Value {
        let mut with_json: Vec<&str> = args.to_vec();
        if !with_json.contains(&"--json") {
            with_json.push("--json");
        }
        let answer = self.run(&with_json);
        answer.value()
    }

    fn try_run(&self, args: &[&str]) -> Option<std::process::Output> {
        let mut child = Command::new(&self.binary)
            .arg("--profile")
            .arg(&self.profile)
            .args(args)
            .envs(environment(&self.config, &self.state, &self.shims, &self.shim_log))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("could not run the browser CLI");

        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            match child.try_wait().expect("could not poll the CLI") {
                Some(_) => return child.wait_with_output().ok(),
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                None => std::thread::sleep(Duration::from_millis(25)),
            }
        }
    }

    /// Navigate the active tab to a fixture and wait for it to settle, which is
    /// what nearly every case wants before it asserts anything.
    pub fn visit(&self, path: &str) -> Value {
        let url = self.web.url(path);
        let answer = self.json(&["nav", "go", &url]);
        self.json(&["page", "wait"]);
        answer
    }

    /// What a shimmed desktop helper was asked to do, one invocation per line.
    pub fn handoffs(&self) -> Vec<String> {
        std::fs::read_to_string(&self.shim_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Forget the recorded handoffs, so a case can assert on its own.
    pub fn forget_handoffs(&self) {
        let _ = std::fs::write(&self.shim_log, "");
    }

    pub fn downloads_dir(&self) -> &Path {
        &self.downloads
    }

    pub fn screenshots_dir(&self) -> &Path {
        &self.screenshots
    }

    /// Where the isolated config file lives, for the cases that read it back.
    pub fn config_path(&self) -> &Path {
        &self.config
    }

    /// The control socket directory, so a case can count windows.
    pub fn sockets_dir(&self) -> &Path {
        &self.sockets
    }

    /// Which of the commands the suite has run so far.
    pub fn covered(&self) -> BTreeSet<String> {
        self.covered.lock().expect("coverage set").clone()
    }

    /// The browser's own log, for a failure message worth reading.
    pub fn log_tail(&self) -> String {
        let text = std::fs::read_to_string(self.root.join("browser.log")).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(30)..].join("\n")
    }

    /// Close the window and take the temporary tree with it.
    pub fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.binary);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The environment every invocation gets: the isolated paths, and a `PATH` with
/// the recording stubs in front of the real desktop tools.
fn environment(
    config: &Path,
    state: &Path,
    shims: &Path,
    shim_log: &Path,
) -> Vec<(String, String)> {
    let path = match std::env::var("PATH") {
        Ok(rest) => format!("{}:{rest}", shims.display()),
        Err(_) => shims.display().to_string(),
    };
    vec![
        ("OMA_BROWSE_CONFIG".into(), config.display().to_string()),
        ("XDG_STATE_HOME".into(), state.display().to_string()),
        ("XDG_CONFIG_HOME".into(), state.join("config").display().to_string()),
        ("XDG_DATA_HOME".into(), state.join("data").display().to_string()),
        ("PATH".into(), path),
        ("OMA_E2E_LOG".into(), shim_log.display().to_string()),
        // Quiet, except when something is being chased.
        (
            "RUST_LOG".into(),
            std::env::var("OMA_E2E_RUST_LOG").unwrap_or_else(|_| "oma_browse=info".into()),
        ),
    ]
}

/// The dotfile the browser under test reads.
///
/// Written rather than defaulted, because three of these settings are the
/// difference between a suite that is safe to run and one that downloads into
/// somebody's real `~/Downloads` and searches DuckDuckGo for its fixtures.
fn write_config(
    path: &Path,
    web: &fixtures::Fixtures,
    sockets: &Path,
    downloads: &Path,
    screenshots: &Path,
) {
    let config = format!(
        r#"# Written by the end-to-end suite. Everything here points somewhere
# temporary on purpose.
home = "{home}"
search = "{base}/index.html?q={{query}}"

[control]
socket = "{sockets}"

[downloads]
dir = "{downloads}"
notify = false

[screenshot]
dir = "{screenshots}"

[startup]
restore = false

[history]
enabled = true

[window]
width = 1100
height = 800

[content]
block = false
"#,
        home = web.url("/index.html"),
        base = web.base(),
        sockets = sockets.display(),
        downloads = downloads.display(),
        screenshots = screenshots.display(),
    );
    std::fs::write(path, config).expect("could not write the test config");
}

/// Stand-ins for the desktop tools the browser hands pages to.
///
/// Each one appends its argv to a log and exits 0, so the command under test
/// takes its success path and the suite can still say exactly what was handed
/// over. Shadowing them on `PATH` is what makes `share terminal` testable at
/// all: the real one opens a terminal window and waits.
fn write_shims(dir: &Path, log: &Path) {
    for name in ["wl-copy", "wl-paste", "ghostty", "xdg-open", "omarchy", "notify-send"] {
        let path = dir.join(name);
        let script = format!(
            "#!/bin/sh\n\
             # Recording stub installed by the oma-browse end-to-end suite.\n\
             printf '%s' \"{name}\" >> \"{log}\"\n\
             for arg in \"$@\"; do printf '\\t%s' \"$arg\" >> \"{log}\"; done\n\
             printf '\\n' >> \"{log}\"\n\
             exit 0\n",
            name = name,
            log = log.display(),
        );
        std::fs::write(&path, script).expect("could not write a shim");
        set_executable(&path);
    }
    // `wl-copy` reads the payload from argv here, but the real one also takes
    // it on stdin; the stub must not block if it is used that way.
    let _ = std::fs::OpenOptions::new().create(true).append(true).open(log);
}

fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let mut permissions = std::fs::metadata(path).expect("could not stat a shim").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("could not chmod a shim");
}

/// Copy the binary under test and run the copy.
///
/// `cargo` rewrites `target/debug/oma-browse` in place, so a build started
/// while this suite is running -- another session on the same checkout, a
/// second `cargo test`, CI doing both jobs at once -- replaces the file the
/// window was started from underneath it. What that produces is not a clean
/// failure: `window new` reports "No such file or directory", `nav go` reports
/// "no webview labelled tab-0", and `nav login` simply times out. Three cases
/// fail and every one of them reads like a real bug in the browser.
///
/// The copy is made *beside* the original rather than inside the test's own
/// directory, and that placement is load-bearing: the binary finds its client
/// runtime by its own path, so a copy in `/tmp` would come up with no palette
/// at all. `target/debug/oma-browse-e2e-<pid>` keeps `../assets` where the
/// binary expects to find it.
fn pin_binary() -> PathBuf {
    let built = PathBuf::from(env!("CARGO_BIN_EXE_oma-browse"));
    let pinned = built.with_file_name(format!("oma-browse-e2e-{}", std::process::id()));
    // A failed copy is not fatal: running the original is what this suite did
    // before, and it is better than refusing to run at all.
    if std::fs::copy(&built, &pinned).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&pinned, std::fs::Permissions::from_mode(0o755));
        }
        return pinned;
    }
    built
}

/// Kill the browser and delete the tree once this test process has gone.
///
/// A sleeping `sh` rather than anything cleverer: the suite has no `unsafe` to
/// spend on `prctl`, and a `Drop` impl does not run when the process aborts.
fn reap_on_exit(browser: u32, root: &Path, binary_path: &Path) {
    let me = std::process::id();
    let script = format!(
        "while kill -0 {me} 2>/dev/null; do sleep 1; done; \
         kill {browser} 2>/dev/null; sleep 1; kill -9 {browser} 2>/dev/null; \
         rm -rf '{root}' '{binary}'",
        root = root.display(),
        binary = binary_path.display()
    );
    let _ = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// One command's answer, with enough context to say what went wrong.
pub struct Answer {
    pub command: String,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Answer {
    /// The answer as JSON.
    pub fn value(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "`{}` did not answer with JSON: {e}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                self.command, self.stdout, self.stderr
            )
        })
    }

    /// Assert the command succeeded, showing what it said if it did not.
    pub fn ok(&self) -> &Answer {
        assert!(
            self.code.unwrap_or(0) == 0,
            "`{}` exited {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.command,
            self.code,
            self.stdout,
            self.stderr
        );
        self
    }
}

/// Read a field out of an answer, failing with the whole answer if it is
/// missing -- which is more useful than `None` when the shape has changed.
pub fn field<'v>(value: &'v Value, key: &str) -> &'v Value {
    value
        .get(key)
        .unwrap_or_else(|| panic!("no `{key}` in {}", serde_json::to_string_pretty(value).unwrap()))
}

pub fn string(value: &Value, key: &str) -> String {
    field(value, key)
        .as_str()
        .unwrap_or_else(|| panic!("`{key}` is not a string in {value}"))
        .into()
}

pub fn integer(value: &Value, key: &str) -> i64 {
    field(value, key).as_i64().unwrap_or_else(|| panic!("`{key}` is not a number in {value}"))
}

pub fn boolean(value: &Value, key: &str) -> bool {
    field(value, key).as_bool().unwrap_or_else(|| panic!("`{key}` is not a boolean in {value}"))
}

/// Poll until a condition holds, so a case can wait on the browser without
/// sleeping for the worst case every time.
pub fn until(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what} after {:?}", timeout);
}

/// The commands this build actually has.
///
/// Read from the binary's own manifest rather than from a list in the suite:
/// the point of the gate is to notice a command that was *added*, and a list
/// the suite maintains cannot notice that.
pub fn manifest(browser: &Browser) -> BTreeSet<String> {
    let answer = browser.run(&["--llms"]);
    answer.ok();
    let mut found = BTreeSet::new();
    for line in answer.stdout.lines() {
        let Some(rest) = line.trim_start().strip_prefix("| `oma-browse ") else { continue };
        let Some(invocation) = rest.split('`').next() else { continue };
        let words: Vec<&str> = invocation.split_whitespace().collect();
        // `<group> <command>`, dropping the `[argument]` placeholders that
        // follow it.
        if words.len() >= 2 && !words[1].starts_with('[') {
            found.insert(format!("{} {}", words[0], words[1]));
        }
    }
    assert!(!found.is_empty(), "the manifest listed no commands:\n{}", answer.stdout);
    found
}
