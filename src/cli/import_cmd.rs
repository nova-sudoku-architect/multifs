use anyhow::{Context, Result};
use clap::Parser;

/// Import an existing pCloud file into MultiFS (register its metadata).
///
/// The file stays where it is on pCloud — this only creates the DB record so the
/// object becomes visible/served through the S3 API. No data is downloaded or moved.
///
/// Idempotent: running it again on an already-managed file is a no-op.
#[derive(Parser)]
pub struct ImportArgs {
    /// Email of the pCloud account that holds the file
    pub email: String,
    /// Full pCloud path of the file (e.g. /video-subtitle/blor-025/blor-025.mkv)
    pub remote_path: String,
    /// Target bucket to register the object under
    #[arg(long)]
    pub bucket: String,
    /// Target key (defaults to the file's basename)
    #[arg(long)]
    pub key: Option<String>,
}

pub async fn run(args: ImportArgs) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;

    let account = cfg
        .storage
        .accounts
        .iter()
        .find(|a| a.email == args.email)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Account not found: {}. Use `multifs account list`.",
                args.email
            )
        })?;

    let token = account.resolve_token()?;

    let key = args.key.unwrap_or_else(|| {
        std::path::Path::new(&args.remote_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&args.remote_path)
            .to_string()
    });

    let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)
        .context("Failed to open metadata database")?;

    // 1. Already managed under the target bucket/key?
    if meta.get_object(&args.bucket, &key)?.is_some() {
        println!("⏭️  Already managed: {}/{}", args.bucket, key);
        return Ok(());
    }

    // 2. Already managed under a different bucket/key (same remote path)?
    if let Some((b, k)) = meta.find_object_by_remote_path(&args.email, &args.remote_path)? {
        println!("⏭️  Already managed: {} -> {}/{}", args.remote_path, b, k);
        return Ok(());
    }

    // 3. Fetch file metadata from pCloud (stat — metadata only, no download).
    let client = reqwest::Client::new();
    let stat_url = format!(
        "https://eapi.pcloud.com/stat?path={}&access_token={}",
        args.remote_path, token
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
            args.remote_path,
            error,
            result
        );
    }

    let md = json
        .get("metadata")
        .context("Missing metadata in pCloud stat response")?;
    let size = md.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
    // pCloud's `hash` is a content hash used for server-side dedup; use it as a
    // lightweight ETag (no download required).
    let etag = md.get("hash").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let content_type = md
        .get("contenttype")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| mime_guess::from_path(&args.remote_path).first_raw().map(|s| s.to_string()));
    // pCloud returns RFC 2822 ("Thu, 16 Jul 2026 21:24:59 +0000"); normalize to
    // ISO-8601 for consistency with the rest of multifs.
    let last_modified = md
        .get("modified")
        .and_then(|v| v.as_str())
        .and_then(|m| {
            chrono::DateTime::parse_from_rfc2822(m)
                .ok()
                .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        })
        .unwrap_or_else(|| {
            md.get("modified")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        });

    // 4. Create the DB record (ensuring the bucket exists first so the object
    //    shows up in ListBuckets / ListObjectsV2).
    if !meta.bucket_exists(&args.bucket)? {
        meta.create_bucket(&args.bucket)?;
    }
    meta.import_object(
        &args.bucket,
        &key,
        &args.email,
        &args.remote_path,
        size,
        &etag,
        &last_modified,
        content_type.as_deref(),
    )?;

    println!("✅ Imported {} -> {}/{}", args.remote_path, args.bucket, key);
    println!(
        "   size={} etag={} modified={}",
        size, etag, last_modified
    );

    Ok(())
}
