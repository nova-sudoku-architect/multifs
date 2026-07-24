use std::sync::Arc;
use tokio::sync::Mutex;
use sha2::{Digest, Sha256};
use chrono::Utc;


use crate::config::Config;

use super::backends::StorageBackend;
use super::metadata::{MetadataDb, BucketRecord};

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
        // Ensure bucket exists
        self.ensure_bucket(bucket)?;

        // Compute ETag (SHA256 of content)
        let etag = hex::encode(Sha256::digest(data));

        // Pick backend — round-robin for now (pick by object count)
        let backends = &*self.backends;
        if backends.is_empty() {
            anyhow::bail!("No storage backends configured");
        }
        let backend = &backends[0]; // For now, always pick first

        let remote_path = format!("{}/{}/{}", backend.mount_prefix, bucket, key);

        // Upload via the generic backend trait
        let (remote_path_actual, _) = backend.backend.upload(&remote_path, data).await?;

        // Record in metadata
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        self.meta.put_object(
            bucket,
            key,
            data.len() as i64,
            &etag,
            &now,
            &backend.label,
            &remote_path_actual,
            None,
        )?;

        Ok(ObjectInfo {
            key: key.to_string(),
            size: data.len() as i64,
            etag,
            last_modified: now,
            content_type: None,
            account_email: backend.label.clone(),
            remote_path: remote_path_actual,
        })
    }

    /// Get an object's data
    pub async fn get_object(&self, bucket: &str, key: &str) -> anyhow::Result<Vec<u8>> {
        let obj = self
            .meta
            .get_object(bucket, key)?
            .ok_or_else(|| anyhow::anyhow!("Object not found: {}/{}", bucket, key))?;

        let backend = self.find_backend(&obj.account_email)?;
        backend.backend.download(&obj.remote_path).await
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
