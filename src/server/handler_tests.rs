/// Tests for HTTP handler-level logic: Range parsing, content-type resolution,
/// S3 multipart body consumption (regression test for the TCP-stall bug),
/// and engine error paths (put_chunked_file failure recovery).
/// These fill the gap between format-only tests and full integration tests.

#[cfg(test)]
mod handler_tests {
    use tower::ServiceExt;
    use crate::server::{parse_range, resolve_content_type, content_type_from_path};
    use crate::storage::metadata::MetadataDb;
    use crate::storage::test_utils::MockBackend;
    use crate::storage::engine::{StorageEngine, BackendHandle};
    use bytes::Bytes;

    // =====================================================================
    // Range Header Parsing
    // =====================================================================

    #[test]
    fn test_parse_range_exact() {
        // bytes=0-1023 → (0, 1024)
        assert_eq!(parse_range("bytes=0-1023", 5000), Some((0, 1024)));
    }

    #[test]
    fn test_parse_range_mid_file() {
        // bytes=100-199 → (100, 200)
        assert_eq!(parse_range("bytes=100-199", 5000), Some((100, 200)));
    }

    #[test]
    fn test_parse_range_open_end() {
        // bytes=100- → (100, total)
        assert_eq!(parse_range("bytes=100-", 5000), Some((100, 5000)));
    }

    #[test]
    fn test_parse_range_suffix() {
        // bytes=-500 → last 500 bytes
        assert_eq!(parse_range("bytes=-500", 5000), Some((4500, 5000)));
    }

    #[test]
    fn test_parse_range_suffix_exceeds_total() {
        // bytes=-99999 → capped at (0, total)
        assert_eq!(parse_range("bytes=-99999", 5000), Some((0, 5000)));
    }

    #[test]
    fn test_parse_range_end_exceeds_total() {
        // bytes=0-99999 → end capped at total (but start still valid)
        // parse_range doesn't cap the end, it just returns (0, 100000)
        // The handler layer is responsible for capping to total_len
        assert_eq!(parse_range("bytes=0-99999", 5000), Some((0, 100000)));
    }

    #[test]
    fn test_parse_range_invalid_format() {
        // Missing "bytes=" prefix
        assert_eq!(parse_range("0-100", 5000), None);
    }

    #[test]
    fn test_parse_range_empty() {
        assert_eq!(parse_range("bytes=", 5000), None);
    }

    #[test]
    fn test_parse_range_no_dash() {
        assert_eq!(parse_range("bytes=100", 5000), None);
    }

    #[test]
    fn test_parse_range_zero_length_file() {
        // Empty file: any range should be None or (0,0)
        let result = parse_range("bytes=0-", 0);
        // parse_range returns (0,0) for bytes=0- on empty file
        assert!(result.is_some());
        if let Some((s, e)) = result {
            assert_eq!(s, 0);
            assert_eq!(e, 0);
        }
    }

    #[test]
    fn test_parse_range_start_exceeds_total() {
        // bytes=6000- → start beyond file → still returns valid range
        // The handler will later return 416 Range Not Satisfiable
        let result = parse_range("bytes=6000-", 5000);
        // With open end, start=6000, end=5000 (end is total, not capped to start)
        assert_eq!(result, Some((6000, 5000)));
    }

    #[test]
    fn test_parse_range_vlc_style() {
        // VLC often sends bytes=0- for header probe
        assert_eq!(parse_range("bytes=0-", 678457386), Some((0, 678457386)));
    }

    // =====================================================================
    // Content-Type Resolution
    // =====================================================================

    #[test]
    fn test_content_type_from_extension() {
        assert_eq!(content_type_from_path("video.mp4"), "video/mp4");
        assert_eq!(content_type_from_path("image.jpg"), "image/jpeg");
        assert_eq!(content_type_from_path("document.pdf"), "application/pdf");
    }

    #[test]
    fn test_content_type_no_extension() {
        let ct = content_type_from_path("file_without_extension");
        assert_eq!(ct, "application/octet-stream");
    }

    #[test]
    fn test_resolve_ct_extension_wins_over_curl_default() {
        // curl sends application/x-www-form-urlencoded by default with --data-binary
        let ct = resolve_content_type("video.mp4", Some("application/x-www-form-urlencoded"));
        assert_eq!(ct, Some("video/mp4".to_string()));
    }

    #[test]
    fn test_resolve_ct_client_overrides_when_no_extension() {
        let ct = resolve_content_type("noext", Some("application/json"));
        assert_eq!(ct, Some("application/json".to_string()));
    }

    #[test]
    fn test_resolve_ct_octet_stream_deferred_to_extension() {
        let ct = resolve_content_type("data.json", Some("application/octet-stream"));
        assert_eq!(ct, Some("application/json".to_string()));
    }

    #[test]
    fn test_resolve_ct_client_custom_type() {
        let ct = resolve_content_type("video.mp4", Some("application/x-custom"));
        assert_eq!(ct, Some("application/x-custom".to_string()));
    }

    #[test]
    fn test_resolve_ct_both_none() {
        assert_eq!(resolve_content_type("noext", None), None);
    }

    // =====================================================================
    // S3 Multipart Body Consumption — Regression Test
    // =====================================================================
    //
    // This tests the S3 PUT handler's multipart path. The critical bug was
    // that the handler returned HTTP 200 while the client request body was
    // still being sent, causing TCP stalls.
    //
    // We simulate this by constructing the exact axum Request that the
    // handler receives and verifying the body is consumed before the
    // response is sent.

    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;

    /// When partNumber query is present, the handler must consume the body
    /// before returning. This test verifies the part is accepted (200) once a
    /// real multipart upload was initiated — proving the body was read and
    /// the upload exists.
    #[tokio::test]
    async fn test_s3_multipart_part_consumes_request_body() {
        use crate::server::s3;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        let backends: Vec<BackendHandle> = vec![
            BackendHandle::new(
                Box::new(MockBackend::new("mock-a")),
                "/mnt/mock-a".to_string(),
                "mock-a".to_string(),
                10,
            ),
        ];

        let engine = Arc::new(StorageEngine::from_backends(backends, db));
        let app = s3::build_router(engine);

        // First initiate the multipart upload so a real upload_id exists.
        let init = Request::builder()
            .method("POST")
            .uri("/test-bucket/large.bin?uploads")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(init).await.unwrap();
        let init_body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let xml = String::from_utf8_lossy(&init_body).to_string();
        let upload_id = xml.lines().find(|l| l.contains("<UploadId>"))
            .map(|l| l.trim().trim_start_matches("<UploadId>").trim_end_matches("</UploadId>").to_string())
            .expect("UploadId in initiate response");

        // Build request that simulates rclone's multipart part upload
        // with partNumber and the real uploadId, and actual body data
        let body_data = vec![0xABu8; 1024 * 1024]; // 1 MB of data
        let request = Request::builder()
            .method("PUT")
            .uri(format!("/test-bucket/large.bin?partNumber=1&uploadId={}", upload_id))
            .header("content-type", "application/octet-stream")
            .header("content-length", body_data.len().to_string())
            .body(Body::from(body_data.clone()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        
        // Critical assertion: the handler MUST return 200 OK
        assert_eq!(response.status(), 200, "Multipart part upload should return 200");
    }

    /// Without partNumber, a normal PUT should route to the regular
    /// upload path and store the data.
    #[tokio::test]
    async fn test_s3_normal_put_stores_data() {
        use crate::server::s3;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        let backends: Vec<BackendHandle> = vec![
            BackendHandle::new(
                Box::new(MockBackend::new("mock-a")),
                "/mnt/mock-a".to_string(),
                "mock-a".to_string(),
                10,
            ),
        ];

        let engine = Arc::new(StorageEngine::from_backends(backends, db));
        let app = s3::build_router(engine.clone());

        // Normal PUT without multipart params
        let body_data = b"hello world. this is a test file." as &[u8];
        let request = Request::builder()
            .method("PUT")
            .uri("/test-bucket/normal.txt")
            .header("content-type", "text/plain")
            .header("content-length", body_data.len().to_string())
            .body(Body::from(body_data.to_vec()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200, "Normal PUT should return 200");
        
        // Verify data was actually stored
        let obj = engine.get_object("test-bucket", "normal.txt").await.unwrap();
        assert_eq!(obj, body_data);
    }

    /// When using multipart-initiate (POST ?uploads), the handler should
    /// return 200 with an UploadId XML.
    #[tokio::test]
    async fn test_s3_multipart_initiate_returns_upload_id() {
        use crate::server::s3;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        let backends: Vec<BackendHandle> = vec![
            BackendHandle::new(
                Box::new(MockBackend::new("mock-a")),
                "/mnt/mock-a".to_string(),
                "mock-a".to_string(),
                10,
            ),
        ];

        let engine = Arc::new(StorageEngine::from_backends(backends, db));
        let app = s3::build_router(engine);

        let request = Request::builder()
            .method("POST")
            .uri("/test-bucket/large-file.bin?uploads")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200, "Multipart initiate should return 200");

        let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.contains("<UploadId>"), "Response should contain UploadId");
        assert!(body_str.contains("<Bucket>test-bucket</Bucket>"));
        assert!(body_str.contains("<Key>large-file.bin</Key>"));
    }

    /// Complete multipart: after initiating an upload and uploading a part,
    /// Complete must return 200 with CompleteMultipartUploadResult XML and a
    /// real object stored. (Previously the handler fabricated a success without
    /// storing anything.)
    #[tokio::test]
    async fn test_s3_multipart_complete_returns_xml() {
        use crate::server::s3;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        let backends: Vec<BackendHandle> = vec![
            BackendHandle::new(
                Box::new(MockBackend::new("mock-a")),
                "/mnt/mock-a".to_string(),
                "mock-a".to_string(),
                10,
            ),
        ];

        let engine = std::sync::Arc::new(StorageEngine::from_backends(backends, db));
        let app = s3::build_router(engine.clone());

        // Initiate the upload so it actually exists.
        let init = Request::builder()
            .method("POST")
            .uri("/test-bucket/large-file.bin?uploads")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(init).await.unwrap();
        let init_body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let xml = String::from_utf8_lossy(&init_body).to_string();
        let upload_id = xml.lines().find(|l| l.contains("<UploadId>"))
            .map(|l| l.trim().trim_start_matches("<UploadId>").trim_end_matches("</UploadId>").to_string())
            .expect("UploadId in initiate response");

        // Upload a real part first so Complete has staged data to stitch.
        // (Completing a multipart upload with zero parts is invalid and would
        // correctly return an error, not 200.)
        let part_data = b"multipart part 1 body -- enough bytes to form a staged chunk" as &[u8];
        let part_req = Request::builder()
            .method("PUT")
            .uri(format!("/test-bucket/large-file.bin?partNumber=1&uploadId={}", upload_id))
            .header("content-type", "application/octet-stream")
            .header("content-length", part_data.len().to_string())
            .body(Body::from(part_data.to_vec()))
            .unwrap();
        let part_resp = app.clone().oneshot(part_req).await.unwrap();
        assert_eq!(part_resp.status(), 200, "UploadPart should return 200");

        let request = Request::builder()
            .method("POST")
            .uri(format!("/test-bucket/large-file.bin?uploadId={}", upload_id))
            .header("content-type", "application/xml")
            .body(Body::from("<CompleteMultipartUpload></CompleteMultipartUpload>"))
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200, "Complete multipart should return 200");

        let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        let body_str = String::from_utf8_lossy(&body);
        // The handler returns CompleteMultipartUploadResult XML with ETag
        assert!(body_str.contains("CompleteMultipartUploadResult") || body_str.contains("ETag"));
    }

    // =====================================================================
    // S3 Multipart Round-Trip (Server-side fix regression test)
    // =====================================================================
    //
    // The original S3 multipart implementation never read/stored the uploaded
    // parts and returned a fabricated ETag on CompleteMultipartUpload, which
    // made rclone hit "broken pipe / empty response payload". These tests
    // exercise the full initiate -> upload-part -> complete flow against the
    // real router and verify the assembled object is actually stored and
    // retrievable with the correct bytes.

    /// Helper: build a router with an in-memory mock-backed engine.
    /// Uses a unique temp DB per call so parallel tests stay isolated.
    async fn build_s3_app() -> std::sync::Arc<StorageEngine> {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
        // Keep tempdir alive for the test by leaking it (std::mem::forget) so it
        // is not removed while the engine references the file.
        std::mem::forget(dir);
        let backends: Vec<BackendHandle> = vec![BackendHandle::new(
            Box::new(MockBackend::new("mock-a")),
            "/mnt/mock-a".to_string(),
            "mock-a".to_string(),
            10,
        )];
        std::sync::Arc::new(StorageEngine::from_backends(backends, db))
    }

    /// Full multipart round-trip: initiate -> two parts -> complete -> verify
    /// the stored object equals the concatenation of the parts.
    #[tokio::test]
    async fn test_s3_multipart_roundtrip_stores_object() {
        use crate::server::s3;

        let engine = build_s3_app().await;
        let app = s3::build_router(engine.clone());

        // 1. Initiate multipart upload
        let init = Request::builder()
            .method("POST")
            .uri("/test-bucket/large.bin?uploads")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(init).await.unwrap();
        assert_eq!(resp.status(), 200, "initiate should be 200");
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let xml = String::from_utf8_lossy(&body).to_string();
        let upload_id = xml
            .lines()
            .find(|l| l.contains("<UploadId>"))
            .and_then(|l| l.trim().strip_prefix("<UploadId>").and_then(|s| s.strip_suffix("</UploadId>")))
            .expect("UploadId in initiate response")
            .to_string();
        assert!(!upload_id.is_empty());

        // 2. Upload part 1
        let part1: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect(); // 1 KB
        let p1 = Request::builder()
            .method("PUT")
            .uri(format!("/test-bucket/large.bin?partNumber=1&uploadId={}", upload_id))
            .header("content-length", part1.len().to_string())
            .body(Body::from(part1.clone()))
            .unwrap();
        let resp = app.clone().oneshot(p1).await.unwrap();
        assert_eq!(resp.status(), 200, "part 1 should be 200");

        // 3. Upload part 2
        let part2: Vec<u8> = (0..2048).map(|i| (i % 199) as u8).collect(); // 2 KB
        let p2 = Request::builder()
            .method("PUT")
            .uri(format!("/test-bucket/large.bin?partNumber=2&uploadId={}", upload_id))
            .header("content-length", part2.len().to_string())
            .body(Body::from(part2.clone()))
            .unwrap();
        let resp = app.clone().oneshot(p2).await.unwrap();
        assert_eq!(resp.status(), 200, "part 2 should be 200");

        // 4. Complete the multipart upload
        let comp = Request::builder()
            .method("POST")
            .uri(format!("/test-bucket/large.bin?uploadId={}", upload_id))
            .header("content-type", "application/xml")
            .body(Body::from("<CompleteMultipartUpload></CompleteMultipartUpload>"))
            .unwrap();
        let resp = app.clone().oneshot(comp).await.unwrap();
        assert_eq!(resp.status(), 200, "complete should be 200");

        // 5. Verify the assembled object is retrievable with correct bytes
        let obj = engine.get_object("test-bucket", "large.bin").await.unwrap();
        let mut expected = part1;
        expected.extend_from_slice(&part2);
        assert_eq!(obj, expected, "assembled object should equal parts concatenated in order");
    }

    /// ListParts: after initiating an upload and uploading parts, GET
    /// /{bucket}/{key}?uploadId=... must return a ListPartsResult XML with the
    /// staged part numbers/sizes. rclone relies on this to verify/resume.
    #[tokio::test]
    async fn test_s3_list_parts_returns_parts_xml() {
        use crate::server::s3;

        let engine = build_s3_app().await;
        let app = s3::build_router(engine.clone());

        // Initiate
        let init = Request::builder()
            .method("POST")
            .uri("/test-bucket/large.bin?uploads")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(init).await.unwrap();
        let xml =
            String::from_utf8_lossy(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap()).to_string();
        let upload_id = xml
            .lines()
            .find(|l| l.contains("<UploadId>"))
            .map(|l| l.trim().trim_start_matches("<UploadId>").trim_end_matches("</UploadId>").to_string())
            .expect("UploadId in initiate response");

        // Upload two parts with distinguishable sizes
        let part1: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect(); // 1 KB
        let part2: Vec<u8> = (0..2048).map(|i| (i % 199) as u8).collect(); // 2 KB
        for (n, data) in [(1, &part1), (2, &part2)].iter() {
            let req = Request::builder()
                .method("PUT")
                .uri(format!("/test-bucket/large.bin?partNumber={}&uploadId={}", n, upload_id))
                .header("content-length", data.len().to_string())
                .body(Body::from((*data).clone()))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), 200, "UploadPart {} should return 200", n);
        }

        // ListParts
        let req = Request::builder()
            .method("GET")
            .uri(format!("/test-bucket/large.bin?uploadId={}", upload_id))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200, "ListParts should return 200");
        let body = String::from_utf8_lossy(&axum::body::to_bytes(resp.into_body(), 8192).await.unwrap()).to_string();

        // Schema + content assertions
        assert!(body.contains("ListPartsResult"), "should be ListPartsResult XML");
        assert!(body.contains(&format!("<UploadId>{}</UploadId>", upload_id)));
        assert!(body.contains("<PartNumber>1</PartNumber>"));
        assert!(body.contains("<PartNumber>2</PartNumber>"));
        assert!(body.contains("<Size>1024</Size>"));
        assert!(body.contains("<Size>2048</Size>"));
        assert!(body.contains("<IsTruncated>false</IsTruncated>"));
    }

    /// ListParts with an unknown upload id must return 404 NoSuchUpload.
    #[tokio::test]
    async fn test_s3_list_parts_unknown_upload_returns_404() {
        use crate::server::s3;

        let app = s3::build_router(build_s3_app().await);
        let req = Request::builder()
            .method("GET")
            .uri("/test-bucket/large.bin?uploadId=nonexistent-upload")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 404, "ListParts for unknown upload should be 404");
    }

    /// UploadPart must consume the request body (regression: previously it
    /// returned without reading the body -> broken pipe). Verify the part is
    /// actually stored by completing and reading it back.
    #[tokio::test]
    async fn test_s3_multipart_part_body_is_consumed_and_stored() {
        use crate::server::s3;

        let engine = build_s3_app().await;
        let app = s3::build_router(engine.clone());

        // Initiate
        let init = Request::builder().method("POST").uri("/b/obj?uploads").body(Body::empty()).unwrap();
        let resp = app.clone().oneshot(init).await.unwrap();
        let xml = String::from_utf8_lossy(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap()).to_string();
        let uid = xml.lines().find(|l| l.contains("<UploadId>")).map(|l| l.trim().trim_start_matches("<UploadId>").trim_end_matches("</UploadId>").to_string()).unwrap();

        // Upload one part with known data
        let data: Vec<u8> = (0..512*1024).map(|i| (i % 256) as u8).collect(); // 512 KB
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/b/obj?partNumber=1&uploadId={}", uid))
            .header("content-length", data.len().to_string())
            .body(Body::from(data.clone()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        // Complete and verify stored bytes
        let comp = Request::builder().method("POST").uri(format!("/b/obj?uploadId={}", uid))
            .body(Body::from("<CompleteMultipartUpload/>")).unwrap();
        let resp = app.clone().oneshot(comp).await.unwrap();
        assert_eq!(resp.status(), 200);
        let stored = engine.get_object("b", "obj").await.unwrap();
        assert_eq!(stored, data);
    }

    /// Unmatched uploadId on complete should be a hard error (NoSuchUpload),
    /// mirroring S3 behavior, not a silent fabricated success.
    #[tokio::test]
    async fn test_s3_multipart_complete_unknown_upload_errors() {
        use crate::server::s3;

        let engine = build_s3_app().await;
        let app = s3::build_router(engine);

        let comp = Request::builder()
            .method("POST")
            .uri("/b/obj?uploadId=nonexistent-upload")
            .body(Body::from("<CompleteMultipartUpload/>"))
            .unwrap();
        let resp = app.oneshot(comp).await.unwrap();
        // No such upload must not fabricate a 200 success.
        assert!(resp.status() == 404 || resp.status() == 400, "expected error, got {}", resp.status());
    }

    // =====================================================================
    // Engine: put_chunked_file Error Handling
    // =====================================================================

    /// When put_chunked_file encounters a backend upload error for one chunk,
    /// the current behavior is to log the error and continue. This test
    /// documents the CURRENT behavior (which is a known issue — the upload
    /// should abort, not silently continue).
    #[tokio::test]
    async fn test_put_chunked_file_partial_failure_current_behavior() {
        // Use a TrackedBackend that fails on chunk index 1
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        // Create a failing backend: it works for all paths except one
        let mut failing = MockBackend::new("failing");
        // We'll make it fail for chunk 1's path

        let b1 = Arc::new(crate::storage::test_utils::TrackedBackend::wrap(
            Box::new(MockBackend::new("tracked-a"))
        ));
        let b2 = Arc::new(crate::storage::test_utils::TrackedBackend::wrap(
            Box::new(MockBackend::new("tracked-b"))
        ));

        // Mark chunk 1's path as missing on BOTH backends (since round-robin placement)
        b1.add_missing_path("/mnt/tracked-a/test-bucket/big.bin.ck.1");
        b2.add_missing_path("/mnt/tracked-b/test-bucket/big.bin.ck.1");

        let backends: Vec<BackendHandle> = vec![
            BackendHandle::new(
                Box::new((*b1).clone()),
                "/mnt/tracked-a".to_string(),
                "tracked-a".to_string(),
                10,
            ),
            BackendHandle::new(
                Box::new((*b2).clone()),
                "/mnt/tracked-b".to_string(),
                "tracked-b".to_string(),
                10,
            ),
        ];

        let engine = StorageEngine::from_backends(backends, db);
        engine.create_bucket("test-bucket").await.unwrap();

        // 65 MB → 3 chunks. Chunk 1 goes to tracked-b (round-robin: 0→a, 1→b, 2→a)
        let data = vec![0xCDu8; 65 * 1024 * 1024];
        
        // The upload to the engine will NOT fail — put_chunked_file logs errors
        // and continues (known issue). It stores metadata only for successful chunks.
        let result = engine.put_object("test-bucket", "big.bin", &data).await;
        
        // CURRENT BEHAVIOR: upload succeeds (silently drops failed chunks)
        assert!(result.is_ok(), "put_object should return Ok (known issue: silent failure)");
        
        // But download SHOULD fail because chunk 1's data was never stored.
        // The engine resolves chunk 1's path and backend, but the backend
        // reports it as missing.
        let download = engine.get_object("test-bucket", "big.bin").await;
        // CURRENT: this fails because the chunk was never stored
        assert!(download.is_err(), "Download should fail because chunk 1 is missing");
        
        // DESIRED BEHAVIOR (documented for future fix):
        // 1. put_object should return Err when any chunk fails
        // 2. Already-uploaded chunks should be cleaned up
    }

    // =====================================================================
    // WebDAV: Handler Status Codes
    // =====================================================================

    /// Test that DELETE on a non-existent object returns 204 (idempotent).
    /// This is the S3 behavior — WebDAV should also handle gracefully.
    #[tokio::test]
    async fn test_s3_delete_nonexistent_returns_204() {
        use crate::server::s3;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        let backends: Vec<BackendHandle> = vec![
            BackendHandle::new(
                Box::new(MockBackend::new("mock-a")),
                "/mnt/mock-a".to_string(),
                "mock-a".to_string(),
                10,
            ),
        ];

        let engine = Arc::new(StorageEngine::from_backends(backends, db));
        let app = s3::build_router(engine);

        let request = Request::builder()
            .method("DELETE")
            .uri("/test-bucket/nonexistent.txt")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        // Delete is idempotent — always returns 204
        assert_eq!(response.status(), 204);
    }

    /// HEAD on a non-existent object returns 404
    #[tokio::test]
    async fn test_s3_head_nonexistent_returns_404() {
        use crate::server::s3;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        let backends: Vec<BackendHandle> = vec![
            BackendHandle::new(
                Box::new(MockBackend::new("mock-a")),
                "/mnt/mock-a".to_string(),
                "mock-a".to_string(),
                10,
            ),
        ];

        let engine = Arc::new(StorageEngine::from_backends(backends, db));
        let app = s3::build_router(engine);

        let request = Request::builder()
            .method("HEAD")
            .uri("/test-bucket/nonexistent.txt")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 404);
    }

    // =====================================================================
    // S3 Bucket Operations
    // =====================================================================

    #[tokio::test]
    async fn test_s3_create_and_head_bucket() {
        use crate::server::s3;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        let backends: Vec<BackendHandle> = vec![
            BackendHandle::new(
                Box::new(MockBackend::new("mock-a")),
                "/mnt/mock-a".to_string(),
                "mock-a".to_string(),
                10,
            ),
        ];

        let engine = Arc::new(StorageEngine::from_backends(backends, db));
        let app = s3::build_router(engine);

        // Create bucket
        let request = Request::builder()
            .method("PUT")
            .uri("/mynewbucket")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200, "Create bucket should return 200");

        // Head bucket
        let request = Request::builder()
            .method("HEAD")
            .uri("/mynewbucket")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200, "Head bucket should return 200");
        assert_eq!(
            response.headers().get("x-amz-bucket-region").unwrap(),
            "us-east-1"
        );
    }

    #[tokio::test]
    async fn test_s3_head_nonexistent_bucket() {
        use crate::server::s3;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        let backends: Vec<BackendHandle> = vec![
            BackendHandle::new(
                Box::new(MockBackend::new("mock-a")),
                "/mnt/mock-a".to_string(),
                "mock-a".to_string(),
                10,
            ),
        ];

        let engine = Arc::new(StorageEngine::from_backends(backends, db));
        let app = s3::build_router(engine);

        let request = Request::builder()
            .method("HEAD")
            .uri("/no-such-bucket")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn test_s3_location_query_returns_xml() {
        use crate::server::s3;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        let backends: Vec<BackendHandle> = vec![
            BackendHandle::new(
                Box::new(MockBackend::new("mock-a")),
                "/mnt/mock-a".to_string(),
                "mock-a".to_string(),
                10,
            ),
        ];

        let engine = std::sync::Arc::new(StorageEngine::from_backends(backends, db));
        let app = s3::build_router(engine);

        let request = Request::builder()
            .method("GET")
            .uri("/anybucket?location")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);

        let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        let body_str = String::from_utf8_lossy(&body);
        // The location endpoint returns LocationConstraint XML
        assert!(body_str.contains("LocationConstraint"));
        assert!(body_str.contains("us-east-1"));
    }

    #[tokio::test]
    async fn test_s3_versioning_query_returns_xml() {
        use crate::server::s3;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        let backends: Vec<BackendHandle> = vec![
            BackendHandle::new(
                Box::new(MockBackend::new("mock-a")),
                "/mnt/mock-a".to_string(),
                "mock-a".to_string(),
                10,
            ),
        ];

        let engine = Arc::new(StorageEngine::from_backends(backends, db));
        let app = s3::build_router(engine);

        let request = Request::builder()
            .method("GET")
            .uri("/anybucket?versioning")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);

        let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.contains("<Status>Suspended</Status>"));
    }
}
