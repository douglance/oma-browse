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
/// and login interstitials, and for a reason worth keeping: it has to arrive
/// through `load_alternate_html`, which takes markup rather than a URL, because
/// that is what keeps the tab's own address intact. Putting the address in a
/// query string instead would put it somewhere a page could forge -- the same
/// objection [`crate::ui`] records against doing that with a refused host.
#[cfg(target_os = "linux")]
fn page(state: &Arc<AppState>, uri: &str, reason: &'static str) -> String {
    crate::interstitial::Interstitial {
        tag: "crashed",
        title: "This page stopped responding",
        sub: &format!("stopped responding, because {reason}."),
        detail: None,
        hint: "Nothing of the page is left to show. <code>nav reload</code> asks for it \
               again &mdash; or press <kbd>Ctrl</kbd>+<kbd>R</kbd>, which is the same \
               thing. Anything you had typed into it is gone.",
        uri,
    }
    .render(state)
}
