use crate::config::AccountConfig;
use crate::storage::metadata::MetadataDb;

/// Manages object distribution across pCloud accounts
pub struct ShardManager {
    accounts: Vec<AccountConfig>,
    usage: Vec<(String, i64)>, // (email, bytes_used_on_this_shard)
    strategy: ShardStrategy,
}

enum ShardStrategy {
    FillLevel,   // Prefer least-full accounts
    RoundRobin,  // Cycle through accounts
}

impl ShardManager {
    pub fn new(accounts: &[AccountConfig], _meta: &MetadataDb) -> Self {
        let strategy = ShardStrategy::FillLevel;
        let usage: Vec<(String, i64)> = accounts.iter().map(|a| (a.email.clone(), 0i64)).collect();
        Self {
            accounts: accounts.to_vec(),
            usage,
            strategy,
        }
    }

    /// Select the best account for storing an object of `size` bytes
    pub fn select_account(&mut self, size: i64) -> anyhow::Result<AccountConfig> {
        if self.accounts.is_empty() {
            anyhow::bail!("No pCloud accounts configured. Add one with 'multifs account add'.");
        }

        match self.strategy {
            ShardStrategy::FillLevel => {
                // Find account with lowest usage/quota ratio
                let mut best_idx = 0;
                let mut best_ratio = f64::MAX;

                for (i, acct) in self.accounts.iter().enumerate() {
                    let quota = acct.quota_gb.unwrap_or(10) as i64 * 1_073_741_824;
                    let current_used = self.usage.iter()
                        .find(|(e, _)| e == &acct.email)
                        .map(|(_, u)| *u)
                        .unwrap_or(0);

                    if current_used + size > quota {
                        continue; // Skip if this account would overflow
                    }

                    let ratio = current_used as f64 / quota as f64;
                    if ratio < best_ratio {
                        best_ratio = ratio;
                        best_idx = i;
                    }
                }

                Ok(self.accounts[best_idx].clone())
            }
            ShardStrategy::RoundRobin => {
                // Simple round-robin — pick the one with least usage
                let mut best_idx = 0;
                let mut min_used = i64::MAX;

                for (i, acct) in self.accounts.iter().enumerate() {
                    let current_used = self.usage.iter()
                        .find(|(e, _)| e == &acct.email)
                        .map(|(_, u)| *u)
                        .unwrap_or(0);

                    if current_used < min_used {
                        min_used = current_used;
                        best_idx = i;
                    }
                }

                Ok(self.accounts[best_idx].clone())
            }
        }
    }

    /// Record that an account has used `size` more bytes
    pub fn record_usage(&mut self, email: &str, size: i64) {
        if let Some(entry) = self.usage.iter_mut().find(|(e, _)| e == email) {
            entry.1 += size;
        }
    }

    pub fn accounts(&self) -> &[AccountConfig] {
        &self.accounts
    }
}
