//! Atomic prefix re-key: rename/move a whole folder by rewriting metadata only.
//!
//! A rename normally means copying every object to the new key and deleting the
//! original — 2 backend operations per file. Because MultiFS keeps the blob
//! (account + remote_path) in metadata and the key is just an index, a folder
//! rename can instead rewrite the keys in a single transaction: no bytes move,
//! no pCloud traffic, and it is instant regardless of folder size.

use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;

use super::delete_routes::{server_error, write_guard};
use super::{json_error, json_response, WebState};

#[derive(Deserialize)]
pub(super) struct MovePrefixBody {
    pub(super) bucket: String,
    /// Source prefix; a trailing slash is optional.
    pub(super) prefix: String,
    /// Destination prefix; a trailing slash is optional.
    pub(super) dst_prefix: String,
}

/// Shared implementation for rename and move — both are a prefix rewrite.
async fn move_prefix(
    state: &WebState,
    bucket: &str,
    src_prefix: &str,
    dst_prefix: &str,
) -> Response {
    let started = Instant::now();
    let src = match normalize(src_prefix) {
        Some(p) => p,
        None => return json_error(StatusCode::BAD_REQUEST, "refusing to move the bucket root"),
    };
    let dst = match normalize(dst_prefix) {
        Some(p) => p,
        None => return json_error(StatusCode::BAD_REQUEST, "refusing to move onto the bucket root"),
    };
    if src == dst {
        return json_error(StatusCode::BAD_REQUEST, "source and destination are identical");
    }
    if dst.starts_with(&src) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "cannot move a folder inside itself",
        );
    }

    match state.engine.rekey_prefix(bucket, &src, &dst) {
        Ok(stats) => json_response(json!({
            "ok": true,
            "bucket": bucket,
            "from": src,
            "to": dst,
            "files": stats.files,
            "versions": stats.versions,
            "symlinks": stats.symlinks,
            "folder_meta": stats.folder_meta,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        })),
        Err(e) if e.to_string().contains("no objects under") => {
            json_error(StatusCode::NOT_FOUND, &e.to_string())
        }
        Err(e) if e.to_string().contains("already has") => {
            json_error(StatusCode::CONFLICT, &e.to_string())
        }
        Err(e) => server_error(e),
    }
}

/// Normalise a user-supplied folder prefix to the canonical `dir/sub/` form.
/// Returns `None` for the bucket root (empty), which callers must reject.
fn normalize(prefix: &str) -> Option<String> {
    let t = prefix.trim().trim_matches('/');
    if t.is_empty() {
        None
    } else {
        Some(format!("{}/", t))
    }
}

/// POST /api/move-prefix — rename or move a folder (alias of rename-prefix
/// with an explicit destination). Metadata-only, so it is instant.
pub(super) async fn move_prefix_handler(
    State(state): State<WebState>,
    super::WebJson(body): super::WebJson<MovePrefixBody>,
) -> Response {
    if let Some(resp) = write_guard(&state) {
        return resp;
    }
    if body.bucket.is_empty() || body.prefix.is_empty() || body.dst_prefix.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "missing 'bucket', 'prefix' or 'dst_prefix'",
        );
    }
    move_prefix(&state, &body.bucket, &body.prefix, &body.dst_prefix).await
}

#[derive(Deserialize)]
pub(super) struct RenamePrefixV2Body {
    pub(super) bucket: String,
    pub(super) prefix: String,
    /// New leaf name (no slashes) — the folder stays in the same parent.
    pub(super) new_name: String,
}

/// POST /api/rename-prefix — rename a folder in place via metadata re-key.
pub(super) async fn rename_prefix_handler(
    State(state): State<WebState>,
    super::WebJson(body): super::WebJson<RenamePrefixV2Body>,
) -> Response {
    if let Some(resp) = write_guard(&state) {
        return resp;
    }
    if body.bucket.is_empty() || body.prefix.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "missing 'bucket' or 'prefix'");
    }
    let name = body.new_name.trim();
    if name.is_empty() || name.contains('/') {
        return json_error(
            StatusCode::BAD_REQUEST,
            "new_name must be a plain name with no slashes",
        );
    }
    let src = match normalize(&body.prefix) {
        Some(p) => p,
        None => return json_error(StatusCode::BAD_REQUEST, "refusing to rename the bucket root"),
    };
    let trimmed = src.trim_end_matches('/');
    let parent = match trimmed.rfind('/') {
        Some(i) => &trimmed[..=i],
        None => "",
    };
    let dst = format!("{}{}/", parent, name);
    if dst == src {
        return json_response(json!({ "ok": true, "noop": true, "bucket": body.bucket }));
    }
    move_prefix(&state, &body.bucket, &src, &dst).await
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn normalize_strips_and_adds_slash() {
        assert_eq!(normalize("a/b"), Some("a/b/".to_string()));
        assert_eq!(normalize("/a/b/"), Some("a/b/".to_string()));
        assert_eq!(normalize("  a  "), Some("a/".to_string()));
    }

    #[test]
    fn normalize_rejects_root() {
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("/"), None);
        assert_eq!(normalize("   "), None);
    }
}
