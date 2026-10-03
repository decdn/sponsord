use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sponsord_api::client::DaemonClient;
use sponsord_api::time::{Clock, SystemClock};
use sponsord_onramp::config::{GateKind, OnrampConfig};
use sponsord_onramp::daemon::CapabilitySource;
use sponsord_onramp::gate::{Gate, TurnstileGate};
use sponsord_onramp::{http, state};
use tokio_util::sync::CancellationToken;

/// Bound on every outbound call (daemon and gate backend), so a hung peer
/// fails a request instead of holding it open.
const OUTBOUND_TIMEOUT: Duration = Duration::from_secs(10);

/// How often expired hand-offs are dropped from memory.
const SWEEP_EVERY: Duration = Duration::from_secs(300);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cfg = OnrampConfig::load()?;
    let http_client = reqwest::Client::builder()
        .timeout(OUTBOUND_TIMEOUT)
        .build()?;
    let daemon: Arc<dyn CapabilitySource> = Arc::new(DaemonClient::new(
        &cfg.daemon_url,
        cfg.daemon_token.clone(),
        http_client.clone(),
    ));
    let gate: Arc<dyn Gate> = match cfg.gate {
        GateKind::Turnstile => Arc::new(TurnstileGate::new(
            cfg.turnstile_secret.clone(),
            cfg.turnstile_sitekey.clone(),
            cfg.brand_name.clone(),
            cfg.gate_template.clone(),
            http_client,
        )),
    };
    let state = state::build(&cfg, daemon, gate).await?;

    let shutdown = CancellationToken::new();
    let grants = state.grants.clone();
    let sweeper = shutdown.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        loop {
            tokio::select! {
                () = sweeper.cancelled() => return,
                _ = tick.tick() => grants.sweep(SystemClock.now_unix()),
            }
        }
    });

    let app = http::router(state).into_make_service_with_connect_info::<SocketAddr>();
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, "sponsord-onramp listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await?;
    Ok(())
}

/// Resolve on Ctrl-C or SIGTERM, cancelling `shutdown`.
async fn shutdown_signal(shutdown: CancellationToken) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = term => {}
    }
    tracing::info!("shutting down");
    shutdown.cancel();
}
