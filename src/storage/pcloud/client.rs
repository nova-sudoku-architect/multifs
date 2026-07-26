
use serde::{Deserialize, Serialize};

/// pCloud API client
pub struct PCloudClient {
    email: String,
    token: String,
    base_url: String,
    client: reqwest::Client,
}

impl PCloudClient {
    /// Create a new pCloud API client
    /// Note: EU accounts use eapi.pcloud.com
    pub fn new(email: &str, token: &str) -> Self {
        Self {
            email: email.to_string(),
            token: token.to_string(),
            base_url: "https://eapi.pcloud.com".to_string(),
            client: reqwest::Client::builder()
                .user_agent("multifs/0.1.0")
                .build()
                .expect("Failed to build HTTP client"),
        }
    }

    /// Check account quota. Returns (used_bytes, total_bytes).
    pub async fn check_quota(&self) -> anyhow::Result<(i64, i64)> {
        let resp = self
            .client
            .get(format!("{}/userinfo", self.base_url))
            .query(&[("access_token", &self.token)])
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        let result: i64 = body["result"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("Missing 'result' in response: {}", body))?;

        if result != 0 {
            anyhow::bail!("pCloud API error {}: {}", result, body["error"]);
        }

        // pCloud API returns top-level fields: usedquota and quota
        let used = body["usedquota"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("Missing usedquota in userinfo response"))?;
        let total = body["quota"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("Missing quota in userinfo response"))?;

        Ok((used, total))
    }

    /// Ensure a directory path exists on pCloud (mkdir -p)
    pub async fn ensure_path(&self, path: &str) -> anyhow::Result<()> {
        let resp = self
            .client
            .get(format!("{}/listfolder", self.base_url))
            .query(&[
                ("access_token", self.token.as_str()),
                ("path", path),
                ("nofiles", "1"),
            ])
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        let result = body["result"].as_i64().unwrap_or(-1);

        // If folder exists, we're done
        if result == 0 {
            return Ok(());
        }

        // Otherwise, create directories recursively
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let mut current = String::new();

        for part in &parts {
            current.push('/');
            current.push_str(part);
            let resp = self
                .client
                .get(format!("{}/createfolder", self.base_url))
                .query(&[
                    ("access_token", self.token.as_str()),
                    ("path", current.as_str()),
                ])
                .send()
                .await?;

            let body: serde_json::Value = resp.json().await?;
            let result = body["result"].as_i64().unwrap_or(-1);
            // Ignore "already exists" errors
            if result != 0 && result != 2004 && result != 2005 {
                anyhow::bail!("Failed to create folder '{}': {}", current, body["error"]);
            }
        }

        Ok(())
    }

    /// Upload a file to pCloud
    /// Returns the actual remote path and file ID
    pub async fn upload(&self, remote_path: &str, data: &[u8]) -> anyhow::Result<(String, i64)> {
        // Ensure parent directory exists
        let parent = std::path::Path::new(remote_path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("/");
        self.ensure_path(parent).await?;

        let filename = std::path::Path::new(remote_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");

        let form = reqwest::multipart::Form::new()
            .part(
                filename.to_string(),
                reqwest::multipart::Part::bytes(data.to_vec())
                    .file_name(filename.to_string()),
            );

        let resp = self
            .client
            .post(format!("{}/uploadfile", self.base_url))
            .query(&[
                ("access_token", self.token.as_str()),
                ("path", parent),
                ("filename", filename),
                ("nopartial", "1"),
            ])
            .multipart(form)
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        let result = body["result"].as_i64().unwrap_or(-1);

        if result != 0 {
            anyhow::bail!("Upload error {}: {}", result, body["error"]);
        }

        // Get file metadata from response
        let metadata = &body["metadata"];
        let file_id = metadata.as_array()
            .and_then(|arr| arr.first())
            .and_then(|f| f["fileid"].as_i64())
            .unwrap_or(0);

        let actual_path = metadata.as_array()
            .and_then(|arr| arr.first())
            .and_then(|f| f["path"].as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| remote_path.to_string());

        Ok((actual_path, file_id))
    }

    /// Download a file from pCloud
    pub async fn download(&self, remote_path: &str) -> anyhow::Result<Vec<u8>> {
        // First, get the file link
        let resp = self
            .client
            .get(format!("{}/getfilelink", self.base_url))
            .query(&[
                ("access_token", self.token.as_str()),
                ("path", remote_path),
            ])
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        let result = body["result"].as_i64().unwrap_or(-1);

        if result != 0 {
            anyhow::bail!("Get file link error {}: {}", result, body["error"]);
        }

        let host = body["hosts"]
            .as_array()
            .and_then(|h| h.first())
            .and_then(|h| h.as_str())
            .ok_or_else(|| anyhow::anyhow!("No hosts in response"))?;
        let link_path = body["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("No path in response"))?;

        let download_url = format!("https://{}{}", host, link_path);
        
        // Stream from pCloud: read chunks sequentially and append to result
        // This way we start returning data without waiting for the entire file.
        let mut response = self.client.get(&download_url).send().await?;
        let mut result_bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            result_bytes.extend_from_slice(&chunk);
        }
        Ok(result_bytes)
    }

    /// Copy a file server-side using pCloud's copyfile API
    pub async fn copy_file(&self, source_path: &str, dest_parent: &str, new_name: &str) -> anyhow::Result<()> {
        let resp = self
            .client
            .get(format!("{}/copyfile", self.base_url))
            .query(&[
                ("access_token", self.token.as_str()),
                ("path", source_path),
                ("topath", dest_parent),
                ("toname", new_name),
            ])
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        let result = body["result"].as_i64().unwrap_or(-1);

        if result != 0 && result != 2004 {
            anyhow::bail!("pCloud copyfile error {}: {}", result, body["error"]);
        }

        Ok(())
    }

    /// Delete a file from pCloud
    pub async fn delete(&self, remote_path: &str) -> anyhow::Result<()> {
        let resp = self
            .client
            .get(format!("{}/deletefile", self.base_url))
            .query(&[
                ("access_token", self.token.as_str()),
                ("path", remote_path),
            ])
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        let result = body["result"].as_i64().unwrap_or(-1);

        if result != 0 {
            anyhow::bail!("Delete error {}: {}", result, body["error"]);
        }

        Ok(())
    }

    /// List files in a directory
    pub async fn list_folder(&self, path: &str) -> anyhow::Result<Vec<PCloudFile>> {
        let resp = self
            .client
            .get(format!("{}/listfolder", self.base_url))
            .query(&[
                ("access_token", self.token.as_str()),
                ("path", path),
            ])
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        let result = body["result"].as_i64().unwrap_or(-1);

        if result != 0 {
            anyhow::bail!("List folder error {}: {}", result, body["error"]);
        }

        let contents = body["metadata"]["contents"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("No contents in response"))?;

        let files: Vec<PCloudFile> = contents
            .iter()
            .filter(|f| f["isfolder"].as_bool().unwrap_or(false) == false)
            .map(|f| PCloudFile {
                file_id: f["fileid"].as_i64().unwrap_or(0),
                name: f["name"].as_str().unwrap_or("").to_string(),
                path: f["path"].as_str().unwrap_or("").to_string(),
                size: f["size"].as_i64().unwrap_or(0),
                modified: f["modified"].as_str().unwrap_or("").to_string(),
                is_folder: false,
            })
            .collect();

        Ok(files)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PCloudFile {
    pub file_id: i64,
    pub name: String,
    pub path: String,
    pub size: i64,
    pub modified: String,
    pub is_folder: bool,
}
