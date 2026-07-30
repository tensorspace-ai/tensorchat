//! TensorChat server entry point.
//!
//! Startup only — the router and every module it wires together live in the
//! library half of this crate, so they can be exercised by integration tests
//! without a live process.

use std::sync::Arc;
use std::time::Duration;

use tc_server::cli::{self, Command};
use tc_server::{AppState, Config, Shared, build_router};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::from_env().map_err(|e| format!("configuration error: {e}"))?;

    // Dispatch before starting a runtime. The operator commands are synchronous
    // database work with no server behind them, and spinning up tokio to print
    // an invite link would be pure ceremony.
    let command = cli::parse(std::env::args().skip(1)).map_err(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    })?;

    if !matches!(command, Command::Serve) {
        let store = cli::open_store(&cfg.db_path)?;
        match cli::run(&store, &cfg, command) {
            Ok(message) => {
                println!("{}", message.trim_end());
                return Ok(());
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }

    serve(cfg)
}

#[tokio::main]
async fn serve(cfg: Config) -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tc_server=info,tower_http=warn".into()),
        )
        .compact()
        .init();

    // Create the directories we own before opening anything inside them, so a
    // fresh checkout runs with no setup step.
    if let Some(parent) = cfg.db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(&cfg.blob_dir)?;

    let store = tc_store::Store::open(&cfg.db_path)?;
    tracing::info!(db = %cfg.db_path.display(), "database ready");

    // A workspace nobody can sign in to fails silently otherwise: the server
    // comes up, serves a login page, and refuses every credential because there
    // are none. Say so, and name the command that fixes it.
    cli::warn_if_unreachable(&store, &cfg);

    // Web Push needs a stable VAPID keypair, minted into the database on first
    // run. A failure here disables push rather than stopping the server: chat
    // works without notifications, and refusing to boot over them would be a
    // poor trade.
    let vapid = if cfg.push_contact.is_empty() {
        tracing::info!("web push disabled (TC_PUSH_CONTACT is empty)");
        None
    } else {
        match tc_server::push::Vapid::load(&store, &cfg.push_contact) {
            Ok(v) => {
                tracing::info!("web push enabled");
                Some(v)
            }
            Err(e) => {
                tracing::warn!(error = %e, "web push disabled");
                None
            }
        }
    };

    let st: Shared = Arc::new(AppState::new(cfg.clone(), store).with_push(vapid));
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

/// How long a spent invite lingers before housekeeping drops it.
///
/// Not zero, because an administrator asking "did that link ever get used?" a
/// week later deserves an answer. Expiry is enforced at redemption regardless,
/// so this only governs when the row stops taking up space.
const INVITE_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Periodic housekeeping: expire sessions and invites, checkpoint the WAL,
/// refresh planner statistics.
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
                    // Saturating, so a clock before the epoch cannot wrap the
                    // cutoff into the far future and delete live invites.
                    let invites =
                        s.purge_expired_invites(now.saturating_sub(INVITE_RETENTION_MS))?;
                    s.maintenance()?;
                    Ok(purged + invites)
                })
                .await;
            match r {
                Ok(n) if n > 0 => tracing::info!(purged = n, "maintenance: expired rows"),
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
