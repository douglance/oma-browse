//! `oma-browse --mcp`, pointed at the window rather than at nothing.
//!
//! An MCP client starts this binary and talks JSON-RPC over stdin and stdout.
//! Left to itself that server would run against a fresh, windowless process, so
//! every tool call answered *the window is not up yet* -- the same hole the CLI
//! had. The command graph is already an MCP server on each window's socket
//! (incurs mounts one at `/cmd/mcp`), so this pumps frames between the two: one
//! line in, one request over the socket, one line out.
//!
//! That works because the mounted service is stateless -- each JSON-RPC message
//! is answered on its own, so no session has to be kept alive between them.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

use crate::control::{self, Target};

/// The route the window answers MCP on.
const ROUTE: &str = "/cmd/mcp";

/// Relay stdio MCP to a live window. `false` if there is no window to relay to,
/// which leaves the caller free to serve MCP here instead.
pub async fn relay(dir: &std::path::Path, target: Target) -> Result<bool> {
    if control::socket_for(dir, target).is_none() {
        return Ok(false);
    }
    tracing::info!("relaying MCP to the browser");

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match ask(dir, target, line.into_bytes()).await {
            // A notification is answered with no content, and writing a blank
            // line for it would desynchronise a client counting replies.
            Ok(None) => {}
            Ok(Some(reply)) => {
                out.write_all(reply.as_bytes()).await?;
                out.write_all(b"\n").await?;
                out.flush().await?;
            }
            // The window closed under us. Ending the relay is the honest answer:
            // the client sees its server exit and can start another.
            Err(control::Failure::NoWindow) => {
                tracing::info!("the window has gone; ending the relay");
                return Ok(true);
            }
            Err(e) => return Err(anyhow::anyhow!("{e}")),
        }
    }
    Ok(true)
}

/// One JSON-RPC message over, and its answer back.
async fn ask(
    dir: &std::path::Path,
    target: Target,
    frame: Vec<u8>,
) -> Result<Option<String>, control::Failure> {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(ROUTE)
        .header("host", control::HOST)
        .header("content-type", "application/json")
        // Both, because the service insists on both: it may answer either with
        // a JSON body or with a one-event stream, and refuses a client that has
        // not said it can read the second.
        .header("accept", "application/json, text/event-stream")
        .body(frame)
        .map_err(|e| control::Failure::Transport(e.into()))?;

    let stream = control::connect(dir, target).await?;
    let response = control::send(stream, request).await?;
    let status = response.status();
    let body = String::from_utf8_lossy(response.body()).into_owned();
    if !status.is_success() {
        return Err(control::Failure::Protocol(format!("{status}: {}", body.trim())));
    }
    Ok(unwrap_event(&body))
}

/// Pull the JSON back out of whatever shape the answer arrived in.
///
/// A single-event `text/event-stream` is one `data:` line and a blank one; a
/// notification is no body at all. Both are ordinary here, so neither is an
/// error.
fn unwrap_event(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let data: Vec<&str> = trimmed
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim))
        .filter(|line| !line.is_empty())
        .collect();
    if data.is_empty() {
        return Some(trimmed.to_string());
    }
    Some(data.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_stream_gives_up_its_json() {
        let sse = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        assert_eq!(
            unwrap_event(sse).as_deref(),
            Some("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}")
        );
    }

    #[test]
    fn a_plain_json_answer_passes_through() {
        assert_eq!(unwrap_event("{\"jsonrpc\":\"2.0\"}").as_deref(), Some("{\"jsonrpc\":\"2.0\"}"));
    }

    #[test]
    fn an_empty_answer_is_a_notification_and_says_nothing() {
        // A blank line back would look like a reply to a client counting them.
        assert_eq!(unwrap_event(""), None);
        assert_eq!(unwrap_event("\n\n"), None);
    }
}
