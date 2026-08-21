//! The load bar, driven by WebKit's own estimate.
//!
//! A browser that shows nothing while a page is being fetched is indistinguishable
//! from one that has hung, and the window here is the whole interface -- there is
//! no toolbar with a spinner in it and no throbber on a tab. What there is, is a
//! strip; this puts a two-pixel line along the bottom of it.
//!
//! Three decisions make it cheap enough to leave on:
//!
//! * **WebKit's number, not ours.** `estimated-load-progress` already knows how
//!   many subresources are outstanding. Counting them ourselves would mean a
//!   second `resource-load-started` listener arriving at a worse answer.
//! * **A notification, not a poll.** The value arrives on a GObject `notify`,
//!   so an idle browser runs no timer -- the cost of *not* loading is zero.
//! * **An `eval`, not a redraw.** The strip is a server-rendered page that
//!   reloads whenever the tab model changes, and reloading it thirty times a
//!   page load would cost more than the load. Instead each step is one call
//!   into a function the strip already has, which sets one custom property;
//!   the bar itself moves by `transform` alone, on the compositor.
//!
//! The strip is still reloaded out from under that function by anything else
//! that changes the tabs, so [`crate::strip`] renders the current fraction into
//! the markup as well. A reload mid-load comes back up where it left off.

use std::sync::Arc;

use anyhow::{Context, Result};
use tauri::webview::Webview;

use crate::state::AppState;

/// Hook a content webview so its load progress reaches the strip.
///
/// Per webview, like [`crate::favicon::watch`] beside it: the signal is on the
/// `WebKitWebView`, and there is nowhere higher to bind it.
#[cfg(target_os = "linux")]
pub fn watch<R: tauri::Runtime>(view: &Webview<R>, state: Arc<AppState>) -> Result<()> {
    let label = view.label().to_string();

    view.with_webview(move |platform| {
        use webkit2gtk::WebViewExt;

        let webview = platform.inner();

        // Both signals, because neither alone is the whole story.
        // `estimated-load-progress` is the movement, but it is not reset to
        // zero between loads and says nothing about a load that finished with
        // no further progress notification; `is-loading` is the beginning and
        // the end, but never moves in between.
        let (progress_state, progress_label) = (state.clone(), label.clone());
        webview.connect_estimated_load_progress_notify(move |w| {
            publish(&progress_state, &progress_label, w.is_loading(), w.estimated_load_progress());
        });

        let (loading_state, loading_label) = (state.clone(), label.clone());
        webview.connect_is_loading_notify(move |w| {
            publish(&loading_state, &loading_label, w.is_loading(), w.estimated_load_progress());
        });
    })
    .context("could not reach the webview to watch its load progress")
}

#[cfg(not(target_os = "linux"))]
pub fn watch<R: tauri::Runtime>(_view: &Webview<R>, _state: Arc<AppState>) -> Result<()> {
    Ok(())
}

/// Take one reading off the GTK main thread and paint it, if it is worth
/// painting -- see [`crate::tabs::Tabs::set_progress`], which is what decides.
#[cfg(target_os = "linux")]
fn publish(state: &Arc<AppState>, label: &str, loading: bool, fraction: f64) {
    if !state.config.chrome.strip.progress {
        return;
    }

    let state = state.clone();
    let label = label.to_string();
    state.runtime().spawn(async move {
        tracing::info!(loading, fraction, tab = %label, "PROBE");
        if !state.tabs.write().await.set_progress(&label, loading.then_some(fraction)) {
            return;
        }
        paint(&state).await;
    });
}

/// Push the active tab's progress into the strip.
///
/// Reads the model rather than taking the fraction it was just given, so what
/// is drawn is always what [`crate::tabs::Tabs::set_progress`] decided to
/// keep -- which is not the same number WebKit reported at the start of a load.
///
/// Silent on every failure on purpose: there is no strip when it is turned off
/// in the config, and none for the moment between a reload being asked for and
/// the page being up again. A load bar is not worth a log line in either case.
pub async fn paint(state: &Arc<AppState>) {
    use tauri::Manager;

    let Some(app) = state.app_handle() else { return };
    let Some(strip) = app.get_webview(crate::strip::LABEL) else { return };
    let value = state.tabs.read().await.active_progress();
    let _ = strip.eval(call(value));
}

/// The one-line call the strip's script answers.
///
/// Guarded, because `eval` can land in the window between a strip reload
/// starting and its script having run, and an unguarded call would be a
/// `ReferenceError` in the chrome's console rather than a dropped frame.
///
/// Also used by [`crate::strip`] to hand a reloaded strip the load already in
/// flight, so there is one spelling of the call and not two.
pub fn call(value: Option<f64>) -> String {
    match value {
        Some(fraction) => {
            format!("window.__omaLoad&&__omaLoad({:.4},true)", fraction.clamp(0.0, 1.0))
        }
        None => "window.__omaLoad&&__omaLoad(1,false)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::call;

    #[test]
    fn a_call_is_guarded_and_carries_both_facts() {
        // The guard is what keeps a reload race out of the console; see `call`.
        assert_eq!(call(Some(0.5)), "window.__omaLoad&&__omaLoad(0.5000,true)");
        assert!(call(None).ends_with("false)"), "finishing has to say so");
        assert!(call(None).starts_with("window.__omaLoad&&"), "unguarded");
    }

    #[test]
    fn a_fraction_outside_the_range_is_brought_back_into_it() {
        // WebKit is well behaved here, but the value crosses a thread and ends
        // up in a `scaleX`, where a number above one would overhang the window.
        assert_eq!(call(Some(1.4)), "window.__omaLoad&&__omaLoad(1.0000,true)");
        assert_eq!(call(Some(-0.2)), "window.__omaLoad&&__omaLoad(0.0000,true)");
    }
}
