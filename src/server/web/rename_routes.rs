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
    super::WebJson(body): super::WebJson<RenameObjectBody>,
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


/// POST /api/move-object
pub(super) async fn move_object_handler(
    State(state): State<WebState>,
    super::WebJson(body): super::WebJson<MoveObjectBody>,
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
