use clap::Parser;
use anyhow::Result;

/// Manage objects
#[derive(Parser)]
pub struct ObjectArgs {
    #[command(subcommand)]
    pub command: ObjectSubcommand,
}

#[derive(Parser)]
pub enum ObjectSubcommand {
    /// List objects in a bucket
    List {
        /// Bucket name
        bucket: String,
        /// Filter by prefix
        #[arg(long)]
        prefix: Option<String>,
        /// Limit results
        #[arg(long, default_value = "100")]
        max_keys: i64,
    },
    /// Copy file to/from storage (like s3 cp)
    Cp {
        /// Source path (local or bucket/key)
        source: String,
        /// Destination path (local or bucket/key)
        dest: String,
        /// Recursive copy for directories
        #[arg(short, long)]
        recursive: bool,
    },
    /// Delete an object
    Rm {
        /// Object path (bucket/key)
        path: String,
    },
    /// Show object metadata
    Info {
        /// Object path (bucket/key)
        path: String,
    },
    /// Coalesce completed multipart object(s) into single blobs
    ///
    /// Multipart objects are read-assembled from their parts (N round-trips) and
    /// keep their part rows forever. Coalescing rewrites the assembled bytes as
    /// one blob under a new version; the old parts become reclaimable by vacuum.
    Coalesce {
        /// Object path (bucket/key). Omit when using --all.
        path: Option<String>,
        /// Coalesce every multipart object in the pool
        #[arg(long)]
        all: bool,
        /// Show what would be coalesced without changing anything
        #[arg(long)]
        dry_run: bool,
    },
}

pub async fn run(args: ObjectArgs) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;
    let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)?;
    let engine = crate::storage::engine::StorageEngine::new(&cfg, meta)?;

    match args.command {
        ObjectSubcommand::List { bucket, prefix, max_keys } => {
            let (objects, _) = engine.list_objects(&bucket, prefix.as_deref(), None, max_keys).await?;
            if objects.is_empty() {
                println!("(no objects)");
                return Ok(());
            }
            println!("{:<50} {:<12} {:<20}", "Key", "Size", "Last Modified");
            println!("{:-<50} {:-<12} {:-<20}", "", "", "");
            for obj in &objects {
                let size = if obj.size < 1024 {
                    format!("{} B", obj.size)
                } else if obj.size < 1_048_576 {
                    format!("{:.1} KiB", obj.size as f64 / 1024.0)
                } else {
                    format!("{:.1} MiB", obj.size as f64 / 1_048_576.0)
                };
                println!("{:<50} {:<12} {:<20}", obj.key, size, obj.last_modified);
            }
        }
        ObjectSubcommand::Cp { source, dest, recursive: _ } => {
            // Determine direction: local -> bucket or bucket -> local
            let _to_storage = !dest.contains('/');
            // Actually: pattern is bucket/key for storage paths
            let src_is_storage = source.contains('/') && !source.starts_with('/') && !source.starts_with('.');
            let dst_is_storage = dest.contains('/') && !dest.starts_with('/') && !dest.starts_with('.');

            match (src_is_storage, dst_is_storage) {
                (true, false) => {
                    // Download: bucket/key -> local
                    let parts: Vec<&str> = source.splitn(2, '/').collect();
                    let (bucket, key) = (parts[0], parts.get(1).ok_or_else(|| anyhow::anyhow!("Invalid storage path: {}", source))?);
                    let data = engine.get_object(bucket, key).await?;
                    if let Some(parent) = std::path::Path::new(&dest).parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    tokio::fs::write(&dest, &data).await?;
                    println!("✅ Downloaded {} to {}", source, dest);
                }
                (false, true) => {
                    // Upload: local -> bucket/key
                    let parts: Vec<&str> = dest.splitn(2, '/').collect();
                    let (bucket, key) = (parts[0], parts.get(1).ok_or_else(|| anyhow::anyhow!("Invalid storage path: {}", dest))?);
                    let data = tokio::fs::read(&source).await?;
                    engine.put_object(bucket, key, &data).await?;
                    println!("✅ Uploaded {} to {}", source, dest);
                }
                _ => anyhow::bail!("Usage: multifs object cp <local> <bucket>/<key>   (upload)"),
            }
        }
        ObjectSubcommand::Rm { path } => {
            let parts: Vec<&str> = path.splitn(2, '/').collect();
            if parts.len() < 2 {
                anyhow::bail!("Invalid path: {}. Use bucket/key format.", path);
            }
            engine.delete_object(parts[0], parts[1]).await?;
            println!("✅ Deleted {}", path);
        }
        ObjectSubcommand::Info { path } => {
            let parts: Vec<&str> = path.splitn(2, '/').collect();
            if parts.len() < 2 {
                anyhow::bail!("Invalid path: {}. Use bucket/key format.", path);
            }
            let object = engine.head_object(parts[0], parts[1]).await?;
            println!("Object: {}", path);
            println!("  Size:         {} bytes", object.size);
            println!("  ETag:         {}", object.etag);
            println!("  Last Modified: {}", object.last_modified);
            println!("  Content Type: {}", object.content_type.unwrap_or_default());
            println!("  Account:      {} ({})", object.account_email, object.remote_path);
        }
        ObjectSubcommand::Coalesce { path, all, dry_run } => {
            let fmt = |bytes: i64| -> String {
                if bytes > 1_073_741_824 {
                    format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0)
                } else if bytes > 1_048_576 {
                    format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
                } else {
                    format!("{} bytes", bytes)
                }
            };
            if all {
                let (count, bytes) = engine.coalesce_all(dry_run).await?;
                if count == 0 {
                    println!("No multipart objects to coalesce.");
                } else if dry_run {
                    println!("Would coalesce {} object(s) ({}) into single blobs.", count, fmt(bytes));
                } else {
                    println!("✅ Coalesced {} object(s) ({} reassembled into single blobs).", count, fmt(bytes));
                }
            } else {
                let Some(p) = path else {
                    anyhow::bail!("Provide an object path (bucket/key) or use --all");
                };
                let parts: Vec<&str> = p.splitn(2, '/').collect();
                if parts.len() < 2 {
                    anyhow::bail!("Invalid path: {}. Use bucket/key format.", p);
                }
                match engine.coalesce_multipart(parts[0], parts[1], dry_run).await? {
                    crate::storage::engine::CoalesceOutcome::NotMultipart => {
                        println!("{} is not a multipart object — nothing to do.", p);
                    }
                    crate::storage::engine::CoalesceOutcome::WouldCoalesce { bytes } => {
                        println!("Would coalesce {} ({}) into a single blob.", p, fmt(bytes));
                    }
                    crate::storage::engine::CoalesceOutcome::Coalesced {
                        old_version,
                        new_version,
                        bytes,
                    } => {
                        println!(
                            "✅ Coalesced {} (v{} → v{}, {}); old parts reclaimable by vacuum.",
                            p,
                            old_version,
                            new_version,
                            fmt(bytes)
                        );
                    }
                }
            }
        }
    }

    Ok(())
}
