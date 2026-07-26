use std::sync::Arc;

use anyhow;
use sha2::{Digest, Sha256};
use axum::{
    extract::DefaultBodyLimit,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get, Router,
};
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
        .route("/{bucket}/{*key}", get(get_object).head(head_object).put(put_object).post(put_object).delete(delete_object))
        .layer(CorsLayer::permissive())
        .layer(DefaultBodyLimit::max(2_147_483_648))
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

/// HEAD /{bucket} — Check if bucket exists (with x-amz-bucket-region for rclone)
async fn head_bucket(
    State(state): State<S3State>,
    Path(bucket): Path<String>,
) -> Response {
    match state.engine.bucket_exists(&bucket).await {
        Ok(true) => Response::builder()
            .status(StatusCode::OK)
            .header("x-amz-bucket-region", "us-east-1")
            .header("x-amz-request-id", "multifs")
            .header("Server", "MultiFS")
            .body(Body::empty())
            .unwrap(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
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
    location: Option<String>,
    versioning: Option<String>,
    uploads: Option<String>,
    upload_id: Option<String>,
    #[serde(rename = "partNumber")]
    part_number: Option<i32>,
    #[serde(rename = "max-parts")]
    max_parts: Option<i32>,
    #[serde(rename = "part-number-marker")]
    part_number_marker: Option<i32>,
    encoding_type: Option<String>, 
}

async fn list_objects(
    State(state): State<S3State>,
    Path(bucket): Path<String>,
    Query(params): Query<ListObjectsParams>,
) -> Response {
    // POST /{bucket}/{key}?uploads — Initiate multipart upload
    if params.uploads.is_some() {
        let upload_id = format!("multipart-{}", chrono::Utc::now().format("%Y%m%d%H%M%S%f"));
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>{}</Bucket>
  <Key>unknown</Key>
  <UploadId>{}</UploadId>
</InitiateMultipartUploadResult>"#,
            bucket, upload_id
        );
        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/xml")
            .body(Body::from(xml))
            .unwrap();
    }

    // GET /{bucket}?location — return bucket location (rclone compat)
    if params.location.is_some() {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<LocationConstraint xmlns="http://s3.amazonaws.com/doc/2006-03-01/">{}</LocationConstraint>"#,
            "us-east-1"
        );
        return Response::builder()
            .header("Content-Type", "application/xml")
            .body(Body::from(xml))
            .unwrap();
    }

    // GET /{bucket}?versioning — return versioning status (rclone compat)
    if params.versioning.is_some() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Status>Suspended</Status>
</VersioningConfiguration>"#;
        return Response::builder()
            .header("Content-Type", "application/xml")
            .body(Body::from(xml))
            .unwrap();
    }

    let prefix = params.prefix.as_deref();
    let delimiter = params.delimiter.as_deref();
    let max_keys = params.max_keys.unwrap_or(100).min(1000);

    match state.engine.list_objects(&bucket, prefix, max_keys).await {
        Ok(objects) => {
            // If delimiter=/ is requested, group objects into CommonPrefixes
            let has_delimiter = delimiter == Some("/");

            if has_delimiter {
                let (common_prefixes, file_objs) = crate::server::group_objects_by_prefix(&objects, prefix);
                let contents: Vec<(String, i64, String, String)> = file_objs
                    .iter()
                    .map(|o| (o.key.clone(), o.size, o.etag.clone(), o.last_modified.clone()))
                    .collect();

                // Build XML response with CommonPrefixes
                let contents_xml: String = contents
                    .iter()
                    .map(|(key, size, etag, modified)| {
                        format!(
                            "<Contents>\n    <Key>{}</Key>\n    <LastModified>{}</LastModified>\n    <ETag>&quot;{}&quot;</ETag>\n    <Size>{}</Size>\n    <StorageClass>STANDARD</StorageClass>\n</Contents>",
                            key, modified, etag, size
                        )
                    })
                    .collect();

                let prefixes_xml: String = common_prefixes
                    .iter()
                    .map(|p| format!("<CommonPrefixes>\n    <Prefix>{}</Prefix>\n</CommonPrefixes>", p))
                    .collect();

                let xml = format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>{}</Name>
  <Prefix>{}</Prefix>
  <Delimiter>/</Delimiter>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>false</IsTruncated>
  {}{}
</ListBucketResult>"#,
                    bucket,
                    prefix.unwrap_or(""),
                    prefixes_xml,
                    contents_xml
                );

                Response::builder()
                    .header("Content-Type", "application/xml")
                    .body(Body::from(xml))
                    .unwrap()
            } else {
                // No delimiter: return flat list (original behavior)
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

/// PUT /{bucket}/{key} — Upload object (supports streaming and multipart)
async fn put_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Body,
) -> Response {
    // Parse query params manually
    let query_str = uri.query().unwrap_or("");
    let has_uploads = query_str.contains("uploads");
    let has_upload_id = query_str.contains("uploadId=") || query_str.contains("upload_id=");
    let has_part_number = query_str.contains("partNumber=") || query_str.contains("partNumber");

    // POST /{bucket}/{key}?uploads — Initiate multipart upload
    if has_uploads && !has_upload_id {
        let upload_id = format!("multipart-{}", chrono::Utc::now().format("%Y%m%d%H%M%S%f"));
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>{}</Bucket>
  <Key>{}</Key>
  <UploadId>{}</UploadId>
</InitiateMultipartUploadResult>"#,
            bucket, key, upload_id
        );
        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/xml")
            .body(Body::from(xml))
            .unwrap();
    }

    // POST /{bucket}/{key}?uploadId=... — Complete multipart upload (not for PUT part)
    if has_upload_id && !has_part_number {
        let etag = format!("\"multipart-{}\"", chrono::Utc::now().format("%Y%m%d%H%M%S"));
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Location>https://vmi3137694.tailb9bfd3.ts.net/s3/{}/{}</Location>
  <Bucket>{}</Bucket>
  <Key>{}</Key>
  <ETag>{}</ETag>
</CompleteMultipartUploadResult>"#,
            bucket, key, bucket, key, etag
        );
        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/xml")
            .body(Body::from(xml))
            .unwrap();
    }

    // PUT /{bucket}/{key}?partNumber=N&uploadId=... — Upload a part
    if has_part_number {
        use sha2::Digest;
        let mut hasher = Sha256::new();
        hasher.update(b"multipart-part");
        let etag_hex = hex::encode(hasher.finalize());
        return Response::builder()
            .header("ETag", format!("\"{}\"", &etag_hex[..16]))
            .body(Body::empty())
            .unwrap();
    }
    // Resolve content type from file extension + client header
    let client_ct = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let content_type = crate::server::resolve_content_type(&key, client_ct);

    // Check Content-Length to decide streaming vs buffered
    let content_length = headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    const CHUNK_SIZE_LIMIT: u64 = 32 * 1024 * 1024; // 32 MB — same as engine's chunk size

    let result = if let Some(len) = content_length {
        // If we know the full size and it's small, use buffered path
        if len <= CHUNK_SIZE_LIMIT {
            // Buffer the full body
            let full_body = match axum::body::to_bytes(body, len as usize).await {
                Ok(b) => b,
                Err(e) => {
                    let xml = s3_error_xml("InternalError", &format!("Failed to buffer body: {}", e), &format!("{}/{}", bucket, key));
                    return Response::builder().status(StatusCode::INTERNAL_SERVER_ERROR).header("Content-Type", "application/xml").body(Body::from(xml)).unwrap();
                }
            };
            state.engine.put_object_with_content_type(&bucket, &key, &full_body, content_type.as_deref()).await
        } else {
            // Large file (>32 MB): buffer the full body and use chunked storage
            let full_body = match axum::body::to_bytes(body, len as usize).await {
                Ok(b) => b,
                Err(e) => {
                    let xml = s3_error_xml("InternalError", &format!("Failed to buffer body: {}", e), &format!("{}/{}", bucket, key));
                    return Response::builder().status(StatusCode::INTERNAL_SERVER_ERROR).header("Content-Type", "application/xml").body(Body::from(xml)).unwrap();
                }
            };
            state.engine.put_object_with_content_type(&bucket, &key, &full_body, content_type.as_deref()).await
        }
    } else {
        // No Content-Length: buffer entirely (handles chunked transfer encoding) up to 2GB
        let full_body = match axum::body::to_bytes(body, 2_147_483_648).await {
            Ok(b) => b,
            Err(e) => {
                let xml = s3_error_xml("InternalError", &format!("Failed to buffer body: {}", e), &format!("{}/{}", bucket, key));
                return Response::builder().status(StatusCode::INTERNAL_SERVER_ERROR).header("Content-Type", "application/xml").body(Body::from(xml)).unwrap();
            }
        };
        state.engine.put_object_with_content_type(&bucket, &key, &full_body, content_type.as_deref()).await
    };

    match result {
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

/// GET /{bucket}/{key} — Download object with streaming Range support.
/// Uses channel-based streaming so video players (VLC) can seek without
/// the server buffering the entire file into memory.
async fn get_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    use tokio::sync::mpsc;
    use bytes::Bytes;
    use futures::stream::StreamExt;
    use std::pin::Pin;

    // Get object metadata first (size, content-type, etag)
    let obj_info = match state.engine.head_object(&bucket, &key).await {
        Ok(info) => info,
        Err(e) => {
            let xml = s3_error_xml("NoSuchKey", &e.to_string(), &format!("{}/{}", bucket, key));
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("Content-Type", "application/xml")
                .body(Body::from(xml))
                .unwrap();
        }
    };

    let total_len = obj_info.size as usize;
    let content_type = obj_info.content_type.unwrap_or_else(|| "application/octet-stream".to_string());

    // Parse optional Range header
    let range_val = headers.get(http::header::RANGE).and_then(|v| v.to_str().ok());
    let parsed_range = range_val.and_then(|rv| crate::server::parse_range(rv, total_len));

    // Set up streaming channel
    let content_length = match parsed_range {
        Some((start, end)) => end.saturating_sub(start),
        None => total_len,
    };

    let (tx, rx) = mpsc::channel::<Result<Bytes, anyhow::Error>>(16);
    let engine = state.engine.clone();
    let b = bucket.clone();
    let k = key.clone();

    tokio::task::spawn(async move {
        let range_for_stream = parsed_range;
        if let Err(e) = engine.get_object_stream(&b, &k, range_for_stream, tx).await {
            tracing::error!("S3 stream error for {}/{}: {}", b, k, e);
        }
    });

    use tokio_stream::wrappers::ReceiverStream;

    // Range slicing is handled inside the engine for chunked files.
    let stream: Pin<Box<dyn futures::Stream<Item = Result<Bytes, std::convert::Infallible>> + Send>> =
        Box::pin(ReceiverStream::new(rx).filter_map(|r| async move { r.ok() }).map(Ok));

    let mut response = Response::builder()
        .header("Content-Type", &content_type)
        .header("Content-Length", content_length.to_string())
        .header("Accept-Ranges", "bytes")
        .header("ETag", format!("\"{}\"", obj_info.etag));

    if let Some((start, end)) = parsed_range {
        response = response
            .header("Content-Range", format!("bytes {}-{}/{}", start, end.saturating_sub(1), total_len))
            .status(StatusCode::PARTIAL_CONTENT);
    }

    response.body(Body::from_stream(stream)).unwrap()
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
