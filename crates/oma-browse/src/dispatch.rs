//! One way to run a command, shared by every surface that is not the CLI.
//!
//! The command graph in [`crate::commands`] is the only place a browser action
//! is defined, but for a long time it was not the only place one was *invoked*:
//! the palette had its own procedures calling `crate::tabs::*` directly, and the
//! keyboard had an `Action` enum doing the same. Both drifted -- adding
//! `page zoom` meant editing two files, and the palette still could not run it.
//!
//! Everything now goes through [`run`], which hands the call to incurs'
//! [`ToolCatalog`] -- the same object the MCP layer enumerates tools from, so a
//! command reachable by an agent is reachable by a keystroke, with the same
//! argument validation and the same middleware.

use std::collections::BTreeMap;

use incurs::tool::{ToolCallOptions, ToolCallOutcome, ToolCatalog, ToolDefinition};
use serde_json::Value;

/// Run a command. `None` on success; on failure, a message fit to show a human.
///
/// Options are always [`ToolCallOptions::isolated`]: a keystroke in a GUI must
/// not pick up ambient `$ENV` or whatever config file happens to be discoverable
/// from the working directory. That resolution belongs to the CLI face of this
/// binary, not to Ctrl-W.
pub async fn run(
    catalog: &ToolCatalog,
    tool: &str,
    args: BTreeMap<String, Value>,
) -> Option<String> {
    match catalog.call(tool, args, ToolCallOptions::isolated()).await {
        ToolCallOutcome::Ok { .. } => {
            tracing::debug!(tool, "command ok");
            None
        }
        ToolCallOutcome::Error { code, message, field_errors, .. } => {
            tracing::warn!(tool, %code, %message, "command failed");
            let fields = field_errors.unwrap_or_default();
            if fields.is_empty() {
                Some(message)
            } else {
                let names: Vec<&str> = fields.iter().map(|f| f.path.as_str()).collect();
                Some(format!("{message} ({})", names.join(", ")))
            }
        }
    }
}

/// Show a failure to the user.
///
/// Omarchy's own notification daemon rather than an in-app surface, for one
/// reason: most commands are reachable from the keyboard, and a keyboard failure
/// happens with no palette on screen and nowhere in-app to put the message. A
/// desktop notification is the only surface that exists in both cases, and it is
/// already themed to match.
///
/// Fire-and-forget: a browser must not stall because a notification daemon is
/// slow, and it must not fail because one is missing.
pub fn toast(message: &str) {
    let message = message.to_string();
    tokio::spawn(async move {
        let sent = tokio::process::Command::new("omarchy")
            .args(["notification", "send", "-u", "normal", "oma-browse", &message])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        if let Err(e) = sent {
            tracing::debug!(error = %e, "could not raise a notification");
        }
    });
}

/// Parse a binding's literal argument JSON. A malformed literal is a programming
/// error caught by [`crate::layout`]'s startup check and its unit test, so this
/// degrades to "no arguments" rather than refusing to act.
pub fn args_from_json(raw: &str) -> BTreeMap<String, Value> {
    match serde_json::from_str::<BTreeMap<String, Value>>(raw) {
        Ok(map) => map,
        Err(e) => {
            tracing::error!(raw, error = %e, "binding arguments are not a JSON object");
            BTreeMap::new()
        }
    }
}

/// Turn typed-in text into a JSON value of the shape the schema asks for.
///
/// Integers before floats, and it matters: `tab select`/`tab close` take `u32`
/// and `tab cycle` takes `i32`, but serde will not accept `3.0` for any of them
/// -- it rejects a float for an integer field outright. Parsing `"3"` as `f64`
/// first would make every id and delta fail validation.
pub fn coerce(raw: &str, json_type: &str) -> Value {
    let text = raw.trim();
    match json_type {
        "integer" | "number" => text
            .parse::<i64>()
            .map(Value::from)
            .or_else(|_| text.parse::<f64>().map(Value::from))
            .unwrap_or_else(|_| Value::String(text.to_string())),
        "boolean" => match text {
            "" | "true" | "yes" | "on" | "1" => Value::Bool(true),
            "false" | "no" | "off" | "0" => Value::Bool(false),
            other => Value::String(other.to_string()),
        },
        _ => Value::String(text.to_string()),
    }
}

/// Split a tool name into the group it came from and a label for the command.
///
/// Tool names are the command path joined with `_` (`page_zoom`), and the
/// catalog flattens groups away, so the prefix is the only thing left of the
/// structure `commands.rs` declared. Sentence case, not title case, to match the
/// rows the palette already had.
pub fn label(tool: &str) -> (String, String) {
    let (group, leaf) = tool.split_once('_').unwrap_or(("", tool));
    let mut label = leaf.replace('_', " ");
    if let Some(first) = label.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    (group.to_string(), label)
}

/// The property a row prompts for first.
///
/// Deliberately "first non-boolean" rather than "first": property order in the
/// merged schema is insertion order only because `serde_json` happens to be
/// built with `preserve_order` (incurs' default `toon` feature pulls it in).
/// Skipping booleans picks the right field under either ordering for all of
/// today's commands, since every boolean in the graph is a flag with a default.
pub fn primary_arg(def: &ToolDefinition) -> Option<(String, String, Option<String>)> {
    let props = def.input_schema.get("properties")?.as_object()?;
    props.iter().find_map(|(name, spec)| {
        let ty = spec.get("type").and_then(Value::as_str).unwrap_or("string");
        if ty == "boolean" {
            return None;
        }
        let description =
            spec.get("description").and_then(Value::as_str).map(str::to_string);
        Some((name.clone(), ty.to_string(), description))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_split_group_from_command() {
        assert_eq!(label("page_zoom"), ("page".into(), "Zoom".into()));
        assert_eq!(label("nav_go"), ("nav".into(), "Go".into()));
        assert_eq!(label("solo"), (String::new(), "Solo".into()));
    }

    #[test]
    fn integers_do_not_become_floats() {
        // serde rejects 3.0 for a u32 field, so this is the whole point.
        assert_eq!(coerce("3", "number"), Value::from(3i64));
        assert!(coerce("3", "number").is_i64());
        assert_eq!(coerce("-1", "integer"), Value::from(-1i64));
        assert_eq!(coerce("1.5", "number"), Value::from(1.5f64));
    }

    #[test]
    fn bare_boolean_flags_mean_true() {
        assert_eq!(coerce("", "boolean"), Value::Bool(true));
        assert_eq!(coerce("off", "boolean"), Value::Bool(false));
    }

    #[test]
    fn strings_survive_verbatim() {
        assert_eq!(coerce(" https://a/b ", "string"), Value::String("https://a/b".into()));
    }

    #[test]
    fn bad_binding_json_is_empty_not_fatal() {
        assert!(args_from_json("{").is_empty());
        assert_eq!(args_from_json(r#"{"delta":-1}"#).get("delta"), Some(&Value::from(-1i64)));
    }
}
