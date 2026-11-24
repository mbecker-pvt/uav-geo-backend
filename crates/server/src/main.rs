//! Binary entry point.

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let config = server::Config::from_env()?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting uav-geo-backend"
    );

    let running = server::start(config).await?;
    server::shutdown_signal().await;
    running.shutdown.cancel();
    running.wait().await;
    Ok(())
}
