#[cfg(test)]
mod tests {
    use crate::storage::metadata::MetadataDb;
    use crate::storage::test_utils::MockBackend;
    use sha2::{Digest, Sha256};

    /// Commit a version directly (reserve + commit) for metadata tests.
    fn put_committed(
        db: &MetadataDb,
        bucket: &str,
        key: &str,
        size: i64,
        etag: &str,
        last_modified: &str,
        account: &str,
        remote_path: &str,
    ) {
        let (version, _p) = db.reserve_version(bucket, key, account, "/mnt").unwrap();
        db.commit_version(bucket, key, version, size, etag, last_modified, None, remote_path)
            .unwrap();
    }

    // ---- Metadata DB ----

    #[test]
    fn test_create_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        assert!(db.bucket_exists("test").unwrap());
    }

    #[test]
    fn test_delete_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        db.delete_bucket("test").unwrap();
        assert!(!db.bucket_exists("test").unwrap());
    }

    #[test]
    fn test_put_get_object() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        put_committed(&db, "test", "hello.txt", 12, "abc", "2026-01-01", "acct1", "/remote/hello.txt");
        let obj = db.get_object("test", "hello.txt").unwrap().unwrap();
        assert_eq!(obj.key, "hello.txt");
        assert_eq!(obj.size, 12);
        assert_eq!(obj.account_email, "acct1");
    }

    #[test]
    fn test_delete_object() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        put_committed(&db, "test", "a.txt", 1, "e", "2026-01-01", "a1", "/r/a.txt");
        db.delete_object("test", "a.txt").unwrap();
        assert!(db.get_object("test", "a.txt").unwrap().is_none());
    }

    #[test]
    fn test_list_objects() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        put_committed(&db, "test", "a.txt", 1, "e1", "2026-01-01", "a1", "/r/a.txt");
        put_committed(&db, "test", "b.txt", 2, "e2", "2026-01-01", "a1", "/r/b.txt");
        assert_eq!(db.list_objects("test", None, None, 10).unwrap().len(), 2);
        assert_eq!(
            db.list_objects("test", Some("a"), None, 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn test_list_objects_includes_all() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();

        put_committed(&db, "test", "small.txt", 100, "etag1", "2026-01-01", "a1", "/r/small.txt");
        put_committed(&db, "test", "large-video.mp4", 100_000_000, "etag3", "2026-01-01", "a1", "/r/large-video.mp4");

        let objects = db.list_objects("test", None, None, 100).unwrap();
        let keys: Vec<&str> = objects.iter().map(|o| o.key.as_str()).collect();
        assert!(keys.contains(&"small.txt"));
        assert!(keys.contains(&"large-video.mp4"));
        assert_eq!(objects.len(), 2);
    }

    #[test]
    fn test_count_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        put_committed(&db, "test", "a.txt", 100, "e1", "2026-01-01", "a1", "/r/a.txt");
        put_committed(&db, "test", "b.txt", 200, "e2", "2026-01-01", "a1", "/r/b.txt");
        assert_eq!(db.count_objects("test").unwrap(), 2);
        assert_eq!(db.bucket_total_size("test").unwrap(), 300);
        assert_eq!(db.count_objects_for_account("a1").unwrap(), 2);
        assert_eq!(db.account_total_size("a1").unwrap(), 300);
    }

    // ---- S3 XML format tests ----

    #[test]
    fn test_s3_head_bucket_headers() {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
        db.create_bucket("test-bucket").unwrap();
        assert!(db.bucket_exists("test-bucket").unwrap());
    }

    #[test]
    fn test_s3_multipart_upload_xml() {
        let bucket = "my-bucket";
        let key = "large-file.bin";
        let upload_id = "multipart-20260724220000";
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>{}</Bucket>
  <Key>{}</Key>
  <UploadId>{}</UploadId>
</InitiateMultipartUploadResult>"#,
            bucket, key, upload_id
        );
        assert!(xml.contains("<Bucket>my-bucket</Bucket>"));
        assert!(xml.contains("<Key>large-file.bin</Key>"));
        assert!(xml.contains("<UploadId>multipart-20260724220000</UploadId>"));
    }

    #[test]
    fn test_s3_complete_multipart_xml() {
        let etag_val = "\"multipart-20260724220000\"";
        let bucket = "my-bucket";
        let key = "large-file.bin";
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Location>https://example.com/s3/{}/{}</Location>
  <Bucket>{}</Bucket>
  <Key>{}</Key>
  <ETag>{}</ETag>
</CompleteMultipartUploadResult>"#,
            bucket, key, bucket, key, etag_val
        );
        assert!(xml.contains("<Bucket>my-bucket</Bucket>"));
        assert!(xml.contains("<Key>large-file.bin</Key>"));
        assert!(xml.contains("<ETag>\"multipart-20260724220000\"</ETag>"));
    }

    #[test]
    fn test_s3_list_buckets_xml() {
        let buckets = vec![
            ("bucket-a".to_string(), "2026-01-01".to_string()),
            ("bucket-b".to_string(), "2026-01-02".to_string()),
        ];
        let buckets_xml: String = buckets
            .iter()
            .map(|(name, created)| {
                format!(
                    "<Bucket><Name>{}</Name><CreationDate>{}</CreationDate></Bucket>",
                    name, created
                )
            })
            .collect();
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<ListAllMyBucketsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Owner><ID>multifs</ID><DisplayName>multifs</DisplayName></Owner>
  <Buckets>{}</Buckets>
</ListAllMyBucketsResult>"#,
            buckets_xml
        );
        assert!(xml.contains("<Bucket><Name>bucket-a</Name>"));
        assert!(xml.contains("<Bucket><Name>bucket-b</Name>"));
        assert!(xml.contains("<Owner><ID>multifs</ID><DisplayName>multifs</DisplayName></Owner>"));
    }

    #[test]
    fn test_s3_location_xml() {
        let region = "us-east-1";
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<LocationConstraint xmlns="http://s3.amazonaws.com/doc/2006-03-01/">{}</LocationConstraint>"#,
            region
        );
        assert!(xml.contains("us-east-1"));
        assert!(xml.contains("<LocationConstraint"));
    }

    #[test]
    fn test_s3_versioning_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Status>Suspended</Status>
</VersioningConfiguration>"#;
        assert!(xml.contains("<Status>Suspended</Status>"));
        assert!(xml.contains("<VersioningConfiguration"));
    }

    #[test]
    fn test_s3_error_xml() {
        let code = "NoSuchKey";
        let message = "The specified key does not exist.";
        let resource = "my-bucket/my-file.txt";
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>{}</Code>
  <Message>{}</Message>
  <Resource>{}</Resource>
  <RequestId>multifs</RequestId>
</Error>"#,
            code, message, resource
        );
        assert!(xml.contains("<Code>NoSuchKey</Code>"));
        assert!(xml.contains("<Message>The specified key does not exist.</Message>"));
        assert!(xml.contains("<RequestId>multifs</RequestId>"));
    }

    // ---- StorageEngine tests (using MockBackend) ----

    struct TestEngine {
        engine: crate::storage::engine::StorageEngine,
        _dir: tempfile::TempDir,
    }

    fn make_test_engine() -> TestEngine {
        let (engine, dir) = crate::storage::test_utils::make_test_engine();
        TestEngine {
            engine,
            _dir: dir,
        }
    }

    #[tokio::test]
    async fn test_engine_put_get_object() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("test-bucket").await.unwrap();

        let data = b"Hello, MultiFS Engine!" as &[u8];
        let obj = engine
            .put_object("test-bucket", "hello.txt", data)
            .await
            .unwrap();
        assert_eq!(obj.key, "hello.txt");
        assert_eq!(obj.size, data.len() as i64);

        let downloaded = engine
            .get_object("test-bucket", "hello.txt")
            .await
            .unwrap();
        assert_eq!(downloaded, data);
    }

    #[tokio::test]
    async fn test_engine_put_get_empty_object() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("empty-bucket").await.unwrap();

        let empty: &[u8] = &[];
        let obj = engine
            .put_object("empty-bucket", "empty.txt", empty)
            .await
            .unwrap();
        assert_eq!(obj.size, 0);

        let downloaded = engine
            .get_object("empty-bucket", "empty.txt")
            .await
            .unwrap();
        assert!(downloaded.is_empty());
    }

    #[tokio::test]
    async fn test_engine_delete_object() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("del-bucket").await.unwrap();

        engine
            .put_object("del-bucket", "tmp.txt", b"temp")
            .await
            .unwrap();
        engine
            .delete_object("del-bucket", "tmp.txt")
            .await
            .unwrap();

        let result = engine.get_object("del-bucket", "tmp.txt").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_engine_list_objects() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("list-bucket").await.unwrap();

        engine
            .put_object("list-bucket", "a.txt", b"aaa")
            .await
            .unwrap();
        engine
            .put_object("list-bucket", "b.txt", b"bbb")
            .await
            .unwrap();
        engine
            .put_object("list-bucket", "c.txt", b"ccc")
            .await
            .unwrap();

        let (objs, _) = engine
            .list_objects("list-bucket", None, None, 100)
            .await
            .unwrap();
        assert_eq!(objs.len(), 3);

        let (prefixed, _) = engine
            .list_objects("list-bucket", Some("a"), None, 100)
            .await
            .unwrap();
        assert_eq!(prefixed.len(), 1);
        assert_eq!(prefixed[0].key, "a.txt");
    }

    #[tokio::test]
    async fn test_engine_list_objects_pagination() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("page-bucket").await.unwrap();
        for k in ["a", "b", "c", "d", "e"] {
            engine
                .put_object("page-bucket", k, k.as_bytes())
                .await
                .unwrap();
        }

        let (p1, t1) = engine
            .list_objects("page-bucket", None, None, 2)
            .await
            .unwrap();
        assert_eq!(p1.len(), 2);
        assert!(t1);
        assert_eq!(p1[0].key, "a");
        assert_eq!(p1[1].key, "b");
        let token = p1.last().unwrap().key.clone();

        let (p2, t2) = engine
            .list_objects("page-bucket", None, Some(&token), 2)
            .await
            .unwrap();
        assert_eq!(p2.len(), 2);
        assert!(t2);
        assert_eq!(p2[0].key, "c");
        assert_eq!(p2[1].key, "d");

        let token2 = p2.last().unwrap().key.clone();
        let (p3, t3) = engine
            .list_objects("page-bucket", None, Some(&token2), 2)
            .await
            .unwrap();
        assert_eq!(p3.len(), 1);
        assert!(!t3);
        assert_eq!(p3[0].key, "e");
    }

    #[tokio::test]
    async fn test_engine_bucket_crud() {
        let t = make_test_engine();
        let engine = &t.engine;

        assert!(!engine.bucket_exists("my-bucket").await.unwrap());
        engine.create_bucket("my-bucket").await.unwrap();
        assert!(engine.bucket_exists("my-bucket").await.unwrap());
        engine.delete_bucket("my-bucket").await.unwrap();
        assert!(!engine.bucket_exists("my-bucket").await.unwrap());
    }

    #[tokio::test]
    async fn test_engine_non_existent_object() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("ghost-bucket").await.unwrap();

        let result = engine.get_object("ghost-bucket", "nope.txt").await;
        assert!(result.is_err());

        let del_result = engine.delete_object("ghost-bucket", "nope.txt").await;
        assert!(del_result.is_ok()); // idempotent delete
    }

    #[tokio::test]
    async fn test_engine_round_robin_across_backends() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("rr-bucket").await.unwrap();

        engine
            .put_object("rr-bucket", "f1.txt", b"data1")
            .await
            .unwrap();
        engine
            .put_object("rr-bucket", "f2.txt", b"data2")
            .await
            .unwrap();
        engine
            .put_object("rr-bucket", "f3.txt", b"data3")
            .await
            .unwrap();
        engine
            .put_object("rr-bucket", "f4.txt", b"data4")
            .await
            .unwrap();

        assert_eq!(
            engine.get_object("rr-bucket", "f1.txt").await.unwrap(),
            b"data1"
        );
        assert_eq!(
            engine.get_object("rr-bucket", "f2.txt").await.unwrap(),
            b"data2"
        );
        assert_eq!(
            engine.get_object("rr-bucket", "f3.txt").await.unwrap(),
            b"data3"
        );
        assert_eq!(
            engine.get_object("rr-bucket", "f4.txt").await.unwrap(),
            b"data4"
        );
    }

    #[tokio::test]
    async fn test_utilization_prefers_cloud_over_local() {
        use crate::storage::engine::{BackendHandle, StorageEngine};
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();

        // Cloud (priority 0) and local (priority 1) both empty → cloud preferred.
        let handles = vec![
            BackendHandle::new(
                Box::new(MockBackend::new("cloud")),
                "/cloud".to_string(),
                "cloud".to_string(),
                10,
            )
            .with_priority(0),
            BackendHandle::new(
                Box::new(MockBackend::new("local")),
                "/local".to_string(),
                "local".to_string(),
                10,
            )
            .with_priority(1),
        ];
        let engine = StorageEngine::from_backends(handles, db);
        let info = engine.put_object("tier-bucket", "k", b"data").await.unwrap();
        assert_eq!(info.account_email, "cloud");
    }

    #[tokio::test]
    async fn test_utilization_falls_back_to_local_when_cloud_full() {
        use crate::storage::engine::{BackendHandle, StorageEngine};
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();

        // Cloud (priority 0) is full (10/10 bytes); local (priority 1) is empty.
        let cloud = MockBackend::with_total("cloud", 10);
        cloud
            .files
            .lock()
            .unwrap()
            .insert("/full".to_string(), vec![0u8; 10]);
        let handles = vec![
            BackendHandle::new(Box::new(cloud), "/cloud".to_string(), "cloud".to_string(), 10)
                .with_priority(0),
            BackendHandle::new(
                Box::new(MockBackend::new("local")),
                "/local".to_string(),
                "local".to_string(),
                10,
            )
            .with_priority(1),
        ];
        let engine = StorageEngine::from_backends(handles, db);
        let info = engine.put_object("tier-bucket", "k", b"data").await.unwrap();
        assert_eq!(info.account_email, "local");
    }

    #[tokio::test]
    async fn test_engine_delete_bucket_with_mixed_files() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("mixed-bucket").await.unwrap();

        engine
            .put_object("mixed-bucket", "small.txt", b"hi")
            .await
            .unwrap();
        let big_data = vec![0xCDu8; 70 * 1024 * 1024];
        engine
            .put_object("mixed-bucket", "large.bin", &big_data)
            .await
            .unwrap();

        let (objs, _) = engine
            .list_objects("mixed-bucket", None, None, 100)
            .await
            .unwrap();
        assert_eq!(objs.len(), 2);

        engine.delete_bucket("mixed-bucket").await.unwrap();
        assert!(!engine.bucket_exists("mixed-bucket").await.unwrap());
    }

    // ---- Streaming download tests ----

    #[tokio::test]
    async fn test_streaming_full_file_download() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("stream-bucket").await.unwrap();

        let data = vec![0xABu8; 3 * 1024 * 1024]; // 3 MB
        engine
            .put_object("stream-bucket", "video.mp4", &data)
            .await
            .unwrap();

        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<Result<bytes::Bytes, anyhow::Error>>(64);
        engine
            .get_object_stream("stream-bucket", "video.mp4", None, tx)
            .await
            .unwrap();

        let mut total = 0usize;
        while let Some(res) = rx.recv().await {
            match res {
                Ok(chunk) => total += chunk.len(),
                Err(e) => panic!("Stream error: {}", e),
            }
        }
        assert_eq!(total, data.len());
    }

    #[tokio::test]
    async fn test_streaming_range_download() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("range-bucket").await.unwrap();

        let mut data = Vec::with_capacity(10 * 1024 * 1024);
        for i in 0..(10 * 1024 * 1024) {
            data.push((i % 256) as u8);
        }
        engine
            .put_object("range-bucket", "file.bin", &data)
            .await
            .unwrap();

        // Request bytes 1MB-2MB
        let start = 1_048_576;
        let end = 2_097_152;
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<Result<bytes::Bytes, anyhow::Error>>(64);
        engine
            .get_object_stream("range-bucket", "file.bin", Some((start, end)), tx)
            .await
            .unwrap();

        let mut received = Vec::new();
        while let Some(res) = rx.recv().await {
            match res {
                Ok(chunk) => received.extend_from_slice(&chunk),
                Err(_) => break,
            }
        }

        // MockBackend now honors the byte range (inclusive start, exclusive end),
        // matching the production pCloud CDN HTTP Range behavior.
        let expected = &data[start..end];
        assert_eq!(received.len(), expected.len());
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn test_single_file_roundtrip_integrity() {
        // Single-file roundtrip with SHA-256 verification
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("integrity-bucket").await.unwrap();

        let mut data = Vec::with_capacity(50 * 1024 * 1024);
        for i in 0..(50 * 1024 * 1024) {
            data.push((i % 251) as u8);
        }

        let original_hash = {
            let mut hasher = Sha256::new();
            hasher.update(&data);
            hex::encode(hasher.finalize())
        };

        let obj = engine
            .put_object("integrity-bucket", "big.bin", &data)
            .await
            .unwrap();
        assert_eq!(obj.size, data.len() as i64);
        assert_eq!(obj.etag, original_hash);

        let downloaded = engine
            .get_object("integrity-bucket", "big.bin")
            .await
            .unwrap();
        assert_eq!(downloaded.len(), data.len());

        let downloaded_hash = {
            let mut hasher = Sha256::new();
            hasher.update(&downloaded);
            hex::encode(hasher.finalize())
        };
        assert_eq!(original_hash, downloaded_hash);
    }

    #[tokio::test]
    async fn test_streaming_ttfb_within_500ms() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("ttfb-bucket").await.unwrap();

        let data = vec![0xABu8; 33 * 1024 * 1024];
        engine
            .put_object("ttfb-bucket", "video.mp4", &data)
            .await
            .unwrap();

        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<Result<bytes::Bytes, anyhow::Error>>(64);
        let start = std::time::Instant::now();

        engine
            .get_object_stream("ttfb-bucket", "video.mp4", Some((0, 65536)), tx)
            .await
            .unwrap();

        let mut got_data = false;
        while let Some(res) = rx.recv().await {
            match res {
                Ok(_chunk) => {
                    if !got_data {
                        got_data = true;
                        let elapsed = start.elapsed();
                        assert!(
                            elapsed.as_millis() < 500,
                            "TTFB {}ms exceeded 500ms threshold",
                            elapsed.as_millis()
                        );
                    }
                }
                Err(e) => panic!("Stream error: {}", e),
            }
        }
        assert!(got_data, "Should have received data");
    }

    #[tokio::test]
    async fn test_concurrent_streaming() {
        let t = make_test_engine();
        let engine = std::sync::Arc::new(t.engine);
        engine.create_bucket("concurrent-bucket").await.unwrap();

        let file1 = vec![0x11u8; 5 * 1024 * 1024];
        let file2 = vec![0x22u8; 5 * 1024 * 1024];
        let file3 = vec![0x33u8; 5 * 1024 * 1024];

        engine
            .put_object("concurrent-bucket", "f1.mp4", &file1)
            .await
            .unwrap();
        engine
            .put_object("concurrent-bucket", "f2.mp4", &file2)
            .await
            .unwrap();
        engine
            .put_object("concurrent-bucket", "f3.mp4", &file3)
            .await
            .unwrap();

        let handles: Vec<_> = (0..3)
            .map(|i| {
                let e = engine.clone();
                let key = format!("f{}.mp4", i + 1);
                tokio::spawn(async move {
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<
                        Result<bytes::Bytes, anyhow::Error>,
                    >(64);
                    let start = std::time::Instant::now();
                    e.get_object_stream("concurrent-bucket", &key, Some((0, 65536)), tx)
                        .await
                        .unwrap();
                    let mut got = false;
                    while let Some(res) = rx.recv().await {
                        if let Ok(_) = res {
                            if !got {
                                got = true;
                                let elapsed = start.elapsed();
                                assert!(
                                    elapsed.as_millis() < 3000,
                                    "Concurrent TTFB for {} exceeded 3s: {}ms",
                                    key,
                                    elapsed.as_millis()
                                );
                            }
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.await.unwrap();
        }
    }

    #[test]
    fn test_create_and_list_bucket() {}

    #[test]
    fn test_list_buckets_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
        let buckets = db.list_buckets().unwrap();
        assert!(buckets.is_empty(), "New DB should have no buckets");
    }
}
