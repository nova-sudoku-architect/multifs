#[cfg(test)]
mod handler_tests {
    use std::sync::Arc;
    use axum::body::Body;
    use http::{Request, Method, StatusCode};
    use tower::ServiceExt;

    use crate::storage::engine::StorageEngine;
    use crate::server::{s3, webdav};

    fn make_engine() -> (Arc<StorageEngine>, tempfile::TempDir) {
        let (engine, dir) = crate::storage::test_utils::make_test_engine();
        (Arc::new(engine), dir)
    }

    async fn read_body(body: Body) -> String {
        use http_body_util::BodyExt;
        let bytes = body.collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).to_string()
    }

    // ---- S3 Handler Tests ----

    #[tokio::test]
    async fn test_s3_list_buckets() {
        let (engine, _dir) = make_engine();
        let app = s3::build_router(engine);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp.into_body()).await;
        assert!(body.contains("ListAllMyBucketsResult"));
    }

    #[tokio::test]
    async fn test_s3_create_bucket() {
        let (engine, _dir) = make_engine();
        let app = s3::build_router(engine);

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/s3-test-bucket")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_s3_create_duplicate_bucket() {
        let (engine, _dir) = make_engine();
        let app = s3::build_router(engine);

        let req1 = Request::builder().method(Method::PUT).uri("/dup-bucket").body(Body::empty()).unwrap();
        let _ = app.clone().oneshot(req1).await.unwrap();

        // create_bucket is idempotent (ensure_bucket), so duplicate returns 200 OK
        let req2 = Request::builder().method(Method::PUT).uri("/dup-bucket").body(Body::empty()).unwrap();
        let resp = app.oneshot(req2).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_s3_upload_and_download() {
        let (engine, _dir) = make_engine();
        let app = s3::build_router(engine);

        // Create bucket
        let req = Request::builder().method(Method::PUT).uri("/dl-bucket").body(Body::empty()).unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        // Upload
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/dl-bucket/hello.txt")
            .header("Content-Type", "text/plain")
            .body(Body::from("Hello S3!"))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Download
        let req = Request::builder()
            .method(Method::GET)
            .uri("/dl-bucket/hello.txt")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp.into_body()).await;
        assert_eq!(body, "Hello S3!");
    }

    #[tokio::test]
    async fn test_s3_head_object() {
        let (engine, _dir) = make_engine();
        let app = s3::build_router(engine);

        let req = Request::builder().method(Method::PUT).uri("/head-bucket").body(Body::empty()).unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/head-bucket/test.txt")
            .body(Body::from("head content"))
            .unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method(Method::HEAD)
            .uri("/head-bucket/test.txt")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_s3_head_missing_object() {
        let (engine, _dir) = make_engine();
        let app = s3::build_router(engine);

        let req = Request::builder().method(Method::PUT).uri("/ghost-bucket").body(Body::empty()).unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method(Method::HEAD)
            .uri("/ghost-bucket/nope.txt")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_s3_delete_object() {
        let (engine, _dir) = make_engine();
        let app = s3::build_router(engine);

        let req = Request::builder().method(Method::PUT).uri("/del-bucket").body(Body::empty()).unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/del-bucket/to-delete.txt")
            .body(Body::from("delete me"))
            .unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/del-bucket/to-delete.txt")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn test_s3_list_objects() {
        let (engine, _dir) = make_engine();
        let app = s3::build_router(engine);

        let req = Request::builder().method(Method::PUT).uri("/list-bucket").body(Body::empty()).unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/list-bucket/a.txt")
            .body(Body::from("aaa"))
            .unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/list-bucket/b.txt")
            .body(Body::from("bbb"))
            .unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/list-bucket")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp.into_body()).await;
        assert!(body.contains("ListBucketResult"));
        assert!(body.contains("a.txt"));
        assert!(body.contains("b.txt"));
    }

    #[tokio::test]
    async fn test_s3_delete_bucket() {
        let (engine, _dir) = make_engine();
        let app = s3::build_router(engine);

        let req = Request::builder().method(Method::PUT).uri("/rm-bucket").body(Body::empty()).unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/rm-bucket")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // List buckets should not include it
        let req = Request::builder().method(Method::GET).uri("/").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = read_body(resp.into_body()).await;
        assert!(!body.contains("rm-bucket"));
    }

    #[tokio::test]
    async fn test_s3_versioning() {
        let (engine, _dir) = make_engine();
        let app = s3::build_router(engine);

        let req = Request::builder().method(Method::PUT).uri("/version-bucket").body(Body::empty()).unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/version-bucket?versioning")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp.into_body()).await;
        assert!(body.contains("Suspended"));
    }

    // ---- WebDAV Handler Tests ----

    #[tokio::test]
    async fn test_webdav_propfind_root() {
        let (engine, _dir) = make_engine();
        let app = webdav::build_router(engine);

        let req = Request::builder()
            .method(Method::from_bytes(b"PROPFIND").unwrap())
            .uri("/")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_webdav_put_and_get() {
        let (engine, _dir) = make_engine();
        let app = webdav::build_router(engine);

        // PUT a file (WebDAV requires bucket/key path)
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/wd-bucket/test-file.txt")
            .body(Body::from("WebDAV content!"))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // GET the file
        let req = Request::builder()
            .method(Method::GET)
            .uri("/wd-bucket/test-file.txt")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp.into_body()).await;
        assert_eq!(body, "WebDAV content!");
    }

    #[tokio::test]
    async fn test_webdav_delete_file() {
        let (engine, _dir) = make_engine();
        let app = webdav::build_router(engine);

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/del-bucket/tmp-file.txt")
            .body(Body::from("temp"))
            .unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/del-bucket/tmp-file.txt")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn test_webdav_get_nonexistent() {
        let (engine, _dir) = make_engine();
        let app = webdav::build_router(engine);

        // WebDAV returns 404 for objects that don't exist in a known bucket
        let req = Request::builder()
            .method(Method::GET)
            .uri("/nonexistent-bucket/no-such-file.txt")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_webdav_browser_folder_listing() {
        let (engine, _dir) = make_engine();
        let app = webdav::build_router(engine);

        // Upload files to same prefix to test folder grouping
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/videos/vid1.mp4")
            .body(Body::from("video content"))
            .unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/videos/vid2.mp4")
            .body(Body::from("more video"))
            .unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        // GET with Accept: text/html triggers browser view
        let req = Request::builder()
            .method(Method::GET)
            .uri("/videos/")
            .header("Accept", "text/html")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp.into_body()).await;
        assert!(body.contains("vid1.mp4"), "Should list vid1.mp4 in HTML");
        assert!(body.contains("vid2.mp4"), "Should list vid2.mp4 in HTML");
    }
}
