//! TensorChat server entry point.
//!
//! Startup only — the router and every module it wires together live in the
//! library half of this crate, so they can be exercised by integration tests
//! without a live process.

use std::sync::Arc;
use std::time::Duration;

use tc_server::{AppState, Config, Shared, build_router};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tc_server=info,tower_http=warn".into()),
        )
        .compact()
        .init();

    let cfg = Config::from_env().map_err(|e| format!("configuration error: {e}"))?;

    // Create the directories we own before opening anything inside them, so a
    // fresh checkout runs with no setup step.
    if let Some(parent) = cfg.db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(&cfg.blob_dir)?;

    let store = tc_store::Store::open(&cfg.db_path)?;
    tracing::info!(db = %cfg.db_path.display(), "database ready");

    let st: Shared = Arc::new(AppState::new(cfg.clone(), store));
    spawn_maintenance(st.clone());

    let app = build_router(st.clone());
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "tensorchat listening");

    axum::serve(
        listener,
        // `ConnectInfo` gives the pre-auth rate limiter a client address.
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    tracing::info!("shutting down");
    Ok(())
}

/// Periodic housekeeping: expire sessions, checkpoint the WAL, refresh planner
/// statistics.
fn spawn_maintenance(st: Shared) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first tick fires immediately; skip it so startup is not delayed.
        tick.tick().await;
        loop {
            tick.tick().await;
            let now = tc_core::now_ms();
            let r = st
                .db(move |s| {
                    let purged = s.purge_expired_sessions(now)?;
                    s.maintenance()?;
                    Ok(purged)
                })
                .await;
            match r {
                Ok(n) if n > 0 => tracing::info!(purged = n, "maintenance: expired sessions"),
                Ok(_) => tracing::debug!("maintenance complete"),
                Err(e) => tracing::warn!(error = %e, "maintenance failed"),
            }
        }
    });
}

/// Resolve on SIGINT or SIGTERM so in-flight requests finish before exit.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            // Without SIGTERM we can still be stopped by Ctrl-C.
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
