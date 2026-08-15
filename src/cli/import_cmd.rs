use anyhow::{Context, Result};
use clap::Parser;

/// Import existing pCloud files into MultiFS (register metadata only).
///
/// Two modes:
///   1. Single file — `multifs import <email> <remote-path> --bucket <b> [--key <k>]`
///   2. Scan — `multifs import <email> --scan [--bucket <b>] [--prefix <p>] [--dry-run]`
///
/// The file(s) stay where they are on pCloud — this only creates the DB record so the
/// object becomes visible/served through the S3 API. No data is downloaded or moved.
/// Idempotent: files already managed are skipped.
#[derive(Parser)]
pub struct ImportArgs {
    /// Email of the pCloud account that holds the file(s)
    pub email: String,
    /// Full pCloud path of a single file (omit when using --scan)
    pub remote_path: Option<String>,
    /// Target bucket to register the object(s) under (scan defaults to "video")
    #[arg(long)]
    pub bucket: Option<String>,
    /// Target key for a single-file import (defaults to the file's basename)
    #[arg(long)]
    pub key: Option<String>,
    /// Scan the whole account and import every file not yet managed by MultiFS
    #[arg(long)]
    pub scan: bool,
    /// With --scan: only list what would be imported (no writes)
    #[arg(long)]
    pub dry_run: bool,
    /// With --scan: only consider files under this pCloud path (default "/")
    #[arg(long, default_value = "/")]
    pub prefix: String,
}

pub async fn run(args: ImportArgs) -> Result<()> {
    if args.scan {
        return scan_and_import(&args).await;
    }
    import_one_file(&args).await
}

// ---------------------------------------------------------------------------
// Single-file import
// ---------------------------------------------------------------------------

async fn import_one_file(args: &ImportArgs) -> Result<()> {
    let remote_path = args
        .remote_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("REMOTE_PATH is required (or use --scan)"))?;
    let bucket = args
        .bucket
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--bucket is required (or use --scan)"))?;

    let cfg = load_config()?;
    let account = find_account(&cfg, &args.email)?;
    let token = account.resolve_token()?;
    let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)
        .context("Failed to open metadata database")?;

    let key = args
        .key
        .clone()
        .unwrap_or_else(|| basename(remote_path));

    // Already managed under the target bucket/key?
    if meta.get_object(bucket, &key)?.is_some() {
        println!("⏭️  Already managed: {}/{}", bucket, key);
        return Ok(());
    }
    // Already managed under a different bucket/key (same remote path)?
    if let Some((b, k)) = meta.find_object_by_remote_path(&args.email, remote_path)? {
        println!("⏭️  Already managed: {} -> {}/{}", remote_path, b, k);
        return Ok(());
    }

    // Fetch file metadata from pCloud (stat — metadata only, no download).
    let (size, etag, modified, content_type) = pcloud_stat(&token, remote_path).await?;
    let resolved_ct = resolve_content_type(remote_path, content_type.as_deref());

    register(
        &meta,
        &args.email,
        bucket,
        &key,
        remote_path,
        size,
        &etag,
        &modified,
        resolved_ct.as_deref(),
    )?;

    println!("✅ Imported {} -> {}/{}", remote_path, bucket, key);
    println!("   size={} etag={} modified={}", size, etag, modified);

    Ok(())
}

// ---------------------------------------------------------------------------
// Scan + bulk import
// ---------------------------------------------------------------------------

async fn scan_and_import(args: &ImportArgs) -> Result<()> {
    let cfg = load_config()?;
    let account = find_account(&cfg, &args.email)?;
    let token = account.resolve_token()?;
    let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)
        .context("Failed to open metadata database")?;
    let bucket = args.bucket.as_deref().unwrap_or("video");

    // Recursively list every file under the prefix. We use non-recursive
    // listfolder calls + a worklist because pCloud's `recursive=1` mode omits
    // the per-entry `path` field (it only returns `name` + `parentfolderid`).
    let client = reqwest::Client::new();
    let prefix = args.prefix.trim_end_matches('/');
    let prefix = if prefix.is_empty() { "/" } else { prefix };
    let mut candidates: Vec<(String, i64, String, String, Option<String>)> = Vec::new();
    let mut dirs: Vec<String> = vec![prefix.to_string()];
    while let Some(dir) = dirs.pop() {
        let url = format!(
            "https://eapi.pcloud.com/listfolder?path={}&access_token={}",
            dir, token
        );
        let json: serde_json::Value = client
            .get(&url)
            .send()
            .await
            .context("Failed to call pCloud listfolder")?
            .json()
            .await
            .context("Invalid pCloud listfolder response")?;

        let result = json.get("result").and_then(|v| v.as_i64()).unwrap_or(-1);
        if result != 0 {
            let error = json.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
            anyhow::bail!("pCloud API error listing folder {}: {} (result={})", dir, error, result);
        }

        let metadata = json.get("metadata").context("Missing metadata in pCloud response")?;
        let contents = metadata
            .get("contents")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("No contents array in pCloud response"))?;

        for entry in contents {
            // pCloud returns `isfolder` as a JSON boolean (true/false), not an integer.
            let is_folder = entry
                .get("isfolder")
                .map(|v| v.as_bool().unwrap_or_else(|| v.as_i64() == Some(1)))
                .unwrap_or(false);
            let entry_path = entry.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if is_folder {
                if !entry_path.is_empty() {
                    dirs.push(entry_path);
                }
                continue;
            }
            let size = entry.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
            let etag = extract_etag(entry.get("hash"));
            let modified = entry.get("modified").and_then(|v| v.as_str()).unwrap_or("");
            let modified = normalize_modified(modified);
            let ct = entry
                .get("contenttype")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            candidates.push((entry_path, size, etag, modified, ct));
        }
    }

    // Partition into to-import vs skip.
    let mut to_import: Vec<(String, i64, String, String, Option<String>)> = Vec::new();
    let mut skipped_managed = 0usize;
    let mut skipped_staging = 0usize;
    for c in &candidates {
        if is_multipart_staging(&c.0) {
            skipped_staging += 1;
            continue;
        }
        if meta.find_object_by_remote_path(&args.email, &c.0)?.is_some() {
            skipped_managed += 1;
            continue;
        }
        to_import.push(c.clone());
    }

    println!("Scan of {} (prefix {}):", args.email, args.prefix);
    println!("   total files: {}", candidates.len());
    println!("   already managed: {}", skipped_managed);
    println!("   multipart staging (skipped): {}", skipped_staging);
    println!("   to import: {}", to_import.len());

    if args.dry_run {
        for (path, size, ..) in &to_import {
            println!("   [would import] {} ({} bytes)", path, size);
        }
        return Ok(());
    }

    let mut imported = 0usize;
    for (path, size, etag, modified, ct) in &to_import {
        let key = path.trim_start_matches('/');
        let resolved_ct = resolve_content_type(path, ct.as_deref());
        register(
            &meta,
            &args.email,
            bucket,
            key,
            path,
            *size,
            etag,
            modified,
            resolved_ct.as_deref(),
        )?;
        imported += 1;
        println!("✅ Imported {} -> {}/{}", path, bucket, key);
    }

    println!(
        "Done: imported {} file(s); skipped {} managed + {} staging",
        imported, skipped_managed, skipped_staging
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_config() -> Result<crate::config::Config> {
    let cfg_path = crate::config::find_config()?;
    crate::config::load(&cfg_path)
}

fn find_account<'a>(
    cfg: &'a crate::config::Config,
    email: &str,
) -> Result<&'a crate::config::AccountConfig> {
    cfg.storage
        .accounts
        .iter()
        .find(|a| a.email == email)
        .ok_or_else(|| {
            anyhow::anyhow!("Account not found: {}. Use `multifs account list`.", email)
        })
}

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Whether a pCloud path is multipart staging (under an `__mp__/` directory).
fn is_multipart_staging(path: &str) -> bool {
    path.contains("__mp__")
}

/// pCloud returns `hash` as a JSON number (u64 content hash). Convert it to a
/// string for use as a lightweight ETag (handles both string and numeric forms).
fn extract_etag(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Normalize a pCloud RFC 2822 timestamp to ISO-8601 (RFC 3339, ms, UTC).
fn normalize_modified(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc2822(raw)
        .ok()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| raw.to_string())
}

/// Resolve a content type, falling back to mime_guess from the file extension.
fn resolve_content_type(path: &str, provided: Option<&str>) -> Option<String> {
    if let Some(s) = provided {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    mime_guess::from_path(path).first_raw().map(|s| s.to_string())
}

/// Register one remote file into the DB (creating the bucket if needed).
fn register(
    meta: &crate::storage::metadata::MetadataDb,
    email: &str,
    bucket: &str,
    key: &str,
    remote_path: &str,
    size: i64,
    etag: &str,
    modified: &str,
    content_type: Option<&str>,
) -> Result<()> {
    if !meta.bucket_exists(bucket)? {
        meta.create_bucket(bucket)?;
    }
    meta.import_object(
        bucket,
        key,
        email,
        remote_path,
        size,
        etag,
        modified,
        content_type,
    )?;
    Ok(())
}

/// Fetch a single file's metadata from pCloud (stat — metadata only).
async fn pcloud_stat(
    token: &str,
    remote_path: &str,
) -> Result<(i64, String, String, Option<String>)> {
    let client = reqwest::Client::new();
    let stat_url = format!(
        "https://eapi.pcloud.com/stat?path={}&access_token={}",
        remote_path, token
    );
    let json: serde_json::Value = client
        .get(&stat_url)
        .send()
        .await
        .context("Failed to call pCloud stat")?
        .json()
        .await
        .context("Invalid pCloud stat response")?;

    let result = json.get("result").and_then(|v| v.as_i64()).unwrap_or(-1);
    if result != 0 {
        let error = json.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
        anyhow::bail!(
            "pCloud stat error for {}: {} (result={})",
            remote_path,
            error,
            result
        );
    }

    let md = json
        .get("metadata")
        .context("Missing metadata in pCloud stat response")?;
    let size = md.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
    let etag = extract_etag(md.get("hash"));
    let content_type = md
        .get("contenttype")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let modified = md.get("modified").and_then(|v| v.as_str()).unwrap_or("");
    let modified = normalize_modified(modified);
    Ok((size, etag, modified, content_type))
}
