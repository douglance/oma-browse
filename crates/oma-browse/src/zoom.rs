//! How big each site is, remembered.
//!
//! Zoom lives on the webview, which is to say per tab, which is to say it is
//! forgotten the moment the tab closes and never applies to the next one. That
//! is where this browser left it, with a note in [`crate::tabs::zoom`] saying
//! Chrome remembers zoom per origin and that doing the same needed somewhere to
//! persist a map. This is that map.
//!
//! It matters more than it sounds. A site whose body text is too small for your
//! screen is too small on every visit, and a browser that makes you fix it again
//! every time is one you stop using for that site. Chrome, Firefox and Safari
//! all key it to the origin and all reapply it before first paint.
//!
//! Deliberately the same shape as [`crate::bookmarks`]: a flat file, one record
//! per line, rewritten whole. There are as many entries here as there are sites
//! you have deliberately resized, which is tens.
//!
//! Keyed by origin -- scheme, host and port -- rather than by host, because
//! `http://localhost:3000` and `http://localhost:8080` are two different things
//! to whoever is looking at them, and because a scheme downgrade should not
//! inherit a setting made over TLS.

use std::collections::HashMap;
use std::path::PathBuf;

/// The level a site with no entry gets: WebKit's own, unless the config says
/// otherwise.
pub const DEFAULT: f64 = 1.0;

#[derive(Default)]
pub struct Zooms {
    levels: HashMap<String, f64>,
    /// Where to write. `None` for a detached store that never touches disk,
    /// which is what the tests use -- otherwise a unit test would rewrite the
    /// running user's real file.
    file: Option<PathBuf>,
}

impl Zooms {
    pub fn load() -> Self {
        let file = path();
        let mut out = Zooms { file: Some(file.clone()), ..Zooms::default() };
        let Ok(raw) = std::fs::read_to_string(&file) else { return out };
        for line in raw.lines() {
            let mut parts = line.splitn(2, '\t');
            let (Some(level), Some(origin)) = (parts.next(), parts.next()) else { continue };
            let Ok(level) = level.parse::<f64>() else { continue };
            // A file edited by hand, or written by an older version with a
            // different ladder. A zero or a negative would make the page vanish.
            if level > 0.0 && origin.starts_with("http") {
                out.levels.insert(origin.to_string(), level);
            }
        }
        out
    }

    /// What this URL should open at, or `None` for the default.
    pub fn level_for(&self, url: &str) -> Option<f64> {
        self.levels.get(&origin_of(url)?).copied()
    }

    /// Remember a level for the site this URL belongs to.
    ///
    /// Setting a site back to the default *forgets* it rather than storing a 1.0,
    /// which is what Chrome does and what makes the list stay short: the file is
    /// the sites you have opinions about, and undoing the opinion should remove
    /// the row rather than pin the default in place.
    pub fn remember(&mut self, url: &str, level: f64) -> bool {
        let Some(origin) = origin_of(url) else { return false };
        let changed = if (level - DEFAULT).abs() < 1e-6 {
            self.levels.remove(&origin).is_some()
        } else {
            self.levels.insert(origin, level) != Some(level)
        };
        if changed {
            self.save();
        }
        changed
    }

    /// Every site with an opinion, for `page zoom --list` and the tests.
    pub fn entries(&self) -> Vec<(String, f64)> {
        let mut out: Vec<(String, f64)> =
            self.levels.iter().map(|(k, v)| (k.clone(), *v)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn save(&self) {
        let Some(file) = self.file.as_ref() else { return };
        let body: String =
            self.entries().iter().map(|(origin, level)| format!("{level}\t{origin}\n")).collect();
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(file, body) {
            tracing::warn!(error = %e, path = %file.display(), "could not save zoom levels");
        }
    }
}

/// Scheme, host and port, which is what a zoom level belongs to.
///
/// `None` for anything that is not a web page: the start page, a crash page and
/// `about:blank` are the browser's own furniture, and remembering that you
/// zoomed one of them would apply it to all of them.
fn origin_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?;
    Some(match parsed.port() {
        Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
        None => format!("{}://{host}", parsed.scheme()),
    })
}

fn path() -> PathBuf {
    crate::history::state_dir().join("zoom")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_applies_to_every_page_on_the_site() {
        let mut z = Zooms::default();
        assert!(z.remember("https://news.example/story/1", 1.25));
        assert_eq!(z.level_for("https://news.example/story/2"), Some(1.25));
        assert_eq!(z.level_for("https://news.example/"), Some(1.25));
    }

    #[test]
    fn other_sites_are_left_alone() {
        let mut z = Zooms::default();
        z.remember("https://news.example/", 1.25);
        assert_eq!(z.level_for("https://other.example/"), None);
    }

    /// Two dev servers on one host are two different things to whoever is
    /// looking at them.
    #[test]
    fn a_port_is_part_of_the_site() {
        let mut z = Zooms::default();
        z.remember("http://localhost:3000/", 1.5);
        assert_eq!(z.level_for("http://localhost:3000/app"), Some(1.5));
        assert_eq!(z.level_for("http://localhost:8080/"), None);
    }

    /// A setting made over TLS should not be inherited by a plain-text page of
    /// the same name.
    #[test]
    fn a_scheme_is_part_of_the_site() {
        let mut z = Zooms::default();
        z.remember("https://a.example/", 1.5);
        assert_eq!(z.level_for("http://a.example/"), None);
    }

    #[test]
    fn setting_a_site_back_to_normal_forgets_it() {
        let mut z = Zooms::default();
        z.remember("https://a.example/", 1.5);
        assert!(z.remember("https://a.example/", 1.0), "going back to 1.0 is a change");
        assert_eq!(z.level_for("https://a.example/"), None);
        assert!(z.entries().is_empty(), "and it leaves no row behind");
    }

    #[test]
    fn setting_the_same_level_again_writes_nothing() {
        let mut z = Zooms::default();
        assert!(z.remember("https://a.example/", 1.5));
        assert!(!z.remember("https://a.example/", 1.5), "no change, so no save");
    }

    /// The browser's own pages are furniture, not sites.
    #[test]
    fn the_chrome_is_not_a_site() {
        let mut z = Zooms::default();
        assert!(!z.remember("oma-chrome://localhost/start", 1.5));
        assert!(!z.remember("about:blank", 1.5));
        assert_eq!(z.level_for("about:blank"), None);
        assert!(z.entries().is_empty());
    }

    #[test]
    fn a_file_url_is_not_a_site_either() {
        let mut z = Zooms::default();
        assert!(!z.remember("file:///home/me/page.html", 1.5));
    }
}
