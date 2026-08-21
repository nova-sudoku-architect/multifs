use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use sha2::{Digest as Sha2Digest, Sha256};
use md5::{Digest as Md5Digest, Md5};
use chrono::Utc;

use crate::config::{Config, PlacementStrategy};

use super::backends::StorageBackend;
use super::metadata::{MetadataDb, BucketRecord};
use rusqlite::params;

#[derive(Debug, Clone)]
pub struct ObjectInfo {
    pub key: String,
    pub size: i64,
    pub etag: String,
    pub last_modified: String,
    pub content_type: Option<String>,
    pub charset: Option<String>,
    pub account_email: String,
    pub remote_path: String,
    pub version: i64,
}

#[derive(Debug, Clone)]
pub struct ShardStatus {
    pub email: String,
    pub object_count: i64,
    pub used_bytes: i64,
    pub total_bytes: i64,
}

pub struct BackendHandle {
    pub backend: Box<dyn StorageBackend>,
    pub mount_prefix: String,
    pub label: String,
    pub quota_gb: u64,
    pub priority: u32,
}

impl BackendHandle {
    pub fn new(
        backend: Box<dyn StorageBackend>,
        mount_prefix: String,
        label: String,
        quota_gb: u64,
    ) -> Self {
        Self {
            backend,
            mount_prefix,
            label,
            quota_gb,
            priority: 0,
        }
    }

    /// Set the placement priority (lower = preferred).
    pub fn with_priority(mut self, priority: u32) -> Self {
        self.priority = priority;
        self
    }
}

#[derive(Clone)]
pub struct StorageEngine {
    meta: MetadataDb,
    backends: Arc<Vec<BackendHandle>>,
    placement: PlacementStrategy,
    next_backend_idx: Arc<AtomicUsize>,
    cached_quotas: Arc<Mutex<CachedQuotas>>,
}

#[derive(Debug, Clone)]
struct CachedQuota {
    fill_ratio: f64,
    total: i64,
}

#[derive(Debug, Clone)]
struct CachedQuotas {
    per_backend: Vec<CachedQuota>,
    last_refresh_ms: u128,
}

const QUOTA_REFRESH_MS: u128 = 60_000;

/// How long a superseded version stays reclaimable before vacuum deletes it
/// (protects in-flight readers).
const GRACE_PERIOD_MS: i64 = 10 * 60 * 1000;
/// How long an abandoned (pending) upload is kept before vacuum sweeps it.
const PENDING_TIMEOUT_MS: i64 = 60 * 60 * 1000;
/// How long an in-progress multipart upload is kept before vacuum sweeps it
/// (abandoned — initiated but never completed). `multipart_uploads.created` is
/// epoch seconds, unlike the versions table which uses epoch milliseconds.
const MULTIPART_TIMEOUT_SECS: i64 = 24 * 60 * 60;
/// Fill ratio at or above which a backend is treated as full (no free space)
/// for tiered placement. 1.0 = any free space counts as available.
const FULL_FILL_RATIO: f64 = 1.0;

impl StorageEngine {
    pub fn new(cfg: &Config, meta: MetadataDb) -> anyhow::Result<Self> {
        let handles = Self::build_backends(cfg)?;
        Ok(Self::from_backends_with_strategy(
            handles,
            meta,
            cfg.storage.placement_strategy,
        ))
    }

    fn build_backends(cfg: &Config) -> anyhow::Result<Vec<BackendHandle>> {
        let mut handles = Vec::new();
        for acct in &cfg.storage.accounts {
            let backend: Box<dyn StorageBackend> = match acct.backend_type.as_deref() {
                Some("pcloud") | None => {
                    let b = super::backends::pcloud::PCloudBackend::new(acct)?;
                    Box::new(b)
                }
                Some("local") | Some("disk") | Some("local-disk") => {
                    let b = super::backends::local_disk::LocalDiskBackend::new(acct)?;
                    Box::new(b)
                }
                Some(other) => anyhow::bail!("Unknown backend type: {}", other),
            };
            handles.push(
                BackendHandle::new(
                    backend,
                    acct.mount_prefix.clone(),
                    acct.email.clone(),
                    acct.quota_gb.unwrap_or(10),
                )
                .with_priority(Self::default_priority(acct)),
            );
        }
        Ok(handles)
    }

    /// Default placement priority for an account: 0 for cloud backends,
    /// 1 for local disk (so local disk is the last resort by default).
    fn default_priority(acct: &crate::config::AccountConfig) -> u32 {
        if let Some(p) = acct.priority {
            return p;
        }
        let is_local = matches!(
            acct.backend_type.as_deref(),
            Some("local") | Some("disk") | Some("local-disk")
        );
        if is_local {
            1
        } else {
            0
        }
    }

    pub fn from_backends(handles: Vec<BackendHandle>, meta: MetadataDb) -> Self {
        Self::from_backends_with_strategy(handles, meta, PlacementStrategy::Utilization)
    }

    pub fn from_backends_with_strategy(
        handles: Vec<BackendHandle>,
        meta: MetadataDb,
        placement: PlacementStrategy,
    ) -> Self {
        let per_backend = handles
            .iter()
            .map(|_| CachedQuota {
                fill_ratio: 0.0,
                total: 1,
            })
            .collect();
        Self {
            meta,
            placement,
            backends: Arc::new(handles),
            next_backend_idx: Arc::new(AtomicUsize::new(0)),
            cached_quotas: Arc::new(Mutex::new(CachedQuotas {
                per_backend,
                last_refresh_ms: 0,
            })),
        }
    }

    // -----------------------------------------------------------------
    //  Backend selection
    // -----------------------------------------------------------------

    async fn pick_backend(&self) -> anyhow::Result<usize> {
        let backends = &*self.backends;
        if backends.is_empty() {
            anyhow::bail!("No storage backends configured");
        }
        match self.placement {
            PlacementStrategy::RoundRobin => {
                let idx = self.next_backend_idx.fetch_add(1, Ordering::Relaxed) % backends.len();
                Ok(idx)
            }
            PlacementStrategy::Utilization => {
                let cached = self.cached_quotas.lock().await;
                // Distinct priority levels, ascending: lowest number = most
                // preferred tier. Fill preferred tiers first; spill to a lower
                // priority only when every preferred tier is full.
                let mut levels: Vec<u32> = self.backends.iter().map(|h| h.priority).collect();
                levels.sort_unstable();
                levels.dedup();

                for level in &levels {
                    let mut best: Option<usize> = None;
                    for (i, h) in self.backends.iter().enumerate() {
                        if h.priority != *level {
                            continue;
                        }
                        let fill = cached.per_backend[i].fill_ratio;
                        match best {
                            None => best = Some(i),
                            Some(b) if fill < cached.per_backend[b].fill_ratio => best = Some(i),
                            Some(_) => {}
                        }
                    }
                    if let Some(b) = best {
                        if cached.per_backend[b].fill_ratio < FULL_FILL_RATIO {
                            return Ok(b);
                        }
                        // This tier is full — fall through to the next priority.
                    }
                }

                // All tiers full: least-full overall as a best effort.
                let (best_idx, _) = cached
                    .per_backend
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| {
                        a.fill_ratio
                            .partial_cmp(&b.fill_ratio)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .ok_or_else(|| anyhow::anyhow!("No backends available"))?;
                Ok(best_idx)
            }
        }
    }

    pub async fn refresh_quotas(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        {
            let cached = self.cached_quotas.lock().await;
            if now.saturating_sub(cached.last_refresh_ms) < QUOTA_REFRESH_MS {
                return;
            }
        }

        let backends = &*self.backends;
        let mut fresh: Vec<CachedQuota> = Vec::with_capacity(backends.len());
        for handle in backends.iter() {
            let mut c = CachedQuota {
                fill_ratio: 0.0,
                total: 1,
            };
            if let Ok((used, total)) = handle.backend.check_quota().await {
                c.total = total;
                c.fill_ratio = if total > 0 {
                    used as f64 / total as f64
                } else {
                    0.0
                };
            }
            fresh.push(c);
        }

        let mut cached = self.cached_quotas.lock().await;
        cached.per_backend = fresh;
        cached.last_refresh_ms = now;
    }

    fn find_backend(&self, label: &str) -> anyhow::Result<&BackendHandle> {
        self.backends
            .iter()
            .find(|b| b.label == label)
            .ok_or_else(|| anyhow::anyhow!("Backend not found: {}", label))
    }

    // -----------------------------------------------------------------
    //  Unified put — in-memory buffer (small files / CLI / tests)
    // -----------------------------------------------------------------

    pub async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
    ) -> anyhow::Result<ObjectInfo> {
        self.put_object_with_content_type(bucket, key, data, None).await
    }

    pub async fn put_object_with_content_type(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: Option<&str>,
    ) -> anyhow::Result<ObjectInfo> {
        self.ensure_bucket(bucket)?;
        let etag = hex::encode(Sha256::digest(data));
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let charset = crate::server::resolve_charset(content_type, data);

        let backends = &*self.backends;
        if backends.is_empty() {
            anyhow::bail!("No storage backends configured");
        }
        if self.placement == PlacementStrategy::Utilization {
            let _ = self.refresh_quotas().await;
        }
        let idx = self.pick_backend().await?;
        let backend = &backends[idx];

        // MVCC: reserve a fresh version + blob path; never touch the old blob.
        let (version, remote_path) = self
            .meta
            .reserve_version(bucket, key, &backend.label, &backend.mount_prefix)?;

        let (remote_path_actual, _) = backend.backend.upload(&remote_path, data).await?;

        self.meta.commit_version(
            bucket,
            key,
            version,
            data.len() as i64,
            &etag,
            &now,
            content_type,
            &remote_path_actual,
        )?;
        // Single-blob upload: the ETag IS the SHA-256, so record it as the
        // integrity checksum too.
        self.meta.set_checksum(bucket, key, version, &etag)?;
        self.meta.set_charset(bucket, key, version, charset.as_deref())?;
        // Best-effort: if this key is a folder artifact (cover / summary /
        // preview GIF), record it in the parent folder's metadata. Failures
        // here must not fail the upload.
        let _ = self.record_folder_metadata(bucket, key);

        Ok(ObjectInfo {
            key: key.to_string(),
            size: data.len() as i64,
            etag,
            last_modified: now,
            content_type: content_type.map(|s| s.to_string()),
            charset,
            account_email: backend.label.clone(),
            remote_path: remote_path_actual,
            version,
        })
    }

    // -----------------------------------------------------------------
    //  Unified streaming put — S3 / WebDAV write path
    // -----------------------------------------------------------------

    /// Stream an object write without full-file RAM buffering.
    /// Picks one pCloud account and streams the body directly to it,
    /// computing the SHA-256 ETag on-the-fly via the backend's `upload_stream`.
    pub async fn put_object_stream<S>(
        &self,
        bucket: &str,
        key: &str,
        content_type: Option<&str>,
        charset: Option<&str>,
        stream: S,
    ) -> anyhow::Result<ObjectInfo>
    where
        S: futures::Stream<Item = Result<bytes::Bytes, anyhow::Error>> + Send + Unpin + 'static,
    {
        tracing::info!("engine::put_object_stream bucket={bucket} key={key} content_type={content_type:?}");
        self.ensure_bucket(bucket)?;
        let backends = &*self.backends;
        if backends.is_empty() {
            anyhow::bail!("No storage backends configured");
        }

        if self.placement == PlacementStrategy::Utilization {
            tracing::info!("engine: calling refresh_quotas");
            let _ = self.refresh_quotas().await;
            tracing::info!("engine: refresh_quotas done");
        }
        tracing::info!("engine: picking backend");
        let idx = self.pick_backend().await?;
        let backend = &backends[idx];
        tracing::info!("engine: picked backend={} prefix={}", backend.label, backend.mount_prefix);

        // MVCC: reserve a fresh version + blob path; never touch the old blob.
        let (version, remote_path) = self
            .meta
            .reserve_version(bucket, key, &backend.label, &backend.mount_prefix)?;

        tracing::info!("engine: calling upload_stream path={}", remote_path);
        let (actual_path, _file_id, etag, file_size) = backend
            .backend
            .upload_stream(&remote_path, Box::new(stream))
            .await?;

        let now = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();

        // Use the actual file size reported by the backend (pCloud includes it
        // in the upload response metadata).
        self.meta.commit_version(
            bucket,
            key,
            version,
            file_size,
            &etag,
            &now,
            content_type,
            &actual_path,
        )?;
        // Streaming uploads also use SHA-256 as the ETag; record it as the checksum.
        self.meta.set_checksum(bucket, key, version, &etag)?;
        self.meta.set_charset(bucket, key, version, charset)?;
        // Best-effort: if this key is a folder artifact (cover / summary /
        // preview GIF), record it in the parent folder's metadata. Failures
        // here must not fail the upload.
        let _ = self.record_folder_metadata(bucket, key);

        Ok(ObjectInfo {
            key: key.to_string(),
            size: file_size,
            etag,
            last_modified: now,
            content_type: content_type.map(|s| s.to_string()),
            charset: charset.map(|s| s.to_string()),
            account_email: backend.label.clone(),
            remote_path: actual_path,
            version,
        })
    }

    // -----------------------------------------------------------------
    //  Unified get — non-streaming (collects into Vec)
    // -----------------------------------------------------------------

    pub async fn get_object(&self, bucket: &str, key: &str) -> anyhow::Result<Vec<u8>> {
        let obj = self
            .meta
            .get_object(bucket, key)?
            .ok_or_else(|| anyhow::anyhow!("Object not found: {}/{}", bucket, key))?;

        // Detect multipart composite and assemble from persisted parts.
        if let Some(upload_id) = Self::multipart_upload_id(&obj.remote_path) {
            return self.get_multipart_object(bucket, key, &upload_id).await;
        }

        let backend = self.find_backend(&obj.account_email)?;
        let result = backend.backend.download(&obj.remote_path).await?;

        // Verify integrity if size > 0
        if obj.size > 0 && result.len() as i64 != obj.size {
            tracing::warn!(
                "Size mismatch for {}/{}: expected {}, got {}",
                bucket, key, obj.size, result.len()
            );
        }

        Ok(result)
    }

    /// Assemble a multipart composite object by downloading its persisted parts
    /// in order and concatenating them into a single buffer (non-streaming).
    async fn get_multipart_object(
        &self,
        _bucket: &str,
        _key: &str,
        upload_id: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let parts = self.meta.list_multipart_parts(upload_id)?;
        if parts.is_empty() {
            anyhow::bail!("Multipart object {} has no persisted parts", upload_id);
        }
        let mut out = Vec::new();
        for (_pn, _size, _etag, account, path) in &parts {
            let backend = self.find_backend(account)?;
            let data = backend.backend.download(path).await?;
            out.extend_from_slice(&data);
        }
        Ok(out)
    }

    // -----------------------------------------------------------------
    //  Unified streaming get — Range passthrough to pCloud CDN
    // -----------------------------------------------------------------

    /// Stream an object's bytes, optionally constrained by a byte range.
    /// The range is passed directly to the pCloud CDN (via `getfilelink` +
    /// `Range` header) — no chunk assembly, no page cache coordination.
    /// Parse a multipart upload_id from a remote_path that carries the staged
    /// multipart marker `__mp__/multipart-<upload_id>/` (or `.../<upload_id>`).
    /// Returns Some(upload_id) if the path looks like a multipart composite.
    fn multipart_upload_id(remote_path: &str) -> Option<String> {
        // Staged paths look like: /multifs/<acct>/<bucket>/__mp__/multipart-<id>/N
        // after complete we store the upload dir: /multifs/<acct>/<bucket>/__mp__/multipart-<id>
        let normalized = remote_path.trim_end_matches('/');
        let last = normalized.rsplit('/').next().unwrap_or("");
        if last.starts_with("multipart-") {
            // strip trailing "/N" part number if present
            return Some(last.trim_end().to_string());
        }
        // Also handle the case where remote_path still ends in the part number:
        // /multifs/<acct>/<bucket>/__mp__/multipart-<id>/1
        if let Some(idx) = normalized.rfind("/multipart-") {
            let base = &normalized[idx + 1..]; // "multipart-<id>/1" or "multipart-<id>"
            let id = base.split('/').next().unwrap_or("");
            if id.starts_with("multipart-") {
                return Some(id.to_string());
            }
        }
        None
    }

    /// Stream an object's bytes, optionally constrained by a byte range.
    /// If the object is a multipart composite (parts persisted after Complete),
    /// streams each part in order with Range slicing across part boundaries.
    pub async fn get_object_stream(
        &self,
        bucket: &str,
        key: &str,
        range: Option<(usize, usize)>,
        tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, anyhow::Error>>,
    ) -> anyhow::Result<()> {
        let obj = self
            .meta
            .get_object(bucket, key)?
            .ok_or_else(|| anyhow::anyhow!("Object not found: {}/{}", bucket, key))?;

        // Detect multipart composite and assemble from parts.
        if let Some(upload_id) = Self::multipart_upload_id(&obj.remote_path) {
            return self.get_multipart_object_stream(bucket, key, &upload_id, range, tx).await;
        }

        let backend = self.find_backend(&obj.account_email)?;
        let (range_start, range_end) = match range {
            Some((s, e)) => (Some(s as u64), Some(e as u64)),
            None => (None, None),
        };
        backend
            .backend
            .download_stream(&obj.remote_path, range_start, range_end, tx)
            .await
    }

    /// Assemble a multipart object from its persisted parts, streaming them in
    /// order. Honors an optional byte range by slicing each part's range to the
    /// requested global window (correct across part boundaries).
    async fn get_multipart_object_stream(
        &self,
        bucket: &str,
        _key: &str,
        upload_id: &str,
        range: Option<(usize, usize)>,
        tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, anyhow::Error>>,
    ) -> anyhow::Result<()> {
        let parts = self.meta.list_multipart_parts(upload_id)?;
        if parts.is_empty() {
            anyhow::bail!("Multipart object {} has no persisted parts", upload_id);
        }

        // Global byte offsets.
        let global_start = range.map(|(s, _)| s as u64).unwrap_or(0);
        let global_end = range.map(|(_, e)| e as u64).unwrap_or(u64::MAX);

        // Establish the absolute offset of part 1 within the composite. Usually
        // parts are contiguous from byte 0; we track cumulative offset.
        let mut offset: u64 = 0;
        for (pn, size, _etag, account, path) in &parts {
            let pn = *pn;
            let size = *size as u64;
            let part_abs_start = offset;
            let part_abs_end = offset.saturating_add(size); // exclusive
            offset = part_abs_end;

            // Compute the requested sub-range of THIS part.
            let (lo, hi) = (part_abs_start, part_abs_end);
            let start_in_part = global_start.saturating_sub(lo);
            if global_end <= lo || global_start >= hi {
                continue; // this part is entirely outside the requested range
            }
            let end_in_part = if global_end >= hi {
                hi - lo // full part
            } else {
                (global_end - lo) // exclusive end within part
            };
            if end_in_part <= start_in_part {
                continue;
            }

            let backend = self.find_backend(account)?;
            // Range is inclusive for the reader (download_stream takes start..end
            // as the byte range to fetch). pCloud Range is inclusive of start,
            // exclusive of end in our read path; pass start..end_in_part.
            backend
                .backend
                .download_stream(
                    path,
                    Some(start_in_part),
                    Some(end_in_part),
                    tx.clone(),
                )
                .await?;
            let _ = pn;
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    //  Metadata
    // -----------------------------------------------------------------

    /// Compute the SHA-256 checksum of an object's full content by streaming
    /// it through the read path (handles both single-blob and multipart
    /// composites). Does not write to the DB — the caller decides.
    pub async fn compute_checksum(&self, bucket: &str, key: &str) -> anyhow::Result<String> {
        use sha2::{Digest, Sha256};
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<Result<bytes::Bytes, anyhow::Error>>(16);
        let b = bucket.to_string();
        let k = key.to_string();
        let engine = self.clone();
        tokio::spawn(async move {
            let _ = engine.get_object_stream(&b, &k, None, tx).await;
        });
        let mut hasher = Sha256::new();
        while let Some(chunk) = rx.recv().await {
            let data = chunk?;
            hasher.update(&data);
        }
        Ok(hex::encode(hasher.finalize()))
    }

    /// Recompute and store the checksum for one object. Returns the checksum.
    pub async fn rebuild_checksum(&self, bucket: &str, key: &str) -> anyhow::Result<String> {
        let obj = self
            .meta
            .get_object(bucket, key)?
            .ok_or_else(|| anyhow::anyhow!("Object not found: {}/{}", bucket, key))?;
        let checksum = self.compute_checksum(bucket, key).await?;
        self.meta
            .set_checksum(bucket, key, obj.version, &checksum)?;
        Ok(checksum)
    }

    /// Return the stored checksum for a file's current version, if any.
    pub fn get_checksum(&self, bucket: &str, key: &str) -> anyhow::Result<Option<String>> {
        self.meta.get_checksum(bucket, key)
    }

    /// Check whether a blob exists on its backend and return its size in bytes.
    /// Returns `Ok(Some(size))` if present, `Ok(None)` if not found. Used by
    /// `fsck` for cheap presence + size verification without downloading bytes.
    pub async fn stat_blob(
        &self,
        account_email: &str,
        remote_path: &str,
    ) -> anyhow::Result<Option<i64>> {
        let backend = self.find_backend(account_email)?;
        backend.backend.stat(remote_path).await
    }

    /// Delete a blob from its backend (best-effort). Used by `fsck --fix` to
    /// reclaim orphaned multipart parts. The caller is responsible for deciding
    /// the blob is safe to delete.
    pub async fn delete_blob(&self, account_email: &str, remote_path: &str) -> anyhow::Result<()> {
        let backend = self.find_backend(account_email)?;
        backend.backend.delete(remote_path).await
    }

    /// Delete a folder and all its contents recursively on a backend
    /// (best-effort). Returns `Ok(Some(files_deleted))` if the backend
    /// performed the deletion, `Ok(None)` if the backend does not support it.
    pub async fn delete_folder_recursive(
        &self,
        account_email: &str,
        remote_path: &str,
    ) -> anyhow::Result<Option<u64>> {
        let backend = self.find_backend(account_email)?;
        backend.backend.delete_folder_recursive(remote_path).await
    }

    /// List every managed object (current versions) across all buckets.
    pub fn list_all_objects(&self) -> anyhow::Result<Vec<crate::storage::metadata::ObjectRecord>> {
        self.meta.list_all_objects()
    }

    pub async fn head_object(&self, bucket: &str, key: &str) -> anyhow::Result<ObjectInfo> {
        let obj = self
            .meta
            .get_object(bucket, key)?
            .ok_or_else(|| anyhow::anyhow!("Object not found: {}/{}", bucket, key))?;
        Ok(ObjectInfo {
            key: obj.key,
            size: obj.size,
            etag: obj.etag,
            last_modified: obj.last_modified,
            content_type: obj.content_type,
            charset: obj.charset,
            account_email: obj.account_email,
            remote_path: obj.remote_path,
            version: obj.version,
        })
    }

    // -----------------------------------------------------------------
    //  Delete
    // -----------------------------------------------------------------

    pub async fn delete_object(&self, bucket: &str, key: &str) -> anyhow::Result<()> {
        // MVCC: remove the file pointer and mark the current version superseded.
        // The orphan blob is reclaimed later by `vacuum` (after the grace period).
        self.meta.delete_object(bucket, key)?;
        Ok(())
    }

    // -----------------------------------------------------------------
    //  Vacuum — reclaim superseded / abandoned versions
    // -----------------------------------------------------------------

    /// Reclaim garbage: abandoned `pending` uploads, `committed` versions
    /// that have outlived the grace period, and abandoned multipart uploads.
    /// Returns (pending_removed, orphans_removed, multipart_removed).
    pub async fn vacuum(&self, dry_run: bool) -> anyhow::Result<(u64, u64, u64)> {
        let now = Utc::now().timestamp_millis();
        let pending = self.meta.list_pending_versions(now - PENDING_TIMEOUT_MS)?;
        let orphans = self.meta.list_orphan_versions(now - GRACE_PERIOD_MS)?;
        let abandoned = self
            .meta
            .list_abandoned_multipart_uploads(now / 1000 - MULTIPART_TIMEOUT_SECS)?;

        let mut pending_removed = 0u64;
        for v in &pending {
            if !dry_run {
                if let Ok(backend) = self.find_backend(&v.account_email) {
                    let _ = backend.backend.delete(&v.remote_path).await;
                }
                self.meta.delete_version(&v.bucket_name, &v.key, v.version)?;
            }
            pending_removed += 1;
        }

        let mut orphans_removed = 0u64;
        for v in &orphans {
            if !dry_run {
                // Multipart (chunked) objects store their parts folder as the
                // version's `remote_path` (`.../__mp__/multipart-<id>`). A plain
                // `delete` is a `deletefile` on a folder, which silently no-ops,
                // and the `multipart_parts` rows were never removed — so every
                // chunked object deletion leaked its parts. Delete the folder
                // recursively (falling back to per-part deletes) and drop the
                // parts rows instead.
                if let Some(upload_id) = Self::multipart_upload_id(&v.remote_path) {
                    if let Ok(backend) = self.find_backend(&v.account_email) {
                        let folder_deleted = match backend
                            .backend
                            .delete_folder_recursive(&v.remote_path)
                            .await
                        {
                            Ok(Some(_)) => true,
                            Ok(None) => false,
                            Err(e) => {
                                eprintln!(
                                    "vacuum: recursive delete of {} failed ({}); falling back to per-part delete",
                                    v.remote_path, e
                                );
                                false
                            }
                        };
                        if !folder_deleted {
                            let parts = self.meta.list_multipart_parts(&upload_id)?;
                            for (_pn, _sz, _etag, acct, path) in &parts {
                                if let Ok(b) = self.find_backend(acct) {
                                    let _ = b.backend.delete(path).await;
                                }
                            }
                        }
                    }
                    self.meta.delete_multipart(&upload_id)?;
                } else if let Ok(backend) = self.find_backend(&v.account_email) {
                    let _ = backend.backend.delete(&v.remote_path).await;
                }
                self.meta.delete_version(&v.bucket_name, &v.key, v.version)?;
            }
            orphans_removed += 1;
        }

        let mut multipart_removed = 0u64;
        for upload_id in &abandoned {
            if !dry_run {
                self.abort_multipart_upload(upload_id).await?;
            }
            multipart_removed += 1;
        }

        Ok((pending_removed, orphans_removed, multipart_removed))
    }

    // -----------------------------------------------------------------
    //  List
    // -----------------------------------------------------------------

    pub async fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        start_after: Option<&str>,
        max_keys: i64,
    ) -> anyhow::Result<(Vec<ObjectInfo>, bool)> {
        let records = self
            .meta
            .list_objects(bucket, prefix, start_after, max_keys + 1)?;
        let truncated = records.len() as i64 > max_keys;
        let mut infos: Vec<ObjectInfo> = records
            .into_iter()
            .map(|r| ObjectInfo {
                key: r.key,
                size: r.size,
                etag: r.etag,
                last_modified: r.last_modified,
                content_type: r.content_type,
                charset: r.charset,
                account_email: r.account_email,
                remote_path: r.remote_path,
                version: r.version,
            })
            .collect();
        if truncated {
            infos.truncate(max_keys as usize);
        }
        Ok((infos, truncated))
    }

    // -----------------------------------------------------------------
    //  Bucket CRUD
    // -----------------------------------------------------------------

    pub async fn bucket_exists(&self, name: &str) -> anyhow::Result<bool> {
        self.meta.bucket_exists(name)
    }

    pub async fn create_bucket(&self, name: &str) -> anyhow::Result<()> {
        self.ensure_bucket(name)
    }

    pub async fn delete_bucket(&self, name: &str) -> anyhow::Result<()> {
        let objects = self.meta.list_objects(name, None, None, 10000)?;
        for obj in &objects {
            let _ = self.delete_object(name, &obj.key).await;
        }
        self.meta.delete_bucket(name)?;
        Ok(())
    }

    pub async fn list_all_buckets(&self) -> anyhow::Result<Vec<BucketRecord>> {
        self.meta.list_buckets()
    }

    // -----------------------------------------------------------------
    //  Folder metadata (cover image, summary, preview GIF)
    // -----------------------------------------------------------------

    /// If `key` is a folder artifact (cover image, summary, or preview GIF),
    /// record it in the parent folder prefix's metadata. No-op for other keys
    /// or bucket-root keys. Best-effort: the caller decides whether to surface
    /// errors.
    fn record_folder_metadata(&self, bucket: &str, key: &str) -> anyhow::Result<()> {
        use crate::storage::metadata::{is_cover_image_key, is_preview_gif_key, is_summary_key};
        let Some(prefix) = crate::storage::metadata::parent_prefix(key) else {
            return Ok(());
        };
        if is_cover_image_key(key) {
            self.meta.set_folder_cover(bucket, &prefix, key)?;
        } else if is_summary_key(key) {
            self.meta.set_folder_summary(bucket, &prefix, key)?;
        } else if is_preview_gif_key(key) {
            self.meta.set_folder_gif(bucket, &prefix, key)?;
        }
        Ok(())
    }

    /// Resolve, for each folder prefix, its recorded folder metadata
    /// (cover/summary/gif), keeping only fields whose object still exists.
    /// Returns a map of prefix -> FolderMeta; prefixes with no valid metadata
    /// are omitted, so the UI degrades gracefully.
    pub fn folder_meta_map(
        &self,
        bucket: &str,
        prefixes: &[String],
    ) -> anyhow::Result<std::collections::HashMap<String, crate::storage::metadata::FolderMeta>> {
        let mut out = std::collections::HashMap::new();
        for p in prefixes {
            let Some(mut m) = self.meta.get_folder_meta(bucket, p)? else {
                continue;
            };
            let keep = |k: &Option<String>| -> Option<String> {
                k.as_deref()
                    .filter(|k| self.meta.get_object(bucket, k).map(|o| o.is_some()).unwrap_or(false))
                    .map(|s| s.to_string())
            };
            m.cover_key = keep(&m.cover_key);
            m.summary_key = keep(&m.summary_key);
            m.preview_gif_key = keep(&m.preview_gif_key);
            out.insert(p.clone(), m);
        }
        Ok(out)
    }

    /// Count the direct children (immediate files + subfolders) of a folder
    /// prefix. Delegates to the metadata layer's prefix scan.
    pub fn count_direct_children(&self, bucket: &str, prefix: &str) -> anyhow::Result<i64> {
        self.meta.count_direct_children(bucket, prefix)
    }

    // -----------------------------------------------------------------
    //  Shard status
    // -----------------------------------------------------------------

    pub async fn shard_status(&self) -> anyhow::Result<Vec<ShardStatus>> {
        let mut statuses = Vec::new();
        for handle in self.backends.iter() {
            let obj_count = self.meta.count_objects_for_account(&handle.label)?;
            let total_size = self.meta.account_total_size(&handle.label)?;
            let quota = handle.quota_gb as i64 * 1_073_741_824;
            if let Ok((used, total)) = handle.backend.check_quota().await {
                statuses.push(ShardStatus {
                    email: handle.label.clone(),
                    object_count: obj_count,
                    used_bytes: used,
                    total_bytes: total,
                });
            } else {
                statuses.push(ShardStatus {
                    email: handle.label.clone(),
                    object_count: obj_count,
                    used_bytes: total_size,
                    total_bytes: quota,
                });
            }
        }
        Ok(statuses)
    }

    fn ensure_bucket(&self, name: &str) -> anyhow::Result<()> {
        if !self.meta.bucket_exists(name)? {
            self.meta.create_bucket(name)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    //  Multipart upload (simplified — one blob per part, no chunking)
    // -----------------------------------------------------------------

    pub async fn create_multipart_upload(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        content_type: Option<&str>,
    ) -> anyhow::Result<()> {
        self.meta
            .create_multipart(upload_id, bucket, key, content_type)
    }

    pub async fn get_multipart_upload(
        &self,
        upload_id: &str,
    ) -> anyhow::Result<Option<(String, String, Option<String>)>> {
        self.meta.get_multipart(upload_id)
    }

    /// Upload a single multipart part as one blob to pCloud.
    /// Returns (staging_key, part_size, part_md5).
    pub async fn upload_part(
        &self,
        bucket: &str,
        upload_id: &str,
        part_number: u64,
        data: &[u8],
    ) -> anyhow::Result<(String, i64, String)> {
        let backends = &*self.backends;
        if backends.is_empty() {
            anyhow::bail!("No storage backends configured");
        }

        if self.placement == PlacementStrategy::Utilization {
            let _ = self.refresh_quotas().await;
        }
        let idx = self.pick_backend().await?;

        let staging_key = format!("__mp__/{}/{}", upload_id, part_number);
        let part_path = format!(
            "{}/{}/{}",
            backends[idx].mount_prefix, bucket, staging_key
        );

        let (actual_path, _file_id) = backends[idx].backend.upload(&part_path, data).await?;
        let part_md5 = hex::encode(Md5::digest(data));
        let part_size = data.len() as i64;

        // Record the part so Complete can stitch it.
        self.meta.store_multipart_part(
            upload_id,
            part_number,
            part_size,
            &part_md5,
            &backends[idx].label,
            &actual_path,
        )?;

        Ok((staging_key, part_size, part_md5))
    }

    /// Complete a multipart upload — stitch parts into the final object.
    /// Each part is already a standalone file on pCloud; we record them
    /// all under the real object key and clean up staging.
    pub async fn complete_multipart_upload(
        &self,
        bucket: &str,
        upload_id: &str,
        content_type: Option<&str>,
    ) -> anyhow::Result<String> {
        let (_, real_key, ct) = self
            .meta
            .get_multipart(upload_id)?
            .ok_or_else(|| anyhow::anyhow!("Multipart upload {} not found", upload_id))?;

        let parts = self.meta.list_multipart_parts(upload_id)?;
        if parts.is_empty() {
            anyhow::bail!("No parts recorded for multipart upload {}", upload_id);
        }

        let total_size: i64 = parts.iter().map(|(_, size, _, _, _)| *size).sum();

        // S3 multipart ETag: MD5 over the concat of each part's binary MD5.
        let mut md5_concat = Vec::new();
        let mut all_parts: Vec<(String, String, i64)> = Vec::new(); // (account, path, size)
        for (pn, size, part_etag, account, path) in &parts {
            let bin = hex::decode(part_etag)?;
            md5_concat.extend_from_slice(&bin);
            all_parts.push((account.clone(), path.clone(), *size));
            let _ = pn;
        }
        let etag = format!("{}-{}", hex::encode(Md5::digest(&md5_concat)), parts.len());

        let now = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();

        // For multipart objects we store them as individual parts under the real
        // key with a prefix. On download they are assembled back into the object.
        // The canonical remote_path we record is the upload's staging directory
        // marker (`__mp__/multipart-<upload_id>/`), from which the read path can
        // recover the upload_id and list the persisted parts for assembly.
        let canonical_account = &all_parts[0].0;
        // Store the multipart staging base path (the upload dir), which the read
        // path parses to find the persisted parts. This keeps the object linked to
        // its parts after completion.
        // The canonical remote_path is the upload staging dir
        // (/<mount>/<bucket>/__mp__/multipart-<id>). The read path parses the
        // upload_id from this marker to list and assemble the persisted parts.
        let canonical_path = all_parts[0]
            .1
            .rsplit_once('/')
            .map(|(dir, _)| dir.to_string())
            .unwrap_or_else(|| all_parts[0].1.clone());

        let (version, _reserved_path) = self.meta.reserve_version(
            bucket,
            &real_key,
            canonical_account,
            &self
                .find_backend(canonical_account)
                .map(|b| b.mount_prefix.clone())
                .unwrap_or_else(|_| "/".to_string()),
        )?;
        self.meta.commit_version(
            bucket,
            &real_key,
            version,
            total_size,
            &etag,
            &now,
            content_type.as_deref().or(ct.as_deref()),
            &canonical_path,
        )?;

        // IMPORTANT (S3 round-trip fix): keep the multipart_parts rows so GET can
        // assemble the object from its parts. Previously we called
        // self.meta.delete_multipart(upload_id) here, which wiped the parts list;
        // the object then pointed only at part 1, so downloads truncated at the
        // first part boundary. We now retain the parts (they are keyed by upload_id
        // and pruned on object delete). Only the transient multipart_uploads row is
        // removed so the upload session is considered complete.
        self.meta.delete_multipart_upload(upload_id)?;

        Ok(etag)
    }

    pub async fn list_multipart_parts(
        &self,
        upload_id: &str,
    ) -> anyhow::Result<Vec<(u64, i64, String, String, String)>> {
        self.meta.list_multipart_parts(upload_id)
    }

    /// Abort an in-progress multipart upload — delete its staged part blobs and
    /// metadata rows. No-op if the upload is already gone (e.g. completed — whose
    /// `multipart_parts` rows are retained for read assembly and must NOT be
    /// deleted here).
    pub async fn abort_multipart_upload(&self, upload_id: &str) -> anyhow::Result<()> {
        // Only abort in-progress uploads: a completed object retains its parts
        // (keyed by upload_id) but has no multipart_uploads row.
        if self.meta.get_multipart(upload_id)?.is_none() {
            return Ok(());
        }
        let parts = self.meta.list_multipart_parts(upload_id)?;
        for (_pn, _size, _etag, account, path) in &parts {
            if let Ok(backend) = self.find_backend(account) {
                let _ = backend.backend.delete(path).await;
            }
        }
        self.meta.delete_multipart(upload_id)?;
        Ok(())
    }

    // -----------------------------------------------------------------
    //  Rebalance — move objects from over-full to under-full backends
    // -----------------------------------------------------------------

    pub async fn rebalance(&self, dry_run: bool) -> anyhow::Result<(u64, i64)> {
        let statuses = self.shard_status().await?;
        if statuses.len() < 2 {
            anyhow::bail!("Need at least 2 backends to rebalance");
        }
        let total_capacity: i64 = statuses.iter().map(|s| s.total_bytes).sum();
        let total_used: i64 = statuses.iter().map(|s| s.used_bytes).sum();
        if total_capacity == 0 {
            anyhow::bail!("Cannot rebalance: no backends with capacity");
        }
        let target_fill = total_used as f64 / total_capacity as f64;

        let over_full_idx: Vec<usize> = statuses
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.total_bytes > 0
                    && (s.used_bytes as f64 / s.total_bytes as f64) > target_fill + 0.05
            })
            .map(|(i, _)| i)
            .collect();

        if over_full_idx.is_empty() {
            println!("  ✅ Distribution already balanced (within ±5% of target).");
            return Ok((0, 0));
        }

        let over_emails: Vec<&str> = over_full_idx
            .iter()
            .map(|i| statuses[*i].email.as_str())
            .collect();

        println!("  Target fill: {:.1}%", target_fill * 100.0);
        println!("  Over-full accounts (will migrate from):");
        for i in &over_full_idx {
            let s = &statuses[*i];
            let pct = s.used_bytes as f64 / s.total_bytes.max(1) as f64 * 100.0;
            println!("    {} — {:.1}% full", s.email, pct);
        }

        let all_objects = self.meta.list_all_objects()?;

        if dry_run {
            let mut moved: u64 = 0;
            let mut bytes: i64 = 0;
            for obj in &all_objects {
                if over_emails.contains(&obj.account_email.as_str()) {
                    let sz = if obj.size > 1_073_741_824 {
                        format!("{:.1} GiB", obj.size as f64 / 1_073_741_824.0)
                    } else if obj.size > 1_048_576 {
                        format!("{:.1} MiB", obj.size as f64 / 1_048_576.0)
                    } else {
                        format!("{} B", obj.size)
                    };
                    println!(
                        "    WOULD MIGRATE: {}/{} ({}) — {}",
                        obj.bucket_name, obj.key, sz, obj.account_email
                    );
                    moved += 1;
                    bytes += obj.size;
                }
            }
            println!("\n  Would migrate {} items ({} bytes total)", moved, bytes);
            return Ok((moved, bytes));
        }

        let mut migrated: u64 = 0;
        let mut total_bytes: i64 = 0;

        for obj in &all_objects {
            if !over_emails.contains(&obj.account_email.as_str()) {
                continue;
            }

            let data = match self.get_object(&obj.bucket_name, &obj.key).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(
                        "  ⚠️  {}/{}: download failed ({}), skipping",
                        obj.bucket_name,
                        obj.key,
                        e
                    );
                    continue;
                }
            };

            let new_info = match self
                .put_object_with_content_type(
                    &obj.bucket_name,
                    &obj.key,
                    &data,
                    obj.content_type.as_deref(),
                )
                .await
            {
                Ok(info) => info,
                Err(e) => {
                    tracing::error!(
                        "  ❌ {}/{}: re-upload failed ({}), skipping",
                        obj.bucket_name,
                        obj.key,
                        e
                    );
                    continue;
                }
            };

            if new_info.account_email != obj.account_email {
                if let Ok(old_backend) = self.find_backend(&obj.account_email) {
                    let _ = old_backend.backend.delete(&obj.remote_path).await;
                }
                // Remove the old version row now that its blob is gone.
                let _ = self.meta.delete_version(&obj.bucket_name, &obj.key, obj.version);
                migrated += 1;
                total_bytes += obj.size;
                if migrated % 10 == 0 {
                    println!("  Progress: {} objects migrated", migrated);
                }
            }
        }

        println!(
            "\n  ✅ Rebalance complete: {} items migrated ({} bytes)",
            migrated, total_bytes
        );
        Ok((migrated, total_bytes))
    }
}


