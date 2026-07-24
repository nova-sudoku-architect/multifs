use clap::Parser;
use anyhow::Result;

/// Show daemon/account status
#[derive(Parser)]
pub struct StatusArgs;

/// Validate configuration and check all accounts
#[derive(Parser)]
pub struct CheckArgs {
    /// Path to configuration file
    #[arg(short, long)]
    pub config: Option<String>,
}

pub async fn run_status() -> Result<()> {
    let cfg_path = crate::config::find_config().ok();
    let cfg = if let Some(ref path) = cfg_path {
        crate::config::load(path).ok()
    } else {
        None
    };

    match cfg {
        Some(cfg) => {
            let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path).ok();
            let engine = match &meta {
                Some(m) => crate::storage::engine::StorageEngine::new(&cfg, m.clone()).ok(),
                None => None,
            };

            println!("pCloudFS Status");
            println!("==============");
            println!("Config: {}", cfg_path.unwrap_or_default());
            println!("Database: {}", cfg.storage.meta_db_path);
            println!("Accounts: {}", cfg.storage.accounts.len());

            if let Some(ref engine) = engine {
                match engine.shard_status().await {
                    Ok(statuses) => {
                        for s in &statuses {
                            println!("  {}: {} objects, {}/{} used",
                                s.email, s.object_count, s.used_bytes, s.total_bytes);
                        }
                    }
                    Err(e) => println!("  Could not query shards: {}", e),
                }
            }
        }
        None => println!("Not configured. Run 'multifs init' first."),
    }

    Ok(())
}

pub async fn run_check(config_path: Option<String>) -> Result<()> {
        let cfg_path = match config_path {
        Some(p) => p,
        None => crate::config::find_config()?,
    };
    let cfg = crate::config::load(&cfg_path)?;

    println!("Checking pCloudFS configuration...");
    println!("Config file: {}", cfg_path);
    println!();

    // Check database
    println!("📁 Metadata database...");
    match crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path) {
        Ok(_) => println!("  ✅ OK: {}", cfg.storage.meta_db_path),
        Err(e) => println!("  ❌ Error: {}", e),
    }

    // Check each account
    println!("\n📧 pCloud accounts...");
    for acct in &cfg.storage.accounts {
        match acct.resolve_token() {
            Ok(token) => {
                let client =
                    crate::storage::pcloud::client::PCloudClient::new(&acct.email, &token);
                match client.check_quota().await {
                    Ok((used, total)) => {
                        let used_gb = used as f64 / 1_073_741_824.0;
                        let total_gb = total as f64 / 1_073_741_824.0;
                        println!("  ✅ {} — {:.1} GB / {:.1} GB used", acct.email, used_gb, total_gb);
                    }
                    Err(e) => println!("  ❌ {} — API error: {}", acct.email, e),
                }
            }
            Err(e) => println!("  ❌ {} — Token not configured: {}", acct.email, e),
        }
    }

    Ok(())
}
