use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

use super::{StorageBackend, StorageFile};
use crate::config::AccountConfig;

/// Local disk storage backend.
///
/// Maps multifs remote paths (e.g. `/multifs/local/video/foo`) onto the local
/// filesystem under a configured root directory. The mount prefix is stripped
/// so `root/video/foo` is used, keeping the on-disk layout clean.
///
/// This backend is a first-class account in the storage pool: the `Utilization`
/// placement strategy treats it like any other shard and reports the real
/// filesystem free space via `statvfs`, so writes naturally spill to local disk
/// as the pCloud accounts fill up.
#[derive(Clone)]
pub struct LocalDiskBackend {
    label: String,
    root: PathBuf,
    mount_prefix: String,
    quota_gb: Option<u64>,
}

impl LocalDiskBackend {
    pub fn new(config: &AccountConfig) -> anyhow::Result<Self> {
        let path = config
            .path
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Local disk account '{}' requires a `path` (root directory)",
                    config.email
                )
            })?;
        let root = PathBuf::from(path);
        std::fs::create_dir_all(&root).map_err(|e| {
            anyhow::anyhow!("Failed to create local disk root {}: {}", root.display(), e)
        })?;
        Ok(Self {
            label: config.email.clone(),
            root,
            mount_prefix: config.mount_prefix.trim_matches('/').to_string(),
            quota_gb: config.quota_gb,
        })
    }

    /// Translate a remote path (leading slash, includes mount prefix) into a
    /// local filesystem path rooted at `self.root`.
    fn to_local(&self, remote_path: &str) -> PathBuf {
        let p = remote_path.trim_start_matches('/');
        let rel = if self.mount_prefix.is_empty() {
            p
        } else {
            p.strip_prefix(&self.mount_prefix)
                .map(|s| s.trim_start_matches('/'))
                .unwrap_or(p)
        };
        self.root.join(rel)
    }

    /// Filesystem usage via statvfs. Returns (used_bytes, total_bytes).
    fn statvfs_usage(&self) -> anyhow::Result<(i64, i64)> {
        use std::ffi::CString;
        let path_str = self.root.to_str().unwrap_or("/");
        let c = CString::new(path_str)
            .map_err(|_| anyhow::anyhow!("Invalid path containing NUL byte"))?;
        let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c.as_ptr(), &mut buf) };
        if rc != 0 {
            anyhow::bail!("statvfs failed for {}", path_str);
        }
        let frsize = buf.f_frsize as i64;
        let total = (buf.f_blocks as i64).saturating_mul(frsize);
        let free = (buf.f_bavail as i64).saturating_mul(frsize);
        let used = total.saturating_sub(free);
        Ok((used, total))
    }

    /// Total bytes stored under the root directory (multifs's own usage).
    fn dir_size(&self) -> i64 {
        let mut total: i64 = 0;
        for entry in walkdir::WalkDir::new(&self.root).follow_links(false).into_iter().filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() {
                total = total.saturating_add(entry.metadata().map(|m| m.len() as i64).unwrap_or(0));
            }
        }
        total
    }
}

#[async_trait]
impl StorageBackend for LocalDiskBackend {
    fn name(&self) -> &str {
        &self.label
    }

    async fn check_quota(&self) -> anyhow::Result<(i64, i64)> {
        // If a quota cap is configured, report multifs's own usage under the root
        // against that cap (prevents filling the OS filesystem). Otherwise report
        // the filesystem's real usage. The 60s quota cache in the engine keeps
        // either path cheap.
        if let Some(gb) = self.quota_gb {
            let total = (gb as i64).saturating_mul(1_073_741_824);
            let used = tokio::task::spawn_blocking({
                let this = self.clone();
                move || this.dir_size()
            })
            .await?;
            return Ok((used, total));
        }
        tokio::task::spawn_blocking({
            let this = self.clone();
            move || this.statvfs_usage()
        })
        .await?
    }

    async fn upload(&self, remote_path: &str, data: &[u8]) -> anyhow::Result<(String, i64)> {
        let local = self.to_local(remote_path);
        if let Some(parent) = local.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Write to a temp file then atomically rename to avoid a partially-written
        // blob being visible to readers.
        let tmp = temp_sibling(&local);
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, &local).await?;
        Ok((remote_path.to_string(), 0))
    }

    async fn upload_stream(
        &self,
        remote_path: &str,
        stream: Box<dyn Stream<Item = Result<Bytes, anyhow::Error>> + Send + Unpin>,
    ) -> anyhow::Result<(String, i64, String, i64)> {
        use futures::StreamExt;
        use sha2::{Digest, Sha256};

        let local = self.to_local(remote_path);
        if let Some(parent) = local.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = temp_sibling(&local);

        let mut f = tokio::fs::File::create(&tmp).await?;
        let mut hasher = Sha256::new();
        let mut size: i64 = 0;
        let mut stream = stream;
        while let Some(chunk) = stream.next().await {
            let data = chunk?;
            hasher.update(&data);
            size = size.saturating_add(data.len() as i64);
            f.write_all(&data).await?;
        }
        f.flush().await?;
        drop(f);
        tokio::fs::rename(&tmp, &local).await?;

        let etag = format!("{:x}", hasher.finalize());
        Ok((remote_path.to_string(), 0, etag, size))
    }

    async fn download(&self, remote_path: &str) -> anyhow::Result<Vec<u8>> {
        let local = self.to_local(remote_path);
        let data = tokio::fs::read(&local)
            .await
            .map_err(|e| anyhow::anyhow!("Read {} failed: {}", local.display(), e))?;
        Ok(data)
    }

    async fn download_stream(
        &self,
        remote_path: &str,
        range_start: Option<u64>,
        range_end: Option<u64>,
        tx: tokio::sync::mpsc::Sender<Result<Bytes, anyhow::Error>>,
    ) -> anyhow::Result<()> {
        let local = self.to_local(remote_path);
        let mut f = tokio::fs::File::open(&local)
            .await
            .map_err(|e| anyhow::anyhow!("Open {} failed: {}", local.display(), e))?;

        if let Some(s) = range_start {
            f.seek(SeekFrom::Start(s)).await?;
        }

        // range_end is exclusive. Remaining bytes to deliver (None = until EOF).
        let mut remaining: Option<u64> = range_end.map(|e| {
            e.saturating_sub(range_start.unwrap_or(0))
        });

        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if let Some(0) = remaining {
                break;
            }
            let n = match remaining {
                Some(r) => {
                    let want = (r as usize).min(buf.len());
                    f.read(&mut buf[..want]).await?
                }
                None => f.read(&mut buf).await?,
            };
            if n == 0 {
                break;
            }
            if tx.send(Ok(Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
                break; // receiver dropped
            }
            if let Some(r) = remaining.as_mut() {
                *r = r.saturating_sub(n as u64);
            }
        }
        Ok(())
    }

    async fn delete(&self, remote_path: &str) -> anyhow::Result<()> {
        let local = self.to_local(remote_path);
        match tokio::fs::remove_file(&local).await {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(anyhow::anyhow!("Delete {} failed: {}", local.display(), e)),
        }
    }

    async fn delete_folder_recursive(&self, remote_path: &str) -> anyhow::Result<Option<u64>> {
        let local = self.to_local(remote_path);
        match tokio::fs::remove_dir_all(&local).await {
            Ok(_) => Ok(Some(0)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Some(0)),
            Err(e) => Err(anyhow::anyhow!("Delete folder {} failed: {}", local.display(), e)),
        }
    }

    async fn stat(&self, remote_path: &str) -> anyhow::Result<Option<i64>> {
        let local = self.to_local(remote_path);
        match tokio::fs::metadata(&local).await {
            Ok(m) if m.is_file() => Ok(Some(m.len() as i64)),
            Ok(_) => Ok(None), // a directory is not a blob
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow::anyhow!("Stat {} failed: {}", local.display(), e)),
        }
    }

    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<StorageFile>> {
        let local_prefix = self.to_local(prefix);
        let files = tokio::task::spawn_blocking({
            let root = self.root.clone();
            let local_prefix = local_prefix.clone();
            move || {
                let mut out = Vec::new();
                if !local_prefix.exists() {
                    return out;
                }
                for entry in walkdir::WalkDir::new(&local_prefix)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    let full = entry.path();
                    let rel = full
                        .strip_prefix(&root)
                        .map(|r| r.to_string_lossy().to_string())
                        .unwrap_or_else(|_| full.to_string_lossy().to_string());
                    let meta = entry.metadata().ok();
                    let size = meta.as_ref().map(|m| m.len() as i64).unwrap_or(0);
                    let modified = meta
                        .as_ref()
                        .and_then(|m| m.modified().ok())
                        .map(|t| {
                            chrono::DateTime::<chrono::Utc>::from(t)
                                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                                .to_string()
                        })
                        .unwrap_or_default();
                    let name = full
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    out.push(StorageFile {
                        name,
                        path: format!("/{}", rel),
                        size,
                        modified,
                        is_folder: false,
                    });
                }
                out
            }
        })
        .await?;
        Ok(files)
    }

    async fn server_side_copy(
        &self,
        source_path: &str,
        dest_path: &str,
    ) -> anyhow::Result<Option<String>> {
        let src = self.to_local(source_path);
        let dst = self.to_local(dest_path);
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // For large blobs, a reflink/hardlink copy is not guaranteed across all
        // filesystems; fall back to a full copy (correctness over speed).
        tokio::fs::copy(&src, &dst)
            .await
            .map_err(|e| anyhow::anyhow!("Copy {} -> {} failed: {}", src.display(), dst.display(), e))?;
        Ok(Some(dest_path.to_string()))
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }
}

/// Build a temp path next to `target` for atomic rename-into-place.
fn temp_sibling(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("blob");
    let id = uuid::Uuid::new_v4();
    let tmp_name = format!(".{}.{}.tmp", name, id);
    target.with_file_name(tmp_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(tmp: &tempfile::TempDir) -> LocalDiskBackend {
        let cfg = AccountConfig {
            email: "local-test".to_string(),
            backend_type: Some("local".to_string()),
            token_env: None,
            mount_prefix: "/multifs/local-test".to_string(),
            quota_gb: None,
            path: Some(tmp.path().to_string_lossy().to_string()),
            priority: None,
            token_override: None,
        };
        LocalDiskBackend::new(&cfg).unwrap()
    }

    #[test]
    fn path_mapping_strips_mount_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let b = backend(&tmp);
        let local = b.to_local("/multifs/local-test/video/foo.mkv");
        assert_eq!(local, tmp.path().join("video/foo.mkv"));
    }

    #[tokio::test]
    async fn upload_download_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let b = backend(&tmp);
        let data = b"hello local disk".to_vec();
        let (path, _id) = b.upload("/multifs/local-test/video/hello.bin", &data).await.unwrap();
        assert_eq!(path, "/multifs/local-test/video/hello.bin");
        let got = b.download("/multifs/local-test/video/hello.bin").await.unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn upload_stream_computes_etag() {
        let tmp = tempfile::tempdir().unwrap();
        let b = backend(&tmp);
        let chunks = vec![
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"def")),
        ];
        let stream = Box::new(futures::stream::iter(chunks));
        let (_, _id, etag, size) = b
            .upload_stream("/multifs/local-test/stream.bin", stream)
            .await
            .unwrap();
        assert_eq!(size, 6);
        // sha256("abcdef")
        assert_eq!(
            etag,
            "bef57ec7f53a6d40beb640a780a639c83bc29ac8a9816f1fc6c5c6dcd93c4721"
        );
    }

    #[tokio::test]
    async fn download_stream_honors_range() {
        let tmp = tempfile::tempdir().unwrap();
        let b = backend(&tmp);
        let data = b"0123456789".to_vec();
        b.upload("/multifs/local-test/range.bin", &data).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        b.download_stream(
            "/multifs/local-test/range.bin",
            Some(2),
            Some(5),
            tx,
        )
        .await
        .unwrap();
        let mut out = Vec::new();
        while let Some(chunk) = rx.recv().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(out, b"234");
    }

    #[tokio::test]
    async fn check_quota_reports_fs() {
        let tmp = tempfile::tempdir().unwrap();
        let b = backend(&tmp);
        let (used, total) = b.check_quota().await.unwrap();
        assert!(total > 0);
        assert!(used >= 0 && used <= total);
    }
}
