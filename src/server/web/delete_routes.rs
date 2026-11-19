// =====================================================================
//  Mutating routes (gated on `server.enable_web_write`)
// =====================================================================

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;

use super::{json_error, json_response, WebState};

/// Guard shared by every mutating handler: refuse when writes are disabled.
pub(super) fn write_guard(state: &WebState) -> Option<Response> {
    if state.write_enabled {
        None
    } else {
        Some(json_error(
            StatusCode::FORBIDDEN,
            "web writes are disabled (set server.enable_web_write = true)",
        ))
    }
}

fn bad_request(msg: &str) -> Response {
    json_error(StatusCode::BAD_REQUEST, msg)
}

pub(super) fn server_error(e: impl std::fmt::Display) -> Response {
    json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

#[derive(Deserialize)]
pub(super) struct DeleteObjectBody {
    pub(super) bucket: String,
    pub(super) key: String,
}

/// POST /api/delete-object — remove a single object (a symlink removes only
/// the link row, never the target).
pub(super) async fn delete_object_handler(
    State(state): State<WebState>,
    super::WebJson(body): super::WebJson<DeleteObjectBody>,
) -> Response {
    if let Some(resp) = write_guard(&state) {
        return resp;
    }
    if body.bucket.is_empty() || body.key.is_empty() {
        return bad_request("missing 'bucket' or 'key'");
    }
    match state.engine.delete_object(&body.bucket, &body.key).await {
        Ok(()) => json_response(json!({
            "ok": true,
            "bucket": body.bucket,
            "key": body.key,
        })),
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize)]
pub(super) struct DeletePrefixBody {
    pub(super) bucket: String,
    pub(super) prefix: String,
}

/// POST /api/delete-prefix — remove every object whose key starts with
/// `prefix` (i.e. delete a folder). An empty prefix is always rejected.
pub(super) async fn delete_prefix_handler(
    State(state): State<WebState>,
    super::WebJson(body): super::WebJson<DeletePrefixBody>,
) -> Response {
    if let Some(resp) = write_guard(&state) {
        return resp;
    }
    if body.bucket.is_empty() {
        return bad_request("missing 'bucket'");
    }
    if body.prefix.is_empty() {
        return bad_request("refusing to delete the bucket root; use /api/delete-bucket");
    }
    // Guard rail: cap a folder delete so one click cannot silently remove a
    // very large subtree. The count is checked before anything is deleted.
    const MAX_FOLDER_DELETE: usize = 100_000;
    let (objects, truncated) = match state
        .engine
        .list_objects_symlink_aware(
            &body.bucket,
            Some(&body.prefix),
            None,
            MAX_FOLDER_DELETE as i64 + 1,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    if truncated || objects.len() > MAX_FOLDER_DELETE {
        return json_error(
            StatusCode::BAD_REQUEST,
            &format!(
                "folder has more than {} objects; delete it in smaller batches",
                MAX_FOLDER_DELETE
            ),
        );
    }
    let mut deleted = 0usize;
    let mut errors: Vec<String> = Vec::new();
    for obj in &objects {
        match state.engine.delete_object(&body.bucket, &obj.key).await {
            Ok(()) => deleted += 1,
            Err(e) => errors.push(format!("{}: {}", obj.key, e)),
        }
    }
    json_response(json!({
        "ok": errors.is_empty(),
        "bucket": body.bucket,
        "prefix": body.prefix,
        "deleted": deleted,
        "errors": errors,
    }))
}

#[derive(Deserialize)]
pub(super) struct DeleteBucketBody {
    pub(super) name: String,
    /// Must equal `name` — a typed confirmation so a stray click cannot fire.
    pub(super) confirm: String,
}

/// POST /api/delete-bucket — drop a bucket and every object in it.
pub(super) async fn delete_bucket_handler(
    State(state): State<WebState>,
    super::WebJson(body): super::WebJson<DeleteBucketBody>,
) -> Response {
    if let Some(resp) = write_guard(&state) {
        return resp;
    }
    if body.name.is_empty() {
        return bad_request("missing 'name'");
    }
    if body.confirm != body.name {
        return bad_request("'confirm' must match the bucket name exactly");
    }
    match state.engine.delete_bucket(&body.name).await {
        Ok(()) => json_response(json!({ "ok": true, "bucket": body.name })),
        Err(e) => server_error(e),
    }
}
