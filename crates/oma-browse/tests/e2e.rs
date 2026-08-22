//! Every command this browser advertises, run against a real browser.
//!
//! The unit tests in `src/` check the pieces. This checks the thing: a window
//! is started, pages are loaded over a loopback web server, and each command is
//! typed at the shipped binary the way a person or an agent would type it. What
//! is asserted is what came back on stdout, and -- where a command's whole point
//! is an effect somewhere else -- the file it wrote, the page it changed, or the
//! desktop tool it handed the page to.
//!
//! ## Why one test function
//!
//! A browser is a single piece of mutable state with tabs in it. Two cases
//! running at once would take turns breaking each other's assumptions about
//! which tab is active, and `cargo test` runs test functions in parallel by
//! default. So the cases live in [`CASES`] and run in order inside one test,
//! which also lets the window be started once rather than seventy times, and
//! lets the last thing that happens be the coverage gate.
//!
//! ## The gate
//!
//! [`Browser::run`] records every `<group> <command>` it is asked to run. The
//! last case compares that record against the command list the binary itself
//! prints, and fails if the binary advertises something this suite never ran.
//! Adding a command to `commands.rs` therefore fails this test until it is
//! exercised here, which is the only way a coverage claim stays true.

// An integration test compiles without `cfg(test)`, so the workspace's ban on
// panicking constructs applies to it in full. In a test they are the assertions.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use serde_json::Value;

use harness::{Browser, boolean, field, integer, manifest, string, until};

/// A named thing to check, and the check.
type Case = (&'static str, fn(&Browser));

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

#[test]
fn every_command_works_against_a_real_browser() {
    let browser = Browser::start();

    let mut failures: Vec<(String, String)> = Vec::new();
    for (name, case) in CASES {
        let started = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| case(&browser)));
        let took = started.elapsed();
        match outcome {
            Ok(()) => println!("ok       {name} ({}ms)", took.as_millis()),
            Err(payload) => {
                println!("FAILED   {name} ({}ms)", took.as_millis());
                failures.push(((*name).to_string(), panic_message(&payload)));
            }
        }
    }

    // The gate, last, so it sees everything the cases ran.
    let advertised = manifest(&browser);
    let covered = browser.covered();
    let missed: Vec<&String> = advertised.difference(&covered).collect();

    let log = browser.log_tail();
    browser.stop();

    let mut report = String::new();
    if !failures.is_empty() {
        report.push_str(&format!("\n{} case(s) failed:\n", failures.len()));
        for (name, why) in &failures {
            report.push_str(&format!("  - {name}: {why}\n"));
        }
    }
    if !missed.is_empty() {
        report.push_str(&format!(
            "\n{} command(s) the browser advertises are never run by this suite:\n",
            missed.len()
        ));
        for name in &missed {
            report.push_str(&format!("  - oma-browse {name}\n"));
        }
        report.push_str(
            "\nAdd a case to CASES in tests/e2e.rs for each. The point of this \
             gate is that a new command cannot ship untested.\n",
        );
    }
    assert!(report.is_empty(), "{report}\n--- the browser's last words ---\n{log}");

    println!(
        "\n{} cases, {} of {} advertised commands exercised end to end",
        CASES.len(),
        covered.intersection(&advertised).count(),
        advertised.len()
    );
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panicked with something unprintable".to_string())
}

// ---------------------------------------------------------------------------
// Shared moves
// ---------------------------------------------------------------------------

/// The active tab, as the browser reports it.
fn active(browser: &Browser) -> Value {
    let list = browser.json(&["tab", "list"]);
    let tabs = field(&list, "tabs").as_array().expect("tabs is a list").clone();
    tabs.into_iter()
        .find(|tab| tab.get("active").and_then(Value::as_bool) == Some(true))
        .unwrap_or_else(|| panic!("no tab is active in {list}"))
}

fn active_url(browser: &Browser) -> String {
    string(&active(browser), "url")
}

fn open_tabs(browser: &Browser) -> Vec<Value> {
    let list = browser.json(&["tab", "list"]);
    field(&list, "tabs").as_array().expect("tabs is a list").clone()
}

fn wait_for_url(browser: &Browser, want: &str) {
    until(&format!("the active tab to be at {want}"), Duration::from_secs(20), || {
        active_url(browser) == want
    });
}

/// Back to one tab on the home fixture, so the next case starts where the last
/// one did. Cases that leave a mess call this; cases that need it call it too,
/// because the cheapest way to be independent is to not rely on anyone else.
fn reset(browser: &Browser) {
    for tab in open_tabs(browser) {
        if !tab.get("active").and_then(Value::as_bool).unwrap_or(false) {
            browser.json(&["tab", "close", &integer(&tab, "id").to_string()]);
        }
    }
    browser.json(&["nav", "home"]);
    browser.json(&["page", "wait"]);
}

// ---------------------------------------------------------------------------
// The cases, in the order they run
// ---------------------------------------------------------------------------

const CASES: &[Case] = &[
    ("config: show reports the file this browser is actually running on", config_show),
    ("config: init refuses to overwrite a config that exists", config_init),
    ("nav: go loads a page and reports where it went", nav_go),
    ("nav: home goes to the configured start page", nav_home),
    ("nav: reload puts the page back the way it was", nav_reload),
    ("nav: back and forward walk the tab's own history", nav_back_and_forward),
    ("nav: stop is accepted while a page is loading", nav_stop),
    ("nav: login answers a site that asked for a password", nav_login),
    ("nav: trust takes a host on request and complains without one", nav_trust),
    ("page: text and markdown extract the article and drop the rest", page_text_and_markdown),
    ("page: source is the markup the server sent", page_source),
    ("page: eval runs JavaScript and hands back the value", page_eval),
    ("page: click presses what a selector names", page_click),
    ("page: fill types into an input and the page notices", page_fill),
    ("page: wait waits for a selector and for text that arrive late", page_wait),
    ("page: console has every level the page logged", page_console),
    ("page: network has the document and its subresources", page_network),
    ("page: screenshot writes a PNG of the size it claims", page_screenshot),
    ("page: print writes a PDF into the downloads directory", page_print),
    ("page: reader replaces the page with its article", page_reader),
    ("page: zoom steps the ladder and comes back to 1", page_zoom),
    ("page: hints labels the links and clears them again", page_hints),
    ("page: devtools opens the inspector and closes it", page_devtools),
    ("find: text counts the matches, and next, previous and clear follow it", find_text),
    ("tab: open, list and select move between real tabs", tab_open_list_select),
    ("tab: cycle wraps around the open tabs", tab_cycle),
    ("tab: close and reopen bring the same page back", tab_close_and_reopen),
    ("tab: mute silences a tab and lets it speak again", tab_mute),
    ("tab: restore is idempotent and says what it did", tab_restore),
    ("history: list has the pages that were visited, and clear empties it", history_list_and_clear),
    ("data: list says what a site stored, and clear takes it away by kind", data_list_and_clear),
    ("bookmark: add, list and remove keep and forget a page", bookmark_add_list_remove),
    ("download: a file arrives, and the list commands act on it", download_lifecycle),
    ("share: copy, terminal and webapp hand the page to the desktop", share_commands),
    ("permission: a page asks, decide answers, and list remembers", permission_round_trip),
    ("permission: allow, deny and forget decide without being asked", permission_written_by_hand),
    ("content: list, off, on and reload report what is blocking", content_commands),
    ("theme: css, show and reload describe the theme being worn", theme_describes_itself),
    ("theme: recolor turns page repainting off and on", theme_recolor),
    ("theme: hook installs Omarchy's theme-set hook and removes it", theme_hook),
    ("ui: palette shows, hides and toggles; dismiss puts it away", ui_commands),
    ("window: fullscreen goes both ways", window_fullscreen),
    ("window: resize asks the compositor and reports honestly", window_resize),
    ("window: new opens a second browser and close shuts it", window_new_and_close),
];

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

fn config_show(browser: &Browser) {
    let shown = browser.json(&["config", "show"]);
    assert_eq!(
        string(&shown, "path"),
        browser.config_path().display().to_string(),
        "config show named a different file than the one the browser was given"
    );
    assert!(boolean(&shown, "exists"));
    let settings = field(&shown, "settings");
    assert_eq!(
        string(settings, "home"),
        browser.web.url("/index.html"),
        "the running config is not the one on disk"
    );
}

fn config_init(browser: &Browser) {
    let before = std::fs::read_to_string(browser.config_path()).expect("the config is readable");
    let answer = browser.json(&["config", "init"]);
    assert_eq!(
        string(&answer, "code"),
        "config",
        "config init overwrote an existing file instead of refusing: {answer}"
    );
    assert!(
        string(&answer, "message").contains("already exists"),
        "the refusal did not say why: {answer}"
    );
    let after = std::fs::read_to_string(browser.config_path()).expect("the config is readable");
    assert_eq!(before, after, "config init changed the file it said it would not touch");
}

// ---------------------------------------------------------------------------
// nav
// ---------------------------------------------------------------------------

fn nav_go(browser: &Browser) {
    let url = browser.web.url("/article.html");
    let answer = browser.json(&["nav", "go", &url]);
    assert_eq!(string(&answer, "url"), url);
    wait_for_url(browser, &url);
    // The title arrives with the document, a moment after the URL does, so
    // asserting it straight after the navigation reads back the URL instead.
    until("the tab to pick up the page's title", Duration::from_secs(20), || {
        string(&active(browser), "title") == "The Fixture Article"
    });
}

fn nav_home(browser: &Browser) {
    browser.visit("/article.html");
    let answer = browser.json(&["nav", "home"]);
    assert_eq!(string(&answer, "url"), browser.web.url("/index.html"));
    wait_for_url(browser, &browser.web.url("/index.html"));
}

fn nav_reload(browser: &Browser) {
    browser.visit("/index.html");
    // Something only this page instance knows, so a reload that did nothing
    // would leave it behind and be caught.
    browser.json(&["page", "eval", "window.__e2e_marker = 'before reload'"]);
    assert!(boolean(&browser.json(&["nav", "reload"]), "ok"));
    browser.json(&["page", "wait"]);
    until("the reloaded page to forget the marker", Duration::from_secs(15), || {
        let answer = browser.json(&["page", "eval", "String(window.__e2e_marker)"]);
        string(&answer, "result").contains("undefined")
    });

    // `--hard` is the same command with the cache thrown away first; it has to
    // work, and the page has to survive it.
    assert!(boolean(&browser.json(&["nav", "reload", "--hard"]), "ok"));
    browser.json(&["page", "wait"]);
    assert_eq!(string(&active(browser), "title"), "Fixture Home");
}

fn nav_back_and_forward(browser: &Browser) {
    let home = browser.web.url("/index.html");
    let article = browser.web.url("/article.html");
    browser.visit("/index.html");
    browser.visit("/article.html");
    wait_for_url(browser, &article);

    assert!(boolean(&browser.json(&["nav", "back"]), "ok"));
    wait_for_url(browser, &home);

    assert!(boolean(&browser.json(&["nav", "forward"]), "ok"));
    wait_for_url(browser, &article);
}

fn nav_stop(browser: &Browser) {
    browser.visit("/index.html");
    // Nothing is loading, and stopping nothing is still a thing a browser must
    // answer for rather than error on -- the key is bound whether or not the
    // page happens to be busy.
    assert!(boolean(&browser.json(&["nav", "stop"]), "ok"));
}

fn nav_login(browser: &Browser) {
    let url = browser.web.url("/auth");
    browser.json(&["nav", "go", &url]);
    // The challenge is answered by the browser's own sign-in page, not by the
    // site, so waiting on the URL would wait for ever.
    until("the sign-in page to come up", Duration::from_secs(20), || {
        string(&browser.json(&["page", "text"]), "title") == "Sign in"
    });

    let answer = browser.json(&["nav", "login", "fixture-user", "fixture-password"]);
    assert_eq!(string(&answer, "host"), "127.0.0.1");
    assert_eq!(string(&answer, "user"), "fixture-user");

    until("the site to open once the login was accepted", Duration::from_secs(20), || {
        string(&browser.json(&["page", "text"]), "title") == "Fixture Secret"
    });
    let text = browser.json(&["page", "text"]);
    assert!(
        string(&text, "content").contains("the door opened"),
        "the page behind the password never appeared: {text}"
    );
    reset(browser);
}

fn nav_trust(browser: &Browser) {
    // Nothing has been refused, so the bare form has nothing to do and says so
    // rather than pretending.
    let bare = browser.json(&["nav", "trust"]);
    assert_eq!(string(&bare, "code"), "usage");
    assert!(string(&bare, "message").contains("nothing to trust"));

    // Named, it takes the host anyway -- which is the form a person uses before
    // the refusal rather than after it.
    let named = browser.json(&["nav", "trust", "e2e.invalid"]);
    assert_eq!(string(&named, "host"), "e2e.invalid");
}

// ---------------------------------------------------------------------------
// page
// ---------------------------------------------------------------------------

fn page_text_and_markdown(browser: &Browser) {
    browser.visit("/article.html");

    let text = browser.json(&["page", "text"]);
    assert_eq!(string(&text, "title"), "The Fixture Article");
    let content = string(&text, "content");
    assert!(content.contains("portmanteau"), "the article body is missing from: {content}");
    assert!(
        !content.contains("Boilerplate that reader mode should drop"),
        "the footer survived extraction: {content}"
    );
    assert!(integer(&text, "chars") > 100);

    let markdown = browser.json(&["page", "markdown"]);
    let md = string(&markdown, "content");
    assert!(md.contains("portmanteau"), "the article body is missing from the markdown: {md}");
    assert!(md.contains('#'), "nothing in the markdown is a heading: {md}");
}

fn page_source(browser: &Browser) {
    browser.visit("/index.html");
    let source = browser.json(&["page", "source"]);
    let html = string(&source, "html");
    assert!(html.contains("<h1 id=\"heading\">"), "the served markup is missing: {html}");
    assert!(integer(&source, "bytes") > 0, "the markup was reported as empty: {source}");
    assert_eq!(string(&source, "url"), browser.web.url("/index.html"));
}

fn page_eval(browser: &Browser) {
    browser.visit("/index.html");
    let answer = browser.json(&["page", "eval", "6 * 7"]);
    assert_eq!(string(&answer, "result"), "42");

    let text = browser.json(&["page", "eval", "document.getElementById('heading').textContent"]);
    assert_eq!(string(&text, "result"), "\"Fixture Home\"");
}

fn page_click(browser: &Browser) {
    browser.visit("/index.html");
    let clicked = browser.json(&["page", "click", "#button"]);
    assert_eq!(string(&clicked, "selector"), "#button");
    assert_eq!(integer(&clicked, "matched"), 1);
    until("the button's handler to change the heading", Duration::from_secs(10), || {
        let answer =
            browser.json(&["page", "eval", "document.getElementById('heading').textContent"]);
        string(&answer, "result") == "\"clicked\""
    });

    // A selector nothing matches waits, then says so -- it must not report a
    // click that never happened.
    let missing = browser.json(&["page", "click", "#not-here", "--timeout", "500"]);
    assert!(
        missing.get("code").is_some() || integer(&missing, "matched") == 0,
        "clicking nothing was reported as a click: {missing}"
    );
}

fn page_fill(browser: &Browser) {
    browser.visit("/form.html");
    let filled = browser.json(&["page", "fill", "#text-input", "typed by the suite"]);
    assert_eq!(integer(&filled, "matched"), 1);
    let value = browser.json(&["page", "eval", "document.getElementById('text-input').value"]);
    assert_eq!(string(&value, "result"), "\"typed by the suite\"");

    // A textarea and a contenteditable are the other two things this is
    // documented to type into.
    browser.json(&["page", "fill", "#text-area", "in the textarea"]);
    let area = browser.json(&["page", "eval", "document.getElementById('text-area').value"]);
    assert_eq!(string(&area, "result"), "\"in the textarea\"");

    browser.json(&["page", "fill", "#editable", "in the editable div"]);
    let editable =
        browser.json(&["page", "eval", "document.getElementById('editable').textContent"]);
    assert_eq!(string(&editable, "result"), "\"in the editable div\"");
}

fn page_wait(browser: &Browser) {
    // Loading and idle first: with nothing outstanding this answers at once,
    // which is what every other case leans on.
    browser.visit("/index.html");
    let idle = browser.json(&["page", "wait"]);
    assert_eq!(string(&idle, "for"), "idle");

    // The element and the text both turn up several hundred milliseconds after
    // the document does, so a `wait` that returned immediately would be wrong
    // and this would catch it.
    browser.json(&["nav", "go", &browser.web.url("/slow.html")]);
    let selector = browser.json(&["page", "wait", "--selector", "#late"]);
    assert!(
        selector.get("code").is_none(),
        "waiting for an element that does arrive timed out: {selector}"
    );
    let exists = browser.json(&["page", "eval", "!!document.getElementById('late')"]);
    assert_eq!(string(&exists, "result"), "true");

    browser.json(&["nav", "go", &browser.web.url("/slow.html")]);
    let text = browser.json(&["page", "wait", "--text", "the late arrival"]);
    assert!(text.get("code").is_none(), "waiting for text that does arrive timed out: {text}");

    // And a selector that never arrives has to time out rather than hang.
    let never = browser.json(&["page", "wait", "--selector", "#never", "--timeout", "700"]);
    assert_eq!(string(&never, "code"), "timeout");
    reset(browser);
}

fn page_console(browser: &Browser) {
    browser.visit("/index.html");
    until("the page's console lines to arrive", Duration::from_secs(15), || {
        let answer = browser.json(&["page", "console"]);
        answer.get("lines").and_then(Value::as_array).is_some_and(|lines| lines.len() >= 3)
    });

    let all = browser.json(&["page", "console"]);
    let lines = field(&all, "lines").as_array().expect("lines is a list").clone();
    let texts: Vec<String> = lines.iter().map(|line| string(line, "text")).collect();
    assert!(texts.iter().any(|t| t.contains("fixture log line")), "no log line in {texts:?}");
    assert!(texts.iter().any(|t| t.contains("fixture warn line")), "no warn line in {texts:?}");
    assert!(texts.iter().any(|t| t.contains("fixture error line")), "no error line in {texts:?}");

    // `--level error` is the whole of what F12 was for, so it has to actually
    // narrow rather than merely be accepted.
    let errors = browser.json(&["page", "console", "--level", "error"]);
    let only = field(&errors, "lines").as_array().expect("lines is a list").clone();
    assert!(!only.is_empty(), "filtering to errors dropped the error too: {errors}");
    for line in &only {
        assert_eq!(string(line, "level"), "error", "a non-error survived --level error: {line}");
    }
}

fn page_network(browser: &Browser) {
    browser.visit("/requests.html");
    let document = browser.web.url("/requests.html");
    let stylesheet = browser.web.url("/style.css");

    until("the page's requests to be recorded", Duration::from_secs(15), || {
        let seen = browser.json(&["page", "network"]);
        let requests = seen.get("requests").and_then(Value::as_array).cloned().unwrap_or_default();
        requests.iter().any(|r| string(r, "url") == stylesheet)
    });

    let seen = browser.json(&["page", "network"]);
    let requests = field(&seen, "requests").as_array().expect("requests is a list").clone();
    let urls: Vec<String> = requests.iter().map(|r| string(r, "url")).collect();
    assert!(urls.contains(&document), "the document itself is not in {urls:?}");
    assert!(urls.contains(&stylesheet), "the stylesheet is not in {urls:?}");

    // The fixture fetches one thing that is not there, so `--failed` has
    // something to find and something to leave out.
    until("the deliberate 404 to be recorded", Duration::from_secs(15), || {
        let failed = browser.json(&["page", "network", "--failed"]);
        let requests =
            failed.get("requests").and_then(Value::as_array).cloned().unwrap_or_default();
        requests.iter().any(|r| string(r, "url").contains("missing-on-purpose"))
    });
    let failed = browser.json(&["page", "network", "--failed"]);
    let narrowed = field(&failed, "requests").as_array().expect("requests is a list").clone();
    assert!(
        narrowed.iter().all(|r| string(r, "url") != stylesheet),
        "--failed kept a request that succeeded: {failed}"
    );
}

fn page_screenshot(browser: &Browser) {
    browser.visit("/index.html");
    let shot = browser.json(&["page", "screenshot"]);
    let path = std::path::PathBuf::from(string(&shot, "path"));
    assert!(
        path.starts_with(browser.screenshots_dir()),
        "the screenshot went outside the configured directory: {}",
        path.display()
    );
    let bytes = std::fs::read(&path).expect("the screenshot is on disk");
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "that file is not a PNG");
    assert!(integer(&shot, "width") > 0 && integer(&shot, "height") > 0, "a zero-sized shot");
}

fn page_print(browser: &Browser) {
    browser.visit("/article.html");
    let printed = browser.json(&["page", "print"]);
    let path = std::path::PathBuf::from(string(&printed, "path"));
    assert!(
        path.starts_with(browser.downloads_dir()),
        "the PDF went outside the configured directory: {}",
        path.display()
    );
    until("the PDF to be written", Duration::from_secs(30), || {
        std::fs::metadata(&path).map(|m| m.len() > 0).unwrap_or(false)
    });
    let bytes = std::fs::read(&path).expect("the PDF is on disk");
    assert_eq!(&bytes[..5], b"%PDF-", "that file is not a PDF");
}

fn page_reader(browser: &Browser) {
    browser.visit("/article.html");
    let read = browser.json(&["page", "reader"]);
    assert!(integer(&read, "chars") > 100, "reader found no article: {read}");

    // The document is replaced rather than restyled, so the site's own
    // furniture is gone from the live DOM -- which is the difference between
    // this and a stylesheet.
    until("the page to be replaced by its article", Duration::from_secs(15), || {
        let answer = browser.json(&["page", "eval", "!!document.getElementById('junk')"]);
        string(&answer, "result") == "false"
    });

    // And `nav reload` puts the page back, as documented.
    browser.json(&["nav", "reload"]);
    browser.json(&["page", "wait"]);
    until("reload to bring the original page back", Duration::from_secs(15), || {
        let answer = browser.json(&["page", "eval", "!!document.getElementById('junk')"]);
        string(&answer, "result") == "true"
    });
}

fn page_zoom(browser: &Browser) {
    browser.visit("/index.html");
    let base = browser.json(&["page", "zoom", "reset"]);
    assert!((field(&base, "level").as_f64().expect("a level") - 1.0).abs() < 1e-6);

    let bigger = field(&browser.json(&["page", "zoom", "in"]), "level").as_f64().expect("a level");
    assert!(bigger > 1.0, "zooming in did not zoom in: {bigger}");

    browser.json(&["page", "zoom", "reset"]);
    let smaller = field(&browser.json(&["page", "zoom", "out"]), "level").as_f64().expect("level");
    assert!(smaller < 1.0, "zooming out did not zoom out: {smaller}");

    let back = browser.json(&["page", "zoom", "reset"]);
    assert!((field(&back, "level").as_f64().expect("a level") - 1.0).abs() < 1e-6);
}

fn page_hints(browser: &Browser) {
    browser.visit("/index.html");
    let shown = browser.json(&["page", "hints", "click"]);
    // Two links and a button are in the viewport, and every one of them should
    // have been given a label.
    assert!(integer(&shown, "shown") >= 3, "not everything clickable was labelled: {shown}");

    let labelled = browser.json(&[
        "page",
        "eval",
        "(document.getElementById('__oma_browse_hints') || {children:[]}).children.length",
    ]);
    assert_ne!(string(&labelled, "result"), "0", "nothing in the page was labelled: {labelled}");

    // `clear` takes them off the page. `ui dismiss`'s own `hints` field says
    // that the clearing script ran, not that there were hints to clear, so the
    // page is what has to be asked.
    browser.json(&["page", "hints", "clear"]);
    until("the hint labels to come off the page", Duration::from_secs(10), || {
        let left = browser.json(&[
            "page",
            "eval",
            "(document.getElementById('__oma_browse_hints') || {children:[]}).children.length",
        ]);
        string(&left, "result") == "0"
    });
}

fn page_devtools(browser: &Browser) {
    browser.visit("/index.html");
    assert!(boolean(&browser.json(&["page", "devtools", "show"]), "on"));
    assert!(!boolean(&browser.json(&["page", "devtools", "hide"]), "on"));
}

// ---------------------------------------------------------------------------
// find
// ---------------------------------------------------------------------------

fn find_text(browser: &Browser) {
    browser.visit("/article.html");
    let found = browser.json(&["find", "text", "portmanteau"]);
    assert_eq!(string(&found, "text"), "portmanteau");
    assert_eq!(integer(&found, "matches"), 1, "the one match was not counted once: {found}");

    let absent = browser.json(&["find", "text", "certainly-not-on-this-page"]);
    assert_eq!(integer(&absent, "matches"), 0, "a match was reported for nothing: {absent}");

    browser.json(&["find", "text", "paragraph"]);
    assert!(boolean(&browser.json(&["find", "next"]), "ok"));
    assert!(boolean(&browser.json(&["find", "previous"]), "ok"));
    assert!(boolean(&browser.json(&["find", "clear"]), "ok"));
}

// ---------------------------------------------------------------------------
// tab
// ---------------------------------------------------------------------------

fn tab_open_list_select(browser: &Browser) {
    reset(browser);
    let first = integer(&active(browser), "id");

    let opened = browser.json(&["tab", "open", &browser.web.url("/article.html")]);
    let second = integer(&opened, "id");
    assert_ne!(second, first, "tab open reused the tab that was already there");
    assert!(boolean(&opened, "active"), "a newly opened tab is the one you are looking at");

    let tabs = open_tabs(browser);
    assert_eq!(tabs.len(), 2, "there should be exactly two tabs: {tabs:?}");

    let selected = browser.json(&["tab", "select", &first.to_string()]);
    let active_now = field(&selected, "tabs")
        .as_array()
        .expect("tabs is a list")
        .iter()
        .find(|t| t.get("active").and_then(Value::as_bool) == Some(true))
        .cloned()
        .expect("something is active");
    assert_eq!(integer(&active_now, "id"), first);

    // By position as well as by id. Positions are 1-based -- Ctrl-1 is the
    // first tab -- and a negative one counts from the end.
    browser.json(&["tab", "select", "--index", "2"]);
    assert_eq!(integer(&active(browser), "id"), second, "--index 2 is the second tab");

    browser.json(&["tab", "select", "--index", "-1"]);
    assert_eq!(integer(&active(browser), "id"), second, "--index -1 is the last tab");

    browser.json(&["tab", "select", "--index", "1"]);
    assert_eq!(integer(&active(browser), "id"), first, "--index 1 is the first tab");

    reset(browser);
}

fn tab_cycle(browser: &Browser) {
    reset(browser);
    let first = integer(&active(browser), "id");
    let second = integer(&browser.json(&["tab", "open", &browser.web.url("/form.html")]), "id");

    browser.json(&["tab", "cycle", "1"]);
    assert_eq!(integer(&active(browser), "id"), first, "cycling forward did not wrap around");

    // `--` first, because a bare `-1` is a flag as far as any argv parser is
    // concerned and would be rejected rather than counted.
    // `--json` before the `--`, or it is read as one of the arguments the
    // separator exists to protect.
    let back = browser.json(&["tab", "cycle", "--json", "--", "-1"]);
    assert!(back.get("code").is_none(), "cycling backwards was refused: {back}");
    assert_eq!(integer(&active(browser), "id"), second, "cycling back did not wrap around");

    reset(browser);
}

fn tab_close_and_reopen(browser: &Browser) {
    reset(browser);
    let url = browser.web.url("/article.html");
    let opened = browser.json(&["tab", "open", &url]);
    let id = integer(&opened, "id");

    let closed = browser.json(&["tab", "close", &id.to_string()]);
    assert_eq!(integer(&closed, "closed"), id);
    assert!(
        open_tabs(browser).iter().all(|t| integer(t, "id") != id),
        "the tab is still open after being closed"
    );

    let reopened = browser.json(&["tab", "reopen"]);
    assert_eq!(string(&reopened, "url"), url, "reopen brought back a different page");
    reset(browser);
}

fn tab_mute(browser: &Browser) {
    browser.visit("/index.html");
    assert!(boolean(&browser.json(&["tab", "mute", "on"]), "on"));
    assert!(!boolean(&browser.json(&["tab", "mute", "off"]), "on"));
}

fn tab_restore(browser: &Browser) {
    reset(browser);
    let before = open_tabs(browser).len();
    let restored = browser.json(&["tab", "restore"]);
    let opened = integer(&restored, "opened");
    let saved = integer(&restored, "saved");
    assert!(opened >= 0 && saved >= 0, "restore answered with nonsense: {restored}");
    assert!(
        opened <= saved,
        "restore claims to have opened more tabs than it had saved: {restored}"
    );

    // Documented to be safe to run twice: anything already open is skipped, so
    // the second run opens nothing at all.
    let again = browser.json(&["tab", "restore"]);
    assert_eq!(integer(&again, "opened"), 0, "running restore twice opened tabs twice: {again}");
    assert!(open_tabs(browser).len() >= before);
    reset(browser);
}

// ---------------------------------------------------------------------------
// history and bookmarks
// ---------------------------------------------------------------------------

fn history_list_and_clear(browser: &Browser) {
    browser.visit("/article.html");
    let article = browser.web.url("/article.html");

    until("the visit to reach the history", Duration::from_secs(15), || {
        let listed = browser.json(&["history", "list"]);
        let entries = listed.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
        entries.iter().any(|e| string(e, "url") == article)
    });

    let listed = browser.json(&["history", "list"]);
    let entry = field(&listed, "entries")
        .as_array()
        .expect("entries is a list")
        .iter()
        .find(|e| string(e, "url") == article)
        .cloned()
        .expect("the article is in the history");
    assert_eq!(string(&entry, "title"), "The Fixture Article");
    assert!(integer(&entry, "visits") >= 1);

    let cleared = browser.json(&["history", "clear"]);
    assert!(integer(&cleared, "cleared") >= 1, "clearing a full history cleared nothing");
    let after = browser.json(&["history", "list"]);
    assert!(
        field(&after, "entries").as_array().expect("entries is a list").is_empty(),
        "history survived being cleared: {after}"
    );
}

/// `data list` and `data clear`, on a real cookie jar.
///
/// The two properties worth holding are the ones that would be worst to get
/// wrong: a bare `data clear` must not sign anybody out, and `--host` must not
/// reach past the site it names.
fn data_list_and_clear(browser: &Browser) {
    browser.visit("/index.html");
    let host = "127.0.0.1";

    // Something of our own to find and then remove.
    browser.json(&[
        "page",
        "eval",
        "document.cookie='e2e=yes; max-age=3600; path=/'; localStorage.setItem('e2e','yes'); 1",
    ]);

    until("the cookie to reach WebKit's store", Duration::from_secs(15), || {
        let listed = browser.json(&["data", "list"]);
        let sites = listed.get("sites").and_then(Value::as_array).cloned().unwrap_or_default();
        sites.iter().any(|s| string(s, "name") == host)
    });

    // A bare clear is the cache, and the cache only. This is the line that keeps
    // "tidy up" from meaning "sign me out of everything".
    let cleared = browser.json(&["data", "clear"]);
    let kinds = field(&cleared, "kinds").as_array().expect("kinds is a list").clone();
    assert_eq!(kinds.len(), 1, "a bare clear took more than the cache: {cleared}");
    assert_eq!(kinds[0].as_str().unwrap_or_default(), "cache");

    let kept = browser.json(&["page", "eval", "document.cookie"]);
    assert!(
        string(&kept, "result").contains("e2e=yes"),
        "a bare `data clear` threw away a cookie: {kept}"
    );

    // Now ask for the cookies by name, for this host only.
    let gone = browser.json(&["data", "clear", "cookies", "--host", host]);
    let kinds = field(&gone, "kinds").as_array().expect("kinds is a list").clone();
    assert_eq!(kinds.len(), 1, "clearing cookies reported the wrong kinds: {gone}");
    assert_eq!(kinds[0].as_str().unwrap_or_default(), "cookies");
    let hosts = field(&gone, "hosts").as_array().expect("hosts is a list").clone();
    assert_eq!(hosts[0].as_str().unwrap_or_default(), host, "it cleared the wrong site: {gone}");

    until("the cookie to go", Duration::from_secs(15), || {
        let now = browser.json(&["page", "eval", "document.cookie"]);
        !string(&now, "result").contains("e2e=yes")
    });

    // ...and only the cookies: local storage was never asked for.
    let storage = browser.json(&["page", "eval", "localStorage.getItem('e2e')"]);
    assert!(
        string(&storage, "result").contains("yes"),
        "clearing cookies took local storage with it: {storage}"
    );

    // A kind nobody has is refused rather than silently doing nothing.
    let refused = browser.json(&["data", "clear", "everything"]);
    assert_eq!(string(&refused, "code"), "bad_kind", "an unknown kind was accepted: {refused}");
}

fn bookmark_add_list_remove(browser: &Browser) {
    browser.visit("/article.html");
    let url = browser.web.url("/article.html");

    let added = browser.json(&["bookmark", "add"]);
    assert!(boolean(&added, "added"));
    assert_eq!(string(&added, "url"), url);
    assert_eq!(string(&added, "title"), "The Fixture Article");

    let listed = browser.json(&["bookmark", "list"]);
    let entries = field(&listed, "entries").as_array().expect("entries is a list").clone();
    assert!(entries.iter().any(|e| string(e, "url") == url), "the bookmark is not listed");

    let removed = browser.json(&["bookmark", "remove"]);
    assert!(!boolean(&removed, "added"), "remove reported the page as still bookmarked");
    let after = browser.json(&["bookmark", "list"]);
    assert!(
        field(&after, "entries")
            .as_array()
            .expect("entries is a list")
            .iter()
            .all(|e| string(e, "url") != url),
        "the bookmark survived removal: {after}"
    );

    // A URL rather than the current page, which is the other half of the
    // command and the half a script uses.
    let other = browser.web.url("/form.html");
    assert!(boolean(&browser.json(&["bookmark", "add", &other]), "added"));
    browser.json(&["bookmark", "remove", &other]);
}

// ---------------------------------------------------------------------------
// downloads
// ---------------------------------------------------------------------------

fn download_lifecycle(browser: &Browser) {
    browser.json(&["download", "clear"]);
    browser.json(&["nav", "go", &browser.web.url("/download.bin")]);

    until("the download to finish", Duration::from_secs(30), || {
        let listed = browser.json(&["download", "list"]);
        let entries = listed.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
        entries
            .iter()
            .any(|e| string(e, "name").starts_with("fixture") && string(e, "state") != "running")
    });

    let listed = browser.json(&["download", "list"]);
    assert_eq!(
        string(&listed, "directory"),
        browser.downloads_dir().display().to_string(),
        "downloads are not going where the config says"
    );
    let entry = field(&listed, "entries")
        .as_array()
        .expect("entries is a list")
        .first()
        .cloned()
        .expect("something was downloaded");
    let path = std::path::PathBuf::from(string(&entry, "path"));
    let contents = std::fs::read_to_string(&path).expect("the downloaded file is on disk");
    assert_eq!(contents, "fixture download payload\n", "the file that arrived is not the one sent");

    // The three commands that hand a download to the desktop. Each is checked
    // by what it asked the desktop to do, which is the whole of what it does.
    browser.forget_handoffs();
    let copied = browser.json(&["download", "copy", "1"]);
    assert_eq!(string(&copied, "path"), path.display().to_string(), "the wrong entry: {copied}");
    assert!(
        browser
            .handoffs()
            .iter()
            .any(|line| line.starts_with("wl-copy") && line.contains(&path.display().to_string())),
        "download copy did not put the path on the clipboard: {:?}",
        browser.handoffs()
    );

    browser.forget_handoffs();
    browser.json(&["download", "open", "1"]);
    assert!(
        browser
            .handoffs()
            .iter()
            .any(|line| line.starts_with("xdg-open") && line.contains(&path.display().to_string())),
        "download open did not hand the file to the desktop: {:?}",
        browser.handoffs()
    );

    browser.forget_handoffs();
    browser.json(&["download", "reveal", "1"]);
    let directory = browser.downloads_dir().display().to_string();
    assert!(
        browser
            .handoffs()
            .iter()
            .any(|line| line.starts_with("xdg-open") && line.ends_with(&directory)),
        "download reveal did not open the folder: {:?}",
        browser.handoffs()
    );

    let cleared = browser.json(&["download", "clear"]);
    assert!(integer(&cleared, "cleared") >= 1);
    let after = browser.json(&["download", "list"]);
    assert!(field(&after, "entries").as_array().expect("entries is a list").is_empty());
    // The list is forgotten; the file is not, as documented.
    assert!(path.exists(), "clearing the list deleted the file it promised to leave alone");
    reset(browser);
}

// ---------------------------------------------------------------------------
// share
// ---------------------------------------------------------------------------

fn share_commands(browser: &Browser) {
    browser.visit("/article.html");
    let url = browser.web.url("/article.html");

    browser.forget_handoffs();
    let copied = browser.json(&["share", "copy"]);
    assert_eq!(string(&copied, "url"), url);
    assert_eq!(string(&copied, "via"), "wl-copy");
    assert!(
        browser.handoffs().iter().any(|line| line.starts_with("wl-copy") && line.contains(&url)),
        "share copy did not reach the clipboard: {:?}",
        browser.handoffs()
    );

    browser.forget_handoffs();
    let terminal = browser.json(&["share", "terminal"]);
    assert_eq!(string(&terminal, "via"), "ghostty");
    assert!(
        browser.handoffs().iter().any(|line| line.starts_with("ghostty") && line.contains(&url)),
        "share terminal did not open a terminal on the URL: {:?}",
        browser.handoffs()
    );

    browser.forget_handoffs();
    let webapp = browser.json(&["share", "webapp"]);
    assert_eq!(string(&webapp, "via"), "omarchy webapp");
    let installed = browser.handoffs();
    let call = installed
        .iter()
        .find(|line| line.starts_with("omarchy"))
        .unwrap_or_else(|| panic!("share webapp did not call omarchy: {installed:?}"));
    assert!(call.contains("webapp"), "the wrong omarchy subcommand: {call}");
    assert!(call.contains(&url), "the web app was installed on the wrong URL: {call}");
    // The launcher must run this browser, not whatever the allowlist prefers.
    assert!(call.contains("--app"), "the installed launcher does not open this browser: {call}");
}

// ---------------------------------------------------------------------------
// permissions
// ---------------------------------------------------------------------------

fn permission_round_trip(browser: &Browser) {
    browser.json(&["permission", "forget", &browser.web.base(), "geolocation"]);
    browser.json(&["nav", "go", &browser.web.url("/geolocation.html")]);
    browser.json(&["page", "wait"]);

    // The page asks; the browser holds the question until somebody answers it.
    until("the site to be waiting on a decision", Duration::from_secs(20), || {
        let waiting = browser.json(&["permission", "decide", "deny"]);
        waiting.get("code").is_none()
    });

    // And the page finds out, which is the part that proves the answer reached
    // WebKit rather than only the browser's own records.
    until("the page to be told it was refused", Duration::from_secs(20), || {
        let answer =
            browser.json(&["page", "eval", "document.getElementById('verdict').textContent"]);
        string(&answer, "result").contains("denied")
    });

    let listed = browser.json(&["permission", "list"]);
    let entry = field(&listed, "entries")
        .as_array()
        .expect("entries is a list")
        .iter()
        .find(|e| string(e, "kind") == "geolocation" && string(e, "origin") == browser.web.base())
        .cloned()
        .unwrap_or_else(|| panic!("the decision was not remembered: {listed}"));
    assert!(!boolean(&entry, "allowed"), "a refusal was recorded as a grant: {entry}");

    browser.json(&["permission", "forget", &browser.web.base(), "geolocation"]);
    reset(browser);
}

fn permission_written_by_hand(browser: &Browser) {
    let origin = "https://e2e.invalid";
    browser.json(&["permission", "forget", origin, "camera"]);

    let allowed = browser.json(&["permission", "allow", origin, "camera"]);
    assert!(boolean(&allowed, "allowed"));
    assert_eq!(string(&allowed, "origin"), origin);

    let denied = browser.json(&["permission", "deny", origin, "microphone"]);
    assert!(!boolean(&denied, "allowed"));

    let listed = browser.json(&["permission", "list"]);
    let entries = field(&listed, "entries").as_array().expect("entries is a list").clone();
    assert!(
        entries.iter().any(|e| string(e, "origin") == origin
            && string(e, "kind") == "camera"
            && boolean(e, "allowed")),
        "the grant is not in the list: {listed}"
    );

    let forgotten = browser.json(&["permission", "forget", origin, "camera"]);
    assert_eq!(integer(&forgotten, "forgotten"), 1);
    let after = browser.json(&["permission", "list"]);
    assert!(
        field(&after, "entries").as_array().expect("entries is a list").iter().all(|e| !(string(
            e, "origin"
        )
            == origin
            && string(e, "kind") == "camera")),
        "the grant survived being forgotten: {after}"
    );
    browser.json(&["permission", "forget", origin, "microphone"]);
}

// ---------------------------------------------------------------------------
// content blocking
// ---------------------------------------------------------------------------

fn content_commands(browser: &Browser) {
    browser.visit("/index.html");

    let listed = browser.json(&["content", "list"]);
    assert!(!boolean(&listed, "on"), "the test config has blocking off: {listed}");
    assert!(field(&listed, "lists").is_array());
    assert!(field(&listed, "problems").is_array());

    // Per tab, and both directions.
    let off = browser.json(&["content", "off"]);
    assert!(!boolean(&off, "blocking"));
    assert_eq!(string(&off, "url"), browser.web.url("/index.html"));

    let on = browser.json(&["content", "on"]);
    assert!(boolean(&on, "blocking"));

    let reloaded = browser.json(&["content", "reload"]);
    assert!(field(&reloaded, "lists").is_array(), "reload did not report the lists: {reloaded}");
    assert_eq!(boolean(&reloaded, "on"), boolean(&listed, "on"));
}

// ---------------------------------------------------------------------------
// userscripts
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// theme
// ---------------------------------------------------------------------------

fn theme_describes_itself(browser: &Browser) {
    let shown = browser.json(&["theme", "show"]);
    let name = string(&shown, "name");
    let mode = string(&shown, "mode");
    assert!(mode == "dark" || mode == "light", "a theme is one or the other: {shown}");
    assert!(integer(&shown, "colors") > 0, "a theme with no colours in it: {shown}");

    let css = string(&browser.json(&["theme", "css"]), "css");
    assert!(css.contains(":root"), "the CSS is not a rule: {css}");
    for property in ["--oma-canvas", "--oma-fg", "--oma-accent"] {
        assert!(css.contains(property), "{property} is missing from the theme CSS");
    }

    // Re-reading a theme that has not changed says so, which is how a hook
    // firing twice is told from a theme actually changing.
    let reloaded = browser.json(&["theme", "reload"]);
    assert_eq!(string(&reloaded, "name"), name);
    assert!(!boolean(&reloaded, "changed"), "an unchanged theme reported a change: {reloaded}");
}

fn theme_recolor(browser: &Browser) {
    browser.visit("/index.html");
    assert!(!boolean(&browser.json(&["theme", "recolor", "off"]), "recolor"));
    assert!(boolean(&browser.json(&["theme", "recolor", "on"]), "recolor"));
}

fn theme_hook(browser: &Browser) {
    let status = browser.json(&["theme", "hook", "status"]);
    let path = std::path::PathBuf::from(string(&status, "path"));
    if boolean(&status, "installed") {
        browser.json(&["theme", "hook", "uninstall"]);
    }

    let installed = browser.json(&["theme", "hook", "install"]);
    assert!(boolean(&installed, "installed"));
    let script = std::fs::read_to_string(&path).expect("the hook is on disk");
    assert!(script.starts_with("#!"), "the hook is not a script: {script}");
    assert!(script.contains("theme"), "the hook does not mention the theme: {script}");
    assert!(is_executable(&path), "the hook was written without the bit that lets it run");

    let after = browser.json(&["theme", "hook", "status"]);
    assert!(boolean(&after, "installed"));

    let removed = browser.json(&["theme", "hook", "uninstall"]);
    assert!(!boolean(&removed, "installed"));
    assert!(!path.exists(), "the hook file survived being uninstalled");
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// the interface
// ---------------------------------------------------------------------------

fn ui_commands(browser: &Browser) {
    assert!(boolean(&browser.json(&["ui", "palette", "show"]), "visible"));
    assert!(!boolean(&browser.json(&["ui", "palette", "hide"]), "visible"));
    assert!(boolean(&browser.json(&["ui", "palette", "toggle"]), "visible"));

    // Escape's command: it must report that it had the palette to put away.
    let dismissed = browser.json(&["ui", "dismiss"]);
    assert!(boolean(&dismissed, "palette"), "dismiss did not close an open palette: {dismissed}");

    // And with nothing open it is still answerable, because Escape is pressed
    // far more often with nothing to dismiss than with something.
    let again = browser.json(&["ui", "dismiss"]);
    assert!(!boolean(&again, "palette"), "the palette came back: {again}");
}

// ---------------------------------------------------------------------------
// the window
// ---------------------------------------------------------------------------

fn window_fullscreen(browser: &Browser) {
    assert!(boolean(&browser.json(&["window", "fullscreen", "on"]), "fullscreen"));
    assert!(!boolean(&browser.json(&["window", "fullscreen", "off"]), "fullscreen"));
}

fn window_resize(browser: &Browser) {
    let answer = browser.json(&["window", "resize", "900x700"]);

    // On Wayland a client cannot size itself, so this asks the compositor and
    // reads the size back. Without a compositor to ask -- CI, or a bare X
    // session -- the honest answer is an error, and the command is documented
    // to give one rather than to claim a resize that did not happen.
    if let Some(code) = answer.get("code") {
        assert_eq!(code, "window", "resize failed for an unexpected reason: {answer}");
        assert!(
            std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_err(),
            "resize failed with a compositor available: {answer}"
        );
        return;
    }

    assert!(field(&answer, "width").as_f64().is_some_and(|w| w > 0.0), "no width: {answer}");
    assert!(field(&answer, "height").as_f64().is_some_and(|h| h > 0.0), "no height: {answer}");
    // `applied` is allowed to be false -- a tiled window is sized by the layout
    // whatever anyone asks -- but it must then agree with the size reported.
    if boolean(&answer, "applied") {
        assert_eq!(
            field(&answer, "width").as_f64(),
            Some(900.0),
            "applied, but not to 900: {answer}"
        );
        assert_eq!(
            field(&answer, "height").as_f64(),
            Some(700.0),
            "applied, but not to 700: {answer}"
        );
    }
}

fn window_new_and_close(browser: &Browser) {
    let before = live_windows(browser);

    let opened = browser.json(&["window", "new", &browser.web.url("/article.html")]);
    let pid = integer(&opened, "pid");
    assert!(pid > 0, "window new did not say which process it started: {opened}");

    // Documented to answer only once the window will take a command, so the pid
    // it returns is usable immediately. Take it at its word and use it.
    let target = pid.to_string();
    let listed = browser.json(&["--window", &target, "tab", "list"]);
    let urls: Vec<String> = field(&listed, "tabs")
        .as_array()
        .expect("tabs is a list")
        .iter()
        .map(|t| string(t, "url"))
        .collect();
    assert!(
        urls.iter().any(|u| u == &browser.web.url("/article.html")),
        "the new window did not open the URL it was given: {urls:?}"
    );
    assert!(live_windows(browser) > before, "the new window is not listening");

    assert!(boolean(&browser.json(&["window", "close", "--window", &target]), "ok"));
    until("the second window to go", Duration::from_secs(20), || live_windows(browser) == before);
}

/// How many windows are answering on the control socket directory.
fn live_windows(browser: &Browser) -> usize {
    let Ok(entries) = std::fs::read_dir(browser.sockets_dir()) else { return 0 };
    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("window-")
                && name.ends_with(".sock")
                && std::os::unix::net::UnixStream::connect(entry.path()).is_ok()
        })
        .count()
}
