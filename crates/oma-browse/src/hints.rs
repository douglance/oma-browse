//! Link hints: the page-side script, and the channel it opens tabs through.
//!
//! The script itself is [`hints.js`](./hints.js). This module is the two ends it
//! cannot reach on its own -- injection, and "open this in a new tab".

use std::sync::Arc;

use crate::state::AppState;

/// Prefix of the URL the hint script navigates to when a link should open in a
/// new tab.
///
/// A page cannot call the control plane: it listens on `http://127.0.0.1` and
/// the page is nearly always `https`, so mixed-content blocking kills the
/// request before it is sent. A navigation, though, always reaches the
/// navigation handler -- so the script asks for a URL that can never resolve,
/// and [`intercept`] answers it and cancels the navigation.
///
/// `.invalid` is reserved by RFC 2606 precisely so that it cannot be registered:
/// if this interception ever fails, the failure is a DNS error rather than a
/// request to somebody else's server.
pub const SENTINEL: &str = "http://oma-browse.invalid/newtab?url=";

const SCRIPT: &str = include_str!("hints.js");

/// The script, ready to inject.
pub fn script() -> String {
    SCRIPT.replace("__OMA_HINT_SENTINEL__", SENTINEL)
}

/// Answer a navigation. Returns whether the webview should go ahead with it.
///
/// Everything that is not the sentinel proceeds untouched, so this is inert for
/// every real navigation the browser does.
pub fn intercept(state: &Arc<AppState>, url: &url::Url) -> bool {
    let raw = url.as_str();
    let Some(encoded) = raw.strip_prefix(SENTINEL) else {
        return true;
    };

    match percent_decode(encoded) {
        Some(target) => {
            let state = state.clone();
            state.runtime().spawn(async move {
                // Background, because the whole point of Shift-F is that the
                // page you are reading stays where it is.
                if let Err(e) = crate::tabs::open(&state, &target, true).await {
                    crate::dispatch::toast(&format!("could not open {target}: {e:#}"));
                }
            });
        }
        None => tracing::warn!(%raw, "a hint asked for a target that would not decode"),
    }
    false
}

/// Percent-decoding, for the one caller that needs it.
///
/// `url` can parse a URL but will not hand back a decoded query value without a
/// form-encoding pass that would also turn `+` into a space -- and a `+` in a
/// path is a `+`. This is the exact inverse of `encodeURIComponent`.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tap rather than an assertion, matching `oma-theme`'s `dump_script`:
    /// `OMA_DUMP_HINTS=<path> cargo test -p oma-browse hints::tests::dump`
    /// writes the link-hint runtime out so a real JavaScript parser can see it.
    /// A syntax error in here is invisible from Rust -- the script is a `&str`
    /// to the compiler, so it builds, ships, and simply never runs, and the
    /// only symptom is that `f` stops drawing hints.
    #[test]
    fn dump() {
        let Ok(path) = std::env::var("OMA_DUMP_HINTS") else { return };
        std::fs::write(path, script()).expect("write");
    }

    #[test]
    fn the_script_carries_the_sentinel() {
        let js = script();
        assert!(js.contains(SENTINEL), "the script must know where to send a new-tab request");
        assert!(!js.contains("__OMA_HINT_SENTINEL__"), "the placeholder must be substituted");
    }

    #[test]
    fn decoding_is_the_inverse_of_encodeuricomponent() {
        // What `encodeURIComponent("https://a.example/x y?q=1&r=2#f")` produces.
        let encoded = "https%3A%2F%2Fa.example%2Fx%20y%3Fq%3D1%26r%3D2%23f";
        assert_eq!(percent_decode(encoded).as_deref(), Some("https://a.example/x y?q=1&r=2#f"));
    }

    #[test]
    fn a_plus_survives_decoding() {
        // Form decoding would turn this into a space and break the URL.
        assert_eq!(percent_decode("a%2Bb").as_deref(), Some("a+b"));
    }

    #[test]
    fn truncated_escapes_do_not_panic() {
        assert_eq!(percent_decode("%"), None);
        assert_eq!(percent_decode("%2"), None);
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(percent_decode(""), None);
    }
}
