use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use remote_config::{
    build_router, Config, ExchangeRateService, GeoService, RiskCheckerService, WalletConfigService,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Parser)]
#[command(name = "remote-config", about = "Minimal remote-config service")]
struct Cli {
    /// Path to TOML configuration file
    #[arg(long, default_value = "config/default.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config =
        Config::load(&cli.config).with_context(|| format!("load config {}", cli.config))?;

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(&config.logging.level))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let wallet_config_service = Arc::new(
        WalletConfigService::new(config.wallet_configs_path())
            .context("initialize wallet config service")?,
    );
    let exchange_rate_service = Arc::new(ExchangeRateService::new(&config.exchange_rate.api_key));
    let risk_checker_service = Arc::new(RiskCheckerService::new(&config.risk_checker));
    let geo_service =
        Arc::new(GeoService::new(config.geo_config_path()).context("initialize geo service")?);

    let app = build_router(
        &config,
        wallet_config_service,
        exchange_rate_service,
        risk_checker_service,
        geo_service,
    );
    let address = config.get_server_address();
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .with_context(|| format!("bind {address}"))?;

    tracing::info!("remote-config listening on {address}");
    axum::serve(listener, app).await.context("server error")?;
    Ok(())
}
