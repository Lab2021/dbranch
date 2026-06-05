//! Embedded frontend bundle. Files under `web/dist/` are baked into the
//! binary at build time.

use axum::{
    body::Body,
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct Asset;

/// Serves static files from the embedded bundle. Maps `/` and unknown paths
/// to `index.html` so the SPA's client-side router (none, currently) can
/// handle them.
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Asset::get(path).or_else(|| Asset::get("index.html")) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(content.data.into_owned()))
                .unwrap()
        }
        None => (StatusCode::NOT_FOUND, "asset bundle missing").into_response(),
    }
}
