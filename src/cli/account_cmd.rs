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
    /// Add a new pCloud account with email and OAuth token. Validates token, auto-detects quota, and saves to config.
    Add {
        /// Email of the pCloud account
        email: String,
        /// OAuth access token (get from pCloud OAuth app)
        token: String,
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

/// Generate the environment variable name for a pCloud token.
/// Example: "nova-video@agentmail.to" → "PCLOUD_TOKEN_NOVA_VIDEO_AT_AGENTMAIL_TO"
pub fn token_env_name(email: &str) -> String {
    let suffix = email
        .to_uppercase()
        .replace('@', "_AT_")
        .replace('.', "_")
        .replace('-', "_");
    format!("PCLOUD_TOKEN_{}", suffix)
}

/// Generate a mount prefix for the n-th account.
/// Example: index 0 → "/multifs/00", index 5 → "/multifs/05"
pub fn mount_prefix_for_index(index: usize) -> String {
    format!("/multifs/{:02}", index)
}

pub async fn run(args: AccountArgs) -> Result<()> {
    match args.command {
        AccountSubcommand::List => {
            let cfg_path = crate::config::find_config()?;
            let cfg = crate::config::load(&cfg_path)?;
            if cfg.storage.accounts.is_empty() {
                println!("No accounts configured.");
                println!("Add one: multifs account add <email> <token>");
                return Ok(());
            }
            println!("{:<30} {:<10} {:<20} {:<15}", "Email", "Quota", "Mount Prefix", "Token Env");
            println!("{:-<30} {:-<10} {:-<20} {:-<15}", "", "", "", "");
            for acct in &cfg.storage.accounts {
                let quota = acct.quota_gb.map(|q| format!("{} GB", q)).unwrap_or("?".into());
                let token_env = acct.token_env.as_deref().unwrap_or("-");
                println!(
                    "{:<30} {:<10} {:<20} {:<15}",
                    acct.email, quota, acct.mount_prefix, token_env
                );
            }
        }

        AccountSubcommand::Add { email, token } => {
            println!("============================================");
            println!("  Adding pCloud account: {}", email);
            println!("============================================");
            println!();

            // 1. Validate token by checking quota via pCloud API
            let client = reqwest::Client::new();
            let quota_url = format!("https://eapi.pcloud.com/userinfo?access_token={}", token);
            let resp = client.get(&quota_url).send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("Token validation failed (HTTP {}). Check your token.", resp.status());
            }
            let info: serde_json::Value = resp.json().await?;
            if info.get("result").and_then(|v| v.as_i64()) != Some(0) {
                let error = info.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
                anyhow::bail!("Invalid token: pCloud API error: {}", error);
            }

            let quota_gb = info["quota"]
                .as_i64()
                .map(|q| (q as f64 / 1_073_741_824.0).ceil() as u64);

            let used = info["usedquota"].as_i64().unwrap_or(0);
            let used_gb = used as f64 / 1_073_741_824.0;

            match quota_gb {
                Some(q) => println!("✅ Token valid — Quota: {} GB ({} GB used)", q, format!("{:.1}", used_gb)),
                None => println!("✅ Token valid — Used: {:.1} GB", used_gb),
            }
            println!();

            // 2. Auto-assign mount prefix
            let cfg_path = crate::config::find_config()?;
            let existing_count = {
                let cfg = crate::config::load(&cfg_path)?;
                cfg.storage.accounts.len()
            };
            let mount_prefix = mount_prefix_for_index(existing_count);

            // 3. Generate token env name
            let token_env = token_env_name(&email);

            println!("Mount prefix: {}", mount_prefix);
            println!("Token env:    {}", token_env);
            println!();

            // 4. Add to config
            let mut cfg = crate::config::load(&cfg_path)?;

            if cfg.storage.accounts.iter().any(|a| a.email == email) {
                anyhow::bail!("Account '{}' already exists in config.", email);
            }

            let new_account = crate::config::AccountConfig {
                email: email.clone(),
                backend_type: Some("pcloud".to_string()),
                token_env: Some(token_env.clone()),
                mount_prefix,
                quota_gb,
                path: None,
                priority: None,
                token_override: None,
            };

            cfg.storage.accounts.push(new_account);
            crate::config::save(&cfg_path, &cfg)?;

            // 5. Print instructions
            println!("✅ Account added to config: {}", cfg_path);
            println!();
            println!("📝 Add this line to your ~/.openclaw/.env file:");
            println!();
            println!("   {}={}", token_env, token);
            println!();
            println!("Then restart MultiFS:");
            println!();
            println!("   sudo systemctl restart multifs");
        }

        AccountSubcommand::Remove { email } => {
            let cfg_path = crate::config::find_config()?;
            let mut cfg = crate::config::load(&cfg_path)?;
            let before = cfg.storage.accounts.len();
            cfg.storage.accounts.retain(|a| a.email != email);
            if cfg.storage.accounts.len() == before {
                println!("⚠️  Account not found: {}", email);
                return Ok(());
            }
            crate::config::save(&cfg_path, &cfg)?;
            println!("✅ Account {} removed from config.", email);
            println!("💡 Objects on this account are still in storage. Run 'multifs shard rebalance' to migrate.");
        }

        AccountSubcommand::Check { email } => {
            let cfg_path = crate::config::find_config()?;
            let cfg = crate::config::load(&cfg_path)?;
            let acct = cfg.storage.accounts.iter()
                .find(|a| a.email == email)
                .ok_or_else(|| anyhow::anyhow!("Account not found: {}", email))?;

            let token = acct.resolve_token()?;
            let client = reqwest::Client::new();
            let url = format!("https://eapi.pcloud.com/userinfo?access_token={}", token);
            let resp = client.get(&url).send().await?;
            let info: serde_json::Value = resp.json().await?;

            let quota = info["quota"].as_i64().unwrap_or(0);
            let used = info["usedquota"].as_i64().unwrap_or(0);
            let email_from_api = info["email"].as_str().unwrap_or(&email);
            let used_gb = used as f64 / 1_073_741_824.0;
            let total_gb = quota as f64 / 1_073_741_824.0;

            println!("✅ {} ({})", email, email_from_api);
            println!("   Used: {:.1} GB / {:.1} GB", used_gb, total_gb);
            println!("   Mount: {}", acct.mount_prefix);

            let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)?;
            let managed_objects = meta.list_all_objects()
                .map(|objs| objs.iter().filter(|o| o.account_email == email).count())
                .unwrap_or(0);
            println!("   Managed objects: {}", managed_objects);
        }

        AccountSubcommand::Refresh { email } => {
            println!("Refreshing token for: {}", email);
            let cfg_path = crate::config::find_config()?;
            let _cfg = crate::config::load(&cfg_path)?;

            let new_token = crate::storage::pcloud::auth::run_oauth_flow(&email).await?;

            let token_env = token_env_name(&email);

            println!("✅ Token refreshed for {}.", email);
            println!();
            println!("📝 Update ~/.openclaw/.env:");
            println!("   {}={}", token_env, new_token);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_env_name_simple() {
        assert_eq!(
            token_env_name("nova-video@agentmail.to"),
            "PCLOUD_TOKEN_NOVA_VIDEO_AT_AGENTMAIL_TO"
        );
    }

    #[test]
    fn test_token_env_name_with_dots() {
        assert_eq!(
            token_env_name("user.name@example.com"),
            "PCLOUD_TOKEN_USER_NAME_AT_EXAMPLE_COM"
        );
    }

    #[test]
    fn test_token_env_name_with_hyphens() {
        assert_eq!(
            token_env_name("my-account@my-domain.io"),
            "PCLOUD_TOKEN_MY_ACCOUNT_AT_MY_DOMAIN_IO"
        );
    }

    #[test]
    fn test_token_env_name_all_caps_input() {
        assert_eq!(
            token_env_name("NOVA@EXAMPLE.COM"),
            "PCLOUD_TOKEN_NOVA_AT_EXAMPLE_COM"
        );
    }

    #[test]
    fn test_mount_prefix_first() {
        assert_eq!(mount_prefix_for_index(0), "/multifs/00");
    }

    #[test]
    fn test_mount_prefix_second() {
        assert_eq!(mount_prefix_for_index(1), "/multifs/01");
    }

    #[test]
    fn test_mount_prefix_tenth() {
        assert_eq!(mount_prefix_for_index(9), "/multifs/09");
    }

    #[test]
    fn test_mount_prefix_large() {
        assert_eq!(mount_prefix_for_index(99), "/multifs/99");
    }

    // ---- Config-based tests ----

    fn make_test_config(dir: &tempfile::TempDir, accounts: Vec<crate::config::AccountConfig>) -> String {
        let cfg = crate::config::Config {
            storage: crate::config::StorageConfig {
                accounts,
                meta_db_path: dir.path().join("meta.db").to_string_lossy().to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let path = dir.path().join("config.toml");
        let path_str = path.to_string_lossy().to_string();
        crate::config::save(&path_str, &cfg).unwrap();
        path_str
    }

    fn test_account(email: &str) -> crate::config::AccountConfig {
        crate::config::AccountConfig {
            email: email.to_string(),
            backend_type: Some("pcloud".to_string()),
            token_env: Some(token_env_name(email)),
            mount_prefix: "/mnt/test".to_string(),
            quota_gb: Some(10),
            path: None,
            priority: None,
            token_override: None,
        }
    }

    #[test]
    fn test_list_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = make_test_config(&dir, vec![]);
        let cfg = crate::config::load(&cfg_path).unwrap();
        assert!(cfg.storage.accounts.is_empty());
    }

    #[test]
    fn test_list_with_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = vec![
            test_account("a@example.com"),
            test_account("b@example.com"),
        ];
        let cfg_path = make_test_config(&dir, accounts);
        let cfg = crate::config::load(&cfg_path).unwrap();
        assert_eq!(cfg.storage.accounts.len(), 2);
        assert_eq!(cfg.storage.accounts[0].email, "a@example.com");
        assert_eq!(cfg.storage.accounts[1].email, "b@example.com");
    }

    #[test]
    fn test_remove_existing_account() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = vec![
            test_account("keep@example.com"),
            test_account("remove@example.com"),
        ];
        let cfg_path = make_test_config(&dir, accounts);
        let mut cfg = crate::config::load(&cfg_path).unwrap();
        let before = cfg.storage.accounts.len();
        cfg.storage.accounts.retain(|a| a.email != "remove@example.com");
        assert_eq!(cfg.storage.accounts.len(), before - 1);
        assert_eq!(cfg.storage.accounts[0].email, "keep@example.com");
    }

    #[test]
    fn test_remove_nonexistent_account() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = vec![test_account("only@example.com")];
        let cfg_path = make_test_config(&dir, accounts);
        let mut cfg = crate::config::load(&cfg_path).unwrap();
        let before = cfg.storage.accounts.len();
        cfg.storage.accounts.retain(|a| a.email != "notfound@example.com");
        assert_eq!(cfg.storage.accounts.len(), before, "Should not remove anything");
    }

    #[test]
    fn test_mount_prefix_sequence() {
        let prefixes: Vec<String> = (0..6).map(mount_prefix_for_index).collect();
        assert_eq!(prefixes, vec![
            "/multifs/00",
            "/multifs/01",
            "/multifs/02",
            "/multifs/03",
            "/multifs/04",
            "/multifs/05",
        ]);
    }

    #[test]
    fn test_token_env_name_idempotent() {
        let first = token_env_name("my.Account-email@domain.com");
        let second = token_env_name("my.Account-email@domain.com");
        assert_eq!(first, second);
    }
}
