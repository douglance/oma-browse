//! The native window: one page filling it, one palette floating above.
//!
//! There is no toolbar. The page owns the whole window, and everything else —
//! the URL bar, tab switching, settings — is summoned into a command palette and
//! dismissed again, the way a TUI does it. See [`crate::layout`] for the GTK
//! surgery that makes a floating overlay possible on a toolkit Tauri only ever
//! stacks vertically.

use std::sync::Arc;

use anyhow::{Context, Result};
use tauri::webview::{Webview, WebviewBuilder};
use tauri::window::WindowBuilder;
use tauri::{LogicalPosition, LogicalSize, Manager, WebviewUrl};

use crate::state::AppState;

/// The palette webview's label. Reserved: it is never a tab.
pub const PALETTE_LABEL: &str = "palette";

/// The scheme the browser's own pages are served on.
///
/// Not `http://127.0.0.1:port`. The palette is a page with a procedure endpoint
/// behind it that can run any command in the catalog, and a loopback port puts
/// that in reach of every process on the machine. A custom scheme is answered
/// inside this process, by this process's webviews, and by nothing else.
pub const CHROME_SCHEME: &str = "oma-chrome";

pub struct Launch {
    pub state: Arc<AppState>,
    pub start_url: url::Url,
    pub incognito: bool,
    /// Theme background, so nothing flashes white before first paint (tauri#10011).
    pub background: oma_theme::Rgb,
    /// Ghostty's `background-opacity`, applied to the window and every webview.
    pub opacity: f64,
    /// The theme's page-facing injection, read on the async side: Tauri owns the
    /// main thread from here on, so nothing may block on the runtime.
    pub page_script: String,
    /// Label for the first tab, allocated before Tauri takes the thread — for
    /// the same reason.
    pub first_tab: String,
    /// The command registry, so keyboard shortcuts dispatch through the same
    /// graph the CLI and MCP do rather than reimplementing it.
    pub catalog: incurs::tool::ToolCatalog,
    /// The browser's own pages, served to its own webviews over
    /// [`CHROME_SCHEME`] rather than over a socket anything else could reach.
    pub chrome: axum::Router,
    /// Come up with the palette already asking where to go. Set by `window new`
    /// when it has no URL to hand the window it is opening; never by a bare
    /// launch, which is someone starting the browser rather than asking it a
    /// question.
    pub open_palette: bool,
}

pub fn run(launch: Launch) -> Result<()> {
    let Launch {
        state,
        start_url,
        incognito,
        background,
        opacity,
        page_script,
        first_tab,
        catalog,
        chrome,
        open_palette,
    } = launch;

    // The veil is painted once, in CSS. Every native surface underneath it is
    // fully transparent, so the two never compound -- see
    // `AppState::background_color`.
    state.set_incognito(incognito);

    // Before GTK builds anything: on Wayland a GTK3 window's `app_id` -- which
    // is what Hyprland calls `class` -- comes from the program name, and the
    // program name is read when the surface is created. Setting it afterwards
    // renames nothing.
    //
    // This is the whole of what makes a `--app` window targetable by a window
    // rule: `windowrule = float, class:oma-browse-app-github-com` cannot be
    // written against a browser that calls every window `oma-browse`.
    if let Some(class) = wm_class(&state, &start_url) {
        tracing::info!(%class, "this window has its own WM class");
        gtk::glib::set_prgname(Some(&class));
    }

    // Either surface wanting translucency is enough to need it from the window:
    // the page's veil is an element, but chrome's alpha has nothing behind it
    // except whatever the compositor shows through an RGBA window.
    let chrome_veil = state.config.chrome.veil.resolve(opacity);
    let translucent = opacity.clamp(0.0, 1.0) < 1.0 || chrome_veil < 1.0;
    let alpha = if translucent { 0 } else { 255 };
    let bg = tauri::window::Color(background.r, background.g, background.b, alpha);
    // The strip is transparent whatever the theme is doing: it floats over the
    // page now, and an opaque one would be the bar this deliberately is not.
    let strip_bg = tauri::window::Color(background.r, background.g, background.b, 0);
    // Before the builder takes `state`: the exit hook below outlives the setup
    // closure, and by then there is no `AppState` to ask.
    let exit_dir = crate::control::dir_for(&state.config.control);
    // The chrome comes off the custom scheme, not the listener: these are our
    // pages, loaded by our webviews, and nothing outside this process has any
    // business reaching them.
    let palette_url: url::Url = format!("{CHROME_SCHEME}://localhost/palette").parse()?;
    let strip_url: url::Url = format!("{CHROME_SCHEME}://localhost/strip").parse()?;

    let chrome_runtime = state.runtime();
    tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol(CHROME_SCHEME, move |ctx, request, responder| {
            let label = ctx.webview_label().to_string();
            let router = chrome.clone();
            // Off the GTK thread: rendering a page runs the same async router
            // the listener does, and blocking here would freeze the window that
            // is waiting for the answer.
            chrome_runtime.spawn(async move {
                responder.respond(chrome_page(router, label, request).await);
            });
        })
        .setup(move |app| {
            state.set_app_handle(app.handle().clone());

            let window = WindowBuilder::new(app, "main")
                .title(&state.config.window.title)
                .inner_size(state.config.window.width, state.config.window.height)
                // Omarchy tiles everything and paints its own themed border, so a
                // GTK titlebar is wasted rows. `SUPER + W` closes the window, as
                // with every other Omarchy app.
                .decorations(state.config.window.decorations)
                // Ghostty-style translucency: the window carries the alpha and
                // the compositor shows the wallpaper through it. Only ask for a
                // transparent window when the theme actually wants one — an
                // unnecessarily transparent surface costs a compositor pass.
                .transparent(translucent)
                .background_color(bg)
                .build()
                .context("could not create the window")?;

            // "The window I was last looking at" is a question only the window
            // can answer, so it answers it: every focus re-points `current.sock`
            // at this process, and `oma-browse tab open <url>` follows the link.
            // The alternative was asking the compositor, which hard-codes
            // Hyprland into a browser to learn something it already knows.
            let focus_dir = crate::control::dir_for(&state.config.control);
            window.on_window_event(move |event| {
                if matches!(event, tauri::WindowEvent::Focused(true)) {
                    crate::control::point_current_in(&focus_dir, std::process::id());
                }
            });

            // Created first so it is the widget we can find the vbox through;
            // `layout::install` then lifts it out into the overlay.
            let palette = window
                .add_child(
                    WebviewBuilder::new(
                        PALETTE_LABEL,
                        WebviewUrl::CustomProtocol(palette_url.clone()),
                    )
                    .transparent(translucent)
                    .background_color(bg),
                    LogicalPosition::new(0.0, 0.0),
                    LogicalSize::new(720.0, 420.0),
                )
                .context("could not create the palette webview")?;

            let label = first_tab.clone();
            let first = crate::profile::in_profile(
                WebviewBuilder::new(&label, WebviewUrl::External(start_url.clone()))
                    .auto_resize()
                    .transparent(translucent)
                    .background_color(bg)
                    .initialization_script(&page_script)
                    .incognito(incognito),
            );
            let content = window
                .add_child(
                    instrument(first, state.clone()),
                    LogicalPosition::new(0.0, 0.0),
                    LogicalSize::new(state.config.window.width, state.config.window.height),
                )
                .context("could not create the first tab")?;

            // Favicons are per webview, and this is the first one.
            // The download signal is on the shared web context, so this registers
            // once however many tabs ask; see `downloads::watch`.
            if let Err(e) = crate::downloads::watch(&content, state.clone()) {
                tracing::warn!(error = %e, "not watching downloads");
            }
            if let Err(e) = crate::favicon::watch(&content, state.clone()) {
                tracing::warn!(error = %e, "not watching the first tab's favicon");
            }
            if let Err(e) = crate::progress::watch(&content, state.clone()) {
                tracing::warn!(error = %e, "the first tab loads without a progress bar");
            }
            // `[engine]`, which is per webview like the two above -- and this is
            // the one webview `tabs::open` never sees.
            if let Err(e) = crate::engine::configure(&content, state.clone()) {
                tracing::warn!(error = %e, "the first tab kept WebKit's own settings");
            }
            // Likewise: pop-ups, permissions and bad certificates are answered
            // per webview, and this is the one webview `tabs::open` never sees.
            if let Err(e) = crate::policy::install(&content, state.clone()) {
                tracing::warn!(error = %e, "the first tab answers pages with WebKit's defaults");
            }
            if let Err(e) = crate::inspect::install(&content, state.clone()) {
                tracing::warn!(error = %e, "the first tab keeps no console or network log");
            }
            if let Err(e) = crate::blocker::install(&content, state.clone()) {
                tracing::warn!(error = %e, "the first tab blocks nothing");
            }
            // On the main thread, which is where the filter store lives, and
            // after the first webview so that anything already cached is applied
            // to it rather than only to the second tab.
            for problem in crate::blocker::reload(&state) {
                state.note_config_problem(format!("content.rules: {problem}"));
            }

            crate::layout::install(&palette, &state.config.chrome)?;
            crate::layout::install_keys(&palette, state.clone(), catalog.clone(), state.runtime())?;

            // After the surgery, not before: `install` sweeps everything in the
            // window box except the palette into the content stack, and a strip
            // created earlier would be swept in with it and become a tab-shaped
            // hole in the page. It also needs the overlay to exist, since that
            // is what it floats in.
            if state.strip_enabled() {
                let height = state.config.chrome.strip.height;
                let strip = window
                    .add_child(
                        WebviewBuilder::new(
                            crate::strip::LABEL,
                            WebviewUrl::CustomProtocol(strip_url.clone()),
                        )
                        .transparent(true)
                        .background_color(strip_bg),
                        LogicalPosition::new(0.0, 0.0),
                        LogicalSize::new(state.config.window.width, f64::from(height)),
                    )
                    .context("could not create the tab strip webview")?;
                crate::layout::adopt_strip(&strip, height)?;
                spawn_strip_refresh(state.clone());
            }

            // Nothing holds keyboard focus after the surgery, so page-level key
            // handlers — which is where every shortcut lives — would never fire.
            if let Err(e) = crate::layout::focus(&content) {
                tracing::warn!(error = %e, "could not focus the first tab");
            }

            tracing::info!(palette = %palette_url, content = %start_url, "window up");

            // Last, once there is a palette to show and a page behind it: a
            // window opened with nowhere to go asks where, exactly as a tab
            // opened with nowhere to go does. See `crate::window::spawn`.
            if open_palette {
                match set_palette_visible(&state, true) {
                    Ok(()) => state.set_palette_visible(true),
                    Err(e) => {
                        tracing::warn!(error = %e, "could not summon the palette at startup");
                    }
                }
            }
            Ok(())
        })
        // `build` then `run` rather than `Builder::run`, which is the same two
        // calls with an empty callback: this is the only hook that fires before
        // the process goes. Tauri exits it directly, so nothing on this stack
        // unwinds and a `Drop` guard would be a lie -- which is also why the
        // socket's liveness is settled by connecting to it rather than by
        // trusting that this ran.
        .build(tauri::generate_context!())
        .context("could not start the Tauri application")?
        .run(move |_app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                crate::control::unlink_in(&exit_dir, std::process::id());
            }
        });

    Ok(())
}

/// Answer one request for a page of the browser's own chrome.
///
/// The router is the one `server.rs` composes, so the palette is the same page
/// however it is reached; only the way in differs.
async fn chrome_page(
    router: axum::Router,
    label: String,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    use tower::ServiceExt as _;

    let (parts, body) = request.into_parts();
    if !may_see(&label, parts.uri.path()) {
        tracing::warn!(webview = %label, path = %parts.uri.path(), "refused the chrome");
        return refusal(tauri::http::StatusCode::FORBIDDEN, "not for this webview");
    }

    let request = axum::http::Request::from_parts(parts, axum::body::Body::from(body));
    let Ok(response) = router.oneshot(request).await;
    let (parts, body) = response.into_parts();
    match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => tauri::http::Response::from_parts(parts, bytes.to_vec()),
        Err(e) => {
            tracing::error!(error = %e, "could not read a chrome response");
            refusal(tauri::http::StatusCode::INTERNAL_SERVER_ERROR, "the page did not finish")
        }
    }
}

/// Who may ask for what.
///
/// The scheme is registered on the shared web context, so a *page* could ask for
/// it too -- and the palette's procedure endpoint runs commands. The chrome
/// belongs to the two webviews that are the chrome; a tab gets the start page
/// and the artwork on it, which is all a tab is ever pointed at.
fn may_see(label: &str, path: &str) -> bool {
    if label == PALETTE_LABEL || label == crate::strip::LABEL {
        return true;
    }
    // `/tls` is the interstitial a refused certificate lands on, so a content
    // webview has to be able to see it -- it is shown *instead of* the page.
    matches!(path, "/start" | "/tls" | "/login" | "/mark.png" | "/icon.png" | "/favicon.ico")
        || path.starts_with("/_topcoat/assets/")
}

fn refusal(status: tauri::http::StatusCode, message: &str) -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(message.as_bytes().to_vec())
        .unwrap_or_else(|_| tauri::http::Response::new(Vec::new()))
}

/// Attach the callbacks that keep the tab model in step with what a webview is
/// actually doing. Applied to every content webview, wherever it was created.
pub fn instrument(
    builder: WebviewBuilder<tauri::Wry>,
    state: Arc<AppState>,
) -> WebviewBuilder<tauri::Wry> {
    let title_state = state.clone();
    let nav_state = state.clone();
    let load_state = state;

    builder
        // Inert for every real navigation; see `crate::hints::intercept`, which
        // is the only way a page can ask the browser to open a tab.
        .on_navigation(move |url| crate::hints::intercept(&nav_state, url))
        .on_document_title_changed(move |webview, title| {
            let state = title_state.clone();
            let label = webview.label().to_string();
            state.runtime().spawn(async move {
                let url = state.tabs.read().await.url_for(&label);
                state.tabs.write().await.update_title(&label, title.clone());
                if state.keeps_history()
                    && let Some(url) = url
                {
                    let mut history = state.history.write().await;
                    history.set_title(&url, &title);
                    history.flush();
                }
                state.notify_tabs();
            });
        })
        .on_page_load(move |webview, payload| {
            let state = load_state.clone();
            let label = webview.label().to_string();
            let url = payload.url().to_string();
            state.runtime().spawn(async move {
                state.tabs.write().await.update_url(&label, url.clone());
                if state.keeps_history() {
                    let mut history = state.history.write().await;
                    history.record(&url, crate::history::now());
                    history.flush();
                }
                state.notify_tabs();
            });
        })
}

/// What this window should call itself to the compositor.
///
/// `None` for an ordinary browser window, which keeps the name the binary was
/// launched under -- renaming those would break every window rule anybody has
/// already written.
fn wm_class(state: &Arc<AppState>, url: &url::Url) -> Option<String> {
    if !state.app_mode() {
        return None;
    }
    Some(format!("oma-browse-app-{}", class_slug(url.host_str().unwrap_or("app"))))
}

/// A host, as something a Hyprland rule can be written against.
fn class_slug(host: &str) -> String {
    let slug: String = host
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() { "app".to_string() } else { trimmed }
}

/// Re-dress every surface for the current theme, without a restart.
///
/// Three separate things have to change, because they are themed by three
/// different mechanisms:
///
/// * loaded pages carry an injected `<style>` element, which we re-run — the
///   script keys off a sentinel id, so it updates in place rather than stacking;
/// * the palette is our own page, so a reload is enough;
/// * `prefers-color-scheme` inside WebKit comes from GTK's
///   `gtk-application-prefer-dark-theme`, a *process-wide* setting that cannot
///   be varied per webview, so it is set once for the whole app.
pub async fn restyle(state: &Arc<AppState>) -> Result<()> {
    let app = state.app_handle().context("the window is not up yet")?;

    let (script, dark, color, strip_color) = {
        let theme = state.theme.read().await;
        let bg = theme.css.tint;
        // Fully transparent while the theme is translucent -- see
        // `AppState::background_color`; the veil belongs to the CSS alone.
        let alpha = if theme.css.opacity < 1.0 { 0 } else { 255 };
        (
            // The strip's inset goes back in with it: this is the same
            // injection `AppState::page_script` composes for a fresh tab, and a
            // restyled page has to end up in the same state as a new one.
            format!("{}\n{}", theme.css.page_script(state.recolor()), state.inset_script()),
            theme.mode.is_dark(),
            tauri::window::Color(bg.r, bg.g, bg.b, alpha),
            tauri::window::Color(bg.r, bg.g, bg.b, 0),
        )
    };

    for tab in state.tabs.read().await.list() {
        if let Some(view) = app.get_webview(&tab.label) {
            let _ = view.set_background_color(Some(color));
            if let Err(e) = view.eval(&script) {
                tracing::debug!(tab = %tab.label, error = %e, "could not restyle tab");
            }
        }
    }

    if let Some(palette) = app.get_webview(PALETTE_LABEL) {
        let _ = palette.set_background_color(Some(color));
        let _ = palette.reload();

        // GTK settings must be touched on the main thread; ride along with the
        // palette's webview to get there.
        let _ = palette.with_webview(move |_| {
            use gtk::prelude::*;
            if let Some(settings) = gtk::Settings::default() {
                settings.set_gtk_application_prefer_dark_theme(dark);
            }
        });
    }

    // The strip is our own page as well, so like the palette it only needs the
    // new stylesheet, which a reload fetches.
    if let Some(strip) = app.get_webview(crate::strip::LABEL) {
        // Transparent in every theme -- see `run`.
        let _ = strip.set_background_color(Some(strip_color));
        let _ = strip.reload();
    }

    if let Some(window) = app.get_window("main") {
        let _ = window.set_background_color(Some(color));
    }

    Ok(())
}

/// Redraw the strip whenever the tab model changes.
///
/// The first subscriber to [`crate::state::UiEvent`]. A reload rather than a
/// diff: the strip is a local page of a dozen elements, so re-rendering it
/// server-side costs less than a mechanism for telling it what changed would.
fn spawn_strip_refresh(state: Arc<AppState>) {
    let runtime = state.runtime();
    runtime.spawn(async move {
        use tokio::sync::broadcast::error::RecvError;

        let mut events = state.subscribe();
        loop {
            match events.recv().await {
                // The strip re-renders from `AppState` either way, so the
                // payload is worth a line in the log and nothing more. It is
                // worth that much: a restyle that did not land is otherwise
                // indistinguishable from a theme that did not change.
                Ok(crate::state::UiEvent::ThemeChanged { name, mode }) => {
                    tracing::debug!(theme = %name, ?mode, "redrawing the strip for a new theme");
                }
                Ok(crate::state::UiEvent::TabsChanged) => {}
                // Lagging means more changed than we heard about, which is the
                // same instruction as any single event: redraw.
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }

            // One page load fires a URL change, a title change and a favicon,
            // in that order and milliseconds apart. Redrawing on each is three
            // reloads of the same row, and the middle one has no icon yet.
            tokio::time::sleep(std::time::Duration::from_millis(
                state.config.chrome.strip.debounce_ms,
            ))
            .await;
            while events.try_recv().is_ok() {}

            if let Some(strip) =
                state.app_handle().and_then(|app| app.get_webview(crate::strip::LABEL))
            {
                let _ = strip.reload();
            }
        }
    });
}

/// Take the window fullscreen, or bring it back. Returns the new state.
///
/// Tauri's own call rather than `gtk::Window::fullscreen`, so this is the
/// Wayland fullscreen state the compositor understands — under Hyprland that is
/// a real fullscreen, not a window resized to fill its tile.
pub fn set_fullscreen(state: &Arc<AppState>, want: Option<bool>) -> Result<bool> {
    let app = state.app_handle().context("the window is not up yet")?;
    let window = app.get_window("main").context("the main window has gone away")?;
    let now = window.is_fullscreen().unwrap_or(false);
    let want = want.unwrap_or(!now);
    if want != now {
        window.set_fullscreen(want).context("could not change the fullscreen state")?;
    }
    Ok(want)
}

/// Close the window, and with it the browser.
pub fn close(state: &Arc<AppState>) -> Result<()> {
    let app = state.app_handle().context("the window is not up yet")?;
    let window = app.get_window("main").context("the main window has gone away")?;
    window.close().context("could not close the window")
}

/// Open another browser window.
///
/// A window here is a *process*. Everything a window owns -- the tab model, the
/// palette, the strip, the theme watcher -- hangs off one `AppState` reached
/// through one app handle, and the two chrome webviews are found by the fixed
/// labels `palette` and `strip`. A second window inside this process would mean
/// giving every one of those a window dimension; launching the binary again
/// gets the same window for the price of an `exec`, and it is already how
/// `omarchy-launch-browser` opens us from the outside.
///
/// The child is told to open its palette when it is being sent nowhere in
/// particular, so Ctrl-N lands in the same "where to?" state Ctrl-T does.
///
/// `quiet` sends its logs to nowhere. Ctrl-N wants them: a window opened from a
/// browser you started in a terminal should keep reporting to that terminal. A
/// window opened by `oma-browse tab open` does not -- that command returns to a
/// prompt, and a browser writing to the prompt it left behind is noise the user
/// did not ask for.
/// Resize the window, and say whether it actually happened.
///
/// The obvious implementation -- `Window::set_size` -- does nothing at all on
/// Wayland, and it does nothing *silently*. There is no xdg-shell request for a
/// client to set its own size: a client draws at whatever size the compositor
/// configures it to, and asking is not part of the protocol. Measured on a
/// genuinely floating window: `set_size(900, 600)` returned `Ok`, the command
/// answered "900x600", and Hyprland went on reporting 1400x900.
///
/// So under Hyprland the request goes to the compositor, which is the only
/// thing that can grant it. That makes this the second place in the browser
/// that knows what compositor it is on, beside [`spawn_on`], and it borrows
/// that function's helpers rather than growing its own.
///
/// A *tiled* window is still sized by the layout whatever anyone asks -- the
/// dispatcher answers `ok` and the size does not move. That is why this reads
/// the size back afterwards instead of trusting either call: the honest answer
/// to `window resize` on a tiled window is "no", and the previous version of
/// this said "yes".
pub fn resize(state: &Arc<AppState>, width: f64, height: f64) -> Result<Placed> {
    let app = state.app_handle().context("the window is not up yet")?;
    let window = app.get_window("main").context("the main window has gone away")?;

    // Still asked for, and first: on a backend where a client *can* size itself
    // -- X11, or a future non-Wayland target -- this is the whole of the job,
    // and it is harmless where it is not.
    window
        .set_size(LogicalSize::new(width, height))
        .context("the window would not take that size")?;

    if !under_hyprland() {
        // Nothing to read back from, and nothing better to try. Report what was
        // asked for, and be plain that it was a request.
        return Ok(Placed {
            width,
            height,
            applied: false,
            note: Some(
                "not running under Hyprland; the compositor decides, and this build                  cannot ask it directly"
                    .to_string(),
            ),
        });
    }

    let pid = std::process::id();
    let dispatch = resize_lua(pid, width, height);
    let out = std::process::Command::new("hyprctl")
        .arg("dispatch")
        .arg(&dispatch)
        .output()
        .context("could not reach hyprctl to resize the window")?;
    let reply = String::from_utf8_lossy(&out.stdout);
    // As `spawn_on`: hyprctl exits 0 and prints the complaint, so the body is
    // the answer and the status is not.
    if !out.status.success() || !reply.trim().eq_ignore_ascii_case("ok") {
        anyhow::bail!(
            "hyprland would not resize the window: {}",
            reply.trim().lines().next().unwrap_or("no reply")
        );
    }

    let Some((actual_w, actual_h, floating)) = geometry(pid) else {
        return Ok(Placed { width, height, applied: false, note: None });
    };
    let landed = (actual_w - width).abs() < 2.0 && (actual_h - height).abs() < 2.0;
    Ok(Placed {
        width: actual_w,
        height: actual_h,
        applied: landed,
        note: (!landed).then(|| {
            if floating {
                "the compositor chose a different size".to_string()
            } else {
                "this window is tiled, so its size is the layout's; float it first                  (SUPER + T in a stock Omarchy)"
                    .to_string()
            }
        }),
    })
}

/// What a resize actually produced.
#[derive(Debug, Clone, Default)]
pub struct Placed {
    pub width: f64,
    pub height: f64,
    /// Whether the window is now the size that was asked for.
    pub applied: bool,
    /// Why not, when it is not.
    pub note: Option<String>,
}

/// The dispatch Hyprland wants, addressed at one process rather than at
/// whatever happens to be focused.
///
/// `pid:` and not `address:` because this process knows its own pid and would
/// otherwise have to go and look its own window up to find out what it is.
/// Nothing here needs quoting -- a pid is digits and the sizes are integers --
/// which is the only reason this is a `format!` and not a trip through
/// [`lua_quote`].
fn resize_lua(pid: u32, width: f64, height: f64) -> String {
    format!(
        "hl.dsp.window.resize({{ window = \"pid:{pid}\", x = {}, y = {}, relative = false }})",
        width.round() as i64,
        height.round() as i64
    )
}

/// This process's window, as the compositor sees it: width, height, floating.
fn geometry(pid: u32) -> Option<(f64, f64, bool)> {
    let out = std::process::Command::new("hyprctl").args(["clients", "-j"]).output().ok()?;
    let clients: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    for client in clients.as_array()? {
        if client.get("pid").and_then(serde_json::Value::as_u64) != Some(u64::from(pid)) {
            continue;
        }
        let size = client.get("size")?.as_array()?;
        return Some((
            size.first()?.as_f64()?,
            size.get(1)?.as_f64()?,
            client.get("floating").and_then(serde_json::Value::as_bool).unwrap_or(false),
        ));
    }
    None
}

/// `1280x720`, as a pair. `None` for anything that is not two numbers with an
/// `x` between them.
pub fn parse_size(raw: &str) -> Option<(f64, f64)> {
    let cleaned = raw.trim().to_ascii_lowercase();
    let (w, h) = cleaned.split_once('x')?;
    let width: f64 = w.trim().parse().ok()?;
    let height: f64 = h.trim().parse().ok()?;
    // A window with no area is not a window, and neither is one the size of a
    // billboard: both are typos, and both are better refused here than sent to
    // a compositor that will do something surprising with them.
    (width >= 100.0 && height >= 100.0 && width <= 16_384.0 && height <= 16_384.0)
        .then_some((width, height))
}

pub fn spawn(incognito: bool, url: Option<String>, palette: bool, quiet: bool) -> Result<u32> {
    Ok(spawn_on(incognito, url, palette, quiet, None)?.pid.unwrap_or_default())
}

/// What opening a window produced.
///
/// `pid` is absent for exactly one case: a window Hyprland launched for us, on
/// a workspace, because `hyprctl` answers `ok` rather than a process id. Every
/// other path knows the child it forked.
#[derive(Debug, Default)]
pub struct Opened {
    pub pid: Option<u32>,
    /// The workspace it was placed on, if it was placed at all.
    pub workspace: Option<String>,
}

/// A workspace name Hyprland will accept and a shell will not reinterpret.
///
/// This reaches the browser from the CLI, the HTTP API and MCP, and it is
/// interpolated into a Lua string that Hyprland then runs through a shell. A
/// deny-list would be the wrong shape here, so the allow-list is the Hyprland
/// selector grammar and nothing else: `4`, `name:web`, `special:magic`,
/// `+1`, `-1`, `empty`.
fn workspace_is_safe(ws: &str) -> bool {
    !ws.is_empty()
        && ws.len() <= 64
        && ws.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '+'))
}

/// Hyprland, or not.
///
/// The signature variable is set by the compositor for every client it starts,
/// so this is asking "am I running under Hyprland" rather than "is hyprctl
/// installed" -- which is the actual question, and is also true inside a
/// Hyprland session that has `hyprctl` missing for some other reason.
fn under_hyprland() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

/// One argument, safe to paste into a command line a shell will parse.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// One string, safe to paste into a Lua double-quoted literal.
///
/// Backslash first, or the escape would escape the backslash it just added. The
/// two line endings are here because Lua rejects a raw newline inside a short
/// string: without them a name containing one produces a syntax error rather
/// than a quoted name. Nothing reaching this today can contain one --
/// `workspace_is_safe` refuses it and a resolved URL has none -- so this is the
/// function keeping its own promise rather than a live bug being fixed.
fn lua_quote(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', "\\\"").replace('\n', r"\n").replace('\r', r"\r")
}

/// Open a window, optionally on a given Hyprland workspace.
///
/// With a workspace, Hyprland launches the binary itself with the rule already
/// attached, so the window is on the target from its first frame. The
/// alternative -- fork here, then ask the compositor to move the window -- races
/// the window's own creation and shows it jumping when it loses.
///
/// This is the one place in the browser that knows what compositor it is on.
/// It is confined here deliberately: everywhere else, a window is a process and
/// nothing more (see the module docs and `crate::control`). Off Hyprland the
/// window still opens, on whatever workspace you are standing on, and the
/// caller is told the placement did not happen rather than being failed.
pub fn spawn_on(
    incognito: bool,
    url: Option<String>,
    palette: bool,
    quiet: bool,
    workspace: Option<&str>,
) -> Result<Opened> {
    let exe = std::env::current_exe().context("could not find the running browser binary")?;

    // Built as a list rather than pushed straight onto a `Command`, because the
    // Hyprland path below needs the same argv as a string for the compositor to
    // run, and two builders would drift.
    let mut args: Vec<String> = Vec::new();
    // An incognito window opens incognito windows: the alternative is a chord
    // that quietly drops you back into a session that records history.
    if incognito {
        args.push("--incognito".into());
    }
    // A window opened from a profile belongs to that profile. The flag is
    // stripped from argv on the way in (see `crate::profile::take_flag`), so it
    // has to be put back on the way out.
    if let Some(profile) = crate::profile::name() {
        args.push("--profile".into());
        args.push(profile.to_string());
    }
    // `--new` on both paths: this is Ctrl-N, and a new window is the whole
    // point. Without it the URL would be handed to the window we are standing
    // in (see `main::join`).
    args.push("--new".into());
    if let Some(url) = url {
        args.push(url);
    }
    // Ctrl-N with nowhere to go comes up asking where; a window opened *for* an
    // agent does not, because the palette would then be sitting over every page
    // it screenshots and every element it clicks.
    if palette {
        args.push("--palette".into());
    }

    if let Some(ws) = workspace {
        if !workspace_is_safe(ws) {
            anyhow::bail!("{ws:?} is not a workspace name");
        }
        if under_hyprland() {
            let mut line = shell_quote(&exe.to_string_lossy());
            for arg in &args {
                line.push(' ');
                line.push_str(&shell_quote(arg));
            }
            let lua = format!(
                "hl.dsp.exec_cmd(\"[workspace {ws} silent] {cmd}\")",
                ws = lua_quote(ws),
                cmd = lua_quote(&line),
            );
            let out = std::process::Command::new("hyprctl")
                .arg("dispatch")
                .arg(&lua)
                .output()
                .context("could not reach hyprctl to place the window")?;
            let reply = String::from_utf8_lossy(&out.stdout);
            // `hyprctl` exits 0 and prints the complaint, so the status is not
            // the answer -- the body is.
            if !out.status.success() || !reply.trim().eq_ignore_ascii_case("ok") {
                anyhow::bail!(
                    "hyprland refused to open the window: {}",
                    reply.trim().lines().next().unwrap_or("no reply")
                );
            }
            tracing::info!(workspace = ws, "opened another window");
            return Ok(Opened { pid: None, workspace: Some(ws.to_string()) });
        }
        tracing::warn!(
            workspace = ws,
            "not running under Hyprland; opening on the current workspace instead"
        );
    }

    let mut command = std::process::Command::new(exe);
    command.args(&args);

    // Nothing may reach the child through our stdin either way.
    if quiet {
        command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    }
    let child = command
        .stdin(std::process::Stdio::null())
        .spawn()
        .context("could not launch another browser window")?;
    let pid = child.id();

    // Nobody would otherwise wait on this child, and an exit nobody waits on is
    // a zombie for as long as this process lives -- which, for a browser, is all
    // day. The thread costs nothing and ends when the window does.
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });

    tracing::info!(pid, "opened another window");
    Ok(Opened { pid: Some(pid), workspace: None })
}

/// Show or hide the palette, and hand it focus when it appears.
pub fn set_palette_visible(state: &Arc<AppState>, visible: bool) -> Result<()> {
    let app = state.app_handle().context("the window is not up yet")?;
    let palette: Webview<tauri::Wry> =
        app.get_webview(PALETTE_LABEL).context("the palette webview is missing")?;

    if visible {
        // Re-render on every summon. The palette is a long-lived webview, so
        // without this it shows whatever was true last time — a stale tab list
        // and whatever you typed before. Reloading is cheap: it is a local page,
        // and it means the palette always opens empty and current.
        let _ = palette.reload();
        palette.show().context("could not show the palette")?;
        // Without this the keystrokes keep going to the page behind it.
        palette.set_focus().context("could not focus the palette")?;
    } else {
        palette.hide().context("could not hide the palette")?;
        // Give focus back to the page, or typing goes nowhere.
        if let Some(label) = state.tabs.try_read().ok().and_then(|t| t.active_label())
            && let Some(tab) = app.get_webview(&label)
        {
            let _ = crate::layout::focus(&tab);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resize_is_addressed_at_this_process_and_not_at_whatever_is_focused() {
        let lua = resize_lua(4242, 900.0, 600.0);
        assert!(lua.contains(r#"window = "pid:4242""#), "{lua}");
        // Integers: Hyprland's dispatcher takes pixels, and `900.0` in the Lua
        // would be a float where it wants a number of them.
        assert!(lua.contains("x = 900"), "{lua}");
        assert!(lua.contains("y = 600"), "{lua}");
        assert!(!lua.contains("900.0"), "sizes must not go over as floats: {lua}");
        assert!(lua.contains("relative = false"), "{lua}");
    }

    #[test]
    fn a_wm_class_is_something_a_window_rule_can_name() {
        assert_eq!(class_slug("github.com"), "github-com");
        assert_eq!(class_slug("app.slack.com"), "app-slack-com");
        assert_eq!(class_slug("127.0.0.1:8911"), "127-0-0-1-8911");
        assert_eq!(class_slug("EXAMPLE.COM"), "example-com");
        assert_eq!(class_slug(""), "app");
        assert_eq!(class_slug("..."), "app");
    }

    #[test]
    fn a_size_is_two_numbers_with_an_x_between_them() {
        assert_eq!(parse_size("1280x720"), Some((1280.0, 720.0)));
        assert_eq!(parse_size(" 375 X 812 "), Some((375.0, 812.0)));
        assert_eq!(parse_size("1280"), None);
        assert_eq!(parse_size("wide x tall"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn a_window_with_no_area_is_refused() {
        assert_eq!(parse_size("0x0"), None);
        assert_eq!(parse_size("10x10"), None);
        assert_eq!(parse_size("99999x99999"), None);
        // The smallest phone anybody tests against still works.
        assert_eq!(parse_size("320x568"), Some((320.0, 568.0)));
    }

    /// The workspace name is interpolated into a Lua literal that Hyprland then
    /// runs through a shell, and it arrives from the CLI, the HTTP API and MCP.
    #[test]
    fn a_workspace_name_is_a_selector_and_nothing_else() {
        for ok in ["4", "name:web", "special:magic", "+1", "-1", "empty", "a_b-c"] {
            assert!(workspace_is_safe(ok), "{ok:?} should be allowed");
        }
        for bad in [
            "",
            "4 evil",
            "4\"; touch /tmp/pwned #",
            "$(id)",
            "`id`",
            "a;b",
            "a\nb",
            "a|b",
            "a&b",
            "../../etc",
        ] {
            assert!(!workspace_is_safe(bad), "{bad:?} should be refused");
        }
        assert!(!workspace_is_safe(&"4".repeat(65)));
    }

    /// Both quoters, round-tripped: what a shell finally sees must be the byte
    /// string we started with. `--profile` names flow through here too.
    #[test]
    fn quoting_survives_the_lua_then_shell_round_trip() {
        // What `lua_quote` produces is what Lua hands to the shell, so undoing
        // the Lua escaping is how we see the shell's input.
        fn lua_unescape(s: &str) -> String {
            let mut out = String::new();
            let mut chars = s.chars();
            while let Some(c) = chars.next() {
                if c != '\\' {
                    out.push(c);
                    continue;
                }
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some(other) => out.push(other),
                    None => out.push('\\'),
                }
            }
            out
        }

        // And undoing the single-quote wrapping is how we see the argument.
        // Minimal POSIX word rules, which is all `shell_quote` ever emits: a
        // quote opens or closes a literal run, and outside one a backslash
        // escapes whatever follows it.
        fn shell_unquote(s: &str) -> String {
            let mut out = String::new();
            let mut chars = s.chars();
            let mut quoted = false;
            while let Some(c) = chars.next() {
                match c {
                    '\'' => quoted = !quoted,
                    '\\' if !quoted => {
                        if let Some(next) = chars.next() {
                            out.push(next);
                        }
                    }
                    other => out.push(other),
                }
            }
            out
        }

        for raw in [
            "plain",
            "a\"b",
            "a'b",
            "a\\b",
            "https://example.com/?q=1&r=2",
            "work profile",
            "quote\"and'both",
        ] {
            let shell_ready = shell_quote(raw);
            let lua_ready = lua_quote(&shell_ready);
            assert_eq!(
                shell_unquote(&lua_unescape(&lua_ready)),
                raw,
                "round trip lost {raw:?} (shell {shell_ready:?}, lua {lua_ready:?})"
            );
        }
    }

    /// Lua has no raw newline inside a short string, so neither may we emit one.
    #[test]
    fn lua_quoting_emits_no_raw_line_ending() {
        let out = lua_quote("a\nb\r\nc");
        assert!(!out.contains('\n') && !out.contains('\r'), "emitted a raw line ending: {out:?}");
        assert_eq!(out, r"a\nb\r\nc");
    }
}
