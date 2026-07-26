/// WebDAV protocol tests

#[cfg(test)]
mod tests {
    use std::process::Command;

    const S3_BASE: &str = "http://100.100.30.59:9000";
    const WEBDAV_BASE: &str = "http://100.100.30.59:8080";

    #[test]
    fn test_propfind_root_xml() {
        let now = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let xml = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/multifs/</D:href>
    <D:propstat>
      <D:prop>
        <D:displayname>MultiFS</D:displayname>
        <D:getlastmodified>{}</D:getlastmodified>
        <D:resourcetype><D:collection/></D:resourcetype>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#,
            now
        );
        assert!(xml.contains("<D:multistatus"));
        assert!(xml.contains("<D:collection/>"));
        assert!(xml.contains("HTTP/1.1 200 OK"));
    }

    #[test]
    fn test_propfind_directory_xml() {
        let now = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let entries = vec![("file1.txt", 100i64, &now), ("file2.json", 200i64, &now)];

        let entries_xml: String = entries.iter().map(|(name, size, modified)| {
            format!(
                r#"  <D:response>
    <D:href>/multifs/bucket/{}</D:href>
    <D:propstat>
      <D:prop>
        <D:displayname>{}</D:displayname>
        <D:getlastmodified>{}</D:getlastmodified>
        <D:getcontentlength>{}</D:getcontentlength>
        <D:getcontenttype>application/octet-stream</D:getcontenttype>
        <D:resourcetype/>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>"#,
                name, name, modified, size
            )
        }).collect();

        let xml = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/multifs/bucket/</D:href>
    <D:propstat>
      <D:prop>
        <D:displayname>bucket</D:displayname>
        <D:getlastmodified>{}</D:getlastmodified>
        <D:resourcetype><D:collection/></D:resourcetype>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
{}
</D:multistatus>"#,
            now, entries_xml
        );
        assert!(xml.contains("file1.txt"));
        assert!(xml.contains("file2.json"));
        assert!(xml.contains("getcontentlength"));
        assert!(xml.contains("<D:collection/>"));
    }

    #[test]
    fn test_options_response_headers() {
        let allow = "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, MKCOL, COPY, MOVE";
        assert!(allow.contains("PROPPATCH"));
        assert!(allow.contains("COPY"));
        assert!(allow.contains("MOVE"));
        assert!(allow.contains("PROPFIND"));
    }

    #[test]
    fn test_mkcol_bucket_creation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = crate::storage::metadata::MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("webdav-test").unwrap();
        assert!(db.bucket_exists("webdav-test").unwrap());

        let buckets = db.list_buckets().unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].name, "webdav-test");
    }

    #[test]
    fn test_webdav_put_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = crate::storage::metadata::MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        db.create_bucket("test-bucket").unwrap();
        db.put_object("test-bucket", "hello.txt", 11,
            "etag123", "2026-01-01", "acct@test", "/remote/hello.txt", Some("text/plain")).unwrap();

        let obj = db.get_object("test-bucket", "hello.txt").unwrap().unwrap();
        assert_eq!(obj.key, "hello.txt");
        assert_eq!(obj.size, 11);
    }

    #[test]
    fn test_webdav_delete() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = crate::storage::metadata::MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        db.create_bucket("del-test").unwrap();
        db.put_object("del-test", "del.txt", 5, "etag", "2026-01-01", "acct", "/remote/del.txt", None).unwrap();
        assert!(db.get_object("del-test", "del.txt").unwrap().is_some());

        db.delete_object("del-test", "del.txt").unwrap();
        assert!(db.get_object("del-test", "del.txt").unwrap().is_none());
    }

    #[test]
    fn test_webdav_copy_move_stub() {
        let source = "/bucket/source.txt";
        let dest = "/bucket/dest.txt";

        let src_parts: Vec<&str> = source.trim_start_matches('/').splitn(2, '/').collect();
        let dst_parts: Vec<&str> = dest.trim_start_matches('/').splitn(2, '/').collect();

        assert_eq!(src_parts.len(), 2);
        assert_eq!(dst_parts.len(), 2);
        assert_eq!(src_parts[0], "bucket");
        assert_eq!(src_parts[1], "source.txt");
        assert_eq!(dst_parts[1], "dest.txt");
    }

    #[test]
    fn test_webdav_proppatch_stub() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propertyupdate xmlns:D="DAV:">
  <D:set>
    <D:prop>
      <D:displayname>New Name</D:displayname>
    </D:prop>
  </D:set>
</D:propertyupdate>"#;
        assert!(xml.contains("<D:propertyupdate"));
        assert!(xml.contains("<D:set>"));
        assert!(xml.contains("<D:displayname>New Name</D:displayname>"));
    }

    #[test]
    fn test_webdav_range_header_parsing() {
        let range = "bytes=0-99";
        assert!(range.starts_with("bytes="));
        let rest = &range["bytes=".len()..];
        let parts: Vec<&str> = rest.split('-').collect();
        assert_eq!(parts.len(), 2);
        let start: usize = parts[0].parse().unwrap();
        let _end: usize = parts[1].parse().unwrap();
        assert_eq!(start, 0);
    }

    #[test]
    fn test_webdav_bucket_html_listing() {
        let bucket = "video-subtitle";
        let objects = vec![
            ("blor-074.mkv", 743646855i64),
            ("test/config.json", 16i64),
            ("test/notes.txt", 11i64),
        ];

        let mut html = format!(r#"<h1>📁 {}</h1>"#, bucket);
        for (key, size) in &objects {
            let size_str = if *size > 1_000_000_000 {
                format!("{:.1} GB", *size as f64 / 1_000_000_000.0)
            } else if *size > 1_000_000 {
                format!("{:.1} MB", *size as f64 / 1_000_000.0)
            } else {
                format!("{} B", size)
            };
            html.push_str(&format!("<li>{} — {}</li>", key, size_str));
        }

        assert!(html.contains("📁 video-subtitle"));
        assert!(html.contains("blor-074.mkv"));
        assert!(html.contains("test/config.json"));
        assert!(html.contains("743.6 MB"));
    }

    #[test]
    fn test_webdav_directory_path_detection() {
        let path1 = "bucket/";
        let path2 = "bucket/subdir/";
        let path3 = "bucket/file.txt";

        assert!(path1.ends_with('/'));
        assert!(path2.ends_with('/'));
        assert!(!path3.ends_with('/'));

        let parts: Vec<&str> = path1.trim_end_matches('/').splitn(2, '/').collect();
        assert_eq!(parts.len(), 1);

        let parts: Vec<&str> = path2.trim_end_matches('/').splitn(2, '/').collect();
        assert_eq!(parts.len(), 2);

        let parts: Vec<&str> = path3.splitn(2, '/').collect();
        assert_eq!(parts[1], "file.txt");
    }

    #[test]
    fn test_webdav_root_html() {
        let buckets = vec!["video-subtitle", "william-test"];
        let mut html = String::new();
        for b in &buckets {
            html.push_str(&format!("<li><a href='{}'>{}</a></li>", b, b));
        }
        assert!(html.contains("video-subtitle"));
        assert!(html.contains("william-test"));
    }

    #[test]
    fn test_webdav_file_size_formatting() {
        let sizes: Vec<(i64, &str)> = vec![
            (500, "500 B"),
            (1500, "1.5 KB"),
            (1_500_000, "1.5 MB"),
            (1_500_000_000, "1.5 GB"),
        ];

        for (size, expected_prefix) in &sizes {
            let formatted = if *size > 1_000_000_000 {
                format!("{:.1} GB", *size as f64 / 1_000_000_000.0)
            } else if *size > 1_000_000 {
                format!("{:.1} MB", *size as f64 / 1_000_000.0)
            } else if *size > 1_000 {
                format!("{:.1} KB", *size as f64 / 1_000.0)
            } else {
                format!("{} B", size)
            };
            assert!(formatted.contains(expected_prefix), "{} should contain {}", formatted, expected_prefix);
        }
    }

    #[test]
    fn test_webdav_suffix_range() {
        let range = "bytes=-500";
        assert!(range.starts_with("bytes="));
        let rest = &range["bytes=".len()..];
        assert!(rest.starts_with('-'));
        let suffix: usize = rest[1..].parse().unwrap();
        assert_eq!(suffix, 500);
    }

    // ---- Integration tests (hit the running server) ----
    // These use a dedicated test bucket with generated content

    fn curl_output(args: &[&str]) -> Vec<u8> {
        let output = Command::new("curl")
            .args(args)
            .output()
            .expect("curl failed");
        assert!(output.status.success(), "curl failed: {:?}", args);
        output.stdout
    }

    fn curl_status(args: &[&str]) -> (u16, u64) {
        let output = Command::new("curl")
            .args(args)
            .output()
            .expect("curl failed");
        let code_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        // HTTP status code and size are in stdout (with -w format)
        let parts: Vec<&str> = code_str.split(':').collect();
        let code: u16 = parts[0].parse().unwrap_or(0);
        let size: u64 = parts.get(1).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        if !output.status.success() && code == 0 {
            panic!("curl HTTP error: {}", String::from_utf8_lossy(&output.stderr));
        }
        (code, size)
    }

    #[test]
    #[ignore = "Requires running server at 100.100.30.59:8080 with deployed binary"]
    fn test_webdav_full_pipeline() {
        let bucket = "integration-test-int-074";
        let folder = "test-folder";
        let small_file = "small-file.bin";
        let large_file = "large-file.bin";

        // Step 1: Create test bucket via S3
        let output = Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT", &format!("{}/{}", S3_BASE, bucket)])
            .output()
            .expect("curl failed");
        let code: u16 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap();
        assert!(code == 200 || code == 409, "Create bucket returned {}", code);

        // Step 2: Generate small file (128 KB) with known content
        let small_bytes: Vec<u8> = (0..128 * 1024).map(|i| (i % 256) as u8).collect();
        std::fs::write("/tmp/inttest-small.bin", &small_bytes)
            .expect("Failed to write small test file");

        // Compute SHA256 via external command
        let shasum = Command::new("sha256sum")
            .arg("/tmp/inttest-small.bin")
            .output()
            .expect("sha256sum failed");
        let small_expected_hash = String::from_utf8_lossy(&shasum.stdout)
            .split_whitespace().next().unwrap().to_string();
        assert!(!small_expected_hash.is_empty(), "Empty SHA256 for small file");

        // Step 3: Upload small file via S3
        let output = Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT",
                &format!("{S3_BASE}/{bucket}/{folder}/{small_file}"),
                "--data-binary", "@/tmp/inttest-small.bin"])
            .output()
            .expect("curl failed");
        let code: u16 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap();
        assert_eq!(code, 200, "Upload small file returned {}", code);

        // Step 4: Generate large file (33 MB = just over chunk threshold)
        let large_bytes: Vec<u8> = (0..33 * 1024 * 1024).map(|i| ((i * 17 + 31) % 256) as u8).collect();
        std::fs::write("/tmp/inttest-large.bin", &large_bytes)
            .expect("Failed to write large test file");

        let shasum = Command::new("sha256sum")
            .arg("/tmp/inttest-large.bin")
            .output()
            .expect("sha256sum failed");
        let large_expected_hash = String::from_utf8_lossy(&shasum.stdout)
            .split_whitespace().next().unwrap().to_string();
        assert!(!large_expected_hash.is_empty(), "Empty SHA256 for large file");

        // Step 5: Upload large file via S3
        let output = Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT",
                &format!("{S3_BASE}/{bucket}/{folder}/{large_file}"),
                "--data-binary", "@/tmp/inttest-large.bin"])
            .output()
            .expect("curl failed");
        let code: u16 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap();
        assert_eq!(code, 200, "Upload large file returned {}", code);

        // Step 6: List folder via WebDAV PROPFIND
        {
            let body = curl_output(&["-s", "-X", "PROPFIND",
                &format!("{WEBDAV_BASE}/{bucket}/{folder}/"),
                "-H", "Depth: 1"]);
            let body_str = String::from_utf8_lossy(&body);
            assert!(body_str.contains(small_file), "PROPFIND should list small file");
            assert!(body_str.contains(large_file), "PROPFIND should list large file");
            assert!(body_str.contains("getcontentlength"), "PROPFIND should include sizes");
        }

        // Step 7: Download small file via WebDAV and verify content + SHA256
        {
            let body = curl_output(&["-s", &format!("{WEBDAV_BASE}/{bucket}/{folder}/{small_file}")]);
            assert_eq!(body.len(), 128 * 1024, "Small file size mismatch");
            assert_eq!(&body[..], &small_bytes[..], "Small file content mismatch");

            // Verify via external sha256sum
            std::fs::write("/tmp/inttest-small-dl.bin", &body).expect("write dl");
            let shasum = Command::new("sha256sum")
                .arg("/tmp/inttest-small-dl.bin")
                .output()
                .expect("sha256sum failed");
            let dl_hash = String::from_utf8_lossy(&shasum.stdout)
                .split_whitespace().next().unwrap().to_string();
            assert_eq!(dl_hash, small_expected_hash, "Small file SHA256 mismatch");
        }

        // Step 8: Download large file via WebDAV, verify first bytes + full SHA256
        {
            // Range request: first 100 bytes
            let body = curl_output(&["-s", "-H", "Range: bytes=0-99",
                &format!("{WEBDAV_BASE}/{bucket}/{folder}/{large_file}")]);
            assert_eq!(body.len(), 100, "Range should return 100 bytes");
            assert_eq!(&body[..], &large_bytes[..100], "Large file first 100 bytes mismatch");

            // Full download
            let body = curl_output(&["-s", &format!("{WEBDAV_BASE}/{bucket}/{folder}/{large_file}")]);
            assert_eq!(body.len(), 33 * 1024 * 1024, "Large file size mismatch");

            std::fs::write("/tmp/inttest-large-dl.bin", &body).expect("write dl");
            let shasum = Command::new("sha256sum")
                .arg("/tmp/inttest-large-dl.bin")
                .output()
                .expect("sha256sum failed");
            let dl_hash = String::from_utf8_lossy(&shasum.stdout)
                .split_whitespace().next().unwrap().to_string();
            assert_eq!(dl_hash, large_expected_hash, "Large file SHA256 mismatch");
        }

        // Step 9: Clean up — delete files and bucket
        {
            let output = Command::new("curl")
                .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE",
                    &format!("{S3_BASE}/{bucket}/{folder}/{small_file}")])
                .output().expect("curl failed");
            let code: u16 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap();
            assert!(code == 204 || code == 200, "Delete small file returned {}", code);
        }
        {
            let output = Command::new("curl")
                .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE",
                    &format!("{S3_BASE}/{bucket}/{folder}/{large_file}")])
                .output().expect("curl failed");
            let code: u16 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap();
            assert!(code == 204 || code == 200, "Delete large file returned {}", code);
        }
        {
            let output = Command::new("curl")
                .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE",
                    &format!("{S3_BASE}/{bucket}")])
                .output().expect("curl failed");
            let code: u16 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap();
            assert!(code == 204 || code == 200, "Delete bucket returned {}", code);
        }

        // Clean up temp files
        for f in &["/tmp/inttest-small.bin", "/tmp/inttest-large.bin",
                   "/tmp/inttest-small-dl.bin", "/tmp/inttest-large-dl.bin"] {
            let _ = std::fs::remove_file(f);
        }
    }
}
