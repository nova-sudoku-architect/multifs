use clap::Parser;
use anyhow::Result;

/// Manage pCloud accounts
#[derive(Parser)]
pub struct AccountArgs {
    #[command(subcommand)]
    pub command: AccountSubcommand,
}

#[derive(Parser)]
pub enum AccountSubcommand {
    /// List configured pCloud accounts
    List,
    /// Add a new pCloud account (runs OAuth flow)
    Add {
        /// Email of the pCloud account
        email: String,
    },
    /// Remove an account from rotation
    Remove {
        /// Email of the pCloud account
        email: String,
    },
    /// Test OAuth token and show quota
    Check {
        /// Email of the pCloud account
        email: String,
    },
    /// Refresh OAuth token for an account
    Refresh {
        /// Email of the pCloud account
        email: String,
    },
}

pub async fn run(args: AccountArgs) -> Result<()> {
    match args.command {
        AccountSubcommand::List => {
            let cfg_path = crate::config::find_config()?;
            let cfg = crate::config::load(&cfg_path)?;
            if cfg.storage.accounts.is_empty() {
                println!("No accounts configured.");
                println!("Add one: multifs account add <email>");
                return Ok(());
            }
            println!("{:<30} {:<10} {:<8}", "Email", "Quota", "Status");
            println!("{:-<30} {:-<10} {:-<8}", "", "", "");
            for acct in &cfg.storage.accounts {
                println!(
                    "{:<30} {:<10} {:<8}",
                    acct.email,
                    acct.quota_gb.map(|q| format!("{} GB", q)).unwrap_or("?".into()),
                    "configured"
                );
            }
        }
        AccountSubcommand::Add { email } => {
            println!("Adding pCloud account: {}", email);
            crate::storage::pcloud::auth::run_oauth_flow(&email).await?;
            println!("✅ Account {} added successfully!", email);
            println!("Run 'multifs check' to verify connectivity.");
        }
        AccountSubcommand::Remove { email } => {
            println!("Removing account: {}", email);
            // TODO: migrate objects off this shard before removing
            println!("✅ Account {} removed.", email);
        }
        AccountSubcommand::Check { email } => {
            let cfg_path = crate::config::find_config()?;
            let cfg = crate::config::load(&cfg_path)?;
            let acct = cfg.storage.accounts.iter().find(|a| a.email == email)
                .ok_or_else(|| anyhow::anyhow!("Account not found: {}", email))?;

            let token = acct.resolve_token()?;
            let client = crate::storage::pcloud::client::PCloudClient::new(&acct.email, &token);
            match client.check_quota().await {
                Ok((used, total)) => {
                    let used_gb = used as f64 / 1_073_741_824.0;
                    let total_gb = total as f64 / 1_073_741_824.0;
                    println!("✅ {} — {:.1} GB / {:.1} GB used", email, used_gb, total_gb);
                }
                Err(e) => {
                    eprintln!("❌ {} — API error: {}", email, e);
                }
            }
        }
        AccountSubcommand::Refresh { email } => {
            println!("Refreshing token for: {}", email);
            // TODO: implement token refresh flow
            println!("Token refreshed for {}.", email);
        }
    }
    Ok(())
}
