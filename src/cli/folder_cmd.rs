use anyhow::Result;
use clap::Parser;
use std::collections::HashMap;

use crate::storage::metadata::{is_cover_image_key, parent_prefix, MetadataDb};

/// Manage per-folder metadata (currently: the folder's preview image).
///
/// A folder is a key prefix (e.g. `video-subtitle/blor-116/`). Its preview
/// image is an object inside that folder (convention: `cover.jpg`, or any
/// `<name>.cover.jpg`). When no preview is recorded — or the recorded object
/// no longer exists — the UI falls back to the folder icon.
#[derive(Parser)]
pub struct FolderArgs {
    #[command(subcommand)]
    pub command: FolderSubcommand,
}

#[derive(Parser)]
pub enum FolderSubcommand {
    /// Set a folder's preview image (an image object inside the folder)
    SetPreview {
        /// Bucket name
        bucket: String,
        /// Folder prefix, e.g. "video-subtitle/blor-116" (trailing slash optional)
        prefix: String,
        /// Object key of the preview image, e.g. "video-subtitle/blor-116/blor-116.cover.jpg"
        key: String,
    },
    /// Clear a folder's preview image (revert to the folder icon)
    ClearPreview {
        /// Bucket name
        bucket: String,
        /// Folder prefix
        prefix: String,
    },
    /// Scan all objects and record cover images as folder previews
    Backfill {
        /// Only backfill this bucket
        #[arg(long)]
        bucket: Option<String>,
        /// Show what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// List recorded folder previews
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

fn exact_cover_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "cover.jpg" | "cover.jpeg" | "cover.png" | "cover.webp" | "cover.gif"
    )
}

pub async fn run(args: FolderArgs) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;
    let meta = MetadataDb::open(&cfg.storage.meta_db_path)?;

    match args.command {
        FolderSubcommand::SetPreview { bucket, prefix, key } => {
            let prefix = normalize_prefix(&prefix);
            if meta.get_object(&bucket, &key)?.is_none() {
                anyhow::bail!("Object not found: {}/{}", bucket, key);
            }
            meta.set_folder_preview(&bucket, &prefix, &key)?;
            println!("✅ Preview set: {}/{} -> {}", bucket, prefix, key);
        }
        FolderSubcommand::ClearPreview { bucket, prefix } => {
            let prefix = normalize_prefix(&prefix);
            meta.clear_folder_preview(&bucket, &prefix)?;
            println!("✅ Preview cleared: {}/{}", bucket, prefix);
        }
        FolderSubcommand::Backfill { bucket, dry_run } => {
            backfill(&meta, bucket.as_deref(), dry_run)?;
        }
        FolderSubcommand::List { bucket } => {
            let rows = meta.list_folder_meta(bucket.as_deref())?;
            if rows.is_empty() {
                println!("No folder previews recorded.");
                return Ok(());
            }
            for (b, p, pk) in &rows {
                println!("{}/{} -> {}", b, p, pk.as_deref().unwrap_or("(none)"));
            }
            println!("{} folder(s)", rows.len());
        }
    }
    Ok(())
}

/// Scan every object, group cover images by folder prefix, and record one
/// preview per folder. Prefers an exact `cover.jpg` over `<slug>.cover.jpg`
/// when both exist. Deterministic (objects are ordered by bucket, key).
fn backfill(meta: &MetadataDb, bucket: Option<&str>, dry_run: bool) -> Result<()> {
    let objects = meta.list_all_objects()?;

    // (bucket, prefix) -> preferred cover key
    let mut best: HashMap<(String, String), String> = HashMap::new();

    for obj in &objects {
        if let Some(b) = bucket {
            if obj.bucket_name != b {
                continue;
            }
        }
        if !is_cover_image_key(&obj.key) {
            continue;
        }
        let Some(prefix) = parent_prefix(&obj.key) else {
            continue;
        };
        let name = obj.key.rsplit('/').next().unwrap_or(&obj.key);
        let key = (obj.bucket_name.clone(), prefix);
        let entry = best
            .entry(key)
            .or_insert_with(|| obj.key.clone());
        // Exact `cover.jpg` outranks any `<slug>.cover.jpg` already recorded.
        if exact_cover_name(name) {
            *entry = obj.key.clone();
        }
    }

    let mut count = 0usize;
    for ((b, p), key) in &best {
        if dry_run {
            println!("[dry-run] {}/{} -> {}", b, p, key);
        } else {
            meta.set_folder_preview(b, p, key)?;
            println!("✅ {}/{} -> {}", b, p, key);
        }
        count += 1;
    }
    println!(
        "{} folder preview(s) {}recorded",
        count,
        if dry_run { "would be " } else { "" }
    );
    Ok(())
}
