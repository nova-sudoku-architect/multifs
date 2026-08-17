use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;

use super::{StorageBackend, StorageFile};
use crate::config::AccountConfig;

/// pCloud storage backend
#[derive(Clone)]
pub struct PCloudBackend {
    config: AccountConfig,
    client: crate::storage::pcloud::client::PCloudClient,
}

impl PCloudBackend {
    pub fn new(config: &AccountConfig) -> anyhow::Result<Self> {
        let token = config.resolve_token()?;
        let client = crate::storage::pcloud::client::PCloudClient::new(&config.email, &token);
        Ok(Self {
            config: config.clone(),
            client,
        })
    }
}

#[async_trait]
impl StorageBackend for PCloudBackend {
    fn name(&self) -> &str {
        &self.config.email
    }

    async fn check_quota(&self) -> anyhow::Result<(i64, i64)> {
        self.client.check_quota().await
    }

    async fn upload(&self, remote_path: &str, data: &[u8]) -> anyhow::Result<(String, i64)> {
        self.client.upload(remote_path, data).await
    }

    async fn upload_stream(
        &self,
        remote_path: &str,
        stream: Box<dyn Stream<Item = Result<Bytes, anyhow::Error>> + Send + Unpin>,
    ) -> anyhow::Result<(String, i64, String, i64)> {
        self.client.upload_stream(remote_path, stream).await
    }

    async fn download(&self, remote_path: &str) -> anyhow::Result<Vec<u8>> {
        self.client.download(remote_path).await
    }

    async fn download_stream(
        &self,
        remote_path: &str,
        range_start: Option<u64>,
        range_end: Option<u64>,
        tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, anyhow::Error>>,
    ) -> anyhow::Result<()> {
        self.client.download_stream(remote_path, range_start, range_end, tx).await
    }

    async fn delete(&self, remote_path: &str) -> anyhow::Result<()> {
        self.client.delete(remote_path).await
    }

    async fn delete_folder_recursive(&self, remote_path: &str) -> anyhow::Result<Option<u64>> {
        Ok(Some(self.client.delete_folder_recursive(remote_path).await?))
    }

    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<StorageFile>> {
        let files = self.client.list_folder(prefix).await?;
        Ok(files.into_iter().map(|f| StorageFile {
            name: f.name,
            path: f.path,
            size: f.size,
            modified: f.modified,
            is_folder: f.is_folder,
        }).collect())
    }

    async fn stat(&self, remote_path: &str) -> anyhow::Result<Option<i64>> {
        self.client.stat(remote_path).await
    }

    /// Server-side copy using pCloud's copyfile API (instant for same account)
    async fn server_side_copy(&self, source_path: &str, dest_parent: &str) -> anyhow::Result<Option<String>> {
        // Extract filename from source path
        let filename = std::path::Path::new(source_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");

        // Ensure destination directory exists
        self.client.ensure_path(dest_parent).await?;

        // Perform server-side copy
        self.client.copy_file(source_path, dest_parent, filename).await?;

        let dest_path = format!("{}/{}", dest_parent.trim_end_matches('/'), filename);
        Ok(Some(dest_path))
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }
}
