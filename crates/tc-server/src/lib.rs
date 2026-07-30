//! TensorChat server library.
//!
//! The binary in `main.rs` is a thin startup wrapper around this; everything
//! testable lives here so integration tests can build a router over a
//! throwaway database without spawning a process or binding a port.

pub mod api;
pub mod auth;
pub mod config;
pub mod error;
pub mod hub;
pub mod push;
pub mod ratelimit;
pub mod service;
pub mod state;
pub mod ws;

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::routing::get;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

pub use config::Config;
pub use state::{AppState, Shared};

/// Build the full application router.
pub fn build_router(st: Shared) -> Router {
    let web_dir = st.cfg.web_dir.clone();
    let index = web_dir.join("index.html");

    let assets = ServeDir::new(web_dir.join("assets"));
    // Single-page app: unknown paths return index.html so a hard refresh on a
    // client-side route still loads.
    //
    // `no-cache` on this document is load-bearing. Asset filenames are
    // content-hashed and cached for a year, and index.html is the only thing
    // that names them — if a browser caches it heuristically (which it will,
    // absent a directive), a returning user keeps loading the previous
    // deployment's bundle indefinitely, and eventually 404s when those files
    // are pruned. "no-cache" still allows a revalidated 304; it just forbids
    // using the copy blind.
    let static_files = ServeDir::new(&web_dir).fallback(ServeFile::new(&index));

    let mut app = Router::new()
        .merge(api::routes())
        .route("/ws", get(ws::handler))
        // Asset filenames are content-hashed at build time, so they are safe to
        // cache forever; everything else revalidates.
        .nest_service(
            "/assets",
            axum::routing::any_service(assets).layer(SetResponseHeaderLayer::overriding(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            )),
        )
        .fallback_service(axum::routing::any_service(static_files).layer(
            SetResponseHeaderLayer::overriding(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache, must-revalidate"),
            ),
        ))
        // Compress JSON and JS. The WebSocket path is already binary and does
        // not pass through this layer.
        .layer(CompressionLayer::new().br(true).gzip(true))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("same-origin"),
        ))
        // The frontend loads no third-party code and makes no cross-origin
        // requests, so it can afford a strict policy. Note the absence of
        // 'unsafe-inline' for scripts: the UI builds DOM nodes and never HTML
        // strings, so it does not need one.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; img-src 'self' data: blob:; media-src 'self' blob:; \
                 connect-src 'self' ws: wss:; style-src 'self'; script-src 'self'; \
                 frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
            ),
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(st.clone());

    if st.cfg.permissive_cors {
        tracing::warn!("permissive CORS enabled — intended for local development only");
        app = app.layer(CorsLayer::permissive());
    }
    app
}
