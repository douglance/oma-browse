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

            // Record the transfer here, where the request is in hand.
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
            download.set_destination(&destination_uri(&path));

            // Where the file is meant to end up, shared with the hooks below.
            // It moves once, when the server turns out to have a better name
            // for it than the URL did.
            let wanted = std::rc::Rc::new(std::cell::RefCell::new(path.clone()));

            // A server that names the file explicitly knows better than its own
            // URL does: `/download?id=7` is not a filename. The response is the
            // first moment that name exists.
            let renamed = std::rc::Rc::new(std::cell::Cell::new(false));
            let rename_state = state.clone();
            let chosen = path.clone();
            let renaming = wanted.clone();
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
                download.set_destination(&destination_uri(&better));
                *renaming.borrow_mut() = better.clone();
                if let Ok(mut list) = rename_state.downloads.lock() {
                    list.rename(&chosen, &better);
                }
            });

            // Where WebKit *actually* put the bytes.
            //
            // On WebKitGTK 2.52 the destination is decided in the network
            // process before the download is announced here at all: neither
            // `decide-destination` nor the `destination` property reaches that
            // decision, so `set_destination` above takes the value, reads it
            // back unchanged, and the file still lands in WebKit's own download
            // directory. `created-destination` is the one hook that says where
            // that was, and the file is put where it was asked to go once the
            // bytes stop arriving -- see `place`.
            let landed = std::rc::Rc::new(std::cell::RefCell::new(None::<PathBuf>));
            let landing = landed.clone();
            download.connect_created_destination(move |_download, where_| {
                *landing.borrow_mut() = Some(path_from_destination(where_));
            });

            // Progress, on every chunk. Cheap by construction: nothing is
            // written to disk (see `Downloads::progress`) and the destination is
            // read from the shared cell rather than captured, so a rename
            // part-way through -- which is exactly what the response hook above
            // does -- does not leave this updating an entry that no longer
            // exists.
            let moving = state.clone();
            let received = std::rc::Rc::new(std::cell::Cell::new(0u64));
            let counted = received.clone();
            let progressing = wanted.clone();
            download.connect_received_data(move |_download, chunk| {
                counted.set(counted.get().saturating_add(chunk));
            });
            download.connect_estimated_progress_notify(move |download| {
                let path = progressing.borrow().clone();
                if let Ok(mut list) = moving.downloads.lock() {
                    list.progress(&path, download.estimated_progress(), received.get());
                }
            });

            let done = state.clone();
            let finishing = wanted.clone();
            let finished_at = landed.clone();
            download.connect_finished(move |download| {
                let target = finishing.borrow().clone();
                let actual = place(finished_at.borrow().as_deref(), &target);
                // A move that could not happen leaves the file where WebKit put
                // it, and the list has to say so: a `download open` on a path
                // with no file is worse than an unexpected path.
                if actual != target
                    && let Ok(mut list) = done.downloads.lock()
                {
                    list.rename(&target, &actual);
                }
                settle(&done, download, &actual, true);
            });
            let failed = state.clone();
            let failing = wanted.clone();
            download.connect_failed(move |download, error| {
                tracing::warn!(%error, "download failed");
                let target = failing.borrow().clone();
                settle(&failed, download, &target, false);
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
    path: &Path,
    ok: bool,
) {
    use webkit2gtk::{DownloadExt, URIRequestExt};

    let url = download.request().and_then(|r| r.uri()).map(|u| u.to_string());
    let closed =
        state.downloads.lock().ok().and_then(|mut list| list.finish(path, url.as_deref(), ok));
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

/// A destination in the form WebKit's `destination` property actually wants.
///
/// It is a URI, not a path. A plain path set into it is not refused -- reading
/// the property straight back hands you the same string -- it is simply ignored
/// when the time comes to open the file, and WebKit falls back to its own
/// choice: `~/Downloads`, under the name the server suggested. The symptoms were
/// a browser that ignored `[downloads] dir` entirely and a `download list` whose
/// entries never left "running", because nothing afterwards matched the path
/// that had been recorded.
///
/// Through glib rather than `format!("file://{path}")` because a filename can
/// contain a space, a `#` or a `%`, and every one of those means something else
/// in a URI.
#[cfg(target_os = "linux")]
fn destination_uri(path: &Path) -> String {
    gtk::glib::filename_to_uri(path, None)
        .map(|uri| uri.to_string())
        .unwrap_or_else(|_| format!("file://{}", path.display()))
}

/// Put a finished download where it was asked to go, and say where that was.
///
/// WebKit decides the destination before this process is told the download
/// exists, so `[downloads] dir` cannot be honoured while the bytes are still
/// arriving -- only afterwards. A file that already landed in the right place is
/// not touched, which is what happens on any WebKit that does obey
/// `set_destination`, so this costs nothing where it is not needed.
///
/// `rename` first because a move within one filesystem is free. The copy is for
/// a downloads directory on another mount, which is ordinary enough -- a
/// `~/Downloads` on a second disk, a temporary directory on tmpfs -- to be worth
/// handling rather than failing on.
#[cfg(target_os = "linux")]
fn place(landed: Option<&Path>, wanted: &Path) -> PathBuf {
    let Some(landed) = landed else {
        // Nothing said where the bytes went, so there is nothing to move and no
        // reason to doubt the transfer.
        return wanted.to_path_buf();
    };
    if landed == wanted {
        return wanted.to_path_buf();
    }
    if let Some(parent) = wanted.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::rename(landed, wanted).is_ok() {
        tracing::info!(from = %landed.display(), to = %wanted.display(), "put the download away");
        return wanted.to_path_buf();
    }
    match std::fs::copy(landed, wanted) {
        Ok(_) => {
            let _ = std::fs::remove_file(landed);
            tracing::info!(from = %landed.display(), to = %wanted.display(), "copied the download");
            wanted.to_path_buf()
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                at = %landed.display(),
                wanted = %wanted.display(),
                "could not move the download; leaving it where WebKit put it"
            );
            landed.to_path_buf()
        }
    }
}

#[cfg(target_os = "linux")]
fn path_from_destination(raw: &str) -> PathBuf {
    if raw.starts_with("file://")
        && let Ok((path, _)) = gtk::glib::filename_from_uri(raw)
    {
        return path;
    }
    PathBuf::from(raw)
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
    // The three below are the WebKit-facing half, and are the reason a download
    // used to land in `~/Downloads` however the config was written.

    #[cfg(target_os = "linux")]
    #[test]
    fn a_destination_survives_the_trip_through_a_uri() {
        // A space and a `#` are both ordinary in a filename and both mean
        // something else in a URI, which is why this does not build the string
        // by hand.
        let path = PathBuf::from("/tmp/oma download #2.bin");
        let uri = destination_uri(&path);
        assert!(uri.starts_with("file:///"), "not a file URI: {uri}");
        assert!(!uri.contains(' '), "a raw space in a URI: {uri}");
        assert_eq!(path_from_destination(&uri), path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_destination_that_is_already_a_path_is_left_alone() {
        // Kept working on purpose: a WebKit that hands back a path rather than
        // a URI must not have it read as a relative filename.
        assert_eq!(path_from_destination("/tmp/plain.bin"), PathBuf::from("/tmp/plain.bin"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_download_is_moved_to_where_it_was_asked_to_go() {
        let dir = std::env::temp_dir().join(format!("oma-place-{}", std::process::id()));
        let webkits = dir.join("webkit");
        let ours = dir.join("ours");
        std::fs::create_dir_all(&webkits).unwrap();
        let landed = webkits.join("file.bin");
        std::fs::write(&landed, b"payload").unwrap();

        let wanted = ours.join("file.bin");
        assert_eq!(place(Some(&landed), &wanted), wanted);
        assert_eq!(std::fs::read(&wanted).unwrap(), b"payload");
        assert!(!landed.exists(), "the file was copied rather than moved");

        // Already in the right place: nothing happens, and nothing is lost.
        assert_eq!(place(Some(&wanted), &wanted), wanted);
        assert_eq!(std::fs::read(&wanted).unwrap(), b"payload");

        // Nowhere to move it from is not a failure; the transfer still happened.
        assert_eq!(place(None, &wanted), wanted);

        // And a move that cannot happen answers with where the file really is,
        // because a list entry pointing at nothing is worse than a surprising
        // path.
        let missing = webkits.join("never-existed.bin");
        assert_eq!(place(Some(&missing), &ours.join("elsewhere.bin")), missing);

        std::fs::remove_dir_all(&dir).ok();
    }
}
