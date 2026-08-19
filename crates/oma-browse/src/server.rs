//! The local control plane: one listener serving the UI, the commands, and MCP.

use std::sync::Arc;

use anyhow::Result;
use incurs::cli::Cli;
use tokio::net::TcpListener;
use topcoat::router::tower::TowerService;

use crate::state::AppState;

pub struct Server {
    pub addr: std::net::SocketAddr,
    pub listener: TcpListener,
    pub app: axum::Router,
}

/// Bind loopback on an ephemeral port and compose the two frameworks onto it.
pub async fn build(cli: &Cli, state: Arc<AppState>) -> Result<Server> {
    let topcoat_router = crate::ui::router(state);

    // incurs is *nested*, never merged: `build_cli_router` registers `/` and
    // `/{*path}` catch-alls that would otherwise swallow every Topcoat route.
    // Topcoat takes everything else as the fallback service.
    let app = axum::Router::new()
        .nest("/cmd", incurs::http::build_cli_router(cli)?)
        .fallback_service(TowerService::new(topcoat_router));

    // Loopback only. This control plane can drive the browser and read page
    // content, so it must never be reachable off-host.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    Ok(Server { addr, listener, app })
}

impl Server {
    pub async fn serve(self) -> Result<()> {
        axum::serve(self.listener, self.app).await?;
        Ok(())
    }
}
