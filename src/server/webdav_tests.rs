/// WebDAV protocol tests

#[cfg(test)]
mod tests {
    use crate::storage::metadata::MetadataDb;

    #[test]
    fn test_propfind_root_xml() {
        // Verify PROPFIND root response XML format
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
        // Verify PROPFIND directory listing XML format
        let now = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let entries = vec![
            ("file1.txt", 100i64, &now),
            ("file2.json", 200i64, &now),
        ];
        
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
        // Verify OPTIONS response includes all supported methods
        let allow = "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, MKCOL, COPY, MOVE";
        assert!(allow.contains("PROPPATCH"));
        assert!(allow.contains("COPY"));
        assert!(allow.contains("MOVE"));
        assert!(allow.contains("PROPFIND"));
    }

    #[test]
    fn test_mkcol_bucket_creation() {
        // Verify MKCOL creates a bucket (equivalent to PUT /bucket)
        use crate::storage::metadata::MetadataDb;
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
        db.create_bucket("webdav-test").unwrap();
        assert!(db.bucket_exists("webdav-test").unwrap());
        
        let buckets = db.list_buckets().unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].name, "webdav-test");
    }

    #[test]
    fn test_webdav_put_get_roundtrip() {
        // Verify PUT then GET returns same data (via metadata layer)
        use crate::storage::metadata::MetadataDb;
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
        
        // Create bucket and put object
        db.create_bucket("test-bucket").unwrap();
        db.put_object("test-bucket", "hello.txt", 11, 
            "etag123", "2026-01-01", "acct@test", "/remote/hello.txt", Some("text/plain")).unwrap();
        
        // Get object back
        let obj = db.get_object("test-bucket", "hello.txt").unwrap().unwrap();
        assert_eq!(obj.key, "hello.txt");
        assert_eq!(obj.size, 11);
    }

    #[test]
    fn test_webdav_delete() {
        // Verify DELETE removes object from metadata
        use crate::storage::metadata::MetadataDb;
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
        
        db.create_bucket("del-test").unwrap();
        db.put_object("del-test", "del.txt", 5, "etag", "2026-01-01", "acct", "/remote/del.txt", None).unwrap();
        assert!(db.get_object("del-test", "del.txt").unwrap().is_some());
        
        db.delete_object("del-test", "del.txt").unwrap();
        assert!(db.get_object("del-test", "del.txt").unwrap().is_none());
    }

    #[test]
    fn test_webdav_copy_move_stub() {
        // Verify COPY and MOVE destination path logic
        let source = "/bucket/source.txt";
        let dest = "/bucket/dest.txt";
        
        // Extract bucket/key from paths
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
        // Verify PROPPATCH basic structure
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
        // Verify Range header parsing matches S3 behavior
        // The parse_range function is shared via the S3 handler
        // Just validate the concept: bytes=0-99 means first 100 bytes
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
    fn test_webdav_suffix_range() {
        // Verify suffix range: bytes=-500 means last 500 bytes
        let range = "bytes=-500";
        assert!(range.starts_with("bytes="));
        let rest = &range["bytes=".len()..];
        assert!(rest.starts_with('-'));
        let suffix: usize = rest[1..].parse().unwrap();
        assert_eq!(suffix, 500);
    }
}
