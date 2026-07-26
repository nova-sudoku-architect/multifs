pub mod s3;
pub mod webdav;
pub mod webdav_tests;
pub mod nfs;

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

/// Run all enabled protocol servers
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    let meta = crate::storage::metadata::MetadataDb::open(&cfg.storage.meta_db_path)?;
    let engine = crate::storage::engine::StorageEngine::new(&cfg, meta)?;

    tracing::info!(
        "Starting MultiFS on {} (S3:{}, WebDAV:{}, NFS:disabled)",
        cfg.server.bind,
        cfg.server.s3_port,
        cfg.server.webdav_port,
    );

    let engine = std::sync::Arc::new(engine);

    // Build S3 app
    let mut s3_handle = None;
    if cfg.server.enable_s3 {
        let s3_app = s3::build_router(engine.clone());
        let s3_addr = format!("{}:{}", cfg.server.bind, cfg.server.s3_port);
        let listener = tokio::net::TcpListener::bind(&s3_addr).await?;
        s3_handle = Some(tokio::spawn(async move {
            axum::serve(listener, s3_app).await
        }));
        tracing::info!("S3 API listening on {}", s3_addr);
    }

    // Build WebDAV app
    let mut webdav_handle = None;
    if cfg.server.enable_webdav {
        let webdav_app = webdav::build_router(engine.clone());
        let webdav_addr = format!("{}:{}", cfg.server.bind, cfg.server.webdav_port);
        let listener = tokio::net::TcpListener::bind(&webdav_addr).await?;
        webdav_handle = Some(tokio::spawn(async move {
            axum::serve(listener, webdav_app).await
        }));
        tracing::info!("WebDAV listening on {}", webdav_addr);
    }

    // Wait for any server to exit
    if let Some(h) = s3_handle { h.await?; }
    if let Some(h) = webdav_handle { h.await?; }

    Ok(())
}
