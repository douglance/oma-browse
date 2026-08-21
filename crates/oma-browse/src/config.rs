//! The config file: every setting the browser has, in one dotfile.
//!
//! `~/.config/oma-browse/config.toml`, which is where the rest of an Omarchy
//! desktop keeps its dotfiles -- `~/.config/hypr`, `~/.config/omarchy` -- so
//! this one travels with them in the same dotfiles repository.
//!
//! Sections are named for the *surface* they affect rather than for the module
//! that reads them, so "where do I change this" has one answer: `[chrome]` is
//! our own interface, `[theme]` is what loaded websites get, `[engine]` is
//! WebKit itself.
//!
//! Everything is optional and everything has a default, so a missing file is
//! not a problem and neither is a file with one line in it. A file that does
//! not parse *is* a problem, but not a fatal one: the browser says so, on the
//! log and in `config show`, and runs on its defaults rather than refusing to
//! start over a stray comma.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where the browser goes when it is not told otherwise.
///
/// Where the browser goes when it is not told otherwise.
///
/// A remote page does put a round trip in front of a bare launch, and the
/// browser's own start page (`home = ""`) is served from this process with no
/// DNS, no TLS and no network at all. This is a product decision rather than a
/// performance one, and it is one line to change in the file.
const DEFAULT_HOME: &str = "https://omarchy.org";

/// The search engine a non-URL lands on. `{query}` is the URL-encoded input;
/// without it the query is appended.
const DEFAULT_SEARCH: &str = "https://duckduckgo.com/?q={query}";

// ---------------------------------------------------------------------------
// Veil
// ---------------------------------------------------------------------------

/// How see-through a surface is: a number, or `"auto"`.
///
/// `"auto"` is not a synonym for 1.0. For pages it means "solve for contrast
/// against the wallpaper", which is a judgement the browser makes and can
/// remake when the wallpaper changes; a number takes that decision away from it
/// permanently. The two are different enough to be worth spelling differently.
#[derive(Debug, Clone, Copy, PartialEq, schemars::JsonSchema)]
#[schemars(with = "String", description = "`\"auto\"`, or an opacity from 0.0 to 1.0")]
pub struct Veil(Option<f64>);

impl Veil {
    pub const AUTO: Veil = Veil(None);
    pub const OPAQUE: Veil = Veil(Some(1.0));

    /// The opacity to actually paint with, given what `auto` would work out to.
    pub fn resolve(self, auto: f64) -> f64 {
        self.0.unwrap_or(auto).clamp(0.0, 1.0)
    }

    /// The pinned value, if this is not `auto`.
    pub fn fixed(self) -> Option<f64> {
        self.0
    }
}

impl Serialize for Veil {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Some(v) => s.serialize_f64(v),
            None => s.serialize_str("auto"),
        }
    }
}

impl<'de> Deserialize<'de> for Veil {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        // Both spellings, because a config file is written by hand: `veil =
        // 0.85` and `veil = "auto"` are the two things anyone would type.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(f64),
            Word(String),
        }

        match Raw::deserialize(d)? {
            Raw::Number(v) => Ok(Veil(Some(v.clamp(0.0, 1.0)))),
            Raw::Word(w) if w.eq_ignore_ascii_case("auto") || w.trim().is_empty() => Ok(Veil::AUTO),
            Raw::Word(w) => match w.trim().parse::<f64>() {
                Ok(v) => Ok(Veil(Some(v.clamp(0.0, 1.0)))),
                Err(_) => Err(D::Error::custom(format!(
                    "expected a number from 0.0 to 1.0, or \"auto\", not {w:?}"
                ))),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// The file
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The page a bare `oma-browse` opens, and where `nav home` goes. Empty
    /// means the browser's own start page.
    pub home: String,
    /// Where anything that is not a URL gets searched.
    pub search: String,

    pub chrome: Chrome,
    pub theme: Theme,
    pub window: Window,
    pub engine: Engine,
    pub control: Control,
    pub startup: Startup,
    pub history: History,
    pub downloads: Downloads,
    pub screenshot: Screenshot,
    pub tabs: Tabs,
    pub content: Content,

    /// Chord to command, overriding the built-in table. An empty command
    /// unbinds the chord: `"ctrl+d" = ""`.
    pub keys: BTreeMap<String, String>,

    /// Why the file was ignored, when it was. Never read from the file itself.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// Content blocking.
///
/// WebKitGTK ships Safari's content-blocker compiler, so this costs nothing at
/// request time and makes pages faster rather than slower -- see
/// [`crate::blocker`]. No list is shipped: a browser that decides for you what
/// the web may not send you is a browser with an opinion you did not ask for,
/// and the README names one you can install in a line.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Content {
    /// Block, using the rule lists below. Off with no lists is the same thing
    /// as off, so this exists to turn blocking off again without deleting the
    /// paths you spent an afternoon collecting.
    pub block: bool,
    /// Rule lists, as paths to Safari-format JSON on disk. `~` is expanded.
    ///
    /// Paths, not URLs: fetching one would mean a TLS stack in this binary to
    /// do a job `curl` already does, once, before the browser starts.
    pub rules: Vec<String>,
}

/// The browser's own interface: the palette and the tab strip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Chrome {
    /// How see-through the palette card is. Opaque by default, deliberately:
    /// it is a card you read dense text off, and it has no reason to inherit
    /// the page's translucency. The strip has no surface of its own either way.
    pub veil: Veil,
    /// The font stack for our own surfaces. Nerd Font first, because the strip
    /// and the palette draw glyphs from the patched range.
    pub font: String,
    /// Skip the GTK overlay surgery: tabs tile instead of stacking, and there
    /// is no floating palette. An escape hatch for bisecting rendering
    /// problems, and the config version of `OMA_LAYOUT=plain`.
    pub plain_layout: bool,
    pub palette: Palette,
    pub strip: Strip,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Palette {
    /// Share of the window's width the card takes, before the bounds below.
    pub width_ratio: f64,
    /// Narrowest it gets. Below this the two-line rows start wrapping.
    pub min_width: i32,
    /// Widest it gets. A launcher stretched across an ultrawide is a bar.
    pub max_width: i32,
    pub height: i32,
    pub min_height: i32,
    pub top_margin: i32,
    pub side_margin: i32,
    pub bottom_margin: i32,
    /// How many results a search shows.
    pub rows: usize,
    /// How many recently-visited pages the idle menu lists.
    pub history_rows: usize,
    /// How many downloaded files it lists there. Short on purpose: this is
    /// "open the thing I just saved"; `download list` is the whole list.
    pub download_rows: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Strip {
    /// The floating row of favicons at the top of the window.
    pub enabled: bool,
    /// Its height in pixels, which is also the top inset given to every page --
    /// turning the strip off gives that inset back.
    pub height: i32,
    /// Show the active tab's title in the middle of the strip.
    pub title: bool,
    /// How long to wait before redrawing after the tab model changes. One page
    /// load fires a URL change, a title change and a favicon within
    /// milliseconds; this is what stops that being three redraws.
    pub debounce_ms: u64,
    /// Draw a line along the bottom of the strip while a page is loading.
    /// WebKit's own estimate, pushed in as it changes rather than polled, so an
    /// idle browser runs nothing for it at all.
    pub progress: bool,
}

/// What loaded websites get.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    /// How see-through a page is. `"auto"` solves for contrast against the
    /// wallpaper and follows Ghostty's `background-opacity`; a number pins it
    /// whatever the wallpaper does. `OMA_VEIL` still wins over both.
    pub veil: Veil,
    /// Repaint neutral page surfaces onto the theme's ramp. Only greyscale
    /// surfaces are touched, so brand colour survives.
    pub recolor: bool,
    /// Give up on repainting a page with more CSS than this, and leave it in its
    /// own colours.
    ///
    /// Recolouring derives its overrides from every rule in the document, so
    /// what it costs is set by how much CSS a site ships -- and a handful of
    /// sites ship an enormous amount. Measured: youtube.com carries 22,000 to
    /// 27,000 rules and pays about two seconds a load for them, on a page that
    /// is already dark and gains very little from being repainted.
    ///
    /// The default sits between the two ends of what real sites measure --
    /// github.com is the heaviest ordinary page found at 10,600 rules, and it is
    /// a light theme, which is where repainting earns the most. `0` means no
    /// limit.
    pub recolor_max_rules: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Window {
    pub width: f64,
    pub height: f64,
    /// A GTK titlebar. Off, because Omarchy tiles everything and paints its own
    /// border, so it would be wasted rows.
    pub decorations: bool,
    /// The window title, which is what a tiling WM labels it by.
    pub title: String,
}

/// WebKit itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Engine {
    pub javascript: bool,
    /// Right-click, Inspect Element.
    pub devtools: bool,
    /// Empty means WebKit's own.
    pub user_agent: String,
    /// Let media start without a click.
    pub autoplay: bool,
    pub webrtc: bool,
    pub webgl: bool,
    pub smooth_scrolling: bool,
    /// The default font size a page gets, in pixels.
    pub font_size: u32,
    /// `always`, `never`, or `no-third-party`.
    pub cookies: String,
    /// Hosts whose certificate is accepted whatever is wrong with it.
    ///
    /// For development, where a self-signed certificate is the point and being
    /// asked about it every morning is friction with no security in it. Empty
    /// by default, and it should stay empty for anything you did not issue the
    /// certificate for yourself.
    pub trust: Vec<String>,
    /// Send every request through this proxy, e.g. `http://127.0.0.1:8080`.
    ///
    /// Empty is the system's own setting, which is what WebKit does with no
    /// help. The point of naming one here is mitmproxy or Burp: a browser you
    /// can put in front of a proxy without touching the environment it was
    /// launched from.
    pub proxy: String,
    /// Underline misspelt words in text boxes.
    ///
    /// Off, and not because WebKit is off: WebKit's default is on, and a red
    /// squiggle under every identifier in every code review is not what a
    /// browser for engineers should do without being asked. The palette's own
    /// input has opted out since the day it was written.
    pub spellcheck: bool,
    /// Languages to spell-check against, e.g. `["en_GB"]`. Empty follows
    /// `$LANG`, which is what WebKit does on its own.
    pub spellcheck_languages: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Control {
    /// Where this window's control socket goes. Empty is
    /// `$XDG_RUNTIME_DIR/oma-browse`, which is where anything driving the
    /// browser should look for it.
    pub socket: String,
    /// Also answer commands on this loopback port. `0` is off, and off is the
    /// default: a port is reachable by every process and every account on the
    /// machine, where the socket is reachable by you. Turn it on deliberately,
    /// for tooling that cannot open a Unix socket.
    pub remote_port: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Startup {
    /// Start every window incognito, as though `--incognito` were always passed.
    pub incognito: bool,
    /// Reopen the tabs that were open when the browser last closed.
    ///
    /// Off by default, and deliberately: a browser that reopens eleven tabs
    /// because you asked it to open one is a browser you stop launching from a
    /// keybind. `tab restore` does it on demand either way.
    pub restore: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct History {
    /// Remember where the browser has been. `false` is `--incognito`'s effect
    /// on history, permanently, without its effect on anything else.
    pub enabled: bool,
    /// Beyond this the oldest entries are dropped.
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Downloads {
    /// Where files land. Empty follows the XDG user directory, which is what
    /// every other application on the desktop uses. `~` is expanded.
    pub dir: String,
    /// Say so, on the desktop, when a download finishes.
    pub notify: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Screenshot {
    /// Where `page screenshot` writes when told nowhere. Empty is
    /// `$XDG_RUNTIME_DIR/oma-browse`: per-boot, user-private, and cleaned up by
    /// the system rather than accumulating in your home directory.
    pub dir: String,
    /// Capture the whole scrollable document rather than the viewport.
    pub full: bool,
    /// Keep the page's transparency instead of compositing onto white.
    pub transparent: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Tabs {
    /// How many closed tabs `Ctrl-Shift-T` can walk back through.
    pub reopen_depth: usize,
    /// The zoom level a new tab starts at.
    pub zoom: f64,
    /// The ladder `Ctrl-+` and `Ctrl--` step along.
    pub zoom_steps: Vec<f64>,
    /// The size favicons are stored at. Sites do ship 512px icons, and one of
    /// those is 40KB of base64 re-sent on every strip redraw.
    pub favicon_size: i32,
}

// ---------------------------------------------------------------------------
// Defaults -- each one is today's hardcoded value, named where it came from
// ---------------------------------------------------------------------------

impl Default for Config {
    fn default() -> Self {
        Self {
            home: DEFAULT_HOME.to_string(),
            search: DEFAULT_SEARCH.to_string(),
            chrome: Chrome::default(),
            theme: Theme::default(),
            window: Window::default(),
            engine: Engine::default(),
            control: Control::default(),
            startup: Startup::default(),
            history: History::default(),
            downloads: Downloads::default(),
            screenshot: Screenshot::default(),
            tabs: Tabs::default(),
            content: Content::default(),
            keys: BTreeMap::new(),
            problem: None,
        }
    }
}

impl Default for Chrome {
    fn default() -> Self {
        Self {
            veil: Veil::OPAQUE,
            font: crate::strip::DEFAULT_FONT.to_string(),
            plain_layout: false,
            palette: Palette::default(),
            strip: Strip::default(),
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            width_ratio: 0.5,
            min_width: 420,
            max_width: 900,
            height: 420,
            min_height: 200,
            top_margin: 72,
            side_margin: 24,
            bottom_margin: 72,
            rows: 24,
            history_rows: 8,
            download_rows: 5,
        }
    }
}

impl Default for Strip {
    fn default() -> Self {
        Self {
            enabled: true,
            height: crate::layout::STRIP_HEIGHT,
            title: true,
            debounce_ms: 80,
            progress: true,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self { veil: Veil::AUTO, recolor: true, recolor_max_rules: 15_000 }
    }
}

impl Default for Window {
    fn default() -> Self {
        Self { width: 1400.0, height: 900.0, decorations: false, title: "oma-browse".to_string() }
    }
}

impl Default for Engine {
    fn default() -> Self {
        // WebKit's own defaults, restated, so that turning one off in the file
        // is a change from what the browser was already doing.
        Self {
            javascript: true,
            devtools: false,
            user_agent: String::new(),
            autoplay: true,
            webrtc: true,
            webgl: true,
            smooth_scrolling: true,
            font_size: 16,
            cookies: "no-third-party".to_string(),
            // Not a WebKit default: WebKit trusts nothing extra, and so does
            // this until someone names a host.
            trust: Vec::new(),
            proxy: String::new(),
            // See the field: a deliberate departure from WebKit's default, and
            // the same call the palette's own input already makes.
            spellcheck: false,
            spellcheck_languages: Vec::new(),
        }
    }
}

impl Default for History {
    fn default() -> Self {
        Self { enabled: true, limit: crate::history::CAP }
    }
}

impl Default for Downloads {
    fn default() -> Self {
        Self { dir: String::new(), notify: true }
    }
}

impl Default for Tabs {
    fn default() -> Self {
        Self {
            reopen_depth: crate::tabs::CLOSED_STACK,
            zoom: 1.0,
            zoom_steps: crate::tabs::ZOOM_STEPS.to_vec(),
            favicon_size: 32,
        }
    }
}

// ---------------------------------------------------------------------------
// Reading and writing it
// ---------------------------------------------------------------------------

impl Config {
    /// Read the file, or fall back to the defaults.
    ///
    /// Never fails. A browser that will not start because its config file has a
    /// typo in it is a browser you cannot open the config file with.
    pub fn load() -> Self {
        let path = Self::path();
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Self::default();
        };

        // The happy path is strict: `deny_unknown_fields` is what turns a
        // misspelled key from silence into a message.
        // Salvage what parses rather than reverting the file wholesale -- one
        // key the browser has since renamed should cost the user that key, not
        // every setting they ever wrote. Taking the error out of the same parse
        // that failed, rather than parsing twice to get at it.
        let strict = match toml::from_str::<Config>(&raw) {
            Ok(config) => return config,
            Err(e) => e.to_string(),
        };
        let mut notes = Vec::new();
        let salvaged = toml::from_str::<toml::Value>(&raw)
            .ok()
            .map(|mut value| {
                migrate(&mut value, &mut notes);
                prune_unknown(&mut value, &known_shape(), "", &mut notes);
                value
            })
            .and_then(|value| value.try_into::<Config>().ok());

        match salvaged {
            Some(mut config) => {
                let problem = format!("{}: {}", path.display(), notes.join("; "));
                tracing::warn!(%problem, "config partially applied");
                config.problem = Some(problem);
                config
            }
            // Nothing could be recovered -- a syntax error, or a value of the
            // wrong type. Say what that costs, because "using defaults" is
            // otherwise indistinguishable from having no config file at all.
            None => {
                let problem = format!(
                    "{}: {} -- every setting in the file was ignored and the browser is \
                     running on defaults",
                    path.display(),
                    summarize(&strict)
                );
                tracing::warn!(%problem, "config ignored; using defaults");
                Self { problem: Some(problem), ..Self::default() }
            }
        }
    }

    /// Where the file lives. `$OMA_BROWSE_CONFIG` wins, for a second profile or
    /// a test that must not read the real one.
    pub fn path() -> PathBuf {
        if let Some(explicit) = std::env::var_os("OMA_BROWSE_CONFIG") {
            return PathBuf::from(explicit);
        }
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        // `--profile work` reads `oma-browse/profiles/work.toml`. A profile
        // that shared the default profile's config could not have its own home
        // page, which is half of what people name a profile for.
        let dir = config.join("oma-browse");
        match crate::profile::name() {
            Some(profile) => {
                dir.join("profiles").join(format!("{}.toml", crate::profile::sanitize(profile)))
            }
            None => dir.join("config.toml"),
        }
    }

    /// The page to open at startup, as typed: a bare host or a search both work
    /// here, exactly as they do in the palette.
    pub fn home_url(&self, fallback: &str) -> String {
        if self.home.trim().is_empty() {
            return fallback.to_string();
        }
        crate::tabs::resolve_input(&self.home, &self.search)
    }

    /// Write a commented file with every setting at its default.
    ///
    /// The defaults are written out rather than left implicit because a config
    /// file you cannot read the options out of is a config file you have to
    /// read the source to use.
    pub fn write_template(force: bool) -> anyhow::Result<PathBuf> {
        use anyhow::Context as _;

        let path = Self::path();
        if path.exists() && !force {
            anyhow::bail!("{} already exists; pass --force to overwrite it", path.display());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        std::fs::write(&path, TEMPLATE)
            .with_context(|| format!("could not write {}", path.display()))?;
        Ok(path)
    }
}

// ---------------------------------------------------------------------------
// Salvaging a file the browser has since outgrown
// ---------------------------------------------------------------------------

/// Sections and keys that have moved, and where they went.
///
/// Renames are the browser's fault, not the user's, so an old spelling is
/// *applied* and then reported -- the file keeps working while it is out of
/// date, and the report says what to change. Dotted paths; a table on the left
/// merges into the table on the right.
const RENAMES: &[(&str, &str)] = &[
    // Translucency was one half-wired `[window] opacity` before it became two
    // deliberate settings; the page is the half it used to almost control.
    ("window.opacity", "theme.veil"),
    ("page", "theme"),
    ("strip", "chrome.strip"),
];

/// Tables whose keys are the user's own words rather than ours, and so must
/// never be pruned for being "unknown".
const FREE_FORM: &[&str] = &["keys"];

/// Move anything written under an old name to its new one.
fn migrate(value: &mut toml::Value, notes: &mut Vec<String>) {
    for (from, to) in RENAMES {
        let Some(moved) = take_path(value, from) else { continue };
        match merge_at(value, to, moved) {
            true => notes.push(format!("`{from}` is now `{to}`, and was read as such")),
            false => notes
                .push(format!("`{from}` is now `{to}`, which the file also sets -- `{to}` won")),
        }
    }
}

/// Remove a dotted path, returning what was there.
fn take_path(value: &mut toml::Value, path: &str) -> Option<toml::Value> {
    let (parent, last) = split_parent(value, path)?;
    parent.remove(last)
}

/// Put a value at a dotted path, creating tables on the way. Merges when both
/// sides are tables; refuses to overwrite an existing value, and says so by
/// returning `false`, because a file that sets both spellings means the new one.
fn merge_at(value: &mut toml::Value, path: &str, incoming: toml::Value) -> bool {
    let mut cursor = value;
    let mut parts = path.split('.').peekable();

    while let Some(part) = parts.next() {
        let table = match cursor.as_table_mut() {
            Some(t) => t,
            None => return false,
        };
        if parts.peek().is_some() {
            cursor = table
                .entry(part.to_string())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            continue;
        }

        return match (table.get_mut(part), incoming) {
            // Two tables: fill in only the keys the new spelling does not have.
            (Some(toml::Value::Table(existing)), toml::Value::Table(old)) => {
                let mut clean = true;
                for (k, v) in old {
                    if existing.contains_key(&k) {
                        clean = false;
                    } else {
                        existing.insert(k, v);
                    }
                }
                clean
            }
            (Some(_), _) => false,
            (None, incoming) => {
                table.insert(part.to_string(), incoming);
                true
            }
        };
    }
    false
}

fn split_parent<'a>(
    value: &'a mut toml::Value,
    path: &'a str,
) -> Option<(&'a mut toml::map::Map<String, toml::Value>, &'a str)> {
    let (head, last) = match path.rsplit_once('.') {
        Some((head, last)) => (Some(head), last),
        None => (None, path),
    };
    let mut cursor = value;
    if let Some(head) = head {
        for part in head.split('.') {
            cursor = cursor.as_table_mut()?.get_mut(part)?;
        }
    }
    Some((cursor.as_table_mut()?, last))
}

/// What a valid config looks like, as a tree of keys: the defaults, serialised.
///
/// Derived rather than listed, so it cannot drift from the struct.
fn known_shape() -> toml::Value {
    toml::Value::try_from(Config::default()).unwrap_or(toml::Value::Table(toml::map::Map::new()))
}

/// Drop keys the config does not have, noting each one.
///
/// This is what makes an unknown key cost only itself. The strict parse has
/// already failed by the time this runs, so the choice here is between losing
/// one key and losing the file.
fn prune_unknown(
    value: &mut toml::Value,
    shape: &toml::Value,
    path: &str,
    notes: &mut Vec<String>,
) {
    let (Some(table), Some(known)) = (value.as_table_mut(), shape.as_table()) else { return };

    table.retain(|key, _| {
        if known.contains_key(key) {
            return true;
        }
        let full = if path.is_empty() { key.to_string() } else { format!("{path}.{key}") };
        notes.push(format!("`{full}` is not a setting, and was ignored"));
        false
    });

    for (key, child) in table.iter_mut() {
        if FREE_FORM.contains(&key.as_str()) {
            continue;
        }
        let Some(known_child) = known.get(key.as_str()) else { continue };
        let full = if path.is_empty() { key.to_string() } else { format!("{path}.{key}") };
        prune_unknown(child, known_child, &full, notes);
    }
}

/// A `toml` error is a location, a source excerpt, and then the complaint
/// itself on the last line. The excerpt is the part nobody needs in a log line;
/// the other two are exactly what tells you which key to go and fix.
fn summarize(message: &str) -> String {
    let mut lines = message.lines().filter(|l| !l.trim().is_empty());
    let Some(first) = lines.next() else { return message.to_string() };
    match lines.next_back() {
        Some(last) if last != first => format!("{first}: {}", last.trim()),
        _ => first.to_string(),
    }
}

/// What `config init` writes. Every value here is the default, so an untouched
/// copy of this file changes nothing -- which the tests below enforce, in both
/// directions: no drifted value, and no setting missing from it.
const TEMPLATE: &str = include_str!("config.toml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(toml::from_str::<Config>("").unwrap(), Config::default());
    }

    #[test]
    fn one_line_overrides_only_that_line() {
        let config: Config = toml::from_str(r#"home = "https://example.com""#).unwrap();
        assert_eq!(config.home, "https://example.com");
        assert_eq!(config.search, Config::default().search, "everything else is untouched");
        assert!(config.chrome.strip.enabled);
    }

    #[test]
    fn a_section_keeps_the_defaults_of_its_other_keys() {
        let config: Config = toml::from_str("[chrome.strip]\nheight = 40").unwrap();
        assert_eq!(config.chrome.strip.height, 40);
        assert!(config.chrome.strip.enabled, "a section is not an all-or-nothing block");
    }

    #[test]
    fn a_misspelled_key_is_an_error_rather_than_silence() {
        // The whole point of `deny_unknown_fields`: `recolour` here would
        // otherwise do nothing at all, with nothing said about it.
        let err = toml::from_str::<Config>("[theme]\nrecolour = false").unwrap_err();
        assert!(err.to_string().contains("recolour"), "the message names the key: {err}");
    }

    #[test]
    fn a_reported_problem_names_the_key_and_the_line() {
        let err = toml::from_str::<Config>("[theme]\nrecolour = false").unwrap_err();
        let summary = summarize(&err.to_string());
        assert!(summary.contains("line 2"), "keeps the location: {summary}");
        assert!(summary.contains("recolour"), "keeps the complaint: {summary}");
        assert_eq!(summary.lines().count(), 1, "one line, for one log line");
    }

    #[test]
    fn the_veil_takes_a_number_or_the_word_auto() {
        #[derive(Deserialize)]
        struct Holder {
            veil: Veil,
        }
        let auto: Holder = toml::from_str(r#"veil = "auto""#).unwrap();
        assert_eq!(auto.veil, Veil::AUTO);
        assert_eq!(auto.veil.resolve(0.7), 0.7, "auto defers to what was worked out");

        let pinned: Holder = toml::from_str("veil = 0.4").unwrap();
        assert_eq!(pinned.veil.fixed(), Some(0.4));
        assert_eq!(pinned.veil.resolve(0.7), 0.4, "a number overrules it");

        // Out of range is clamped rather than refused: 1.5 plainly means opaque.
        let over: Holder = toml::from_str("veil = 1.5").unwrap();
        assert_eq!(over.veil.fixed(), Some(1.0));

        assert!(toml::from_str::<Holder>(r#"veil = "mostly""#).is_err());
    }

    /// Load a file's worth of text the way `load` does, without touching the
    /// user's real config path.
    fn salvage(raw: &str) -> Config {
        if let Ok(config) = toml::from_str::<Config>(raw) {
            return config;
        }
        let mut notes = Vec::new();
        let mut value = toml::from_str::<toml::Value>(raw).expect("valid TOML syntax");
        migrate(&mut value, &mut notes);
        prune_unknown(&mut value, &known_shape(), "", &mut notes);
        let mut config: Config = value.try_into().expect("salvageable");
        config.problem = Some(notes.join("; "));
        config
    }

    /// A section this browser renamed keeps working, and says it has moved.
    ///
    /// The regression this exists for: `[page]` became `[theme]` and every
    /// setting in every existing config file stopped being read -- silently,
    /// because the loader fell back to defaults wholesale.
    #[test]
    fn a_renamed_section_is_still_read() {
        let config = salvage("home = \"https://example.com\"\n[page]\nrecolor = false\n");

        assert!(!config.theme.recolor, "the old spelling still sets the setting");
        assert_eq!(config.home, "https://example.com", "and the rest of the file survives");

        let problem = config.problem.unwrap();
        assert!(problem.contains("`page` is now `theme`"), "names the move: {problem}");
    }

    #[test]
    fn a_renamed_section_that_moved_house_lands_in_the_right_table() {
        let config = salvage("[strip]\nheight = 44\nenabled = false\n");
        assert_eq!(config.chrome.strip.height, 44);
        assert!(!config.chrome.strip.enabled);
    }

    #[test]
    fn the_new_spelling_wins_when_a_file_has_both() {
        let config = salvage("[page]\nrecolor = false\n[theme]\nrecolor = true\n");
        assert!(config.theme.recolor, "the current spelling is the deliberate one");
        assert!(config.problem.unwrap().contains("won"));
    }

    /// One unknown key costs one unknown key.
    #[test]
    fn an_unknown_key_does_not_take_the_file_with_it() {
        let config =
            salvage("home = \"https://example.com\"\nnonsense = 1\n[theme]\nrecolor = false\n");

        assert_eq!(config.home, "https://example.com");
        assert!(!config.theme.recolor, "settings after the bad key still apply");
        assert!(config.problem.unwrap().contains("`nonsense` is not a setting"));
    }

    #[test]
    fn an_unknown_key_is_reported_with_its_full_path() {
        let config = salvage("[chrome.strip]\nheight = 30\nwidth = 9\n");
        assert_eq!(config.chrome.strip.height, 30);
        assert!(config.problem.unwrap().contains("`chrome.strip.width`"));
    }

    /// `[keys]` is the user's own vocabulary, so nothing in it is "unknown".
    #[test]
    fn keys_are_never_pruned_for_being_unrecognised() {
        let config = salvage("nonsense = 1\n[keys]\n\"ctrl+j\" = \"tab_cycle\"\n");
        assert_eq!(config.keys.get("ctrl+j").map(String::as_str), Some("tab_cycle"));
    }

    #[test]
    fn the_template_parses_and_means_the_defaults() {
        let config: Config = toml::from_str(TEMPLATE).expect("the template must parse");
        assert_eq!(config, Config::default(), "an untouched template changes nothing");
    }

    /// The template is hand-written, so nothing but this stops a new setting
    /// from being added and never documented.
    #[test]
    fn the_template_mentions_every_setting() {
        let rendered = toml::Value::try_from(Config::default()).expect("serialises");
        let mut missing = Vec::new();
        collect_keys(&rendered, &mut Vec::new(), &mut |path, key| {
            // Commented-out lines count: a setting whose default is "leave it
            // to the system" is documented by showing what it would look like.
            if !TEMPLATE.contains(key) {
                missing.push(path.to_string());
            }
        });
        assert!(missing.is_empty(), "settings missing from the template: {missing:?}");
    }

    /// Walk every leaf key of the serialised config, with its dotted path.
    fn collect_keys(
        value: &toml::Value,
        path: &mut Vec<String>,
        found: &mut impl FnMut(&str, &str),
    ) {
        let toml::Value::Table(table) = value else { return };
        for (key, child) in table {
            path.push(key.clone());
            if matches!(child, toml::Value::Table(_)) {
                collect_keys(child, path, found);
            } else {
                found(&path.join("."), key);
            }
            path.pop();
        }
    }
}
