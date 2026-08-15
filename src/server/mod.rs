pub mod s3;
pub mod nfs;

#[cfg(test)]
mod handler_tests;

use crate::config::Config;

/// Parse HTTP Range header like "bytes=0-1023" or "bytes=100-"
/// Returns Some((start, end)) where end is exclusive and <= total_len
pub fn parse_range(range: &str, total_len: usize) -> Option<(usize, usize)> {
    let range = range.strip_prefix("bytes=")?;
    if let Some(dash_pos) = range.find('-') {
        let start_str = &range[..dash_pos];
        let end_str = &range[dash_pos + 1..];

        let start: usize = if start_str.is_empty() {
            // Suffix range: bytes=-500 → last 500 bytes
            let suffix: usize = end_str.parse().ok()?;
            if suffix >= total_len {
                return Some((0, total_len));
            }
            return Some((total_len - suffix, total_len));
        } else {
            start_str.parse().ok()?
        };

        let end: usize = if end_str.is_empty() {
            total_len
        } else {
            // End is inclusive in HTTP range, convert to exclusive
            let inclusive_end: usize = end_str.parse().ok()?;
            inclusive_end + 1
        };

        Some((start, end))
    } else {
        None
    }
}

/// Group objects by their first path segment, separating folder-like prefixes
/// from leaf files. Returns (prefixes, files) where prefixes are unique directory
/// names (with trailing /) and files are objects at the current level.
///
/// Used by both S3 (CommonPrefixes) and WebDAV (folder listing).
/// Guess content type from file extension using mime_guess.
pub fn content_type_from_path(key: &str) -> String {
    mime_guess::from_path(key)
        .first_or_octet_stream()
        .to_string()
}

/// Resolve content type for an upload, combining client-provided Content-Type
/// header with extension-based detection. The S3 handler uses this to handle
/// curl's default `application/x-www-form-urlencoded` that it sends with --data-binary.
pub fn resolve_content_type(key: &str, client_ct: Option<&str>) -> Option<String> {
    let ext_ct = mime_guess::from_path(key).first().map(|m| m.to_string());

    match ext_ct {
        Some(_) => {
            // Use extension detection unless client overrides with something meaningful
            match client_ct {
                Some("application/x-www-form-urlencoded") => ext_ct,
                Some(other) => {
                    if other == "application/octet-stream" {
                        ext_ct
                    } else {
                        Some(other.to_string())
                    }
                }
                None => ext_ct,
            }
        }
        None => client_ct.map(|s| s.to_string()),
    }
}

pub fn group_objects_by_prefix<'a>(
    objects: &'a [crate::storage::engine::ObjectInfo],
    prefix: Option<&str>,
) -> (Vec<String>, Vec<&'a crate::storage::engine::ObjectInfo>) {
    let mut prefixes: Vec<String> = Vec::new();
    let mut files: Vec<&crate::storage::engine::ObjectInfo> = Vec::new();

    // Normalize the strip prefix: ensure it ends with /
    let strip_prefix = prefix.map(|p| {
        if p.ends_with('/') { p.to_string() } else { format!("{}/", p) }
    });

    for obj in objects {
        let relative = if let Some(ref sp) = strip_prefix {
            obj.key.strip_prefix(sp.as_str()).unwrap_or(&obj.key)
        } else {
            &obj.key
        };

        if let Some(slash_pos) = relative.find('/') {
            // This object belongs to a subdirectory
            let folder_name = &relative[..slash_pos];
            // Build full prefix including the original prefix path
            let dir_name = if let Some(ref sp) = strip_prefix {
                format!("{}{}/", sp, folder_name)
            } else {
                format!("{}/", folder_name)
            };
            if !prefixes.contains(&dir_name) {
                prefixes.push(dir_name);
            }
        } else if !relative.is_empty() {
            files.push(obj);
        }
    }

    (prefixes, files)
}

/// Run all enabled protocol servers
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)?;
    let engine = crate::storage::engine::StorageEngine::new(&cfg, meta)?;

    tracing::info!(
        "Starting MultiFS on {} (S3:{})",
        cfg.server.bind,
        cfg.server.s3_port,
    );

    let engine = std::sync::Arc::new(engine);

    // Background vacuum: periodically reclaim superseded + abandoned versions.
    {
        let vacuum_engine = engine.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(600));
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                match vacuum_engine.vacuum(false).await {
                    Ok((pending, orphans, multipart)) => {
                        if pending > 0 || orphans > 0 || multipart > 0 {
                            tracing::info!(
                                "vacuum: reclaimed {} pending, {} superseded versions, {} abandoned multipart uploads",
                                pending,
                                orphans,
                                multipart
                            );
                        }
                    }
                    Err(e) => tracing::warn!("vacuum failed: {}", e),
                }
            }
        });
    }

    // Build S3 app (the only protocol server)
    if cfg.server.enable_s3 {
        let s3_app = s3::build_router(engine.clone());
        let s3_addr = format!("{}:{}", cfg.server.bind, cfg.server.s3_port);
        let listener = tokio::net::TcpListener::bind(&s3_addr).await?;
        let handle = tokio::spawn(async move {
            axum::serve(listener, s3_app).await
        });
        tracing::info!("S3 API listening on {}", s3_addr);
        handle.await?;
    }

    Ok(())
}
