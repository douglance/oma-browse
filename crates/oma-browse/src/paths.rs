//! What a path the user typed actually points at.
//!
//! A command like `page screenshot --path shot.png` is typed in one process and
//! run in another: the CLI forwards the argv down the control socket and the
//! *browser* writes the file. So "shot.png" cannot mean what `PathBuf::from`
//! makes of it, which is the browser's working directory -- some directory the
//! user has never been in, chosen by whichever launcher started the window.
//!
//! The caller's working directory travels with the request ([`crate::control::Request::cwd`])
//! and is put in scope for the length of that command by [`with_caller_cwd`].
//! A relative path is joined onto it, so the file lands where the user was
//! standing when they asked. When no caller is in scope -- a raw HTTP or MCP
//! request, which carries no working directory -- a relative path is refused
//! rather than quietly resolved against the browser's, because a file written
//! somewhere the caller cannot name is a file they have lost.

use std::cell::RefCell;
use std::path::PathBuf;

use anyhow::{Result, bail};

thread_local! {
    /// The working directory of whoever asked, for the length of one command.
    ///
    /// A thread-local rather than a task-local because of how a forwarded argv
    /// is run: `Cli::run_to` writes into a `&mut dyn Write` with no `Send`
    /// bound, so `server::run_argv` drives its future to completion inside one
    /// blocking-pool thread. Everything that command does happens on that
    /// thread, and nothing else does -- which is precisely the scope this needs.
    static CALLER_CWD: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Put the caller's working directory in scope for `f`.
///
/// Restores whatever was there before on the way out, panic included: a leaked
/// value would attach one caller's directory to the next command that blocking
/// thread happens to run.
pub fn with_caller_cwd<T>(cwd: &str, f: impl FnOnce() -> T) -> T {
    // A relative "current directory" is not one, and an empty string is a
    // client that did not send one. Either way there is nothing to join onto.
    let dir = Some(PathBuf::from(cwd)).filter(|p| p.is_absolute());
    let _guard = Scope::new(dir);
    f()
}

/// Sets the slot on the way in and puts back what was there on the way out.
struct Scope(Option<PathBuf>);

impl Scope {
    fn new(dir: Option<PathBuf>) -> Self {
        Scope(CALLER_CWD.with(|slot| std::mem::replace(&mut *slot.borrow_mut(), dir)))
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let previous = self.0.take();
        CALLER_CWD.with(|slot| *slot.borrow_mut() = previous);
    }
}

/// Where the caller was standing, if anyone said.
fn caller_cwd() -> Option<PathBuf> {
    CALLER_CWD.with(|slot| slot.borrow().clone())
}

/// Turn a path the user typed into one this process can write to.
///
/// Fails rather than guesses: see the module comment for why a relative path
/// with no caller in scope is an error and not the browser's own directory.
pub fn resolve(path: &str) -> Result<PathBuf> {
    let expanded = PathBuf::from(shellexpand(path));
    if expanded.is_absolute() {
        return Ok(expanded);
    }
    match caller_cwd() {
        Some(cwd) => Ok(cwd.join(expanded)),
        None => bail!(
            "`{path}` is a relative path, and this command runs inside the browser process -- \
             where that would mean a directory you never chose. Give an absolute path, or run it \
             through the `oma-browse` command line, which sends yours along."
        ),
    }
}

/// `~` only. Anything more is the shell's job, and this is also called over
/// HTTP where there is no shell to have done it.
///
/// Shared with the config file, which is written by a human, and a human writes
/// `~/Downloads`. Full shell expansion in a path a browser writes files to
/// would be a liability rather than a feature.
pub fn shellexpand(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest).display().to_string(),
            None => path.to_string(),
        },
        None => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_path_is_left_alone() {
        assert_eq!(resolve("/tmp/a.png").unwrap(), PathBuf::from("/tmp/a.png"));
        // And does not pick up a caller's directory it does not need.
        with_caller_cwd("/home/someone", || {
            assert_eq!(resolve("/tmp/a.png").unwrap(), PathBuf::from("/tmp/a.png"));
        });
    }

    #[test]
    fn a_relative_path_lands_where_the_caller_was_standing() {
        with_caller_cwd("/home/someone/work", || {
            assert_eq!(resolve("shot.png").unwrap(), PathBuf::from("/home/someone/work/shot.png"));
            assert_eq!(
                resolve("./out/shot.png").unwrap(),
                PathBuf::from("/home/someone/work/./out/shot.png")
            );
        });
    }

    #[test]
    fn a_relative_path_with_nobody_asking_is_refused() {
        // The bug this module exists for: writing to the browser's own working
        // directory and reporting success.
        let e = resolve("shot.png").unwrap_err().to_string();
        assert!(e.contains("relative path"), "{e}");
    }

    #[test]
    fn the_scope_does_not_outlive_the_command() {
        with_caller_cwd("/home/someone", || assert!(caller_cwd().is_some()));
        assert!(caller_cwd().is_none());

        // Nested, because one blocking thread runs one command after another and
        // the second must not inherit the first's directory.
        with_caller_cwd("/a", || {
            with_caller_cwd("/b", || assert_eq!(caller_cwd(), Some(PathBuf::from("/b"))));
            assert_eq!(caller_cwd(), Some(PathBuf::from("/a")));
        });
    }

    #[test]
    fn a_caller_without_a_directory_is_not_one() {
        // An empty or relative `cwd` on the wire is a client that did not send
        // one; joining onto it would be worse than saying so.
        for bad in ["", "relative/dir"] {
            with_caller_cwd(bad, || assert!(resolve("shot.png").is_err()));
        }
    }

    #[test]
    fn a_tilde_is_the_only_expansion() {
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(shellexpand("~/x"), format!("{home}/x"));
        assert_eq!(shellexpand("$HOME/x"), "$HOME/x");
        assert_eq!(shellexpand("/tmp/~/x"), "/tmp/~/x");
    }
}
