use anyhow::Result;
use clap::Parser;

/// Initialize config and database
#[derive(Parser)]
pub struct InitArgs {
    /// Path to configuration file to create
    #[arg(short, long, default_value = "/etc/multifs/config.toml")]
    pub config: String,

    /// Force overwrite existing config
    #[arg(short, long)]
    pub force: bool,
}

pub fn run(args: InitArgs) -> Result<()> {
    let cfg_path = &args.config;

    if std::path::Path::new(cfg_path).exists() && !args.force {
        anyhow::bail!(
            "Config file already exists at {}. Use --force to overwrite.",
            cfg_path
        );
    }

    // Create default config
    let default_config = include_str!("../../config.example.toml");
    if let Some(parent) = std::path::Path::new(cfg_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(cfg_path, default_config)?;
    tracing::info!("Created default config at {}", cfg_path);

    // Initialize SQLite database
    let cfg = crate::config::load(cfg_path)?;
    let _meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)?;
    tracing::info!("Initialized metadata database at {}", cfg.storage.meta_db_path);

    println!("✅ pCloudFS initialized!");
    println!("   Config: {}", cfg_path);
    println!("   Database: {}", cfg.storage.meta_db_path);
    println!();
    println!("Next steps:");
    println!("  1. Add pCloud accounts:  multifs account add <email>");
    println!("  2. Check everything:     multifs check");
    println!("  3. Start the daemon:     multifs serve");

    Ok(())
}
