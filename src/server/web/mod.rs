//! Read-only web UI — a browser-friendly file navigator over the storage pool.
//!
//! Deliberately exposes **only** GET endpoints (list buckets, list objects,
//! download). There is no write/delete/upload path here, so the page is safe to
//! expose to a wider audience (e.g. over Tailscale) without risking data loss.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::json;

use crate::storage::engine::{ObjectInfo, StorageEngine};

/// Embedded single-file UI (vanilla HTML+JS, no build step).
const INDEX_HTML: &str = include_str!("index.html");

#[derive(Clone)]
pub struct WebState {
    pub engine: Arc<StorageEngine>,
}

pub fn build_router(engine: Arc<StorageEngine>) -> Router {
    let state = WebState { engine };
    Router::new()
        .route("/", get(index))
        .route("/api/buckets", get(list_buckets))
        .route("/api/list", get(list_objects))
        .route("/api/download", get(download))
        .with_state(state)
}

async fn index() -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(axum::body::Body::from(INDEX_HTML))
        .unwrap()
}

fn json_response(value: serde_json::Value) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(axum::body::Body::from(value.to_string()))
        .unwrap()
}

fn json_error(status: StatusCode, message: &str) -> Response {
    let mut resp = json_response(json!({ "error": message })).into_response();
    *resp.status_mut() = status;
    resp
}

async fn list_buckets(State(state): State<WebState>) -> Response {
    match state.engine.list_all_buckets().await {
        Ok(buckets) => {
            let arr: Vec<serde_json::Value> = buckets
                .into_iter()
                .map(|b| json!({ "name": b.name, "created_at": b.created_at }))
                .collect();
            json_response(json!({ "buckets": arr }))
        }
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct ListQuery {
    bucket: Option<String>,
    prefix: Option<String>,
}

async fn list_objects(
    State(state): State<WebState>,
    Query(q): Query<ListQuery>,
) -> Response {
    let bucket = match q.bucket {
        Some(b) if !b.is_empty() => b,
        _ => return json_error(StatusCode::BAD_REQUEST, "missing 'bucket' parameter"),
    };
    let prefix = q.prefix.filter(|p| !p.is_empty());

    // A large-but-bounded page: enough to group a folder's direct children.
    const MAX_KEYS: i64 = 100_000;
    let (objects, truncated) = match state
        .engine
        .list_objects(&bucket, prefix.as_deref(), None, MAX_KEYS)
        .await
    {
        Ok(v) => v,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let (prefixes, files) = crate::server::group_objects_by_prefix(&objects, prefix.as_deref());

    let prefixes_json: Vec<String> = prefixes;
    let files_json: Vec<serde_json::Value> = files
        .into_iter()
        .map(|o: &ObjectInfo| {
            json!({
                "key": o.key,
                "size": o.size,
                "last_modified": o.last_modified,
                "content_type": o.content_type,
            })
        })
        .collect();

    json_response(json!({
        "bucket": bucket,
        "prefix": prefix.unwrap_or_default(),
        "prefixes": prefixes_json,
        "files": files_json,
        "truncated": truncated,
    }))
}

#[derive(Deserialize)]
struct DownloadQuery {
    bucket: String,
    key: String,
}

async fn download(
    State(state): State<WebState>,
    Query(q): Query<DownloadQuery>,
    headers: HeaderMap,
) -> Response {
    if q.bucket.is_empty() || q.key.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "missing 'bucket' or 'key' parameter");
    }

    let obj_info = match state.engine.head_object(&q.bucket, &q.key).await {
        Ok(info) => info,
        Err(e) => return json_error(StatusCode::NOT_FOUND, &e.to_string()),
    };

    let total_len = obj_info.size as usize;
    let content_type = obj_info
        .content_type
        .unwrap_or_else(|| "application/octet-stream".to_string());

    // Optional Range support (lets browsers seek within a video).
    let range_val = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let parsed_range = range_val.and_then(|rv| crate::server::parse_range(rv, total_len));

    let content_length = match parsed_range {
        Some((start, end)) => end.saturating_sub(start),
        None => total_len,
    };

    use tokio::sync::mpsc;
    let (tx, rx) = mpsc::channel::<Result<Bytes, anyhow::Error>>(16);
    let engine = state.engine.clone();
    let b = q.bucket.clone();
    let k = q.key.clone();

    tokio::task::spawn(async move {
        if let Err(e) = engine.get_object_stream(&b, &k, parsed_range, tx).await {
            tracing::error!("web stream error for {}/{}: {}", b, k, e);
        }
    });

    use tokio_stream::wrappers::ReceiverStream;
    let stream = Box::pin(ReceiverStream::new(rx));

    let filename = q
        .key
        .rsplit('/')
        .next()
        .unwrap_or(&q.key)
        .to_string();
    let disposition = format!("attachment; filename=\"{}\"", filename.replace('"', "_"));

    let mut response = Response::builder()
        .header(header::CONTENT_TYPE, &content_type)
        .header(header::CONTENT_LENGTH, content_length.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_DISPOSITION, disposition);

    if let Some((start, end)) = parsed_range {
        response = response
            .header(
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{}", start, end.saturating_sub(1), total_len),
            )
            .status(StatusCode::PARTIAL_CONTENT);
    }

    response.body(axum::body::Body::from_stream(stream)).unwrap()
}
