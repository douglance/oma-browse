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

        // Cookies, the proxy and the spell checker belong to the web context,
        // which every tab shares, so these are set once however many tabs ask --
        // the same shape as the favicon database. Doing them per tab would be
        // harmless but repeated work.
        if state.claim_shared_context()
            && let Some(context) = webview.context()
        {
            apply_cookie_policy(&context, &engine.cookies, &state);
            apply_proxy(&context, engine, &state);
            apply_spellcheck(&context, engine);
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

/// Point the engine at a proxy.
///
/// The mode matters as much as the URI: `Default` follows the desktop's own
/// settings, which is what a browser does when nobody has said otherwise, and
/// only `Custom` reads the settings object -- which is why it is the only
/// branch that builds one.
#[cfg(target_os = "linux")]
fn apply_proxy(
    context: &webkit2gtk::WebContext,
    engine: &crate::config::Engine,
    state: &Arc<AppState>,
) {
    use webkit2gtk::{
        NetworkProxyMode, NetworkProxySettings, WebContextExt, WebsiteDataManagerExt,
    };

    let asked = engine.proxy.trim();
    if asked.is_empty() {
        return;
    }
    // A proxy URI with no scheme is the commonest way to get this wrong, and
    // GLib swallows it rather than complaining -- so complain here instead.
    if !asked.contains("://") {
        state.note_config_problem(format!(
            "engine.proxy: {asked:?} has no scheme; write it as http://host:port"
        ));
        return;
    }
    let Some(manager) = WebContextExt::website_data_manager(context) else {
        state.note_config_problem(
            "engine.proxy: this web context has no data manager, so the proxy was not set"
                .to_string(),
        );
        return;
    };

    let mut settings = NetworkProxySettings::new(Some(asked), &[]);
    manager.set_network_proxy_settings(NetworkProxyMode::Custom, Some(&mut settings));
    tracing::info!(proxy = %asked, "proxy set");
}

/// Spell checking, and the languages to do it in.
///
/// The languages have to be set before the switch is thrown: WebKit reads them
/// when checking is enabled, and an empty list means "$LANG", which is what it
/// would have done anyway.
#[cfg(target_os = "linux")]
fn apply_spellcheck(context: &webkit2gtk::WebContext, engine: &crate::config::Engine) {
    use webkit2gtk::WebContextExt;

    let languages: Vec<&str> =
        engine.spellcheck_languages.iter().map(String::as_str).filter(|l| !l.is_empty()).collect();
    if !languages.is_empty() {
        context.set_spell_checking_languages(&languages);
    }
    context.set_spell_checking_enabled(engine.spellcheck);
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
