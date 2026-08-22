//! What a tab does when its web process dies.
//!
//! WebKit runs pages in a separate process, and that process can go: it can
//! crash, and the kernel or WebKit's own memory monitor can kill it under
//! pressure. The webview it was drawing for stays alive and keeps its widget,
//! its URL and its place in the strip.
//!
//! What it does *not* keep is the page. Nothing repaints, nothing scrolls, no
//! script runs and every `eval` comes back empty. On Wayland the compositor
//! still holds the last frame it was handed, so the window goes on showing the
//! page exactly as it looked a moment before it died -- which makes a dead tab
//! indistinguishable from a live one until you try to use it. Measured here by
//! killing the web process under a loaded page: the window kept the text on
//! screen, `tab list` kept answering, and `page eval` returned an empty string
//! rather than an error.
//!
//! That is the worst of the options. A blank window at least tells you
//! something is wrong. So this does what every other browser does -- Chrome's
//! "Aw, Snap!", Firefox's tab crash page -- and puts an honest page in the tab
//! saying what happened and how to get the site back.
//!
//! Two details are load-bearing:
//!
//! * **`load_alternate_html`, not a navigation.** It shows our markup *at the
//!   failed page's URL*, so the tab still knows where it was: the strip does not
//!   change, history is not rewritten, and a plain `nav reload` -- Ctrl-R --
//!   asks for the site again rather than for the crash page. Recovering is the
//!   key it would have been anyway.
//! * **Nothing reloads by itself.** A page that crashes reliably would crash
//!   again, and a browser that retries it forever is worse than one that stops
//!   and says so. The one exception is the browser's own chrome, which has no
//!   user to tell; see [`watch_chrome`].

use std::sync::Arc;

use anyhow::{Context, Result};
use tauri::webview::Webview;

use crate::state::AppState;

/// A tab whose web process is gone.
#[derive(Debug, Clone)]
pub struct Crash {
    /// Why it went, in words fit for a page. See [`describe`].
    pub reason: &'static str,
    /// Where the tab was when it died, so the page can say so.
    pub uri: String,
    /// Set while the crash page itself is on its way in.
    ///
    /// Reporting a crash means loading a page into the webview that just
    /// crashed, and that load looks exactly like the recovery that should clear
    /// the crash. This is how the two are told apart: the first load after a
    /// crash is ours and consumes the flag, and the next one is a real page and
    /// clears the whole record. It is set in the same breath as the crash,
    /// synchronously, before the load it is guarding against can be asked for.
    pub reporting: bool,
}

/// Watch a content webview for its web process dying.
///
/// Per webview, like [`crate::progress::watch`] beside it: the signal is on the
/// `WebKitWebView` and there is nowhere higher to bind it.
#[cfg(target_os = "linux")]
pub fn watch<R: tauri::Runtime>(view: &Webview<R>, state: Arc<AppState>) -> Result<()> {
    let label = view.label().to_string();

    view.with_webview(move |platform| {
        use webkit2gtk::WebViewExt as _;

        platform.inner().connect_web_process_terminated(move |view, reason| {
            let uri = view.uri().map(|u| u.to_string()).unwrap_or_default();
            let why = describe(reason);
            tracing::error!(tab = %label, %uri, reason = why, "a tab's web process died");

            // Before the load, not after: the load is what would otherwise
            // clear this, and both halves happen on this thread.
            state.note_crash(&label, why, &uri);
            view.load_alternate_html(&page(&state, &uri, why), &uri, None);
        });
    })
    .context("could not reach the webview to watch for its web process dying")
}

#[cfg(not(target_os = "linux"))]
pub fn watch<R: tauri::Runtime>(_view: &Webview<R>, _state: Arc<AppState>) -> Result<()> {
    Ok(())
}

/// The same, for a webview that *is* the browser: the strip and the palette.
///
/// These get the opposite treatment. There is no page to explain and nobody to
/// explain it to -- the strip is a row of favicons and the palette is not even
/// on screen most of the time -- and what they show is rendered from the
/// browser's own state, so re-rendering it costs one local request and loses
/// nothing. A crash here should look like a flicker, not an incident.
///
/// It is still logged at error level. Chrome dying is not normal, and the log
/// is the only trace it leaves.
#[cfg(target_os = "linux")]
pub fn watch_chrome<R: tauri::Runtime>(view: &Webview<R>, name: &'static str) -> Result<()> {
    view.with_webview(move |platform| {
        use webkit2gtk::WebViewExt as _;

        platform.inner().connect_web_process_terminated(move |view, reason| {
            tracing::error!(chrome = name, reason = describe(reason), "the chrome's process died");
            view.reload();
        });
    })
    .context("could not reach the webview to watch for its web process dying")
}

#[cfg(not(target_os = "linux"))]
pub fn watch_chrome<R: tauri::Runtime>(_view: &Webview<R>, _name: &'static str) -> Result<()> {
    Ok(())
}

/// WebKit's reason, in words that mean something to whoever is reading the page.
///
/// Deliberately plain. "Exceeded memory limit" is what the enum is called; "ran
/// out of memory" is what happened.
#[cfg(target_os = "linux")]
fn describe(reason: webkit2gtk::WebProcessTerminationReason) -> &'static str {
    use webkit2gtk::WebProcessTerminationReason as R;
    match reason {
        R::ExceededMemoryLimit => "it ran out of memory",
        R::TerminatedByApi => "the browser stopped it",
        // `Crashed`, and whatever WebKit adds next. Both are "it died and we do
        // not know why", which is the honest thing to print.
        _ => "it stopped unexpectedly",
    }
}

/// The page a crashed tab shows.
///
/// Built here rather than served from the chrome scheme like the certificate
/// and login interstitials, for two reasons. It has to carry the URL it is
/// standing in for, and a query string on a page a content webview can navigate
/// to is a query string a page could forge -- the same objection [`crate::ui`]
/// records against putting the refused host in one. And it has to arrive
/// through `load_alternate_html`, which takes markup rather than a URL, because
/// that is what keeps the tab's own address intact.
fn page(state: &Arc<AppState>, uri: &str, reason: &'static str) -> String {
    // `try_read`, not `read`: this runs on the GTK main thread inside a signal,
    // where there is no runtime to await on. The lock is only ever taken to
    // swap themes, so failing here means the theme is changing in this exact
    // millisecond; the page then renders on the fallback below, which is
    // readable in any theme rather than pretty in one.
    let (vars, mine) = match state.theme.try_read() {
        Ok(theme) => {
            (theme.css.chrome.clone(), crate::strip::chrome_vars(&state.config, theme.css.opacity))
        }
        Err(_) => (FALLBACK_VARS.to_string(), String::new()),
    };

    format!(
        "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>This page stopped responding</title>\
         <style>{vars}</style><style>{mine}</style><style>{CRASH_CSS}</style>\
         </head><body><main>\
         <p class=\"tag\">crashed</p>\
         <h1>{host}</h1>\
         <p class=\"sub\">stopped responding, because {reason}.</p>\
         <p class=\"hint\">Nothing of the page is left to show. \
         <code>nav reload</code> asks for it again &mdash; or press \
         <kbd>Ctrl</kbd>+<kbd>R</kbd>, which is the same thing. \
         Anything you had typed into it is gone.</p>\
         <p class=\"uri\">{shown}</p>\
         </main></body></html>",
        host = escape(&host_of(uri)),
        shown = escape(uri),
    )
}

/// The host, for the heading -- a whole URL in 2rem type wraps to three lines
/// and says nothing the line underneath does not.
fn host_of(uri: &str) -> String {
    match url::Url::parse(uri) {
        Ok(parsed) => parsed.host_str().unwrap_or_default().to_string(),
        // A tab that crashed before it had a URL at all, or on something that is
        // not one. "This page" is wrong-sounding but true, and better than an
        // empty heading.
        Err(_) => "This page".to_string(),
    }
}

/// Text into markup.
///
/// The URL here came off the page that just died and is echoed into a document,
/// so it is escaped like anything else that is not ours. Quotes included: this
/// is written into a format string by hand rather than by a template engine,
/// and the whole point of doing that carefully is not to have to reason about
/// which contexts the value can reach.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Enough of the theme to read the page by, for the moment the theme is being
/// swapped underneath us. Omarchy's own defaults, so it is not jarring even
/// when it is wrong.
const FALLBACK_VARS: &str = ":root { --oma-veil: #1a1b26; --oma-fg: #c0caf5; \
     --oma-muted: #565f89; --oma-accent: #7aa2f7; --oma-color-red: #f7768e; \
     --oma-space: 8px; --oma-space-2: 16px; --oma-font-small: 0.85rem; \
     --oma-font-mono: monospace; --oma-control-normal-border: #414868; }";

/// Deliberately the same shape as the certificate page's stylesheet: a crash and
/// a refused certificate are the same kind of event to whoever is looking at
/// them, and they should not look like two different browsers.
const CRASH_CSS: &str = r#"
* { box-sizing: border-box; }
html, body { margin: 0; height: 100%; background: var(--oma-veil); color: var(--oma-fg);
  font-family: system-ui, sans-serif; }
main { height: 100%; display: flex; flex-direction: column; align-items: center;
  justify-content: center; gap: var(--oma-space); padding: var(--oma-space-2);
  max-width: 46rem; margin: 0 auto; text-align: center; }
.tag { margin: 0; text-transform: uppercase; letter-spacing: 0.16em;
  font-size: var(--oma-font-small); color: var(--oma-color-red); }
h1 { margin: 0; font-size: 2rem; font-weight: 600; color: var(--oma-fg);
  font-family: var(--oma-font-mono); word-break: break-all; }
.sub { margin: 0; color: var(--oma-fg); }
.hint { margin: var(--oma-space) 0 0; color: var(--oma-fg); line-height: 1.6; }
code, kbd { font-family: var(--oma-font-mono); color: var(--oma-accent);
  border: 1px solid var(--oma-control-normal-border); padding: 0 6px; }
.uri { margin: var(--oma-space-2) 0 0; color: var(--oma-muted);
  font-family: var(--oma-font-mono); font-size: var(--oma-font-small);
  word-break: break-all; opacity: 0.7; }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_becomes_a_host() {
        assert_eq!(host_of("https://youtube.com/watch?v=1"), "youtube.com");
        assert_eq!(host_of("http://127.0.0.1:8731/index.html"), "127.0.0.1");
    }

    #[test]
    fn something_that_is_not_a_url_still_gets_a_heading() {
        assert_eq!(host_of(""), "This page");
        assert_eq!(host_of("not a url"), "This page");
    }

    /// The URL on this page came off a page that just died, and it is written
    /// into markup by hand.
    #[test]
    fn the_url_cannot_carry_markup_onto_the_page() {
        let nasty = "https://x.test/?q=<script>alert('x')</script>&a=\"b\"";
        let escaped = escape(nasty);
        assert!(!escaped.contains('<'), "{escaped}");
        assert!(!escaped.contains('>'), "{escaped}");
        assert!(!escaped.contains('"'), "{escaped}");
        assert!(escaped.contains("&lt;script&gt;"), "{escaped}");
        // And the ampersand that was already there is not left half-escaped.
        assert!(escaped.contains("&amp;a="), "{escaped}");
    }

    #[test]
    fn escaping_leaves_an_ordinary_url_alone() {
        let plain = "https://omarchy.org/docs/getting-started";
        assert_eq!(escape(plain), plain);
    }
}
