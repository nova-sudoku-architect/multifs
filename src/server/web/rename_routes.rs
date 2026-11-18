//! Rename / move operations for the web UI (gated on `server.enable_web_write`).
//!
//! Rename and move are the same primitive: copy to the destination key, then
//! delete the source. Both are POST-only and refuse when writes are disabled.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;

use super::delete_routes::{server_error, write_guard};
use super::{json_error, json_response, WebState};

#[derive(Deserialize)]
pub(super) struct RenameObjectBody {
    pub(super) bucket: String,
    pub(super) key: String,
    /// New key, full path (the UI sends `<dir>/<new name>`).
    pub(super) new_key: String,
}

/// POST /api/rename-object — rename content at `key` to `new_key` in place.
async fn rename_object(
    state: &WebState,
    bucket: &str,
    key: &str,
    new_key: &str,
) -> Response {
    if key == new_key {
        return json_error(StatusCode::BAD_REQUEST, "source and destination are identical");
    }
    if let Err(e) = state.engine.copy_object(bucket, key, bucket, new_key).await {
        return server_error(e);
    }
    if let Err(e) = state.engine.delete_object(bucket, key).await {
        // The copy landed; report the partial state rather than silently losing it.
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("copied to new key but failed to remove the original: {}", e),
        );
    }
    json_response(json!({ "ok": true, "bucket": bucket, "from": key, "to": new_key }))
}

/// POST /api/rename-object
pub(super) async fn rename_object_handler(
    State(state): State<WebState>,
    axum::Json(body): axum::Json<RenameObjectBody>,
) -> Response {
    if let Some(resp) = write_guard(&state) {
        return resp;
    }
    if body.bucket.is_empty() || body.key.is_empty() || body.new_key.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "missing 'bucket', 'key' or 'new_key'");
    }
    rename_object(&state, &body.bucket, &body.key, &body.new_key).await
}

#[derive(Deserialize)]
pub(super) struct MoveObjectBody {
    pub(super) bucket: String,
    pub(super) key: String,
    /// Destination bucket (may equal the source bucket).
    pub(super) dst_bucket: String,
    /// Destination key, full path.
    pub(super) dst_key: String,
}


#[derive(Deserialize)]
pub(super) struct RenamePrefixBody {
    pub(super) bucket: String,
    /// Existing folder prefix, without a trailing slash.
    pub(super) prefix: String,
    /// New leaf name (no slashes).
    pub(super) new_name: String,
}

/// POST /api/rename-prefix — rename a folder by copying every object under the
/// old prefix to the new one, then deleting the originals.
async fn rename_prefix(
    state: &WebState,
    bucket: &str,
    prefix: &str,
    new_name: &str,
) -> Response {
    const MAX_FOLDER_MOVE: usize = 10_000;
    let trimmed = prefix.trim_end_matches('/');
    if trimmed.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "refusing to rename the bucket root");
    }
    if new_name.is_empty() || new_name.contains('/') {
        return json_error(StatusCode::BAD_REQUEST, "new_name must be a plain name with no slashes");
    }
    let parent = match trimmed.rfind('/') {
        Some(i) => &trimmed[..=i],
        None => "",
    };
    let dst_prefix = format!("{}{}/", parent, new_name);
    if dst_prefix == format!("{}/", trimmed) {
        return json_error(StatusCode::BAD_REQUEST, "source and destination are identical");
    }
    let src_prefix = format!("{}/", trimmed);

    let (objects, truncated) = match state
        .engine
        .list_objects_symlink_aware(bucket, Some(&src_prefix), None, MAX_FOLDER_MOVE as i64 + 1)
        .await
    {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    if truncated || objects.len() > MAX_FOLDER_MOVE {
        return json_error(
            StatusCode::BAD_REQUEST,
            &format!("folder has more than {} objects; move it in smaller batches", MAX_FOLDER_MOVE),
        );
    }
    if objects.is_empty() {
        return json_error(StatusCode::NOT_FOUND, "folder is empty or does not exist");
    }

    let mut moved = 0usize;
    let mut errors: Vec<String> = Vec::new();
    for obj in &objects {
        let rest = obj.key.strip_prefix(&src_prefix).unwrap_or(&obj.key);
        let dst_key = format!("{}{}", dst_prefix, rest);
        if let Err(e) = state.engine.copy_object(bucket, &obj.key, bucket, &dst_key).await {
            errors.push(format!("copy {}: {}", obj.key, e));
            continue;
        }
        if let Err(e) = state.engine.delete_object(bucket, &obj.key).await {
            errors.push(format!("delete {}: {}", obj.key, e));
            continue;
        }
        moved += 1;
    }
    json_response(json!({
        "ok": errors.is_empty(),
        "bucket": bucket,
        "from": src_prefix,
        "to": dst_prefix,
        "moved": moved,
        "errors": errors,
    }))
}

/// POST /api/rename-prefix
pub(super) async fn rename_prefix_handler(
    State(state): State<WebState>,
    axum::Json(body): axum::Json<RenamePrefixBody>,
) -> Response {
    if let Some(resp) = write_guard(&state) {
        return resp;
    }
    if body.bucket.is_empty() || body.prefix.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "missing 'bucket' or 'prefix'");
    }
    rename_prefix(&state, &body.bucket, &body.prefix, &body.new_name).await
}

/// POST /api/move-object
pub(super) async fn move_object_handler(
    State(state): State<WebState>,
    axum::Json(body): axum::Json<MoveObjectBody>,
) -> Response {
    if let Some(resp) = write_guard(&state) {
        return resp;
    }
    if body.bucket.is_empty() || body.key.is_empty() || body.dst_bucket.is_empty() || body.dst_key.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "missing 'bucket', 'key', 'dst_bucket' or 'dst_key'",
        );
    }
    if body.bucket == body.dst_bucket && body.key == body.dst_key {
        return json_error(StatusCode::BAD_REQUEST, "source and destination are identical");
    }
    if let Err(e) = state
        .engine
        .copy_object(&body.bucket, &body.key, &body.dst_bucket, &body.dst_key)
        .await
    {
        return server_error(e);
    }
    if let Err(e) = state.engine.delete_object(&body.bucket, &body.key).await {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("copied to destination but failed to remove the original: {}", e),
        );
    }
    json_response(json!({
        "ok": true,
        "from": { "bucket": body.bucket, "key": body.key },
        "to": { "bucket": body.dst_bucket, "key": body.dst_key },
    }))
}
