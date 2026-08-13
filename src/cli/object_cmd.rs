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
            println!("  Shard:        {} ({})", object.account_email, object.remote_path);
        }
    }

    Ok(())
}
