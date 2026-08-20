//! Pages worth keeping.
//!
//! Deliberately the same shape as [`crate::history`]: a flat list, one file, no
//! index. The difference is intent, not structure -- history is what happened,
//! bookmarks are what you chose -- so they stay separate types rather than one
//! store with a flag, and the palette ranks a bookmark above a merely-visited
//! page because you said it mattered.
//!
//! No folders. A fuzzy-searchable flat list with tags in the title is what
//! anyone actually uses a bookmark bar for, and folders would be a tree to
//! navigate with a keyboard for no gain.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Bookmark {
    pub url: String,
    pub title: String,
    /// Unix seconds, so the newest sort first.
    pub added: u64,
}

#[derive(Default)]
pub struct Bookmarks {
    entries: Vec<Bookmark>,
    /// Where to write. `None` for a detached store that never touches disk,
    /// which is what the tests use -- otherwise adding a bookmark in a unit
    /// test would write into the running user's real bookmarks file.
    file: Option<PathBuf>,
}

impl Bookmarks {
    pub fn load() -> Self {
        let file = path();
        let mut out = Bookmarks { file: Some(file.clone()), ..Bookmarks::default() };
        let Ok(raw) = std::fs::read_to_string(&file) else { return out };
        for line in raw.lines() {
            let mut parts = line.splitn(3, '\t');
            let (Some(added), Some(url)) = (parts.next(), parts.next()) else { continue };
            let Ok(added) = added.parse() else { continue };
            out.entries.push(Bookmark {
                url: url.to_string(),
                title: parts.next().unwrap_or("").to_string(),
                added,
            });
        }
        out
    }

    /// Add, or update the title of one already kept. Returns whether it is new.
    pub fn add(&mut self, url: &str, title: &str, now: u64) -> bool {
        if url.is_empty() {
            return false;
        }
        if let Some(existing) = self.entries.iter_mut().find(|b| b.url == url) {
            if !title.is_empty() {
                existing.title = title.to_string();
            }
            self.save();
            return false;
        }
        self.entries
            .insert(0, Bookmark { url: url.to_string(), title: title.to_string(), added: now });
        self.save();
        true
    }

    /// Remove by URL. Returns whether anything went.
    pub fn remove(&mut self, url: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|b| b.url != url);
        let removed = self.entries.len() != before;
        if removed {
            self.save();
        }
        removed
    }

    pub fn entries(&self) -> &[Bookmark] {
        &self.entries
    }

    /// Written on every change rather than on a timer: the list is small, and a
    /// bookmark that vanishes because the browser was killed before a flush is
    /// worse than a few milliseconds of I/O.
    fn save(&self) {
        let Some(file) = self.file.as_ref() else { return };
        let body: String = self
            .entries
            .iter()
            .map(|b| format!("{}\t{}\t{}\n", b.added, b.url, b.title.replace('\t', " ")))
            .collect();
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(file, body) {
            tracing::warn!(error = %e, path = %file.display(), "could not save bookmarks");
        }
    }
}

fn path() -> PathBuf {
    crate::history::state_dir().join("bookmarks")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adding_twice_updates_rather_than_duplicates() {
        let mut b = Bookmarks::default();
        assert!(b.add("https://a.example", "First", 1));
        assert!(!b.add("https://a.example", "Renamed", 2));
        assert_eq!(b.entries().len(), 1);
        assert_eq!(b.entries()[0].title, "Renamed");
        // The original add time survives a re-add; it is when *you* kept it.
        assert_eq!(b.entries()[0].added, 1);
    }

    #[test]
    fn removing_reports_whether_it_was_there() {
        let mut b = Bookmarks::default();
        b.add("https://a.example", "A", 1);
        assert!(b.remove("https://a.example"));
        assert!(!b.remove("https://a.example"));
        assert!(b.entries().is_empty());
    }

    #[test]
    fn an_empty_url_is_not_a_bookmark() {
        let mut b = Bookmarks::default();
        assert!(!b.add("", "nothing", 1));
        assert!(b.entries().is_empty());
    }
}
