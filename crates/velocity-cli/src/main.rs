use anyhow::Result;
use clap::Parser;

/// velocity — Velocity command-line interface
#[derive(Debug, Parser)]
#[command(name = "velocity", version, about, long_about = None)]
struct Cli {}

#[tokio::main]
async fn main() -> Result<()> {
    let _cli = Cli::parse();
    // stdout is the CLI's primary output channel — allow here, not workspace-wide
    #[allow(clippy::print_stdout)]
    {
        println!("velocity (stub) — Phase 9 lands the real command set");
    }
    Ok(())
}
