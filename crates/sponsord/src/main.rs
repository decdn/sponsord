use std::sync::Arc;

use sponsord::config::DaemonConfig;
use sponsord::http::{self, ApiState};
use sponsord_core::Sponsor;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cfg = DaemonConfig::from_env()?;
    let (keystore, password) = (cfg.treasury_keystore.clone(), cfg.treasury_password.clone());
    let signer = tokio::task::spawn_blocking(move || {
        decdn_incentive::eth_identity::load_signer(&keystore, &password)
    })
    .await??;
    let sponsor = Arc::new(Sponsor::connect(signer, cfg.chain, cfg.limits).await?);
    let shutdown = CancellationToken::new();
    tokio::spawn(sponsor.keeper(cfg.keeper, shutdown.clone()));
    let app = http::router(ApiState::new(sponsor, Arc::from(cfg.api_token)));
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, "sponsord listening");
    axum::serve(listener, app).await?;
    Ok(())
}
