//! `--follow`, which is the one thing a command cannot do on its own.
//!
//! Every command here answers exactly once: argv goes down the control socket,
//! the window runs it, one reply comes back (see [`crate::control`]). That is
//! the right shape for all but two of them. `page console` and `page network`
//! are logs, and a log is worth watching.
//!
//! So following is done on the CLI's side of the socket rather than the
//! browser's: ask, print what is new, remember the sequence number the answer
//! came back with, ask again. The browser stays a request/response server, the
//! ring buffers stay the only state, and `oma-browse page console --follow`
//! behaves like `tail -f` -- including surviving a navigation, a crash of the
//! page, and the tab being closed and another opened.
//!
//! Polling rather than pushing, and honestly: at four times a second against a
//! Unix socket the cost is not measurable, and the alternative is a streaming
//! protocol for two commands.

use std::io::Write as _;
use std::path::Path;

use anyhow::Result;

use crate::control::{self, Failure, Request, Target};

/// How often to ask again.
const EVERY: std::time::Duration = std::time::Duration::from_millis(250);

/// How much backlog the first answer carries, when the caller did not say.
/// `tail -f` shows ten lines before it starts following; this shows twenty.
const BACKLOG: usize = 20;

/// A `--follow` invocation, taken apart.
#[derive(Debug, Clone)]
pub struct Follow {
    /// `console` or `network`.
    what: String,
    /// The argv to send each time, with `--follow` taken out and `--json` put
    /// in. `--since` is appended per round.
    argv: Vec<String>,
    /// Whether the caller asked for a particular number of entries.
    limited: bool,
}

/// Recognise a following command. `None` for everything else, which is nearly
/// everything.
pub fn wanted(argv: &[String]) -> Option<Follow> {
    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
    if words.first() != Some(&"page") {
        return None;
    }
    let what = match words.get(1) {
        Some(&"console") => "console",
        Some(&"network") => "network",
        _ => return None,
    };
    if !words.contains(&"--follow") {
        return None;
    }

    let mut kept: Vec<String> = Vec::with_capacity(argv.len());
    let mut limited = false;
    let mut skip_next = false;
    for word in argv {
        if skip_next {
            skip_next = false;
            continue;
        }
        match word.as_str() {
            "--follow" => continue,
            // Ours to set, per round; a caller's is a starting point we honour
            // for the first ask and then move past.
            "--since" => {
                skip_next = true;
                continue;
            }
            w if w.starts_with("--since=") => continue,
            "--limit" => {
                limited = true;
                skip_next = true;
                kept.push(word.clone());
                continue;
            }
            w if w.starts_with("--limit=") => limited = true,
            _ => {}
        }
        kept.push(word.clone());
    }
    // The reply has to be parsed rather than read, so ask for the machine's
    // rendering however the caller's terminal is set up.
    if !kept.iter().any(|w| w == "--json") {
        kept.push("--json".to_string());
    }
    Some(Follow { what: what.to_string(), argv: kept, limited })
}

/// Follow until interrupted, or until the window goes away.
pub async fn run(dir: &Path, target: Target, follow: Follow) -> Result<()> {
    let mut since: u64 = 0;
    let mut first = true;
    let mut out = std::io::stdout();

    loop {
        let mut argv = follow.argv.clone();
        argv.push("--since".to_string());
        argv.push(since.to_string());
        // Only the first ask carries a backlog; after that "since" is the whole
        // of what is wanted, and a limit would silently drop the middle of a
        // burst.
        if first && !follow.limited {
            argv.push("--limit".to_string());
            argv.push(BACKLOG.to_string());
        }

        let request = Request::from_process(argv);
        match control::forward(dir, target, &request).await {
            Ok(reply) => {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&reply.stdout) else {
                    // A command that failed answers with prose, not JSON. Say it
                    // once and stop, rather than repeating it four times a
                    // second forever.
                    anyhow::bail!("{}", reply.stdout.trim());
                };
                since = value.get("next").and_then(serde_json::Value::as_u64).unwrap_or(since);
                for line in render(&follow.what, &value) {
                    writeln!(out, "{line}")?;
                }
                let _ = out.flush();
            }
            Err(Failure::NoWindow) => anyhow::bail!("the browser window has gone"),
            Err(e) => anyhow::bail!("{e}"),
        }

        first = false;
        tokio::time::sleep(EVERY).await;
    }
}

/// One answer, as lines fit for a terminal.
fn render(what: &str, value: &serde_json::Value) -> Vec<String> {
    let empty = Vec::new();
    match what {
        "console" => value
            .get("lines")
            .and_then(serde_json::Value::as_array)
            .unwrap_or(&empty)
            .iter()
            .map(console_line)
            .collect(),
        _ => value
            .get("requests")
            .and_then(serde_json::Value::as_array)
            .unwrap_or(&empty)
            .iter()
            .map(network_line)
            .collect(),
    }
}

fn console_line(entry: &serde_json::Value) -> String {
    let level = entry.get("level").and_then(serde_json::Value::as_str).unwrap_or("log");
    let text = entry.get("text").and_then(serde_json::Value::as_str).unwrap_or("");
    let source = entry.get("source").and_then(serde_json::Value::as_str).unwrap_or("");
    if source.is_empty() {
        format!("{level:>5}  {text}")
    } else {
        format!("{level:>5}  {text}  ({source})")
    }
}

fn network_line(entry: &serde_json::Value) -> String {
    let method = entry.get("method").and_then(serde_json::Value::as_str).unwrap_or("GET");
    let url = entry.get("url").and_then(serde_json::Value::as_str).unwrap_or("");
    let status = entry.get("status").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let ms = entry.get("ms").and_then(serde_json::Value::as_u64).unwrap_or(0);
    match entry.get("failed").and_then(serde_json::Value::as_str) {
        Some(why) => format!("  ---  {method:<6} {url}  ({why})"),
        // A request reported the moment it starts has no status yet; saying
        // `0` would read as a failure, and it is not one.
        None if status == 0 => format!("  ...  {method:<6} {url}"),
        None => format!("  {status}  {method:<6} {url}  {ms}ms"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    #[test]
    fn only_the_two_logs_are_followable() {
        assert!(wanted(&argv(&["page", "console", "--follow"])).is_some());
        assert!(wanted(&argv(&["page", "network", "--follow"])).is_some());
        assert!(wanted(&argv(&["page", "console"])).is_none());
        assert!(wanted(&argv(&["page", "source", "--follow"])).is_none());
        assert!(wanted(&argv(&["tab", "list", "--follow"])).is_none());
        assert!(wanted(&[]).is_none());
    }

    #[test]
    fn the_flags_the_loop_owns_are_taken_out_of_the_argv() {
        let follow =
            wanted(&argv(&["page", "console", "--follow", "--level", "warn", "--since", "44"]))
                .expect("this follows");
        assert!(!follow.argv.iter().any(|w| w == "--follow"), "{:?}", follow.argv);
        assert!(!follow.argv.iter().any(|w| w == "--since"), "{:?}", follow.argv);
        assert!(!follow.argv.iter().any(|w| w == "44"), "{:?}", follow.argv);
        // Everything the caller asked for that is not ours survives.
        assert!(follow.argv.iter().any(|w| w == "--level"), "{:?}", follow.argv);
        assert!(follow.argv.iter().any(|w| w == "warn"), "{:?}", follow.argv);
        assert!(follow.argv.iter().any(|w| w == "--json"), "{:?}", follow.argv);
        assert!(!follow.limited);
    }

    #[test]
    fn a_caller_who_named_a_limit_keeps_it_and_gets_no_backlog_of_ours() {
        let follow =
            wanted(&argv(&["page", "network", "--follow", "--limit", "5"])).expect("follows");
        assert!(follow.limited);
        assert!(follow.argv.iter().any(|w| w == "--limit"));
        let joined = wanted(&argv(&["page", "network", "--follow", "--limit=5"])).expect("follows");
        assert!(joined.limited);
    }

    #[test]
    fn a_line_reads_as_a_log_line() {
        let entry = serde_json::json!({
            "level": "error", "text": "boom", "source": "app.js:4:2"
        });
        assert_eq!(console_line(&entry), "error  boom  (app.js:4:2)");
        let bare = serde_json::json!({ "level": "log", "text": "hello", "source": "" });
        assert_eq!(console_line(&bare), "  log  hello");
    }

    #[test]
    fn a_request_in_flight_does_not_read_as_a_failure() {
        let flying = serde_json::json!({
            "method": "GET", "url": "https://example.com/", "status": 0, "ms": 0, "failed": null
        });
        assert!(network_line(&flying).contains("..."));
        let done = serde_json::json!({
            "method": "GET", "url": "https://example.com/", "status": 200, "ms": 12, "failed": null
        });
        assert!(network_line(&done).contains("200"));
        let bad = serde_json::json!({
            "method": "GET", "url": "https://example.com/", "status": 0, "ms": 1,
            "failed": "could not resolve"
        });
        assert!(network_line(&bad).contains("could not resolve"));
    }
}
