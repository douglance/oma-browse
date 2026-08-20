//! Favicons, taken from WebKit rather than fetched ourselves.
//!
//! The obvious implementation -- ask the page for its `<link rel=icon>`, or just
//! GET `/favicon.ico` -- is the wrong one twice over. It re-fetches an icon
//! WebKit has already downloaded and cached, and it gets the answer wrong on
//! every site that declares several sizes, an SVG, or a manifest icon, because
//! then *we* would have to implement the selection rules. WebKit implements them
//! already and hands over the winner as a cairo surface.
//!
//! The one catch is that WebKit only tracks favicons at all once the favicon
//! database has a directory, so [`watch`] sets one before hooking anything.

use std::sync::Arc;

use anyhow::{Context, Result};
use tauri::webview::Webview;

use crate::state::AppState;

/// Hook a content webview so its favicon reaches the tab model.
///
/// Called for every tab, including the first, because the signal is per webview
/// and there is nowhere higher to bind it.
#[cfg(target_os = "linux")]
pub fn watch<R: tauri::Runtime>(view: &Webview<R>, state: Arc<AppState>) -> Result<()> {
    let label = view.label().to_string();

    view.with_webview(move |platform| {
        use webkit2gtk::WebViewExt;

        let webview = platform.inner();
        if let Some(context) = webview.context() {
            enable_database(&context, state.incognito());
        }

        // An icon already in the database is attached to the webview before any
        // notification fires, so the first tab of a site visited before would
        // otherwise stay blank until something made it change.
        publish(&state, &label, webview.favicon());

        let state = state.clone();
        let label = label.clone();
        webview.connect_favicon_notify(move |webview| {
            publish(&state, &label, webview.favicon());
        });
    })
    .context("could not reach the webview to watch its favicon")
}

#[cfg(not(target_os = "linux"))]
pub fn watch<R: tauri::Runtime>(_view: &Webview<R>, _state: Arc<AppState>) -> Result<()> {
    Ok(())
}

/// Give the favicon database somewhere to live, once per web context.
///
/// Without this `favicon()` is always `None`: the database is disabled until it
/// has a directory, and WebKit does not download icons it has nowhere to put.
#[cfg(target_os = "linux")]
fn enable_database(context: &webkit2gtk::WebContext, incognito: bool) {
    use webkit2gtk::WebContextExt;

    // Already pointed somewhere: this runs per webview, and every tab in the
    // window shares one context.
    if context.favicon_database_directory().is_some() {
        return;
    }

    // An incognito window's icons go where its history would have gone, which
    // is to say nowhere that survives the session: `XDG_RUNTIME_DIR` is
    // per-boot and user-private, and the database is a record of where the
    // browser has been just as much as `history.jsonl` is.
    let root = if incognito {
        std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME").map(std::path::PathBuf::from).or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache"))
        })
    };
    let Some(dir) = root.map(|r| r.join("oma-browse").join("favicons")) else {
        tracing::warn!("no cache directory; favicons are off");
        return;
    };

    context.set_favicon_database_directory(dir.to_str());
    tracing::debug!(dir = %dir.display(), incognito, "favicon database enabled");
}

/// Encode a favicon and hand it to the tab model.
///
/// Runs on the GTK main thread -- a cairo surface is not `Send`, so the encoding
/// happens here and only a `data:` URL crosses back to the runtime.
#[cfg(target_os = "linux")]
fn publish(state: &Arc<AppState>, label: &str, surface: Option<gtk::cairo::Surface>) {
    let size = state.config.tabs.favicon_size.max(1);
    let Some(icon) = surface.and_then(|s| encode(&s, size)) else { return };

    let state = state.clone();
    let label = label.to_string();
    state.runtime().spawn(async move {
        if state.tabs.write().await.update_icon(&label, icon) {
            state.notify_tabs();
        }
    });
}

/// A cairo surface as a PNG `data:` URL, scaled down if the site sent a big one.
///
/// Via `GdkPixbuf` rather than `write_to_png` because the surface is
/// premultiplied ARGB32 and the pixbuf conversion is the thing that knows how to
/// undo that -- and because scaling is then one call rather than a second
/// surface and a cairo context.
#[cfg(target_os = "linux")]
fn encode(surface: &gtk::cairo::Surface, max_px: i32) -> Option<String> {
    use gtk::gdk_pixbuf::InterpType;

    let image = gtk::cairo::ImageSurface::try_from(surface.clone()).ok()?;
    let (width, height) = (image.width(), image.height());
    if width <= 0 || height <= 0 {
        return None;
    }

    let pixbuf = gtk::gdk::pixbuf_get_from_surface(surface, 0, 0, width, height)?;
    let pixbuf = if width > max_px || height > max_px {
        pixbuf.scale_simple(max_px, max_px, InterpType::Bilinear)?
    } else {
        pixbuf
    };

    let png = pixbuf.save_to_bufferv("png", &[]).ok()?;
    Some(format!("data:image/png;base64,{}", base64(&png)))
}

/// Standard base64, no line breaks.
///
/// Hand-rolled to keep the dependency list honest: this is the only base64 in
/// the binary, it encodes a few hundred bytes at a time, and a crate for it
/// would be more lines of `Cargo.toml` churn than of code.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_matches_the_rfc_vectors() {
        // RFC 4648 section 10, which is also every padding case there is.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_covers_the_whole_alphabet() {
        // Every byte value round-trips through the encoder's arithmetic, which
        // is where an off-by-one in the shift table would show up.
        let all: Vec<u8> = (0..=255u8).collect();
        let encoded = base64(&all);
        assert_eq!(encoded.len(), 344, "256 bytes is 344 base64 characters with padding");
        assert!(encoded.ends_with('='), "256 is not a multiple of three");
        assert!(encoded.chars().all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c)));
    }
}
