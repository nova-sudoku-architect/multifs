use std::sync::Arc;
use tokio::sync::Mutex;
use sha2::{Digest, Sha256};
use chrono::Utc;

use crate::config::Config;

use super::backends::StorageBackend;
use super::metadata::{MetadataDb, BucketRecord};
use super::chunk_manager;

const CHUNK_SIZE: usize = 32 * 1024 * 1024;

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
    /// Cached fill-level quotas for each backend index, refreshed periodically.
    cached_quotas: Arc<Mutex<Vec<CachedQuota>>>,
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
        Ok(Self::from_backends(handles, meta))
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
        let cached = handles.iter().map(|_| CachedQuota { fill_ratio: 0.0, total: 1 }).collect();
        Self {
            meta,
            backends: Arc::new(handles),
            cached_quotas: Arc::new(Mutex::new(cached)),
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

    /// Pick the backend with the lowest fill ratio (used/quota).
    /// Refreshes the cache at most once per call (the refresh is async, so we rely on
    /// the cached values which are updated lazily).
    async fn pick_least_full_backend(&self) -> anyhow::Result<usize> {
        let backends = &*self.backends;
        if backends.is_empty() {
            anyhow::bail!("No storage backends configured");
        }
        let cached = self.cached_quotas.lock().await;
        let (best_idx, _) = cached.iter().enumerate()
            .min_by(|(_, a), (_, b)| a.fill_ratio.partial_cmp(&b.fill_ratio).unwrap_or(std::cmp::Ordering::Equal))
            .ok_or_else(|| anyhow::anyhow!("No backends available"))?;
        Ok(best_idx)
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
        // Refresh quotas for utilization-based placement
        let _ = self.refresh_quotas().await;
        let idx = self.pick_least_full_backend().await?;
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

        // Refresh quotas for utilization-based placement
        let _ = self.refresh_quotas().await;

        let mut results = Vec::new();
        for (local_idx, chunk) in data_chunks.iter().enumerate() {
            let global_idx = local_idx as u32;
            // Pick least-full backend for each chunk
            let bi = self.pick_least_full_backend().await?;
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

    /// Stream a file's content through a channel. Each chunk from pCloud is sent immediately.
    pub async fn get_object_stream(
        &self,
        bucket: &str,
        key: &str,
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
            Some("chunked") => self.get_chunked_file_stream(bucket, key, tx).await,
            _ => {
                let obj = self.meta.get_object(bucket, key)?
                    .ok_or_else(|| anyhow::anyhow!("Object not found: {}/{}", bucket, key))?;
                let backend = self.find_backend(&obj.account_email)?;
                // Stream from pCloud: each chunk forwarded through the channel as it arrives
                // No full-file buffering — VLC can start playing immediately
                backend.backend.download_stream(&obj.remote_path, None, None, tx).await
            }
        }
    }

    async fn get_chunked_file_stream(
        &self,
        bucket: &str,
        key: &str,
        tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, anyhow::Error>>,
    ) -> anyhow::Result<()> {
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

        for ci in &chunks_info {
            if let Some(backend) = backends.iter().find(|b| b.label == ci.account_email) {
                let owned_path = if ci.remote_path.is_empty() {
                    format!("{}/{}/{}.ck.{}", backend.mount_prefix, bucket, key, ci.index)
                } else {
                    ci.remote_path.clone()
                };

                match backend.backend.download(&owned_path).await {
                    Ok(data) => {
                        for chunk in data.chunks(64 * 1024) {
                            if tx.send(Ok(bytes::Bytes::copy_from_slice(chunk))).await.is_err() {
                                // Receiver dropped (client disconnected)
                                return Ok(());
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to download chunk {} of {}/{}: {}", ci.index, bucket, key, e);
                        let _ = tx.send(Err(anyhow::anyhow!("Storage error: {}", e))).await;
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
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
}
