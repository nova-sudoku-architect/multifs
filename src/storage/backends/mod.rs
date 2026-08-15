use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;

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

    /// Upload a file from an in-memory buffer. Returns (remote_path, file_id).
    async fn upload(&self, remote_path: &str, data: &[u8]) -> anyhow::Result<(String, i64)>;

    /// Upload a file from a streaming source, computing the SHA-256 ETag
    /// on-the-fly. Returns (remote_path, file_id, sha256_etag, file_size).
    async fn upload_stream(
        &self,
        remote_path: &str,
        stream: Box<dyn Stream<Item = Result<Bytes, anyhow::Error>> + Send + Unpin>,
    ) -> anyhow::Result<(String, i64, String, i64)>;

    /// Download a file's complete content.
    async fn download(&self, remote_path: &str) -> anyhow::Result<Vec<u8>>;

    /// Download a file, streaming chunks through a channel.
    /// Each chunk is sent as it arrives — no full-file buffering.
    /// Optional range_start/range_end for partial downloads (VLC seeking).
    async fn download_stream(
        &self,
        remote_path: &str,
        range_start: Option<u64>,
        range_end: Option<u64>,
        tx: tokio::sync::mpsc::Sender<Result<Bytes, anyhow::Error>>,
    ) -> anyhow::Result<()>;

    /// Delete a file.
    async fn delete(&self, remote_path: &str) -> anyhow::Result<()>;

    /// List files under a prefix (directory).
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<StorageFile>>;

    /// Server-side copy (optional). Returns the new remote path if supported.
    /// Returns `None` if the backend doesn't support server-side copy.
    async fn server_side_copy(
        &self,
        _source_path: &str,
        _dest_path: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    /// Clone this backend into a boxed trait object.
    fn clone_box(&self) -> Box<dyn StorageBackend>;
}

pub mod pcloud;
pub mod local_disk;
