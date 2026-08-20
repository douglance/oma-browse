//! Chords, as written in the config file.
//!
//! `"ctrl+shift+t" = "tab_reopen"` has to become the same thing the built-in
//! table holds: modifier flags, a set of GDK keyval names, a tool, and its
//! arguments as JSON. Two halves, both here so that both can be tested without
//! a window: the chord on the left of the `=`, and the command on the right.
//!
//! Key names are GDK's, and deliberately so at the bottom end: anything this
//! does not recognise is passed through untouched, so `"ctrl+ISO_Left_Tab"`
//! works for someone who knows what they want and is not waiting on this list
//! to grow.

use std::collections::BTreeMap;

use serde_json::Value;

/// A chord, parsed out of a config key.
#[derive(Debug, Clone, PartialEq)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    /// `Some(true)` when the chord says `shift+`, `Some(false)` when it does
    /// not. Never `None`: only the built-in table has chords that do not care,
    /// and it says so by hand.
    pub shift: Option<bool>,
    /// Every GDK keyval name this chord should match.
    pub keys: Vec<String>,
}

/// Turn `"ctrl+shift+t"` into modifiers and key names.
pub fn parse_chord(spec: &str) -> Result<Chord, String> {
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    let mut key = None;

    // Split on `+`, but not on a trailing one: `ctrl++` means Ctrl and the plus
    // key, and is a chord someone will absolutely write for zoom.
    let cleaned = spec.trim();
    if cleaned.is_empty() {
        return Err("empty chord".to_string());
    }
    let (body, trailing_plus) = match cleaned.strip_suffix('+') {
        Some(rest) if !rest.is_empty() => (rest, true),
        _ => (cleaned, false),
    };

    for part in body.split('+') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" | "meta" | "mod1" => alt = true,
            "shift" => shift = true,
            _ => {
                if key.replace(part.to_string()).is_some() {
                    return Err(format!("{spec:?} names more than one key"));
                }
            }
        }
    }
    if trailing_plus && key.replace("plus".to_string()).is_some() {
        return Err(format!("{spec:?} names more than one key"));
    }

    let Some(key) = key else { return Err(format!("{spec:?} is modifiers with no key")) };
    Ok(Chord { ctrl, alt, shift: Some(shift), keys: key_names(&key) })
}

/// Every GDK keyval name a written key should match.
///
/// A letter matches both cases because GTK reports the shifted name when Shift
/// is down; a digit and the zoom keys match their keypad twins, because a chord
/// that works on the number row and not the keypad is a bug report.
fn key_names(key: &str) -> Vec<String> {
    let lower = key.to_ascii_lowercase();

    let names: Vec<&str> = match lower.as_str() {
        "plus" | "+" => vec!["plus", "equal", "KP_Add"],
        "minus" | "-" => vec!["minus", "underscore", "KP_Subtract"],
        "space" => vec!["space", "KP_Space"],
        "enter" | "return" => vec!["Return", "KP_Enter"],
        "escape" | "esc" => vec!["Escape"],
        "tab" => vec!["Tab", "ISO_Left_Tab"],
        "backspace" => vec!["BackSpace"],
        "delete" | "del" => vec!["Delete", "KP_Delete"],
        "left" => vec!["Left", "KP_Left"],
        "right" => vec!["Right", "KP_Right"],
        "up" => vec!["Up", "KP_Up"],
        "down" => vec!["Down", "KP_Down"],
        "home" => vec!["Home", "KP_Home"],
        "end" => vec!["End", "KP_End"],
        "pageup" | "page_up" | "prior" => vec!["Page_Up", "Prior", "KP_Page_Up", "KP_Prior"],
        "pagedown" | "page_down" | "next" => vec!["Page_Down", "Next", "KP_Page_Down", "KP_Next"],
        _ => vec![],
    };
    if !names.is_empty() {
        return names.into_iter().map(str::to_string).collect();
    }

    // A single letter: both cases. A single digit: the number row and the pad.
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return vec![c.to_ascii_lowercase().to_string(), c.to_ascii_uppercase().to_string()];
        }
        if c.is_ascii_digit() {
            return vec![c.to_string(), format!("KP_{c}")];
        }
    }

    // Anything else is handed to GDK as written: `F5`, `ISO_Left_Tab`, and
    // whatever else someone found in `gdkkeysyms.h`.
    vec![key.to_string()]
}

/// Split `"ui_palette --action toggle"` into the tool and its arguments.
///
/// The flag spelling is the CLI's, because that is what the command is
/// documented as everywhere else -- `oma-browse ui palette --action toggle` is
/// the same command, and having the config file spell it differently would be a
/// second thing to learn. Values are coerced against the command's own schema
/// through [`crate::dispatch::coerce`], so `--delta -1` is a number and
/// `--background true` is a boolean.
pub fn parse_command(
    spec: &str,
    schema: Option<&serde_json::Map<String, Value>>,
) -> Result<(String, BTreeMap<String, Value>), String> {
    let mut parts = spec.split_whitespace();
    let Some(tool) = parts.next() else { return Err("no command".to_string()) };

    let mut args = BTreeMap::new();
    let rest: Vec<&str> = parts.collect();
    let mut i = 0;
    while i < rest.len() {
        let Some(name) = rest[i].strip_prefix("--") else {
            return Err(format!("{:?} is not a --flag", rest[i]));
        };
        let name = name.replace('-', "_");

        let ty = schema
            .and_then(|props| props.get(&name))
            .and_then(|spec| spec.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("string");

        // A boolean flag may stand alone: `--background` means true.
        if ty == "boolean" && rest.get(i + 1).is_none_or(|next| next.starts_with("--")) {
            args.insert(name, Value::Bool(true));
            i += 1;
            continue;
        }

        let Some(value) = rest.get(i + 1) else {
            return Err(format!("--{name} has no value"));
        };
        args.insert(name, crate::dispatch::coerce(value, ty));
        i += 2;
    }

    Ok((tool.to_string(), args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chord_is_modifiers_and_one_key() {
        let c = parse_chord("ctrl+shift+t").unwrap();
        assert!(c.ctrl && !c.alt);
        assert_eq!(c.shift, Some(true));
        assert_eq!(c.keys, ["t", "T"], "both cases, because GTK reports the shifted name");

        let alt_left = parse_chord("alt+left").unwrap();
        assert!(alt_left.alt && !alt_left.ctrl);
        assert_eq!(alt_left.shift, Some(false));
        assert!(alt_left.keys.contains(&"Left".to_string()));
    }

    #[test]
    fn the_zoom_keys_survive_being_written_as_symbols() {
        // `ctrl++` is the chord everyone writes for zoom in, and splitting on
        // `+` naively turns it into modifiers with no key at all.
        let plus = parse_chord("ctrl++").unwrap();
        assert!(plus.ctrl);
        assert!(plus.keys.contains(&"plus".to_string()));
        assert!(plus.keys.contains(&"KP_Add".to_string()), "and on the keypad");

        assert_eq!(parse_chord("ctrl+plus").unwrap().keys, parse_chord("ctrl++").unwrap().keys);
        assert!(parse_chord("ctrl+-").unwrap().keys.contains(&"minus".to_string()));
    }

    #[test]
    fn an_unknown_key_is_passed_through_to_gdk() {
        // Not every keyval name will ever be in the table above, and someone
        // who has read `gdkkeysyms.h` should not be blocked by that.
        assert_eq!(parse_chord("ctrl+ISO_Left_Tab").unwrap().keys, ["ISO_Left_Tab"]);
        assert_eq!(parse_chord("F5").unwrap().keys, ["F5"]);
    }

    #[test]
    fn nonsense_chords_are_refused_rather_than_guessed_at() {
        assert!(parse_chord("").is_err());
        assert!(parse_chord("ctrl+shift").is_err(), "modifiers with no key");
        assert!(parse_chord("ctrl+a+b").is_err(), "two keys");
    }

    #[test]
    fn a_command_carries_its_flags() {
        let schema = serde_json::json!({
            "action": { "type": "string" },
            "delta": { "type": "integer" },
            "background": { "type": "boolean" },
        });
        let props = schema.as_object().cloned().unwrap();

        let (tool, args) = parse_command("ui_palette --action toggle", Some(&props)).unwrap();
        assert_eq!(tool, "ui_palette");
        assert_eq!(args.get("action"), Some(&Value::String("toggle".into())));

        // Coerced against the schema, so the command receives a number.
        let (_, args) = parse_command("tab_cycle --delta -1", Some(&props)).unwrap();
        assert_eq!(args.get("delta"), Some(&Value::from(-1)));

        // A boolean flag may stand alone.
        let (_, args) = parse_command("tab_open --background", Some(&props)).unwrap();
        assert_eq!(args.get("background"), Some(&Value::Bool(true)));
    }

    #[test]
    fn a_bare_command_takes_no_arguments() {
        let (tool, args) = parse_command("tab_reopen", None).unwrap();
        assert_eq!(tool, "tab_reopen");
        assert!(args.is_empty());
    }

    #[test]
    fn a_stray_word_is_an_error_not_an_argument() {
        // Without this, `"nav_go example.com"` would silently bind a command
        // with no arguments and the URL would go nowhere.
        assert!(parse_command("nav_go example.com", None).is_err());
    }
}
