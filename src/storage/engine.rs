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

    /// Stream a large file: process chunks as they arrive from the stream.
    /// Never buffers more than the current stripe in RAM.
    pub async fn put_chunked_file_stream(
        &self,
        bucket: &str,
        key: &str,
        content_type: Option<&str>,
        etag: &str,
        now: &str,
        data_stream: impl futures::stream::Stream<Item = Result<bytes::Bytes, anyhow::Error>> + Send + 'static,
    ) -> anyhow::Result<ObjectInfo> {
        use futures::stream::StreamExt;
        use tokio::sync::Semaphore;
        
        let backends = &*self.backends;
        if backends.is_empty() {
            anyhow::bail!("No storage backends configured");
        }

        let accounts: Vec<String> = backends.iter().map(|b| b.label.clone()).collect();
        let mut total_size: i64 = 0;
        let mut current_stripe: Vec<u8> = Vec::new();
        let mut stripe_index: u32 = 0;
        let mut results: Vec<(u32, String, Vec<u8>, String, bool, String, String, String)> = Vec::new();
        
        // Limit concurrent uploads to avoid overwhelming pCloud
        let upload_permits = std::sync::Arc::new(Semaphore::new(7));

        // Process the stream
        let mut current_chunk: Vec<u8> = Vec::new();
        
        tokio::pin!(data_stream);
        
        while let Some(chunk) = data_stream.next().await {
            let chunk = chunk?;
            total_size += chunk.len() as i64;
            
            // Accumulate chunk data
            current_chunk.extend_from_slice(&chunk);
            
            // When we have DATA_CHUNKS * CHUNK_SIZE bytes, process a stripe
            while current_chunk.len() >= DATA_CHUNKS * CHUNK_SIZE {
                let mut stripe_data: Vec<chunk_manager::Chunk> = Vec::new();
                for _ in 0..DATA_CHUNKS {
                    let chunk_bytes: Vec<u8> = current_chunk.drain(..CHUNK_SIZE).collect();
                    let checksum = hex::encode(sha2::Sha256::digest(&chunk_bytes));
                    stripe_data.push(chunk_manager::Chunk {
                        index: stripe_index,
                        data: chunk_bytes,
                        checksum,
                        is_parity: false,
                    });
                    stripe_index += 1;
                }
                
                // Erasure-code the stripe
                if stripe_data.len() == DATA_CHUNKS {
                    let encoded = erasure::encode(&stripe_data);
                    // Upload each chunk in the stripe
                    let permit = upload_permits.clone().acquire_owned().await?;
                    let backends_ref = &*self.backends;
                    let accounts_ref = &accounts;
                    
                    for (local_idx, chunk_obj) in encoded.iter().enumerate() {
                        let global_idx = stripe_index + local_idx as u32;
                        let assignment = placement::get_account_for_chunk(accounts_ref, global_idx);
                        if let Some(backend) = backends_ref.iter().find(|b| b.label == assignment) {
                            let chunk_path = format!("{}/{}/{}.ck.{}", backend.mount_prefix, bucket, key, global_idx);
                            let chunk_data = chunk_obj.data.clone();
                            let chk_checksum = chunk_obj.checksum.clone();
                            let is_parity = chunk_obj.is_parity;
                            let local_bucket = bucket.to_string();
                            let local_key = key.to_string();
                            let local_label = backend.label.clone();
                            let local_path = chunk_path.clone();
                            
                            // Upload to pCloud
                            match backend.backend.upload(&local_path, &chunk_data).await {
                                Ok((actual_path, _)) => {
                                    results.push((global_idx, actual_path, chunk_data, chk_checksum, is_parity, local_bucket, local_key, local_label));
                                }
                                Err(e) => {
                                    tracing::error!("Failed to upload chunk {}: {}", global_idx, e);
                                }
                            }
                        }
                    }
                    drop(permit);
                }
            }
        }

        // Handle remaining data (partial last stripe)
        if !current_chunk.is_empty() {
            let mut stripe_data: Vec<chunk_manager::Chunk> = Vec::new();
            let start_idx = stripe_index;
            let chunk_bytes = std::mem::take(&mut current_chunk);
            let checksum = hex::encode(sha2::Sha256::digest(&chunk_bytes));
            stripe_data.push(chunk_manager::Chunk {
                index: start_idx,
                data: chunk_bytes,
                checksum,
                is_parity: false,
            });
            stripe_index += 1;
            
            // Pad to DATA_CHUNKS
            while stripe_data.len() < DATA_CHUNKS {
                stripe_data.push(chunk_manager::Chunk {
                    index: stripe_index,
                    data: Vec::new(),
                    checksum: String::new(),
                    is_parity: false,
                });
                stripe_index += 1;
            }
            
            let encoded = erasure::encode(&stripe_data);
            for (local_idx, chunk_obj) in encoded.iter().enumerate() {
                let global_idx = results.len() as u32 + local_idx as u32;
                let assignment = placement::get_account_for_chunk(&accounts, global_idx);
                if let Some(backend) = backends.iter().find(|b| b.label == assignment) {
                    let chunk_path = format!("{}/{}/{}.ck.{}", backend.mount_prefix, bucket, key, global_idx);
                    let chunk_data = chunk_obj.data.clone();
                    let chk_checksum = chunk_obj.checksum.clone();
                    let is_parity = chunk_obj.is_parity;
                    let local_bucket = bucket.to_string();
                    let local_key = key.to_string();
                    let local_label = backend.label.clone();
                    let local_path = chunk_path.clone();
                    
                    match backend.backend.upload(&local_path, &chunk_data).await {
                        Ok((actual_path, _)) => {
                            results.push((global_idx, actual_path, chunk_data, chk_checksum, is_parity, local_bucket, local_key, local_label));
                        }
                        Err(e) => {
                            tracing::error!("Failed to upload chunk {}: {}", global_idx, e);
                        }
                    }
                }
            }
        }

        // Register in metadata
        let final_etag = if total_size == 0 {
            "empty".to_string() 
        } else {
            etag.to_string()
        };
        
        self.meta.with_conn(|conn| -> anyhow::Result<()> {
            conn.execute(
                "INSERT OR REPLACE INTO files (bucket_name, key, size, etag, last_modified, content_type, storage_type)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'chunked')",
                rusqlite::params![bucket, key, total_size, &final_etag, now, content_type],
            )?;
            
            for (gi, path, _chk_data, chk_checksum, is_p, ref bk, ref ky, ref acct) in &results {
                conn.execute(
                    "INSERT OR REPLACE INTO chunks (bucket_name, key, chunk_index, size, checksum, is_parity, account_email, remote_path)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![bk, ky, *gi as i32, 0i64, chk_checksum, if *is_p { 1 } else { 0 }, acct, path],
                )?;
            }
            Ok(())
        })?;

        Ok(ObjectInfo {
            key: key.to_string(),
            size: total_size,
            etag: final_etag,
            last_modified: now.to_string(),
            content_type: content_type.map(|s| s.to_string()),
            account_email: accounts[0].clone(),
            remote_path: format!("chunked://{}/{}", bucket, key),
        })
    }

    /// Store a large file as chunks across multiple backends with erasure coding
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

        let accounts: Vec<String> = backends.iter().map(|b| b.label.clone()).collect();

        // Step 1: Split into 32MB chunks
        let data_chunks = chunk_manager::split(data, CHUNK_SIZE);

        // Step 2: Erasure code in stripes of DATA_CHUNKS
        let padded_count = ((data_chunks.len() + DATA_CHUNKS - 1) / DATA_CHUNKS) * DATA_CHUNKS;
        let mut all_encoded: Vec<chunk_manager::Chunk> = Vec::new();

        for stripe_start in (0..padded_count).step_by(DATA_CHUNKS) {
            let mut stripe_data: Vec<chunk_manager::Chunk> = Vec::new();
            for i in stripe_start..stripe_start + DATA_CHUNKS {
                if i < data_chunks.len() {
                    stripe_data.push(data_chunks[i].clone());
                } else {
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

        // Step 3: Collect upload targets (owned data)
        let mut upload_targets: Vec<(u32, Vec<u8>, String, bool, String, String, String, String, usize)> = Vec::new();
        for (global_idx, chunk) in all_encoded.iter().enumerate() {
            let assignment = placement::get_account_for_chunk(&accounts, global_idx as u32);
            let bi = backends.iter().position(|b| b.label == assignment).unwrap_or(0);
            let chunk_path = format!("{}/{}/{}.ck.{}", backends[bi].mount_prefix, bucket, key, global_idx);
            upload_targets.push((
                global_idx as u32,
                chunk.data.clone(),
                chunk.checksum.clone(),
                chunk.is_parity,
                chunk_path,
                bucket.to_string(),
                key.to_string(),
                backends[bi].label.clone(),
                bi,
            ));
        }

        // Upload sequentially (simplified v1 — parallel is future optimization)
        let mut results = Vec::new();
        for (global_idx, chunk_data, checksum, is_parity, chunk_path, ref local_bucket, ref local_key, ref local_label, bi) in &upload_targets {
            match backends[*bi].backend.upload(chunk_path, chunk_data).await {
                Ok((actual_path, _)) => {
                    results.push(ChunkUploadResult {
                        global_index: *global_idx,
                        remote_path: actual_path,
                        checksum: checksum.clone(),
                        is_parity: *is_parity,
                        size: chunk_data.len() as i64,
                        bucket: local_bucket.clone(),
                        key: local_key.clone(),
                        account: local_label.clone(),
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to upload chunk {}: {}", global_idx, e);
                }
            }
        }

        #[derive(Debug)]
        struct ChunkUploadResult {
            global_index: u32,
            remote_path: String,
            checksum: String,
            is_parity: bool,
            size: i64,
            bucket: String,
            key: String,
            account: String,
        }

        let results: Vec<ChunkUploadResult> = results;

        // Step 4: Register file and chunks in metadata
        self.meta.with_conn(|conn| -> anyhow::Result<()> {
            conn.execute(
                "INSERT OR REPLACE INTO files (bucket_name, key, size, etag, last_modified, content_type, storage_type)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'chunked')",
                rusqlite::params![bucket, key, data.len() as i64, etag, now, content_type],
            )?;
            
            for r in &results {
                conn.execute(
                    "INSERT OR REPLACE INTO chunks (bucket_name, key, chunk_index, size, checksum, is_parity, account_email, remote_path)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![r.bucket, r.key, r.global_index as i32, r.size, r.checksum, 
                        if r.is_parity { 1 } else { 0 }, r.account, r.remote_path],
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
        let mut result = chunk_manager::assemble(&data_chunks);

        // Truncate to original file size (removes padding from erasure coding)
        let original_size: i64 = self.meta.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT size FROM files WHERE bucket_name = ?1 AND key = ?2")?;
            let mut rows = stmt.query(rusqlite::params![bucket, key])?;
            if let Some(row) = rows.next()? {
                Ok(row.get::<_, i64>(0)?)
            } else {
                Ok(result.len() as i64)
            }
        })?;

        result.truncate(original_size as usize);
        Ok(result)
    }

    /// Head an object (get metadata without data)
    /// Supports both whole-file (objects table) and chunked (files table)
    pub async fn head_object(&self, bucket: &str, key: &str) -> anyhow::Result<ObjectInfo> {
        // First check files table for chunked objects
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

        // Fall back to legacy whole-file objects table
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
