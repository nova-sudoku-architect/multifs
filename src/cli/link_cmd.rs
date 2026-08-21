use anyhow::Result;
use clap::Parser;

use crate::storage::metadata::MetadataDb;

/// Manage symlinks (tag folders) — link a folder prefix to another folder
/// prefix in the same bucket, so the same content appears under multiple paths
/// without duplicating bytes. S3 has no symlink verb, so links are CLI-only.
#[derive(Parser)]
pub struct LinkArgs {
    #[command(subcommand)]
    pub command: LinkSubcommand,
}

#[derive(Parser)]
pub enum LinkSubcommand {
    /// Create (or replace) a symlink: `multifs link <bucket>/<key> <bucket>/<target_key>`
    Link {
        /// Link path, e.g. "tags/kiss/blor-116"
        path: String,
        /// Target folder prefix, e.g. "video-subtitle/blor-116"
        target: String,
    },
    /// Remove a symlink (never touches the target)
    Unlink {
        /// Link path, e.g. "tags/kiss/blor-116"
        path: String,
    },
    /// List symlinks
    ListLinks {
        /// Only list this bucket
        #[arg(long)]
        bucket: Option<String>,
    },
}

/// Split a "bucket/key" path. Requires a non-empty key (a symlink at the bucket
/// root is nonsensical and rejected).
fn split_path(path: &str) -> anyhow::Result<(String, String)> {
    let parts: Vec<&str> = path.splitn(2, '/').collect();
    if parts.len() < 2 || parts[1].trim_end_matches('/').is_empty() {
        anyhow::bail!("Invalid path: {}. Use bucket/key format.", path);
    }
    Ok((parts[0].to_string(), parts[1].trim_end_matches('/').to_string()))
}

pub async fn run(args: LinkArgs) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;
    let meta = MetadataDb::open(&cfg.storage.meta_db_path)?;

    match args.command {
        LinkSubcommand::Link { path, target } => {
            let (bucket, key) = split_path(&path)?;
            let (target_bucket, target_key) = split_path(&target)?;
            if target_bucket != bucket {
                anyhow::bail!(
                    "cross-bucket symlinks not supported yet (link {} in {} -> {}/{})",
                    key,
                    bucket,
                    target_bucket,
                    target_key
                );
            }
            meta.create_symlink(&bucket, &key, &target_bucket, &target_key)?;
            println!("✅ link created: {}/{} -> {}/{}", bucket, key, target_bucket, target_key);
        }
        LinkSubcommand::Unlink { path } => {
            let (bucket, key) = split_path(&path)?;
            meta.delete_symlink(&bucket, &key)?;
            println!("✅ link removed: {}/{}", bucket, key);
        }
        LinkSubcommand::ListLinks { bucket } => {
            let links = if let Some(b) = bucket.as_deref() {
                meta.list_symlinks_for_bucket(b)?
            } else {
                meta.list_all_symlinks()?
            };
            if links.is_empty() {
                println!("No symlinks recorded.");
                return Ok(());
            }
            for l in &links {
                println!("{}/{} -> {}/{}", l.bucket_name, l.key, l.target_bucket, l.target_key);
            }
            println!("{} symlink(s)", links.len());
        }
    }
    Ok(())
}
