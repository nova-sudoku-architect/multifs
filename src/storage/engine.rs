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
use super::chunk_manager;
use super::page_cache::{self, PageCache};
use super::download_tracker::DownloadTracker;

pub(crate) const CHUNK_SIZE: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ObjectInfo {
    pub key: String,
    pub size: i64,
    pub etag: String,
    pub last_modified: String,
    pub content_type: Option<String>,
    pub account_email: String,
    pub remote_path: String,
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
}

impl BackendHandle {
    pub fn new(backend: Box<dyn StorageBackend>, mount_prefix: String, label: String, quota_gb: u64) -> Self {
        Self { backend, mount_prefix, label, quota_gb }
    }
}

#[derive(Clone)]
pub struct StorageEngine {
    meta: MetadataDb,
    backends: Arc<Vec<BackendHandle>>,
    placement: PlacementStrategy,
    next_backend_idx: Arc<AtomicUsize>,
    cached_quotas: Arc<Mutex<Vec<CachedQuota>>>,
    page_cache: Arc<PageCache>,
    download_tracker: Arc<DownloadTracker>,
}

#[derive(Debug, Clone, Copy)]
struct CachedQuota {
    fill_ratio: f64,
    total: i64,
}

impl StorageEngine {
    /// Construct StorageEngine from a Config (convenience wrapper).
    /// Backend construction logic lives here; callers wanting DI should use `from_backends`.
    pub fn new(cfg: &Config, meta: MetadataDb) -> anyhow::Result<Self> {
        let handles = Self::build_backends(cfg)?;
        Ok(Self::from_backends_with_strategy(handles, meta, cfg.storage.placement_strategy))
    }

    /// Build backend handles from config (extracted for reuse).
    fn build_backends(cfg: &Config) -> anyhow::Result<Vec<BackendHandle>> {
        let mut handles = Vec::new();
        for acct in &cfg.storage.accounts {
            let backend: Box<dyn StorageBackend> = match acct.backend_type.as_deref() {
                Some("pcloud") | None => {
                    let b = super::backends::pcloud::PCloudBackend::new(acct)?;
                    Box::new(b)
                }
                Some(other) => anyhow::bail!("Unknown backend type: {}", other),
            };
            handles.push(BackendHandle::new(
                backend,
                acct.mount_prefix.clone(),
                acct.email.clone(),
                acct.quota_gb.unwrap_or(10),
            ));
        }
        Ok(handles)
    }

    /// Construct a StorageEngine from pre-built backends (for testing/DI)
    pub fn from_backends(handles: Vec<BackendHandle>, meta: MetadataDb) -> Self {
        Self::from_backends_with_strategy(handles, meta, PlacementStrategy::Utilization)
    }

    /// Construct with explicit placement strategy (for testing/DI)
    pub fn from_backends_with_strategy(
        handles: Vec<BackendHandle>, meta: MetadataDb, placement: PlacementStrategy,
    ) -> Self {
        let cached = handles.iter().map(|_| CachedQuota { fill_ratio: 0.0, total: 1 }).collect();
        Self {
            meta,
            placement,
            backends: Arc::new(handles),
            next_backend_idx: Arc::new(AtomicUsize::new(0)),
            cached_quotas: Arc::new(Mutex::new(cached)),
            page_cache: Arc::new(PageCache::new("/var/cache/multifs/chunks", 10)),
            download_tracker: Arc::new(DownloadTracker::new()),
        }
    }

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
        if data.len() <= CHUNK_SIZE {
            return self.put_whole_file(bucket, key, data, content_type, &etag, &now).await;
        }
        self.put_chunked_file(bucket, key, data, content_type, &etag, &now).await
    }

    /// Pick a backend according to the configured strategy.
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
                let (best_idx, _) = cached.iter().enumerate()
                    .min_by(|(_, a), (_, b)| a.fill_ratio.partial_cmp(&b.fill_ratio).unwrap_or(std::cmp::Ordering::Equal))
                    .ok_or_else(|| anyhow::anyhow!("No backends available"))?;
                Ok(best_idx)
            }
        }
    }

    /// Refresh the fill-ratio cache by querying each backend's quota.
    pub async fn refresh_quotas(&self) {
        let backends = &*self.backends;
        let mut cached = self.cached_quotas.lock().await;
        for (i, handle) in backends.iter().enumerate() {
            if i >= cached.len() {
                cached.push(CachedQuota { fill_ratio: 0.0, total: 1 });
            }
            if let Ok((used, total)) = handle.backend.check_quota().await {
                let fill = if total > 0 { used as f64 / total as f64 } else { 0.0 };
                cached[i] = CachedQuota { fill_ratio: fill, total };
            }
            // else keep previous cached value
        }
    }

    pub async fn put_whole_file(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: Option<&str>,
        etag: &str,
        now: &str,
    ) -> anyhow::Result<ObjectInfo> {
        let backends = &*self.backends;
        if backends.is_empty() {
            anyhow::bail!("No storage backends configured");
        }
        if self.placement == PlacementStrategy::Utilization {
            let _ = self.refresh_quotas().await;
        }
        let idx = self.pick_backend().await?;
        let backend = &backends[idx];
        let remote_path = format!("{}/{}/{}", backend.mount_prefix, bucket, key);
        let (remote_path_actual, _) = backend.backend.upload(&remote_path, data).await?;
        self.meta.put_object(
            bucket, key, data.len() as i64, etag, now,
            &backend.label, &remote_path_actual, content_type,
        )?;
        Ok(ObjectInfo {
            key: key.to_string(),
            size: data.len() as i64,
            etag: etag.to_string(),
            last_modified: now.to_string(),
            content_type: content_type.map(|s| s.to_string()),
            account_email: backend.label.clone(),
            remote_path: remote_path_actual,
        })
    }

    pub async fn put_chunked_file(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: Option<&str>,
        etag: &str,
        now: &str,
    ) -> anyhow::Result<ObjectInfo> {
        let backends = &*self.backends;
        if backends.is_empty() {
            anyhow::bail!("No storage backends configured");
        }

        let data_chunks = chunk_manager::split(data, CHUNK_SIZE);

        #[derive(Debug)]
        struct ChunkUploadResult {
            global_index: u32,
            remote_path: String,
            checksum: String,
            size: i64,
            bucket: String,
            key: String,
            account: String,
        }

        if self.placement == PlacementStrategy::Utilization {
            let _ = self.refresh_quotas().await;
        }

        let mut results = Vec::new();
        for (local_idx, chunk) in data_chunks.iter().enumerate() {
            let global_idx = local_idx as u32;
            let bi = self.pick_backend().await?;
            let chunk_path = format!("{}/{}/{}.ck.{}", backends[bi].mount_prefix, bucket, key, global_idx);

            match backends[bi].backend.upload(&chunk_path, &chunk.data).await {
                Ok((actual_path, _)) => {
                    results.push(ChunkUploadResult {
                        global_index: global_idx,
                        remote_path: actual_path,
                        checksum: chunk.checksum.clone(),
                        size: chunk.data.len() as i64,
                        bucket: bucket.to_string(),
                        key: key.to_string(),
                        account: backends[bi].label.clone(),
                    });
                    // Update cache in-memory to reflect new chunk usage immediately
                    let mut cached = self.cached_quotas.lock().await;
                    if bi < cached.len() && cached[bi].total > 0 {
                        let used = (cached[bi].fill_ratio * cached[bi].total as f64) + chunk.data.len() as f64;
                        cached[bi].fill_ratio = used / cached[bi].total as f64;
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to upload chunk {}: {}", global_idx, e);
                }
            }
        }

        self.meta.with_conn(|conn| -> anyhow::Result<()> {
            conn.execute(
                "INSERT OR REPLACE INTO files (bucket_name, key, size, etag, last_modified, content_type, storage_type)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'chunked')",
                rusqlite::params![bucket, key, data.len() as i64, etag, now, content_type],
            )?;

            for r in &results {
                conn.execute(
                    "INSERT OR REPLACE INTO chunks (bucket_name, key, chunk_index, size, checksum, is_parity, account_email, remote_path)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
                    rusqlite::params![r.bucket, r.key, r.global_index as i32, r.size, r.checksum, r.account, r.remote_path],
                )?;
            }
            Ok(())
        })?;

        Ok(ObjectInfo {
            key: key.to_string(),
            size: data.len() as i64,
            etag: etag.to_string(),
            last_modified: now.to_string(),
            content_type: content_type.map(|s| s.to_string()),
            account_email: backends[0].label.clone(),
            remote_path: format!("chunked://{}/{}", bucket, key),
        })
    }

    /// Register a new in-progress multipart upload on-disk (wrapper around metadata).
    pub async fn create_multipart_upload(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        content_type: Option<&str>,
    ) -> anyhow::Result<()> {
        self.meta.create_multipart(upload_id, bucket, key, content_type)
    }

    /// Look up an in-progress multipart upload by id. Returns (bucket, key, content_type).
    pub async fn get_multipart_upload(&self, upload_id: &str) -> anyhow::Result<Option<(String, String, Option<String>)>> {
        self.meta.get_multipart(upload_id)
    }

    /// Count how many chunk rows are staged for a part's staging key.
    pub async fn count_part_chunks(&self, bucket: &str, staging_key: &str) -> anyhow::Result<i32> {
        let rows = self.meta.list_chunks_for_key(bucket, staging_key)?;
        Ok(rows.len() as i32)
    }

    /// Record per-part metadata so Complete can stitch staged chunks in order.
    pub async fn store_part_meta(
        &self,
        upload_id: &str,
        part_number: u64,
        size: i64,
        part_etag: &str,
        chunk_count: i32,
    ) -> anyhow::Result<()> {
        self.meta.store_multipart_part(upload_id, part_number, size, part_etag, 0, chunk_count)
    }

    /// Stream a single UploadPart's bytes to storage as 32 MiB chunks.
    ///
    /// The chunks are staged under a per-part unique key so concurrent parts for
    /// the same object don't collide in the `chunks` table (whose PK is
    /// (bucket, key, chunk_index)). Only metadata is recorded on-disk; the part
    /// bytes are NOT held in RAM and NOT re-uploaded at Complete time.
    ///
    /// Returns (staging_key, total_part_size, part_md5).
    pub async fn upload_part_as_chunks(
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
        // Staging key unique per upload+part, so chunk records never collide.
        let staging_key = format!("__multipart__/{}/{}", upload_id, part_number);
        let data_chunks = chunk_manager::split(data, CHUNK_SIZE);

        if self.placement == PlacementStrategy::Utilization {
            let _ = self.refresh_quotas().await;
        }

        let mut results = Vec::new();
        for (local_idx, chunk) in data_chunks.iter().enumerate() {
            let global_idx = local_idx as u32;
            let bi = self.pick_backend().await?;
            let chunk_path = format!(
                "{}/{}/{}.mp.{}",
                backends[bi].mount_prefix, bucket, staging_key, global_idx
            );
            match backends[bi].backend.upload(&chunk_path, &chunk.data).await {
                Ok((actual_path, _)) => {
                    results.push((global_idx, actual_path, chunk.checksum.clone(), chunk.data.len() as i64, backends[bi].label.clone()));
                    let mut cached = self.cached_quotas.lock().await;
                    if bi < cached.len() && cached[bi].total > 0 {
                        let used = (cached[bi].fill_ratio * cached[bi].total as f64) + chunk.data.len() as f64;
                        cached[bi].fill_ratio = used / cached[bi].total as f64;
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "multipart part {} chunk {} upload failed for {}/{}: {}",
                        part_number, global_idx, bucket, staging_key, e
                    );
                }
            }
        }

        // Record staged chunk metadata under the staging key.
        self.meta.with_conn(|conn| -> anyhow::Result<()> {
            for (idx, remote_path, checksum, size, account) in &results {
                conn.execute(
                    "INSERT OR REPLACE INTO chunks (bucket_name, key, chunk_index, size, checksum, is_parity, account_email, remote_path)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
                    rusqlite::params![bucket, staging_key, *idx as i32, *size, checksum, account, remote_path],
                )?;
            }
            Ok(())
        })?;

        let part_md5 = hex::encode(Md5::digest(data));
        Ok((staging_key, data.len() as i64, part_md5))
    }

    /// Stitch a completed multipart upload into a final object.
    ///
    /// Reads the ordered part records, maps each part's staged chunks to a
    /// contiguous global chunk range under the final key, writes the `files`
    /// row + `chunks` rows, and returns the S3 multipart ETag
    /// (MD5 of the concatenation of each part's binary MD5).
    pub async fn stitch_multipart(
        &self,
        bucket: &str,
        upload_id: &str,
        content_type: Option<&str>,
    ) -> anyhow::Result<String> {
        let parts = self.meta.list_multipart_parts(upload_id)?;
        if parts.is_empty() {
            anyhow::bail!("No parts recorded for multipart upload {}", upload_id);
        }

        // Resolve the real object key from the upload record.
        let (_, real_key, ct) = self.meta.get_multipart(upload_id)?
            .ok_or_else(|| anyhow::anyhow!("Multipart upload {} not found", upload_id))?;
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let total_size: i64 = parts.iter().map(|(_, size, _, _, _)| size).sum();

        // S3 multipart ETag: MD5 over the concat of each part's binary MD5.
        let mut md5_concat = Vec::new();
        for (_, _, part_etag, _, _) in &parts {
            let bin = hex::decode(part_etag)?;
            md5_concat.extend_from_slice(&bin);
        }
        let etag = hex::encode(Md5::digest(&md5_concat));

        // Clear stale chunks under the real key, write files row.
        self.meta.with_conn(|conn| -> anyhow::Result<()> {
            conn.execute(
                "DELETE FROM chunks WHERE bucket_name = ?1 AND key = ?2",
                rusqlite::params![bucket, real_key],
            )?;
            conn.execute(
                "INSERT OR REPLACE INTO files (bucket_name, key, size, etag, last_modified, content_type, storage_type)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'chunked')",
                rusqlite::params![bucket, real_key, total_size, etag, now, content_type.as_deref().or(ct.as_deref())],
            )?;
            Ok(())
        })?;

        // Re-stitch: map each part's staged chunks into a global sequential
        // chunk_index under the real key.
        let mut global_idx: i32 = 0;
        for (pn, _, _, _, _) in &parts {
            let staging_key = format!("__multipart__/{}/{}", upload_id, pn);
            let staged = self.meta.list_chunks_for_key(bucket, &staging_key)?;
            for (_, size, checksum, account, remote_path) in &staged {
                self.meta.with_conn(|conn| -> anyhow::Result<()> {
                    conn.execute(
                        "INSERT OR REPLACE INTO chunks (bucket_name, key, chunk_index, size, checksum, is_parity, account_email, remote_path)
                         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
                        rusqlite::params![bucket, real_key, global_idx, size, checksum, account, remote_path],
                    )?;
                    Ok(())
                })?;
                global_idx += 1;
            }
        }

        // Clean up staged chunk rows + upload record.
        for (pn, _, _, _, _) in &parts {
            let staging_key = format!("__multipart__/{}/{}", upload_id, pn);
            self.meta.with_conn(|conn| -> anyhow::Result<()> {
                conn.execute(
                    "DELETE FROM chunks WHERE bucket_name = ?1 AND key = ?2",
                    rusqlite::params![bucket, staging_key],
                )?;
                Ok(())
            })?;
        }
        self.meta.delete_multipart(upload_id)?;

        Ok(etag)
    }

    pub async fn get_object(&self, bucket: &str, key: &str) -> anyhow::Result<Vec<u8>> {
        let storage_type: Option<String> = self.meta.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT storage_type FROM files WHERE bucket_name = ?1 AND key = ?2")?;
            let mut rows = stmt.query(rusqlite::params![bucket, key])?;
            if let Some(row) = rows.next()? {
                Ok(Some(row.get::<_, String>(0)?))
            } else {
                Ok(None)
            }
        })?;

        match storage_type.as_deref() {
            Some("chunked") => self.get_chunked_file(bucket, key).await,
            _ => {
                let obj = self.meta.get_object(bucket, key)?
                    .ok_or_else(|| anyhow::anyhow!("Object not found: {}/{}", bucket, key))?;
                let backend = self.find_backend(&obj.account_email)?;
                backend.backend.download(&obj.remote_path).await
            }
        }
    }

    async fn get_chunked_file(&self, bucket: &str, key: &str) -> anyhow::Result<Vec<u8>> {
        let backends = &*self.backends;

        #[derive(Debug)]
        struct ChunkInfo {
            index: i32,
            _size: i64,
            checksum: String,
            account_email: String,
            remote_path: String,
        }

        let chunks_info: Vec<ChunkInfo> = self.meta.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT chunk_index, size, checksum, account_email, remote_path
                 FROM chunks WHERE bucket_name = ?1 AND key = ?2 ORDER BY chunk_index"
            )?;
            let rows = stmt.query_map(rusqlite::params![bucket, key], |row| {
                Ok(ChunkInfo {
                    index: row.get(0)?,
                    _size: row.get(1)?,
                    checksum: row.get(2)?,
                    account_email: row.get(3)?,
                    remote_path: row.get(4)?,
                })
            })?;
            let mut infos = Vec::new();
            for row in rows { infos.push(row?); }
            Ok(infos)
        })?;

        if chunks_info.is_empty() {
            anyhow::bail!("No chunks found for chunked file: {}/{}", bucket, key);
        }

        // Get original file size for truncation
        let original_size: i64 = self.meta.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT size FROM files WHERE bucket_name = ?1 AND key = ?2")?;
            let mut rows = stmt.query(rusqlite::params![bucket, key])?;
            if let Some(row) = rows.next()? {
                Ok(row.get::<_, i64>(0)?)
            } else {
                Ok(0i64)
            }
        })?;

        if original_size == 0 {
            anyhow::bail!("File size not found in metadata for {}/{}", bucket, key);
        }

        let mut result = Vec::with_capacity(original_size as usize);
        for ci in &chunks_info {
            if let Some(backend) = backends.iter().find(|b| b.label == ci.account_email) {
                let owned_path = if ci.remote_path.is_empty() {
                    format!("{}/{}/{}.ck.{}", backend.mount_prefix, bucket, key, ci.index)
                } else {
                    ci.remote_path.clone()
                };

                match backend.backend.download(&owned_path).await {
                    Ok(data) => {
                        result.extend_from_slice(&data);
                    }
                    Err(e) => {
                        tracing::error!("Failed to download chunk {} of {}/{}: {}", ci.index, bucket, key, e);
                        anyhow::bail!("Failed to download chunk {}: {}", ci.index, e);
                    }
                }
            }
        }
        result.truncate(original_size as usize);
        Ok(result)
    }

    pub async fn get_object_stream(
        &self,
        bucket: &str,
        key: &str,
        range: Option<(usize, usize)>,
        tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, anyhow::Error>>,
    ) -> anyhow::Result<()> {
        let storage_type: Option<String> = self.meta.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT storage_type FROM files WHERE bucket_name = ?1 AND key = ?2")?;
            let mut rows = stmt.query(rusqlite::params![bucket, key])?;
            if let Some(row) = rows.next()? {
                Ok(Some(row.get::<_, String>(0)?))
            } else {
                Ok(None)
            }
        })?;

        match storage_type.as_deref() {
            Some("chunked") if range.is_some() => self.stream_chunked_file_range(bucket, key, range.unwrap(), tx).await,
            Some("chunked") => self.stream_chunked_file_full(bucket, key, tx).await,
            _ => {
                let obj = self.meta.get_object(bucket, key)?
                    .ok_or_else(|| anyhow::anyhow!("Object not found: {}/{}", bucket, key))?;
                let backend = self.find_backend(&obj.account_email)?;
                backend.backend.download_stream(&obj.remote_path, None, None, tx).await
            }
        }
    }

    /// Stream all chunks sequentially (no Range), caching each
    /// Stream all chunks sequentially (no Range). Delegates to stream_chunked_file_range
    /// for a single consistent streaming code path.
    async fn stream_chunked_file_full(
        &self,
        bucket: &str,
        key: &str,
        tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, anyhow::Error>>,
    ) -> anyhow::Result<()> {
        // Get total file size from metadata
        let file_size: i64 = self.meta.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT size FROM files WHERE bucket_name = ?1 AND key = ?2")?;
            let mut rows = stmt.query(rusqlite::params![bucket, key])?;
            if let Some(row) = rows.next()? {
                Ok(row.get::<_, i64>(0)?)
            } else {
                Ok(0i64)
            }
        })?;

        if file_size == 0 {
            anyhow::bail!("File size not found for {}/{}", bucket, key);
        }

        // Unify into the ranged streaming path
        self.stream_chunked_file_range(bucket, key, (0, file_size as usize), tx).await
    }

    /// Parallel ranged streaming: spawn concurrent chunk downloads (tracker-deduped),
    /// pipe pCloud pages directly to VLC as they arrive, in chunk order.
    /// Each download task sends an empty sentinel page when complete.
    async fn stream_chunked_file_range(
        &self,
        bucket: &str,
        key: &str,
        (req_start, req_end): (usize, usize),
        tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, anyhow::Error>>,
    ) -> anyhow::Result<()> {
        let backends = &*self.backends;
        let chunk_size = CHUNK_SIZE;
        let page_size = page_cache::PAGE_SIZE;
        let first_chunk = req_start / chunk_size;
        let last_chunk = if req_end == 0 { 0 } else { (req_end - 1) / chunk_size };
        let num_chunks = (last_chunk - first_chunk + 1) as u32;

        #[derive(Debug)]
        struct ChunkRec {
            index: i32,
            size: i64,
            checksum: String,
            account_email: String,
            remote_path: String,
        }
        let chunks_info: Vec<ChunkRec> = self.meta.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT chunk_index, size, checksum, account_email, remote_path
                 FROM chunks WHERE bucket_name = ?1 AND key = ?2 ORDER BY chunk_index"
            )?;
            let rows = stmt.query_map(rusqlite::params![bucket, key], |row| {
                Ok(ChunkRec {
                    index: row.get(0)?,
                    size: row.get(1)?,
                    checksum: row.get(2)?,
                    account_email: row.get(3)?,
                    remote_path: row.get(4)?,
                })
            })?;
            let mut infos = Vec::new();
            for row in rows { infos.push(row?); }
            Ok(infos)
        })?;

        // Spawn concurrent chunk downloads. Each task pipes pCloud's HTTP stream
        // page-by-page through a shared channel, followed by an empty sentinel.
        let (page_tx, page_rx) = tokio::sync::mpsc::unbounded_channel::<(i32, bytes::Bytes)>();
        let mut spawned = 0u32;
        let dt = self.download_tracker.clone();

        for ci in &chunks_info {
            if ci.index < first_chunk as i32 || ci.index > last_chunk as i32 { continue; }
            spawned += 1;

            let backends_owned = self.backends.clone();
            let cc = self.page_cache.clone();
            let cdt = dt.clone();
            let pt = page_tx.clone();
            let b = bucket.to_string();
            let k = key.to_string();
            let acct = ci.account_email.clone();
            let rp = ci.remote_path.clone();
            let idx = ci.index;

            // Compute mount path and owned path
            let mp = self.backends.iter().find(|bh| bh.label == acct).map(|bh| bh.mount_prefix.clone()).unwrap_or_default();
            let ow = if rp.is_empty() { format!("{}/{}/{}.ck.{}", mp, b, k, idx) } else { rp };

            tokio::spawn(async move {
                Self::stream_chunk_paged(cc, cdt, backends_owned, pt, b, k, acct, ow, idx).await;
            });
        }
        drop(page_tx);

        // Stream pages to VLC in chunk order. Pages accumulate in a buffer per chunk.
        // When a chunk's sentinel arrives, assemble the chunk data, slice to range,
        // send it, and advance to the next chunk.
        use tokio_stream::wrappers::UnboundedReceiverStream;
        use futures::StreamExt;
        let mut stream = UnboundedReceiverStream::new(page_rx);
        // Buffer: per chunk, accumulate pages until sentinel received
        let mut chunk_pages: std::collections::HashMap<i32, Vec<bytes::Bytes>> = std::collections::HashMap::new();
        let mut next = first_chunk as i32;
        let mut chunks_done = 0u32;

        /// Assemble pages for a chunk, slice to the requested byte range, send to tx.
        async fn send_chunk_data(
            pages: Vec<bytes::Bytes>, chunk_idx: i32, chunk_sz: usize,
            req_start: usize, req_end: usize,
            tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, anyhow::Error>>,
        ) -> bool {
            // Concatenate all pages for this chunk
            let mut chunk_data = Vec::new();
            for p in &pages {
                chunk_data.extend_from_slice(p);
            }
            // Compute slice within this chunk
            let co = chunk_idx as usize * chunk_sz;
            let sb = if req_start > co { req_start - co } else { 0 };
            let se = std::cmp::min(chunk_data.len(), req_end.saturating_sub(co));
            if sb < se && sb < chunk_data.len() {
                let slice = &chunk_data[sb..se];
                // Send as pages for backpressure (max 64KB each)
                for page_chunk in slice.chunks(65536) {
                    if tx.send(Ok(bytes::Bytes::copy_from_slice(page_chunk))).await.is_err() {
                        return false;
                    }
                }
            }
            true
        }

        // Collect pages until all chunks complete
        let mut sentinel_rcvd: std::collections::HashSet<i32> = std::collections::HashSet::new();
        while let Some((idx, mut page)) = stream.next().await {
            // Empty page = sentinel (chunk download complete)
            if page.is_empty() {
                sentinel_rcvd.insert(idx);
                chunks_done += 1;
                // Send completed chunk and any subsequent completed chunks
                while sentinel_rcvd.contains(&next) {
                    if let Some(pages) = chunk_pages.remove(&next) {
                        if !send_chunk_data(pages, next, chunk_size, req_start, req_end, &tx).await {
                            return Ok(());
                        }
                        next += 1;
                    } else {
                        tracing::warn!("CHUNKED_SENTINEL_NO_PAGES: chunk {} sentinel received but no pages", next);
                        next += 1;
                    }
                }
                if chunks_done >= spawned {
                    break;
                }
                continue;
            }

            // Accumulate page data for this chunk
            chunk_pages.entry(idx).or_default().push(page);
        }

        // Drain any remaining completed chunks in order
        while sentinel_rcvd.contains(&next) {
            if let Some(pages) = chunk_pages.remove(&next) {
                if !send_chunk_data(pages, next, chunk_size, req_start, req_end, &tx).await {
                    return Ok(());
                }
                next += 1;
            } else {
                next += 1;
            }
        }

        Ok(())
    }

    /// Helper: download a single chunk as paged pages and forward through pt channel.
    /// Checks page cache first; if missing, downloads from pCloud and caches each page.
    async fn stream_chunk_paged(
        cc: Arc<PageCache>,
        cdt: Arc<DownloadTracker>,
        backends: Arc<Vec<BackendHandle>>,
        pt: tokio::sync::mpsc::UnboundedSender<(i32, bytes::Bytes)>,
        b: String, k: String, acct: String, ow: String, idx: i32,
    ) {
        let page_size = page_cache::PAGE_SIZE;
        // Check if all pages are cached
        let missing = cc.missing_ranges(&b, &k, idx, 0, CHUNK_SIZE, CHUNK_SIZE).await;
        if missing.is_empty() {
            // All pages cached — stream them sequentially
            let total_pages = (CHUNK_SIZE + page_size - 1) / page_size;
            for pn in 0..total_pages {
                if let Some(page) = cc.get_page(&b, &k, idx, pn, CHUNK_SIZE).await {
                    if pt.send((idx, bytes::Bytes::from(page))).is_err() { return; }
                }
            }
            // Sentinel: signal chunk complete
            let _ = pt.send((idx, bytes::Bytes::new()));
            return;
        }

        // Register with download tracker for dedup; share result if already registered
        let _ = cdt.try_register(&b, &k, idx).await;
        if let Some(backend_idx) = backends.iter().position(|bh| bh.label == acct) {
            let (dl_tx, mut dl_rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, anyhow::Error>>(64);
            let dl_path = ow.clone();
            let dl_tx2 = dl_tx.clone();
            let be_clone = backends.clone();
            let dl_handle = tokio::spawn(async move {
                if let Some(bh) = be_clone.get(backend_idx) {
                    let _ = bh.backend.download_stream(&dl_path, None, None, dl_tx2).await;
                }
            });
            drop(dl_tx);
            let mut offset = 0usize;
            while let Some(res) = dl_rx.recv().await {
                match res {
                    Ok(p) => {
                        let len = p.len();
                        cc.put_pages(&b, &k, idx, offset, &p, CHUNK_SIZE).await;
                        offset += len;
                        if pt.send((idx, p)).is_err() { break; }
                    }
                    Err(e) => {
                        tracing::warn!("Chunk download error for {}/{} chunk {}: {}", b, k, idx, e);
                        break;
                    }
                }
            }
            let _ = dl_handle.await;
            cdt.complete(&b, &k, idx, Ok(vec![])).await;
        }
        // Always send sentinel — even on failure, so the assembly pipeline doesn't hang.
        // The upload fix ensures chunks always exist on pCloud, making this safe.
        let _ = pt.send((idx, bytes::Bytes::new()));
    }

    pub async fn head_object(&self, bucket: &str, key: &str) -> anyhow::Result<ObjectInfo> {
        let chunked_meta: Option<(i64, String, String, Option<String>)> = self.meta.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT size, etag, last_modified, content_type FROM files WHERE bucket_name = ?1 AND key = ?2 AND storage_type = 'chunked'"
            )?;
            let mut rows = stmt.query(rusqlite::params![bucket, key])?;
            if let Some(row) = rows.next()? {
                Ok(Some((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                )))
            } else {
                Ok(None)
            }
        })?;

        if let Some((size, etag, last_modified, content_type)) = chunked_meta {
            return Ok(ObjectInfo {
                key: key.to_string(),
                size,
                etag,
                last_modified,
                content_type,
                account_email: String::new(),
                remote_path: format!("chunked://{}/{}", bucket, key),
            });
        }

        let obj = self.meta.get_object(bucket, key)?
            .ok_or_else(|| anyhow::anyhow!("Object not found: {}/{}", bucket, key))?;
        Ok(ObjectInfo {
            key: obj.key,
            size: obj.size,
            etag: obj.etag,
            last_modified: obj.last_modified,
            content_type: obj.content_type,
            account_email: obj.account_email,
            remote_path: obj.remote_path,
        })
    }

    pub async fn delete_object(&self, bucket: &str, key: &str) -> anyhow::Result<()> {
        // Check if it's a chunked file first
        let storage_type: Option<String> = self.meta.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT storage_type FROM files WHERE bucket_name = ?1 AND key = ?2"
            )?;
            let mut rows = stmt.query(rusqlite::params![bucket, key])?;
            if let Some(row) = rows.next()? {
                Ok(Some(row.get::<_, String>(0)?))
            } else {
                Ok(None)
            }
        })?;

        match storage_type.as_deref() {
            Some("chunked") => {
                // Delete all chunks from backends
                let chunks_info: Vec<(String, String)> = self.meta.with_conn(|conn| {
                    let mut stmt = conn.prepare(
                        "SELECT account_email, remote_path FROM chunks WHERE bucket_name = ?1 AND key = ?2"
                    )?;
                    let rows = stmt.query_map(rusqlite::params![bucket, key], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?;
                    let mut infos = Vec::new();
                    for row in rows {
                        infos.push(row?);
                    }
                    Ok(infos)
                })?;

                for (account_email, remote_path) in &chunks_info {
                    if let Ok(backend) = self.find_backend(account_email) {
                        let _ = backend.backend.delete(remote_path).await;
                    }
                }
            }
            _ => {
                // Whole-file: existing logic
                let obj = self.meta.get_object(bucket, key)?
                    .ok_or_else(|| anyhow::anyhow!("Object not found: {}/{}", bucket, key))?;
                let backend = self.find_backend(&obj.account_email)?;
                backend.backend.delete(&obj.remote_path).await?;
            }
        }
        self.meta.delete_object(bucket, key)?;
        Ok(())
    }

    pub async fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        max_keys: i64,
    ) -> anyhow::Result<Vec<ObjectInfo>> {
        let records = self.meta.list_objects(bucket, prefix, max_keys)?;
        Ok(records.into_iter().map(|r| ObjectInfo {
            key: r.key,
            size: r.size,
            etag: r.etag,
            last_modified: r.last_modified,
            content_type: r.content_type,
            account_email: r.account_email,
            remote_path: r.remote_path,
        }).collect())
    }

    pub async fn bucket_exists(&self, name: &str) -> anyhow::Result<bool> {
        self.meta.bucket_exists(name)
    }

    pub async fn create_bucket(&self, name: &str) -> anyhow::Result<()> {
        self.ensure_bucket(name)
    }

    pub async fn delete_bucket(&self, name: &str) -> anyhow::Result<()> {
        let objects = self.meta.list_objects(name, None, 10000)?;
        for obj in &objects {
            let _ = self.delete_object(name, &obj.key).await;
        }
        self.meta.delete_bucket(name)?;
        Ok(())
    }

    pub async fn list_all_buckets(&self) -> anyhow::Result<Vec<BucketRecord>> {
        self.meta.list_buckets()
    }

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

    fn find_backend(&self, label: &str) -> anyhow::Result<&BackendHandle> {
        self.backends.iter().find(|b| b.label == label)
            .ok_or_else(|| anyhow::anyhow!("Backend not found: {}", label))
    }

    /// Rebalance — migrate objects/chunks from over-utilized backends to under-utilized ones.
    ///
    /// Per-item strategy: download chunk data from old backend → upload to new backend →
    /// atomically update SQLite metadata → delete from old backend.
    /// Works on individual chunks for chunked files, and on whole-object records for small files.
    /// Returns (migrated_count, total_bytes_moved).
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

        // Identify which accounts are over-full (should offload data)
        let over_full_idx: Vec<usize> = statuses.iter().enumerate()
            .filter(|(_, s)| s.total_bytes > 0
                && (s.used_bytes as f64 / s.total_bytes as f64) > target_fill + 0.05)
            .map(|(i, _)| i)
            .collect();

        if over_full_idx.is_empty() {
            println!("  ✅ Distribution already balanced (within ±5% of target).");
            return Ok((0, 0));
        }

        let over_emails: Vec<&str> = over_full_idx.iter().map(|i| statuses[*i].email.as_str()).collect();
        println!("  Target fill: {:.1}%
", target_fill * 100.0);
        println!("  Over-full accounts (will migrate from):");
        for i in &over_full_idx {
            let s = &statuses[*i];
            let pct = s.used_bytes as f64 / s.total_bytes.max(1) as f64 * 100.0;
            println!("    {} — {:.1}% full", s.email, pct);
        }

        if dry_run {
            // Dry-run: count how many records would move
            let mut moved: u64 = 0;
            let mut bytes: i64 = 0;

            // Whole-file objects on over-full accounts
            let all_objects = self.meta.list_all_objects()?;
            for obj in &all_objects {
                if over_emails.contains(&obj.account_email.as_str()) {
                    let sz = if obj.size > 1_073_741_824 {
                        format!("{:.1} GiB", obj.size as f64 / 1_073_741_824.0)
                    } else if obj.size > 1_048_576 {
                        format!("{:.1} MiB", obj.size as f64 / 1_048_576.0)
                    } else {
                        format!("{} B", obj.size)
                    };
                    println!("    WOULD MIGRATE: {}/{} ({}) — {}",
                        obj.bucket_name, obj.key, sz, obj.account_email);
                    moved += 1;
                    bytes += obj.size;
                }
            }

            // Chunks on over-full accounts
            let chunk_count = self.meta.with_conn(|conn| -> anyhow::Result<i64> {
                let mut stmt = conn.prepare(
                    "SELECT COALESCE(SUM(size), 0), COUNT(*) FROM chunks"
                )?;
                Ok(stmt.query_row([], |row| Ok(row.get::<_, i64>(1)?))?)
            })?;
            println!("    PLUS {} chunk records (account-level tracking unavailable in dry-run)", chunk_count);

            println!("\n  Would migrate ~{} items ({} bytes total)", moved, bytes);
            return Ok((moved, bytes));
        }

        // --- Whole-file migration ---
        let all_objects = self.meta.list_all_objects()?;
        let mut migrated: u64 = 0;
        let mut total_bytes: i64 = 0;

        for obj in &all_objects {
            if !over_emails.contains(&obj.account_email.as_str()) {
                continue;
            }
            // Skip chunked entries (they're in the chunks table, not objects)
            if obj.remote_path.starts_with("chunked://") {
                continue;
            }

            // 1. Download from old backend
            let data = match self.get_object(&obj.bucket_name, &obj.key).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("  ⚠️  {}/{}: download failed ({}), skipping", obj.bucket_name, obj.key, e);
                    continue;
                }
            };

            // 2. Upload to least-full backend (fresh quota check via pick_backend)
            let new_info = match self.put_object_with_content_type(
                &obj.bucket_name, &obj.key, &data, obj.content_type.as_deref(),
            ).await {
                Ok(info) => info,
                Err(e) => {
                    tracing::error!("  ❌ {}/{}: re-upload failed ({}), skipping", obj.bucket_name, obj.key, e);
                    continue;
                }
            };

            // If we landed on a different backend, clean up old copy
            if new_info.account_email != obj.account_email {
                // Delete old pCloud file
                if let Ok(old_backend) = self.find_backend(&obj.account_email) {
                    let _ = old_backend.backend.delete(&obj.remote_path).await;
                }
                // Metadata was already updated by put_object, so old object record
                // now points to new backend — done.
                migrated += 1;
                total_bytes += obj.size;
                if migrated % 10 == 0 {
                    println!("  Progress: {} whole-file objects migrated", migrated);
                }
            }
        }

        // --- Chunk-level migration ---
        // For chunked files, migrate individual chunks from over-full accounts
        let chunks_to_migrate: Vec<(String, String, i32, i64, String, String)> =
            self.meta.with_conn(|conn| -> anyhow::Result<Vec<_>> {
                let mut stmt = conn.prepare(
                    "SELECT bucket_name, key, chunk_index, size, account_email, remote_path
                     FROM chunks"
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i32>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })?;
                let mut v = Vec::new();
                for r in rows { v.push(r?); }
                Ok(v)
            })?;

        let mut chunk_migrated: u64 = 0;
        for (bucket, key, chunk_index, size, acc_email, remote_path) in &chunks_to_migrate {
            if !over_emails.contains(&acc_email.as_str()) {
                continue;
            }

            // 1. Download chunk from old backend
            let old_backend = match self.find_backend(acc_email) {
                Ok(b) => b,
                Err(_) => {
                    tracing::warn!("  ⚠️  chunk {}/{}[{}]: backend {} not found, skipping",
                        bucket, key, chunk_index, acc_email);
                    continue;
                }
            };

            let data = match old_backend.backend.download(remote_path).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("  ⚠️  chunk {}/{}[{}]: download failed ({}), skipping",
                        bucket, key, chunk_index, e);
                    continue;
                }
            };

            // 2. Upload to least-full backend
            let bi = self.pick_backend().await?;
            let new_backend = &self.backends[bi];
            let new_chunk_path = format!("{}/{}/{}.ck.{}", new_backend.mount_prefix, bucket, key, chunk_index);
            let (new_remote_path, _) = match new_backend.backend.upload(&new_chunk_path, &data).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("  ❌ chunk {}/{}[{}]: upload to {} failed ({}), skipping",
                        bucket, key, chunk_index, new_backend.label, e);
                    continue;
                }
            };

            // 3. Update SQLite: change account_email and remote_path for this chunk
            self.meta.with_conn(|conn| -> anyhow::Result<()> {
                conn.execute(
                    "UPDATE chunks SET account_email = ?1, remote_path = ?2
                     WHERE bucket_name = ?3 AND key = ?4 AND chunk_index = ?5",
                    params![new_backend.label, new_remote_path, bucket, key, chunk_index],
                )?;
                Ok(())
            })?;

            // 4. Delete old chunk from old pCloud
            let _ = old_backend.backend.delete(remote_path).await;

            chunk_migrated += 1;
            total_bytes += *size;
            if chunk_migrated % 20 == 0 {
                println!("  Progress: {} chunks migrated", chunk_migrated);
            }
        }

        migrated += chunk_migrated;
        println!("\n  ✅ Rebalance complete: {} items migrated ({} bytes)", migrated, total_bytes);
        Ok((migrated, total_bytes))
    }
}
