use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, watch};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// Page size: 16 KB
pub const PAGE_SIZE: usize = 16 * 1024;

/// Bitmap length for a given chunk size
fn bitmap_len(chunk_size: usize) -> usize {
    (chunk_size + PAGE_SIZE - 1) / PAGE_SIZE
}

/// A contiguous byte range that is page-aligned
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRange {
    pub start_byte: usize,
    pub length: usize,
}

impl PageRange {
    pub fn end_byte(&self) -> usize { self.start_byte + self.length }
}

/// Tracks a single chunk cached on disk with its page bitmap.
struct ChunkState {
    file_path: PathBuf,
    /// Bitmap: bit i = 1 means page i is cached
    bitmap: Vec<u8>,
    chunk_size: usize,
    access_ord: u64,
    /// Notifies waiters when pages are written
    notify: watch::Sender<()>,
}

/// PageCache: manages chunk files on `/tmp` with per-page bitmap tracking,
/// merged contiguous page requests, LRU eviction, and per-chunk notifications.
pub struct PageCache {
    cache_dir: PathBuf,
    max_chunks: usize,
    state: Arc<Mutex<HashMap<String, ChunkState>>>,
    counter: Arc<Mutex<u64>>,
}

impl PageCache {
    pub fn new(cache_dir: &str, max_chunks: usize) -> Self {
        let dir = PathBuf::from(cache_dir);
        let _ = std::fs::create_dir_all(&dir);
        Self {
            cache_dir: dir,
            max_chunks,
            state: Arc::new(Mutex::new(HashMap::new())),
            counter: Arc::new(Mutex::new(0)),
        }
    }

    fn cache_key(bucket: &str, key: &str, chunk_index: i32) -> String {
        format!("{}/{}/ck.{}", bucket, key, chunk_index)
    }

    fn file_path(&self, ck: &str) -> PathBuf {
        self.cache_dir.join(ck.replace('/', "_"))
    }

    /// Read a single page from cache. Returns `None` if not cached or read fails.
    pub async fn get_page(
        &self,
        bucket: &str, key: &str, chunk_index: i32,
        page_num: usize, chunk_size: usize,
    ) -> Option<Vec<u8>> {
        let ck = Self::cache_key(bucket, key, chunk_index);
        let mut map = self.state.lock().await;
        if let Some(cs) = map.get_mut(&ck) {
            let bm_len = bitmap_len(cs.chunk_size);
            if page_num >= bm_len { return None; }
            let byte = page_num / 8;
            let bit = page_num % 8;
            if byte < cs.bitmap.len() && (cs.bitmap[byte] & (1 << bit)) != 0 {
                // Touch LRU
                let mut ctr = self.counter.lock().await;
                *ctr += 1;
                cs.access_ord = *ctr;
                drop(ctr);
                // Read from disk
                let offset = page_num * PAGE_SIZE;
                let len = PAGE_SIZE.min(chunk_size.saturating_sub(offset));
                match read_partial(&cs.file_path, offset, len).await {
                    Ok(data) if !data.is_empty() => Some(data),
                    _ => {
                        cs.bitmap[byte] &= !(1 << bit);
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Write pages to a chunk file. Updates bitmap and notifies waiters.
    /// `start_byte` is the offset in the chunk, `data` is the byte slice to write.
    pub async fn put_pages(
        &self,
        bucket: &str, key: &str, chunk_index: i32,
        start_byte: usize, data: &[u8], chunk_size: usize,
    ) {
        let ck = Self::cache_key(bucket, key, chunk_index);
        let mut map = self.state.lock().await;

        // Evict if at capacity
        if !map.contains_key(&ck) && map.len() >= self.max_chunks {
            Self::evict_one(&mut map);
        }

        // Get or create chunk state
        let file_path = self.file_path(&ck);
        let bm_len = bitmap_len(chunk_size);

        let (exists, should_notify) = if let Some(cs) = map.get_mut(&ck) {
            // Write to file
            let _ = write_partial(&cs.file_path, start_byte, data).await;
            // Update bitmap
            let start_page = start_byte / PAGE_SIZE;
            let end_page = ((start_byte + data.len()) + PAGE_SIZE - 1) / PAGE_SIZE;
            for p in start_page..end_page.min(bm_len) {
                let b = p / 8;
                let bit = p % 8;
                if b < cs.bitmap.len() {
                    cs.bitmap[b] |= 1 << bit;
                }
            }
            let mut ctr = self.counter.lock().await;
            *ctr += 1;
            cs.access_ord = *ctr;
            (true, true)
        } else {
            // Create new chunk file
            let _ = write_partial(&file_path, start_byte, data).await;
            let mut bitmap = vec![0u8; (bm_len + 7) / 8];
            let start_page = start_byte / PAGE_SIZE;
            let end_page = ((start_byte + data.len()) + PAGE_SIZE - 1) / PAGE_SIZE;
            for p in start_page..end_page.min(bm_len) {
                let b = p / 8;
                let bit = p % 8;
                if b < bitmap.len() {
                    bitmap[b] |= 1 << bit;
                }
            }
            let mut ctr = self.counter.lock().await;
            *ctr += 1;
            let (tx, _) = watch::channel(());
            map.insert(ck.clone(), ChunkState {
                file_path,
                bitmap,
                chunk_size,
                access_ord: *ctr,
                notify: tx,
            });
            (false, true)
        };

        if should_notify {
            if let Some(cs) = map.get(&ck) {
                let _ = cs.notify.send(());
            }
        }
    }

    /// Find contiguous missing pages within `[byte_start, byte_end)`. Returns
    /// merged ranges so the caller can make one pCloud request per range.
    pub async fn missing_ranges(
        &self,
        bucket: &str, key: &str, chunk_index: i32,
        byte_start: usize, byte_end: usize, chunk_size: usize,
    ) -> Vec<PageRange> {
        let ck = Self::cache_key(bucket, key, chunk_index);
        let map = self.state.lock().await;
        let bm_len = bitmap_len(chunk_size);
        let bitmap = map.get(&ck).map(|cs| &cs.bitmap[..]);

        let start_page = byte_start / PAGE_SIZE;
        let end_page = (byte_end + PAGE_SIZE - 1) / PAGE_SIZE;
        let mut ranges: Vec<PageRange> = Vec::new();
        let mut i = start_page;
        while i < end_page && i < bm_len {
            let byte = i / 8;
            let bit = i % 8;
            let cached = bitmap.map_or(false, |bm| byte < bm.len() && (bm[byte] & (1 << bit)) != 0);
            if !cached {
                let range_start = i * PAGE_SIZE;
                let mut j = i + 1;
                while j < end_page && j < bm_len {
                    let b2 = j / 8;
                    let bi2 = j % 8;
                    let c = bitmap.map_or(false, |bm| b2 < bm.len() && (bm[b2] & (1 << bi2)) != 0);
                    if c { break; }
                    j += 1;
                }
                let range_end = (j * PAGE_SIZE).min(chunk_size).min(byte_end);
                if range_end > range_start {
                    ranges.push(PageRange { start_byte: range_start, length: range_end - range_start });
                }
                i = j;
            } else {
                i += 1;
            }
        }
        ranges
    }

    /// Subscribe to notifications for a chunk (returns None if chunk not tracked)
    pub async fn subscribe(
        &self, bucket: &str, key: &str, chunk_index: i32,
    ) -> Option<watch::Receiver<()>> {
        let ck = Self::cache_key(bucket, key, chunk_index);
        let map = self.state.lock().await;
        map.get(&ck).map(|cs| cs.notify.subscribe())
    }

    pub async fn clear(&self) {
        let mut map = self.state.lock().await;
        for cs in map.values() { let _ = std::fs::remove_file(&cs.file_path); }
        map.clear();
        *self.counter.lock().await = 0;
    }

    pub async fn len(&self) -> usize { self.state.lock().await.len() }

    pub async fn chunk_ids(&self) -> Vec<String> {
        self.state.lock().await.keys().cloned().collect()
    }

    fn evict_one(map: &mut HashMap<String, ChunkState>) {
        if let Some(victim) = map.iter().min_by_key(|(_, cs)| cs.access_ord).map(|(k, _)| k.clone()) {
            if let Some(cs) = map.remove(&victim) {
                let _ = std::fs::remove_file(&cs.file_path);
            }
        }
    }
}

/// Read bytes from a file at offset
async fn read_partial(path: &Path, offset: usize, len: usize) -> std::io::Result<Vec<u8>> {
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(std::io::SeekFrom::Start(offset as u64)).await?;
    let mut buf = vec![0u8; len];
    let n = f.read(&mut buf).await?;
    buf.truncate(n);
    if buf.iter().all(|&b| b == 0) && n == len {
        // Sparse file: the file was created but never written at this offset
        // Check file size
        let meta = f.metadata().await?;
        if offset as u64 >= meta.len() {
            return Ok(vec![]);
        }
    }
    Ok(buf)
}

/// Write bytes to a file at offset (creates sparse file if needed)
async fn write_partial(path: &Path, offset: usize, data: &[u8]) -> std::io::Result<()> {
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(path).await?;
    f.seek(std::io::SeekFrom::Start(offset as u64)).await?;
    f.write_all(data).await?;
    f.sync_all().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CS: usize = 65536; // 64 KB chunks for tests

    fn test_cache(name: &str) -> PageCache {
        let dir = format!("/tmp/multifs_pc_test_{}_{}", std::process::id(), name);
        let _ = std::fs::remove_dir_all(&dir);
        PageCache::new(&dir, 5)
    }

    #[tokio::test]
    async fn test_put_and_get_page() {
        let c = test_cache("pg");
        let payload = b"page0_data";
        c.put_pages("b", "k", 0, 0, payload, CS).await;
        let data = c.get_page("b", "k", 0, 0, CS).await;
        assert!(data.is_some());
        assert_eq!(data.unwrap().as_slice(), &payload[..]);
    }

    #[tokio::test]
    async fn test_get_missing_page() {
        let c = test_cache("miss");
        assert!(c.get_page("b", "k", 0, 0, CS).await.is_none());
    }

    #[tokio::test]
    async fn test_multiple_pages() {
        let c = test_cache("multi");
        let chunk_size = 3 * PAGE_SIZE;
        let mut data = vec![0u8; PAGE_SIZE + 16]; // covers pages 0 and 1
        data[..10].copy_from_slice(b"page0_data");
        data[PAGE_SIZE..PAGE_SIZE+12].copy_from_slice(b"_page1_data!");
        c.put_pages("b", "k", 0, 0, &data, chunk_size).await;
        let d0 = c.get_page("b", "k", 0, 0, chunk_size).await;
        assert_eq!(&d0.unwrap()[..10], b"page0_data");
        let d1 = c.get_page("b", "k", 0, 1, chunk_size).await;
        let d1_data = d1.unwrap();
        assert_eq!(&d1_data[..12], b"_page1_data!");
    }

    #[tokio::test]
    async fn test_missing_ranges_none_cached() {
        let c = test_cache("mr_none");
        c.put_pages("b", "k", 0, 0, b"partial", CS).await;
        let ranges = c.missing_ranges("b", "k", 0, 0, CS, CS).await;
        assert!(!ranges.is_empty(), "should have missing ranges");
        assert!(ranges[0].start_byte >= PAGE_SIZE, "first gap should start after page 0");
    }

    #[tokio::test]
    async fn test_missing_ranges_all_cached() {
        let c = test_cache("mr_all");
        let big = vec![0u8; CS];
        c.put_pages("b", "k", 0, 0, &big, CS).await;
        let ranges = c.missing_ranges("b", "k", 0, 0, CS, CS).await;
        assert!(ranges.is_empty(), "all pages cached");
    }

    #[tokio::test]
    async fn test_missing_ranges_merges_contiguous() {
        let c = test_cache("mr_merge");
        // Cache only pages 0 and 3 (leaving 1-2 as gap)
        c.put_pages("b", "k", 0, 0, b"p0", CS).await;
        c.put_pages("b", "k", 0, 3 * PAGE_SIZE, b"p3", CS).await;
        let ranges = c.missing_ranges("b", "k", 0, 0, 4 * PAGE_SIZE, CS).await;
        assert_eq!(ranges.len(), 1, "pages 1-2 should merge into one range");
        assert_eq!(ranges[0].start_byte, PAGE_SIZE);
        assert_eq!(ranges[0].length, 2 * PAGE_SIZE);
    }

    #[tokio::test]
    async fn test_eviction() {
        let c = test_cache("evict");
        for i in 0..5 {
            c.put_pages("b", "k", i, 0, &[i as u8], PAGE_SIZE).await;
        }
        assert_eq!(c.len().await, 5);
        c.put_pages("b", "k", 5, 0, &[5], PAGE_SIZE).await;
        assert_eq!(c.len().await, 5);
        // Chunk 0 should be evicted
        assert!(c.get_page("b", "k", 0, 0, PAGE_SIZE).await.is_none());
        assert!(c.get_page("b", "k", 5, 0, PAGE_SIZE).await.is_some());
    }

    #[tokio::test]
    async fn test_lru_refresh() {
        let c = test_cache("lru");
        for i in 0..5 {
            c.put_pages("b", "k", i, 0, &[i as u8], PAGE_SIZE).await;
        }
        // Access chunk 0 (refreshes LRU)
        assert!(c.get_page("b", "k", 0, 0, PAGE_SIZE).await.is_some());
        c.put_pages("b", "k", 5, 0, &[5], PAGE_SIZE).await;
        assert!(c.get_page("b", "k", 0, 0, PAGE_SIZE).await.is_some());
        assert!(c.get_page("b", "k", 1, 0, PAGE_SIZE).await.is_none());
    }

    #[tokio::test]
    async fn test_clear() {
        let c = test_cache("clear");
        c.put_pages("b", "k", 0, 0, b"data", PAGE_SIZE).await;
        assert_eq!(c.len().await, 1);
        c.clear().await;
        assert_eq!(c.len().await, 0);
        assert!(c.get_page("b", "k", 0, 0, PAGE_SIZE).await.is_none());
    }

    #[tokio::test]
    async fn test_notify_on_put() {
        let c = std::sync::Arc::new(test_cache("notify"));
        c.put_pages("b", "k", 0, 0, b"hello", PAGE_SIZE).await;
        let mut rx = c.subscribe("b", "k", 0).await.expect("subscribe after put");
        c.put_pages("b", "k", 0, PAGE_SIZE, b"world", PAGE_SIZE * 2).await;
        assert!(rx.changed().await.is_ok(), "should be notified");
    }

    #[tokio::test]
    async fn test_subscribe_before_chunk_exists() {
        let c = std::sync::Arc::new(test_cache("sub_before"));
        assert!(c.subscribe("b", "k", 0).await.is_none());
        c.put_pages("b", "k", 0, 0, b"hello", PAGE_SIZE).await;
        assert!(c.subscribe("b", "k", 0).await.is_some());
    }

    #[tokio::test]
    async fn test_put_pages_at_offset() {
        let c = test_cache("offset");
        // Write data at offset 32K (page 2)
        let data = b"hello_at_page_2";
        c.put_pages("b", "k", 0, 2 * PAGE_SIZE, data, CS).await;
        // Page 0 should be missing
        assert!(c.get_page("b", "k", 0, 0, CS).await.is_none());
        // Page 2 should have the data
        let d2 = c.get_page("b", "k", 0, 2, CS).await;
        assert!(d2.is_some());
        assert_eq!(d2.unwrap().as_slice(), &data[..]);
    }

    #[tokio::test]
    async fn test_missing_ranges_middle_only() {
        let c = test_cache("mr_mid");
        // Cache page 0 and page 3, not 1-2
        let full_page = vec![1u8; PAGE_SIZE];
        c.put_pages("b", "k", 0, 0, &full_page, CS).await;
        c.put_pages("b", "k", 0, 3 * PAGE_SIZE, &full_page, CS).await;
        let ranges = c.missing_ranges("b", "k", 0, 0, 4 * PAGE_SIZE, CS).await;
        assert_eq!(ranges.len(), 1, "pages 1-2 should be a single gap");
        assert_eq!(ranges[0].start_byte, PAGE_SIZE);
        assert_eq!(ranges[0].length, 2 * PAGE_SIZE);
    }
}
