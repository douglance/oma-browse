//! What the browser has saved to disk.
//!
//! Same shape as [`crate::history`] and [`crate::bookmarks`]: a flat list, one
//! TSV file, newest first. It is persisted rather than kept for the session
//! because a downloads list that empties on restart is a list you cannot rely
//! on, and "where did that file go" is the question it exists to answer.
//!
//! Behind a `std::sync::Mutex` rather than tokio's: WebKit's download callbacks
//! arrive on the GTK main thread, outside any runtime, and an async lock cannot
//! be taken there.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Download {
    pub url: String,
    pub path: PathBuf,
    /// Unix seconds.
    pub started: u64,
    /// `None` while the transfer is still running.
    pub ok: Option<bool>,
    /// How far along, 0.0 to 1.0. WebKit's own estimate, which is a guess
    /// wherever the server sent no `Content-Length`.
    ///
    /// In memory only, and deliberately: a progress figure is meaningless the
    /// moment the process that was making it exits, and writing it to the
    /// downloads file would mean reading back "47%" for a transfer nothing is
    /// working on.
    pub progress: f64,
    /// Bytes written so far.
    pub bytes: u64,
}

impl Download {
    /// How far along, as a whole percent, or `None` for a transfer that has
    /// ended -- where the honest answer is its outcome, not a number.
    pub fn percent(&self) -> Option<u8> {
        self.ok.is_none().then(|| (self.progress.clamp(0.0, 1.0) * 100.0).round() as u8)
    }

    /// The bit a human recognises: the file's own name.
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.url.clone())
    }

    pub fn state(&self) -> &'static str {
        match self.ok {
            None => "running",
            Some(true) => "done",
            Some(false) => "failed",
        }
    }
}

/// How many to remember. A downloads list is a recent-work list, not an archive.
const CAP: usize = 200;

#[derive(Default)]
pub struct Downloads {
    entries: Vec<Download>,
    /// `None` for a detached store that never touches disk, which is what the
    /// tests use.
    file: Option<PathBuf>,
}

impl Downloads {
    pub fn load() -> Self {
        let file = path();
        let mut out = Downloads { file: Some(file.clone()), ..Downloads::default() };
        let Ok(raw) = std::fs::read_to_string(&file) else { return out };
        for line in raw.lines() {
            // `started \t state \t path \t url`. Tabs, because a path can hold
            // very nearly anything else and a URL cannot hold a tab.
            let mut parts = line.splitn(4, '\t');
            let (Some(started), Some(state), Some(path), Some(url)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let Ok(started) = started.parse() else { continue };
            out.entries.push(Download {
                url: url.to_string(),
                path: PathBuf::from(path),
                started,
                // A transfer that was still running when the browser exited did
                // not finish, and there is nothing left to finish it.
                ok: match state {
                    "done" => Some(true),
                    _ => Some(false),
                },
                progress: 0.0,
                bytes: 0,
            });
        }
        out
    }

    /// Record a started transfer, newest first.
    pub fn start(&mut self, url: &str, path: &Path, now: u64) {
        self.entries.insert(
            0,
            Download {
                url: url.to_string(),
                path: path.to_path_buf(),
                started: now,
                ok: None,
                progress: 0.0,
                bytes: 0,
            },
        );
        self.entries.truncate(CAP);
        self.save();
    }

    /// Close out the running transfer headed for this path.
    ///
    /// Matched on the destination rather than the URL because the destination is
    /// the one identifier *we* chose: it is unique by construction (see
    /// [`unique`]), whereas the same URL can be downloading twice at once, and
    /// WebKit does not always have the request to hand.
    ///
    /// `url` fills in a URL that was not yet known when the transfer started.
    pub fn finish(&mut self, path: &Path, url: Option<&str>, ok: bool) -> Option<Download> {
        let at = self.entries.iter().position(|d| d.path == path && d.ok.is_none())?;
        let entry = &mut self.entries[at];
        entry.ok = Some(ok);
        if entry.url.is_empty()
            && let Some(url) = url
        {
            entry.url = url.to_string();
        }
        let done = entry.clone();
        self.save();
        Some(done)
    }

    /// How far along the transfer headed for this path is.
    ///
    /// Not saved: see [`Download::progress`]. That also makes this cheap enough
    /// to call on every chunk, which is what WebKit's signal does.
    pub fn progress(&mut self, path: &Path, progress: f64, bytes: u64) {
        if let Some(entry) = self.entries.iter_mut().find(|d| d.path == path && d.ok.is_none()) {
            entry.progress = progress;
            entry.bytes = bytes;
        }
    }

    /// Move a running entry to a different destination.
    ///
    /// The path is this list's identity (see [`Downloads::finish`]), so a
    /// rename has to go through here rather than being written on the entry.
    pub fn rename(&mut self, from: &Path, to: &Path) {
        if let Some(entry) = self.entries.iter_mut().find(|d| d.path == from && d.ok.is_none()) {
            entry.path = to.to_path_buf();
            self.save();
        }
    }

    pub fn entries(&self) -> &[Download] {
        &self.entries
    }

    /// One entry by its position in the list, newest first and 1-based, the way
    /// `download list` prints it.
    pub fn nth(&self, index: usize) -> Option<&Download> {
        self.entries.get(index.checked_sub(1)?)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.save();
    }

    fn save(&self) {
        let Some(file) = &self.file else { return };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut out = String::new();
        for d in &self.entries {
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\n",
                d.started,
                d.state(),
                d.path.display(),
                d.url.replace(['\t', '\n'], " ")
            ));
        }
        let _ = std::fs::write(file, out);
    }
}

fn path() -> PathBuf {
    crate::history::state_dir().join("downloads")
}

/// Where the desktop keeps downloads.
///
/// `XDG_DOWNLOAD_DIR` is exported by very little, so the real source of truth is
/// `user-dirs.dirs`, which is what `xdg-user-dir` reads. Parsing it directly
/// avoids spawning a process on a path that runs while a download is being
/// decided, which is a moment the UI is blocked on.
pub fn download_dir() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    if let Some(dir) = std::env::var_os("XDG_DOWNLOAD_DIR").map(PathBuf::from)
        && dir.is_absolute()
    {
        return dir;
    }

    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    if let Ok(raw) = std::fs::read_to_string(config.join("user-dirs.dirs")) {
        for line in raw.lines() {
            let line = line.trim();
            let Some(value) = line.strip_prefix("XDG_DOWNLOAD_DIR=") else { continue };
            let value = value.trim_matches('"');
            let expanded = match value.strip_prefix("$HOME/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(value),
            };
            if expanded.is_absolute() {
                return expanded;
            }
        }
    }
    home.join("Downloads")
}

/// A name that will not overwrite something already there.
///
/// Chrome's rule: `report.pdf`, then `report (1).pdf`. Suffix before the
/// extension, so the file still opens in the right application.
pub fn unique(dir: &Path, name: &str) -> PathBuf {
    let name = sanitise(name);
    let candidate = dir.join(&name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        // A leading dot is the whole name of a dotfile, not an extension.
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (name.clone(), String::new()),
    };
    for n in 1..1000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(name)
}

/// Strip anything that would make the name escape the download directory.
///
/// A server chooses this name, so it is untrusted input: `../../.bashrc` is a
/// filename as far as `Content-Disposition` is concerned.
fn sanitise(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let name = name.trim().trim_matches('.');
    let cleaned: String = name.chars().filter(|c| !c.is_control() && *c != '/').collect();
    if cleaned.is_empty() { "download".to_string() } else { cleaned }
}

/// The filename a URL implies, before the disk is consulted.
pub fn name_from_url(url: &url::Url) -> String {
    url.path_segments()
        .and_then(|mut s| s.next_back())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| url.host_str().unwrap_or("download").to_string())
}

/// Hook WebKit's downloads so the browser chooses where files go and remembers
/// what it saved.
///
/// Deliberately *not* Tauri's `WebviewBuilder::on_download`. That path is wired
/// through wry, whose `decide-destination` handler reads `download.request()` --
/// which is null at that point on WebKitGTK 2.52, so the whole arm is skipped
/// and only the completion is ever reported. Verified here: `Finished` fired,
/// `Requested` never did, and the file landed in WebKit's own default directory.
/// Connecting to the signal directly is the same layer `find` already uses.
///
/// The signal is on the `WebContext`, which every tab shares, so this registers
/// exactly once however many tabs open.
#[cfg(target_os = "linux")]
pub fn watch<R: tauri::Runtime>(
    view: &tauri::webview::Webview<R>,
    state: std::sync::Arc<crate::state::AppState>,
) -> anyhow::Result<()> {
    use anyhow::Context as _;

    view.with_webview(move |platform| {
        use webkit2gtk::{DownloadExt, URIRequestExt, WebContextExt, WebViewExt};

        let Some(context) = platform.inner().context() else {
            tracing::error!("no web context; downloads will land wherever WebKit chooses");
            return;
        };
        if !state.claim_download_hook() {
            return;
        }
        tracing::debug!("hooked WebKit's download signal");

        context.connect_download_started(move |_context, download| {
            let state = state.clone();

            // Choose the destination *here*, not in `decide-destination`.
            //
            // That signal is the documented hook and it does not fire on
            // WebKitGTK 2.52 -- proven by probe: `download-started` arrives with
            // the request in hand, `destination` and `response` both null, and
            // `decide-destination` never follows. wry's own download support is
            // built on it, which is why Tauri's `on_download` reports only the
            // completion and never the request.
            //
            // At this moment only the URL is known, so the name comes from its
            // last path segment. `Content-Disposition` arrives with the
            // response, later, and is applied then when it says something
            // better -- see the `notify::response` hook below.
            let url =
                download.request().and_then(|r| r.uri()).map(|u| u.to_string()).unwrap_or_default();
            let from_url = url
                .parse::<url::Url>()
                .map(|u| name_from_url(&u))
                .unwrap_or_else(|_| "download".to_string());
            let path = state.download_path(&from_url);

            tracing::info!(%url, path = %path.display(), "download started");
            if let Ok(mut list) = state.downloads.lock() {
                list.start(&url, &path, crate::history::now());
            }
            if state.config.downloads.notify {
                crate::dispatch::toast(&format!("Downloading {}", path_name(&path)));
            }
            download.set_destination(&path.to_string_lossy());

            // A server that names the file explicitly knows better than its own
            // URL does: `/download?id=7` is not a filename. The response is the
            // first moment that name exists, and WebKit accepts a destination
            // right up until the first byte is written.
            let renamed = std::rc::Rc::new(std::cell::Cell::new(false));
            let rename_state = state.clone();
            let chosen = path.clone();
            download.connect_response_notify(move |download| {
                use webkit2gtk::URIResponseExt;
                if renamed.replace(true) {
                    return;
                }
                let Some(suggested) = download.response().and_then(|r| r.suggested_filename())
                else {
                    return;
                };
                if suggested.as_str() == from_url || suggested.is_empty() {
                    return;
                }
                let better = rename_state.download_path(suggested.as_str());
                tracing::info!(path = %better.display(), "the server named the file");
                download.set_destination(&better.to_string_lossy());
                if let Ok(mut list) = rename_state.downloads.lock() {
                    list.rename(&chosen, &better);
                }
            });

            // Progress, on every chunk. Cheap by construction: nothing is
            // written to disk (see `Downloads::progress`) and the destination is
            // read from the download rather than captured, so a rename part-way
            // through -- which is exactly what the response hook above does --
            // does not leave this updating an entry that no longer exists.
            let moving = state.clone();
            let received = std::rc::Rc::new(std::cell::Cell::new(0u64));
            let counted = received.clone();
            download.connect_received_data(move |_download, chunk| {
                counted.set(counted.get().saturating_add(chunk));
            });
            download.connect_estimated_progress_notify(move |download| {
                let Some(destination) = download.destination() else { return };
                let path = PathBuf::from(destination.as_str());
                if let Ok(mut list) = moving.downloads.lock() {
                    list.progress(&path, download.estimated_progress(), received.get());
                }
            });

            let done = state.clone();
            download.connect_finished(move |download| settle(&done, download, true));
            let failed = state.clone();
            download.connect_failed(move |download, error| {
                tracing::warn!(%error, "download failed");
                settle(&failed, download, false);
            });
        });
    })
    .context("could not reach the webview to watch downloads")
}

#[cfg(not(target_os = "linux"))]
pub fn watch<R: tauri::Runtime>(
    _view: &tauri::webview::Webview<R>,
    _state: std::sync::Arc<crate::state::AppState>,
) -> anyhow::Result<()> {
    Ok(())
}

/// Close out a transfer, however it ended.
///
/// `finished` fires after `failed` as well as instead of it, so a failure would
/// otherwise be immediately overwritten by a success. The entry is already
/// closed by then, and `finish` only matches a *running* one, which is what
/// makes the second call a no-op rather than a lie.
#[cfg(target_os = "linux")]
fn settle(
    state: &std::sync::Arc<crate::state::AppState>,
    download: &webkit2gtk::Download,
    ok: bool,
) {
    use webkit2gtk::{DownloadExt, URIRequestExt};

    let Some(destination) = download.destination() else { return };
    let path = PathBuf::from(destination.as_str());
    let url = download.request().and_then(|r| r.uri()).map(|u| u.to_string());
    let closed =
        state.downloads.lock().ok().and_then(|mut list| list.finish(&path, url.as_deref(), ok));
    let Some(entry) = closed else { return };

    tracing::info!(path = %path.display(), ok, "download settled");
    if state.config.downloads.notify {
        crate::dispatch::toast(&if ok {
            format!("Saved {}", entry.name())
        } else {
            format!("Download failed: {}", entry.name())
        });
    }
}

fn path_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_started_download_is_running_until_it_finishes() {
        let mut d = Downloads::default();
        d.start("https://a.example/x.zip", Path::new("/tmp/x.zip"), 10);
        assert_eq!(d.entries()[0].state(), "running");
        let done = d.finish(Path::new("/tmp/x.zip"), None, true);
        assert_eq!(done.map(|d| d.state()), Some("done"));
        assert_eq!(d.entries()[0].state(), "done");
    }

    #[test]
    fn a_url_learned_late_is_filled_in() {
        // WebKit does not always have the request to hand when the transfer
        // starts, so the URL can arrive with the completion.
        let mut d = Downloads::default();
        d.start("", Path::new("/tmp/x.zip"), 10);
        d.finish(Path::new("/tmp/x.zip"), Some("https://a.example/x.zip"), true);
        assert_eq!(d.entries()[0].url, "https://a.example/x.zip");
    }

    #[test]
    fn two_transfers_to_different_paths_do_not_close_each_other_out() {
        let mut d = Downloads::default();
        d.start("https://a.example/x.zip", Path::new("/tmp/x.zip"), 10);
        d.start("https://a.example/x.zip", Path::new("/tmp/x (1).zip"), 11);
        d.finish(Path::new("/tmp/x (1).zip"), None, true);
        assert_eq!(d.nth(1).unwrap().state(), "done");
        assert_eq!(d.nth(2).unwrap().state(), "running");
    }

    #[test]
    fn a_rename_keeps_the_entry_findable() {
        let mut d = Downloads::default();
        d.start("https://a.example/get?id=7", Path::new("/tmp/get"), 10);
        d.rename(Path::new("/tmp/get"), Path::new("/tmp/invoice.pdf"));
        assert_eq!(d.entries()[0].name(), "invoice.pdf");
        assert!(d.finish(Path::new("/tmp/invoice.pdf"), None, true).is_some());
    }

    #[test]
    fn finishing_something_never_started_is_not_a_panic() {
        let mut d = Downloads::default();
        assert!(d.finish(Path::new("/tmp/x.zip"), None, true).is_none());
    }

    #[test]
    fn the_newest_is_first_and_one_indexed() {
        let mut d = Downloads::default();
        d.start("https://a.example/1", Path::new("/tmp/1"), 1);
        d.start("https://a.example/2", Path::new("/tmp/2"), 2);
        assert_eq!(d.nth(1).map(|d| d.url.as_str()), Some("https://a.example/2"));
        assert_eq!(d.nth(2).map(|d| d.url.as_str()), Some("https://a.example/1"));
        assert!(d.nth(0).is_none(), "the list is 1-based, so 0 names nothing");
        assert!(d.nth(3).is_none());
    }

    #[test]
    fn a_server_cannot_name_a_file_outside_the_download_directory() {
        assert_eq!(sanitise("../../.bashrc"), "bashrc");
        assert_eq!(sanitise("/etc/passwd"), "passwd");
        assert_eq!(sanitise("  "), "download");
        assert_eq!(sanitise(""), "download");
        assert_eq!(sanitise("report.pdf"), "report.pdf");
    }

    #[test]
    fn a_second_copy_is_numbered_before_the_extension() {
        let dir = std::env::temp_dir().join(format!("oma-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = unique(&dir, "report.pdf");
        assert_eq!(first.file_name().unwrap(), "report.pdf");
        std::fs::write(&first, b"x").unwrap();
        assert_eq!(unique(&dir, "report.pdf").file_name().unwrap(), "report (1).pdf");
        let noext = unique(&dir, "LICENSE");
        assert_eq!(noext.file_name().unwrap(), "LICENSE");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_url_with_no_filename_still_names_something() {
        let named: url::Url = "https://a.example/files/report.pdf?v=2".parse().unwrap();
        assert_eq!(name_from_url(&named), "report.pdf");
        let bare: url::Url = "https://a.example/".parse().unwrap();
        assert_eq!(name_from_url(&bare), "a.example");
    }
}
