use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().json().init();
    tracing::info!(component = "velocity-log-collector", "starting (stub)");
    Ok(())
}
