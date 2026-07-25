use async_trait::async_trait;

/// File entry returned by list operations
#[derive(Debug, Clone)]
pub struct StorageFile {
    pub name: String,
    pub path: String,
    pub size: i64,
    pub modified: String,
    pub is_folder: bool,
}

/// Generic storage backend trait.
/// Implement this to add a new cloud storage provider.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Human-readable name for this backend instance
    fn name(&self) -> &str;

    /// Check account usage. Returns (used_bytes, total_bytes).
    async fn check_quota(&self) -> anyhow::Result<(i64, i64)>;

    /// Upload a file. Returns (remote_path, file_id).
    async fn upload(&self, remote_path: &str, data: &[u8]) -> anyhow::Result<(String, i64)>;

    /// Download a file's content.
    async fn download(&self, remote_path: &str) -> anyhow::Result<Vec<u8>>;

    /// Delete a file.
    async fn delete(&self, remote_path: &str) -> anyhow::Result<()>;

    /// List files under a prefix (directory).
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<StorageFile>>;

    /// Server-side copy (optional). Returns the new remote path if supported.
    /// Returns `None` if the backend doesn't support server-side copy.
    /// `source_path` is the backend's remote path to copy FROM.
    /// Returns the destination remote path on success.
    async fn server_side_copy(&self, _source_path: &str, _dest_path: &str) -> anyhow::Result<Option<String>> {
        // Default: not supported
        Ok(None)
    }
}
pub mod pcloud;
