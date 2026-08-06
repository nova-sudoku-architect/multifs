
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};

/// pCloud API client
#[derive(Clone)]
pub struct PCloudClient {
    email: String,
    token: String,
    base_url: String,
    client: reqwest::Client,
    /// Directories already confirmed to exist (path -> created/verified once).
    /// Avoids a pCloud /listfolder round-trip on every chunk upload to the
    /// same bucket/prefix.
    known_dirs: Arc<StdMutex<HashSet<String>>>,
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
            known_dirs: Arc::new(StdMutex::new(HashSet::new())),
        }
    }

    /// Normalize a pCloud path: collapse duplicate slashes and trailing slash so
    // the same logical directory maps to one cache key.
    fn normalize_path(path: &str) -> String {
        let trimmed = path.trim_end_matches('/');
        let mut out = String::new();
        for part in trimmed.split('/') {
            if part.is_empty() {
                continue;
            }
            out.push('/');
            out.push_str(part);
        }
        if out.is_empty() {
            "/".to_string()
        } else {
            out
        }
    }

    /// Check account quota. Returns (used_bytes, total_bytes).
    pub async fn check_quota(&self) -> anyhow::Result<(i64, i64)> {
        let resp = self
            .client
            .post(format!("{}/userinfo", self.base_url))
            .form(&[("access_token", &self.token)])
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
    /// Ensure a directory path exists on pCloud (mkdir -p).
    ///
    /// Normalizes the path so different spellings of the same dir hit one cache
    /// entry, then returns immediately if we already know it exists. Only does a
    /// pCloud /listfolder (and possibly /createfolder) the first time a parent
    /// dir is seen — avoids a round-trip on every chunk upload to the same path.
    pub async fn ensure_path(&self, path: &str) -> anyhow::Result<()> {
        // Root or empty path is trivially present.
        let norm = Self::normalize_path(path);
        if norm.is_empty() || norm == "/" {
            return Ok(());
        }
        {
            let known = self.known_dirs.lock().unwrap();
            if known.contains(&norm) {
                return Ok(());
            }
        }

        let resp = self
            .client
            .post(format!("{}/listfolder", self.base_url))
            .form(&[
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
            self.known_dirs.lock().unwrap().insert(norm);
            return Ok(());
        }

        // Otherwise, create directories recursively
        let parts: Vec<&str> = norm.split('/').filter(|p| !p.is_empty()).collect();
        let mut current = String::new();

        for part in &parts {
            current.push('/');
            current.push_str(part);
            let resp = self
                .client
                .post(format!("{}/createfolder", self.base_url))
                .form(&[
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
            self.known_dirs.lock().unwrap().insert(current.clone());
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

        // Build multipart form with all fields (auth + file) in the same multipart body.
        // .form() and .multipart() can't be used together in reqwest — multipart wins.
        // So access_token, path, filename, and nopartial go as .text() fields alongside the file.
        let form = reqwest::multipart::Form::new()
            .text("access_token", self.token.clone())
            .text("path", parent.to_string())
            .text("filename", filename.to_string())
            .text("nopartial", "1")
            .part(
                "file",
                reqwest::multipart::Part::bytes(data.to_vec())
                    .file_name(filename.to_string()),
            );

        let resp = self
            .client
            .post(format!("{}/uploadfile", self.base_url))
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
            .post(format!("{}/getfilelink", self.base_url))
            .form(&[
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

    /// Download a file from pCloud, streaming chunks through a channel.
    /// Each chunk is sent as it arrives from the CDN — no full-file buffering.
    /// Supports Range headers via the optional `range_start`/`range_end` parameters.
    pub async fn download_stream(
        &self,
        remote_path: &str,
        range_start: Option<u64>,
        range_end: Option<u64>,
        tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, anyhow::Error>>,
    ) -> anyhow::Result<()> {
        // Get the file link (same auth as regular download)
        let resp = self
            .client
            .post(format!("{}/getfilelink", self.base_url))
            .form(&[
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

        // Build the request, optionally with Range header
        let mut req = self.client.get(&download_url);
        if let Some(start) = range_start {
            let end = range_end.map_or("".to_string(), |e| e.to_string());
            req = req.header("Range", format!("bytes={}-{}", start, end));
        }

        let response = req.send().await?;
        let status = response.status();
        if !status.is_success() && status.as_u16() != 206 {
            anyhow::bail!("pCloud download failed with status {}", status);
        }

        // Stream chunks to the channel as they arrive
        let mut stream = response.bytes_stream();
        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    if tx.send(Ok(bytes)).await.is_err() {
                        break; // Receiver dropped (client disconnected)
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(anyhow::anyhow!("Download stream error: {}", e))).await;
                    anyhow::bail!("Download stream error: {}", e);
                }
            }
        }
        Ok(())
    }

    /// Copy a file server-side using pCloud's copyfile API
    pub async fn copy_file(&self, source_path: &str, dest_parent: &str, new_name: &str) -> anyhow::Result<()> {
        let resp = self
            .client
            .post(format!("{}/copyfile", self.base_url))
            .form(&[
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
            .post(format!("{}/deletefile", self.base_url))
            .form(&[
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
            .post(format!("{}/listfolder", self.base_url))
            .form(&[
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
