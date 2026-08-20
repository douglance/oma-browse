//! Where the browser has been.
//!
//! Deliberately small: an append-only list of visited pages, deduplicated by
//! URL, kept in memory and flushed to one file. There is no index and no
//! database because there does not need to be one -- the palette scores the
//! whole list on every keystroke, and a few thousand entries is nothing.
//!
//! Incognito windows record nothing at all; that is checked at the call site in
//! [`crate::window::instrument`] so the store cannot be handed something it
//! should not keep.

use std::collections::HashMap;
use std::path::PathBuf;

/// Beyond this the oldest entries are dropped, unless the config says
/// otherwise. Generous enough to be useful and small enough that scoring it
/// stays free.
pub const CAP: usize = 5_000;

/// One page, however many times it has been seen.
#[derive(Debug, Clone)]
pub struct Visit {
    pub url: String,
    pub title: String,
    pub visits: u32,
    /// Unix seconds, so recency can beat similarity when scores are close.
    pub last: u64,
}

pub struct History {
    /// See [`CAP`]; overridden by `history.limit` in the config file.
    cap: usize,
    /// Most recently visited first.
    entries: Vec<Visit>,
    index: HashMap<String, usize>,
    dirty: bool,
    /// Where to write. `None` for a detached store that never touches disk --
    /// a unit test must not be able to delete the real history file.
    file: Option<PathBuf>,
}

impl Default for History {
    /// Detached from disk, and holding the standard cap.
    ///
    /// Hand-written rather than derived: a derived `cap` is `0`, which is not
    /// "unlimited" but "drop every entry on the way in".
    fn default() -> Self {
        Self { cap: CAP, entries: Vec::new(), index: HashMap::new(), dirty: false, file: None }
    }
}

impl History {
    /// Read whatever was saved last time, keeping `cap` entries. A missing or
    /// corrupt file is an empty history, never an error: losing history must
    /// not stop the browser.
    pub fn load_with(cap: usize) -> Self {
        let file = path();
        // A zero cap would drop every entry on the way in, which is what
        // `history.enabled = false` is for and not what a limit means.
        let cap = cap.max(1);
        let mut history = History { cap, file: Some(file.clone()), ..History::default() };
        let Ok(raw) = std::fs::read_to_string(&file) else { return history };
        for line in raw.lines() {
            // `visits \t last \t url \t title`. Tabs, because a URL cannot
            // contain one and a title with a tab in it is not worth a parser.
            let mut parts = line.splitn(4, '\t');
            let (Some(visits), Some(last), Some(url)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let title = parts.next().unwrap_or("").to_string();
            let (Ok(visits), Ok(last)) = (visits.parse(), last.parse()) else { continue };
            history.push(Visit { url: url.to_string(), title, visits, last });
        }
        history.reindex();
        history
    }

    fn push(&mut self, visit: Visit) {
        if self.entries.len() < self.cap {
            self.entries.push(visit);
        }
    }

    fn reindex(&mut self) {
        self.index = self.entries.iter().enumerate().map(|(i, v)| (v.url.clone(), i)).collect();
    }

    /// Note a visit, moving the page to the front.
    pub fn record(&mut self, url: &str, now: u64) {
        if !worth_keeping(url) {
            return;
        }
        self.dirty = true;
        match self.index.get(url).copied() {
            Some(at) => {
                let mut visit = self.entries.remove(at);
                visit.visits = visit.visits.saturating_add(1);
                visit.last = now;
                self.entries.insert(0, visit);
            }
            None => {
                self.entries.insert(
                    0,
                    Visit { url: url.to_string(), title: String::new(), visits: 1, last: now },
                );
                self.entries.truncate(self.cap);
            }
        }
        self.reindex();
    }

    /// Titles arrive after the page load that created the entry, so they are
    /// filled in separately rather than being part of `record`.
    pub fn set_title(&mut self, url: &str, title: &str) {
        if title.is_empty() {
            return;
        }
        if let Some(at) = self.index.get(url).copied()
            && self.entries[at].title != title
        {
            self.entries[at].title = title.to_string();
            self.dirty = true;
        }
    }

    pub fn entries(&self) -> &[Visit] {
        &self.entries
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.index.clear();
        self.dirty = true;
        if let Some(file) = self.file.as_ref() {
            let _ = std::fs::remove_file(file);
        }
    }

    /// Write the file if anything changed. Cheap enough to call after every
    /// navigation: a few thousand short lines.
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let body: String = self
            .entries
            .iter()
            .map(|v| {
                format!("{}\t{}\t{}\t{}\n", v.visits, v.last, v.url, v.title.replace('\t', " "))
            })
            .collect();
        let Some(file) = self.file.as_ref() else { return };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(file, body) {
            tracing::warn!(error = %e, path = %file.display(), "could not save history");
        }
    }
}

/// Pages not worth remembering: the blank page, and our own chrome. Recording
/// `/start` would put the new-tab page at the top of every search.
fn worth_keeping(url: &str) -> bool {
    if url.is_empty() || url == "about:blank" {
        return false;
    }
    // Our own chrome has a scheme of its own, which makes this exact: the
    // start page is `oma-chrome://localhost/start` and nothing else is.
    !url.starts_with(crate::window::CHROME_SCHEME)
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where this browser keeps what it remembers. Shared with
/// [`crate::bookmarks`], so the two files sit together.
pub fn state_dir() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        });
    state.join("oma-browse")
}

fn path() -> PathBuf {
    state_dir().join("history")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisiting_moves_to_the_front_and_counts() {
        let mut h = History::default();
        h.record("https://a.example", 10);
        h.record("https://b.example", 20);
        h.record("https://a.example", 30);

        assert_eq!(h.entries()[0].url, "https://a.example");
        assert_eq!(h.entries()[0].visits, 2);
        assert_eq!(h.entries()[0].last, 30);
        assert_eq!(h.entries().len(), 2);
    }

    #[test]
    fn our_own_pages_are_not_history() {
        let mut h = History::default();
        h.record("about:blank", 1);
        h.record("oma-chrome://localhost/start", 1);
        h.record("", 1);
        assert!(h.entries().is_empty());

        // And the other half of the bargain: now that the chrome has a scheme of
        // its own, a loopback page is unambiguously the user's own work.
        h.record("http://127.0.0.1:3000/", 2);
        assert_eq!(h.entries().len(), 1);
    }

    #[test]
    fn titles_attach_to_the_page_they_belong_to() {
        let mut h = History::default();
        h.record("https://a.example", 1);
        h.record("https://b.example", 2);
        h.set_title("https://a.example", "Alpha");
        assert_eq!(
            h.entries().iter().find(|v| v.url == "https://a.example").unwrap().title,
            "Alpha"
        );
        assert_eq!(h.entries()[0].title, "");
    }

    #[test]
    fn the_cap_holds() {
        let mut h = History::default();
        for i in 0..(CAP + 50) {
            h.record(&format!("https://{i}.example"), i as u64);
        }
        assert_eq!(h.entries().len(), CAP);
        // The newest survived, the oldest did not.
        assert_eq!(h.entries()[0].url, format!("https://{}.example", CAP + 49));
    }
}
