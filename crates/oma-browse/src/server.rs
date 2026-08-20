//! The local control plane: one listener serving the UI, the commands, and MCP.

use std::sync::Arc;

use anyhow::Result;
use incurs::cli::Cli;
use incurs::tool::ToolCatalog;
use tokio::net::TcpListener;
use topcoat::router::tower::TowerService;

use crate::state::AppState;

/// The browser's own mark, and the same art squared off for a favicon.
///
/// Compiled in rather than read from disk: the binary is the whole install --
/// `oma-browse` is one file you can copy onto a machine -- and a start page that
/// depends on a file beside it is a start page that breaks the moment it is not.
const MARK_PNG: &[u8] = include_bytes!("../../../assets/mark.png");
const ICON_PNG: &[u8] = include_bytes!("../../../assets/icon.png");

/// Serve compiled-in bytes as a PNG.
///
/// An hour of caching: long enough that a reload does not re-fetch the mark,
/// short enough that a rebuild during development is picked up without a hard
/// refresh. The port is ephemeral by default anyway, so a fresh run is usually
/// a fresh origin and a cold cache.
fn png(bytes: &'static [u8]) -> axum::response::Response {
    use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
    use axum::response::IntoResponse;
    ([(CONTENT_TYPE, "image/png"), (CACHE_CONTROL, "max-age=3600")], bytes).into_response()
}

pub struct Server {
    pub addr: std::net::SocketAddr,
    pub listener: TcpListener,
    pub app: axum::Router,
}

/// Bind loopback and compose the two frameworks onto it.
///
/// The port is `control.port`, which defaults to 0 -- whatever is free. A fixed
/// port is what makes the browser reachable by something that did not watch it
/// start; the port file below is the other answer to the same problem.
pub async fn build(cli: &Cli, catalog: ToolCatalog, state: Arc<AppState>) -> Result<Server> {
    let port = state.config.control.port;
    let port_file = state.config.control.port_file;
    let topcoat_router = crate::ui::router(state, catalog);

    // incurs is *nested*, never merged: `build_cli_router` registers `/` and
    // `/{*path}` catch-alls that would otherwise swallow every Topcoat route.
    // Topcoat takes everything else as the fallback service.
    let app = axum::Router::new()
        .nest("/cmd", incurs::http::build_cli_router(cli)?)
        // The start page's mark, and the icon every page in this origin gets --
        // including `/favicon.ico`, which WebKit asks for unprompted and which
        // was answering 404 on every start-page load.
        .route("/mark.png", axum::routing::get(|| async { png(MARK_PNG) }))
        .route("/icon.png", axum::routing::get(|| async { png(ICON_PNG) }))
        .route("/favicon.ico", axum::routing::get(|| async { png(ICON_PNG) }))
        .fallback_service(TowerService::new(topcoat_router));

    // Loopback only. This control plane can drive the browser and read page
    // content, so it must never be reachable off-host.
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| anyhow::anyhow!("could not listen on 127.0.0.1:{port}: {e}"))?;
    let addr = listener.local_addr()?;

    if port_file {
        write_port_file(addr.port());
    }

    Ok(Server { addr, listener, app })
}

/// Where the running browser's port is written, for anything that wants to
/// drive it without having read the startup log.
///
/// Under `$XDG_RUNTIME_DIR`, beside the screenshots: per-boot and user-private,
/// so a file left behind by a crash is cleaned up at the next reboot rather
/// than pointing at nothing forever.
pub fn port_file() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("oma-browse")
        .join("port")
}

fn write_port_file(port: u16) {
    let path = port_file();
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(error = %e, path = %parent.display(), "no port file");
        return;
    }
    match std::fs::write(&path, port.to_string()) {
        Ok(()) => tracing::debug!(path = %path.display(), port, "port file written"),
        Err(e) => tracing::warn!(error = %e, path = %path.display(), "no port file"),
    }
}

impl Server {
    pub async fn serve(self) -> Result<()> {
        axum::serve(self.listener, self.app).await?;
        Ok(())
    }
}
