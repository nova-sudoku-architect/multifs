use std::path::PathBuf;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;

/// A simple LRU disk cache for object data
pub struct LocalCache {
    root: PathBuf,
    max_size: u64,
    current_size: Arc<Mutex<u64>>,
    lru_list: Arc<Mutex<VecDeque<String>>>,
}

impl LocalCache {
    /// Create or open a local cache at `root` with max size `max_size_mb` MB
    pub fn new(root: &str, max_size_mb: u64) -> anyhow::Result<Self> {
        let root = PathBuf::from(root);
        std::fs::create_dir_all(&root)?;

        let max_size = max_size_mb * 1_048_576;

        // Calculate current size by scanning
        let mut current_size = 0u64;
        if root.exists() {
            for entry in walkdir::WalkDir::new(&root).max_depth(3) {
                if let Ok(entry) = entry {
                    if entry.file_type().is_file() {
                        current_size += entry.metadata().map(|m| m.len()).unwrap_or(0);
                    }
                }
            }
        }

        Ok(Self {
            root,
            max_size,
            current_size: Arc::new(Mutex::new(current_size)),
            lru_list: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    /// Get the cache path for a given (account, bucket, key) tuple
    fn cache_path(&self, account: &str, bucket: &str, key: &str) -> PathBuf {
        // Flatten key into path segments
        let key_path = key.replace('/', "_");
        self.root.join(account).join(bucket).join(key_path)
    }

    /// Try to read from cache. Returns `None` if not cached.
    pub async fn get(&self, account: &str, bucket: &str, key: &str) -> Option<Vec<u8>> {
        let path = self.cache_path(account, bucket, key);
        if !path.exists() {
            return None;
        }

        // Update LRU order
        let key_str = format!("{}/{}/{}", account, bucket, key);
        let mut lru = self.lru_list.lock().await;
        if let Some(pos) = lru.iter().position(|k| k == &key_str) {
            lru.remove(pos);
            lru.push_back(key_str);
        }

        tokio::fs::read(&path).await.ok()
    }

    /// Write data to cache
    pub async fn put(&self, account: &str, bucket: &str, key: &str, data: &[u8]) {
        let path = self.cache_path(account, bucket, key);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // Check if we need to evict
        let mut current = self.current_size.lock().await;
        let data_len = data.len() as u64;

        // Evict entries until we have space
        let mut lru = self.lru_list.lock().await;
        while *current + data_len > self.max_size && !lru.is_empty() {
            if let Some(old_key) = lru.pop_front() {
                let parts: Vec<&str> = old_key.splitn(3, '/').collect();
                if parts.len() == 3 {
                    let old_path = self.cache_path(parts[0], parts[1], parts[2]);
                    if old_path.exists() {
                        if let Ok(meta) = old_path.metadata() {
                            *current -= meta.len();
                        }
                        let _ = std::fs::remove_file(&old_path);
                    }
                }
            }
        }

        // Write new data
        if let Ok(()) = tokio::fs::write(&path, data).await {
            *current += data_len;
            let key_str = format!("{}/{}/{}", account, bucket, key);
            lru.push_back(key_str);
        }
    }

    /// Invalidate cache entry
    pub async fn invalidate(&self, account: &str, bucket: &str, key: &str) {
        let path = self.cache_path(account, bucket, key);
        let _ = std::fs::remove_file(&path);

        let key_str = format!("{}/{}/{}", account, bucket, key);
        let mut lru = self.lru_list.lock().await;
        if let Some(pos) = lru.iter().position(|k| k == &key_str) {
            lru.remove(pos);
        }
    }

    /// Get current cache size in bytes
    pub async fn current_size(&self) -> u64 {
        *self.current_size.lock().await
    }
}
