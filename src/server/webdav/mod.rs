use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
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
        .route("/{*path}", any(webdav_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Universal WebDAV handler
async fn webdav_handler(
    State(state): State<WebDAVState>,
    method: Method,
    Path(path): Path<String>,
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
                    "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, MKCOL, COPY, MOVE",
                )
                .status(StatusCode::OK)
                .body(Body::empty())
                .unwrap()
        }
        "PROPFIND" => handle_propfind(&state, &path).await,
        "MKCOL" => handle_mkcol(&state, &path).await,
        "GET" | "HEAD" => handle_get(&state, &path, method).await,
        "PUT" => handle_put(&state, &path, body).await,
        "DELETE" => handle_delete(&state, &path).await,
        "COPY" => handle_copy(&state, &path, "").await,
        "MOVE" => handle_move(&state, &path, "").await,
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
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

/// WebDAV GET — download object
async fn handle_get(state: &WebDAVState, path: &str, method: Method) -> Response {
    let parts: Vec<&str> = path.splitn(2, '/').collect();
    if parts.len() < 2 || parts[1].is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }

    match state.engine.get_object(parts[0], parts[1]).await {
        Ok(data) => {
            let content_type =
                mime_guess::from_path(parts[1]).first_or_octet_stream().to_string();
            let builder = Response::builder()
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, data.len().to_string());

            if method == Method::HEAD {
                builder.body(Body::empty()).unwrap()
            } else {
                builder.body(Body::from(data)).unwrap()
            }
        }
        Err(e) => {
            tracing::error!("WebDAV GET error: {}", e);
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
