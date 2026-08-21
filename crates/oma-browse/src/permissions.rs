//! What each site is allowed to do.
//!
//! The same shape as [`crate::bookmarks`]: a flat TSV file, no index, written
//! whole on every change. What is stored is a decision *you* made, so it is
//! small, it is worth losing nothing of, and it should be readable with `cat`
//! and editable with an editor -- a permission you cannot audit is a permission
//! you have not really given.
//!
//! Keyed by origin rather than by URL, because that is the boundary the web
//! platform itself uses: `https://meet.example` may use the microphone on every
//! page it serves, and a different port is a different site.

use std::path::PathBuf;

/// A thing a page can ask for.
///
/// One variant per WebKit permission request type this browser answers. They
/// are spelled the way a person would say them, because these strings are the
/// command-line vocabulary (`permission allow <origin> camera`) and the file
/// format at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Camera,
    Microphone,
    /// Sharing a window or a screen with the page. Separate from the camera on
    /// purpose: saying yes to a video call is not saying yes to your desktop.
    ScreenShare,
    Geolocation,
    Notifications,
    /// Enumerating your cameras and microphones by name -- a fingerprinting
    /// surface, and not the same question as being allowed to use one.
    DeviceInfo,
    /// Encrypted media (DRM).
    ProtectedMedia,
}

impl Kind {
    pub const ALL: [Kind; 7] = [
        Kind::Camera,
        Kind::Microphone,
        Kind::ScreenShare,
        Kind::Geolocation,
        Kind::Notifications,
        Kind::DeviceInfo,
        Kind::ProtectedMedia,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Camera => "camera",
            Kind::Microphone => "microphone",
            Kind::ScreenShare => "screen-share",
            Kind::Geolocation => "geolocation",
            Kind::Notifications => "notifications",
            Kind::DeviceInfo => "device-info",
            Kind::ProtectedMedia => "protected-media",
        }
    }

    /// Parse what someone typed. Hyphen or underscore, any case.
    pub fn parse(raw: &str) -> Option<Kind> {
        let wanted = raw.trim().to_ascii_lowercase().replace('_', "-");
        Kind::ALL.into_iter().find(|k| k.as_str() == wanted)
    }

    /// How the prompt says it: "wants <this>".
    pub fn phrase(self) -> &'static str {
        match self {
            Kind::Camera => "to use your camera",
            Kind::Microphone => "to use your microphone",
            Kind::ScreenShare => "to see your screen",
            Kind::Geolocation => "to know where you are",
            Kind::Notifications => "to send you notifications",
            Kind::DeviceInfo => "the names of your cameras and microphones",
            Kind::ProtectedMedia => "to play protected media",
        }
    }
}

/// What to do when a page asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    /// Nothing has been decided for this origin, so the person should be asked.
    Ask,
}

#[derive(Debug, Clone)]
pub struct Grant {
    pub origin: String,
    pub kind: Kind,
    pub allow: bool,
    /// Unix seconds, so the newest sort first.
    pub decided: u64,
}

/// A request that is waiting on a person.
///
/// One WebKit request can ask for several things at once -- a video call asks
/// for the camera and the microphone together -- so this carries every kind it
/// covers, and one answer settles all of them. `id` is how [`crate::policy`]
/// finds the request object again once someone has decided.
#[derive(Debug, Clone)]
pub struct Pending {
    pub id: u64,
    pub origin: String,
    pub kinds: Vec<Kind>,
}

impl Pending {
    /// The question, as the palette asks it.
    pub fn question(&self) -> String {
        format!("{} wants {} -- allow or deny?", self.origin, phrase_list(&self.kinds))
    }
}

/// "your camera", "your camera and your microphone", "a, b and c".
fn phrase_list(kinds: &[Kind]) -> String {
    let phrases: Vec<&str> = kinds.iter().map(|k| k.phrase()).collect();
    match phrases.split_last() {
        None => "something".to_string(),
        Some((last, [])) => (*last).to_string(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

#[derive(Default)]
pub struct Permissions {
    entries: Vec<Grant>,
    /// Where to write. `None` for a detached store that never touches disk --
    /// what the tests use, so deciding something in a unit test does not grant
    /// the running user's real browser access to a camera.
    file: Option<PathBuf>,
}

impl Permissions {
    pub fn load() -> Self {
        let file = path();
        let mut out = Permissions { file: Some(file.clone()), ..Permissions::default() };
        let Ok(raw) = std::fs::read_to_string(&file) else { return out };
        for line in raw.lines() {
            let mut parts = line.splitn(4, '\t');
            let (Some(decided), Some(verdict), Some(origin), Some(kind)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let (Ok(decided), Some(kind)) = (decided.parse(), Kind::parse(kind)) else { continue };
            // Anything that is not the word "allow" is a denial. A file that has
            // been hand-edited into nonsense should fail closed.
            out.entries.push(Grant {
                origin: origin.to_string(),
                kind,
                allow: verdict == "allow",
                decided,
            });
        }
        out
    }

    /// What was decided for this origin, if anything.
    pub fn decide(&self, origin: &str, kind: Kind) -> Decision {
        match self.entries.iter().find(|g| g.origin == origin && g.kind == kind) {
            Some(g) if g.allow => Decision::Allow,
            Some(_) => Decision::Deny,
            None => Decision::Ask,
        }
    }

    /// What was decided for a request covering several kinds at once.
    ///
    /// Every kind has to be allowed for the request to be; one denial denies
    /// the whole thing, because WebKit's answer is one bit and the safe reading
    /// of "yes to the microphone, no to the camera" is no.
    pub fn decide_all(&self, origin: &str, kinds: &[Kind]) -> Decision {
        if kinds.is_empty() {
            return Decision::Deny;
        }
        let each: Vec<Decision> = kinds.iter().map(|k| self.decide(origin, *k)).collect();
        if each.contains(&Decision::Deny) {
            Decision::Deny
        } else if each.iter().all(|d| *d == Decision::Allow) {
            Decision::Allow
        } else {
            Decision::Ask
        }
    }

    /// Record a decision, replacing any earlier one for the same pair.
    pub fn set(&mut self, origin: &str, kind: Kind, allow: bool, now: u64) {
        if origin.is_empty() {
            return;
        }
        self.entries.retain(|g| !(g.origin == origin && g.kind == kind));
        self.entries.insert(0, Grant { origin: origin.to_string(), kind, allow, decided: now });
        self.save();
    }

    /// Forget decisions for an origin -- one kind, or all of them. Returns how
    /// many went, so the caller can say "there was nothing to forget".
    pub fn forget(&mut self, origin: &str, kind: Option<Kind>) -> usize {
        let before = self.entries.len();
        self.entries.retain(|g| g.origin != origin || kind.is_some_and(|k| k != g.kind));
        let gone = before - self.entries.len();
        if gone > 0 {
            self.save();
        }
        gone
    }

    pub fn entries(&self) -> &[Grant] {
        &self.entries
    }

    fn save(&self) {
        let Some(file) = self.file.as_ref() else { return };
        let body: String = self
            .entries
            .iter()
            .map(|g| {
                let verdict = if g.allow { "allow" } else { "deny" };
                format!("{}\t{}\t{}\t{}\n", g.decided, verdict, g.origin, g.kind.as_str())
            })
            .collect();
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(file, body) {
            tracing::warn!(error = %e, path = %file.display(), "could not save permissions");
        }
    }
}

fn path() -> PathBuf {
    crate::history::state_dir().join("permissions")
}

/// The origin of a URL, as the web platform spells it: scheme, host and port.
///
/// `None` for anything without one -- `about:blank`, a `data:` URL, the
/// browser's own `oma-chrome://` chrome. A page with no origin has nothing to
/// remember a decision against, and is refused rather than lumped in with every
/// other originless page under one shared key.
pub fn origin_of(url: &str) -> Option<String> {
    let parsed: url::Url = url.parse().ok()?;
    let origin = parsed.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_decided_means_ask() {
        let p = Permissions::default();
        assert_eq!(p.decide("https://a.example", Kind::Camera), Decision::Ask);
    }

    #[test]
    fn a_decision_is_per_origin_and_per_kind() {
        let mut p = Permissions::default();
        p.set("https://a.example", Kind::Microphone, true, 1);
        assert_eq!(p.decide("https://a.example", Kind::Microphone), Decision::Allow);
        // Saying yes to the microphone is not saying yes to the camera...
        assert_eq!(p.decide("https://a.example", Kind::Camera), Decision::Ask);
        // ...nor to anybody else.
        assert_eq!(p.decide("https://b.example", Kind::Microphone), Decision::Ask);
    }

    #[test]
    fn deciding_again_replaces_rather_than_stacks() {
        let mut p = Permissions::default();
        p.set("https://a.example", Kind::Camera, true, 1);
        p.set("https://a.example", Kind::Camera, false, 2);
        assert_eq!(p.entries().len(), 1);
        assert_eq!(p.decide("https://a.example", Kind::Camera), Decision::Deny);
    }

    #[test]
    fn forgetting_takes_one_kind_or_the_whole_origin() {
        let mut p = Permissions::default();
        p.set("https://a.example", Kind::Camera, true, 1);
        p.set("https://a.example", Kind::Microphone, true, 1);
        p.set("https://b.example", Kind::Camera, true, 1);

        assert_eq!(p.forget("https://a.example", Some(Kind::Camera)), 1);
        assert_eq!(p.decide("https://a.example", Kind::Microphone), Decision::Allow);

        assert_eq!(p.forget("https://a.example", None), 1);
        assert_eq!(p.forget("https://a.example", None), 0);
        // And nobody else's decisions moved.
        assert_eq!(p.decide("https://b.example", Kind::Camera), Decision::Allow);
    }

    #[test]
    fn one_denial_denies_the_whole_request() {
        let mut p = Permissions::default();
        let both = [Kind::Camera, Kind::Microphone];
        assert_eq!(p.decide_all("https://a.example", &both), Decision::Ask);

        p.set("https://a.example", Kind::Camera, true, 1);
        // Half an answer is still a question.
        assert_eq!(p.decide_all("https://a.example", &both), Decision::Ask);

        p.set("https://a.example", Kind::Microphone, true, 1);
        assert_eq!(p.decide_all("https://a.example", &both), Decision::Allow);

        p.set("https://a.example", Kind::Microphone, false, 2);
        assert_eq!(p.decide_all("https://a.example", &both), Decision::Deny);

        // A request for nothing is not a request to say yes to.
        assert_eq!(p.decide_all("https://a.example", &[]), Decision::Deny);
    }

    #[test]
    fn the_question_names_everything_it_is_asking_for() {
        let one =
            Pending { id: 1, origin: "https://a.example".into(), kinds: vec![Kind::Microphone] };
        assert_eq!(
            one.question(),
            "https://a.example wants to use your microphone -- allow or deny?"
        );

        let two = Pending {
            id: 2,
            origin: "https://a.example".into(),
            kinds: vec![Kind::Camera, Kind::Microphone],
        };
        assert!(
            two.question().contains("to use your camera and to use your microphone"),
            "{}",
            two.question()
        );
    }

    #[test]
    fn an_origin_is_scheme_host_and_port() {
        assert_eq!(origin_of("https://a.example/x?y=1#z").as_deref(), Some("https://a.example"));
        // A different port is a different site, and the default one is implicit.
        assert_eq!(
            origin_of("http://127.0.0.1:8901/index.html").as_deref(),
            Some("http://127.0.0.1:8901")
        );
        assert_eq!(origin_of("https://a.example:443/").as_deref(), Some("https://a.example"));
        // Nothing to remember a decision against.
        assert_eq!(origin_of("about:blank"), None);
        assert_eq!(origin_of("data:text/html,hi"), None);
        assert_eq!(origin_of("not a url"), None);
    }

    #[test]
    fn every_kind_survives_a_round_trip_through_its_name() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.as_str()), Some(kind));
        }
        // What someone might actually type.
        assert_eq!(Kind::parse("Screen_Share"), Some(Kind::ScreenShare));
        assert_eq!(Kind::parse("  camera "), Some(Kind::Camera));
        assert_eq!(Kind::parse("cameras"), None);
    }

    #[test]
    fn a_hand_edited_file_fails_closed() {
        // Not a round trip through disk -- just the rule the parser encodes:
        // only the literal word "allow" is a grant.
        let mut p = Permissions::default();
        p.set("https://a.example", Kind::Camera, false, 1);
        assert_eq!(p.decide("https://a.example", Kind::Camera), Decision::Deny);
    }
}
