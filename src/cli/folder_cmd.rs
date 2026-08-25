use anyhow::Result;
use clap::Parser;
use std::collections::HashMap;

use crate::storage::metadata::{
    cover_rank, gif_rank, is_cover_image_key, is_preview_gif_key, is_summary_key, parent_prefix,
    summary_rank, FolderMeta, MetadataDb,
};

/// Manage per-folder metadata for the folder preview page (Feature 5): one
/// cover image, one preview GIF, and one summary per folder.
///
/// A folder is a key prefix (e.g. `video-subtitle/blor-116/`). Each metadata
/// field is an object key inside that folder. When a field is not recorded —
/// or the recorded object no longer exists — the UI skips it.
#[derive(Parser)]
pub struct FolderArgs {
    #[command(subcommand)]
    pub command: FolderSubcommand,
}

#[derive(Parser)]
pub enum FolderSubcommand {
    /// Set a folder's cover image (an image object inside the folder)
    SetCover {
        /// Bucket name
        bucket: String,
        /// Folder prefix, e.g. "video-subtitle/blor-116" (trailing slash optional)
        prefix: String,
        /// Object key of the cover image
        key: String,
    },
    /// Set a folder's summary document (markdown/json/text inside the folder)
    SetSummary {
        bucket: String,
        prefix: String,
        key: String,
    },
    /// Set a folder's preview animation GIF (inside the folder)
    SetGif {
        bucket: String,
        prefix: String,
        key: String,
    },
    /// Clear all folder metadata (revert to the folder icon / no summary)
    Clear {
        /// Bucket name
        bucket: String,
        /// Folder prefix
        prefix: String,
    },
    /// Scan all objects and record cover + summary + GIF per folder
    Backfill {
        /// Only backfill this bucket
        #[arg(long)]
        bucket: Option<String>,
        /// Show what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// List recorded folder metadata
    List {
        /// Only list this bucket
        #[arg(long)]
        bucket: Option<String>,
    },
}

/// Normalize a folder prefix so it ends with a single trailing slash.
fn normalize_prefix(p: &str) -> String {
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        format!("{}/", trimmed)
    }
}

pub async fn run(args: FolderArgs) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;
    let meta = MetadataDb::open(&cfg.storage.meta_db_path)?;

    match args.command {
        FolderSubcommand::SetCover { bucket, prefix, key } => {
            let prefix = normalize_prefix(&prefix);
            if meta.get_object(&bucket, &key)?.is_none() {
                anyhow::bail!("Object not found: {}/{}", bucket, key);
            }
            meta.set_folder_cover(&bucket, &prefix, &key)?;
            println!("✅ cover set: {}/{} -> {}", bucket, prefix, key);
        }
        FolderSubcommand::SetSummary { bucket, prefix, key } => {
            let prefix = normalize_prefix(&prefix);
            if meta.get_object(&bucket, &key)?.is_none() {
                anyhow::bail!("Object not found: {}/{}", bucket, key);
            }
            meta.set_folder_summary(&bucket, &prefix, &key)?;
            println!("✅ summary set: {}/{} -> {}", bucket, prefix, key);
        }
        FolderSubcommand::SetGif { bucket, prefix, key } => {
            let prefix = normalize_prefix(&prefix);
            if meta.get_object(&bucket, &key)?.is_none() {
                anyhow::bail!("Object not found: {}/{}", bucket, key);
            }
            meta.set_folder_gif(&bucket, &prefix, &key)?;
            println!("✅ gif set: {}/{} -> {}", bucket, prefix, key);
        }
        FolderSubcommand::Clear { bucket, prefix } => {
            let prefix = normalize_prefix(&prefix);
            meta.clear_folder_meta(&bucket, &prefix)?;
            println!("✅ folder metadata cleared: {}/{}", bucket, prefix);
        }
        FolderSubcommand::Backfill { bucket, dry_run } => {
            backfill(&meta, bucket.as_deref(), dry_run)?;
        }
        FolderSubcommand::List { bucket } => {
            let rows = meta.list_folder_meta(bucket.as_deref())?;
            if rows.is_empty() {
                println!("No folder metadata recorded.");
                return Ok(());
            }
            for (b, p, m) in &rows {
                println!("{}/{}", b, p);
                println!("  cover:   {}", m.cover_key.as_deref().unwrap_or("(none)"));
                println!("  summary: {}", m.summary_key.as_deref().unwrap_or("(none)"));
                println!("  gif:     {}", m.preview_gif_key.as_deref().unwrap_or("(none)"));
            }
            println!("{} folder(s)", rows.len());
        }
    }
    Ok(())
}

/// Scan every object, group cover/summary/gif artifacts by folder prefix, and
/// record the best of each per folder. Deterministic (objects are ordered by
/// bucket, key); ties resolve to the first-encountered key.
fn backfill(meta: &MetadataDb, bucket: Option<&str>, dry_run: bool) -> Result<()> {
    let objects = meta.list_all_objects()?;

    // (bucket, prefix) -> best FolderMeta so far.
    let mut best: HashMap<(String, String), FolderMeta> = HashMap::new();

    for obj in &objects {
        if let Some(b) = bucket {
            if obj.bucket_name != b {
                continue;
            }
        }
        let Some(prefix) = parent_prefix(&obj.key) else {
            continue;
        };
        let name = obj.key.rsplit('/').next().unwrap_or(&obj.key);
        let key = (obj.bucket_name.clone(), prefix);
        let entry = best.entry(key).or_default();

        if is_cover_image_key(&obj.key) {
            if entry.cover_key.is_none()
                || cover_rank(name) > cover_rank(entry.cover_key.as_deref().unwrap_or(""))
            {
                entry.cover_key = Some(obj.key.clone());
            }
        } else if is_summary_key(&obj.key) {
            if entry.summary_key.is_none()
                || summary_rank(name) > summary_rank(entry.summary_key.as_deref().unwrap_or(""))
            {
                entry.summary_key = Some(obj.key.clone());
            }
        } else if is_preview_gif_key(&obj.key) {
            if entry.preview_gif_key.is_none()
                || gif_rank(name) > gif_rank(entry.preview_gif_key.as_deref().unwrap_or(""))
            {
                entry.preview_gif_key = Some(obj.key.clone());
            }
        }
    }

    let mut count = 0usize;
    for ((b, p), m) in &best {
        if dry_run {
            println!("[dry-run] {}/{} cover={} summary={} gif={}",
                b, p,
                m.cover_key.as_deref().unwrap_or("(none)"),
                m.summary_key.as_deref().unwrap_or("(none)"),
                m.preview_gif_key.as_deref().unwrap_or("(none)"));
        } else {
            if let Some(k) = &m.cover_key {
                meta.set_folder_cover(b, p, k)?;
            }
            if let Some(k) = &m.summary_key {
                meta.set_folder_summary(b, p, k)?;
            }
            if let Some(k) = &m.preview_gif_key {
                meta.set_folder_gif(b, p, k)?;
            }
            println!("✅ {}/{} cover={} summary={} gif={}",
                b, p,
                m.cover_key.as_deref().unwrap_or("(none)"),
                m.summary_key.as_deref().unwrap_or("(none)"),
                m.preview_gif_key.as_deref().unwrap_or("(none)"));
        }
        count += 1;
    }
    println!(
        "{} folder(s) {}recorded",
        count,
        if dry_run { "would be " } else { "" }
    );
    Ok(())
}
