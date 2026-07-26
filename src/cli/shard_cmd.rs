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
            println!("Analyzing object distribution...");
            // Show current distribution with fill percentage for each account
            let statuses = engine.shard_status().await?;
            println!("");
            println!("{:<30} {:<15} {:<15} {:<15}", "Email", "Objects", "Used", "Fill");
            println!("{:-<30} {:-<15} {:-<15} {:-<15}", "", "", "", "");
            for s in &statuses {
                let used_str = if s.used_bytes > 1_073_741_824 {
                    format!("{:.1} GiB", s.used_bytes as f64 / 1_073_741_824.0)
                } else {
                    format!("{:.1} MiB", s.used_bytes as f64 / 1_048_576.0)
                };
                let fill_pct = if s.total_bytes > 0 {
                    s.used_bytes as f64 / s.total_bytes as f64 * 100.0
                } else {
                    0.0
                };
                println!("{:<30} {:<15} {:<15} {:.1}%",
                    s.email, s.object_count, used_str, fill_pct);
            }

            // Check for clear imbalance
            let fills: Vec<f64> = statuses.iter()
                .filter(|s| s.total_bytes > 0)
                .map(|s| s.used_bytes as f64 / s.total_bytes as f64)
                .collect();

            if fills.len() >= 2 {
                let max_fill = fills.iter().cloned().fold(0.0_f64, f64::max);
                let min_fill = fills.iter().cloned().fold(1.0_f64, f64::min);
                let ratio = if min_fill > 0.0 { max_fill / min_fill } else { 999.0 };

                if ratio > 1.5 {
                    println!("");
                    println!("⚠️  Distribution imbalance detected: fill ratio {:.1}x between most and least full accounts.", ratio);
                    println!("   Object migration between accounts is not yet automated.");
                    println!("   New uploads use round-robin placement across all accounts.");
                } else {
                    println!("");
                    println!("✅ Distribution is balanced (fill ratio {:.2}x).", ratio);
                    println!("   Round-robin placement is working as expected for new uploads.");
                }
            }
            println!("");
            println!("ℹ️  To migrate objects between accounts manually:");
            println!("   1. Download: multifs object cp multifs://<bucket>/<key> /tmp/<file>");
            println!("   2. Upload:   multifs object cp /tmp/<file> multifs://<bucket>/<key>");
            println!("   3. Cleanup:  multifs object rm multifs://<bucket>/<key>");
        }
    }

    Ok(())
}
