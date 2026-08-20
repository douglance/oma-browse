//! How bright is the desktop behind the window?
//!
//! A translucent page is only as readable as whatever shows through it. Ghostty
//! gets away with a fixed `background-opacity` because its foreground is a
//! handful of high-contrast ANSI colours on a black veil; a web page is mostly
//! mid-tone text on surfaces the site chose, and over a bright wallpaper it
//! washes out completely.
//!
//! So the veil is not a constant. Measure the wallpaper once, and open the
//! window up as far as it can go while still holding text — which on a dark
//! wallpaper is exactly the Ghostty setting, and on a bright one is more.

use crate::paths;

/// A high percentile rather than the mean, because the mean is a lie for
/// readability: a wallpaper that is mostly dark with one bright quadrant still
/// destroys the text sitting over that quadrant. On the stock `winding-road`
/// background the mean is 0.26 while the sunset it is named for reaches 0.69 --
/// and it is the sunset the text has to survive.
const BRIGHT_PERCENTILE: f64 = 0.95;

/// Sampled down before measuring — this runs on every theme change, and the
/// answer does not get better with more pixels.
const SAMPLE: u32 = 64;

/// Points averaged per sample cell. Enough that sparse bright detail survives
/// into the cell's value instead of falling between the samples.
const SUB: u32 = 8;

/// Where a measurement is remembered, keyed by the file it came from.
///
/// Decoding is the expensive part and it cannot be avoided: `image` 0.25 does
/// not expose JPEG's DCT scaling, so a 24-megapixel wallpaper is decoded in
/// full to answer a question about 4,096 pixels. Doing it once per wallpaper
/// rather than once per launch is the next best thing.
fn cache_path(key: &str) -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| paths::home().join(".cache"));
    // One file per wallpaper rather than one file overall: switching between two
    // themes and back is a normal thing to do, and a single slot would make each
    // switch pay the full decode again.
    let mut hasher = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(key, &mut hasher);
    let digest = std::hash::Hasher::finish(&hasher);
    base.join("oma-browse/backdrop").join(format!("{digest:016x}"))
}

/// Identity of a wallpaper, cheap enough to check on every startup: a rename
/// alone is not enough to invalidate, but a rewrite in place changes both the
/// length and the modification time.
fn stamp(path: &std::path::Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{} {} {}", path.display(), meta.len(), mtime))
}

fn cached(key: &str) -> Option<f64> {
    let raw = std::fs::read_to_string(cache_path(key)).ok()?;
    // The key is stored as well as hashed, so a hash collision reads as a miss
    // rather than as another wallpaper's brightness.
    let (stored_key, value) = raw.rsplit_once('\n')?;
    if stored_key != key {
        return None;
    }
    value.trim().parse().ok()
}

fn remember(key: &str, value: f64) {
    let path = cache_path(key);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{key}\n{value}"));
}

/// The luminance a page has to contend with, or `None` when the background is
/// something we cannot measure (a video, a missing link, an unknown codec).
pub fn backdrop_luminance() -> Option<f64> {
    let path = std::fs::canonicalize(paths::background_link()).ok()?;
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif" | "tif" | "tiff") {
        tracing::debug!(path = %path.display(), "background is not a still image; veil stays fixed");
        return None;
    }

    let key = stamp(&path);
    if let Some(hit) = key.as_deref().and_then(cached) {
        return Some(hit);
    }

    let image = image::ImageReader::open(&path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .inspect_err(|e| tracing::debug!(error = %e, "could not decode the background"))
        .ok()?;

    // Average a small block per cell rather than resampling the whole image.
    // `resize` filters every source pixel -- 4.5 seconds on a 6000x4000
    // wallpaper in a debug build, long enough that the window had not opened yet
    // -- to build a 64x64 thumbnail we then reduce to a single percentile.
    //
    // One point per cell is not enough: a wallpaper of sparse bright dots on
    // black reads as pure black, because the grid lands between the dots. So
    // each cell averages a SUB x SUB block, which is a box filter over a
    // fraction of the pixels and lands within 0.03 of the filtered resize on
    // every stock wallpaper -- close enough that no theme's veil moves.
    let (w, h) = image::GenericImageView::dimensions(&image);
    if w == 0 || h == 0 {
        return None;
    }
    let cells_x = SAMPLE.min(w);
    let cells_y = SAMPLE.min(h);
    let mut lums: Vec<f64> = Vec::with_capacity((cells_x * cells_y) as usize);
    for cy in 0..cells_y {
        for cx in 0..cells_x {
            let mut sum = 0.0;
            for jy in 0..SUB {
                for jx in 0..SUB {
                    let x =
                        ((cx * SUB + jx) as u64 * (w - 1) as u64 / (cells_x * SUB) as u64) as u32;
                    let y =
                        ((cy * SUB + jy) as u64 * (h - 1) as u64 / (cells_y * SUB) as u64) as u32;
                    let p = image::GenericImageView::get_pixel(&image, x, y);
                    sum += crate::Rgb::new(p[0], p[1], p[2]).luminance();
                }
            }
            lums.push(sum / (SUB * SUB) as f64);
        }
    }
    if lums.is_empty() {
        return None;
    }
    lums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((lums.len() - 1) as f64 * BRIGHT_PERCENTILE).round() as usize;
    let value = lums[idx];
    if let Some(key) = key {
        remember(&key, value);
    }
    Some(value)
}
