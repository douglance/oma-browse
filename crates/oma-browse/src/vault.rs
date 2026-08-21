//! Passwords, without an extension.
//!
//! WebKitGTK has no extension API at all, so 1Password's and Bitwarden's
//! browser extensions cannot run here and never will. That sounds like a gap
//! and is mostly a shape mismatch: every one of those products already ships a
//! command-line client, and this browser's entire premise is that a capability
//! reachable from the command line is a capability reachable from a key, from
//! the palette, from a script and from an agent.
//!
//! So `page fill --from rbw` asks `rbw` for the login that matches the page you
//! are on and types it in. No extension, no vault living inside the browser's
//! address space, and no new place for a password to be at rest -- this process
//! holds the secret for as long as it takes to build one `page eval` and not a
//! moment longer.
//!
//! # What this deliberately does not do
//!
//! It does not remember anything, it does not offer to save anything, and it
//! does not guess. The entry is the page's host unless you name one; if the
//! vault has no such entry the answer is an error naming what was looked for,
//! not a list of near misses. A password manager that fuzzy-matches hostnames is
//! a password manager that will one day type your bank's password into
//! somebody else's form.

use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};

/// A password manager this browser knows how to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vault {
    /// The unofficial Bitwarden CLI, which is what Omarchy users mostly have.
    Rbw,
    /// 1Password's `op`.
    Op,
    /// `pass`, the standard Unix password manager.
    Pass,
}

impl Vault {
    pub const ALL: [Vault; 3] = [Vault::Rbw, Vault::Op, Vault::Pass];

    pub fn as_str(self) -> &'static str {
        match self {
            Vault::Rbw => "rbw",
            Vault::Op => "op",
            Vault::Pass => "pass",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "rbw" | "bitwarden" | "bw" => Some(Vault::Rbw),
            "op" | "1password" | "onepassword" => Some(Vault::Op),
            "pass" | "gopass" => Some(Vault::Pass),
            _ => None,
        }
    }

    /// The command line to run, for one entry and one field.
    ///
    /// Split out from running it so the shape of every call is a unit test
    /// rather than something only a machine with a vault on it can check.
    pub fn argv(self, entry: &str, field: Field) -> Vec<String> {
        let own = |parts: &[&str]| parts.iter().map(|p| (*p).to_string()).collect::<Vec<_>>();
        match (self, field) {
            (Vault::Rbw, Field::Password) => own(&["get", entry]),
            (Vault::Rbw, Field::Username) => own(&["get", "--field", "username", entry]),
            (Vault::Op, Field::Password) => {
                own(&["item", "get", entry, "--fields", "label=password", "--reveal"])
            }
            (Vault::Op, Field::Username) => {
                own(&["item", "get", entry, "--fields", "label=username", "--reveal"])
            }
            // `pass` has no field model: an entry is a file whose first line is
            // the password and whose remaining lines are `key: value` by
            // convention. The convention is followed here and nowhere else.
            (Vault::Pass, _) => own(&["show", entry]),
        }
    }

    /// Pull the wanted value out of what the tool printed.
    pub fn read(self, field: Field, stdout: &str) -> Result<String> {
        match (self, field) {
            (Vault::Pass, Field::Username) => {
                for line in stdout.lines().skip(1) {
                    let lowered = line.to_ascii_lowercase();
                    for key in ["username:", "user:", "login:"] {
                        if let Some(rest) = lowered.strip_prefix(key) {
                            let start = line.len() - rest.len();
                            return Ok(line[start..].trim().to_string());
                        }
                    }
                }
                bail!("that entry has no username: line")
            }
            // Everything else prints the value and nothing else -- but a
            // trailing newline is universal and a second line, where one turns
            // up, is never the secret.
            _ => stdout
                .lines()
                .next()
                .map(str::to_string)
                .filter(|line| !line.is_empty())
                .ok_or_else(|| anyhow!("the vault answered with nothing")),
        }
    }
}

/// Which half of a login is wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Password,
    Username,
}

impl Field {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "password" | "pass" | "secret" => Some(Field::Password),
            "username" | "user" | "login" | "email" => Some(Field::Username),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Field::Password => "password",
            Field::Username => "username",
        }
    }
}

/// The entry to look up for a page, when the caller did not name one.
///
/// The registrable host, with `www.` taken off -- `https://www.github.com/x`
/// and `https://github.com/y` are one login in everybody's vault and typing the
/// `www.` is nobody's idea of the entry name. Nothing cleverer: no suffix list,
/// no walking up the domain trying `github.com` then `com`. See the module
/// note on why guessing is the wrong instinct here.
pub fn entry_for(url: &str) -> Option<String> {
    let parsed: url::Url = url.parse().ok()?;
    let host = parsed.host_str()?;
    Some(host.strip_prefix("www.").unwrap_or(host).to_string())
}

/// Ask a vault for one value.
///
/// The secret goes from the child's stdout into a `String` and back out to the
/// caller. It is never logged, never written to a file, and never put in a
/// command's output -- `page fill --from` answers with the selector it filled
/// and nothing else.
pub async fn get(vault: Vault, entry: &str, field: Field) -> Result<String> {
    let argv = vault.argv(entry, field);
    let output = tokio::process::Command::new(vault.as_str())
        .args(&argv)
        // Inherited, deliberately: `rbw` and `pass` both prompt for a master
        // password or a GPG passphrase through their own agent, and a child
        // with no terminal cannot ask. If the agent is locked the user sees the
        // prompt where they are.
        .stdin(Stdio::inherit())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("could not run {}; is it installed?", vault.as_str()))?;

    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr);
        let why = why.trim();
        bail!(
            "{} could not read the {} for {entry:?}{}",
            vault.as_str(),
            field.as_str(),
            if why.is_empty() { String::new() } else { format!(": {why}") }
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .context("the vault answered with something that is not text")?;
    vault.read(field, &stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_vault_survives_a_round_trip_and_its_common_aliases() {
        for vault in Vault::ALL {
            assert_eq!(Vault::parse(vault.as_str()), Some(vault));
        }
        assert_eq!(Vault::parse("bitwarden"), Some(Vault::Rbw));
        assert_eq!(Vault::parse("1Password"), Some(Vault::Op));
        assert_eq!(Vault::parse("keepass"), None);
    }

    #[test]
    fn the_entry_is_the_host_without_its_www() {
        assert_eq!(entry_for("https://github.com/a/b").as_deref(), Some("github.com"));
        assert_eq!(entry_for("https://www.github.com/").as_deref(), Some("github.com"));
        assert_eq!(entry_for("http://localhost:3000/login").as_deref(), Some("localhost"));
        assert_eq!(entry_for("not a url"), None);
        // A page with no host is a page with no login.
        assert_eq!(entry_for("about:blank"), None);
    }

    #[test]
    fn each_vault_is_asked_the_way_that_vault_expects() {
        assert_eq!(Vault::Rbw.argv("github.com", Field::Password), ["get", "github.com"]);
        assert_eq!(
            Vault::Rbw.argv("github.com", Field::Username),
            ["get", "--field", "username", "github.com"]
        );
        assert!(
            Vault::Op.argv("github.com", Field::Password).contains(&"--reveal".to_string()),
            "op prints a placeholder without it"
        );
        assert_eq!(Vault::Pass.argv("github.com", Field::Password), ["show", "github.com"]);
    }

    #[test]
    fn the_password_is_the_first_line_and_only_the_first_line() {
        let printed = "hunter2\nusername: ada\notp: 123456\n";
        assert_eq!(Vault::Pass.read(Field::Password, printed).unwrap(), "hunter2");
        assert_eq!(Vault::Rbw.read(Field::Password, "hunter2\n").unwrap(), "hunter2");
    }

    #[test]
    fn pass_finds_a_username_under_any_of_the_names_people_use_for_it() {
        for line in ["username: ada", "user: ada", "login: ada", "Username:  ada  "] {
            let printed = format!("hunter2\n{line}\n");
            assert_eq!(Vault::Pass.read(Field::Username, &printed).unwrap(), "ada", "{line}");
        }
    }

    #[test]
    fn a_username_line_that_is_not_there_is_an_error_and_not_the_password() {
        let printed = "hunter2\notp: 123456\n";
        let answer = Vault::Pass.read(Field::Username, printed);
        assert!(answer.is_err(), "{answer:?}");
        // The failure mode this guards against: falling back to line one and
        // typing the password into the username box, in the clear.
        assert!(!format!("{answer:?}").contains("hunter2"));
    }

    #[test]
    fn an_empty_answer_is_an_error_rather_than_an_empty_password() {
        assert!(Vault::Rbw.read(Field::Password, "").is_err());
        assert!(Vault::Rbw.read(Field::Password, "\n").is_err());
    }
}
