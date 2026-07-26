/// In-memory mock backend for testing without real cloud accounts.
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::storage::backends::{StorageBackend, StorageFile};

pub struct MockBackend {
    pub name: String,
    pub files: Mutex<HashMap<String, Vec<u8>>>,
}

impl MockBackend {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            files: Mutex::new(HashMap::new()),
        }
    }

    pub fn file_count(&self) -> usize {
        self.files.lock().unwrap().len()
    }
}

#[async_trait]
impl StorageBackend for MockBackend {
    fn name(&self) -> &str {
        &self.name
    }

    async fn check_quota(&self) -> anyhow::Result<(i64, i64)> {
        let used: i64 = self.files.lock().unwrap().values().map(|v| v.len() as i64).sum();
        Ok((used, 1_000_000_000))
    }

    async fn upload(&self, remote_path: &str, data: &[u8]) -> anyhow::Result<(String, i64)> {
        self.files.lock().unwrap().insert(remote_path.to_string(), data.to_vec());
        Ok((remote_path.to_string(), data.len() as i64))
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
        _range_start: Option<u64>,
        _range_end: Option<u64>,
        tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, anyhow::Error>>,
    ) -> anyhow::Result<()> {
        // For mock: send the full file in 64KB chunks (no real Range support needed for tests)
        let data = self.download(remote_path).await?;
        for chunk in data.chunks(64 * 1024) {
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
