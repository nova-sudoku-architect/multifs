use std::sync::Arc;

use tokio::net::TcpListener;

use crate::storage::engine::StorageEngine;

/// NFS v3 server stub
///
/// NFS is the most complex protocol to implement. This is a scaffold
/// that will be fleshed out with actual ONC RPC / NFS v3 protocol handling.
///
/// For a production implementation, we would use the `nfs-rs` crate or
/// implement ONC RPC portmap + mount + NFS v3 procedures manually.

/// Start the NFS server
///
/// NFS requires:
/// - Portmap daemon (port 111) — maps RPC program numbers to ports
/// - Mount daemon (portmap registered) — handles mount/unmount
/// - NFS daemon (portmap registered) — handles file operations
pub async fn run(_engine: Arc<StorageEngine>, addr: &str) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let _listener = TcpListener::bind(addr).await?;
    tracing::info!("NFS server listening on {}", addr);

    // TODO: Implement NFS v3 protocol
    // This requires:
    // 1. ONC RPC (RFC 5531) message parsing
    // 2. Portmap (RPC program 100000) — register services
    // 3. Mount protocol (RPC program 100005) — handle mount requests
    // 4. NFS v3 (RPC program 100003) — handle file operations:
    //    - NULL, GETATTR, SETATTR, LOOKUP, ACCESS, READLINK, READ
    //    - WRITE, CREATE, MKDIR, SYMLINK, MKNOD, REMOVE, RMDIR
    //    - RENAME, LINK, READDIR, READDIRPLUS, FSSTAT, FSINFO
    //    - PATHCONF, COMMIT
    //
    // The mapping from NFS file handles to pCloud objects would be:
    // - Root file handle → list of buckets
    // - Bucket file handle → bucket metadata
    // - Object file handle → object metadata + content

    let handle = tokio::spawn(async move {
        tracing::warn!("NFS server is a stub — not fully implemented yet");
        tracing::info!(
            "To mount this storage without NFS, use: multifs object cp <local> <bucket>/<key>"
        );
        // Keep alive
        tokio::signal::ctrl_c().await.ok();
    });

    Ok(handle)
}
