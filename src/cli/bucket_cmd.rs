use clap::Parser;
use anyhow::Result;

/// Manage buckets
#[derive(Parser)]
pub struct BucketArgs {
    #[command(subcommand)]
    pub command: BucketSubcommand,
}

#[derive(Parser)]
pub enum BucketSubcommand {
    /// List all buckets
    List,
    /// Create a bucket
    Create {
        /// Bucket name (S3 naming rules: lowercase, no underscores)
        name: String,
    },
    /// Delete a bucket (must be empty unless --force)
    Delete {
        /// Bucket name
        name: String,
        /// Force delete even if bucket is non-empty
        #[arg(long)]
        force: bool,
    },
    /// Show bucket info and stats
    Info {
        /// Bucket name
        name: String,
    },
}

pub async fn run(args: BucketArgs) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;
    let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)?;

    match args.command {
        BucketSubcommand::List => {
            let buckets = meta.list_buckets()?;
            if buckets.is_empty() {
                println!("No buckets.");
                println!("Create one: multifs bucket create <name>");
                return Ok(());
            }
            println!("{:<40} {:<15} {:<20}", "Name", "Objects", "Created");
            println!("{:-<40} {:-<15} {:-<20}", "", "", "");
            for b in &buckets {
                let obj_count = meta.count_objects(&b.name)?;
                println!("{:<40} {:<15} {:<20}", b.name, obj_count, b.created_at);
            }
        }
        BucketSubcommand::Create { name } => {
            // Validate S3 bucket naming rules
            if name.is_empty() || name.len() > 63 {
                anyhow::bail!("Bucket name must be 1-63 characters");
            }
            if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                anyhow::bail!("Bucket name must contain only lowercase letters, digits, and hyphens");
            }
            if meta.bucket_exists(&name)? {
                anyhow::bail!("Bucket already exists: {}", name);
            }
            meta.create_bucket(&name)?;
            println!("✅ Created bucket: {}", name);
        }
        BucketSubcommand::Delete { name, force } => {
            if !meta.bucket_exists(&name)? {
                anyhow::bail!("Bucket not found: {}", name);
            }
            let obj_count = meta.count_objects(&name)?;
            if obj_count > 0 && !force {
                anyhow::bail!(
                    "Bucket '{}' has {} object(s). Use --force to delete anyway.",
                    name,
                    obj_count
                );
            }
            if force {
                // TODO: also delete objects from pCloud
                meta.delete_all_objects(&name)?;
            }
            meta.delete_bucket(&name)?;
            println!("✅ Deleted bucket: {}", name);
        }
        BucketSubcommand::Info { name } => {
            let bucket = meta.get_bucket(&name)?
                .ok_or_else(|| anyhow::anyhow!("Bucket not found: {}", name))?;
            let obj_count = meta.count_objects(&name)?;
            let size: i64 = meta.bucket_total_size(&name)?;

            println!("Bucket: {}", bucket.name);
            println!("  Created: {}", bucket.created_at);
            println!("  Objects: {}", obj_count);
            println!("  Size:    {} bytes ({:.2} MiB)", size, size as f64 / 1_048_576.0);
        }
    }

    Ok(())
}
