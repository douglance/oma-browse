//! Cookies, caches and the rest of what sites leave behind.
//!
//! Every mainstream browser puts this behind one dialog -- Chrome's
//! Ctrl-Shift-Delete, Firefox's "Clear Data" -- because it is what you reach for
//! when a login has wedged, when a site is serving you a stale bundle, or when
//! you want to stop being recognised by somewhere you visited once. This browser
//! had no way to do any of it: `history clear` forgets the browser's own list of
//! places, which is a different thing entirely and leaves every cookie in place.
//! Short of deleting the profile directory with the browser shut, there was no
//! answer.
//!
//! WebKit keeps all of it behind one object, so this is a thin thing: name the
//! kinds, name the sites, say how far back, and hand it over.
//!
//! Two decisions worth keeping:
//!
//! * **Per site, not only wholesale.** "Log me out of everywhere" and "unstick
//!   this one site" are both common, and only the second is safe to run without
//!   thinking. `--host` is the one that gets used.
//! * **Cookies are not cleared by a bare `data clear`.** Clearing the cache is
//!   housekeeping; clearing cookies logs you out of everything you are signed in
//!   to, which is not something to do because a command was short. The default
//!   is the cache and nothing else, and cookies are asked for by name.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use crate::state::AppState;

/// One site, and what it is keeping here.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Site {
    /// The origin WebKit files this under, e.g. `youtube.com`.
    pub name: String,
    /// Which kinds it has, in the same words `data clear` takes.
    pub kinds: Vec<String>,
    /// What it comes to on disk, where WebKit will say. Zero means it does not
    /// account for that kind by size rather than that the site is storing
    /// nothing -- cookies are the usual case.
    pub bytes: u64,
}

/// What a clear did.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Cleared {
    /// Which kinds were cleared.
    pub kinds: Vec<String>,
    /// The sites it was narrowed to, or empty for all of them.
    pub hosts: Vec<String>,
    /// How far back it went, in words. Empty means everything ever stored.
    pub since: String,
}

/// The kinds a person can name, and what each covers in WebKit's terms.
///
/// Deliberately fewer names than WebKit has types. Nobody clearing their
/// browser wants to reason about the difference between an offline application
/// cache, a DOM cache and a service worker registration -- they want "the cache"
/// gone. Each name here maps to every WebKit type that belongs to it, so a name
/// that means something to a person is still exact underneath.
#[cfg(target_os = "linux")]
pub fn kinds_of(name: &str) -> Result<webkit2gtk::WebsiteDataTypes> {
    use webkit2gtk::WebsiteDataTypes as T;

    let cache = T::MEMORY_CACHE | T::DISK_CACHE | T::OFFLINE_APPLICATION_CACHE | T::DOM_CACHE;
    let storage = T::SESSION_STORAGE
        | T::LOCAL_STORAGE
        | T::WEBSQL_DATABASES
        | T::INDEXEDDB_DATABASES
        | T::SERVICE_WORKER_REGISTRATIONS;

    Ok(match name {
        "cache" => cache,
        "cookies" => T::COOKIES,
        "storage" => storage,
        // Not `T::ALL`: that also takes the HSTS cache with it, and forgetting
        // that a site said "only ever reach me over HTTPS" is a downgrade of
        // your own security posture that nobody asked for by typing "all".
        // Device ID hash salts go the same way -- they are what keeps a site
        // from correlating you across origins.
        "all" => cache | storage | T::COOKIES | T::ITP,
        other => bail!("{other:?} is not a kind of data; try cache, cookies, storage, or all"),
    })
}

/// The same set, back in the words it was asked for, so a result can say what it
/// actually did rather than echoing the argument.
#[cfg(target_os = "linux")]
fn name_kinds(types: webkit2gtk::WebsiteDataTypes) -> Vec<String> {
    use webkit2gtk::WebsiteDataTypes as T;

    let mut names = Vec::new();
    if types
        .intersects(T::MEMORY_CACHE | T::DISK_CACHE | T::OFFLINE_APPLICATION_CACHE | T::DOM_CACHE)
    {
        names.push("cache".to_string());
    }
    if types.contains(T::COOKIES) {
        names.push("cookies".to_string());
    }
    if types.intersects(
        T::SESSION_STORAGE
            | T::LOCAL_STORAGE
            | T::WEBSQL_DATABASES
            | T::INDEXEDDB_DATABASES
            | T::SERVICE_WORKER_REGISTRATIONS,
    ) {
        names.push("storage".to_string());
    }
    names
}

/// `2h`, `7d`, `30m`, or nothing at all for everything ever stored.
///
/// Chrome offers a fixed menu here -- last hour, last day, last week -- and this
/// takes the same idea without the menu, because a browser driven by typing
/// should not make you count hours in a week.
pub fn since_of(text: &str) -> Result<Option<Duration>> {
    let text = text.trim();
    if text.is_empty() || text == "all" || text == "everything" {
        return Ok(None);
    }
    let (count, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| anyhow!("{text:?} has no unit; try 1h, 7d or 30m"))?,
    );
    let count: u64 =
        count.parse().with_context(|| format!("{text:?} does not start with a number"))?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        other => bail!("{other:?} is not a unit of time; try s, m, h, d or w"),
    };
    Ok(Some(Duration::from_secs(count * seconds)))
}

/// Throw data away.
///
/// `hosts` narrows it to those sites; empty is every site. `since` bounds it to
/// data touched that recently; `None` is everything ever stored.
#[cfg(target_os = "linux")]
pub async fn clear(
    state: &Arc<AppState>,
    types: webkit2gtk::WebsiteDataTypes,
    hosts: Vec<String>,
    since: Option<Duration>,
) -> Result<Cleared> {
    use webkit2gtk::{
        WebContextExt, WebViewExt as _, WebsiteDataManagerExt as _,
        WebsiteDataManagerExtManual as _, gio,
    };

    let view = any_webview(state)?;
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    let wanted = hosts.clone();

    view.with_webview(move |platform| {
        let Some(context) = platform.inner().context() else {
            let _ = tx.send(Err("this webview has no web context to clear".to_string()));
            return;
        };
        let Some(manager) = WebContextExt::website_data_manager(&context) else {
            let _ = tx.send(Err("this web context has no data manager".to_string()));
            return;
        };

        // Zero means "everything", which is what WebKit does with a zero
        // timespan too -- so the two agree without a special case.
        let span = gtk::glib::TimeSpan::from_microseconds(
            since.map(|d| d.as_micros() as i64).unwrap_or(0),
        );

        if wanted.is_empty() {
            manager.clear(types, span, None::<&gio::Cancellable>, move |result| {
                let _ = tx.send(result.map_err(|e| e.to_string()));
            });
            return;
        }

        // Named sites. WebKit will only remove data it can hand back, so the
        // list has to come from `fetch` rather than from the names as typed --
        // there is no way to build a `WebsiteData` from a string.
        // Cloned for the callback: `fetch` borrows the manager for the call
        // while the closure it is handed outlives it. A `WebsiteDataManager` is
        // a GObject, so this is a reference count rather than a copy.
        let owner = manager.clone();
        manager.fetch(types, None::<&gio::Cancellable>, move |result| {
            let found = match result {
                Ok(found) => found,
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                    return;
                }
            };
            let matched: Vec<_> = found
                .into_iter()
                .filter(|site| {
                    let stored = site.name().unwrap_or_default();
                    wanted.iter().any(|host| matches_host(&stored, host))
                })
                .collect();
            if matched.is_empty() {
                // Not an error: "there was nothing of theirs here" is a
                // successful outcome of being asked to remove it.
                let _ = tx.send(Ok(()));
                return;
            }
            let refs: Vec<&webkit2gtk::WebsiteData> = matched.iter().collect();
            owner.remove(types, refs.as_slice(), None::<&gio::Cancellable>, move |result| {
                let _ = tx.send(result.map_err(|e| e.to_string()));
            });
        });
    })
    .context("could not reach a webview to clear its data")?;

    match rx.await {
        Ok(Ok(())) => Ok(Cleared {
            kinds: name_kinds(types),
            hosts,
            since: since.map(describe_span).unwrap_or_default(),
        }),
        Ok(Err(e)) => bail!("{e}"),
        Err(_) => bail!("the clear was dropped before it finished"),
    }
}

/// What is stored, by site.
#[cfg(target_os = "linux")]
pub async fn list(state: &Arc<AppState>) -> Result<Vec<Site>> {
    use webkit2gtk::{WebContextExt, WebViewExt as _, WebsiteDataManagerExt as _, gio};

    let view = any_webview(state)?;
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<Vec<Site>, String>>();
    // Everything nameable, so the answer is the whole picture rather than the
    // picture of whatever the caller happened to ask about.
    let types = kinds_of("all")?;

    view.with_webview(move |platform| {
        let Some(context) = platform.inner().context() else {
            let _ = tx.send(Err("this webview has no web context to ask".to_string()));
            return;
        };
        let Some(manager) = WebContextExt::website_data_manager(&context) else {
            let _ = tx.send(Err("this web context has no data manager".to_string()));
            return;
        };

        manager.fetch(types, None::<&gio::Cancellable>, move |result| {
            let _ = tx.send(result.map_err(|e| e.to_string()).map(|found| {
                let mut sites: Vec<Site> = found
                    .into_iter()
                    .map(|site| Site {
                        name: site.name().unwrap_or_default().to_string(),
                        kinds: name_kinds(site.types()),
                        bytes: site.size(site.types()),
                    })
                    .collect();
                // Biggest first, then by name: the reason to look at this list
                // is almost always to find what is taking up room.
                sites.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
                sites
            }));
        });
    })
    .context("could not reach a webview to ask what is stored")?;

    match rx.await {
        Ok(Ok(sites)) => Ok(sites),
        Ok(Err(e)) => bail!("{e}"),
        Err(_) => bail!("the query was dropped before it finished"),
    }
}

/// Whether a stored site answers to a host somebody typed.
///
/// WebKit files data under a registrable domain -- `youtube.com`, not
/// `www.youtube.com` -- so an exact match alone would miss almost everything
/// anybody would think to type. A suffix match on a dot boundary catches the
/// subdomain without letting `oogle.com` match `google.com`.
fn matches_host(stored: &str, asked: &str) -> bool {
    let stored = stored.trim_start_matches('.').to_ascii_lowercase();
    let asked = asked.trim().trim_start_matches('.').to_ascii_lowercase();
    if stored == asked {
        return true;
    }
    asked.strip_suffix(&stored).is_some_and(|rest| rest.ends_with('.'))
        || stored.strip_suffix(&asked).is_some_and(|rest| rest.ends_with('.'))
}

/// A duration, in the words it was probably asked for.
fn describe_span(span: Duration) -> String {
    let seconds = span.as_secs();
    let (count, unit) = match seconds {
        s if s % (7 * 24 * 60 * 60) == 0 => (s / (7 * 24 * 60 * 60), "week"),
        s if s % (24 * 60 * 60) == 0 => (s / (24 * 60 * 60), "day"),
        s if s % (60 * 60) == 0 => (s / (60 * 60), "hour"),
        s if s % 60 == 0 => (s / 60, "minute"),
        s => (s, "second"),
    };
    if count == 1 { format!("the last {unit}") } else { format!("the last {count} {unit}s") }
}

/// Any content webview will do: the data manager belongs to the web context,
/// which every tab in this window shares. The active tab is preferred only so
/// that a window with one tab does not depend on the tab model being in step.
#[cfg(target_os = "linux")]
fn any_webview(state: &Arc<AppState>) -> Result<tauri::webview::Webview<tauri::Wry>> {
    use tauri::Manager as _;

    let app = state.app_handle().context("the window is not up yet")?;
    let label = state
        .tabs
        .try_read()
        .ok()
        .and_then(|tabs| tabs.active_label())
        .ok_or_else(|| anyhow!("there is no tab to ask"))?;
    app.get_webview(&label).with_context(|| format!("no webview labelled {label}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_read_the_way_people_write_them() {
        assert_eq!(since_of("1h").unwrap(), Some(Duration::from_secs(3600)));
        assert_eq!(since_of("30m").unwrap(), Some(Duration::from_secs(1800)));
        assert_eq!(since_of("7d").unwrap(), Some(Duration::from_secs(604_800)));
        assert_eq!(since_of("2w").unwrap(), Some(Duration::from_secs(1_209_600)));
    }

    #[test]
    fn no_duration_means_everything_ever_stored() {
        assert_eq!(since_of("").unwrap(), None);
        assert_eq!(since_of("all").unwrap(), None);
    }

    #[test]
    fn a_duration_that_makes_no_sense_says_so() {
        assert!(since_of("h").is_err(), "no number");
        assert!(since_of("7").is_err(), "no unit");
        assert!(since_of("7y").is_err(), "not a unit we take");
    }

    /// The reason this is not `==`: WebKit files data under the registrable
    /// domain, so the host somebody types is usually longer than the one stored.
    #[test]
    fn a_stored_domain_answers_for_its_subdomains() {
        assert!(matches_host("youtube.com", "www.youtube.com"));
        assert!(matches_host("youtube.com", "youtube.com"));
        assert!(matches_host("youtube.com", "YouTube.com"), "case is not part of a host");
        assert!(matches_host(".youtube.com", "youtube.com"), "a leading dot is a cookie's habit");
    }

    /// The failure that matters: clearing a site nobody asked to clear.
    #[test]
    fn a_partial_name_is_not_a_match() {
        assert!(!matches_host("google.com", "oogle.com"));
        assert!(!matches_host("google.com", "notgoogle.com"));
        assert!(!matches_host("google.com", "google.com.evil.test"));
        assert!(!matches_host("example.com", "example.org"));
    }

    #[test]
    fn a_span_is_described_the_way_it_was_asked_for() {
        assert_eq!(describe_span(Duration::from_secs(3600)), "the last hour");
        assert_eq!(describe_span(Duration::from_secs(7200)), "the last 2 hours");
        assert_eq!(describe_span(Duration::from_secs(604_800)), "the last week");
    }
}
