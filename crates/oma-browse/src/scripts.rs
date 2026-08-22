//! Your own JavaScript and CSS, on the sites you choose.
//!
//! This browser cannot have extensions. The WebKitGTK port has no
//! WebExtensions API at all -- `WebKitWebExtension` is a different thing with a
//! confusingly similar name, an in-process hook for the embedder rather than
//! anything a `.crx` could be loaded into -- so uBlock Origin and its kind
//! genuinely cannot run here, and no amount of work in this crate changes that.
//!
//! What *can* be answered is the reason people install them. Ad blocking is
//! [`crate::blocker`], which compiles Safari-format lists in C++ before a socket
//! is opened. Passwords are [`crate::vault`], reading the manager you already
//! use. Dark mode is the theme, on every page. What was left was the last
//! common one: a userscript manager -- Greasemonkey, Tampermonkey, Violentmonkey
//! -- for the small per-site fixes that are the whole reason a lot of people
//! keep an extension installed at all.
//!
//! That part WebKit gives us outright. `WebKitUserContentManager` takes scripts
//! and stylesheets with URL allow and block patterns, an injection time and a
//! frame scope, and applies them itself -- so a userscript here runs the way the
//! engine runs its own injections rather than through a script that watches for
//! navigations and races them.
//!
//! Drop a `.js` or a `.css` file in `~/.config/oma-browse/scripts/` with a
//! header saying where it applies:
//!
//! ```text
//! // ==UserScript==
//! // @name     Wider articles
//! // @match    https://example.com/*
//! // @exclude  https://example.com/admin/*
//! // @run-at   document-end
//! // ==/UserScript==
//! ```
//!
//! A pattern is `scheme://host/path`, and WebKit wants all three: `*://*/*` is
//! everywhere, `https://example.com/*` is a site. It has no concept of a port,
//! so `http://localhost:3000/*` matches nothing at all -- write
//! `http://localhost/*`, which covers every port on the host. A pattern WebKit
//! cannot use is reported against its file rather than left to be discovered,
//! because it does not fail loudly; it fails by never running.
//!
//! A file with no `@match` is not injected anywhere, and says so in
//! `script list`. That is deliberate: the failure mode of guessing "everywhere"
//! is a broken script running on your bank, and a userscript that silently does
//! nothing is a much better accident than one that silently runs everywhere.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// One file in the scripts directory.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Script {
    /// `@name`, or the file name when it has none.
    pub name: String,
    /// The file it came from.
    pub path: String,
    /// `js` or `css`.
    pub kind: String,
    /// The URL patterns it runs on.
    pub matches: Vec<String>,
    /// The URL patterns it is kept off.
    pub excludes: Vec<String>,
    /// `document-start` or `document-end`. Always `document-start` for CSS,
    /// which is not injected at a time so much as applied.
    pub run_at: String,
    /// Why this file is not being injected, or empty when it is.
    ///
    /// A listed-but-inert script is the case worth reporting well: the file is
    /// there, the person thinks it is running, and the only evidence otherwise
    /// is that nothing happens.
    pub problem: String,
}

impl Script {
    /// Whether this one will actually be injected.
    pub fn live(&self) -> bool {
        self.problem.is_empty()
    }
}

/// Where userscripts live: beside the config file, because that is what they
/// are -- configuration that happens to be code.
///
/// Per profile, like the filter cache and the history: `--profile work` is a
/// second browser, and a script you wrote for one is not automatically wanted in
/// the other.
pub fn dir() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    crate::profile::within(config.join("oma-browse")).join("scripts")
}

/// Read every script in the directory, in a stable order.
///
/// Unreadable and unparseable files come back too, carrying their problem, so
/// that `script list` can say why a file is not doing anything rather than
/// leaving it out and looking like it was never there.
pub fn load() -> Vec<Script> {
    let dir = dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        // No directory is the normal case, not an error: almost nobody has one.
        return Vec::new();
    };

    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
                Some("js") | Some("css")
            )
        })
        .collect();
    // By name, so the order two scripts are applied in is the order they are
    // listed in and does not change between runs on directory order.
    paths.sort();

    paths.iter().map(|path| read_one(path)).collect()
}

fn read_one(path: &Path) -> Script {
    let kind = match path.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case("css") => "css",
        _ => "js",
    };
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut script = Script {
        name: file_name.clone(),
        path: path.display().to_string(),
        kind: kind.to_string(),
        matches: Vec::new(),
        excludes: Vec::new(),
        run_at: "document-end".to_string(),
        problem: String::new(),
    };

    let Ok(source) = std::fs::read_to_string(path) else {
        script.problem = "could not be read".to_string();
        return script;
    };

    parse_header(&source, &mut script);
    if script.kind == "css" {
        // A stylesheet is applied for the life of the document; the concept
        // does not apply, and reporting `document-end` for one would be a
        // number that means nothing.
        script.run_at = "document-start".to_string();
    }
    if script.matches.is_empty() {
        script.problem =
            "no @match, so it is not injected anywhere; add one to the header".to_string();
        return script;
    }
    for pattern in script.matches.iter_mut().chain(script.excludes.iter_mut()) {
        *pattern = normalise(pattern);
    }
    if let Some((pattern, why)) = script.matches.iter().find_map(|p| Some((p, trouble(p)?))) {
        script.problem = format!("`{pattern}` {why}");
    }
    script
}

/// The spellings everybody writes for "everywhere", in the one WebKit takes.
///
/// `*` is what Tampermonkey accepts and `<all_urls>` is what Chrome does.
/// WebKit's matcher takes neither, and answers by matching nothing. Both mean
/// exactly `*://*/*`, which it does take, so they are rewritten rather than
/// refused -- a person who wrote either meant something unambiguous.
fn normalise(pattern: &str) -> String {
    match pattern.trim() {
        "*" | "<all_urls>" => "*://*/*".to_string(),
        other => other.to_string(),
    }
}

/// Why WebKit will not match on this pattern, if it will not.
///
/// Every case here is one WebKit *accepts* and then silently never matches,
/// which is the worst way for it to fail: the file exists, `script list` shows
/// it, and the only evidence of trouble is that nothing happens. So each is
/// caught at read time and reported against the file it came from.
///
/// Found by putting patterns in front of a running engine rather than by
/// reading a document -- `http://host` and `host/*` and `<all_urls>` all look
/// reasonable, and all match nothing.
fn trouble(pattern: &str) -> Option<&'static str> {
    let Some((_, rest)) = pattern.split_once("://") else {
        return Some("names no scheme, and WebKit matches on one; put `*://` in front of the host");
    };
    // The host is what sits between `://` and the next `/`.
    let Some((host, _)) = rest.split_once('/') else {
        return Some("names no path, and WebKit matches on one too; end it with `/*`");
    };
    // A `:` in the path is ordinary -- a URL can contain one -- so only the host
    // span is looked at.
    if host.contains(':') {
        return Some(
            "names a port, which WebKit's matcher has no concept of, so it matches nothing; \
             drop the port and it applies to every port on that host",
        );
    }
    None
}

/// Pull `@name`, `@match`, `@exclude` and `@run-at` out of a file's header.
///
/// Deliberately forgiving about the wrapper and strict about the keys. A `.js`
/// file writes `// @match`, a `.css` file writes the same line inside a `/* */`
/// block, and Greasemonkey's `==UserScript==` fences are optional -- they are
/// what everybody's existing files have, so they are accepted, but a two-line
/// header without them is not worth rejecting.
///
/// Only the top of the file is read. A `@match` three hundred lines down is
/// somebody's string constant, not a header.
fn parse_header(source: &str, script: &mut Script) {
    for line in source.lines().take(40) {
        let line = line.trim();
        let line = line
            .trim_start_matches("/*")
            .trim_end_matches("*/")
            .trim()
            .trim_start_matches("//")
            .trim_start_matches('*')
            .trim();
        let Some(rest) = line.strip_prefix('@') else { continue };
        let (key, value) = match rest.split_once(char::is_whitespace) {
            Some((key, value)) => (key.to_ascii_lowercase(), value.trim()),
            None => continue,
        };
        if value.is_empty() {
            continue;
        }
        match key.as_str() {
            "name" => script.name = value.to_string(),
            "match" | "include" => script.matches.push(value.to_string()),
            "exclude" | "exclude-match" => script.excludes.push(value.to_string()),
            "run-at" => {
                script.run_at = match value {
                    "document-start" | "document_start" => "document-start".to_string(),
                    _ => "document-end".to_string(),
                }
            }
            _ => {}
        }
    }
}

/// Hand every live script to a webview's content manager.
#[cfg(target_os = "linux")]
pub fn install<R: tauri::Runtime>(
    view: &tauri::webview::Webview<R>,
    scripts: Vec<(Script, String)>,
) -> anyhow::Result<()> {
    use anyhow::Context as _;

    if scripts.is_empty() {
        return Ok(());
    }

    view.with_webview(move |platform| {
        use webkit2gtk::{
            UserContentInjectedFrames, UserContentManagerExt, UserScript, UserScriptInjectionTime,
            UserStyleLevel, UserStyleSheet, WebViewExt,
        };

        let Some(manager) = platform.inner().user_content_manager() else {
            tracing::warn!("no user content manager; this tab runs no scripts");
            return;
        };

        for (script, source) in &scripts {
            let allow: Vec<&str> = script.matches.iter().map(String::as_str).collect();
            let block: Vec<&str> = script.excludes.iter().map(String::as_str).collect();

            if script.kind == "css" {
                // `User` rather than `Author`: a userscript stylesheet exists to
                // overrule the site, which is the whole point of writing one,
                // and an author-level sheet loses to the page's own specificity.
                manager.add_style_sheet(&UserStyleSheet::new(
                    source,
                    UserContentInjectedFrames::TopFrame,
                    UserStyleLevel::User,
                    &allow,
                    &block,
                ));
            } else {
                let when = if script.run_at == "document-start" {
                    UserScriptInjectionTime::Start
                } else {
                    UserScriptInjectionTime::End
                };
                // Top frame only. Every userscript manager defaults this way:
                // running someone's page fix inside every advertising iframe is
                // both surprising and, on a page with forty of them, slow.
                manager.add_script(&UserScript::new(
                    source,
                    UserContentInjectedFrames::TopFrame,
                    when,
                    &allow,
                    &block,
                ));
            }
        }
    })
    .context("could not reach the webview to install its scripts")
}

#[cfg(not(target_os = "linux"))]
pub fn install<R: tauri::Runtime>(
    _view: &tauri::webview::Webview<R>,
    _scripts: Vec<(Script, String)>,
) -> anyhow::Result<()> {
    Ok(())
}

/// The live scripts, paired with their source, ready for [`install`].
///
/// Read per tab rather than once at startup, which is also how a change to a
/// file takes effect: the next tab you open has it. There is no `script reload`
/// on purpose. Reloading would mean `remove_all_scripts`, and this browser's own
/// injections -- the theme, and wry's IPC bridge -- are user scripts on the same
/// content manager, so removing "all" of them takes the browser with it.
pub fn loaded() -> Vec<(Script, String)> {
    load()
        .into_iter()
        .filter(Script::live)
        .filter_map(|script| {
            let source = std::fs::read_to_string(&script.path).ok()?;
            Some((script, source))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str, kind: &str) -> Script {
        let mut script = Script {
            name: "f".into(),
            path: "f".into(),
            kind: kind.into(),
            matches: Vec::new(),
            excludes: Vec::new(),
            run_at: "document-end".into(),
            problem: String::new(),
        };
        parse_header(source, &mut script);
        script
    }

    /// The whole point of [`trouble`]: WebKit accepts each of these and then
    /// matches nothing with it, so the file has to be listed as inert or nobody
    /// ever finds out why their script does not run.
    #[test]
    fn the_patterns_webkit_takes_and_ignores_are_reported() {
        // A port is the obvious thing to write for the local dev server this
        // browser is pointed at all day, and the symptom is silence.
        assert!(trouble("http://localhost:3000/*").is_some_and(|w| w.contains("port")));
        assert!(trouble("*://localhost:*/*").is_some_and(|w| w.contains("port")));
        assert!(trouble("example.com/*").is_some_and(|w| w.contains("scheme")));
        assert!(trouble("https://example.com").is_some_and(|w| w.contains("path")));
    }

    #[test]
    fn an_ordinary_pattern_has_no_trouble() {
        assert!(trouble("https://example.com/*").is_none());
        assert!(trouble("*://localhost/*").is_none());
        assert!(trouble("*://*/*").is_none());
        assert!(trouble("https://example.com/a/b").is_none());
        // A colon in the path is ordinary and is not the host's.
        assert!(trouble("https://example.com/a:b").is_none());
    }

    /// Two spellings of "everywhere" that other managers take and WebKit does
    /// not. Rewriting beats refusing: nothing about either is ambiguous.
    #[test]
    fn everywhere_is_spelled_the_way_webkit_wants_it() {
        assert_eq!(normalise("*"), "*://*/*");
        assert_eq!(normalise("<all_urls>"), "*://*/*");
        assert!(trouble(&normalise("*")).is_none());
        assert_eq!(normalise("https://a.example/*"), "https://a.example/*");
    }

    #[test]
    fn a_greasemonkey_header_is_read() {
        let s = parse(
            "// ==UserScript==\n// @name    Wider\n// @match   https://a.example/*\n\
             // @exclude https://a.example/admin/*\n// @run-at  document-start\n\
             // ==/UserScript==\nconsole.log(1)",
            "js",
        );
        assert_eq!(s.name, "Wider");
        assert_eq!(s.matches, ["https://a.example/*"]);
        assert_eq!(s.excludes, ["https://a.example/admin/*"]);
        assert_eq!(s.run_at, "document-start");
    }

    /// A stylesheet cannot write `//` comments, so the same keys have to work
    /// inside a block comment.
    #[test]
    fn a_css_header_is_read_out_of_a_block_comment() {
        let s = parse(
            "/* @name Wider */\n/* @match https://a.example/* */\nbody { color: red }",
            "css",
        );
        assert_eq!(s.name, "Wider");
        assert_eq!(s.matches, ["https://a.example/*"]);
    }

    #[test]
    fn several_matches_all_count() {
        let s = parse("// @match https://a.example/*\n// @match https://b.example/*", "js");
        assert_eq!(s.matches.len(), 2);
    }

    /// The one that matters: a file with no `@match` must not become a script
    /// that runs everywhere.
    #[test]
    fn a_file_with_no_match_is_inert_and_says_why() {
        let dir = std::env::temp_dir().join(format!("oma-scripts-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("nomatch.js");
        std::fs::write(&path, "console.log('hello')").expect("write the fixture");

        let script = read_one(&path);
        assert!(script.matches.is_empty());
        assert!(!script.live(), "a script with no @match must not be injected");
        assert!(script.problem.contains("@match"), "{}", script.problem);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_run_at_we_do_not_know_falls_back_to_document_end() {
        let s = parse("// @match https://a.example/*\n// @run-at whenever", "js");
        assert_eq!(s.run_at, "document-end");
    }

    /// `@match` far down a file is somebody's string, not a header.
    #[test]
    fn only_the_top_of_the_file_is_a_header() {
        let mut source = String::from("// @match https://real.example/*\n");
        source.push_str(&"const x = 1;\n".repeat(60));
        source.push_str("// @match https://sneaky.example/*\n");
        let s = parse(&source, "js");
        assert_eq!(s.matches, ["https://real.example/*"], "a line 60 deep is not a header");
    }

    #[test]
    fn a_bare_at_sign_with_no_value_is_not_a_key() {
        let s = parse("// @match\n// @match https://a.example/*", "js");
        assert_eq!(s.matches, ["https://a.example/*"]);
    }
}
