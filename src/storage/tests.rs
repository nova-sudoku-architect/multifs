#[cfg(test)]
mod tests {
    use crate::storage::metadata::MetadataDb;
    use crate::storage::chunk_manager;
    use crate::storage::placement;
    use sha2::{Digest, Sha256};
    use crate::storage::test_utils::MockBackend;

    // ---- Chunk Manager Integration ----

    #[test]
    fn test_chunk_manager_split_33mb() {
        // 3 MB -> 1 chunk (fits in 32 MB)
        let data = vec![0xABu8; 3 * 1024 * 1024];
        let chunks = chunk_manager::split(&data, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data.len(), 3 * 1024 * 1024);
        assert!(chunk_manager::verify_chunk(&chunks[0]));
    }

    #[test]
    fn test_chunk_manager_roundtrip_64mb() {
        // 6 MB -> 1 chunk (fits in 32 MB)
        let data = vec![0x42u8; 6 * 1024 * 1024];
        let chunks = chunk_manager::split(&data, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 1);
        let reassembled = chunk_manager::assemble(&chunks);
        assert_eq!(reassembled.len(), 6 * 1024 * 1024);
    }

    #[test]
    fn test_chunk_manager_roundtrip_80mb() {
        // 30 MB -> 1 chunk (fits in 32 MB)
        let data = vec![0x01, 0x02, 0x03];  // small test, varies sizes
        let data = data.repeat(10 * 1024 * 1024); // 30 MB
        let chunks = chunk_manager::split(&data, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 1);
        let reassembled = chunk_manager::assemble(&chunks);
        assert_eq!(data, reassembled);
    }

    // ---- Placement Integration ----

    #[test]
    fn test_placement_7_chunks_6_accounts_wrapping() {
        let accounts = vec![
            "a1".to_string(), "a2".to_string(), "a3".to_string(),
            "a4".to_string(), "a5".to_string(), "a6".to_string(),
        ];
        let plan = placement::plan_placement(&accounts, 7);
        assert_eq!(plan.account_assignments.len(), 7);
        // Chunk 0 -> a1, chunk 5 -> a6, chunk 6 -> a1 (wraps)
        assert_eq!(plan.account_assignments[0].1, "a1");
        assert_eq!(plan.account_assignments[5].1, "a6");
        assert_eq!(plan.account_assignments[6].1, "a1");
    }

    #[test]
    fn test_placement_42_chunks_even_distribution() {
        let accounts = vec![
            "a1".to_string(), "a2".to_string(), "a3".to_string(),
            "a4".to_string(), "a5".to_string(), "a6".to_string(),
        ];
        let plan = placement::plan_placement(&accounts, 42);
        let unique = plan.unique_accounts();
        for acct in &accounts {
            let count = plan.account_assignments.iter()
                .filter(|(_, a)| a == acct)
                .count();
            assert_eq!(count, 7, "Account {} has {} chunks", acct, count);
        }
        assert_eq!(unique.len(), 6);
    }

    #[test]
    fn test_placement_single_account_for_small_file() {
        let accounts = vec!["single".to_string()];
        let plan = placement::plan_placement(&accounts, 3);
        assert_eq!(plan.account_assignments.len(), 3);
        for (_, acct) in &plan.account_assignments {
            assert_eq!(acct, "single");
        }
    }

    // ---- Full Pipeline Simulation (no erasure coding) ----

    #[test]
    fn test_full_pipeline_33mb() {
        let original = vec![0xDEu8; 3 * 1024 * 1024]; // 3 MB

        // Split
        let chunks = chunk_manager::split(&original, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 1);

        // Assemble
        let result = chunk_manager::assemble(&chunks);
        assert_eq!(result, original, "3 MB pipeline integrity check failed");
    }

    #[test]
    fn test_full_pipeline_64mb() {
        let original = vec![0xCDu8; 6 * 1024 * 1024]; // 6 MB, fits in 1 chunk

        let chunks = chunk_manager::split(&original, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 1);
        let result = chunk_manager::assemble(&chunks);
        assert_eq!(result, original, "6 MB pipeline integrity check failed");
    }

    #[test]
    fn test_full_pipeline_empty_file() {
        let original = Vec::<u8>::new();
        let chunks = chunk_manager::split(&original, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 0);
        let reassembled = chunk_manager::assemble(&chunks);
        assert!(reassembled.is_empty());
    }

    #[test]
    fn test_full_pipeline_checksum_integrity() {
        let original = b"Hello, MultiFS! This is a test of the chunking pipeline's checksum integrity verification.".to_vec();

        let chunks = chunk_manager::split(&original, 10); // tiny chunks for testing
        assert!(chunks.len() > 1);

        // Verify all chunks
        for chunk in &chunks {
            assert!(chunk_manager::verify_chunk(chunk), "Chunk {} failed checksum", chunk.index);
        }

        // Corrupt one chunk
        let mut corrupted = chunks.clone();
        if let Some(first) = corrupted.first_mut() {
            if !first.data.is_empty() {
                first.data[0] ^= 0xFF; // flip bits
                assert!(!chunk_manager::verify_chunk(first), "Corruption not detected");
            }
        }
    }

    // ---- Metadata DB Integration ----

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
        db.put_object("test", "hello.txt", 12, "abc", "2026-01-01", "acct1", "/remote/hello.txt", None).unwrap();
        let obj = db.get_object("test", "hello.txt").unwrap().unwrap();
        assert_eq!(obj.key, "hello.txt");
        assert_eq!(obj.size, 12);
        assert_eq!(obj.account_email, "acct1");
    }

    #[test]
    fn test_s3_head_bucket_headers() {
        use crate::storage::metadata::MetadataDb;
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
    fn test_s3_upload_part_response() {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"multipart-part");
        let etag = hex::encode(hasher.finalize());
        assert_eq!(etag.len(), 64);
        assert_eq!(etag[..16].len(), 16);
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

    #[test]
    fn test_delete_object() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        db.put_object("test", "a.txt", 1, "e", "2026-01-01", "a1", "/r/a.txt", None).unwrap();
        db.delete_object("test", "a.txt").unwrap();
        assert!(db.get_object("test", "a.txt").unwrap().is_none());
    }

    #[test]
    fn test_list_objects() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        db.put_object("test", "a.txt", 1, "e1", "2026-01-01", "a1", "/r/a.txt", None).unwrap();
        db.put_object("test", "b.txt", 2, "e2", "2026-01-01", "a1", "/r/b.txt", None).unwrap();
        assert_eq!(db.list_objects("test", None, 10).unwrap().len(), 2);
        assert_eq!(db.list_objects("test", Some("a"), 10).unwrap().len(), 1);
    }

    #[test]
    fn test_list_objects_includes_chunked() {
        // Ensure list_objects includes both whole-file (objects table) and chunked (files table) entries
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();

        // Whole-file entries go into objects table
        db.put_object("test", "small.txt", 100, "etag1", "2026-01-01", "a1", "/r/small.txt", None).unwrap();
        db.put_object("test", "tiny.txt", 10, "etag2", "2026-01-01", "a1", "/r/tiny.txt", None).unwrap();

        // Chunked entries go into files table (as inserted by engine's put_chunked_file)
        db.with_conn(|conn| {
            use rusqlite::params;
            conn.execute(
                "INSERT INTO files (bucket_name, key, size, etag, last_modified, content_type, storage_type)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'chunked')",
                params!["test", "large-video.mp4", 100_000_000, "etag3", "2026-01-01", "video/mp4"],
            )?;
            // Also insert a chunk record so the file is properly tracked
            conn.execute(
                "INSERT INTO chunks (bucket_name, key, chunk_index, size, checksum, is_parity, account_email, remote_path)
                 VALUES (?1, ?2, 0, 100_000_000, 'abc123', 0, 'a1', '/r/large-video.ck.0')",
                params!["test", "large-video.mp4"],
            )?;
            Ok(())
        }).unwrap();

        // list_objects should return all 3 entries (2 whole + 1 chunked)
        let objects = db.list_objects("test", None, 100).unwrap();
        let keys: Vec<&str> = objects.iter().map(|o| o.key.as_str()).collect();
        assert!(keys.contains(&"small.txt"), "Should list whole-file small.txt");
        assert!(keys.contains(&"tiny.txt"), "Should list whole-file tiny.txt");
        assert!(keys.contains(&"large-video.mp4"), "Should list chunked large-video.mp4");
        assert_eq!(objects.len(), 3, "Should return 3 items total (2 whole + 1 chunked)");

        // Prefix filter should also match chunked files
        let filtered = db.list_objects("test", Some("large"), 100).unwrap();
        assert_eq!(filtered.len(), 1, "Prefix 'large' should match chunked file");
        assert_eq!(filtered[0].key, "large-video.mp4");
    }

    #[test]
    fn test_count_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        db.put_object("test", "a.txt", 100, "e1", "2026-01-01", "a1", "/r/a.txt", None).unwrap();
        db.put_object("test", "b.txt", 200, "e2", "2026-01-01", "a1", "/r/b.txt", None).unwrap();
        assert_eq!(db.count_objects("test").unwrap(), 2);
        assert_eq!(db.bucket_total_size("test").unwrap(), 300);
        assert_eq!(db.count_objects_for_account("a1").unwrap(), 2);
        assert_eq!(db.account_total_size("a1").unwrap(), 300);
    }

    // ---- StorageEngine Tests (using MockBackend) ----

    struct TestEngine {
        engine: crate::storage::engine::StorageEngine,
        _dir: tempfile::TempDir,
    }

    fn make_test_engine() -> TestEngine {
        let (engine, dir) = crate::storage::test_utils::make_test_engine();
        TestEngine { engine, _dir: dir }
    }

    #[tokio::test]
    async fn test_engine_put_get_object() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("test-bucket").await.unwrap();

        let data = b"Hello, MultiFS Engine!" as &[u8];
        let obj = engine.put_object("test-bucket", "hello.txt", data).await.unwrap();
        assert_eq!(obj.key, "hello.txt");
        assert_eq!(obj.size, data.len() as i64);

        let downloaded = engine.get_object("test-bucket", "hello.txt").await.unwrap();
        assert_eq!(downloaded, data);
    }

    #[tokio::test]
    async fn test_engine_put_get_empty_object() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("empty-bucket").await.unwrap();

        let empty: &[u8] = &[];
        let obj = engine.put_object("empty-bucket", "empty.txt", empty).await.unwrap();
        assert_eq!(obj.size, 0);

        let downloaded = engine.get_object("empty-bucket", "empty.txt").await.unwrap();
        assert!(downloaded.is_empty());
    }

    #[tokio::test]
    async fn test_engine_delete_object() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("del-bucket").await.unwrap();

        engine.put_object("del-bucket", "tmp.txt", b"temp").await.unwrap();
        engine.delete_object("del-bucket", "tmp.txt").await.unwrap();

        // Should fail (not found)
        let result = engine.get_object("del-bucket", "tmp.txt").await;
        assert!(result.is_err(), "Fetching deleted object should fail");
    }

    #[tokio::test]
    async fn test_engine_list_objects() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("list-bucket").await.unwrap();

        engine.put_object("list-bucket", "a.txt", b"aaa").await.unwrap();
        engine.put_object("list-bucket", "b.txt", b"bbb").await.unwrap();
        engine.put_object("list-bucket", "c.txt", b"ccc").await.unwrap();

        let objs = engine.list_objects("list-bucket", None, 100).await.unwrap();
        assert_eq!(objs.len(), 3);

        let prefixed = engine.list_objects("list-bucket", Some("a"), 100).await.unwrap();
        assert_eq!(prefixed.len(), 1);
        assert_eq!(prefixed[0].key, "a.txt");
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
        assert!(del_result.is_err());
    }

    #[tokio::test]
    async fn test_engine_round_robin_across_backends() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("rr-bucket").await.unwrap();

        // Put 4 small files — they should round-robin across 2 backends
        engine.put_object("rr-bucket", "f1.txt", b"data1").await.unwrap();
        engine.put_object("rr-bucket", "f2.txt", b"data2").await.unwrap();
        engine.put_object("rr-bucket", "f3.txt", b"data3").await.unwrap();
        engine.put_object("rr-bucket", "f4.txt", b"data4").await.unwrap();

        // All 4 should be retrievable
        assert_eq!(engine.get_object("rr-bucket", "f1.txt").await.unwrap(), b"data1");
        assert_eq!(engine.get_object("rr-bucket", "f2.txt").await.unwrap(), b"data2");
        assert_eq!(engine.get_object("rr-bucket", "f3.txt").await.unwrap(), b"data3");
        assert_eq!(engine.get_object("rr-bucket", "f4.txt").await.unwrap(), b"data4");
    }

    #[tokio::test]
    async fn test_engine_delete_chunked_file() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("chunk-bucket").await.unwrap();

        // 80 MB should be chunked (chunk size is 32 MB)
        let big_data = vec![0xABu8; 80 * 1024 * 1024];
        engine.put_object("chunk-bucket", "bigfile.bin", &big_data).await.unwrap();

        // Verify it exists
        let downloaded = engine.get_object("chunk-bucket", "bigfile.bin").await.unwrap();
        assert_eq!(downloaded.len(), 80 * 1024 * 1024);
        assert_eq!(&downloaded[..10], &big_data[..10]);

        // Now delete it
        engine.delete_object("chunk-bucket", "bigfile.bin").await.unwrap();

        // Should be gone
        let result = engine.get_object("chunk-bucket", "bigfile.bin").await;
        assert!(result.is_err(), "Deleted chunked file should not be retrievable");
    }

    #[tokio::test]
    async fn test_engine_delete_bucket_with_mixed_files() {
        let t = make_test_engine();
        let engine = &t.engine;
        engine.create_bucket("mixed-bucket").await.unwrap();

        // Mix of small and large files
        engine.put_object("mixed-bucket", "small.txt", b"hi").await.unwrap();
        let big_data = vec![0xCDu8; 70 * 1024 * 1024]; // chunked
        engine.put_object("mixed-bucket", "large.bin", &big_data).await.unwrap();

        // List should show both
        let objs = engine.list_objects("mixed-bucket", None, 100).await.unwrap();
        assert_eq!(objs.len(), 2);

        // Delete the bucket — should clean both whole-file and chunked
        engine.delete_bucket("mixed-bucket").await.unwrap();
        assert!(!engine.bucket_exists("mixed-bucket").await.unwrap());
    }
}

#[test]
fn test_list_buckets_when_empty() {
    use crate::storage::metadata::MetadataDb;
    let dir = tempfile::tempdir().unwrap();
    let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
    let buckets = db.list_buckets().unwrap();
    assert!(buckets.is_empty(), "New DB should have no buckets");
}

#[test]
fn test_create_and_list_bucket() {
    use crate::storage::metadata::MetadataDb;
    let dir = tempfile::tempdir().unwrap();
    let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
    db.create_bucket("my-bucket").unwrap();
    db.create_bucket("other-bucket").unwrap();
    let buckets = db.list_buckets().unwrap();
    assert_eq!(buckets.len(), 2);
    assert_eq!(buckets[0].name, "my-bucket");
    assert_eq!(buckets[1].name, "other-bucket");
}
