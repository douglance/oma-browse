//! What the page said, and what it fetched.
//!
//! Debugging a page in this browser used to mean the same thing it means in
//! every other one: open the inspector, look at it with your eyes, and be a
//! human sitting in front of a window. That is the one shape of work this
//! browser is otherwise built to avoid -- everything else is a command, in a
//! pipe, reachable from a script and from an agent. `page console` and
//! `page network` are the two that were missing.
//!
//! Two different mechanisms, because the two questions are different:
//!
//! * The console is the page's own, so it is observed from inside the page --
//!   [`inspect.js`](./inspect.js) wraps `console.*` and holds each line in a
//!   buffer that [`drain`] empties into this one. The originals are still
//!   called, so the inspector shows exactly what it always showed.
//!
//!   Collected rather than pushed, and for a reason worth writing down: WebKit's
//!   script-message handlers are named, but wry connects to
//!   `script-message-received` with no name filter at all
//!   (`wry-0.55.1/src/webkitgtk/mod.rs:638`), so a message posted under *any*
//!   name is handed to Tauri's IPC parser. Tauri cannot read a console line,
//!   complains with `console.error`, and the patch catches that and posts it
//!   again. That is an infinite loop and it was measured as one -- a few
//!   thousand lines a second, all of them `missing field \`cmd\``, before
//!   anything on the page had happened.
//! * The network is WebKit's, so it is observed from outside -- the
//!   `resource-load-started` signal reports every request the engine makes,
//!   including the ones no script can see: the document itself, stylesheets,
//!   images, redirects, and anything `fetch` was never involved in.
//!
//! Both are kept per tab, in a ring buffer that forgets its oldest entry rather
//! than growing without limit: a browser left open for a week on a page that
//! logs in a loop must not become a memory leak with a command attached.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::Serialize;
use tauri::webview::Webview;

use crate::state::AppState;

/// The object the page hangs its buffer off, and the one this module asks for
/// it by name. The two have to agree; the test at the bottom of this file is
/// what keeps them agreeing.
pub const CHANNEL: &str = "__omaConsole";

/// How many entries of each kind are kept, per tab.
///
/// Five hundred is roughly what a chatty single-page application produces in a
/// minute, and about as much as anybody reads in one go. `--limit` takes fewer;
/// nothing takes more, because the older ones are gone.
const KEEP: usize = 500;

const SCRIPT: &str = include_str!("inspect.js");

/// The console patch, ready to inject.
pub fn script() -> String {
    SCRIPT.to_string()
}

/// Sequence numbers, so `--since` means something across a whole browser rather
/// than only within one tab.
static NEXT: AtomicU64 = AtomicU64::new(1);

fn next_seq() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// How loud a console line was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Debug,
    Log,
    Info,
    Warn,
    Error,
}

impl Level {
    pub const ALL: [Level; 5] = [Level::Debug, Level::Log, Level::Info, Level::Warn, Level::Error];

    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Log => "log",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }

    /// Also accepts the spellings a person actually types: `warning`, `err`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "debug" | "verbose" => Some(Level::Debug),
            "log" => Some(Level::Log),
            "info" => Some(Level::Info),
            "warn" | "warning" => Some(Level::Warn),
            "error" | "err" => Some(Level::Error),
            _ => None,
        }
    }
}

/// One line the page logged.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Line {
    pub seq: u64,
    pub level: Level,
    pub text: String,
    /// `file:line:column`, when the page said. Empty for an ordinary
    /// `console.log`, which does not carry one.
    pub source: String,
    /// Unix milliseconds.
    pub at: u64,
}

/// One thing the engine fetched.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Exchange {
    pub seq: u64,
    pub method: String,
    pub url: String,
    /// `0` while the request is still in flight, or if it failed before a
    /// response arrived.
    pub status: u32,
    pub mime: String,
    pub bytes: u64,
    /// Unix milliseconds, at the moment the request started.
    pub at: u64,
    /// How long it took, in milliseconds. `0` until it finishes.
    pub ms: u64,
    /// Set when the load failed rather than completed, with what GLib said.
    pub failed: Option<String>,
}

/// How a request ended: what [`Inspector::finished`] fills in.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub status: u32,
    pub mime: String,
    pub bytes: u64,
    pub ms: u64,
    /// What GLib said, when the load failed rather than completed.
    pub failed: Option<String>,
}

/// Everything both signals have collected, by tab.
#[derive(Debug, Default)]
pub struct Inspector {
    console: HashMap<String, VecDeque<Line>>,
    network: HashMap<String, VecDeque<Exchange>>,
}

impl Inspector {
    fn push<T>(room: &mut VecDeque<T>, item: T) {
        if room.len() >= KEEP {
            room.pop_front();
        }
        room.push_back(item);
    }

    pub fn say(&mut self, tab: &str, line: Line) {
        Self::push(self.console.entry(tab.to_string()).or_default(), line);
    }

    /// Record a request that has just started, and answer with its sequence
    /// number so that whoever is watching it can come back and finish it.
    pub fn started(&mut self, tab: &str, exchange: Exchange) -> u64 {
        let seq = exchange.seq;
        Self::push(self.network.entry(tab.to_string()).or_default(), exchange);
        seq
    }

    /// Fill in what was only knowable once the response arrived.
    pub fn finished(&mut self, tab: &str, seq: u64, outcome: Outcome) {
        let Some(room) = self.network.get_mut(tab) else { return };
        let Some(entry) = room.iter_mut().find(|e| e.seq == seq) else { return };
        entry.status = outcome.status;
        entry.mime = outcome.mime;
        entry.bytes = outcome.bytes;
        entry.ms = outcome.ms;
        entry.failed = outcome.failed;
    }

    /// Console lines for one tab, oldest first.
    pub fn console_of(&self, tab: &str) -> Vec<Line> {
        self.console.get(tab).map(|room| room.iter().cloned().collect()).unwrap_or_default()
    }

    pub fn network_of(&self, tab: &str) -> Vec<Exchange> {
        self.network.get(tab).map(|room| room.iter().cloned().collect()).unwrap_or_default()
    }

    /// Forget one tab's console, leaving its network log alone.
    pub fn clear_console(&mut self, tab: &str) {
        self.console.remove(tab);
    }

    pub fn clear_network(&mut self, tab: &str) {
        self.network.remove(tab);
    }

    /// Throw away what has been collected: one tab's worth, or all of it.
    pub fn clear(&mut self, tab: Option<&str>) {
        match tab {
            Some(tab) => {
                self.console.remove(tab);
                self.network.remove(tab);
            }
            None => {
                self.console.clear();
                self.network.clear();
            }
        }
    }
}

/// Connect this webview's network tap.
///
/// The console needs no connecting: its side is the injected script, and this
/// side is [`drain`], which asks for what has piled up when somebody wants it.
#[cfg(target_os = "linux")]
pub fn install<R: tauri::Runtime>(view: &Webview<R>, state: Arc<AppState>) -> Result<()> {
    let label = view.label().to_string();
    view.with_webview(move |platform| {
        network(&platform.inner(), &label, &state);
    })
    .context("could not reach the webview to watch it")
}

#[cfg(not(target_os = "linux"))]
pub fn install<R: tauri::Runtime>(_view: &Webview<R>, _state: Arc<AppState>) -> Result<()> {
    Ok(())
}

/// Move whatever the active tab has logged since last time into its buffer.
///
/// Called by `page console` before it answers, so the cost is paid by whoever
/// asked rather than by every page all day. `--follow` calls it four times a
/// second, which is what makes a long session lose nothing.
///
/// A page that runs no scripts of ours -- the browser's own chrome, a PDF --
/// has no buffer to drain, and that is not an error: it is a page with nothing
/// to say.
pub async fn drain(state: &Arc<AppState>, tab: &str) {
    let js = format!("window.{CHANNEL} ? window.{CHANNEL}.drain() : \"[]\"");
    let Ok(raw) = crate::tabs::eval(state, &js).await else { return };
    // `eval` answers with JSON, and the page's answer is itself a JSON string,
    // so it arrives quoted and escaped.
    let inner = serde_json::from_str::<String>(&raw).unwrap_or(raw);
    let Ok(said) = serde_json::from_str::<Vec<Said>>(&inner) else {
        tracing::debug!(%inner, "a console buffer that was not ours");
        return;
    };
    if said.is_empty() {
        return;
    }
    let Ok(mut inspector) = state.inspector.lock() else { return };
    for one in said {
        inspector.say(
            tab,
            Line {
                seq: next_seq(),
                level: Level::parse(&one.level).unwrap_or(Level::Log),
                text: one.text,
                source: one.source,
                // The page's clock, not this one's: a line is timed when it was
                // logged, not when somebody got round to collecting it.
                at: if one.at > 0 { one.at } else { now_ms() },
            },
        );
    }
}

/// What [`inspect.js`](./inspect.js) holds. Its own type rather than [`Line`],
/// because the page supplies four of those fields and this process supplies the
/// sequence number -- and a page that could pick its own sequence number could
/// rewrite history.
#[derive(serde::Deserialize)]
struct Said {
    level: String,
    text: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    at: u64,
}

#[cfg(target_os = "linux")]
fn network(webview: &webkit2gtk::WebView, label: &str, state: &Arc<AppState>) {
    use webkit2gtk::{URIRequestExt, URIResponseExt, WebResourceExt, WebViewExt};

    let state = state.clone();
    let tab = label.to_string();
    webview.connect_resource_load_started(move |_view, resource, request| {
        let url = request.uri().map(|u| u.to_string()).unwrap_or_default();
        // WebKit reports the method as `NULL` for anything it did not build a
        // request object for, which in practice means a GET.
        let method = request.http_method().map(|m| m.to_string()).unwrap_or_else(|| "GET".into());

        let seq = next_seq();
        let exchange = Exchange {
            seq,
            method,
            url,
            status: 0,
            mime: String::new(),
            bytes: 0,
            at: now_ms(),
            ms: 0,
            failed: None,
        };
        if let Ok(mut inspector) = state.inspector.lock() {
            inspector.started(&tab, exchange);
        }

        let began = std::time::Instant::now();
        let done_state = state.clone();
        let done_tab = tab.clone();
        resource.connect_finished(move |resource| {
            let response = resource.response();
            let status = response.as_ref().map_or(0, URIResponseExt::status_code);
            let mime = response
                .as_ref()
                .and_then(|r| r.mime_type())
                .map(|m| m.to_string())
                .unwrap_or_default();
            let bytes = response.as_ref().map_or(0, URIResponseExt::content_length);
            if let Ok(mut inspector) = done_state.inspector.lock() {
                inspector.finished(
                    &done_tab,
                    seq,
                    Outcome {
                        status,
                        mime,
                        bytes,
                        ms: began.elapsed().as_millis() as u64,
                        failed: None,
                    },
                );
            }
        });

        let failed_state = state.clone();
        let failed_tab = tab.clone();
        resource.connect_failed(move |_resource, error| {
            if let Ok(mut inspector) = failed_state.inspector.lock() {
                inspector.finished(
                    &failed_tab,
                    seq,
                    Outcome {
                        ms: began.elapsed().as_millis() as u64,
                        failed: Some(error.to_string()),
                        ..Outcome::default()
                    },
                );
            }
        });
    });
}

/// A HAR 1.2 log, for anything that already reads one.
///
/// Deliberately partial and honestly so: WebKit's `resource-load-started` gives
/// a URL, a method, a status, a mime type and a size, and nothing at all about
/// headers, timings-within-the-request or bodies. A HAR with invented values in
/// those fields would be worse than one with empty ones, so they are empty.
pub fn har(url: &str, exchanges: &[Exchange]) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = exchanges
        .iter()
        .map(|e| {
            serde_json::json!({
                "startedDateTime": iso8601(e.at),
                "time": e.ms,
                "request": {
                    "method": e.method,
                    "url": e.url,
                    "httpVersion": "",
                    "cookies": [],
                    "headers": [],
                    "queryString": [],
                    "headersSize": -1,
                    "bodySize": -1,
                },
                "response": {
                    "status": e.status,
                    "statusText": "",
                    "httpVersion": "",
                    "cookies": [],
                    "headers": [],
                    "content": { "size": e.bytes, "mimeType": e.mime },
                    "redirectURL": "",
                    "headersSize": -1,
                    "bodySize": e.bytes,
                },
                "cache": {},
                "timings": { "send": 0, "wait": e.ms, "receive": 0 },
            })
        })
        .collect();

    serde_json::json!({
        "log": {
            "version": "1.2",
            "creator": { "name": "oma-browse", "version": env!("CARGO_PKG_VERSION") },
            "pages": [{
                "startedDateTime": iso8601(exchanges.first().map_or(0, |e| e.at)),
                "id": "page_1",
                "title": url,
                "pageTimings": {},
            }],
            "entries": entries,
        }
    })
}

/// Unix milliseconds as the ISO 8601 a HAR reader expects.
///
/// Written out by hand rather than pulling `chrono` in for one format string:
/// this is the only date this browser has ever needed to print.
fn iso8601(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let millis = ms % 1000;
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Howard Hinnant's `civil_from_days`, which is the shortest correct way to get
/// a date out of a day count without a calendar library.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(seq: u64, level: Level, text: &str) -> Line {
        Line { seq, level, text: text.to_string(), source: String::new(), at: 0 }
    }

    #[test]
    fn the_page_hangs_its_buffer_where_the_browser_looks_for_it() {
        assert!(
            script().contains(&format!("window.{CHANNEL} = {{")),
            "the page and the browser must agree on the channel's name"
        );
        assert!(
            script().contains("drain: function"),
            "the browser calls drain(); the page has to have one"
        );
    }

    /// The loop that made the first version of this unusable: a console patch
    /// that posts through WebKit reaches Tauri's IPC parser, which answers with
    /// `console.error`, which the patch catches and posts again.
    #[test]
    fn nothing_is_posted_through_webkit() {
        assert!(
            !script().contains("postMessage"),
            "posting reaches wry's unfiltered IPC handler and feeds itself"
        );
    }

    #[test]
    fn every_level_survives_a_round_trip() {
        for level in Level::ALL {
            assert_eq!(Level::parse(level.as_str()), Some(level), "{}", level.as_str());
        }
        assert_eq!(Level::parse("WARNING"), Some(Level::Warn));
        assert_eq!(Level::parse("shout"), None);
    }

    #[test]
    fn levels_are_ordered_by_how_loud_they_are() {
        assert!(Level::Error > Level::Warn);
        assert!(Level::Warn > Level::Info);
        assert!(Level::Debug < Level::Log);
    }

    #[test]
    fn the_oldest_line_is_the_one_that_goes() {
        let mut inspector = Inspector::default();
        for seq in 0..(KEEP as u64 + 10) {
            inspector.say("tab-1", line(seq, Level::Log, "hello"));
        }
        let kept = inspector.console_of("tab-1");
        assert_eq!(kept.len(), KEEP, "the buffer must not grow past its cap");
        assert_eq!(kept[0].seq, 10, "the ten oldest lines should be gone");
    }

    #[test]
    fn one_tab_does_not_see_another_tabs_console() {
        let mut inspector = Inspector::default();
        inspector.say("tab-1", line(1, Level::Log, "mine"));
        inspector.say("tab-2", line(2, Level::Log, "theirs"));
        assert_eq!(inspector.console_of("tab-1").len(), 1);
        assert_eq!(inspector.console_of("tab-1")[0].text, "mine");
        assert!(inspector.console_of("tab-3").is_empty());
    }

    #[test]
    fn a_request_is_completed_in_place_rather_than_appended() {
        let mut inspector = Inspector::default();
        let seq = inspector.started(
            "tab-1",
            Exchange {
                seq: 7,
                method: "GET".into(),
                url: "https://example.com/".into(),
                status: 0,
                mime: String::new(),
                bytes: 0,
                at: 0,
                ms: 0,
                failed: None,
            },
        );
        inspector.finished(
            "tab-1",
            seq,
            Outcome { status: 200, mime: "text/html".into(), bytes: 1234, ms: 42, failed: None },
        );

        let seen = inspector.network_of("tab-1");
        assert_eq!(seen.len(), 1, "finishing a request must not add a second row");
        assert_eq!(seen[0].status, 200);
        assert_eq!(seen[0].bytes, 1234);
        assert_eq!(seen[0].ms, 42);
    }

    #[test]
    fn finishing_something_that_was_never_started_is_ignored() {
        let mut inspector = Inspector::default();
        inspector.finished("tab-1", 99, Outcome { status: 200, ..Outcome::default() });
        assert!(inspector.network_of("tab-1").is_empty());
    }

    #[test]
    fn clearing_one_tab_leaves_the_others_alone() {
        let mut inspector = Inspector::default();
        inspector.say("tab-1", line(1, Level::Log, "a"));
        inspector.say("tab-2", line(2, Level::Log, "b"));
        inspector.clear(Some("tab-1"));
        assert!(inspector.console_of("tab-1").is_empty());
        assert_eq!(inspector.console_of("tab-2").len(), 1);
        inspector.clear(None);
        assert!(inspector.console_of("tab-2").is_empty());
    }

    #[test]
    fn a_har_is_shaped_the_way_a_har_reader_expects() {
        let exchanges = vec![Exchange {
            seq: 1,
            method: "GET".into(),
            url: "https://example.com/app.js".into(),
            status: 200,
            mime: "text/javascript".into(),
            bytes: 90,
            at: 1_700_000_000_000,
            ms: 12,
            failed: None,
        }];
        let har = har("https://example.com/", &exchanges);
        assert_eq!(har["log"]["version"], "1.2");
        assert_eq!(har["log"]["entries"][0]["request"]["url"], "https://example.com/app.js");
        assert_eq!(har["log"]["entries"][0]["response"]["status"], 200);
    }

    #[test]
    fn a_timestamp_is_written_the_way_a_har_reads_one() {
        // 2023-11-14T22:13:20.000Z, checked against `date -u -d @1700000000`.
        assert_eq!(iso8601(1_700_000_000_000), "2023-11-14T22:13:20.000Z");
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        // A leap day, which is where a hand-written calendar goes wrong.
        assert_eq!(iso8601(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
    }
}
