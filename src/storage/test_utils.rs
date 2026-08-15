use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::storage::backends::{StorageBackend, StorageFile};

pub struct MockBackend {
    pub name: String,
    pub files: Mutex<HashMap<String, Vec<u8>>>,
    pub total: i64,
}

impl MockBackend {
    pub fn new(name: &str) -> Self {
        Self::with_total(name, 1_000_000_000)
    }

    /// Mock backend with a configurable quota total (so a small file can fill it).
    pub fn with_total(name: &str, total: i64) -> Self {
        Self {
            name: name.to_string(),
            files: Mutex::new(HashMap::new()),
            total,
        }
    }

    pub fn file_count(&self) -> usize {
        self.files.lock().unwrap().len()
    }
}

impl Clone for MockBackend {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            files: Mutex::new(self.files.lock().unwrap().clone()),
            total: self.total,
        }
    }
}

#[async_trait]
impl StorageBackend for MockBackend {
    fn name(&self) -> &str {
        &self.name
    }

    async fn check_quota(&self) -> anyhow::Result<(i64, i64)> {
        let used: i64 = self.files.lock().unwrap().values().map(|v| v.len() as i64).sum();
        Ok((used, self.total))
    }

    async fn upload(&self, remote_path: &str, data: &[u8]) -> anyhow::Result<(String, i64)> {
        self.files.lock().unwrap().insert(remote_path.to_string(), data.to_vec());
        Ok((remote_path.to_string(), data.len() as i64))
    }

    async fn upload_stream(
        &self,
        remote_path: &str,
        stream: Box<dyn Stream<Item = Result<Bytes, anyhow::Error>> + Send + Unpin>,
    ) -> anyhow::Result<(String, i64, String, i64)> {
        use sha2::{Digest, Sha256};
        use futures::StreamExt;
        let mut all = Vec::new();
        let mut hasher = Sha256::new();
        let mut pinned = stream;
        while let Some(item) = pinned.next().await {
            let chunk = item?;
            hasher.update(&chunk);
            all.extend_from_slice(&chunk);
        }
        let etag = hex::encode(hasher.finalize());
        let size = all.len() as i64;
        self.files.lock().unwrap().insert(remote_path.to_string(), all);
        Ok((remote_path.to_string(), 0, etag, size))
    }

    async fn download(&self, remote_path: &str) -> anyhow::Result<Vec<u8>> {
        self.files
            .lock()
            .unwrap()
            .get(remote_path)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("File not found: {}", remote_path))
    }

    async fn download_stream(
        &self,
        remote_path: &str,
        range_start: Option<u64>,
        range_end: Option<u64>,
        tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, anyhow::Error>>,
    ) -> anyhow::Result<()> {
        // Honor the byte range (inclusive start, exclusive end) and stream the
        // slice in 64KB chunks. Ignoring the range would flood the bounded
        // channel with the full file and deadlock when the caller awaits the
        // stream setup before draining.
        let data = self.download(remote_path).await?;
        let start = range_start.unwrap_or(0) as usize;
        let end = range_end
            .map(|e| e as usize)
            .unwrap_or(data.len())
            .min(data.len());
        let slice = if start < end && start < data.len() {
            &data[start..end]
        } else {
            &[][..]
        };
        for chunk in slice.chunks(64 * 1024) {
            if tx.send(Ok(bytes::Bytes::copy_from_slice(chunk))).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn delete(&self, remote_path: &str) -> anyhow::Result<()> {
        self.files.lock().unwrap().remove(remote_path);
        Ok(())
    }

    async fn list(&self, _prefix: &str) -> anyhow::Result<Vec<StorageFile>> {
        let files = self.files.lock().unwrap();
        Ok(files
            .iter()
            .map(|(path, data)| StorageFile {
                name: path.rsplit('/').next().unwrap_or(path).to_string(),
                path: path.clone(),
                size: data.len() as i64,
                modified: "2026-01-01".to_string(),
                is_folder: false,
            })
            .collect())
    }

    async fn stat(&self, remote_path: &str) -> anyhow::Result<Option<i64>> {
        Ok(self.files.lock().unwrap().get(remote_path).map(|d| d.len() as i64))
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }
}

/// Build a test StorageEngine with two MockBackends, keeping the TempDir alive.
pub fn make_test_engine() -> (crate::storage::engine::StorageEngine, tempfile::TempDir) {
    use crate::storage::metadata::MetadataDb;
    use crate::storage::engine::StorageEngine;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

    let backends: Vec<crate::storage::engine::BackendHandle> = vec![
        crate::storage::engine::BackendHandle::new(
            Box::new(MockBackend::new("mock-a")),
            "/mnt/mock-a".to_string(),
            "mock-a".to_string(),
            10,
        ),
        crate::storage::engine::BackendHandle::new(
            Box::new(MockBackend::new("mock-b")),
            "/mnt/mock-b".to_string(),
            "mock-b".to_string(),
            10,
        ),
    ];

    let engine = StorageEngine::from_backends(backends, db);
    (engine, dir)
}

// ---- TrackableMockBackend (with spy and configurable latency) ----
//
// Wraps a MockBackend with call tracking and simulated latency.
// Used by streaming performance tests to verify which chunks were accessed
// and to simulate realistic network conditions.

use std::sync::atomic::{AtomicBool, AtomicI64};

/// A record of a backend access via download or download_stream.
#[derive(Debug, Clone)]
pub struct AccessRecord {
    pub method: &'static str,  // "download" or "download_stream"
    pub remote_path: String,
}

/// A spy backend that wraps any StorageBackend with call tracking,
/// configurable latency, and the ability to set a "missing" path list
/// (for erasure recovery testing).
#[derive(Clone)]
pub struct TrackedBackend {
    inner: std::sync::Arc<dyn StorageBackend + Send + Sync>,
    pub name: String,
    pub accesses: std::sync::Arc<Mutex<Vec<AccessRecord>>>,
    pub latency_ms: std::sync::Arc<AtomicI64>,
    /// Optional set of remote_paths that will cause download/download_stream to fail
    /// (to simulate missing chunks).
    pub missing_paths: std::sync::Arc<Mutex<Vec<String>>>,
}

impl TrackedBackend {
    pub fn wrap(backend: Box<dyn StorageBackend + Send + Sync>) -> Self {
        let name = backend.name().to_string();
        Self {
            inner: std::sync::Arc::from(backend),
            name,
            accesses: std::sync::Arc::new(Mutex::new(Vec::new())),
            latency_ms: std::sync::Arc::new(AtomicI64::new(0)),
            missing_paths: std::sync::Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Set a simulated latency that applies to every download/download_stream call.
    pub fn set_latency_ms(&self, ms: i64) {
        self.latency_ms.store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Mark a remote_path as "missing" so calls to it return error (for erasure testing).
    pub fn add_missing_path(&self, path: &str) {
        self.missing_paths.lock().unwrap().push(path.to_string());
    }

    /// Clear all access records.
    pub fn clear_accesses(&self) {
        self.accesses.lock().unwrap().clear();
    }

    /// Get the list of remote_paths that were accessed via download/download_stream.
    pub fn accessed_paths(&self) -> Vec<String> {
        self.accesses.lock().unwrap().iter().map(|a| a.remote_path.clone()).collect()
    }

    /// Check if a specific remote_path was accessed.
    pub fn was_accessed(&self, path: &str) -> bool {
        self.accesses.lock().unwrap().iter().any(|a| a.remote_path == path)
    }

    async fn apply_latency(&self) {
        let ms = self.latency_ms.load(std::sync::atomic::Ordering::Relaxed);
        if ms > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(ms as u64)).await;
        }
    }

    fn is_missing(&self, path: &str) -> bool {
        self.missing_paths.lock().unwrap().iter().any(|p| p == path)
    }
}

#[async_trait]
impl StorageBackend for TrackedBackend {
    fn name(&self) -> &str {
        &self.name
    }

    async fn check_quota(&self) -> anyhow::Result<(i64, i64)> {
        self.inner.check_quota().await
    }

    async fn upload(&self, remote_path: &str, data: &[u8]) -> anyhow::Result<(String, i64)> {
        self.inner.upload(remote_path, data).await
    }

    async fn upload_stream(
        &self,
        remote_path: &str,
        stream: Box<dyn Stream<Item = Result<Bytes, anyhow::Error>> + Send + Unpin>,
    ) -> anyhow::Result<(String, i64, String, i64)> {
        self.inner.upload_stream(remote_path, stream).await
    }

    async fn download(&self, remote_path: &str) -> anyhow::Result<Vec<u8>> {
        self.accesses.lock().unwrap().push(AccessRecord {
            method: "download",
            remote_path: remote_path.to_string(),
        });
        if self.is_missing(remote_path) {
            return Err(anyhow::anyhow!("Simulated missing chunk: {}", remote_path));
        }
        self.apply_latency().await;
        self.inner.download(remote_path).await
    }

    async fn download_stream(
        &self,
        remote_path: &str,
        range_start: Option<u64>,
        range_end: Option<u64>,
        tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, anyhow::Error>>,
    ) -> anyhow::Result<()> {
        self.accesses.lock().unwrap().push(AccessRecord {
            method: "download_stream",
            remote_path: remote_path.to_string(),
        });
        if self.is_missing(remote_path) {
            return Err(anyhow::anyhow!("Simulated missing chunk: {}", remote_path));
        }
        self.apply_latency().await;
        self.inner.download_stream(remote_path, range_start, range_end, tx).await
    }

    async fn delete(&self, remote_path: &str) -> anyhow::Result<()> {
        self.inner.delete(remote_path).await
    }

    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<StorageFile>> {
        self.inner.list(prefix).await
    }

    async fn stat(&self, remote_path: &str) -> anyhow::Result<Option<i64>> {
        self.inner.stat(remote_path).await
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }
}

/// Build a test StorageEngine with two TrackedBackends (wrapping MockBackends),
/// keeping the TempDir alive.
pub fn make_tracked_engine() -> (crate::storage::engine::StorageEngine, tempfile::TempDir, std::sync::Arc<TrackedBackend>, std::sync::Arc<TrackedBackend>) {
    use crate::storage::metadata::MetadataDb;
    use crate::storage::engine::StorageEngine;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

    let b1 = std::sync::Arc::new(TrackedBackend::wrap(Box::new(MockBackend::new("tracked-a"))));
    let b2 = std::sync::Arc::new(TrackedBackend::wrap(Box::new(MockBackend::new("tracked-b"))));

    let backends: Vec<crate::storage::engine::BackendHandle> = vec![
        crate::storage::engine::BackendHandle::new(
            Box::new((*b1).clone()),
            "/mnt/tracked-a".to_string(),
            "tracked-a".to_string(),
            10,
        ),
        crate::storage::engine::BackendHandle::new(
            Box::new((*b2).clone()),
            "/mnt/tracked-b".to_string(),
            "tracked-b".to_string(),
            10,
        ),
    ];

    let engine = StorageEngine::from_backends(backends, db);
    (engine, dir, b1, b2)
}
