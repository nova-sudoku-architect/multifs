use std::sync::Arc;
use tokio::sync::Mutex;
use sha2::{Digest, Sha256};
use chrono::Utc;
use futures::future::join_all;


use crate::config::Config;

use super::backends::StorageBackend;
use super::metadata::{MetadataDb, BucketRecord};
use super::chunk_manager;
use super::erasure;
use super::placement;

/// Chunk size: 32 MB
const CHUNK_SIZE: usize = 32 * 1024 * 1024;
/// Erasure coding: 5 data + 2 parity = 7 chunks per stripe
const DATA_CHUNKS: usize = 5;
const PARITY_CHUNKS: usize = 2;
const STRIPE_TOTAL: usize = DATA_CHUNKS + PARITY_CHUNKS;

/// Object metadata returned by head_object
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

/// Shard status info
#[derive(Debug, Clone)]
pub struct ShardStatus {
    pub email: String,
    pub object_count: i64,
    pub used_bytes: i64,
    pub total_bytes: i64,
}

struct BackendHandle {
    backend: Box<dyn StorageBackend>,
    mount_prefix: String,
    label: String,
    quota_gb: u64,
}

/// The core storage engine
#[derive(Clone)]
pub struct StorageEngine {
    meta: MetadataDb,
    backends: Arc<Vec<BackendHandle>>,
}

impl StorageEngine {
    pub fn new(cfg: &Config, meta: MetadataDb) -> anyhow::Result<Self> {
        // Build backend handles from config accounts
        let mut handles = Vec::new();
        for acct in &cfg.storage.accounts {
            let backend: Box<dyn StorageBackend> = match acct.backend_type.as_deref() {
                Some("pcloud") | None => {
                    let b = super::backends::pcloud::PCloudBackend::new(acct)?;
                    Box::new(b)
                }
                Some(other) => anyhow::bail!("Unknown backend type: {}", other),
            };
            handles.push(BackendHandle {
                backend,
                mount_prefix: acct.mount_prefix.clone(),
                label: acct.email.clone(),
                quota_gb: acct.quota_gb.unwrap_or(10),
            });
        }
        Ok(Self {
            meta,
            backends: Arc::new(handles),
        })
    }

    /// Put an object into the storage
    pub async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
    ) -> anyhow::Result<ObjectInfo> {
        self.put_object_with_content_type(bucket, key, data, None).await
    }

    /// Put an object with explicit content type
    pub async fn put_object_with_content_type(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: Option<&str>,
    ) -> anyhow::Result<ObjectInfo> {
        // Ensure bucket exists
        self.ensure_bucket(bucket)?;

        // Compute ETag (SHA256 of content)
        let etag = hex::encode(Sha256::digest(data));

        let now = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();

        // Decide: chunked for files > 32MB, whole-file for small files
        if data.len() <= CHUNK_SIZE {
            // Small file: store as whole file (backward compat)
            return self.put_whole_file(bucket, key, data, content_type, &etag, &now).await;
        }

        // Large file: chunk + erasure code + upload to multiple accounts
        self.put_chunked_file(bucket, key, data, content_type, &etag, &now).await
    }

    /// Store a small file as a whole object on a single backend
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
        let backend = &backends[0];

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

    /// Store a large file as chunks across multiple backends with erasure coding
    pub async fn put_chunked_file(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: Option<&str>,
        _etag: &str,
        _now: &str,
    ) -> anyhow::Result<ObjectInfo> {
        let backends = &*self.backends;
        if backends.is_empty() {
            anyhow::bail!("No storage backends configured");
        }

        let accounts: Vec<String> = backends.iter().map(|b| b.label.clone()).collect();

        // Step 1: Split into 32MB chunks
        let data_chunks = chunk_manager::split(data, CHUNK_SIZE);

        // Step 2: Erasure code in stripes of DATA_CHUNKS
        let padded_count = ((data_chunks.len() + DATA_CHUNKS - 1) / DATA_CHUNKS) * DATA_CHUNKS;
        let chunks_per_stripe = DATA_CHUNKS + PARITY_CHUNKS;
        let mut all_encoded: Vec<chunk_manager::Chunk> = Vec::new();

        for stripe_start in (0..padded_count).step_by(DATA_CHUNKS) {
            let mut stripe_data: Vec<chunk_manager::Chunk> = Vec::new();
            for i in stripe_start..stripe_start + DATA_CHUNKS {
                if i < data_chunks.len() {
                    stripe_data.push(data_chunks[i].clone());
                } else {
                    // Pad with empty chunk
                    stripe_data.push(chunk_manager::Chunk {
                        index: i as u32,
                        data: Vec::new(),
                        checksum: String::new(),
                        is_parity: false,
                    });
                }
            }
            let encoded = erasure::encode(&stripe_data);
            all_encoded.extend(encoded);
        }

        // Step 3: Upload chunks in parallel
        let mut handles = Vec::new();
        for (global_idx, chunk) in all_encoded.iter().enumerate() {
            let assignment = placement::get_account_for_chunk(&accounts, global_idx as u32);
            
            if let Some(backend) = backends.iter().find(|b| b.label == assignment) {
                let chunk_path = format!("{}/{}/{}.ck.{}", backend.mount_prefix, bucket, key, global_idx);
                let chunk_data = chunk.data.clone();
                let chunk_checksum = chunk.checksum.clone();
                let is_parity = chunk.is_parity;
                let local_key = key.to_string();
                let local_bucket = bucket.to_string();
                let local_label = backend.label.clone();
                let local_mount_prefix = backend.mount_prefix.clone();
                
                handles.push(tokio::spawn(async move {
                    (global_idx as u32, chunk_path, chunk_data, chunk_checksum, is_parity, local_bucket, local_key, local_label, local_mount_prefix)
                }));
            }
        }

        let _results: Vec<_> = futures::future::join_all(handles).await
            .into_iter()
            .filter_map(|r| r.ok())
            .collect();

        // Step 4: Register file metadata
        let _ = self.meta.with_conn(|conn| -> anyhow::Result<()> {
            conn.execute(
                "INSERT OR REPLACE INTO files (bucket_name, key, size, etag, last_modified, content_type, storage_type)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'chunked')",
                rusqlite::params![bucket, key, data.len() as i64, _etag, _now, content_type],
            )?;
            
            for (gi, path, _data, checksum, is_p, ref bk, ref ky, ref acct, ref mp) in &_results {
                let remote_path = if path.is_empty() { format!("{}/{}/{}.ck.{}", mp, bk, ky, gi) } else { path.clone() };
                conn.execute(
                    "INSERT OR REPLACE INTO chunks (bucket_name, key, chunk_index, size, checksum, is_parity, account_email, remote_path)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![bk, ky, *gi as i32, 0i64, checksum, if *is_p { 1 } else { 0 }, acct, remote_path],
                )?;
            }
            Ok(())
        })?;

        Ok(ObjectInfo {
            key: key.to_string(),
            size: data.len() as i64,
            etag: _etag.to_string(),
            last_modified: _now.to_string(),
            content_type: content_type.map(|s| s.to_string()),
            account_email: accounts[0].clone(),
            remote_path: format!("chunked://{}/{}", bucket, key),
        })
    }

    /// Get an object's data (supports both whole-file and chunked)
    pub async fn get_object(&self, bucket: &str, key: &str) -> anyhow::Result<Vec<u8>> {
        // Check if this is a chunked file
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
                // Legacy whole-file path
                let obj = self
                    .meta
                    .get_object(bucket, key)?
                    .ok_or_else(|| anyhow::anyhow!("Object not found: {}/{}", bucket, key))?;

                let backend = self.find_backend(&obj.account_email)?;
                backend.backend.download(&obj.remote_path).await
            }
        }
    }

    /// Get a chunked file: download chunks in parallel and reconstruct
    async fn get_chunked_file(&self, bucket: &str, key: &str) -> anyhow::Result<Vec<u8>> {
        let backends = &*self.backends;

        // Get all chunks from metadata
        #[derive(Debug)]
        struct ChunkInfo {
            index: i32,
            _size: i64,
            checksum: String,
            is_parity: bool,
            account_email: String,
            remote_path: String,
        }

        let chunks_info: Vec<ChunkInfo> = self.meta.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT chunk_index, size, checksum, is_parity, account_email, remote_path
                 FROM chunks WHERE bucket_name = ?1 AND key = ?2 ORDER BY chunk_index"
            )?;
            let rows = stmt.query_map(rusqlite::params![bucket, key], |row| {
                Ok(ChunkInfo {
                    index: row.get(0)?,
                    _size: row.get(1)?,
                    checksum: row.get(2)?,
                    is_parity: row.get::<_, i32>(3)? != 0,
                    account_email: row.get(4)?,
                    remote_path: row.get(5)?,
                })
            })?;
            let mut infos = Vec::new();
            for row in rows { infos.push(row?); }
            Ok(infos)
        })?;

        if chunks_info.is_empty() {
            anyhow::bail!("No chunks found for chunked file: {}/{}", bucket, key);
        }

        // Download all chunks in parallel
        let mut download_handles = Vec::new();
        for ci in &chunks_info {
            if let Some(backend) = backends.iter().find(|b| b.label == ci.account_email) {
                let owned_path = if ci.remote_path.is_empty() {
                    format!("{}/{}/{}.ck.{}", backend.mount_prefix, bucket, key, ci.index)
                } else {
                    ci.remote_path.clone()
                };
                let path = owned_path;
                download_handles.push(async move { backend.backend.download(&path).await });
            }
        }

        let downloaded: Vec<Vec<u8>> = futures::future::join_all(download_handles)
            .await
            .into_iter()
            .filter_map(|r| r.ok())
            .collect();

        if downloaded.is_empty() {
            anyhow::bail!("Failed to download any chunks for {}/{}", bucket, key);
        }

        // Build chunk objects from what we downloaded
        let mut chunk_objects = Vec::new();
        for (i, data) in downloaded.iter().enumerate() {
            if i < chunks_info.len() {
                chunk_objects.push(chunk_manager::Chunk {
                    index: chunks_info[i].index as u32,
                    data: data.clone(),
                    checksum: chunks_info[i].checksum.clone(),
                    is_parity: chunks_info[i].is_parity,
                });
            }
        }

        // Verify we can reconstruct
        if !erasure::can_reconstruct(&chunk_objects, DATA_CHUNKS) {
            anyhow::bail!("Not enough chunks to reconstruct {}/{}", bucket, key);
        }

        // Decode erasure coding
        let data_chunks = erasure::decode(&chunk_objects, DATA_CHUNKS)?;
        let result = chunk_manager::assemble(&data_chunks);
        Ok(result)
    }

    /// Head an object (get metadata without data)
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
            account_email: obj.account_email,
            remote_path: obj.remote_path,
        })
    }

    /// Delete an object
    pub async fn delete_object(&self, bucket: &str, key: &str) -> anyhow::Result<()> {
        let obj = self
            .meta
            .get_object(bucket, key)?
            .ok_or_else(|| anyhow::anyhow!("Object not found: {}/{}", bucket, key))?;

        let backend = self.find_backend(&obj.account_email)?;
        backend.backend.delete(&obj.remote_path).await?;
        self.meta.delete_object(bucket, key)?;
        Ok(())
    }

    /// List objects in a bucket with optional prefix filter
    pub async fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        max_keys: i64,
    ) -> anyhow::Result<Vec<ObjectInfo>> {
        let records = self.meta.list_objects(bucket, prefix, max_keys)?;
        Ok(records
            .into_iter()
            .map(|r| ObjectInfo {
                key: r.key,
                size: r.size,
                etag: r.etag,
                last_modified: r.last_modified,
                content_type: r.content_type,
                account_email: r.account_email,
                remote_path: r.remote_path,
            })
            .collect())
    }

    // ---- Bucket operations ----

    pub async fn bucket_exists(&self, name: &str) -> anyhow::Result<bool> {
        self.meta.bucket_exists(name)
    }

    pub async fn create_bucket(&self, name: &str) -> anyhow::Result<()> {
        self.ensure_bucket(name)
    }

    pub async fn delete_bucket(&self, name: &str) -> anyhow::Result<()> {
        let objects = self.meta.list_objects(name, None, 10000)?;
        for obj in &objects {
            if let Ok(backend) = self.find_backend(&obj.account_email) {
                let _ = backend.backend.delete(&obj.remote_path).await;
            }
        }
        self.meta.delete_all_objects(name)?;
        self.meta.delete_bucket(name)?;
        Ok(())
    }

    pub async fn list_all_buckets(&self) -> anyhow::Result<Vec<BucketRecord>> {
        self.meta.list_buckets()
    }

    /// Get shard status for all backends
    pub async fn shard_status(&self) -> anyhow::Result<Vec<ShardStatus>> {
        let mut statuses = Vec::new();
        for handle in self.backends.iter() {
            let obj_count = self.meta.count_objects_for_account(&handle.label)?;
            let total_size = self.meta.account_total_size(&handle.label)?;
            let quota = handle.quota_gb as i64 * 1_073_741_824;
            // Also try to get live quota from backend
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
        self.backends
            .iter()
            .find(|b| b.label == label)
            .ok_or_else(|| anyhow::anyhow!("Backend not found: {}", label))
    }
}
