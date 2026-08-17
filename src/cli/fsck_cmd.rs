use anyhow::Result;
use clap::Parser;
use futures::{stream, StreamExt};
use std::collections::HashSet;

use crate::storage::engine::StorageEngine;
use crate::storage::metadata::MetadataDb;

/// Filesystem-check: verify database integrity, backend presence + size, and
/// (optionally) content checksums for every managed object.
///
/// This is the health-check companion to `checksum verify` — it layers the
/// structural checks (dangling pointers, mirror mismatches, missing/short blobs,
/// orphan multipart state) on top of the byte-level checksum comparison, and
/// reports everything in one pass.
#[derive(Parser)]
pub struct FsckArgs {
    /// Also recompute and compare SHA-256 content checksums for every object.
    /// Slow — reads every blob's bytes from its backend.
    #[arg(long)]
    pub checksums: bool,

    /// Fix safely-repairable problems: delete orphan multipart parts, then run
    /// `vacuum` to reclaim pending/superseded versions and abandoned uploads.
    #[arg(long)]
    pub fix: bool,

    /// Delete DB rows for committed versions whose blob is confirmed missing
    /// from its backend (single-blob objects only). Dry-run by default — pass
    /// `--apply` to actually write.
    #[arg(long)]
    pub prune_missing: bool,

    /// Actually write changes (required for `--prune-missing`).
    #[arg(long)]
    pub apply: bool,
}

enum CheckOutcome {
    Ok,
    Missing,
    Mismatch { stored: String, computed: String },
    Failed(String),
}

pub async fn run(args: FsckArgs) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;
    let meta = MetadataDb::open(&cfg.storage.meta_db_path)?;
    let engine = StorageEngine::new(&cfg, meta.clone())?;

    println!("=== MultiFS fsck ===");
    println!("DB: {}", cfg.storage.meta_db_path);
    let mut problems: usize = 0;
    let mut warnings: usize = 0;

    // ------------------------------------------------------------------
    // [1/5] Database referential integrity
    // ------------------------------------------------------------------
    println!("\n[1/5] Database integrity");

    let dangling = meta.list_dangling_files()?;
    if dangling.is_empty() {
        println!("  ✅ no dangling files (current_version → committed version)");
    } else {
        problems += dangling.len();
        println!(
            "  ❌ {} file(s) with dangling current_version:",
            dangling.len()
        );
        for (b, k, v) in &dangling {
            println!("    - {}/{} → v{}", b, k, v);
        }
    }

    let mismatches = meta.list_mirror_mismatches()?;
    if mismatches.is_empty() {
        println!("  ✅ file/version mirrors consistent (size/etag/checksum)");
    } else {
        problems += mismatches.len();
        println!("  ❌ {} mirror mismatch(es):", mismatches.len());
        for (b, k, field) in &mismatches {
            println!("    - {}/{} ({})", b, k, field);
        }
    }

    let unreferenced = meta.list_unreferenced_versions()?;
    if unreferenced.is_empty() {
        println!("  ✅ no unreferenced committed versions");
    } else {
        warnings += unreferenced.len();
        println!(
            "  ⚠️  {} committed version(s) with no files row:",
            unreferenced.len()
        );
        for (b, k, v) in &unreferenced {
            println!("    - {}/{} v{}", b, k, v);
        }
    }

    let pending_count = meta.count_pending_versions()?;
    if pending_count == 0 {
        println!("  ✅ no pending (abandoned) versions");
    } else {
        warnings += pending_count as usize;
        println!("  ⚠️  {} pending version(s) awaiting vacuum", pending_count);
    }

    // ------------------------------------------------------------------
    // [2/5] Multipart state
    // ------------------------------------------------------------------
    println!("\n[2/5] Multipart state");

    let committed_versions = meta.list_committed_versions()?;
    let referenced: HashSet<String> = committed_versions
        .iter()
        .filter_map(|v| extract_multipart_id(&v.remote_path))
        .collect();

    let parts_without_upload = meta.list_multipart_parts_without_upload()?;
    let orphan_parts: Vec<String> = parts_without_upload
        .into_iter()
        .filter(|id| !referenced.contains(id))
        .collect();
    if orphan_parts.is_empty() {
        println!("  ✅ no orphan multipart parts");
    } else {
        warnings += orphan_parts.len();
        println!(
            "  ⚠️  {} orphan multipart part-set(s) (no upload, unreferenced):",
            orphan_parts.len()
        );
        for id in &orphan_parts {
            println!("    - {}", id);
        }
    }

    let now_secs = chrono::Utc::now().timestamp();
    let abandoned = meta.list_abandoned_multipart_uploads(now_secs - 86_400)?;
    if abandoned.is_empty() {
        println!("  ✅ no abandoned multipart uploads (>24h)");
    } else {
        warnings += abandoned.len();
        println!(
            "  ⚠️  {} abandoned multipart upload(s) (>24h):",
            abandoned.len()
        );
        for id in &abandoned {
            println!("    - {}", id);
        }
    }

    // ------------------------------------------------------------------
    // [3/5] Backend presence + size
    // ------------------------------------------------------------------
    let (multipart_versions, single_versions): (Vec<_>, Vec<_>) = committed_versions
        .iter()
        .partition(|v| extract_multipart_id(&v.remote_path).is_some());

    println!(
        "\n[3/5] Backend presence + size ({} committed blob(s))",
        committed_versions.len()
    );

    // Multipart composites: verify parts exist and sizes sum correctly.
    let mut missing = 0usize;
    let mut size_mismatch = 0usize;
    let mut missing_blobs: Vec<(String, String, i64)> = Vec::new();
    for v in &multipart_versions {
        let upload_id = extract_multipart_id(&v.remote_path).unwrap_or_default();
        let parts = meta.list_multipart_parts(&upload_id)?;
        if parts.is_empty() {
            missing += 1;
            eprintln!("  ❌ {}/{} v{}: multipart {} has no persisted parts", v.bucket_name, v.key, v.version, upload_id);
        } else {
            let total: i64 = parts.iter().map(|(_, size, _, _, _)| *size).sum();
            if total != v.size {
                size_mismatch += 1;
                eprintln!(
                    "  ❌ {}/{} v{}: multipart size {} != {}",
                    v.bucket_name, v.key, v.version, total, v.size
                );
            }
        }
    }

    // Single-blob objects: cheap stat against each backend.
    let jobs = single_versions.into_iter().map(|v| {
        let engine = engine.clone();
        let account = v.account_email.clone();
        let path = v.remote_path.clone();
        let expected = v.size;
        let bucket = v.bucket_name.clone();
        let key = v.key.clone();
        let version = v.version;
        async move {
            let res = engine.stat_blob(&account, &path).await;
            (bucket, key, version, path, expected, res)
        }
    });
    let results = stream::iter(jobs).buffer_unordered(16).collect::<Vec<_>>().await;

    for (bucket, key, version, path, expected, res) in results {
        match res {
            Ok(Some(actual)) if actual == expected => {}
            Ok(Some(actual)) => {
                size_mismatch += 1;
                eprintln!(
                    "  ❌ {}/{} v{}: size mismatch (db={} backend={}) — {}",
                    bucket, key, version, expected, actual, path
                );
            }
            Ok(None) => {
                missing += 1;
                missing_blobs.push((bucket.clone(), key.clone(), version));
                eprintln!(
                    "  ❌ {}/{} v{}: blob missing — {}",
                    bucket, key, version, path
                );
            }
            Err(e) => {
                problems += 1;
                eprintln!("  ❌ {}/{} v{}: stat failed — {}", bucket, key, version, e);
            }
        }
    }

    if missing == 0 && size_mismatch == 0 && multipart_versions.is_empty() {
        println!(
            "  ✅ all blobs present with correct size ({} checked)",
            committed_versions.len()
        );
    }
    problems += missing;
    problems += size_mismatch;

    // ------------------------------------------------------------------
    // [4/5] Content checksum verification (optional, slow)
    // ------------------------------------------------------------------
    if args.checksums {
        let objects = meta.list_all_objects()?;
        println!(
            "\n[4/5] Content checksum verification ({} object(s))",
            objects.len()
        );
        let mut ok = 0usize;
        let mut cs_mismatch = 0usize;
        let mut cs_missing = 0usize;
        let mut cs_failed = 0usize;

        let jobs = objects.into_iter().map(|o| {
            let engine = engine.clone();
            let bucket = o.bucket_name.clone();
            let key = o.key.clone();
            async move {
                let outcome = verify_checksum(&engine, &bucket, &key).await;
                (bucket, key, outcome)
            }
        });
        let results = stream::iter(jobs).buffer_unordered(4).collect::<Vec<_>>().await;

        for (bucket, key, outcome) in results {
            match outcome {
                CheckOutcome::Ok => ok += 1,
                CheckOutcome::Missing => {
                    cs_missing += 1;
                    println!("  ⚠️  {}/{} has no stored checksum", bucket, key);
                }
                CheckOutcome::Mismatch { stored, computed } => {
                    cs_mismatch += 1;
                    eprintln!(
                        "  ❌ {}/{} CHECKSUM MISMATCH: stored={} computed={}",
                        bucket, key, stored, computed
                    );
                }
                CheckOutcome::Failed(e) => {
                    cs_failed += 1;
                    eprintln!("  ❌ {}/{}: {}", bucket, key, e);
                }
            }
        }
        println!(
            "  {} OK, {} mismatch, {} missing, {} failed",
            ok, cs_mismatch, cs_missing, cs_failed
        );
        problems += cs_mismatch;
        problems += cs_failed;
        warnings += cs_missing;
    } else {
        println!("\n[4/5] Content checksum verification: skipped (use --checksums)");
    }

    // ------------------------------------------------------------------
    // [5/5] GC state
    // ------------------------------------------------------------------
    println!("\n[5/5] Garbage-collection state");
    let superseded = meta.count_superseded_versions()?;
    if superseded == 0 {
        println!("  ✅ no superseded versions awaiting vacuum");
    } else {
        println!(
            "  ℹ️  {} superseded version(s) awaiting vacuum (`multifs vacuum`)",
            superseded
        );
    }

    // ------------------------------------------------------------------
    // Summary
    // ------------------------------------------------------------------
    println!("\n--- Summary ---");
    if problems == 0 && warnings == 0 {
        println!("✅ fsck clean: no problems, no warnings");
    } else {
        println!(
            "❌ fsck found {} problem(s) and {} warning(s)",
            problems, warnings
        );
    }

    // ------------------------------------------------------------------
    // --fix
    // ------------------------------------------------------------------
    if args.prune_missing {
        println!("\n--- Prune missing ---");
        if missing_blobs.is_empty() {
            println!("  no missing single-blob versions to prune");
        } else {
            println!(
                "  {} missing version(s) would be removed{}",
                missing_blobs.len(),
                if args.apply { "" } else { " (dry-run — pass --apply to write)" }
            );
            if !args.apply {
                for (b, k, v) in &missing_blobs {
                    println!("    - {}/{} v{}", b, k, v);
                }
            } else {
                let mut removed_files = 0usize;
                for (b, k, v) in &missing_blobs {
                    match meta.purge_missing_version(b, k, *v) {
                        Ok(true) => {
                            removed_files += 1;
                            println!("  🗑️  removed object {}/{} (v{})", b, k, v);
                        }
                        Ok(false) => {
                            println!("  🧹 removed superseded version {}/{} v{}", b, k, v);
                        }
                        Err(e) => eprintln!("  ❌ failed to purge {}/{} v{}: {}", b, k, v, e),
                    }
                }
                println!(
                    "  done: {} versions purged ({} objects removed)",
                    missing_blobs.len(),
                    removed_files
                );
            }
        }
    }

    if args.fix {
        println!("\n--- Fixing ---");
        let synced = meta.sync_checksum_mirrors()?;
        if synced > 0 {
            println!("  🔧 synced {} checksum mirror(s)", synced);
        } else {
            println!("  no checksum mirrors to sync");
        }

        if !orphan_parts.is_empty() {
            for id in &orphan_parts {
                let parts = meta.list_multipart_parts(id)?;
                // All parts of one multipart upload share a single
                // `__mp__/<id>/N` folder. Prefer one recursive folder delete;
                // fall back to per-file deletes when the backend can't do it.
                let folder = parts
                    .first()
                    .and_then(|(_, _, _, _, path)| path.rsplit_once('/').map(|(dir, _)| dir.to_string()));
                let account = parts
                    .first()
                    .map(|(_, _, _, account, _)| account.clone());
                let mut folder_deleted = false;
                if let (Some(account), Some(folder)) = (account.as_deref(), folder.as_deref()) {
                    match engine.delete_folder_recursive(account, folder).await {
                        Ok(Some(n)) => {
                            folder_deleted = true;
                            println!("  🧹 deleted orphan multipart folder {} ({}) — {} file(s)", id, folder, n);
                        }
                        Ok(None) => {}
                        Err(e) => eprintln!(
                            "  ⚠️  recursive delete of {} failed ({}); falling back to per-file delete",
                            folder, e
                        ),
                    }
                }
                if !folder_deleted {
                    for (_pn, _size, _etag, account, path) in &parts {
                        let _ = engine.delete_blob(account, path).await;
                    }
                    println!("  🧹 deleted orphan multipart parts: {}", id);
                }
                meta.delete_multipart(id)?;
            }
        } else {
            println!("  no orphan multipart parts to delete");
        }

        let (pending, orphans, multipart) = engine.vacuum(false).await?;
        println!(
            "  🧹 vacuum: {} pending, {} superseded, {} abandoned multipart reclaimed",
            pending, orphans, multipart
        );
    }

    Ok(())
}

/// Parse a multipart `upload_id` from a remote_path that carries the staged
/// multipart marker `__mp__/multipart-<upload_id>` (or `.../<upload_id>/N`).
/// Mirrors `StorageEngine::multipart_upload_id`.
fn extract_multipart_id(remote_path: &str) -> Option<String> {
    let normalized = remote_path.trim_end_matches('/');
    let last = normalized.rsplit('/').next().unwrap_or("");
    if last.starts_with("multipart-") {
        return Some(last.to_string());
    }
    if let Some(idx) = normalized.rfind("/multipart-") {
        let base = &normalized[idx + 1..];
        let id = base.split('/').next().unwrap_or("");
        if id.starts_with("multipart-") {
            return Some(id.to_string());
        }
    }
    None
}

/// Verify the stored checksum against the live content for one object.
async fn verify_checksum(engine: &StorageEngine, bucket: &str, key: &str) -> CheckOutcome {
    match engine.get_checksum(bucket, key) {
        Ok(None) => CheckOutcome::Missing,
        Err(e) => CheckOutcome::Failed(e.to_string()),
        Ok(Some(stored)) => match engine.compute_checksum(bucket, key).await {
            Ok(computed) if computed == stored => CheckOutcome::Ok,
            Ok(computed) => CheckOutcome::Mismatch { stored, computed },
            Err(e) => CheckOutcome::Failed(e.to_string()),
        },
    }
}
