use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get, Router,
};
use bytes::Bytes;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;

use crate::storage::engine::StorageEngine;

/// Shared application state
#[derive(Clone)]
pub struct S3State {
    pub engine: Arc<StorageEngine>,
}

/// Build the S3-compatible router
pub fn build_router(engine: Arc<StorageEngine>) -> Router {
    let state = S3State { engine };

    Router::new()
        // Service operations (MinIO compatibility)
        .route("/", get(list_buckets))
        // Bucket operations
        .route("/{bucket}", get(list_objects).head(head_bucket).put(create_bucket).delete(delete_bucket))
        // Object operations
        .route("/{bucket}/{*key}", get(get_object).head(head_object).put(put_object).delete(delete_object))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// ---- Error handling ----

#[derive(Serialize)]
struct S3Error {
    #[serde(rename = "Code")]
    code: String,
    #[serde(rename = "Message")]
    message: String,
    #[serde(rename = "Resource")]
    resource: String,
    #[serde(rename = "RequestId")]
    request_id: String,
}

fn s3_error_xml(code: &str, message: &str, resource: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>{}</Code>
  <Message>{}</Message>
  <Resource>{}</Resource>
  <RequestId>multifs</RequestId>
</Error>"#,
        code, message, resource
    )
}

fn s3_list_buckets_xml(buckets: &[(String, String)]) -> String {
    let buckets_xml: String = buckets
        .iter()
        .map(|(name, created)| {
            format!(
                "<Bucket><Name>{}</Name><CreationDate>{}</CreationDate></Bucket>",
                name, created
            )
        })
        .collect();

    let _now = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ListAllMyBucketsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Owner><ID>multifs</ID><DisplayName>multifs</DisplayName></Owner>
  <Buckets>{}</Buckets>
</ListAllMyBucketsResult>"#,
        buckets_xml
    )
}

fn s3_list_objects_xml(
    bucket: &str,
    prefix: Option<&str>,
    objects: &[(String, i64, String, String)],
    is_truncated: bool,
) -> String {
    let contents_xml: String = objects
        .iter()
        .map(|(key, size, etag, modified)| {
            format!(
                "<Contents>
    <Key>{}</Key>
    <LastModified>{}</LastModified>
    <ETag>&quot;{}&quot;</ETag>
    <Size>{}</Size>
    <StorageClass>STANDARD</StorageClass>
</Contents>",
                key, modified, etag, size
            )
        })
        .collect();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>{}</Name>
  <Prefix>{}</Prefix>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>{}</IsTruncated>
  {}
</ListBucketResult>"#,
        bucket,
        prefix.unwrap_or(""),
        if is_truncated { "true" } else { "false" },
        contents_xml
    )
}

// ---- Handlers ----

/// GET / — List all buckets (S3 ListBuckets)
async fn list_buckets(State(state): State<S3State>) -> Response {
    match state.engine.list_all_buckets().await {
        Ok(buckets) => {
            let bucket_list: Vec<(String, String)> = buckets
                .into_iter()
                .map(|b| (b.name, b.created_at))
                .collect();
            let xml = s3_list_buckets_xml(&bucket_list);
            Response::builder()
                .header("Content-Type", "application/xml")
                .body(Body::from(xml))
                .unwrap()
        }
        Err(e) => {
            let xml = s3_error_xml("InternalError", &e.to_string(), "/");
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/xml")
                .body(Body::from(xml))
                .unwrap()
        }
    }
}

/// HEAD /{bucket} — Check if bucket exists
async fn head_bucket(
    State(state): State<S3State>,
    Path(bucket): Path<String>,
) -> StatusCode {
    match state.engine.bucket_exists(&bucket).await {
        Ok(true) => StatusCode::OK,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// PUT /{bucket} — Create bucket
async fn create_bucket(
    State(state): State<S3State>,
    Path(bucket): Path<String>,
) -> StatusCode {
    match state.engine.create_bucket(&bucket).await {
        Ok(_) => StatusCode::OK,
        Err(e) => {
            tracing::error!("Failed to create bucket '{}': {}", bucket, e);
            StatusCode::CONFLICT
        }
    }
}

/// DELETE /{bucket} — Delete bucket
async fn delete_bucket(
    State(state): State<S3State>,
    Path(bucket): Path<String>,
) -> StatusCode {
    match state.engine.delete_bucket(&bucket).await {
        Ok(_) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// GET /{bucket} — List objects (ListObjectsV2)
#[derive(Deserialize)]
struct ListObjectsParams {
    prefix: Option<String>,
    #[serde(rename = "max-keys")]
    max_keys: Option<i64>,
    delimiter: Option<String>,
    #[serde(rename = "list-type")]
    list_type: Option<i64>,
    #[serde(rename = "continuation-token")]
    continuation_token: Option<String>,
    marker: Option<String>,
}

async fn list_objects(
    State(state): State<S3State>,
    Path(bucket): Path<String>,
    Query(params): Query<ListObjectsParams>,
) -> Response {
    let prefix = params.prefix.as_deref();
    let max_keys = params.max_keys.unwrap_or(100).min(1000);

    match state.engine.list_objects(&bucket, prefix, max_keys).await {
        Ok(objects) => {
            let obj_tuples: Vec<(String, i64, String, String)> = objects
                .into_iter()
                .map(|o| (o.key, o.size, o.etag, o.last_modified))
                .collect();
            let xml = s3_list_objects_xml(&bucket, prefix, &obj_tuples, false);
            Response::builder()
                .header("Content-Type", "application/xml")
                .body(Body::from(xml))
                .unwrap()
        }
        Err(e) => {
            let xml = s3_error_xml("NoSuchBucket", &e.to_string(), &bucket);
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("Content-Type", "application/xml")
                .body(Body::from(xml))
                .unwrap()
        }
    }
}

/// PUT /{bucket}/{key} — Upload object
async fn put_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    match state.engine.put_object(&bucket, &key, &body).await {
        Ok(info) => {
            let etag_header = format!("\"{}\"", info.etag);
            Response::builder()
                .header("ETag", &etag_header)
                .status(StatusCode::OK)
                .body(Body::empty())
                .unwrap()
        }
        Err(e) => {
            let xml = s3_error_xml("InternalError", &e.to_string(), &format!("{}/{}", bucket, key));
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/xml")
                .body(Body::from(xml))
                .unwrap()
        }
    }
}

/// GET /{bucket}/{key} — Download object
async fn get_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
) -> Response {
    match state.engine.get_object(&bucket, &key).await {
        Ok(data) => Response::builder()
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", data.len().to_string())
            .body(Body::from(data))
            .unwrap(),
        Err(e) => {
            let xml = s3_error_xml("NoSuchKey", &e.to_string(), &format!("{}/{}", bucket, key));
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("Content-Type", "application/xml")
                .body(Body::from(xml))
                .unwrap()
        }
    }
}

/// HEAD /{bucket}/{key} — Object metadata
async fn head_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
) -> Response {
    match state.engine.head_object(&bucket, &key).await {
        Ok(info) => Response::builder()
            .header("Content-Type", info.content_type.unwrap_or("application/octet-stream".to_string()))
            .header("Content-Length", info.size.to_string())
            .header("ETag", format!("\"{}\"", info.etag))
            .header("Last-Modified", &info.last_modified)
            .body(Body::empty())
            .unwrap(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// DELETE /{bucket}/{key} — Delete object
async fn delete_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
) -> StatusCode {
    match state.engine.delete_object(&bucket, &key).await {
        Ok(_) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::NO_CONTENT, // S3 idempotent delete
    }
}
