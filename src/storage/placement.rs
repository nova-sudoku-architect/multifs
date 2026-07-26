/// Placement Strategy: Round-robin across available pCloud accounts.
///
/// For 7 chunks across 6 accounts, accounts wrap around:
///   chunk 0 → account[0]
///   chunk 1 → account[1]
///   ...
///   chunk 6 → account[0] (wraps to index 0)

/// Maps chunk index to account assignment for a specific stripe.
#[derive(Debug, Clone)]
pub struct PlacementPlan {
    /// (chunk_index, account_email) for each chunk in the stripe
    pub account_assignments: Vec<(u32, String)>,
}

impl PlacementPlan {
    /// Get the account email for a specific chunk index
    pub fn account_for_chunk(&self, chunk_index: u32) -> Option<&str> {
        self.account_assignments
            .iter()
            .find(|(idx, _)| *idx == chunk_index)
            .map(|(_, email)| email.as_str())
    }

    /// Get the set of unique account emails used in this plan
    pub fn unique_accounts(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for (_, email) in &self.account_assignments {
            if seen.insert(email.clone()) {
                result.push(email.clone());
            }
        }
        result.sort();
        result
    }
}

/// Plan placement of `total_chunks` chunks across `accounts` using round-robin.
///
/// Returns a `PlacementPlan` mapping each chunk index to an account email.
pub fn plan_placement(accounts: &[String], total_chunks: u32) -> PlacementPlan {
    let mut assignments = Vec::with_capacity(total_chunks as usize);
    for chunk_idx in 0..total_chunks {
        let acct_idx = (chunk_idx as usize) % accounts.len();
        let email = accounts[acct_idx].clone();
        assignments.push((chunk_idx, email));
    }
    PlacementPlan {
        account_assignments: assignments,
    }
}

/// Get the account email for a specific chunk index using round-robin.
///
/// Returns a reference to the account string at `chunk_index % accounts.len()`.
pub fn get_account_for_chunk(accounts: &[String], chunk_index: u32) -> &str {
    let idx = (chunk_index as usize) % accounts.len();
    &accounts[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_accounts() -> Vec<String> {
        vec![
            "nova-video-00@agentmail.to".to_string(),
            "nova-video-01@agentmail.to".to_string(),
            "nova-video-02@agentmail.to".to_string(),
            "nova-video-10@agentmail.to".to_string(),
            "nova-video-11@agentmail.to".to_string(),
            "nova-video-12@agentmail.to".to_string(),
        ]
    }

    // ---- Test 1: 7 chunks across 6 accounts (wrapping) ----

    #[test]
    fn test_round_robin_7_chunks_6_accounts() {
        let accounts = test_accounts();
        let plan = plan_placement(&accounts, 7);

        assert_eq!(plan.account_assignments.len(), 7);

        // Verify assignments
        assert_eq!(plan.account_assignments[0].0, 0);
        assert_eq!(plan.account_assignments[0].1, accounts[0]);

        assert_eq!(plan.account_assignments[1].0, 1);
        assert_eq!(plan.account_assignments[1].1, accounts[1]);

        assert_eq!(plan.account_assignments[5].0, 5);
        assert_eq!(plan.account_assignments[5].1, accounts[5]);

        // Chunk 6 should wrap back to account[0]
        assert_eq!(plan.account_assignments[6].0, 6);
        assert_eq!(plan.account_assignments[6].1, accounts[0]);
    }

    // ---- Test 2: 5 chunks only uses 5 of 6 accounts ----

    #[test]
    fn test_round_robin_5_chunks() {
        let accounts = test_accounts();
        let plan = plan_placement(&accounts, 5);

        assert_eq!(plan.account_assignments.len(), 5);

        // Verify each chunk goes to a different account (no wrapping)
        let used_accounts = plan.unique_accounts();
        assert_eq!(used_accounts.len(), 5);
        assert!(used_accounts.contains(&accounts[0]));
        assert!(used_accounts.contains(&accounts[4]));

        // Account[5] should NOT be used
        assert!(!used_accounts.contains(&accounts[5]));
    }

    // ---- Test 3: 1 chunk uses 1 account ----

    #[test]
    fn test_round_robin_1_chunk() {
        let accounts = test_accounts();
        let plan = plan_placement(&accounts, 1);

        assert_eq!(plan.account_assignments.len(), 1);
        assert_eq!(plan.account_assignments[0].0, 0);
        assert_eq!(plan.account_assignments[0].1, accounts[0]);

        let used = plan.unique_accounts();
        assert_eq!(used.len(), 1);
        assert_eq!(used[0], accounts[0]);
    }

    // ---- Test 4: even distribution ----

    #[test]
    fn test_even_distribution() {
        let accounts = test_accounts();
        let total_chunks = 42; // 6 accounts × 7 rounds = exactly even

        let plan = plan_placement(&accounts, total_chunks);
        assert_eq!(plan.account_assignments.len() as u32, total_chunks);

        // Count how many times each account is used
        let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for (_, email) in &plan.account_assignments {
            *counts.entry(email.clone()).or_insert(0) += 1;
        }

        // With 42 chunks and 6 accounts, each account should get exactly 7
        assert_eq!(counts.len(), 6, "All 6 accounts should be used");
        for acct in &accounts {
            assert_eq!(
                *counts.get(acct).unwrap_or(&0),
                7,
                "Account {} should have exactly 7 chunks",
                acct
            );
        }
    }

    // ---- Test 6: get_account_for_chunk helper ----

    #[test]
    fn test_get_account_for_chunk() {
        let accounts = test_accounts();

        assert_eq!(get_account_for_chunk(&accounts, 0), accounts[0]);
        assert_eq!(get_account_for_chunk(&accounts, 5), accounts[5]);
        assert_eq!(get_account_for_chunk(&accounts, 6), accounts[0]); // wraps
        assert_eq!(get_account_for_chunk(&accounts, 12), accounts[0]); // wraps: 12 % 6 = 0
        assert_eq!(get_account_for_chunk(&accounts, 13), accounts[1]); // 13 % 6 = 1
    }

    // ---- Test 7: account_for_chunk on PlacementPlan ----

    #[test]
    fn test_placement_plan_account_for_chunk() {
        let accounts = test_accounts();
        let plan = plan_placement(&accounts, 7);

        assert_eq!(plan.account_for_chunk(0), Some(accounts[0].as_str()));
        assert_eq!(plan.account_for_chunk(5), Some(accounts[5].as_str()));
        assert_eq!(plan.account_for_chunk(6), Some(accounts[0].as_str()));

        // Non-existent chunk index
        assert_eq!(plan.account_for_chunk(99), None);
    }

    // ---- Test 8: plan with 0 chunks returns empty ----

    #[test]
    fn test_plan_placement_zero_chunks() {
        let accounts: Vec<String> = vec![];
        let plan = plan_placement(&accounts, 0);
        assert!(plan.account_assignments.is_empty());
    }

    // ---- Test 9: single account, many chunks ----

    #[test]
    fn test_single_account_placement() {
        let accounts = vec!["single@account.to".to_string()];
        let plan = plan_placement(&accounts, 10);

        assert_eq!(plan.account_assignments.len(), 10);
        // All chunks should go to the same account
        for (_, email) in &plan.account_assignments {
            assert_eq!(email, "single@account.to");
        }

        let unique = plan.unique_accounts();
        assert_eq!(unique.len(), 1);
        assert_eq!(unique[0], "single@account.to");
    }

    // ---- Test 10: planning for exactly 7 (5+2) chunks across 6 accounts ----

    #[test]
    fn test_standard_erasure_stripe() {
        let accounts = test_accounts();
        let plan = plan_placement(&accounts, 7); // 5 data + 2 parity

        assert_eq!(plan.account_assignments.len(), 7);

        // Verify specific assignments for a standard stripe
        let stripe: Vec<(&str, u32)> = plan
            .account_assignments
            .iter()
            .map(|(idx, email)| (email.as_str(), *idx))
            .collect();

        // Account 0 gets chunks 0 and 6 (wraps)
        assert!(stripe.contains(&(accounts[0].as_str(), 0)));
        assert!(stripe.contains(&(accounts[0].as_str(), 6)));

        // Each other account gets exactly one chunk
        assert!(stripe.contains(&(accounts[1].as_str(), 1)));
        assert!(stripe.contains(&(accounts[2].as_str(), 2)));
        assert!(stripe.contains(&(accounts[3].as_str(), 3)));
        assert!(stripe.contains(&(accounts[4].as_str(), 4)));
        assert!(stripe.contains(&(accounts[5].as_str(), 5)));

        // Verify we have exactly 6 unique accounts and all are used
        let unique = plan.unique_accounts();
        assert_eq!(unique.len(), 6);
        for acct in &accounts {
            assert!(unique.contains(acct), "Missing {}", acct);
        }
    }
}
