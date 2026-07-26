use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// LRU-evicted local disk cache for downloaded chunks.
///
/// Avoids redundant pCloud API calls when the same chunk is requested
/// multiple times (e.g., VLC seeking back/forth, parallel Range requests).
pub struct ChunkCache {
    cache_dir: PathBuf,
    max_entries: usize,
    index: Arc<Mutex<HashMap<String, CacheEntry>>>,
    counter: Arc<Mutex<u64>>,
}

struct CacheEntry {
    disk_path: PathBuf,
    access_ord: u64,
}

impl ChunkCache {
    pub fn new(cache_dir: &str, max_entries: usize) -> Self {
        let dir = PathBuf::from(cache_dir);
        let _ = std::fs::create_dir_all(&dir);
        Self { cache_dir: dir, max_entries, index: Arc::new(Mutex::new(HashMap::new())), counter: Arc::new(Mutex::new(0)) }
    }

    fn cache_key(bucket: &str, key: &str, chunk_index: i32) -> String {
        format!("{}/{}.ck.{}", bucket, key, chunk_index)
    }

    pub async fn get(&self, bucket: &str, key: &str, chunk_index: i32) -> Option<Vec<u8>> {
        let ck = Self::cache_key(bucket, key, chunk_index);
        let mut map = self.index.lock().await;
        let mut ctr = self.counter.lock().await;
        if let Some(entry) = map.get_mut(&ck) {
            *ctr += 1;
            entry.access_ord = *ctr;
            match tokio::fs::read(&entry.disk_path).await {
                Ok(data) => Some(data),
                Err(_) => { map.remove(&ck); None }
            }
        } else {
            None
        }
    }

    pub async fn put(&self, bucket: &str, key: &str, chunk_index: i32, data: &[u8]) {
        let ck = Self::cache_key(bucket, key, chunk_index);
        let mut map = self.index.lock().await;
        let mut ctr = self.counter.lock().await;
        if !map.contains_key(&ck) && map.len() >= self.max_entries {
            let victim_key = map.iter().min_by_key(|(_, e)| e.access_ord).map(|(k, _)| k.clone());
            if let Some(vk) = victim_key {
                if let Some(ve) = map.remove(&vk) {
                    let _ = std::fs::remove_file(&ve.disk_path);
                }
            }
        }
        let safe = format!("{}_{}_{}", bucket, key.replace('/', "_"), chunk_index);
        let dp = self.cache_dir.join(&safe);
        if tokio::fs::write(&dp, data).await.is_ok() {
            *ctr += 1;
            map.insert(ck, CacheEntry { disk_path: dp, access_ord: *ctr });
        }
    }

    pub async fn clear(&self) {
        let mut map = self.index.lock().await;
        for e in map.values() { let _ = std::fs::remove_file(&e.disk_path); }
        map.clear();
        *self.counter.lock().await = 0;
    }

    pub async fn len(&self) -> usize { self.index.lock().await.len() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Barrier;

    fn test_cache(name: &str) -> ChunkCache {
        let dir = format!("/tmp/multifs_test_cache_{}_{}", std::process::id(), name);
        let _ = std::fs::remove_dir_all(&dir);
        ChunkCache::new(&dir, 5)
    }

    #[tokio::test]
    async fn test_put_and_get() {
        let c = test_cache("put_get");
        c.put("bucket", "key", 0, b"hello").await;
        let data = c.get("bucket", "key", 0).await;
        assert_eq!(data, Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn test_get_missing() {
        let c = test_cache("get_missing");
        assert_eq!(c.get("bucket", "key", 99).await, None);
    }

    #[tokio::test]
    async fn test_cache_eviction() {
        let c = test_cache("eviction");
        for i in 0..5 {
            c.put("b", "k", i, &[i as u8]).await;
        }
        assert_eq!(c.len().await, 5);
        c.put("b", "k", 5, &[5]).await;
        assert_eq!(c.len().await, 5);
        assert_eq!(c.get("b", "k", 0).await, None);
        for i in 1..=5 {
            assert!(c.get("b", "k", i).await.is_some(), "chunk {} should exist", i);
        }
    }

    #[tokio::test]
    async fn test_lru_refresh_on_get() {
        let c = test_cache("lru");
        for i in 0..5 {
            c.put("b", "k", i, &[i as u8]).await;
        }
        assert!(c.get("b", "k", 0).await.is_some());
        c.put("b", "k", 5, &[5]).await;
        assert!(c.get("b", "k", 0).await.is_some());
        assert_eq!(c.get("b", "k", 1).await, None);
    }

    #[tokio::test]
    async fn test_clear() {
        let c = test_cache("clear");
        c.put("b", "k", 0, b"data").await;
        assert!(c.get("b", "k", 0).await.is_some());
        c.clear().await;
        assert_eq!(c.len().await, 0);
        assert_eq!(c.get("b", "k", 0).await, None);
    }

    #[tokio::test]
    async fn test_put_same_key_twice_updates() {
        let c = test_cache("same_key");
        c.put("b", "k", 0, b"old").await;
        c.put("b", "k", 0, b"new").await;
        assert_eq!(c.len().await, 1);
        let data = c.get("b", "k", 0).await;
        assert_eq!(data, Some(b"new".to_vec()));
    }

    #[tokio::test]
    async fn test_concurrent_access() {
        let c = Arc::new(test_cache("concurrent"));
        let mut handles = Vec::new();
        for i in 0..3 {
            let cc = c.clone();
            handles.push(tokio::spawn(async move {
                cc.put("b", "k", i, &[i as u8]).await;
                cc.get("b", "k", i).await
            }));
        }
        for (i, h) in handles.into_iter().enumerate() {
            let result = h.await.unwrap();
            assert_eq!(result, Some(vec![i as u8]));
        }
    }

    #[tokio::test]
    async fn test_different_buckets_dont_interfere() {
        let c = test_cache("buckets");
        c.put("bucket_a", "key1", 0, b"a").await;
        c.put("bucket_b", "key2", 0, b"b").await;
        assert_eq!(c.get("bucket_a", "key1", 0).await, Some(b"a".to_vec()));
        assert_eq!(c.get("bucket_b", "key2", 0).await, Some(b"b".to_vec()));
    }

    #[tokio::test]
    async fn test_key_path_normalization() {
        let c = test_cache("path");
        c.put("bucket", "path/to/file", 0, b"data").await;
        let data = c.get("bucket", "path/to/file", 0).await;
        assert_eq!(data, Some(b"data".to_vec()));
    }
}
