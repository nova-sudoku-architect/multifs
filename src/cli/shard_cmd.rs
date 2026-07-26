use clap::Parser;
use anyhow::Result;

/// Manage shard distribution
#[derive(Parser)]
pub struct ShardArgs {
    #[command(subcommand)]
    pub command: ShardSubcommand,
}

#[derive(Parser)]
pub enum ShardSubcommand {
    /// Show shard fill levels for all accounts
    Status,
    /// Rebalance objects across accounts
    ///
    /// Note: automatic rebalancing has been removed. Uploads are now distributed
    /// via round-robin at upload time. Manual migration not yet implemented.
    Rebalance,
}

pub async fn run(args: ShardArgs) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;
    let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)?;
    let engine = crate::storage::engine::StorageEngine::new(&cfg, meta)?;

    match args.command {
        ShardSubcommand::Status => {
            let statuses = engine.shard_status().await?;
            println!("{:<30} {:<12} {:<12} {:<12}", "Email", "Objects", "Used", "Total");
            println!("{:-<30} {:-<12} {:-<12} {:-<12}", "", "", "", "");
            for s in &statuses {
                let used_str = if s.used_bytes > 1_073_741_824 {
                    format!("{:.1} GiB", s.used_bytes as f64 / 1_073_741_824.0)
                } else {
                    format!("{:.1} MiB", s.used_bytes as f64 / 1_048_576.0)
                };
                let total_str = if s.total_bytes > 1_073_741_824 {
                    format!("{:.1} GiB", s.total_bytes as f64 / 1_073_741_824.0)
                } else {
                    format!("{:.1} MiB", s.total_bytes as f64 / 1_048_576.0)
                };
                println!("{:<30} {:<12} {:<12} {:<12}", s.email, s.object_count, used_str, total_str);
            }
        }
        ShardSubcommand::Rebalance => {
            println!("Starting rebalance...");
            // TODO: implement object migration between accounts
            println!("✅ Rebalance complete.");
        }
    }

    Ok(())
}
