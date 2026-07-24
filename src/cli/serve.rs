use clap::Parser;
use anyhow::Result;

/// Start the pCloudFS daemon
#[derive(Parser)]
pub struct ServeArgs {
    /// Path to configuration file
    #[arg(short, long, default_value = "/etc/multifs/config.toml")]
    pub config: String,
}

pub async fn run(args: ServeArgs) -> Result<()> {
    let cfg = crate::config::load(&args.config)?;
    tracing::info!("Starting pCloudFS daemon with config: {}", args.config);
    crate::server::run(cfg).await
}
