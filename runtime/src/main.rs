use anyhow::Result;
use tracing_subscriber;

mod daemon;
use daemon::Daemon;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    tracing::info!("Starting AIOS Runtime v0.1.0");

    let daemon = Daemon::new().await?;
    daemon.serve().await?;

    Ok(())
}
