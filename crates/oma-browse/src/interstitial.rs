//! The page a tab shows when there is no page to show.
//!
//! Four things can leave a content webview with nothing in it: a certificate
//! that did not check out, a site asking for a password, a web process that
//! died, and a request that never arrived. The first two are served from the
//! chrome scheme as ordinary pages, because they are reached by navigating and
//! answered by a command. The last two cannot be: they have to appear *at the
//! URL that failed*, which means [`webkit2gtk::WebViewExt::load_alternate_html`]
//! and markup built here rather than a route.
//!
//! What they must not do is look like four different browsers. This is the one
//! place the shape is decided -- the tag, the host, the sentence, the way out,
//! the URL in small type at the bottom -- and it is deliberately the same shape
//! as the certificate page in [`crate::ui`].

use std::sync::Arc;

use crate::state::AppState;

/// One failure, ready to render.
pub struct Interstitial<'a> {
    /// The small red word above the heading: "crashed", "not reached".
    pub tag: &'a str,
    /// The document title, which is also what the strip will show for the tab.
    pub title: &'a str,
    /// What follows the host, completing the sentence it starts.
    pub sub: &'a str,
    /// The engine's own words, if they add anything: "Name or service not
    /// known", "Connection refused".
    ///
    /// Kept separate from `sub` and set in smaller type, because it is the one
    /// line here that was not written for a person to read -- but it is also the
    /// only line that says *which* failure it was, and dropping it would be
    /// throwing away the diagnosis to keep the page tidy.
    pub detail: Option<&'a str>,
    /// What to do about it.
    ///
    /// Markup, not text: this is the one field that carries `<code>` and
    /// `<kbd>`, and every value of it is written in this crate. Nothing that
    /// came off the network reaches it -- see [`escape`], which is what handles
    /// everything that did.
    pub hint: &'a str,
    /// The address that failed, shown in full at the bottom and reduced to its
    /// host for the heading.
    pub uri: &'a str,
}

impl Interstitial<'_> {
    /// Render, in the theme the browser is wearing.
    pub fn render(&self, state: &Arc<AppState>) -> String {
        // `try_read`, not `read`: every caller is inside a WebKit signal on the
        // GTK main thread, where there is no runtime to await on. The lock is
        // only ever taken to swap themes, so failing here means the theme is
        // changing in this exact millisecond; the page then renders on the
        // fallback below, which is readable in any theme rather than pretty in
        // one.
        let (vars, mine) = match state.theme.try_read() {
            Ok(theme) => (
                theme.css.chrome.clone(),
                crate::strip::chrome_vars(&state.config, theme.css.opacity),
            ),
            Err(_) => (FALLBACK_VARS.to_string(), String::new()),
        };

        format!(
            "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
             <title>{title}</title>\
             <style>{vars}</style><style>{mine}</style><style>{CSS}</style>\
             </head><body><main>\
             <p class=\"tag\">{tag}</p>\
             <h1>{host}</h1>\
             <p class=\"sub\">{sub}</p>\
             {detail}\
             <p class=\"hint\">{hint}</p>\
             <p class=\"uri\">{shown}</p>\
             </main></body></html>",
            title = escape(self.title),
            tag = escape(self.tag),
            host = escape(&host_of(self.uri)),
            sub = escape(self.sub),
            detail = match self.detail {
                Some(detail) => format!("<p class=\"why\">{}</p>", escape(detail)),
                None => String::new(),
            },
            hint = self.hint,
            shown = escape(self.uri),
        )
    }
}

/// The host, for the heading.
///
/// A whole URL in 2rem type wraps to three lines and says nothing the line of
/// small type underneath does not.
pub fn host_of(uri: &str) -> String {
    match url::Url::parse(uri) {
        Ok(parsed) if !parsed.host_str().unwrap_or_default().is_empty() => {
            parsed.host_str().unwrap_or_default().to_string()
        }
        // A tab that failed before it had a URL at all, or on something that is
        // not one -- `file:` has no host, and neither does a typo. "This page"
        // is vague but true, and better than an empty heading.
        _ => "This page".to_string(),
    }
}

/// Text into markup.
///
/// Everything variable on these pages came off the network -- a URL that
/// failed, a host that a page chose to navigate to -- and is written into a
/// document by hand rather than by a template engine. Quotes included: the
/// point of doing it by hand carefully is not to have to reason about which
/// contexts a value can reach.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Enough of the theme to read the page by, for the moment the theme is being
/// swapped underneath us. Omarchy's own defaults, so it is not jarring even
/// when it is wrong.
const FALLBACK_VARS: &str = ":root { --oma-veil: #1a1b26; --oma-fg: #c0caf5; \
     --oma-muted: #565f89; --oma-accent: #7aa2f7; --oma-color-red: #f7768e; \
     --oma-space: 8px; --oma-space-2: 16px; --oma-font-small: 0.85rem; \
     --oma-font-mono: monospace; --oma-control-normal-border: #414868; }";

/// Deliberately the same stylesheet as the certificate page's. A crash, a
/// refused certificate and a host that does not resolve are the same kind of
/// event to whoever is looking at them, and they should not look like three
/// different browsers.
const CSS: &str = r#"
* { box-sizing: border-box; }
html, body { margin: 0; height: 100%; background: var(--oma-veil); color: var(--oma-fg);
  font-family: system-ui, sans-serif; }
main { height: 100%; display: flex; flex-direction: column; align-items: center;
  justify-content: center; gap: var(--oma-space); padding: var(--oma-space-2);
  max-width: 46rem; margin: 0 auto; text-align: center; }
.tag { margin: 0; text-transform: uppercase; letter-spacing: 0.16em;
  font-size: var(--oma-font-small); color: var(--oma-color-red); }
h1 { margin: 0; font-size: 2rem; font-weight: 600; color: var(--oma-fg);
  font-family: var(--oma-font-mono); word-break: break-all; }
.sub { margin: 0; color: var(--oma-fg); }
.why { margin: 0; color: var(--oma-muted); font-family: var(--oma-font-mono);
  font-size: var(--oma-font-small); word-break: break-word; }
.hint { margin: var(--oma-space) 0 0; color: var(--oma-fg); line-height: 1.6; }
code, kbd { font-family: var(--oma-font-mono); color: var(--oma-accent);
  border: 1px solid var(--oma-control-normal-border); padding: 0 6px; }
.uri { margin: var(--oma-space-2) 0 0; color: var(--oma-muted);
  font-family: var(--oma-font-mono); font-size: var(--oma-font-small);
  word-break: break-all; opacity: 0.7; }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_becomes_a_host() {
        assert_eq!(host_of("https://youtube.com/watch?v=1"), "youtube.com");
        assert_eq!(host_of("http://127.0.0.1:8731/index.html"), "127.0.0.1");
    }

    #[test]
    fn something_that_is_not_a_url_still_gets_a_heading() {
        assert_eq!(host_of(""), "This page");
        assert_eq!(host_of("not a url"), "This page");
        // Parses, but has no host to show.
        assert_eq!(host_of("file:///etc/hosts"), "This page");
    }

    /// The URL on these pages came off the network, and it is written into
    /// markup by hand.
    #[test]
    fn the_url_cannot_carry_markup_onto_the_page() {
        let nasty = "https://x.test/?q=<script>alert('x')</script>&a=\"b\"";
        let escaped = escape(nasty);
        assert!(!escaped.contains('<'), "{escaped}");
        assert!(!escaped.contains('>'), "{escaped}");
        assert!(!escaped.contains('"'), "{escaped}");
        assert!(escaped.contains("&lt;script&gt;"), "{escaped}");
        // And the ampersand that was already there is not left half-escaped.
        assert!(escaped.contains("&amp;a="), "{escaped}");
    }

    /// A host is echoed into the heading, and a page chooses what it navigates
    /// to -- so the heading is as much of an injection point as the URL.
    #[test]
    fn a_host_cannot_carry_markup_onto_the_page() {
        let escaped = escape(&host_of("https://x.test/"));
        assert_eq!(escaped, "x.test");
        assert_eq!(escape("<b>evil</b>.test"), "&lt;b&gt;evil&lt;/b&gt;.test");
    }

    #[test]
    fn escaping_leaves_an_ordinary_url_alone() {
        let plain = "https://omarchy.org/docs/getting-started";
        assert_eq!(escape(plain), plain);
    }
}
