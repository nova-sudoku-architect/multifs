use clap::Parser;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};

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
    /// Reconcile pCloud usage against the metadata DB across accounts.
    ///
    /// Lists every file (recursively) under each account's mount prefix and
    /// diffs it against the union of `versions.remote_path` +
    /// `multipart_parts.pcloud_path`. Anything on pCloud but in no DB table is a
    /// true orphan (pCloud Trash is NOT returned by `listfolder`). Reports
    /// per-account orphan counts + byte totals.
    Reconcile {
        /// Restrict reconciliation to a single account email (default: all pCloud accounts)
        #[arg(long)]
        account: Option<String>,
    },
    /// Delete orphaned files (safe: re-verifies each upload_id against the DB).
    ///
    /// Dry-run by default — reports what would be deleted. Pass `--apply` to
    /// actually delete. Multipart part files are grouped by their upload
    /// directory and only deleted if the upload_id has zero DB references.
    Cleanup {
        /// Restrict cleanup to a single account email (default: all pCloud accounts)
        #[arg(long)]
        account: Option<String>,
        /// Actually delete (default is a dry-run report)
        #[arg(long)]
        apply: bool,
    },
}

pub async fn run(args: AuditArgs) -> Result<()> {
    match args.command {
        AuditSubcommand::Scan { email, summary } => scan_account(&email, summary).await,
        AuditSubcommand::ListFiles { email } => list_account_files(&email).await,
        AuditSubcommand::Reconcile { account } => reconcile(account.as_deref()).await,
        AuditSubcommand::Cleanup { account, apply } => cleanup(account.as_deref(), apply).await,
    }
}

/// Is this account a pCloud backend (as opposed to `local` disk)?
fn is_pcloud(backend_type: &Option<String>) -> bool {
    backend_type.as_deref().map(|b| b != "local").unwrap_or(true)
}

/// Recursively list every file under `start_path` on pCloud.
///
/// Walks the tree level-by-level using non-recursive `listfolder` calls (one per
/// directory). Non-recursive responses carry an explicit `path` field on every
/// entry, so no path reconstruction is needed. `isfolder` is a JSON **boolean**,
/// not an integer — parsed with `as_bool` here (the previous `as_i64` read a
/// bool as `None` and therefore misclassified every folder as a file).
///
/// Returns `(full_path, size)` for each file, sorted.
///
/// Uses a single `listfolder?recursive=1` call (NOT a per-folder walk) so a
/// full 46-account reconcile is ~46 API calls instead of thousands — which also
/// avoids pCloud rate-limiting. The recursive response is a *nested* tree where
/// folder entries carry their own `contents[]` and only a `name` (no `path`), so
/// we reconstruct full paths while walking the tree.
async fn list_pcloud_files(
    client: &reqwest::Client,
    token: &str,
    start_path: &str,
) -> Result<Vec<(String, i64)>> {
    let url = format!(
        "https://eapi.pcloud.com/listfolder?path={}&recursive=1&access_token={}",
        start_path, token
    );
    let resp = client.get(&url).send().await?;
    let json: serde_json::Value = resp.json().await?;

    let result = json.get("result").and_then(|v| v.as_i64()).unwrap_or(-1);
    if result != 0 {
        let error = json.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
        anyhow::bail!("pCloud API error listing '{}': {} (result={})", start_path, error, result);
    }

    let mut files: Vec<(String, i64)> = Vec::new();
    let contents = json["metadata"]["contents"].as_array().cloned().unwrap_or_default();
    walk_contents(&contents, start_path, &mut files);
    files.sort();
    Ok(files)
}

/// Recursively walk a `recursive=1` `contents[]` array, reconstructing full paths.
fn walk_contents(entries: &[serde_json::Value], parent: &str, files: &mut Vec<(String, i64)>) {
    for entry in entries {
        let is_folder = entry.get("isfolder").and_then(|v| v.as_bool()).unwrap_or(false);
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let full = if parent == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", parent, name)
        };
        if is_folder {
            if let Some(children) = entry.get("contents").and_then(|v| v.as_array()) {
                walk_contents(children, &full, files);
            }
        } else {
            let size = entry.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
            files.push((full, size));
        }
    }
}

fn fmt_bytes(n: i64) -> String {
    if n >= 1_073_741_824 {
        format!("{:.2} GiB", n as f64 / 1_073_741_824.0)
    } else if n >= 1_048_576 {
        format!("{:.2} MiB", n as f64 / 1_048_576.0)
    } else if n >= 1_024 {
        format!("{:.2} KiB", n as f64 / 1_024.0)
    } else {
        format!("{} B", n)
    }
}

async fn scan_account(email: &str, summary: bool) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;

    let account = cfg.storage.accounts.iter()
        .find(|a| a.email == email)
        .ok_or_else(|| anyhow::anyhow!("Account not found: {}. Use `multifs account list` to see configured accounts.", email))?;

    let token = account.resolve_token()?;
    let mount_prefix = account.mount_prefix.clone();

    let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)
        .context("Failed to open metadata database")?;

    // Managed = committed versions' remote_paths ∪ multipart part paths.
    let managed_paths: HashSet<String> = meta
        .list_managed_remote_paths(email)?
        .into_iter()
        .collect();

    println!("🔍 Scanning pCloud account: {}", email);
    println!("   Mount prefix: {}", if mount_prefix.is_empty() { "(root)" } else { &mount_prefix });
    println!("   Managed paths (versions + parts): {}", managed_paths.len());
    println!();

    let client = reqwest::Client::new();
    let start = if mount_prefix.is_empty() { "/".to_string() } else { mount_prefix.clone() };
    let all_files = list_pcloud_files(&client, &token, &start).await?;

    let mut orphaned_files: Vec<(String, i64)> = Vec::new();
    let mut orphaned_total: i64 = 0;
    let mut managed_on_cloud: i64 = 0;
    for (path, size) in &all_files {
        if managed_paths.contains(path) {
            managed_on_cloud += 1;
        } else {
            orphaned_files.push((path.clone(), *size));
            orphaned_total += size;
        }
    }

    println!("📊 Scan Results");
    println!("   Total files on pCloud: {}", all_files.len());
    println!("   Files matched in MultiFS DB: {}", managed_on_cloud);
    println!("   Orphaned (not in MultiFS): {}", orphaned_files.len());

    if orphaned_total > 0 {
        println!("   Orphaned size: {}", fmt_bytes(orphaned_total));
    }

    if !summary && !orphaned_files.is_empty() {
        println!();
        println!("📄 Orphaned Files:");
        println!("{:-<80}", "");
        for (path, size) in &orphaned_files {
            println!("  {:<70} {}", path, fmt_bytes(*size));
        }
        println!();
        println!("Total: {} orphaned files", orphaned_files.len());
    }

    if orphaned_files.is_empty() {
        println!("✅ All files on this account are managed by MultiFS!");
    } else if !summary {
        println!();
        println!("💡 These files exist on pCloud but are referenced by no DB row.");
        println!("   (pCloud Trash is NOT included — `listfolder` does not return it.)");
    }

    Ok(())
}

async fn list_account_files(email: &str) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;

    let account = cfg.storage.accounts.iter()
        .find(|a| a.email == email)
        .ok_or_else(|| anyhow::anyhow!("Account not found: {}", email))?;

    let token = account.resolve_token()?;
    let mount_prefix = account.mount_prefix.clone();
    let client = reqwest::Client::new();
    let start = if mount_prefix.is_empty() { "/".to_string() } else { mount_prefix.clone() };
    let files = list_pcloud_files(&client, &token, &start).await?;

    println!("{:<80} {:<12}", "Path", "Size");
    println!("{:-<80} {:-<12}", "", "");

    for (path, size) in &files {
        println!("  {:<78} {}", path, fmt_bytes(*size));
    }

    println!();
    println!("Total files: {}", files.len());
    println!("Total size: {}", fmt_bytes(files.iter().map(|(_, s)| s).sum()));

    Ok(())
}

/// Reconcile one account, returning (files_on_pcloud, managed_matched, orphans, orphan_bytes).
async fn reconcile_account(
    account: &crate::config::AccountConfig,
    meta: &crate::storage::metadata::MetadataDb,
) -> Result<(i64, i64, i64, i64)> {
    let token = account.resolve_token()?;
    let mount_prefix = account.mount_prefix.clone();

    let managed_paths: HashSet<String> = meta
        .list_managed_remote_paths(&account.email)?
        .into_iter()
        .collect();

    let client = reqwest::Client::new();
    let start = if mount_prefix.is_empty() { "/".to_string() } else { mount_prefix.clone() };
    let all_files = list_pcloud_files(&client, &token, &start).await?;

    let mut matched: i64 = 0;
    let mut orphans: i64 = 0;
    let mut orphan_bytes: i64 = 0;
    for (path, size) in &all_files {
        if managed_paths.contains(path) {
            matched += 1;
        } else {
            orphans += 1;
            orphan_bytes += size;
        }
    }
    Ok((all_files.len() as i64, matched, orphans, orphan_bytes))
}

async fn reconcile(account_filter: Option<&str>) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;
    let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)
        .context("Failed to open metadata database")?;

    let accounts: Vec<&crate::config::AccountConfig> = cfg.storage.accounts.iter()
        .filter(|a| is_pcloud(&a.backend_type))
        .filter(|a| account_filter.map(|f| a.email == f).unwrap_or(true))
        .collect();

    if accounts.is_empty() {
        anyhow::bail!("No pCloud accounts found{}.",
            account_filter.map(|f| format!(" matching '{}'", f)).unwrap_or_default());
    }

    println!("🔎 Reconciling {} pCloud account(s) against the metadata DB…\n", accounts.len());
    println!("{:<30} {:<10} {:<10} {:<10} {:<12}", "Account", "Files", "Matched", "Orphans", "Orphan bytes");
    println!("{:-<30} {:-<10} {:-<10} {:-<10} {:-<12}", "", "", "", "", "");

    let mut total_files = 0i64;
    let mut total_matched = 0i64;
    let mut total_orphans = 0i64;
    let mut total_orphan_bytes = 0i64;

    for account in &accounts {
        match reconcile_account(account, &meta).await {
            Ok((files, matched, orphans, orphan_bytes)) => {
                println!("{:<30} {:<10} {:<10} {:<10} {:<12}",
                    account.email, files, matched, orphans, fmt_bytes(orphan_bytes));
                total_files += files;
                total_matched += matched;
                total_orphans += orphans;
                total_orphan_bytes += orphan_bytes;
            }
            Err(e) => {
                println!("{:<30} {:<10} {:<10} {:<10} {:<12}",
                    account.email, "-", "-", "-", format!("ERROR: {}", e));
            }
        }
    }

    println!("{:-<30} {:-<10} {:-<10} {:-<10} {:-<12}", "", "", "", "", "");
    println!("{:<30} {:<10} {:<10} {:<10} {:<12}",
        "TOTAL", total_files, total_matched, total_orphans, fmt_bytes(total_orphan_bytes));
    println!();
    println!("Note: `listfolder` does not include pCloud Trash, so Trash is NOT counted here.");
    println!("      Orphan bytes above are true unreferenced files on pCloud.");

    Ok(())
}

/// If `path` is a multipart part file (`.../__mp__/multipart-<id>/<N>`), return
/// `(upload_dir, upload_id)` where `upload_dir` is the `__mp__/multipart-<id>`
/// directory and `upload_id` is `multipart-<id>`.
fn multipart_upload_dir(path: &str) -> Option<(String, String)> {
    const MARKER: &str = "__mp__/multipart-";
    let pos = path.find(MARKER)?;
    let after = &path[pos + MARKER.len()..];
    let id_end = after.find('/').unwrap_or(after.len());
    let upload_id = &after[..id_end];
    let upload_dir = &path[..pos + MARKER.len() + id_end];
    Some((upload_dir.to_string(), upload_id.to_string()))
}

async fn pcloud_delete_folder(client: &reqwest::Client, token: &str, path: &str) -> Result<u64> {
    let resp = client
        .post("https://eapi.pcloud.com/deletefolderrecursive")
        .form(&[("access_token", token), ("path", path)])
        .send()
        .await?;
    let json: serde_json::Value = resp.json().await?;
    let result = json.get("result").and_then(|v| v.as_i64()).unwrap_or(-1);
    if result != 0 {
        anyhow::bail!("deletefolderrecursive '{}' failed: result={} error={:?}", path, result, json.get("error"));
    }
    Ok(json.get("deletedfiles").and_then(|v| v.as_u64()).unwrap_or(0))
}

async fn pcloud_delete_file(client: &reqwest::Client, token: &str, path: &str) -> Result<()> {
    let resp = client
        .post("https://eapi.pcloud.com/deletefile")
        .form(&[("access_token", token), ("path", path)])
        .send()
        .await?;
    let json: serde_json::Value = resp.json().await?;
    let result = json.get("result").and_then(|v| v.as_i64()).unwrap_or(-1);
    if result != 0 {
        anyhow::bail!("deletefile '{}' failed: result={} error={:?}", path, result, json.get("error"));
    }
    Ok(())
}

/// Delete orphaned files across pCloud accounts.
///
/// Dry-run by default. In `--apply` mode it deletes, but only after re-verifying
/// each multipart upload_id has zero DB references (no committed version, no
/// `multipart_parts` rows) — so a live object can never be corrupted.
async fn cleanup(account_filter: Option<&str>, apply: bool) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;
    let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)
        .context("Failed to open metadata database")?;

    let accounts: Vec<&crate::config::AccountConfig> = cfg.storage.accounts.iter()
        .filter(|a| is_pcloud(&a.backend_type))
        .filter(|a| account_filter.map(|f| a.email == f).unwrap_or(true))
        .collect();

    if accounts.is_empty() {
        anyhow::bail!("No pCloud accounts found{}.",
            account_filter.map(|f| format!(" matching '{}'", f)).unwrap_or_default());
    }

    let client = reqwest::Client::new();
    let mut plan_dirs = 0u64;
    let mut plan_files = 0u64;
    let mut plan_bytes = 0i64;
    let mut deleted_files = 0u64;
    let mut deleted_bytes = 0i64;

    println!("{} orphan cleanup…\n", if apply { "🗑️  APPLYING" } else { "🔍 DRY-RUN (no changes)" });

    for account in &accounts {
        let token = match account.resolve_token() {
            Ok(t) => t,
            Err(e) => { println!("  ⚠️  {}: {}", account.email, e); continue; }
        };
        let mount_prefix = account.mount_prefix.clone();
        let start = if mount_prefix.is_empty() { "/".to_string() } else { mount_prefix.clone() };

        let managed_paths: HashSet<String> = meta
            .list_managed_remote_paths(&account.email)?
            .into_iter()
            .collect();

        let all_files = match list_pcloud_files(&client, &token, &start).await {
            Ok(f) => f,
            Err(e) => { println!("  ⚠️  {}: list error: {}", account.email, e); continue; }
        };

        let orphans: Vec<(String, i64)> = all_files
            .into_iter()
            .filter(|(p, _)| !managed_paths.contains(p))
            .collect();

        if orphans.is_empty() {
            continue;
        }

        // Partition: multipart part files (grouped by upload dir) vs standalone files.
        let mut upload_dirs: HashMap<String, (String, i64)> = HashMap::new();
        let mut single_files: Vec<(String, i64)> = Vec::new();
        for (path, size) in &orphans {
            match multipart_upload_dir(path) {
                Some((dir, uid)) => {
                    let e = upload_dirs.entry(dir).or_insert((uid, 0));
                    e.1 += *size;
                }
                None => single_files.push((path.clone(), *size)),
            }
        }

        // Safety gate: only delete upload dirs whose upload_id has zero DB refs.
        let mut safe_dirs: Vec<(String, i64)> = Vec::new();
        let mut skipped = 0u64;
        for (dir, (uid, bytes)) in upload_dirs {
            match meta.upload_id_referenced(&uid) {
                Ok(true) => { skipped += 1; println!("  ⚠️  {}: SKIP {} (still referenced in DB)", account.email, dir); }
                Ok(false) => safe_dirs.push((dir, bytes)),
                Err(e) => { skipped += 1; println!("  ⚠️  {}: SKIP {} (check error: {})", account.email, dir, e); }
            }
        }

        let acct_dirs = safe_dirs.len() as u64;
        let acct_files = single_files.len() as u64;
        let acct_bytes = safe_dirs.iter().map(|(_, b)| *b).sum::<i64>()
            + single_files.iter().map(|(_, b)| *b).sum::<i64>();

        if acct_dirs + acct_files == 0 {
            continue;
        }

        plan_dirs += acct_dirs;
        plan_files += acct_files;
        plan_bytes += acct_bytes;

        println!("  {} : {} upload dirs + {} files = {} ({} skipped)",
            account.email, acct_dirs, acct_files, fmt_bytes(acct_bytes), skipped);

        if apply {
            for (dir, bytes) in &safe_dirs {
                match pcloud_delete_folder(&client, &token, dir).await {
                    Ok(n) => { deleted_files += n; deleted_bytes += *bytes; println!("    ✅ deleted {} ({} files)", dir, n); }
                    Err(e) => println!("    ❌ {}: {}", dir, e),
                }
            }
            for (path, bytes) in &single_files {
                match pcloud_delete_file(&client, &token, path).await {
                    Ok(()) => { deleted_files += 1; deleted_bytes += *bytes; }
                    Err(e) => println!("    ❌ {}: {}", path, e),
                }
            }
        }
    }

    println!();
    if apply {
        println!("✅ Cleanup complete: deleted {} files, {} reclaimed.", deleted_files, fmt_bytes(deleted_bytes));
    } else {
        println!("Would delete {} multipart dirs + {} standalone files = {}.",
            plan_dirs, plan_files, fmt_bytes(plan_bytes));
        println!("Re-run with `--apply` to actually delete.");
    }

    Ok(())
}
