use anyhow::Result;
use clap::Parser;

/// Garbage-collect superseded and abandoned object versions.
///
/// Reclaims blobs that no live file references: superseded versions past the
/// grace period, and abandoned (pending) uploads past the timeout.
#[derive(Parser)]
pub struct VacuumArgs {
    /// Dry-run: report what would be reclaimed without deleting anything
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: VacuumArgs) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;
    let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)?;
    let engine = crate::storage::engine::StorageEngine::new(&cfg, meta)?;

    let (pending, orphans) = engine.vacuum(args.dry_run).await?;

    if args.dry_run {
        println!(
            "Dry run — would reclaim {} pending upload(s) and {} superseded version(s)",
            pending, orphans
        );
    } else {
        println!(
            "✅ Vacuum complete: {} pending upload(s), {} superseded version(s) reclaimed",
            pending, orphans
        );
    }

    Ok(())
}
