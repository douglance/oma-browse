//! What the browser does when a page asks for something.
//!
//! WebKit answers a page's requests -- to open a window, to use the camera, to
//! be trusted despite a bad certificate -- by emitting a signal and taking the
//! first handler's word for it. With no handler connected the built-in default
//! stands, and every one of those defaults is "no, silently": `window.open`
//! returns null, `getUserMedia` rejects, a self-signed host is a dead end. None
//! of that is a policy anybody chose. This module is where the choosing goes.
//!
//! Connected per content webview, beside [`crate::favicon::watch`] and
//! [`crate::engine::configure`], through the same `with_webview` closure -- the
//! only way to reach the real `webkit2gtk::WebView` behind Tauri's.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use tauri::webview::Webview;

use crate::state::AppState;

/// A site that wants a username and a password before it will answer.
#[derive(Debug, Clone)]
pub struct Challenge {
    pub host: String,
    pub port: u32,
    /// What the server calls the thing it is guarding. Often empty.
    pub realm: String,
    /// The page that was being loaded, so answering can go back to it.
    pub uri: String,
    /// Set once a login has already been tried and refused, so the page can say
    /// "that was wrong" rather than asking again as if nothing happened.
    pub retry: bool,
}

impl Challenge {
    /// How a credential is filed: one login per host and port.
    pub fn key(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// A page that never arrived: a host that does not resolve, a connection
/// refused, a protocol the browser does not speak.
///
/// WebKit does answer this one -- it is the only failure that does not leave a
/// blank window -- but what it answers with is a single line of unstyled text in
/// the top-left corner of the viewport. No heading, nothing to do about it, and
/// nothing of the theme: in a browser whose whole argument is that everything is
/// themed, it reads as the browser having given up. Measured against a host that
/// does not exist: one line, "Error resolving ...: Name or service not known",
/// and no way forward but the palette.
///
/// So it gets the same page the crash does, for the same reason and by the same
/// route -- `load_alternate_html`, so the address stays the address that failed
/// and `nav reload` retries the site rather than the error.
#[cfg(target_os = "linux")]
fn failures(webview: &webkit2gtk::WebView, state: Arc<AppState>) {
    use webkit2gtk::WebViewExt as _;

    webview.connect_load_failed(move |view, _event, uri, error| {
        let Some(sub) = worth_a_page(error) else {
            // Not a failure anybody needs a page about; see `worth_a_page`.
            // `false` leaves WebKit to do whatever it would have done, which for
            // these is nothing.
            tracing::debug!(%uri, error = %error, "a load ended without arriving");
            return false;
        };
        tracing::warn!(%uri, error = %error, "a page could not be reached");

        let page = crate::interstitial::Interstitial {
            tag: "not reached",
            title: "This site could not be reached",
            sub,
            // WebKit's own sentence. It names the actual failure -- which of
            // resolving, connecting or speaking the protocol went wrong -- and
            // nothing else on the page does.
            detail: Some(error.message()),
            hint: "Check the address, and whether you are online. \
                   <code>nav reload</code> tries again &mdash; or press \
                   <kbd>Ctrl</kbd>+<kbd>R</kbd>, which is the same thing.",
            uri,
        }
        .render(&state);

        view.load_alternate_html(&page, uri, None);
        true
    });
}

/// Whether a failed load is worth putting a page in front of, and what to say.
///
/// The answer defaults to *yes*, and that direction was measured rather than
/// chosen. The first version of this recognised WebKit's own error domains and
/// ignored everything else, which sounded careful and meant the commonest
/// failure of all showed nothing: a host that does not resolve arrives as
/// `g-resolver-error-quark`, not as a `WebKitNetworkError`, and a connection
/// refused comes up from GIO the same way. Recognising failures one domain at a
/// time is a list that is always missing the one in front of you.
///
/// So this names only what must *not* get a page. Two of these are not failures
/// at all, and either one wrongly turned into a page would be worse than the
/// bare line this replaces:
///
/// * **Cancelled** is what every superseded load reports. Clicking a second link
///   before the first arrives cancels the first, and so does `nav stop`. A page
///   here would replace the page you asked for with an error about the page you
///   changed your mind about.
/// * **Interrupted by a policy change**, and its other half **cannot show this
///   MIME type**, are what a *download* reports. WebKit decides it cannot
///   display a response, hands it to the download machinery, and tells the view
///   its load failed. Every download would otherwise end with the tab claiming
///   the site could not be reached.
#[cfg(target_os = "linux")]
fn worth_a_page(error: &gtk::glib::Error) -> Option<&'static str> {
    use webkit2gtk::{NetworkError, PolicyError};

    if let Some(policy) = error.kind::<PolicyError>() {
        return match policy {
            PolicyError::FrameLoadInterruptedByPolicyChange => None,
            PolicyError::CannotShowMimeType => None,
            PolicyError::CannotUseRestrictedPort => {
                Some("is on a port a browser is not allowed to use.")
            }
            _ => Some("could not be opened."),
        };
    }

    if let Some(network) = error.kind::<NetworkError>() {
        return match network {
            NetworkError::Cancelled => None,
            NetworkError::UnknownProtocol => Some("is not an address this browser can open."),
            NetworkError::FileDoesNotExist => Some("is not there."),
            _ => Some("could not be reached."),
        };
    }

    // A load abandoned on the way out -- the tab was closed, or the browser is
    // shutting down. Nobody is left to read a page about it.
    if error.kind::<gtk::gio::IOErrorEnum>() == Some(gtk::gio::IOErrorEnum::Cancelled) {
        return None;
    }

    // Everything else, which in practice is where the real failures live: GIO
    // and the resolver. The engine's own message is what says which.
    Some("could not be reached.")
}

/// A page that would not load because its certificate did not check out.
///
/// Kept so the interstitial can say which host and what was wrong with it, and
/// so `nav trust` knows what it is being asked to trust without the person
/// having to retype the hostname they just failed to reach.
#[derive(Debug, Clone)]
pub struct Refused {
    pub host: String,
    pub uri: String,
    /// What GLib objected to, in words.
    pub reasons: Vec<String>,
}

/// Connect this webview's policy handlers.
#[cfg(target_os = "linux")]
pub fn install<R: tauri::Runtime>(view: &Webview<R>, state: Arc<AppState>) -> Result<()> {
    view.with_webview(move |platform| {
        let webview = platform.inner();
        popups(&webview, state.clone());
        permissions(&webview, state.clone());
        certificates(&webview, state.clone());
        failures(&webview, state.clone());
        logins(&webview, state.clone());
    })
    .context("could not reach the webview to set its policy")
}

#[cfg(not(target_os = "linux"))]
pub fn install<R: tauri::Runtime>(_view: &Webview<R>, _state: Arc<AppState>) -> Result<()> {
    Ok(())
}

/// `target="_blank"`, `window.open`, and a middle-click.
///
/// WebKit asks for a *widget* to put the new page in, and what it is given
/// decides what the page gets back. Answering `None` means "no window was
/// made", so `window.open` evaluates to `null`.
///
/// A link and a script want different things here, and the first version of
/// this gave both of them a tab and answered `None`. That fixed `_blank` links,
/// which had previously done nothing at all, and left every OAuth pop-up
/// broken: "log in with Google" on x.com opened the chooser as two detached
/// tabs, `window.open` returned null, the new page had no `opener`, and the
/// provider had nowhere to hand the credential back to. The login could not
/// complete and said nothing about why.
///
/// So the two cases are separated by [`NavigationType`]. A clicked link becomes
/// a tab, which is what a person means by `target="_blank"`. A scripted
/// `window.open` gets a real related view in a window of its own -- see
/// [`popup_window`] -- because a login flow needs the two halves to be able to
/// see each other.
#[cfg(target_os = "linux")]
fn popups(webview: &webkit2gtk::WebView, state: Arc<AppState>) {
    use webkit2gtk::{NavigationType, URIRequestExt as _, WebViewExt as _};

    webview.connect_create(move |view, action| {
        let url = action.request().and_then(|request| request.uri()).map(|u| u.to_string());
        let Some(url) = url.filter(|u| !u.is_empty() && u != "about:blank") else {
            // `window.open('')` with nothing to load. There is no page to put
            // in a tab, and an empty tab in front of what you were reading is
            // worse than the null the page already handles.
            tracing::debug!("a page asked for a window with no URL");
            return None;
        };

        // A link and a script asking for a window want different things, and
        // giving both of them the same thing is what broke logging in.
        //
        // A `target="_blank"` link wants a tab: that is what a person means by
        // it, and a tab is what every other browser gives them.
        //
        // `window.open()` from script is nearly always an OAuth pop-up -- "log
        // in with Google", "log in with X" -- and those need a *window object*
        // back. Handing the page a tab and returning `None` here means
        // `window.open` evaluates to `null`, the new page has no `opener`, and
        // the provider has nowhere to hand the credential to. Measured on
        // x.com: the Google chooser opened as two detached tabs and the login
        // could never complete, because the two halves of the flow could not
        // see each other.
        if action.navigation_type() != NavigationType::LinkClicked {
            return popup_window(view, &url);
        }

        // A window the reader asked for comes to the front; one they did not
        // goes behind what they are reading.
        //
        // In practice this is nearly always a gesture, because WebKit blocks an
        // ungestured `window.open` itself and never emits this signal for it --
        // measured: a page calling `window.open` from a `<script>` on load
        // produces no `create` at all. So this is the cheap insurance for the
        // cases that do get through, not the pop-up blocker; that is upstream
        // and it is WebKit's.
        let background = !action.is_user_gesture();
        tracing::debug!(%url, background, "a page asked for a new tab");

        let state = state.clone();
        state.runtime().spawn(async move {
            if let Err(e) = crate::tabs::open(&state, &url, background).await {
                crate::dispatch::toast(&format!("could not open {url}: {e:#}"));
            }
        });
        None
    });
}

/// A real pop-up: a second WebKit view that shares the opener's session, in a
/// window of its own.
///
/// It has to be *related* to the view that asked. `webkit_web_view_new_with_related_view`
/// is what makes the two halves of an OAuth flow the same browsing context --
/// same session, same cookies, and a live `window.opener` for the provider to
/// post the credential back through. A fresh view, or a tab, is a stranger to
/// the page that opened it however identical its configuration.
///
/// Deliberately not one of our tabs, and deliberately without this browser's
/// injections. A login pop-up exists for ten seconds and closes itself; giving
/// it a tab strip entry to be cleaned up afterwards, and a theme, would be
/// work in service of something nobody looks at. `window.close()` from the
/// page takes the window with it, which is what the opener is polling for.
#[cfg(target_os = "linux")]
fn popup_window(opener: &webkit2gtk::WebView, url: &str) -> Option<gtk::Widget> {
    use gtk::prelude::*;
    use webkit2gtk::WebViewExt as _;

    // SAFETY: `opener` is a live view on this thread, and the constructor
    // returns a new floating reference that `from_glib_none` takes a strong one
    // to. The safe crate binds every other `WebView` constructor and not this
    // one, which is the only reason this is here.
    #[allow(unsafe_code, reason = "webkit2gtk 2.0 does not bind the related-view constructor")]
    let popup: webkit2gtk::WebView = unsafe {
        use gtk::glib::translate::{FromGlibPtrNone as _, ToGlibPtr as _};
        let raw = webkit2gtk_sys::webkit_web_view_new_with_related_view(opener.to_glib_none().0);
        if raw.is_null() {
            return None;
        }
        webkit2gtk::WebView::from_glib_none(raw as *mut webkit2gtk_sys::WebKitWebView)
    };

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title(url);
    // What a login pop-up is shaped like everywhere else. WebKit will resize it
    // from the page's own window features when it has them.
    window.set_default_size(520, 640);
    window.add(&popup);

    // `window.close()` has to actually close it: an OAuth opener sits polling
    // `popup.closed`, and a pop-up that cannot close is a login that hangs on
    // the last step rather than failing outright.
    let closing = window.clone();
    popup.connect_close(move |_| closing.close());

    // Shown when WebKit says it is ready rather than immediately, so the window
    // does not appear empty while the provider redirects.
    let showing = window.clone();
    popup.connect_ready_to_show(move |_| showing.show_all());

    window.show_all();
    tracing::info!(%url, "opened a pop-up window for a scripted window.open");
    Some(popup.upcast::<gtk::Widget>())
}

#[cfg(target_os = "linux")]
thread_local! {
    /// Requests that have been put to a person and not yet answered.
    ///
    /// A `webkit2gtk::PermissionRequest` is a GObject: it belongs to the GTK
    /// main thread and cannot be sent anywhere else, so it cannot live on
    /// `AppState` with everything else a command touches. It stays here, on the
    /// thread that owns it, and [`crate::state::AppState::asked`] holds the
    /// Send-able half -- the origin and the kinds -- so a command can word the
    /// question. The two are joined by `id`.
    static WAITING: RefCell<Vec<(u64, webkit2gtk::PermissionRequest)>> =
        const { RefCell::new(Vec::new()) };
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Answer a request someone has decided about. Main thread only.
///
/// `false` if there is nothing with that id, which is ordinary rather than
/// exceptional: the tab may have been closed or navigated away while the
/// palette was open, and WebKit drops the request when that happens.
#[cfg(target_os = "linux")]
pub fn answer(id: u64, allow: bool) -> bool {
    use webkit2gtk::PermissionRequestExt as _;

    let found = WAITING.with(|waiting| {
        let mut waiting = waiting.borrow_mut();
        waiting.iter().position(|(waiting_id, _)| *waiting_id == id).map(|at| waiting.remove(at))
    });
    match found {
        Some((_, request)) => {
            if allow {
                request.allow();
            } else {
                request.deny();
            }
            tracing::debug!(id, allow, "answered a permission request");
            true
        }
        None => {
            tracing::debug!(id, "nothing was waiting under that id");
            false
        }
    }
}

/// Camera, microphone, screen, location, notifications.
///
/// With no handler here WebKit denies everything and tells nobody, which is why
/// a video call in this browser fails with no error and no prompt. This answers
/// from the stored decisions, and asks when there is nothing stored.
#[cfg(target_os = "linux")]
fn permissions(webview: &webkit2gtk::WebView, state: Arc<AppState>) {
    use webkit2gtk::{PermissionRequestExt as _, WebViewExt as _};

    webview.connect_permission_request(move |view, request| {
        let Some(kinds) = kinds_of(request) else {
            // A request type this browser has no opinion about. Returning false
            // leaves WebKit's own default in place rather than inventing one.
            tracing::debug!("an unrecognised permission request; leaving it to WebKit");
            return false;
        };

        // The page's own URL, because a permission request carries no origin of
        // its own in this API. That is the top document asking, which is the
        // thing a person would name if you asked them who wanted the camera.
        let origin = view.uri().and_then(|uri| crate::permissions::origin_of(&uri));
        let Some(origin) = origin else {
            tracing::warn!("a page with no origin asked for a permission; denying");
            request.deny();
            return true;
        };

        let decision = state
            .permissions
            .lock()
            .map(|store| store.decide_all(&origin, &kinds))
            .unwrap_or(crate::permissions::Decision::Deny);

        match decision {
            crate::permissions::Decision::Allow => {
                tracing::debug!(%origin, ?kinds, "allowed, as decided earlier");
                request.allow();
            }
            crate::permissions::Decision::Deny => {
                tracing::debug!(%origin, ?kinds, "denied, as decided earlier");
                request.deny();
            }
            crate::permissions::Decision::Ask => ask(request, &state, origin, kinds),
        }
        true
    });
}

/// Hold on to the request and put the question on screen.
#[cfg(target_os = "linux")]
fn ask(
    request: &webkit2gtk::PermissionRequest,
    state: &Arc<AppState>,
    origin: String,
    kinds: Vec<crate::permissions::Kind>,
) {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let pending = crate::permissions::Pending { id, origin: origin.clone(), kinds };
    let question = pending.question();
    WAITING.with(|waiting| waiting.borrow_mut().push((id, request.clone())));
    if let Ok(mut queue) = state.asked.lock() {
        queue.push_back(pending);
    }
    tracing::info!(id, %origin, "asking about a permission");

    // The palette is the only place this browser has to ask a question, and
    // staging is how a command asks for one word. Set directly rather than
    // through `ui_palette`, because the catalog is not reachable from a GTK
    // signal handler -- these are the same two calls that command makes.
    state.set_stage(Some(crate::ui::stage_with("permission_decide", &question)));
    match crate::window::set_palette_visible(state, true) {
        Ok(()) => state.set_palette_visible(true),
        Err(e) => {
            // No way to ask means no way to consent.
            tracing::warn!(error = %e, "could not open the palette to ask; denying");
            answer(id, false);
            forget(state, id);
        }
    }
}

/// Drop a pending question from the Send-able half of the queue.
#[cfg(target_os = "linux")]
fn forget(state: &Arc<AppState>, id: u64) {
    if let Ok(mut queue) = state.asked.lock() {
        queue.retain(|pending| pending.id != id);
    }
}

/// What a request is asking for, or `None` if this browser has no opinion.
#[cfg(target_os = "linux")]
fn kinds_of(request: &webkit2gtk::PermissionRequest) -> Option<Vec<crate::permissions::Kind>> {
    use crate::permissions::Kind;
    use gtk::glib::prelude::*;
    use webkit2gtk::UserMediaPermissionRequestExt as _;

    if let Some(media) = request.dynamic_cast_ref::<webkit2gtk::UserMediaPermissionRequest>() {
        let mut kinds = Vec::new();
        // Sharing a screen is not using a camera, and WebKit reports it as a
        // *video* device as well -- so it is checked first and takes the place
        // of the camera rather than adding to it.
        if is_display_capture(media) {
            kinds.push(Kind::ScreenShare);
        } else if media.is_for_video_device() {
            kinds.push(Kind::Camera);
        }
        if media.is_for_audio_device() {
            kinds.push(Kind::Microphone);
        }
        // A media request for neither is not a question worth putting to
        // somebody; `decide_all` denies an empty list.
        return Some(kinds);
    }
    if request.is::<webkit2gtk::GeolocationPermissionRequest>() {
        return Some(vec![Kind::Geolocation]);
    }
    if request.is::<webkit2gtk::NotificationPermissionRequest>() {
        return Some(vec![Kind::Notifications]);
    }
    if request.is::<webkit2gtk::DeviceInfoPermissionRequest>() {
        return Some(vec![Kind::DeviceInfo]);
    }
    if request.is::<webkit2gtk::MediaKeySystemPermissionRequest>() {
        return Some(vec![Kind::ProtectedMedia]);
    }
    None
}

/// Whether a media request is for the screen rather than a camera.
///
/// The property arrived in WebKit 2.34 and this crate does not bind it, so it
/// is read by name -- guarded, because asking a GObject for a property it does
/// not have is a panic, and this browser must not take the window down over a
/// missing accessor on an older WebKit.
#[cfg(target_os = "linux")]
fn is_display_capture(media: &webkit2gtk::UserMediaPermissionRequest) -> bool {
    use gtk::glib::prelude::*;

    const PROPERTY: &str = "is-for-display-device";
    if media.property_type(PROPERTY) != Some(bool::static_type()) {
        return false;
    }
    media.property::<bool>(PROPERTY)
}

/// Answer a request from wherever a command happens to be running.
///
/// The request object lives on the GTK main thread and cannot be sent
/// anywhere, so the *decision* is sent instead: one bool, hopped onto that
/// thread through Tauri. Everything above this line can stay ordinary async
/// code that knows nothing about GTK.
#[cfg(target_os = "linux")]
pub fn settle(state: &Arc<AppState>, id: u64, allow: bool) -> Result<()> {
    let app = state.app_handle().context("the window is not up yet")?;
    app.run_on_main_thread(move || {
        answer(id, allow);
    })
    .context("could not reach the main thread to answer")
}

#[cfg(not(target_os = "linux"))]
pub fn settle(_state: &Arc<AppState>, _id: u64, _allow: bool) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
thread_local! {
    /// Certificates offered by hosts this browser refused to reach, by host,
    /// each with the web context that has to be told to accept it.
    ///
    /// Same reason as `WAITING`: these are GObjects and stay on the thread that
    /// owns them. One per host, newest wins -- an older certificate for the
    /// same name is exactly what has been superseded.
    ///
    /// The context is kept rather than looked up later because
    /// `WebContext::default()` is *not* the context these webviews use: wry
    /// builds its own, and trusting a certificate in the default one is a call
    /// that succeeds and changes nothing -- which is precisely the bug this
    /// carried until the reload failed a second time with the same error.
    static REFUSED: RefCell<Vec<(String, gtk::gio::TlsCertificate, webkit2gtk::WebContext)>> =
        const { RefCell::new(Vec::new()) };
}

/// A page whose certificate does not check out.
///
/// WebKit's default is to fail the load, which in this browser means a blank
/// white window and no way forward -- no message, no host, no reason. That is
/// the wrong answer for the case an engineer actually meets: a staging box with
/// a self-signed certificate, or mkcert on localhost.
///
/// So the failure becomes a page that says what happened, and `nav trust` is
/// the way past it. Deliberately *not* a prompt in the palette the way a
/// permission is: a certificate you should not trust and a certificate you
/// issued yourself look identical from here, and a question with Enter on the
/// dangerous answer is not a question. Getting past this should cost a
/// deliberate command.
#[cfg(target_os = "linux")]
fn certificates(webview: &webkit2gtk::WebView, state: Arc<AppState>) {
    use webkit2gtk::WebViewExt as _;

    webview.connect_load_failed_with_tls_errors(move |view, uri, certificate, flags| {
        let host = host_of(uri);
        let reasons = describe(flags);
        tracing::warn!(%uri, %host, ?reasons, "a certificate did not check out");

        // Named in the config as trusted, so this is not a question. The point
        // of the list is the host you issued a certificate for yourself.
        if trusted_by_config(&state, &host) {
            tracing::info!(%host, "trusted by [engine] trust; going on");
            if let Some(context) = view.context() {
                use webkit2gtk::WebContextExt as _;
                context.allow_tls_certificate_for_host(certificate, &host);
            }
            view.load_uri(uri);
            return true;
        }

        match view.context() {
            Some(context) => REFUSED.with(|refused| {
                let mut refused = refused.borrow_mut();
                refused.retain(|(known, _, _)| *known != host);
                refused.push((host.clone(), certificate.clone(), context));
            }),
            // Without a context there is nothing that could ever be told to
            // trust this, so say so on the page rather than offering a command
            // that cannot work.
            None => tracing::warn!(%host, "no web context; this one cannot be trusted"),
        }
        if let Ok(mut slot) = state.tls.lock() {
            *slot = Some(Refused { host, uri: uri.to_string(), reasons });
        }

        view.load_uri(&format!("{}://localhost/tls", crate::window::CHROME_SCHEME));
        true
    });
}

/// Trust the certificate this host offered, and go back to the page.
///
/// Main thread only, like [`answer`]; `nav trust` reaches it through
/// [`trust_host`].
#[cfg(target_os = "linux")]
fn allow_host(host: &str) -> bool {
    use webkit2gtk::WebContextExt as _;

    let found = REFUSED.with(|refused| {
        refused
            .borrow()
            .iter()
            .find(|(known, _, _)| known == host)
            .map(|(_, certificate, context)| (certificate.clone(), context.clone()))
    });
    let Some((certificate, context)) = found else {
        tracing::warn!(%host, "asked to trust a host that never offered a certificate");
        return false;
    };
    // That context is shared by every webview in this window, so trusting once
    // is trusting for the window -- which is what a person means by "trust it".
    context.allow_tls_certificate_for_host(&certificate, host);
    tracing::info!(%host, "trusting this certificate for the rest of the session");
    true
}

/// Ask the main thread to trust a host, from wherever a command is running.
#[cfg(target_os = "linux")]
pub fn trust_host(state: &Arc<AppState>, host: &str) -> Result<()> {
    let app = state.app_handle().context("the window is not up yet")?;
    let host = host.to_string();
    app.run_on_main_thread(move || {
        allow_host(&host);
    })
    .context("could not reach the main thread to trust the certificate")
}

#[cfg(not(target_os = "linux"))]
pub fn trust_host(_state: &Arc<AppState>, _host: &str) -> Result<()> {
    Ok(())
}

/// Whether `[engine] trust` names this host.
///
/// Exact names only, plus a leading `*.` for one level of subdomain. No general
/// globbing: a pattern language in a list that turns certificate checking off
/// is a way to turn it off for more than you meant.
fn trusted_by_config(state: &AppState, host: &str) -> bool {
    state.config.engine.trust.iter().any(|pattern| matches_host(pattern, host))
}

fn matches_host(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim();
    match pattern.strip_prefix("*.") {
        Some(suffix) => host.strip_suffix(suffix).is_some_and(|head| {
            // `*.test` matches `a.test` but not `test` and not `ab.c.test`.
            head.ends_with('.') && !head.trim_end_matches('.').contains('.') && head.len() > 1
        }),
        None => !pattern.is_empty() && pattern.eq_ignore_ascii_case(host),
    }
}

/// The host of a URI, for naming and for trusting.
fn host_of(uri: &str) -> String {
    uri.parse::<url::Url>().ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default()
}

/// What is wrong with a certificate, in words rather than bit flags.
#[cfg(target_os = "linux")]
fn describe(flags: gtk::gio::TlsCertificateFlags) -> Vec<String> {
    use gtk::gio::TlsCertificateFlags as F;

    let named: [(F, &str); 6] = [
        (F::UNKNOWN_CA, "it was not issued by an authority this machine trusts"),
        (F::BAD_IDENTITY, "it was issued for a different host"),
        (F::NOT_ACTIVATED, "it is not valid yet"),
        (F::EXPIRED, "it has expired"),
        (F::REVOKED, "it has been revoked"),
        (F::INSECURE, "it uses an algorithm considered insecure"),
    ];
    let mut out: Vec<String> = named
        .iter()
        .filter(|(flag, _)| flags.contains(*flag))
        .map(|(_, said)| (*said).to_string())
        .collect();
    if out.is_empty() {
        out.push("it did not check out".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trusted_host_is_matched_by_name() {
        assert!(matches_host("staging.example", "staging.example"));
        assert!(matches_host("STAGING.example", "staging.example"));
        assert!(!matches_host("staging.example", "other.example"));
        // An empty entry in the list must not trust the empty host.
        assert!(!matches_host("", ""));
        assert!(!matches_host("  ", "anything"));
    }

    #[test]
    fn a_star_matches_one_level_and_no_more() {
        assert!(matches_host("*.test", "api.test"));
        // Not the bare domain, and not two levels down: a wildcard that grew
        // extra levels is a wildcard that trusts more than it was given.
        assert!(!matches_host("*.test", "test"));
        assert!(!matches_host("*.test", "a.b.test"));
        assert!(!matches_host("*.test", "nottest"));
        assert!(!matches_host("*.test", ".test"));
    }

    #[test]
    fn a_host_comes_out_of_a_uri() {
        assert_eq!(host_of("https://staging.example:8443/a/b"), "staging.example");
        assert_eq!(host_of("not a uri"), "");
    }
}

#[cfg(target_os = "linux")]
thread_local! {
    /// Authentication requests waiting on a username and a password.
    ///
    /// One at a time, and the newest wins: an older challenge for the same page
    /// has already been abandoned by the load that replaced it.
    static CHALLENGE: RefCell<Option<webkit2gtk::AuthenticationRequest>> =
        const { RefCell::new(None) };
}

/// A site behind HTTP authentication.
///
/// With nothing connected here the page comes up *blank* -- no dialog, no
/// error, no 401 body, measured against a real server. A staging box behind
/// basic auth is simply unreachable, which is the same class of failure as the
/// certificate above: the browser knows exactly what is wrong and says nothing.
///
/// So the challenge is held open and the page explains it, and `nav login` is
/// how you answer. Credentials are typed as a command rather than into a field
/// in the palette: the palette's input is not a password field, and putting a
/// password in it would look like somewhere safe to type one.
#[cfg(target_os = "linux")]
fn logins(webview: &webkit2gtk::WebView, state: Arc<AppState>) {
    use webkit2gtk::{AuthenticationRequestExt as _, WebViewExt as _};

    webview.connect_authenticate(move |view, request| {
        // A proxy asking is not the site asking, and this browser has no proxy
        // configuration to have credentials for. Leave it to WebKit.
        if request.is_for_proxy() {
            return false;
        }
        let challenge = Challenge {
            host: request.host().map(|h| h.to_string()).unwrap_or_default(),
            port: request.port(),
            realm: request.realm().map(|r| r.to_string()).unwrap_or_default(),
            uri: view.uri().map(|u| u.to_string()).unwrap_or_default(),
            retry: request.is_retry(),
        };
        let key = challenge.key();
        tracing::info!(host = %challenge.host, realm = %challenge.realm, retry = challenge.retry,
            "a site wants a username and a password");

        // A retry means what was typed was wrong, so forget it rather than
        // answering with it again and locking the account out.
        if challenge.retry {
            forget_login(&state, &key);
        }

        if let Some((user, password)) = stored_login(&state, &key) {
            tracing::debug!(%key, "answering with the login already given");
            answer_challenge(request, &user, &password);
            return true;
        }

        CHALLENGE.with(|slot| *slot.borrow_mut() = Some(request.clone()));
        if let Ok(mut slot) = state.login.lock() {
            *slot = Some(challenge);
        }
        view.load_uri(&format!("{}://localhost/login", crate::window::CHROME_SCHEME));
        true
    });
}

/// Hand a username and password to the request that asked for them.
#[cfg(target_os = "linux")]
fn answer_challenge(request: &webkit2gtk::AuthenticationRequest, username: &str, password: &str) {
    use webkit2gtk::CredentialPersistence;

    // `webkit_authentication_request_authenticate` is the one call in this
    // whole flow that `webkit2gtk` 2.0 does not generate -- it binds `cancel`
    // and the getters but not the answer -- so it is made through the `-sys`
    // crate directly.
    //
    // SAFETY: both pointers come from live, safe wrappers held on this thread
    // for the length of the call, and the function borrows them rather than
    // taking ownership -- `webkit_authentication_request_authenticate` refs the
    // credential itself and copies what it needs. The types are the same types
    // the wrapper crate uses, because `webkit2gtk-sys` is pinned to the version
    // `webkit2gtk` was generated against.
    #[allow(unsafe_code, reason = "the safe crate does not bind this one call; see above")]
    unsafe {
        use gtk::glib::translate::ToGlibPtr as _;

        // FOR_SESSION rather than PERMANENT: a password this browser wrote into
        // the system keyring without ever showing a save prompt is a password
        // somebody did not choose to store.
        let credential =
            webkit2gtk::Credential::new(username, password, CredentialPersistence::ForSession);
        webkit2gtk_sys::webkit_authentication_request_authenticate(
            request.to_glib_none().0,
            credential.to_glib_none().0,
        );
    }
    tracing::info!(username, "answered a login challenge");
}

/// Keep a login for this session, in memory only.
///
/// Never written down. WebKit can put a credential in the system keyring, and
/// this deliberately does not: a password stored without ever showing a "save
/// this?" prompt is a password nobody chose to store. Close the window and it
/// is gone.
pub fn remember_login(state: &Arc<AppState>, key: &str, user: &str, password: &str) {
    if let Ok(mut logins) = state.logins.lock() {
        logins.insert(key.to_string(), (user.to_string(), password.to_string()));
    }
}

fn stored_login(state: &AppState, key: &str) -> Option<(String, String)> {
    state.logins.lock().ok().and_then(|logins| logins.get(key).cloned())
}

fn forget_login(state: &AppState, key: &str) {
    if let Ok(mut logins) = state.logins.lock() {
        logins.remove(key);
    }
}

/// Let go of the challenge that is on screen, once it has been answered another
/// way -- the pending request belongs to a load that has already been replaced.
#[cfg(target_os = "linux")]
pub fn drop_challenge(state: &Arc<AppState>) -> Result<()> {
    let app = state.app_handle().context("the window is not up yet")?;
    app.run_on_main_thread(move || {
        use webkit2gtk::AuthenticationRequestExt as _;
        // Cancelled rather than dropped: the connection is still waiting on an
        // answer, and leaving it waiting leaks it until the page goes away.
        if let Some(request) = CHALLENGE.with(|slot| slot.borrow_mut().take()) {
            request.cancel();
        }
    })
    .context("could not reach the main thread to let go of the challenge")
}

#[cfg(not(target_os = "linux"))]
pub fn drop_challenge(_state: &Arc<AppState>) -> Result<()> {
    Ok(())
}
