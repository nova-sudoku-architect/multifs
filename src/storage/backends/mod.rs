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

    /// Download a file, streaming chunks through a channel.
    /// Each chunk is sent as it arrives — no full-file buffering.
    /// Optional range_start/range_end for partial downloads (VLC seeking).
    async fn download_stream(
        &self,
        remote_path: &str,
        range_start: Option<u64>,
        range_end: Option<u64>,
        tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, anyhow::Error>>,
    ) -> anyhow::Result<()>;

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

    /// Get a direct download link for a file (optional).
    /// Returns the URL or an error if not supported.
    /// Used by the link pre-fetch optimisation in stream_chunked_file_range.
    async fn get_download_link(&self, _remote_path: &str) -> anyhow::Result<String> {
        anyhow::bail!("get_download_link not implemented for this backend");
    }

    /// Clone this backend into a boxed trait object.
    fn clone_box(&self) -> Box<dyn StorageBackend>;
}
pub mod pcloud;
