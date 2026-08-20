//! Screenshots, taken by the browser itself.
//!
//! The alternative is a compositor grabber like `grim`, which needs the window
//! to be on the active workspace, needs its geometry looked up separately, and
//! silently captures whatever else is on screen when either assumption is
//! wrong. WebKit already knows how to paint the page into a surface; asking it
//! directly removes all three failure modes and works while the window is on
//! another workspace entirely.
//!
//! `page screenshot` is therefore the supported way to look at a page --
//! including for an agent, which is why it lives in the command graph rather
//! than in a script somewhere.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use tauri::Manager as _;
use serde::Serialize;

use crate::state::AppState;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Shot {
    /// Where the PNG was written.
    pub path: String,
    pub width: i32,
    pub height: i32,
    /// The tab it came from.
    pub label: String,
}

/// Capture the active tab.
///
/// `full` swaps WebKit's visible-region snapshot for the whole scrollable
/// document, which is what you want when checking that theming reaches the
/// bottom of a long page rather than just the part that fits on screen.
pub async fn capture(
    state: &Arc<AppState>,
    path: Option<String>,
    full: bool,
    transparent: bool,
) -> Result<Shot> {
    let app = state.app_handle().context("the window is not up yet")?;
    let label = state
        .tabs
        .read()
        .await
        .active_label()
        .ok_or_else(|| anyhow!("there is no active tab"))?;
    let view = app.get_webview(&label).with_context(|| format!("no webview labelled {label}"))?;

    let path = resolve_path(path, &state.config.screenshot.dir)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<(i32, i32), String>>();
    let target = path.clone();

    view.with_webview(move |platform| {
        use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt, gio};

        let region = if full { SnapshotRegion::FullDocument } else { SnapshotRegion::Visible };
        // Without this the veil is composited onto WebKit's default white, and
        // every screenshot of a translucent page comes back looking opaque --
        // which is exactly the thing we most often need to inspect.
        let options = if transparent {
            SnapshotOptions::TRANSPARENT_BACKGROUND
        } else {
            SnapshotOptions::NONE
        };

        // The callback lands back on the GTK main thread, and a cairo surface is
        // not `Send`, so the PNG is written here and only the dimensions travel.
        platform.inner().snapshot(
            region,
            options,
            None::<&gio::Cancellable>,
            move |result| {
                let _ = tx.send(write_png(result, &target));
            },
        );
    })
    .context("could not reach the webview to snapshot it")?;

    match rx.await {
        Ok(Ok((width, height))) => {
            Ok(Shot { path: path.display().to_string(), width, height, label })
        }
        Ok(Err(e)) => bail!("{e}"),
        Err(_) => bail!("the snapshot was dropped before it finished"),
    }
}

fn write_png(
    result: Result<gtk::cairo::Surface, webkit2gtk::glib::Error>,
    path: &std::path::Path,
) -> Result<(i32, i32), String> {
    let surface = result.map_err(|e| format!("WebKit could not snapshot the page: {e}"))?;
    let image = gtk::cairo::ImageSurface::try_from(surface)
        .map_err(|_| "the snapshot came back in a form we cannot write as PNG".to_string())?;
    let (width, height) = (image.width(), image.height());

    let mut file = std::fs::File::create(path)
        .map_err(|e| format!("could not create {}: {e}", path.display()))?;
    image
        .write_to_png(&mut file)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok((width, height))
}

/// Somewhere to put a file the caller did not name.
///
/// Shared with `page source`, so the two agree: per-boot, user-private, and
/// cleaned up by the system rather than accumulating in the user's home.
pub fn scratch_file(
    requested: Option<String>,
    prefix: &str,
    extension: &str,
) -> Result<PathBuf> {
    if let Some(p) = requested.filter(|p| !p.trim().is_empty()) {
        return Ok(PathBuf::from(shellexpand(&p)));
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("oma-browse");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not make {}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Ok(dir.join(format!("{prefix}-{stamp}.{extension}")))
}

/// Where the PNG goes when the caller does not say.
///
/// Under `$XDG_RUNTIME_DIR` beside the port file, so shots are per-boot,
/// user-private, and cleaned up by the system rather than accumulating in the
/// user's home.
fn resolve_path(requested: Option<String>, configured: &str) -> Result<PathBuf> {
    if let Some(p) = requested.filter(|p| !p.trim().is_empty()) {
        return Ok(PathBuf::from(shellexpand(&p)));
    }
    let dir = if configured.trim().is_empty() {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("oma-browse")
    } else {
        PathBuf::from(shellexpand(configured.trim()))
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Ok(dir.join(format!("shot-{stamp}.png")))
}

/// `~` only. Anything more is the shell's job, and this is also called over
/// HTTP where there is no shell to have done it.
fn shellexpand(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest).display().to_string(),
            None => path.to_string(),
        },
        None => path.to_string(),
    }
}
