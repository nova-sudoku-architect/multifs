use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, watch};

/// Tracks in-flight chunk downloads so concurrent requests for the same
/// chunk share one download instead of starting duplicates.
pub struct DownloadTracker {
    pending: Arc<Mutex<HashMap<String, watch::Sender<Result<Vec<u8>, String>>>>>,
}

impl DownloadTracker {
    pub fn new() -> Self {
        Self { pending: Arc::new(Mutex::new(HashMap::new())) }
    }

    fn key(bucket: &str, key: &str, chunk_index: i32) -> String {
        format!("{}:{}:ck.{}", bucket, key, chunk_index)
    }

    pub async fn try_register(
        &self,
        bucket: &str,
        key: &str,
        chunk_index: i32,
    ) -> Result<watch::Receiver<Result<Vec<u8>, String>>, watch::Receiver<Result<Vec<u8>, String>>> {
        let k = Self::key(bucket, key, chunk_index);
        let mut map = self.pending.lock().await;
        if let Some(existing) = map.get(&k) {
            return Err(existing.subscribe());
        }
        let (tx, rx) = watch::channel(Err("pending".to_string()));
        map.insert(k, tx);
        Ok(rx)
    }

    pub async fn complete(&self, bucket: &str, key: &str, chunk_index: i32, result: Result<Vec<u8>, String>) {
        let k = Self::key(bucket, key, chunk_index);
        let mut map = self.pending.lock().await;
        if let Some(tx) = map.remove(&k) {
            let _ = tx.send(result);
        }
    }

    pub async fn cancel(&self, bucket: &str, key: &str, chunk_index: i32) {
        let k = Self::key(bucket, key, chunk_index);
        self.pending.lock().await.remove(&k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_first_registration_returns_ok() {
        let t = DownloadTracker::new();
        assert!(t.try_register("b", "k", 0).await.is_ok());
    }

    #[tokio::test]
    async fn test_second_registration_returns_existing() {
        let t = DownloadTracker::new();
        let _ = t.try_register("b", "k", 0).await.unwrap();
        assert!(t.try_register("b", "k", 0).await.is_err());
    }

    #[tokio::test]
    async fn test_both_waiters_get_result() {
        let t = Arc::new(DownloadTracker::new());
        let mut rx1 = t.try_register("b", "k", 0).await.unwrap();
        let mut rx2 = t.try_register("b", "k", 0).await.unwrap_err();
        t.complete("b", "k", 0, Ok(vec![1, 2, 3])).await;
        assert!(rx1.changed().await.is_ok());
        assert_eq!(*rx1.borrow(), Ok(vec![1, 2, 3]));
        assert!(rx2.changed().await.is_ok());
        assert_eq!(*rx2.borrow(), Ok(vec![1, 2, 3]));
    }

    #[tokio::test]
    async fn test_completion_clears_pending() {
        let t = DownloadTracker::new();
        let mut rx = t.try_register("b", "k", 0).await.unwrap();
        t.complete("b", "k", 0, Ok(vec![])).await;
        let _ = rx.changed().await;
        assert!(t.try_register("b", "k", 0).await.is_ok());
    }

    #[tokio::test]
    async fn test_error_on_one_chunk_does_not_affect_others() {
        let t = DownloadTracker::new();
        let _ = t.try_register("b", "k", 1).await.unwrap();
        let mut rx2 = t.try_register("b", "k", 2).await.unwrap();
        t.complete("b", "k", 1, Ok(vec![5, 6])).await;
        t.complete("b", "k", 2, Err("fail".to_string())).await;
        assert!(rx2.changed().await.is_ok());
        assert!(rx2.borrow().is_err());
    }

    #[tokio::test]
    async fn test_cancel_allows_retry() {
        let t = DownloadTracker::new();
        let _ = t.try_register("b", "k", 0).await.unwrap();
        t.cancel("b", "k", 0).await;
        assert!(t.try_register("b", "k", 0).await.is_ok());
    }

    #[tokio::test]
    async fn test_cancel_does_not_affect_other_chunks() {
        let t = DownloadTracker::new();
        let _ = t.try_register("b", "k", 1).await.unwrap();
        let mut rx2 = t.try_register("b", "k", 2).await.unwrap();
        t.cancel("b", "k", 1).await;
        assert!(t.try_register("b", "k", 2).await.is_err());
        t.complete("b", "k", 2, Ok(vec![7])).await;
        assert!(rx2.changed().await.is_ok());
        assert_eq!(*rx2.borrow(), Ok(vec![7]));
    }

    #[tokio::test]
    async fn test_independent_chunks() {
        let t = Arc::new(DownloadTracker::new());
        let mut r0 = t.try_register("b", "k", 0).await.unwrap();
        let mut r1 = t.try_register("b", "k", 1).await.unwrap();
        let mut r2 = t.try_register("b", "k", 2).await.unwrap();
        t.complete("b", "k", 0, Ok(vec![0])).await;
        t.complete("b", "k", 1, Ok(vec![1])).await;
        t.complete("b", "k", 2, Ok(vec![2])).await;
        for r in [&mut r0, &mut r1, &mut r2] {
            let _ = r.changed().await;
        }
        assert_eq!(*r0.borrow(), Ok(vec![0]));
        assert_eq!(*r1.borrow(), Ok(vec![1]));
        assert_eq!(*r2.borrow(), Ok(vec![2]));
    }
}
