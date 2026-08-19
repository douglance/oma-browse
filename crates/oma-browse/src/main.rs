//! oma-browse — an Omarchy-themed, agent-drivable browser.

mod commands;
mod layout;
mod server;
mod shot;
mod state;
mod tabs;
mod ui;
mod window;

use std::sync::Arc;

use anyhow::Result;

use crate::state::AppState;

/// Which face of the binary the user asked for.
///
/// One binary, two modes. A subcommand means "talk to the browser"; anything
/// else means "be the browser". `omarchy-launch-browser` passes a bare URL and
/// rewrites its own `--private` to `--incognito` for non-Firefox, non-Edge
/// browsers, so both of those have to land in GUI mode.
enum Invocation {
    Gui { url: Option<String>, incognito: bool },
    Command,
}

fn classify(args: &[String]) -> Invocation {
    let mut url = None;
    let mut incognito = false;

    for arg in args {
        match arg.as_str() {
            "--incognito" | "--private" => incognito = true,
            a if looks_like_url(a) => url = Some(normalize_url(a)),
            // Anything else is a subcommand or a flag for one; hand the whole
            // argv to incurs and let it do the parsing and the error messages.
            _ => return Invocation::Command,
        }
    }
    Invocation::Gui { url, incognito }
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://") || s.starts_with("file://")
}

fn normalize_url(s: &str) -> String {
    s.to_string()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "oma_browse=info,oma_theme=info".into()),
        )
        .init();

    let state = Arc::new(AppState::new());
    let cli = commands::command_graph(state.clone());

    let args: Vec<String> = std::env::args().skip(1).collect();
    match classify(&args) {
        // A subcommand: dispatch through incurs and exit. `serve` reads argv
        // itself and handles --help/--version/--json/completions.
        Invocation::Command => {
            cli.serve().await.map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(())
        }

        Invocation::Gui { url, incognito } => {
            let server = server::build(&cli, state.clone()).await?;
            let addr = server.addr;
            tracing::info!(%addr, ?url, incognito, "oma-browse control plane up");

            // The control plane serves the chrome, so it has to be listening
            // before the webview points at it. Tauri then takes the main thread.
            tokio::spawn(async move {
                if let Err(e) = server.serve().await {
                    tracing::error!(error = %e, "control plane stopped");
                }
            });

            let base: url::Url = format!("http://{addr}/").parse()?;
            state.set_base_url(base.clone());
            let start_url = match url {
                Some(u) => u.parse()?,
                None => base.join("start")?,
            };

            let (background, opacity) = {
                let theme = state.theme.read().await;
                (theme.css.tint, theme.css.opacity)
            };
            let page_script = state.page_script().await;

            // Allocate the first tab here: Tauri owns the main thread from the
            // moment `run` is called, and the tab model lives behind async locks.
            let first_tab = {
                let mut tabs = state.tabs.write().await;
                let (id, label) = tabs.allocate(start_url.to_string());
                tabs.set_active(id);
                label
            };

            // Omarchy emits no D-Bus signal and no broadcast on a theme change:
            // it swaps the directory, rewrites `theme.name`, and runs its hooks.
            // Watching the *parent* directory catches it without asking the user
            // to install anything. (`oma-browse theme install-hook` wires up the
            // faster, official path, but is deliberately opt-in — it writes into
            // their Omarchy config.)
            spawn_theme_watcher(state.clone());

            window::run(window::Launch {
                state,
                base,
                start_url,
                incognito,
                background,
                opacity,
                page_script,
                first_tab,
            })
        }
    }
}

/// Watch for Omarchy theme changes and re-dress the browser in place.
fn spawn_theme_watcher(state: Arc<AppState>) {
    tokio::spawn(async move {
        let (mut rx, _watcher) = match oma_theme::watch::watch_theme_changes() {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "not watching for theme changes");
                return;
            }
        };
        tracing::info!("watching for Omarchy theme changes");

        // `_watcher` must outlive the loop: dropping it stops the watch.
        while let Some(name) = rx.recv().await {
            if state.reload_theme().await {
                tracing::info!(theme = %name, "restyling");
                if let Err(e) = crate::window::restyle(&state).await {
                    tracing::warn!(error = %e, "could not restyle");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_launch_is_gui() {
        assert!(matches!(classify(&args(&[])), Invocation::Gui { url: None, .. }));
    }

    #[test]
    fn a_url_is_gui_not_a_subcommand() {
        // `omarchy-launch-browser` passes the URL positionally.
        match classify(&args(&["https://example.com"])) {
            Invocation::Gui { url, .. } => assert_eq!(url.as_deref(), Some("https://example.com")),
            _ => panic!("a bare URL must open the GUI"),
        }
    }

    #[test]
    fn incognito_is_gui() {
        // Omarchy rewrites its own `--private` to `--incognito` for us, so both
        // spellings must reach GUI mode rather than incurs' parser.
        for flag in ["--incognito", "--private"] {
            match classify(&args(&[flag])) {
                Invocation::Gui { incognito, .. } => assert!(incognito, "{flag} should set incognito"),
                _ => panic!("{flag} must open the GUI"),
            }
        }
    }

    #[test]
    fn incognito_with_a_url_is_still_gui() {
        match classify(&args(&["--incognito", "https://example.com"])) {
            Invocation::Gui { url, incognito } => {
                assert!(incognito);
                assert_eq!(url.as_deref(), Some("https://example.com"));
            }
            _ => panic!("expected GUI"),
        }
    }

    #[test]
    fn subcommands_go_to_the_cli() {
        for a in [vec!["theme"], vec!["theme", "show"], vec!["--help"], vec!["--version"]] {
            assert!(matches!(classify(&args(&a)), Invocation::Command), "{a:?}");
        }
    }
}
