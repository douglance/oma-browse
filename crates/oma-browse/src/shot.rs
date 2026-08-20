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
use serde::Serialize;
use tauri::Manager as _;

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
    // What the page is *seen* against. `--opaque` flattens onto this rather
    // than onto white, because white is not what is behind the page on any
    // Omarchy theme, and the whole reason to ask for an opaque shot is to see
    // the thing the way the user sees it.
    let ground = state.theme.read().await.css.tint;
    let label =
        state.tabs.read().await.active_label().ok_or_else(|| anyhow!("there is no active tab"))?;
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
        // Always transparent, and `--opaque` flattens afterwards in
        // `write_png`. `SnapshotOptions::NONE` composites onto the *webview's*
        // background colour, and `window.rs` has already set that to fully
        // transparent so the page can show the desktop through it -- so the
        // flag is a no-op here and the two modes came back byte-identical.
        let options = SnapshotOptions::TRANSPARENT_BACKGROUND;
        let ground = if transparent { None } else { Some(ground) };

        // The callback lands back on the GTK main thread, and a cairo surface is
        // not `Send`, so the PNG is written here and only the dimensions travel.
        platform.inner().snapshot(region, options, None::<&gio::Cancellable>, move |result| {
            let _ = tx.send(write_png(result, &target, ground));
        });
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
    ground: Option<oma_theme::Rgb>,
) -> Result<(i32, i32), String> {
    let surface = result.map_err(|e| format!("WebKit could not snapshot the page: {e}"))?;
    let image = gtk::cairo::ImageSurface::try_from(surface)
        .map_err(|_| "the snapshot came back in a form we cannot write as PNG".to_string())?;
    let (width, height) = (image.width(), image.height());
    let image = match ground {
        Some(rgb) => flatten(&image, rgb)?,
        None => image,
    };

    let mut file = std::fs::File::create(path)
        .map_err(|e| format!("could not create {}: {e}", path.display()))?;
    image
        .write_to_png(&mut file)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok((width, height))
}

/// Paint the snapshot over a solid colour.
///
/// A translucent PNG viewed on a white background looks fine and hides exactly
/// the failure this browser is most likely to have -- dark text left behind on
/// a surface that has gone see-through. Flattening onto the theme tint puts the
/// image back into the colour world the page actually renders in.
fn flatten(
    image: &gtk::cairo::ImageSurface,
    ground: oma_theme::Rgb,
) -> Result<gtk::cairo::ImageSurface, String> {
    use gtk::cairo::{Context, Format, ImageSurface};

    let (width, height) = (image.width(), image.height());
    let out = ImageSurface::create(Format::ARgb32, width, height)
        .map_err(|e| format!("could not allocate the backing surface: {e}"))?;
    {
        let cr = Context::new(&out).map_err(|e| format!("could not paint the snapshot: {e}"))?;
        let f = |c: u8| c as f64 / 255.0;
        cr.set_source_rgb(f(ground.r), f(ground.g), f(ground.b));
        cr.paint().map_err(|e| format!("could not fill the background: {e}"))?;
        cr.set_source_surface(image, 0.0, 0.0)
            .map_err(|e| format!("could not place the snapshot: {e}"))?;
        cr.paint().map_err(|e| format!("could not composite the snapshot: {e}"))?;
    }
    // The `Context` above borrows `out`; it is gone by here, so this cannot fail.
    Ok(out)
}

/// Somewhere to put a file the caller did not name.
///
/// Shared with `page source`, so the two agree: per-boot, user-private, and
/// cleaned up by the system rather than accumulating in the user's home.
pub fn scratch_file(requested: Option<String>, prefix: &str, extension: &str) -> Result<PathBuf> {
    if let Some(p) = requested.filter(|p| !p.trim().is_empty()) {
        return Ok(PathBuf::from(shellexpand(&p)));
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("oma-browse");
    std::fs::create_dir_all(&dir).with_context(|| format!("could not make {}", dir.display()))?;
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
pub fn shellexpand(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest).display().to_string(),
            None => path.to_string(),
        },
        None => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::cairo::{Context, Format, ImageSurface};

    /// A half-transparent white pixel over a known ground.
    fn blended(ground: oma_theme::Rgb) -> [u8; 4] {
        let surface = ImageSurface::create(Format::ARgb32, 1, 1).unwrap();
        {
            let cr = Context::new(&surface).unwrap();
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.5);
            cr.paint().unwrap();
        }
        let mut out = flatten(&surface, ground).unwrap();
        let data = out.data().unwrap();
        [data[0], data[1], data[2], data[3]]
    }

    #[test]
    fn opaque_flattens_onto_the_theme_tint() {
        // The bug this exists for: `--opaque` came back byte-identical to the
        // transparent shot, because WebKit was compositing onto a webview whose
        // own background we had already made transparent. Flattening happens
        // here now, so the alpha channel must come back solid.
        let out = blended(oma_theme::Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(out[3], 255, "an opaque shot must have no transparency left");
    }

    #[test]
    fn the_ground_shows_through_what_the_page_left_open() {
        // Half-transparent white over black lands mid-grey; over white it stays
        // white. If the ground were ignored -- the old behaviour -- these two
        // would be identical.
        let over_black = blended(oma_theme::Rgb { r: 0, g: 0, b: 0 });
        let over_white = blended(oma_theme::Rgb { r: 255, g: 255, b: 255 });
        assert_ne!(over_black, over_white);
        // Premultiplied ARGB32, so with alpha at 255 the channels are literal.
        assert!((110..=145).contains(&over_black[2]), "got {over_black:?}");
        assert!(over_white[2] >= 250, "got {over_white:?}");
    }
}
