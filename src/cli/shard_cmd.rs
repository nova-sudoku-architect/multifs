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
    /// Rebalance objects/chunks from over-full accounts to under-utilized ones
    ///
    /// Migrates data by downloading from the old account and uploading to the
    /// least-full account. Each chunk or whole-file object is processed
    /// individually — crash-safe since old data isn't deleted until after
    /// the new copy is confirmed.
    Rebalance {
        /// Dry-run: show what would be migrated without actually moving anything
        #[arg(long)]
        dry_run: bool,
    },
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
        ShardSubcommand::Rebalance { dry_run } => {
            println!("Rebalancing object distribution across accounts...\n");
            let (migrated, bytes) = engine.rebalance(dry_run).await?;
            if migrated == 0 && !dry_run {
                println!("✅ Already balanced — no objects needed migration.");
            } else if migrated > 0 {
                let total = if bytes > 1_073_741_824 {
                    format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0)
                } else if bytes > 1_048_576 {
                    format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
                } else {
                    format!("{} bytes", bytes)
                };
                println!("✅ Rebalance complete: {} items migrated ({})", migrated, total);
            }
        }
    }

    Ok(())
}
