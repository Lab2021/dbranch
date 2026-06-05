//! HTTP API + embedded web UI for dBranch.
//!
//! Boots alongside the TCP proxy when `dbranch start` runs. Bound to
//! [`Config::api_port`] (default 8000). All endpoints live under `/api/`;
//! anything else is served from the embedded static bundle.

pub mod assets;
pub mod routes;

use std::net::SocketAddr;

use axum::Router;
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info};

use crate::error::AppError;

/// Builds the axum router with API routes + static asset fallback.
pub fn app() -> Router {
    Router::new()
        .nest("/api", routes::api_router())
        .fallback(assets::serve)
        .layer(
            CorsLayer::new()
                .allow_methods(Any)
                .allow_origin(Any)
                .allow_headers(Any),
        )
}

/// Runs the HTTP server until cancelled. Intended to be spawned alongside the
/// TCP proxy from `main::run_server`.
pub async fn serve(port: u16) -> Result<(), AppError> {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("🌐 Web UI + API on http://{}", addr);

    let listener = TcpListener::bind(addr).await.map_err(|e| AppError::Network {
        message: format!("Failed to bind HTTP {}: {}", addr, e),
    })?;

    if let Err(e) = axum::serve(listener, app()).await {
        error!("HTTP server error: {}", e);
        return Err(AppError::Network {
            message: format!("HTTP serve error: {}", e),
        });
    }
    Ok(())
}
