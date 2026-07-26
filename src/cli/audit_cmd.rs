use clap::Parser;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Audit pCloud accounts — find orphaned files not managed by MultiFS
#[derive(Parser)]
pub struct AuditArgs {
    #[command(subcommand)]
    pub command: AuditSubcommand,
}

#[derive(Parser)]
pub enum AuditSubcommand {
    /// Scan a pCloud account and report files NOT tracked by MultiFS
    Scan {
        /// Email of the pCloud account to scan
        email: String,
        /// Only show summary, not individual files
        #[arg(long)]
        summary: bool,
    },
    /// List all files in a pCloud account (both managed and orphaned)
    ListFiles {
        /// Email of the pCloud account
        email: String,
    },
}

pub async fn run(args: AuditArgs) -> Result<()> {
    match args.command {
        AuditSubcommand::Scan { email, summary } => scan_account(&email, summary).await,
        AuditSubcommand::ListFiles { email } => list_account_files(&email).await,
    }
}

async fn scan_account(email: &str, summary: bool) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;

    // Find the account config
    let account = cfg.storage.accounts.iter()
        .find(|a| a.email == email)
        .ok_or_else(|| anyhow::anyhow!("Account not found: {}. Use `multifs account list` to see configured accounts.", email))?;

    // Find the token from env
    let token = if let Some(ref env_var) = account.token_env {
        std::env::var(env_var)
            .map_err(|_| anyhow::anyhow!("Environment variable {} not set for account {}", env_var, email))?
    } else {
        anyhow::bail!("No token_env configured for account {}", email);
    };

    let mount_prefix = if account.mount_prefix.is_empty() { "" } else { &account.mount_prefix };

    // Open metadata DB
    let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)
        .context("Failed to open metadata database")?;

    // Get all managed remote paths for this account
    let managed_paths: HashSet<String> = meta.list_all_objects()?
        .into_iter()
        .filter(|o| o.account_email == email)
        .map(|o| o.remote_path.clone())
        .collect();

    println!("🔍 Scanning pCloud account: {}", email);
    println!("   Mount prefix: {}", mount_prefix);
    println!("   Managed objects: {}", managed_paths.len());
    println!();

    // List all files on pCloud
    let client = reqwest::Client::new();
    let mut offset: i64 = 0;
    let mut orphaned_files: Vec<String> = Vec::new();
    let mut orphaned_total: i64 = 0;
    let mut all_files: Vec<(String, i64)> = Vec::new();

    // Use pCloud's listfolder with recursive to get all files
    loop {
        let url = if mount_prefix.is_empty() {
            format!("https://eapi.pcloud.com/listfolder?path=/&recursive=1&offset={}&limit=10000&access_token={}", offset, token)
        } else {
            format!("https://eapi.pcloud.com/listfolder?path={}&recursive=1&offset={}&limit=10000&access_token={}", mount_prefix, offset, token)
        };

        let resp = client.get(&url).send().await?;
        let json: serde_json::Value = resp.json().await?;

        let result = json.get("result").and_then(|v| v.as_i64()).unwrap_or(-1);
        if result != 0 {
            // Check metadata for error
            let error = json.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
            anyhow::bail!("pCloud API error listing folder: {} (result={})", error, result);
        }

        let metadata = json.get("metadata").context("Missing metadata in response")?;
        let contents = metadata.get("contents").and_then(|v| v.as_array()).ok_or_else(|| {
            anyhow::anyhow!("No contents array in pCloud response")
        })?;

        if contents.is_empty() {
            break;
        }

        for entry in contents {
            let is_folder = entry.get("isfolder").and_then(|v| v.as_i64()).unwrap_or(0) == 1;
            if is_folder {
                continue;
            }

            let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let path = entry.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let file_size = entry.get("size").and_then(|v| v.as_i64()).unwrap_or(0);

            // Only track actual files (skip metadata/.done/.log files for root scan)
            let full_remote_path = path.to_string();
            all_files.push((full_remote_path.clone(), file_size));

            if !managed_paths.contains(&full_remote_path) {
                orphaned_files.push(full_remote_path);
                orphaned_total += file_size;
            }
        }

        if contents.len() < 10000 {
            break;
        }
        offset += contents.len() as i64;
    }

    // Report
    println!("📊 Scan Results");
    println!("   Total files on pCloud: {}", all_files.len());
    println!("   Files managed by MultiFS: {}", managed_paths.len());
    println!("   Orphaned (not in MultiFS): {}", orphaned_files.len());

    if orphaned_total > 0 {
        let size_str = if orphaned_total > 1_000_000_000 {
            format!("{:.2} GB", orphaned_total as f64 / 1_000_000_000.0)
        } else if orphaned_total > 1_000_000 {
            format!("{:.2} MB", orphaned_total as f64 / 1_000_000.0)
        } else {
            format!("{} B", orphaned_total)
        };
        println!("   Orphaned size: {}", size_str);
    }

    if !summary && !orphaned_files.is_empty() {
        println!();
        println!("📄 Orphaned Files:");
        println!("{:-<80}", "");
        for f in &orphaned_files {
            println!("  {}", f);
        }
        println!();
        println!("Total: {} orphaned files", orphaned_files.len());
    }

    if orphaned_files.is_empty() {
        println!("✅ All files on this account are managed by MultiFS!");
    } else if !summary {
        println!();
        println!("💡 To delete orphaned files: multifs audit cleanup {} <remote-path>", email);
    }

    Ok(())
}

async fn list_account_files(email: &str) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;

    let account = cfg.storage.accounts.iter()
        .find(|a| a.email == email)
        .ok_or_else(|| anyhow::anyhow!("Account not found: {}", email))?;

    let token = if let Some(ref env_var) = account.token_env {
        std::env::var(env_var)
            .map_err(|_| anyhow::anyhow!("Environment variable {} not set", env_var))?
    } else {
        anyhow::bail!("No token_env configured for account {}", email);
    };

    let mount_prefix = if account.mount_prefix.is_empty() { "" } else { &account.mount_prefix };

    let client = reqwest::Client::new();
    let url = if mount_prefix.is_empty() {
        format!("https://eapi.pcloud.com/listfolder?path=/&recursive=1&limit=10000&access_token={}", token)
    } else {
        format!("https://eapi.pcloud.com/listfolder?path={}&recursive=1&limit=10000&access_token={}", mount_prefix, token)
    };

    let resp = client.get(&url).send().await?;
    let json: serde_json::Value = resp.json().await?;

    let metadata = json.get("metadata").context("Missing metadata")?;
    let contents = metadata.get("contents").and_then(|v| v.as_array()).ok_or_else(|| {
        anyhow::anyhow!("No contents array")
    })?;

    println!("{:<80} {:<12}", "Path", "Size");
    println!("{:-<80} {:-<12}", "", "");

    for entry in contents {
        let is_folder = entry.get("isfolder").and_then(|v| v.as_i64()).unwrap_or(0) == 1;
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let path = entry.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let file_size = entry.get("size").and_then(|v| v.as_i64()).unwrap_or(0);

        if is_folder {
            println!("📁 {:<77} {:<12}", path, "folder");
        } else {
            let size_str = if file_size > 1_000_000_000 {
                format!("{:.1} GB", file_size as f64 / 1_000_000_000.0)
            } else if file_size > 1_000_000 {
                format!("{:.1} MB", file_size as f64 / 1_000_000.0)
            } else if file_size > 1_000 {
                format!("{:.1} KB", file_size as f64 / 1_000.0)
            } else {
                format!("{} B", file_size)
            };
            println!("  {:<78} {}", path, size_str);
        }
    }

    println!();
    println!("Total entries: {}", contents.len());

    Ok(())
}
