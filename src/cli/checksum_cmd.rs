use anyhow::Result;
use clap::Parser;

/// Manage content checksums (SHA-256) for managed blobs.
///
/// Every blob's checksum is stored in the DB and used to detect accidental
/// in-place modification. New single-blob uploads record it automatically;
/// files imported from pCloud (or written before this feature) start empty
/// and need `rebuild`.
#[derive(Parser)]
pub struct ChecksumArgs {
    #[command(subcommand)]
    pub command: ChecksumSubcommand,
}

#[derive(Parser)]
pub enum ChecksumSubcommand {
    /// Recompute and store the SHA-256 checksum for one object, or all objects
    Rebuild {
        /// Object path (bucket/key). Omit with --all to rebuild every object.
        path: Option<String>,
        /// Rebuild checksums for every managed object
        #[arg(long)]
        all: bool,
    },
    /// Verify stored checksums against live blob content
    Verify {
        /// Object path (bucket/key). Omit with --all to verify every object.
        path: Option<String>,
        /// Verify every managed object
        #[arg(long)]
        all: bool,
    },
}

pub async fn run(args: ChecksumArgs) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;
    let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)?;
    let engine = crate::storage::engine::StorageEngine::new(&cfg, meta)?;

    match args.command {
        ChecksumSubcommand::Rebuild { path, all } => {
            if let Some(p) = path {
                let (bucket, key) = split_path(&p)?;
                let checksum = engine.rebuild_checksum(&bucket, &key).await?;
                println!("✅ {}/{} checksum={}", bucket, key, checksum);
            } else if all {
                let objects = engine.list_all_objects()?;
                println!("Rebuilding checksums for {} object(s)...", objects.len());
                let mut done = 0usize;
                let mut failed = 0usize;
                for obj in objects {
                    match engine.rebuild_checksum(&obj.bucket_name, &obj.key).await {
                        Ok(cs) => {
                            done += 1;
                            println!("  ✅ {}/{} = {}", obj.bucket_name, obj.key, cs);
                        }
                        Err(e) => {
                            failed += 1;
                            eprintln!("  ❌ {}/{}: {}", obj.bucket_name, obj.key, e);
                        }
                    }
                }
                println!("Done: {} rebuilt, {} failed", done, failed);
            } else {
                anyhow::bail!("Specify a bucket/key path, or --all");
            }
        }
        ChecksumSubcommand::Verify { path, all } => {
            if let Some(p) = path {
                let (bucket, key) = split_path(&p)?;
                match verify_one(&engine, &bucket, &key).await? {
                    Some((stored, computed)) if stored == computed => {
                        println!("✅ {}/{} OK ({})", bucket, key, computed);
                    }
                    Some((stored, computed)) => {
                        println!(
                            "❌ {}/{} MISMATCH: stored={} computed={}",
                            bucket, key, stored, computed
                        );
                    }
                    None => println!("⚠️  {}/{} has no stored checksum", bucket, key),
                }
            } else if all {
                let objects = engine.list_all_objects()?;
                println!("Verifying {} object(s)...", objects.len());
                let mut ok = 0usize;
                let mut mismatch = 0usize;
                let mut missing = 0usize;
                let mut failed = 0usize;
                for obj in objects {
                    match verify_one(&engine, &obj.bucket_name, &obj.key).await {
                        Ok(Some((stored, computed))) if stored == computed => ok += 1,
                        Ok(Some((_stored, _computed))) => {
                            mismatch += 1;
                            eprintln!("  ❌ {}/{} MISMATCH", obj.bucket_name, obj.key);
                        }
                        Ok(None) => {
                            missing += 1;
                            println!("  ⚠️  {}/{} no checksum", obj.bucket_name, obj.key);
                        }
                        Err(e) => {
                            failed += 1;
                            eprintln!("  ❌ {}/{}: {}", obj.bucket_name, obj.key, e);
                        }
                    }
                }
                println!(
                    "Done: {} OK, {} mismatch, {} missing, {} failed",
                    ok, mismatch, missing, failed
                );
            } else {
                anyhow::bail!("Specify a bucket/key path, or --all");
            }
        }
    }

    Ok(())
}

/// Verify one object, returning (stored, computed) when a stored checksum exists.
async fn verify_one(
    engine: &crate::storage::engine::StorageEngine,
    bucket: &str,
    key: &str,
) -> Result<Option<(String, String)>> {
    let stored = match engine.get_checksum(bucket, key)? {
        Some(s) => s,
        None => return Ok(None),
    };
    let computed = engine.compute_checksum(bucket, key).await?;
    Ok(Some((stored, computed)))
}

fn split_path(p: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = p.splitn(2, '/').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid path '{}' — use bucket/key format", p);
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}
