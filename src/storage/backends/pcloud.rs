use async_trait::async_trait;

use super::{StorageBackend, StorageFile};
use crate::config::AccountConfig;

/// pCloud storage backend
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

    async fn download(&self, remote_path: &str) -> anyhow::Result<Vec<u8>> {
        self.client.download(remote_path).await
    }

    async fn delete(&self, remote_path: &str) -> anyhow::Result<()> {
        self.client.delete(remote_path).await
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
}
