use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().json().init();
    tracing::info!(component = "velocity-log-processor", "starting (stub)");
    Ok(())
}
