//! WebKit's own settings, as configured.
//!
//! Everything here is a `WebKitSettings` property the browser previously left
//! at whatever WebKit chose. The defaults in [`crate::config::Engine`] restate
//! those choices, so a value in the config file is always a change from what
//! the browser was already doing rather than a change from nothing.
//!
//! Applied per webview, at creation, in the same `with_webview` closure shape
//! [`crate::favicon::watch`] uses -- that is the only way to reach the real
//! `webkit2gtk::WebView` behind Tauri's.

use std::sync::Arc;

use anyhow::{Context, Result};
use tauri::webview::Webview;

use crate::state::AppState;

/// Apply the engine settings to one content webview.
#[cfg(target_os = "linux")]
pub fn configure<R: tauri::Runtime>(view: &Webview<R>, state: Arc<AppState>) -> Result<()> {
    view.with_webview(move |platform| {
        use webkit2gtk::{SettingsExt, WebViewExt};

        let webview = platform.inner();
        let engine = &state.config.engine;

        let Some(settings) = WebViewExt::settings(&webview) else {
            tracing::warn!("this webview has no settings object; leaving the engine alone");
            return;
        };

        settings.set_enable_javascript(engine.javascript);
        settings.set_enable_developer_extras(engine.devtools);
        settings.set_enable_webrtc(engine.webrtc);
        settings.set_enable_webgl(engine.webgl);
        settings.set_enable_smooth_scrolling(engine.smooth_scrolling);
        settings.set_default_font_size(engine.font_size);
        // Inverted, because the property is about what a page may *not* do.
        settings.set_media_playback_requires_user_gesture(!engine.autoplay);

        if !engine.user_agent.trim().is_empty() {
            settings.set_user_agent(Some(engine.user_agent.trim()));
        }

        // Cookies belong to the web context, which every tab shares, so this is
        // set once however many tabs ask -- the same shape as the favicon
        // database. Doing it per tab would be harmless but repeated work.
        if state.claim_cookie_policy()
            && let Some(context) = webview.context()
        {
            apply_cookie_policy(&context, &engine.cookies, &state);
        }

        // Per tab and not a `WebKitSettings` property at all: WebKit keeps zoom
        // on the view itself, which is also why `page zoom` is per tab.
        if (engine_zoom(&state) - 1.0).abs() > f64::EPSILON {
            webview.set_zoom_level(engine_zoom(&state));
        }
    })
    .context("could not reach the webview to configure it")
}

#[cfg(not(target_os = "linux"))]
pub fn configure<R: tauri::Runtime>(_view: &Webview<R>, _state: Arc<AppState>) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn engine_zoom(state: &Arc<AppState>) -> f64 {
    state.config.tabs.zoom.clamp(0.1, 10.0)
}

/// Third-party cookies are off by default here, which is not WebKit's default.
///
/// The one place this deviates from "restate what WebKit already does", and
/// deliberately: every mainstream browser now blocks third-party cookies, and a
/// browser whose entire chrome is a command palette is not the place to
/// discover you needed to go and turn tracking off.
#[cfg(target_os = "linux")]
fn apply_cookie_policy(context: &webkit2gtk::WebContext, policy: &str, state: &Arc<AppState>) {
    use webkit2gtk::{CookieAcceptPolicy, CookieManagerExt, WebContextExt};

    let wanted = match policy.trim().to_ascii_lowercase().as_str() {
        "always" => CookieAcceptPolicy::Always,
        "never" => CookieAcceptPolicy::Never,
        "no-third-party" | "no_third_party" => CookieAcceptPolicy::NoThirdParty,
        other => {
            state.note_config_problem(format!(
                "engine.cookies: {other:?} is not \"always\", \"never\" or \"no-third-party\"; \
                 leaving cookies as they were"
            ));
            return;
        }
    };

    let Some(manager) = context.cookie_manager() else {
        tracing::warn!("no cookie manager on this web context");
        return;
    };
    manager.set_accept_policy(wanted);
    tracing::debug!(policy = %policy, "cookie policy set");
}
