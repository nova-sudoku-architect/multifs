use std::pin::Pin;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use axum::extract::DefaultBodyLimit;
use bytes::Bytes;
use hyper::header;
use tower_http::cors::CorsLayer;

use crate::storage::engine::StorageEngine;

/// WebDAV server state
#[derive(Clone)]
pub struct WebDAVState {
    pub engine: Arc<StorageEngine>,
}

/// Build the WebDAV router
pub fn build_router(engine: Arc<StorageEngine>) -> Router {
    let state = WebDAVState { engine };

    Router::new()
        .route("/", any(webdav_root_handler))
        .route("/{*path}", any(webdav_handler))
        .layer(CorsLayer::permissive())
        .layer(DefaultBodyLimit::max(2_147_483_648))
        .with_state(state)
}


/// WebDAV root handler — handles PROPFIND / directly
async fn webdav_root_handler(
    State(state): State<WebDAVState>,
    method: Method,
    _body: Bytes,
) -> Response {
    tracing::debug!("WebDAV root {} /", method);
    let method_str = method.as_str();
    match method_str {
        "OPTIONS" => {
            Response::builder()
                .header("DAV", "1, 2")
                .header("Allow", "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, MKCOL, COPY, MOVE")
                .status(StatusCode::OK)
                .body(Body::empty())
                .unwrap()
        }
        "GET" | "HEAD" => handle_root_get(&state).await,
        "PROPFIND" => handle_propfind(&state, "").await,
        "PROPPATCH" => StatusCode::OK.into_response(),
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}


/// Universal WebDAV handler
async fn webdav_handler(
    State(state): State<WebDAVState>,
    method: Method,
    Path(path): Path<String>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    tracing::debug!("WebDAV {} /{}", method, path);

    // Match known HTTP methods + WebDAV extensions
    let method_str = method.as_str();
    match method_str {
        "OPTIONS" => {
            Response::builder()
                .header("DAV", "1, 2")
                .header(
                    "Allow",
                    "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, MKCOL, COPY, MOVE",
                )
                .status(StatusCode::OK)
                .body(Body::empty())
                .unwrap()
        }
        "PROPFIND" => handle_propfind(&state, &path).await,
        "PROPPATCH" => StatusCode::OK.into_response(),
        "MKCOL" => handle_mkcol(&state, &path).await,
        "GET" | "HEAD" => handle_get(&state, &path, method, Some(&headers)).await,
        "PUT" => handle_put(&state, &path, body).await,
        "DELETE" => handle_delete(&state, &path).await,
        "COPY" => handle_copy(&state, &path, "").await,
        "MOVE" => handle_move(&state, &path, "").await,
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

/// Detect whether a path refers to a directory.
/// Preserves the original path (not trimmed) so trailing-slash info is available.
fn is_directory_request(original_path: &str, bucket: &str, key: &str) -> bool {
    key.is_empty() || original_path.ends_with('/') || key.ends_with('/')
}

/// WebDAV root GET — returns an HTML index page for browsers
async fn handle_root_get(state: &WebDAVState) -> Response {
    match state.engine.list_all_buckets().await {
        Ok(buckets) => {
            let mut html = String::from(r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><title>MultiFS</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  body { font-family: -apple-system, sans-serif; max-width: 600px; margin: 40px auto; padding: 0 20px; }
  h1 { color: #333; }
  ul { list-style: none; padding: 0; }
  li { padding: 8px 12px; margin: 4px 0; background: #f5f5f5; border-radius: 6px; }
  a { color: #0066cc; text-decoration: none; }
  a:hover { text-decoration: underline; }
  .info { color: #666; font-size: 0.9em; margin-top: 20px; }
</style></head><body>
<h1>📦 MultiFS</h1>
<p>Your multi-cloud storage pool</p>
<h2>Buckets</h2>
<ul>"#);

            if buckets.is_empty() {
                html.push_str("<li><em>No buckets yet. Create one with:</em> <code>curl -X PUT https://vmi3137694.tailb9bfd3.ts.net/s3/my-bucket</code></li>");
            }

            for b in &buckets {
                html.push_str(&format!(
                    r#"<li><a href="{href}/">{name}</a> <span style="color:#999;font-size:0.85em">({created})</span></li>"#,
                    href = b.name,
                    name = b.name,
                    created = &b.created_at[..10]
                ));
            }

            html.push_str(r#"</ul>
<div class="info">
<p><strong>S3 API:</strong> <a href="https://vmi3137694.tailb9bfd3.ts.net/s3/">https://vmi3137694.tailb9bfd3.ts.net/s3/</a></p>
<p><strong>WebDAV:</strong> <a href="/">https://vmi3137694.tailb9bfd3.ts.net/multifs/</a></p>
<p><strong>Storage:</strong> 3 × pCloud accounts (~12 GB total)</p>
</div>
</body></html>"#);

            Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(html))
                .unwrap()
        }
        Err(e) => {
            tracing::error!("Root GET error: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// WebDAV PROPFIND — list buckets or objects with prefix
async fn handle_propfind(state: &WebDAVState, path: &str) -> Response {
    // Root path → list all buckets
    if path.is_empty() || path == "/" || path == "." {
        return match state.engine.list_all_buckets().await {
            Ok(buckets) => {
                let entries: Vec<(String, String)> = buckets
                    .iter()
                    .map(|b| (format!("/{}", b.name), b.created_at.clone()))
                    .collect();
                let xml = webdav_root_multistatus(&entries);
                Response::builder()
                    .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
                    .header("DAV", "1")
                    .body(Body::from(xml))
                    .unwrap()
            }
            Err(e) => {
                tracing::error!("PROPFIND root error: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        };
    }

    // Split path into bucket and prefix
    let parts: Vec<&str> = path.splitn(2, '/').collect();
    let bucket = parts[0];
    let prefix = if parts.len() > 1 { parts[1] } else { "" };

    // Check if bucket exists
    match state.engine.bucket_exists(bucket).await {
        Ok(false) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(e) => {
            tracing::error!("PROPFIND bucket check error: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Ok(true) => {}
    }

    // List objects in bucket (with prefix)
    let prefix_param = if prefix.is_empty() { None } else { Some(prefix) };
    match state.engine.list_objects(bucket, prefix_param, 1000).await {
        Ok(objects) => {
            let entries: Vec<(String, String, i64)> = objects
                .iter()
                .map(|o| {
                    (
                        format!("/{}/{}", bucket, o.key),
                        o.last_modified.clone(),
                        o.size,
                    )
                })
                .collect();
            let xml = webdav_bucket_multistatus(&format!("/{}", path), &entries);
            Response::builder()
                .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
                .header("DAV", "1")
                .body(Body::from(xml))
                .unwrap()
        }
        Err(_) => {
            let xml = webdav_bucket_multistatus(&format!("/{}", path), &[]);
            Response::builder()
                .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
                .body(Body::from(xml))
                .unwrap()
        }
    }
}

/// WebDAV MKCOL — create bucket
async fn handle_mkcol(state: &WebDAVState, path: &str) -> Response {
    match state.engine.create_bucket(path).await {
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(e) => {
            tracing::error!("MKCOL error: {}", e);
            StatusCode::METHOD_NOT_ALLOWED.into_response()
        }
    }
}

/// WebDAV GET — download object or list directory with Range support
async fn handle_get(state: &WebDAVState, path: &str, method: Method, headers: Option<&axum::http::HeaderMap>) -> Response {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return handle_root_get(state).await;
    }

    let parts: Vec<&str> = trimmed.splitn(2, '/').collect();
    let bucket = parts[0];
    let key = parts.get(1).copied().unwrap_or("");

    // Detect directory request (key empty or path ends with /)
    if key.is_empty() || path.ends_with('/') {
        let prefix = if key.is_empty() { None } else { Some(key.to_string() + "/") };
        return handle_directory_listing(state, bucket, method, prefix.as_deref()).await;
    }

    // Get object info first for size and content type
    let info = match state.engine.head_object(bucket, key).await {
        Ok(info) => info,
        Err(e) => {
            tracing::error!("WebDAV HEAD error: {}", e);
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    let content_type = info.content_type.unwrap_or_else(||
        crate::server::content_type_from_path(key)
    );
    let total_len = info.size as usize;

    // For HEAD, return metadata without body
    if method == Method::HEAD {
        return Response::builder()
            .header(header::CONTENT_TYPE, &content_type)
            .header(header::CONTENT_LENGTH, total_len.to_string())
            .header("Accept-Ranges", "bytes")
            .body(Body::empty())
            .unwrap();
    }

    // Check for Range header and parse byte range
    use tokio::sync::mpsc;
    use bytes::Bytes;
    use futures::stream::StreamExt;

    // Parse optional HTTP Range header
    let parsed_range: Option<(usize, usize)> = headers
        .and_then(|hdrs| hdrs.get("range").and_then(|v| v.to_str().ok()))
        .and_then(|rv| parse_range(rv, total_len));

    let content_length = match parsed_range {
        Some((start, end)) => end - start,
        None => total_len,
    };

    let (tx, rx) = mpsc::channel::<Result<Bytes, anyhow::Error>>(16);
    let engine_clone = state.engine.clone();
    let b = bucket.to_string();
    let k = key.to_string();

    tokio::task::spawn(async move {
        if let Err(e) = engine_clone.get_object_stream(&b, &k, tx).await {
            tracing::error!("Stream error: {}", e);
        }
    });

    use tokio_stream::wrappers::ReceiverStream;
    use futures::stream::StreamExt as _;

    let base_stream = ReceiverStream::new(rx)
        .filter_map(|r| async move { r.ok() });

    let stream: Pin<Box<dyn futures::Stream<Item = Result<Bytes, std::convert::Infallible>> + Send>> =
        if let Some((req_start, req_end)) = parsed_range {
            let mut emitted: usize = 0;
            let sliced = base_stream
                .take_while(move |_| futures::future::ready(emitted < req_end))
                .filter_map(move |chunk| {
                    let chunk_len = chunk.len();
                    let chunk_start = emitted;
                    let chunk_end = emitted + chunk_len;
                    emitted = chunk_end;
                    if chunk_end <= req_start || chunk_start >= req_end {
                        futures::future::ready(None)
                    } else {
                        let slice_begin = if chunk_start < req_start {
                            req_start - chunk_start
                        } else { 0 };
                        let slice_end = if chunk_end > req_end {
                            chunk_len - (chunk_end - req_end)
                        } else { chunk_len };
                        let sliced = chunk.slice(slice_begin..slice_end);
                        futures::future::ready(if sliced.is_empty() { None } else { Some(Ok(sliced)) })
                    }
                });
            Box::pin(sliced)
        } else {
            let full = base_stream.map(Ok);
            Box::pin(full)
        };

    let mut response = Response::builder()
        .header(header::CONTENT_TYPE, &content_type)
        .header(header::CONTENT_LENGTH, content_length.to_string())
        .header("Accept-Ranges", "bytes");

    // Add Content-Range header for partial responses
    if let Some((start, end)) = parsed_range {
        response = response
            .header("Content-Range", format!("bytes {}-{}/{}", start, end - 1, total_len))
            .status(StatusCode::PARTIAL_CONTENT);
    }

    response.body(Body::from_stream(stream)).unwrap()
}

/// WebDAV directory listing — show files in a bucket (or subdirectory) as HTML
/// Groups objects by their first path segment to create a folder-style view.
async fn handle_directory_listing(state: &WebDAVState, bucket: &str, method: Method, prefix: Option<&str>) -> Response {
    if method == Method::HEAD {
        return Response::builder()
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Body::empty())
            .unwrap();
    }

    // If prefix is set, only list objects with that prefix (subdirectory navigation)
    match state.engine.list_objects(bucket, prefix, 1000).await {
        Ok(objects) => {
            let mut html = format!(r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><title>{bucket} — MultiFS</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  body {{ font-family: -apple-system, sans-serif; max-width: 600px; margin: 40px auto; padding: 0 20px; }}
  h1 {{ color: #333; }}
  ul {{ list-style: none; padding: 0; }}
  li {{ padding: 8px 12px; margin: 4px 0; background: #f5f5f5; border-radius: 6px; display: flex; justify-content: space-between; }}
  a {{ color: #0066cc; text-decoration: none; }}
  a:hover {{ text-decoration: underline; }}
  .size {{ color: #999; font-size: 0.85em; }}
  .folder {{ color: #0066cc; font-weight: 500; }}
  .back {{ margin-bottom: 20px; }}
  .info {{ color: #666; font-size: 0.9em; margin-top: 20px; }}
</style></head><body>
<h1>📁 {bucket}</h1>
<p class="back"><a href="../">← Back to buckets</a></p>
<h2>Files</h2>
<ul>"#, bucket = bucket);

            if objects.is_empty() {
                html.push_str("<li><em>No files in this bucket.</em></li>");
                html.push_str(&format!(r#"</ul>
<div class="info">
<p><strong>S3 API:</strong> <a href="/s3/{bucket}/">/s3/{bucket}/</a></p>
</div>
</body></html>"#, bucket = bucket));
                return Response::builder()
                    .header("Content-Type", "text/html; charset=utf-8")
                    .body(Body::from(html))
                    .unwrap();
            }

            let (prefixes, files) = crate::server::group_objects_by_prefix(&objects, prefix);

            // Extract just the last segment of each prefix for display
            let strip_prefix = prefix.map(|p| {
                if p.ends_with('/') { p.to_string() } else { format!("{}/", p) }
            });

            // Render folders first — derive display name from the full prefix path
            for folder_href in &prefixes {
                let folder_name = if let Some(ref sp) = strip_prefix {
                    folder_href.strip_prefix(sp.as_str()).unwrap_or(folder_href)
                } else {
                    folder_href
                };
                let folder_name = folder_name.trim_end_matches('/');
                let href = if let Some(ref sp) = strip_prefix {
                    format!("{}{}/", sp, folder_name)
                } else {
                    format!("{}/", folder_name)
                };
                html.push_str(&format!(
                    r#"<li><a href="{href}" class="folder">📁 {name}/</a> <span class="size">folder</span></li>"#,
                    href = href,
                    name = folder_name
                ));
            }

            // Render files without a folder prefix
            for obj in &files {
                let size_str = if obj.size > 1_000_000_000 {
                    format!("{:.1} GB", obj.size as f64 / 1_000_000_000.0)
                } else if obj.size > 1_000_000 {
                    format!("{:.1} MB", obj.size as f64 / 1_000_000.0)
                } else if obj.size > 1_000 {
                    format!("{:.1} KB", obj.size as f64 / 1_000.0)
                } else {
                    format!("{} B", obj.size)
                };
                let display_name = if let Some(ref sp) = strip_prefix {
                    obj.key.strip_prefix(sp.as_str()).unwrap_or(&obj.key)
                } else {
                    &obj.key
                };
                html.push_str(&format!(
                    r#"<li><a href="{key}">{key}</a> <span class="size">{size}</span></li>"#,
                    key = display_name,
                    size = size_str
                ));
            }

            html.push_str(&format!(r#"</ul>
<div class="info">
<p><strong>S3 API:</strong> <a href="/s3/{bucket}/">/s3/{bucket}/</a></p>
</div>
</body></html>"#, bucket = bucket));

            Response::builder()
                .header("Content-Type", "text/html; charset=utf-8")
                .body(Body::from(html))
                .unwrap()
        }
        Err(_) => {
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// WebDAV PUT — upload object
async fn handle_put(state: &WebDAVState, path: &str, data: Bytes) -> Response {
    let parts: Vec<&str> = path.splitn(2, '/').collect();
    if parts.len() < 2 || parts[1].is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }

    // Auto-create bucket if it doesn't exist
    if !state.engine.bucket_exists(parts[0]).await.unwrap_or(false) {
        if let Err(_) = state.engine.create_bucket(parts[0]).await {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    match state.engine.put_object(parts[0], parts[1], &data).await {
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(e) => {
            tracing::error!("WebDAV PUT error: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// WebDAV DELETE — delete object or bucket
async fn handle_delete(state: &WebDAVState, path: &str) -> Response {
    let parts: Vec<&str> = path.splitn(2, '/').collect();

    if parts.len() == 2 && !parts[1].is_empty() {
        match state.engine.delete_object(parts[0], parts[1]).await {
            Ok(_) => StatusCode::NO_CONTENT.into_response(),
            Err(_) => StatusCode::NO_CONTENT.into_response(),
        }
    } else {
        match state.engine.delete_bucket(parts[0]).await {
            Ok(_) => StatusCode::NO_CONTENT.into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

/// WebDAV COPY — copy object within storage
async fn handle_copy(state: &WebDAVState, source: &str, _dest: &str) -> Response {
    let src_parts: Vec<&str> = source.splitn(2, '/').collect();
    if src_parts.len() < 2 || src_parts[1].is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }

    match state.engine.get_object(src_parts[0], src_parts[1]).await {
        Ok(data) => match state
            .engine
            .put_object(src_parts[0], &format!("{}_copy", src_parts[1]), &data)
            .await
        {
            Ok(_) => StatusCode::CREATED.into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// WebDAV MOVE — move/rename object (copy + delete)
async fn handle_move(state: &WebDAVState, source: &str, _dest: &str) -> Response {
    let src_parts: Vec<&str> = source.splitn(2, '/').collect();
    if src_parts.len() < 2 || src_parts[1].is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }

    match state.engine.get_object(src_parts[0], src_parts[1]).await {
        Ok(data) => {
            let dest_key = format!("{}_moved", src_parts[1]);
            match state
                .engine
                .put_object(src_parts[0], &dest_key, &data)
                .await
            {
                Ok(_) => {
                    let _ = state.engine.delete_object(src_parts[0], src_parts[1]).await;
                    StatusCode::NO_CONTENT.into_response()
                }
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Build WebDAV multistatus XML for root (list buckets)
fn webdav_root_multistatus(buckets: &[(String, String)]) -> String {
    let now = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT");

    let entries_xml: String = buckets
        .iter()
        .map(|(href, created)| {
            let display_name = href.trim_start_matches('/');
            format!(
                r#"<D:response>
    <D:href>{}/</D:href>
    <D:propstat>
        <D:prop>
            <D:displayname>{}</D:displayname>
            <D:getcontenttype>httpd/unix-directory</D:getcontenttype>
            <D:getcontentlength>0</D:getcontentlength>
            <D:getlastmodified>{}</D:getlastmodified>
            <D:resourcetype><D:collection/></D:resourcetype>
        </D:prop>
        <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
</D:response>"#,
                href, display_name, created
            )
        })
        .collect();

    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
<D:response>
    <D:href>/</D:href>
    <D:propstat>
        <D:prop>
            <D:displayname>pCloudFS</D:displayname>
            <D:getcontenttype>httpd/unix-directory</D:getcontenttype>
            <D:getcontentlength>0</D:getcontentlength>
            <D:getlastmodified>{}</D:getlastmodified>
            <D:resourcetype><D:collection/></D:resourcetype>
        </D:prop>
        <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
</D:response>
{}
</D:multistatus>"#,
        now, entries_xml
    )
}

/// Build WebDAV multistatus XML for a bucket listing
fn webdav_bucket_multistatus(parent_path: &str, entries: &[(String, String, i64)]) -> String {
    let now = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT");

    let entries_xml: String = entries
        .iter()
        .map(|(href, modified, size)| {
            let display_name = href.rsplit('/').next().unwrap_or("");
            format!(
                r#"<D:response>
    <D:href>{}</D:href>
    <D:propstat>
        <D:prop>
            <D:displayname>{}</D:displayname>
            <D:getcontenttype>application/octet-stream</D:getcontenttype>
            <D:getcontentlength>{}</D:getcontentlength>
            <D:getlastmodified>{}</D:getlastmodified>
            <D:resourcetype/>
        </D:prop>
        <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
</D:response>"#,
                href, display_name, size, modified
            )
        })
        .collect();

    let display_name = parent_path.trim_start_matches('/').rsplit('/').next().unwrap_or(parent_path);

    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
<D:response>
    <D:href>{}/</D:href>
    <D:propstat>
        <D:prop>
            <D:displayname>{}</D:displayname>
            <D:getcontenttype>httpd/unix-directory</D:getcontenttype>
            <D:getcontentlength>0</D:getcontentlength>
            <D:getlastmodified>{}</D:getlastmodified>
            <D:resourcetype><D:collection/></D:resourcetype>
        </D:prop>
        <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
</D:response>
{}
</D:multistatus>"#,
        parent_path, display_name, now, entries_xml
    )
}

/// Parse HTTP Range header like "bytes=0-1023" or "bytes=100-"
/// Returns Some((start, end)) where end is exclusive and <= total_len
fn parse_range(range: &str, total_len: usize) -> Option<(usize, usize)> {
    crate::server::parse_range(range, total_len)
}
