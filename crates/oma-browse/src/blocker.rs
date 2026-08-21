//! Content blocking, through WebKit's own compiler.
//!
//! WebKitGTK ships the same content-blocker engine Safari uses: a JSON rule
//! list is compiled once into a bytecode blob, and from then on every request
//! the engine makes is matched against it in C++ before a socket is opened.
//! Nothing is fetched and then hidden, which is what an extension-based blocker
//! does and why an extension-based blocker costs so much. This one makes pages
//! faster rather than slower.
//!
//! The catch, and the reason this module exists at all: `webkit2gtk` 2.0
//! generates `UserContentManager::add_filter(&self, filter: /*Ignored*/&UserContentFilter)`
//! -- the parameter type is not bound, and `UserContentFilterStore` is not bound
//! at any level. The C API is complete in `webkit2gtk-sys`, so this is the one
//! module in the tree that reaches past the safe wrapper, and every `unsafe`
//! block in it is one call wide with its argument contract written above it.
//!
//! Three things live on the GTK main thread and never leave it: the store, the
//! filters that have been compiled, and the content managers to apply them to.
//! They are `thread_local!` rather than on [`AppState`] for the usual reason --
//! a `WebKitUserContentFilter` is not `Send`, and pretending otherwise is how a
//! browser gets a use-after-free on somebody else's thread.
//!
//! # What is not here
//!
//! Rule lists are read from files on disk, not from URLs. Fetching one over
//! HTTPS would mean a TLS stack in the dependency tree -- `reqwest` is in the
//! lock file but with no TLS backend -- to do a job `curl` already does, once,
//! before the browser ever starts. The README documents the one-liner.

#![cfg(target_os = "linux")]
// The one module in the tree that is allowed to be unsafe, and the lint is
// lifted here rather than at each of the dozen call sites because the reason is
// the same at every one of them: `webkit2gtk` 2.0 binds neither
// `UserContentFilter` nor `UserContentFilterStore`, so every call below is a
// direct FFI call into a C API that is complete in `webkit2gtk-sys`. The
// discipline the lint was protecting is kept a different way -- every call has
// a `SAFETY:` note above it saying what it assumes, and nothing unsafe leaves
// this file: `Filter` owns its refcount, the raw pointers never cross a thread
// boundary, and everything public here takes and returns ordinary Rust types.
//
// See the workspace lint in Cargo.toml, which anticipates exactly this.
#![allow(unsafe_code, reason = "webkit2gtk 2.0 binds no part of the content-filter API")]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use tauri::webview::Webview;

use crate::state::AppState;

/// One compiled rule list, owning a reference to it.
///
/// `WebKitUserContentFilter` is a refcounted box, not a GObject, so it gets its
/// refcounting written out here rather than inherited from `glib::wrapper!`.
struct Filter {
    ptr: *mut webkit2gtk_sys::WebKitUserContentFilter,
}

impl Filter {
    /// Take ownership of a filter returned by one of the `_finish` calls, which
    /// hand back a full reference.
    ///
    /// # Safety
    /// `ptr` must be null or a valid filter this code owns a reference to.
    unsafe fn from_full(ptr: *mut webkit2gtk_sys::WebKitUserContentFilter) -> Option<Self> {
        (!ptr.is_null()).then_some(Filter { ptr })
    }
}

impl Clone for Filter {
    fn clone(&self) -> Self {
        unsafe {
            webkit2gtk_sys::webkit_user_content_filter_ref(self.ptr);
        }
        Filter { ptr: self.ptr }
    }
}

impl Drop for Filter {
    fn drop(&mut self) {
        unsafe {
            webkit2gtk_sys::webkit_user_content_filter_unref(self.ptr);
        }
    }
}

impl std::fmt::Debug for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Filter({:p})", self.ptr)
    }
}

thread_local! {
    /// The compiled-filter cache, opened once.
    static STORE: RefCell<Option<*mut webkit2gtk_sys::WebKitUserContentFilterStore>> =
        const { RefCell::new(None) };
    /// What has been compiled and loaded this session, newest last.
    static LOADED: RefCell<Vec<(String, Filter)>> = const { RefCell::new(Vec::new()) };
    /// Every content webview's manager.
    ///
    /// Kept so that a list which finishes compiling after a tab has opened still
    /// reaches that tab -- compilation of a real blocklist takes seconds, and a
    /// browser that only blocked in tabs opened afterwards would look broken.
    static MANAGERS: RefCell<Vec<webkit2gtk::UserContentManager>> =
        const { RefCell::new(Vec::new()) };
    /// Managers currently excused from blocking, by webview label.
    static EXCUSED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Where compiled blobs live: a cache, because they are derived data that can
/// always be rebuilt from the rule file.
pub fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        });
    crate::profile::within(base.join("oma-browse")).join("filters")
}

/// An identifier WebKit will accept, and that changes when the file does.
///
/// The length and modification time stand in for a hash of the contents: a
/// blocklist is megabytes, hashing it on every launch would cost more than the
/// lookup saves, and a rule file that changes without changing either is a file
/// somebody has gone out of their way to forge.
pub fn identifier(path: &Path) -> String {
    let stem: String = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rules".to_string())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let stamp = std::fs::metadata(path)
        .ok()
        .map(|m| {
            let len = m.len();
            let secs = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            format!("{len:x}-{secs:x}")
        })
        .unwrap_or_else(|| "missing".to_string());
    format!("{}-{stamp}", stem.trim_matches('-'))
}

/// The rule files this browser is configured to block with, resolved and
/// checked.
///
/// Returns the problems as well as the paths, because a blocklist that silently
/// is not there is a blocklist you believe is working.
pub fn configured(content: &crate::config::Content) -> (Vec<PathBuf>, Vec<String>) {
    let mut paths = Vec::new();
    let mut problems = Vec::new();
    for rule in &content.rules {
        let raw = rule.trim();
        if raw.is_empty() {
            continue;
        }
        if raw.starts_with("http://") || raw.starts_with("https://") {
            problems.push(format!(
                "{raw}: rule lists are read from disk, not fetched. Download it once with \
                 `curl -Lo <path> {raw}` and put the path here instead"
            ));
            continue;
        }
        let path = PathBuf::from(crate::paths::shellexpand(raw));
        if path.is_file() {
            paths.push(path);
        } else {
            problems.push(format!("{}: no such file", path.display()));
        }
    }
    (paths, problems)
}

// ---------------------------------------------------------------------------
// The GTK-thread side
// ---------------------------------------------------------------------------

/// Open the compiled-filter cache, once.
fn store() -> Option<*mut webkit2gtk_sys::WebKitUserContentFilterStore> {
    STORE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(store) = *slot {
            return Some(store);
        }
        let dir = cache_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %e, dir = %dir.display(), "no filter cache; nothing will block");
            return None;
        }
        let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
        // SAFETY: `path` is a valid NUL-terminated C string that outlives the
        // call, which is all this constructor reads. It returns a full
        // reference, kept for the life of the process.
        let store = unsafe { webkit2gtk_sys::webkit_user_content_filter_store_new(path.as_ptr()) };
        if store.is_null() {
            return None;
        }
        *slot = Some(store);
        Some(store)
    })
}

/// What a load or a compile answers with.
type Answered = Box<dyn FnOnce(Result<Filter>)>;

/// The `_finish` half of an async call, given the function that performs it.
///
/// # Safety
/// Called by GLib with the source object and result it was handed; `data` must
/// be the `Answered` this module boxed on the way in.
unsafe fn settle(
    source: *mut gtk::glib::gobject_ffi::GObject,
    result: *mut gtk::gio::ffi::GAsyncResult,
    data: gtk::glib::ffi::gpointer,
    finish: unsafe extern "C" fn(
        *mut webkit2gtk_sys::WebKitUserContentFilterStore,
        *mut gtk::gio::ffi::GAsyncResult,
        *mut *mut gtk::glib::ffi::GError,
    ) -> *mut webkit2gtk_sys::WebKitUserContentFilter,
) {
    let answer: Answered = unsafe { *Box::from_raw(data as *mut Answered) };
    let mut error: *mut gtk::glib::ffi::GError = std::ptr::null_mut();
    let filter = unsafe {
        finish(source as *mut webkit2gtk_sys::WebKitUserContentFilterStore, result, &mut error)
    };

    if !error.is_null() {
        let message = unsafe {
            let owned: gtk::glib::Error = gtk::glib::translate::from_glib_full(error);
            owned.to_string()
        };
        answer(Err(anyhow!(message)));
        return;
    }
    match unsafe { Filter::from_full(filter) } {
        Some(filter) => answer(Ok(filter)),
        None => answer(Err(anyhow!("the filter store answered with nothing"))),
    }
}

unsafe extern "C" fn loaded(
    source: *mut gtk::glib::gobject_ffi::GObject,
    result: *mut gtk::gio::ffi::GAsyncResult,
    data: gtk::glib::ffi::gpointer,
) {
    unsafe {
        settle(source, result, data, webkit2gtk_sys::webkit_user_content_filter_store_load_finish);
    }
}

unsafe extern "C" fn compiled(
    source: *mut gtk::glib::gobject_ffi::GObject,
    result: *mut gtk::gio::ffi::GAsyncResult,
    data: gtk::glib::ffi::gpointer,
) {
    unsafe {
        settle(
            source,
            result,
            data,
            webkit2gtk_sys::webkit_user_content_filter_store_save_from_file_finish,
        );
    }
}

/// Load an already-compiled list out of the cache.
fn load(identifier: &str, answer: Answered) {
    let Some(store) = store() else {
        answer(Err(anyhow!("there is no filter cache to load from")));
        return;
    };
    let Ok(id) = std::ffi::CString::new(identifier) else {
        answer(Err(anyhow!("{identifier:?} is not a usable identifier")));
        return;
    };
    let data = Box::into_raw(Box::new(answer)) as gtk::glib::ffi::gpointer;
    // SAFETY: `store` is live for the process, `id` outlives the call (WebKit
    // copies it), the cancellable is deliberately null, and `data` is the boxed
    // `Answered` that `loaded` unpacks exactly once.
    unsafe {
        webkit2gtk_sys::webkit_user_content_filter_store_load(
            store,
            id.as_ptr(),
            std::ptr::null_mut(),
            Some(loaded),
            data,
        );
    }
}

/// Compile a rule file into the cache. Slow -- seconds, for a real blocklist --
/// which is the whole reason the cache exists.
fn compile(identifier: &str, path: &Path, answer: Answered) {
    use gtk::glib::translate::ToGlibPtr as _;

    let Some(store) = store() else {
        answer(Err(anyhow!("there is no filter cache to compile into")));
        return;
    };
    let Ok(id) = std::ffi::CString::new(identifier) else {
        answer(Err(anyhow!("{identifier:?} is not a usable identifier")));
        return;
    };
    let file = gtk::gio::File::for_path(path);
    let data = Box::into_raw(Box::new(answer)) as gtk::glib::ffi::gpointer;
    // SAFETY: as `load`, plus `file` -- which WebKit refs for the duration of
    // the operation, so the local going out of scope here is fine.
    unsafe {
        webkit2gtk_sys::webkit_user_content_filter_store_save_from_file(
            store,
            id.as_ptr(),
            file.to_glib_none().0,
            std::ptr::null_mut(),
            Some(compiled),
            data,
        );
    }
}

/// Add a filter to one manager.
fn apply(manager: &webkit2gtk::UserContentManager, filter: &Filter) {
    use gtk::glib::translate::ToGlibPtr as _;
    // SAFETY: both pointers are live and belong to this thread; `add_filter`
    // takes its own reference.
    unsafe {
        webkit2gtk_sys::webkit_user_content_manager_add_filter(
            manager.to_glib_none().0,
            filter.ptr,
        );
    }
}

fn unapply_all(manager: &webkit2gtk::UserContentManager) {
    use gtk::glib::translate::ToGlibPtr as _;
    // SAFETY: the manager is live and belongs to this thread.
    unsafe {
        webkit2gtk_sys::webkit_user_content_manager_remove_all_filters(manager.to_glib_none().0);
    }
}

/// Remember a compiled list and put it into every open tab.
fn adopt(identifier: String, filter: Filter) {
    LOADED.with(|loaded| {
        let mut loaded = loaded.borrow_mut();
        loaded.retain(|(name, _)| *name != identifier);
        loaded.push((identifier, filter.clone()));
    });
    MANAGERS.with(|managers| {
        for manager in managers.borrow().iter() {
            apply(manager, &filter);
        }
    });
}

/// Compile and apply everything the config names. Safe to call again.
///
/// Returns immediately: compiling is asynchronous, and a browser that blocked
/// its own main thread for four seconds at startup would be a browser nobody
/// kept open.
pub fn reload(state: &Arc<AppState>) -> Vec<String> {
    let (paths, problems) = configured(&state.config.content);
    if !state.config.content.block {
        return problems;
    }
    for path in paths {
        let id = identifier(&path);
        let already = LOADED.with(|loaded| loaded.borrow().iter().any(|(name, _)| *name == id));
        if already {
            continue;
        }
        let for_compile = id.clone();
        let file = path.clone();
        load(
            &id,
            Box::new(move |result| match result {
                Ok(filter) => {
                    tracing::info!(list = %for_compile, "blocklist loaded from cache");
                    adopt(for_compile, filter);
                }
                // Not in the cache, or the cache is from an older WebKit. Either
                // way the answer is the same: compile it.
                Err(e) => {
                    tracing::info!(list = %for_compile, why = %e, "compiling blocklist");
                    let name = for_compile.clone();
                    compile(
                        &for_compile,
                        &file,
                        Box::new(move |result| match result {
                            Ok(filter) => {
                                tracing::info!(list = %name, "blocklist compiled");
                                adopt(name, filter);
                            }
                            Err(e) => {
                                tracing::warn!(list = %name, error = %e, "blocklist rejected");
                                crate::dispatch::toast(&format!("{name}: {e}"));
                            }
                        }),
                    );
                }
            }),
        );
    }
    problems
}

/// What is blocking right now.
pub fn loaded_lists() -> Vec<String> {
    LOADED.with(|loaded| loaded.borrow().iter().map(|(name, _)| name.clone()).collect())
}

/// [`reload`] and [`loaded_lists`], from a command.
///
/// Everything above this line runs on the GTK main thread and cannot leave it;
/// a command runs on the runtime. So the work hops, the same way
/// [`crate::policy::settle`] does, and the answer comes back down a channel.
pub async fn ask(state: &Arc<AppState>, recompile: bool, tab: Option<String>) -> Result<Report> {
    let app = state.app_handle().context("the window is not up yet")?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let inner = state.clone();
    app.run_on_main_thread(move || {
        let problems = if recompile { reload(&inner) } else { configured(&inner.config.content).1 };
        let here = tab.map(|label| !is_excused(&label));
        let _ = tx.send(Report { lists: loaded_lists(), problems, here });
    })
    .context("could not reach the main thread")?;
    rx.await.context("the main thread dropped the question")
}

/// What [`ask`] answers with.
#[derive(Debug, Default)]
pub struct Report {
    /// Lists compiled and applied.
    pub lists: Vec<String>,
    /// Rule files that could not be used, and why.
    pub problems: Vec<String>,
    /// Whether the tab that was asked about is blocking. `None` when no tab
    /// was named.
    pub here: Option<bool>,
}

/// Stop blocking in one tab, or start again.
///
/// Per webview, because that is where WebKit keeps filters: a manager belongs to
/// a webview, so excusing a tab is exactly removing this tab's filters and
/// putting them back. There is no global switch to flip and no page to reload
/// for the tabs that were not asked about.
pub fn excuse<R: tauri::Runtime>(view: &Webview<R>, off: bool) -> Result<()> {
    let label = view.label().to_string();
    view.with_webview(move |platform| {
        use webkit2gtk::WebViewExt;

        let Some(manager) = platform.inner().user_content_manager() else { return };
        EXCUSED.with(|excused| {
            let mut excused = excused.borrow_mut();
            excused.retain(|l| *l != label);
            if off {
                excused.push(label.clone());
            }
        });
        if off {
            unapply_all(&manager);
        } else {
            LOADED.with(|loaded| {
                for (_, filter) in loaded.borrow().iter() {
                    apply(&manager, filter);
                }
            });
        }
    })
    .context("could not reach the webview")
}

/// Whether this tab is currently excused.
pub fn is_excused(label: &str) -> bool {
    EXCUSED.with(|excused| excused.borrow().iter().any(|l| l == label))
}

/// Register a new content webview, and give it whatever is already compiled.
pub fn install<R: tauri::Runtime>(view: &Webview<R>, state: Arc<AppState>) -> Result<()> {
    if !state.config.content.block {
        return Ok(());
    }
    view.with_webview(move |platform| {
        use webkit2gtk::WebViewExt;

        let Some(manager) = platform.inner().user_content_manager() else {
            tracing::warn!("no user content manager; this tab blocks nothing");
            return;
        };
        LOADED.with(|loaded| {
            for (_, filter) in loaded.borrow().iter() {
                apply(&manager, filter);
            }
        });
        MANAGERS.with(|managers| managers.borrow_mut().push(manager));
    })
    .context("could not reach the webview to install the blocker")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identifier_is_safe_to_put_in_a_filename() {
        let id = identifier(Path::new("/tmp/easylist v3.json"));
        assert!(!id.contains(' '), "{id}");
        assert!(!id.contains('/'), "{id}");
        assert!(id.starts_with("easylist-v3-"), "{id}");
    }

    #[test]
    fn a_missing_file_still_gets_an_identifier() {
        // It has to: the identifier is what the "is it already compiled" check
        // is keyed on, and answering that question must not panic on a file
        // somebody deleted between two launches.
        let id = identifier(Path::new("/nowhere/at/all.json"));
        assert!(id.starts_with("all-"), "{id}");
        assert!(id.ends_with("missing"), "{id}");
    }

    #[test]
    fn the_identifier_changes_when_the_file_does() {
        let dir = std::env::temp_dir().join("oma-browse-blocker-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.json");
        std::fs::write(&path, b"[]").unwrap();
        let before = identifier(&path);
        std::fs::write(&path, b"[{}]").unwrap();
        let after = identifier(&path);
        assert_ne!(before, after, "a changed file must recompile rather than reuse the cache");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_url_is_refused_with_the_command_that_fixes_it() {
        let content = crate::config::Content {
            block: true,
            rules: vec!["https://example.com/easylist.json".to_string()],
        };
        let (paths, problems) = configured(&content);
        assert!(paths.is_empty());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("curl -Lo"), "{}", problems[0]);
    }

    #[test]
    fn a_rule_file_that_is_not_there_is_reported_rather_than_ignored() {
        let content =
            crate::config::Content { block: true, rules: vec!["/nowhere/rules.json".to_string()] };
        let (paths, problems) = configured(&content);
        assert!(paths.is_empty());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("no such file"), "{}", problems[0]);
    }

    #[test]
    fn a_rule_file_that_is_there_is_used() {
        let dir = std::env::temp_dir().join("oma-browse-blocker-used");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.json");
        std::fs::write(&path, b"[]").unwrap();
        let content = crate::config::Content {
            block: true,
            rules: vec![path.display().to_string(), String::new(), "  ".to_string()],
        };
        let (paths, problems) = configured(&content);
        assert_eq!(paths.len(), 1, "an empty entry is not a rule list");
        assert!(problems.is_empty(), "{problems:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
