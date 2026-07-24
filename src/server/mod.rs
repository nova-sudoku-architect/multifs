pub mod s3;
pub mod webdav;
pub mod nfs;

use crate::config::Config;

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

    // Request body size limit: 2 GB for large file uploads
    let body_limit = tower_http::limit::RequestBodyLimitLayer::new(2_147_483_648);

    // Build S3 app
    let mut s3_handle = None;
    if cfg.server.enable_s3 {
        let s3_app = s3::build_router(engine.clone()).layer(body_limit.clone());
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
        let webdav_app = webdav::build_router(engine.clone()).layer(body_limit.clone());
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
