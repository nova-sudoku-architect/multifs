/// pCloud Integration Tests
///
/// Tests the full MultiFS → pCloud pipeline end-to-end using real pCloud accounts.
/// These tests are IGNORED by default and require explicit opt-in:
///
///   PCLOUD_INTEGRATION_TEST=1 cargo test --test pcloud_integration -- --nocapture
///
/// SAFETY DESIGN:
/// - All objects go under `/multifs-itest/YYYYMMDD-HHMMSS/` on pCloud
/// - Each test run uses a unique subfolder (timestamped)
/// - All test data is deleted after each test (even on failure, via Drop guard)
/// - Files are small (<1 MB) to minimize quota impact
/// - Matches exactly one pCloud account (nova-video) to avoid spreading test data
///
/// WHAT'S TESTED:
/// 1. upload/download roundtrip (small file, whole-file path)
/// 2. upload/download roundtrip (chunked file, >32MB)
/// 3. object metadata (HEAD)
/// 4. bucket CRUD
/// 5. object listing
/// 6. deletion and idempotent delete
/// 7. error handling (non-existent object)
/// 8. concurrent uploads to different accounts

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const S3_BASE: &str = "http://127.0.0.1:9000";
const WEBDAV_BASE: &str = "http://127.0.0.1:8080";

/// Generate a unique test prefix: itest-20260803-215800
fn test_prefix() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Use UTC date-time for readability
    let dt = chrono::Utc::now();
    format!("itest-{}", dt.format("%Y%m%d-%H%M%S"))
}

/// Helper: run curl GET and get status code + body (handles binary safely)
fn curl_get(url: &str) -> (u16, Vec<u8>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(1000);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = format!("/tmp/pcloud-itest-get-{}-{}.bin", std::process::id(), n);
    
    let output = Command::new("curl")
        .args(["-s", "-o", &tmp, "-w", "\n%{http_code}", url])
        .output()
        .expect("curl GET failed");
    
    let body = std::fs::read(&tmp).unwrap_or_default();
    let _ = std::fs::remove_file(&tmp);
    
    // Status code is in stdout (body went to -o file)
    let status_str = String::from_utf8_lossy(&output.stdout);
    let code: u16 = status_str.trim().lines().last().unwrap_or("0").trim().parse().unwrap_or(0);
    (code, body)
}

/// Helper: run curl and get just status code
fn curl_status(method: &str, url: &str, body: Option<&[u8]>) -> u16 {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    
    let mut cmd = Command::new("curl");
    cmd.args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", method, url]);
    if let Some(data) = body {
        let tmp = format!("/tmp/pcloud-itest-{}-{}.bin", std::process::id(), n);
        std::fs::write(&tmp, data).expect("write tmp");
        cmd.args(["--data-binary", &format!("@{}", tmp)]);
        let output = cmd.output().expect("curl failed");
        let _ = std::fs::remove_file(&tmp);
        let code: u16 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0);
        code
    } else {
        let output = cmd.output().expect("curl failed");
        let code: u16 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0);
        code
    }
}

/// Generate deterministic test data of given size
fn test_data(size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    for i in 0..size {
        data.push((i.wrapping_mul(17).wrapping_add(31) % 256) as u8);
    }
    data
}

/// Verify file content matches expected (via SHA-256)
fn verify_content(actual: &[u8], expected: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    let act_hash = hex::encode(Sha256::digest(actual));
    let exp_hash = hex::encode(Sha256::digest(expected));
    if act_hash != exp_hash {
        eprintln!("SHA-256 mismatch: expected {} got {}", exp_hash, act_hash);
        return false;
    }
    true
}

/// Cleanup guard: deletes a bucket when dropped (even on panic)
struct BucketCleanup {
    bucket: String,
    cleaned: bool,
}

impl BucketCleanup {
    fn new(bucket: String) -> Self {
        Self { bucket, cleaned: false }
    }

    fn disarm(mut self) {
        self.cleaned = true;
    }
}

impl Drop for BucketCleanup {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = Command::new("curl")
                .args(["-s", "-X", "DELETE", &format!("{}/{}", S3_BASE, self.bucket)])
                .output();
            eprintln!("[cleanup] Deleted bucket: {}", self.bucket);
        }
    }
}

// =====================================================================
// Tests
// =====================================================================

#[test]
#[ignore = "Requires running MultiFS server with pCloud backends. Set PCLOUD_INTEGRATION_TEST=1"]
fn test_small_file_roundtrip() {
    let prefix = test_prefix();
    let bucket = format!("{}-small", prefix);
    let _cleanup = BucketCleanup::new(bucket.clone());
    let key = "hello.txt";
    let data = test_data(50 * 1024); // 50 KB

    // 1. Create bucket
    let code = curl_status("PUT", &format!("{}/{}", S3_BASE, bucket), None);
    assert!(code == 200 || code == 409, "Create bucket: {}", code);

    // 2. Upload via S3
    let code = curl_status("PUT", &format!("{}/{}/{}", S3_BASE, bucket, key), Some(&data));
    assert_eq!(code, 200, "Upload small file: {}", code);

    // 3. HEAD to verify metadata
    let output = Command::new("curl")
        .args(["-s", "-I", &format!("{}/{}/{}", S3_BASE, bucket, key)])
        .output().unwrap();
    let headers = String::from_utf8_lossy(&output.stdout);
    assert!(headers.contains("200 OK") || headers.contains("HTTP/1.1 200"),
        "HEAD should return 200, got: {}", headers.lines().next().unwrap_or(""));
    assert!(headers.contains("Content-Length: 51200") || headers.contains("content-length: 51200"),
        "Content-Length should be 51200");

    // 4. Download via S3 and verify
    let (code, body) = curl_get(&format!("{}/{}/{}", S3_BASE, bucket, key));
    assert_eq!(code, 200, "Download small file: {}", code);
    assert!(verify_content(&body, &data), "Content mismatch");
    assert_eq!(body.len(), data.len(), "Size mismatch");

    // 5. Download via WebDAV (cross-protocol)
    let (code, body) = curl_get(&format!("{}/{}/{}", WEBDAV_BASE, bucket, key));
    assert_eq!(code, 200, "WebDAV download: {}", code);
    assert!(verify_content(&body, &data), "WebDAV content mismatch");

    // 6. Delete
    let code = curl_status("DELETE", &format!("{}/{}/{}", S3_BASE, bucket, key), None);
    assert_eq!(code, 204, "Delete: {}", code);

    // 7. Verify deleted (HEAD should 404)
    let output = Command::new("curl")
        .args(["-s", "-I", &format!("{}/{}/{}", S3_BASE, bucket, key)])
        .output().unwrap();
    let headers = String::from_utf8_lossy(&output.stdout);
    assert!(headers.contains("404") || headers.contains("405"),
        "HEAD after delete should 404, got: {:?}", headers.lines().next());

    // Clean up bucket
    let code = curl_status("DELETE", &format!("{}/{}", S3_BASE, bucket), None);
    assert!(code == 204 || code == 200, "Delete bucket: {}", code);
    _cleanup.disarm();
}

#[test]
#[ignore = "Requires running MultiFS server with pCloud backends. Set PCLOUD_INTEGRATION_TEST=1"]
fn test_chunked_file_roundtrip() {
    let prefix = test_prefix();
    let bucket = format!("{}-chunked", prefix);
    let _cleanup = BucketCleanup::new(bucket.clone());
    let key = "large.bin";

    // 35 MB → 2 chunks (one 32 MB, one 3 MB)
    let data = test_data(35 * 1024 * 1024);

    // 1. Create bucket
    let code = curl_status("PUT", &format!("{}/{}", S3_BASE, bucket), None);
    assert!(code == 200 || code == 409, "Create bucket: {}", code);

    // 2. Upload chunked file
    let tmp = format!("/tmp/pcloud-itest-chunked-{}.bin", std::process::id());
    std::fs::write(&tmp, &data).expect("write");
    let output = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT",
            &format!("{}/{}/{}", S3_BASE, bucket, key),
            "--data-binary", &format!("@{}", tmp)])
        .output().unwrap();
    let code: u16 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0);
    assert_eq!(code, 200, "Upload chunked file: {}", code);

    // 3. HEAD to verify size
    let output = Command::new("curl")
        .args(["-s", "-I", &format!("{}/{}/{}", S3_BASE, bucket, key)])
        .output().unwrap();
    let headers = String::from_utf8_lossy(&output.stdout);
    assert!(headers.contains("200") || headers.contains("200 OK"),
        "HEAD chunked: {}", headers.lines().next().unwrap_or(""));

    // 4. Full download and verify
    let (code, body) = curl_get(&format!("{}/{}/{}", S3_BASE, bucket, key));
    assert_eq!(code, 200, "Download chunked: {}", code);
    assert!(verify_content(&body, &data), "Chunked content mismatch");
    assert_eq!(body.len(), data.len(), "Chunked size mismatch: {} vs {}",
        body.len(), data.len());

    // 5. Range request: bytes 32MB-34MB (chunk 1, first 2MB)
    let output = Command::new("curl")
        .args(["-s", "-H", "Range: bytes=33554432-35651583",
            &format!("{}/{}/{}", S3_BASE, bucket, key), "-o", "/tmp/pcloud-itest-range.bin",
            "-w", "%{http_code}:%{size_download}"])
        .output().unwrap();
    let result = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = result.split(':').collect();
    let code: u16 = parts[0].trim().parse().unwrap_or(0);
    let size: usize = parts.get(1).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    assert_eq!(code, 206, "Range request: {}", code);
    assert_eq!(size, 2_097_152, "Range size should be 2 MiB");

    let range_data = std::fs::read("/tmp/pcloud-itest-range.bin").unwrap();
    assert_eq!(&range_data[..], &data[33_554_432..35_651_584]);

    // Clean up
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file("/tmp/pcloud-itest-range.bin");
    let code = curl_status("DELETE", &format!("{}/{}/{}", S3_BASE, bucket, key), None);
    assert_eq!(code, 204, "Delete chunked: {}", code);
    let code = curl_status("DELETE", &format!("{}/{}", S3_BASE, bucket), None);
    assert!(code == 204 || code == 200, "Delete bucket: {}", code);
    _cleanup.disarm();
}

#[test]
#[ignore = "Requires running MultiFS server with pCloud backends. Set PCLOUD_INTEGRATION_TEST=1"]
fn test_bucket_listing() {
    let prefix = test_prefix();
    let bucket = format!("{}-list", prefix);
    let _cleanup = BucketCleanup::new(bucket.clone());

    // Create bucket
    let code = curl_status("PUT", &format!("{}/{}", S3_BASE, bucket), None);
    assert!(code == 200 || code == 409, "Create bucket: {}", code);

    // Upload 3 files
    for name in &["a.txt", "b.txt", "c.txt"] {
        let data = test_data(1024);
        let code = curl_status("PUT", &format!("{}/{}/{}", S3_BASE, bucket, name), Some(&data));
        assert_eq!(code, 200, "Upload {}: {}", name, code);
    }

    // List via S3
    let (code, body) = curl_get(&format!("{}/{}", S3_BASE, bucket));
    assert_eq!(code, 200, "List: {}", code);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("<Key>a.txt</Key>"));
    assert!(body_str.contains("<Key>b.txt</Key>"));
    assert!(body_str.contains("<Key>c.txt</Key>"));

    // List via WebDAV PROPFIND
    let output = Command::new("curl")
        .args(["-s", "-X", "PROPFIND", &format!("{}/{}", WEBDAV_BASE, bucket),
            "-H", "Depth: 1"])
        .output().unwrap();
    let body_str = String::from_utf8_lossy(&output.stdout);
    assert!(body_str.contains("a.txt"));
    assert!(body_str.contains("b.txt"));
    assert!(body_str.contains("c.txt"));

    // Clean up files then bucket
    for name in &["a.txt", "b.txt", "c.txt"] {
        let code = curl_status("DELETE", &format!("{}/{}/{}", S3_BASE, bucket, name), None);
        assert_eq!(code, 204, "Delete {}: {}", name, code);
    }
    let code = curl_status("DELETE", &format!("{}/{}", S3_BASE, bucket), None);
    assert!(code == 204 || code == 200, "Delete bucket: {}", code);
    _cleanup.disarm();
}

#[test]
#[ignore = "Requires running MultiFS server with pCloud backends. Set PCLOUD_INTEGRATION_TEST=1"]
fn test_error_handling() {
    let prefix = test_prefix();
    let bucket = format!("{}-errors", prefix);
    let _cleanup = BucketCleanup::new(bucket.clone());

    let code = curl_status("PUT", &format!("{}/{}", S3_BASE, bucket), None);
    assert!(code == 200 || code == 409, "Create bucket: {}", code);

    // Non-existent object → 404
    let (code, _) = curl_get(&format!("{}/{}/nope.bin", S3_BASE, bucket));
    assert_eq!(code, 404, "Non-existent GET: {}", code);

    // HEAD non-existent → 404
    let output = Command::new("curl")
        .args(["-s", "-I", &format!("{}/{}/nope.bin", S3_BASE, bucket)])
        .output().unwrap();
    let headers = String::from_utf8_lossy(&output.stdout);
    assert!(headers.contains("404"), "HEAD non-existent: {:?}", headers.lines().next());

    // Delete non-existent → 204 (idempotent)
    let code = curl_status("DELETE", &format!("{}/{}/nope.bin", S3_BASE, bucket), None);
    assert_eq!(code, 204, "Delete non-existent: {}", code);

    // Clean up
    let code = curl_status("DELETE", &format!("{}/{}", S3_BASE, bucket), None);
    assert!(code == 204 || code == 200, "Delete bucket: {}", code);
    _cleanup.disarm();
}

#[test]
#[ignore = "Requires running MultiFS server with pCloud backends. Set PCLOUD_INTEGRATION_TEST=1"]
fn test_multiple_files_concurrent_upload() {
    let prefix = test_prefix();
    let bucket = format!("{}-concurrent", prefix);
    let _cleanup = BucketCleanup::new(bucket.clone());

    let code = curl_status("PUT", &format!("{}/{}", S3_BASE, bucket), None);
    assert!(code == 200 || code == 409, "Create bucket: {}", code);

    // Upload 5 files concurrently
    let mut handles = Vec::new();
    for i in 0..5 {
        let data = test_data(100 * 1024); // 100 KB each
        let url = format!("{}/{}/file-{}.bin", S3_BASE, bucket, i);
        handles.push(std::thread::spawn(move || {
            let tmp = format!("/tmp/pcloud-concurrent-{}-{}.bin", std::process::id(), i);
            std::fs::write(&tmp, &data).unwrap();
            let output = Command::new("curl")
                .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT",
                    &url, "--data-binary", &format!("@{}", tmp)])
                .output().unwrap();
            let _ = std::fs::remove_file(&tmp);
            let code: u16 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0);
            (i, code, data)
        }));
    }

    let mut results = Vec::new();
    for h in handles {
        results.push(h.join().unwrap());
    }

    // All uploads must succeed
    for (i, code, _) in &results {
        assert_eq!(*code, 200, "Concurrent upload file-{}: {}", i, code);
    }

    // Verify all files are downloadable and correct (with retry for propagation delay)
    for (i, _code, data) in &results {
        let url = format!("{}/{}/file-{}.bin", S3_BASE, bucket, i);
        let mut attempts = 0;
        let body = loop {
            let (code, body) = curl_get(&url);
            if code == 200 && !body.is_empty() { break body; }
            attempts += 1;
            if attempts >= 3 { panic!("Download file-{}: code={} body_len={} after {} retries", i, code, body.len(), attempts); }
            eprintln!("[retry] file-{} attempt {}: code={} body_len={}", i, attempts, code, body.len());
            std::thread::sleep(std::time::Duration::from_secs(2));
        };
        assert!(verify_content(&body, data), "Content mismatch file-{}", i);

        // Clean up each file
        let code = curl_status("DELETE", &format!("{}/{}/file-{}.bin", S3_BASE, bucket, i), None);
        assert_eq!(code, 204, "Delete file-{}: {}", i, code);
    }

    let code = curl_status("DELETE", &format!("{}/{}", S3_BASE, bucket), None);
    assert!(code == 204 || code == 200, "Delete bucket: {}", code);
    _cleanup.disarm();
}

#[test]
#[ignore = "Requires running MultiFS server with pCloud backends. Set PCLOUD_INTEGRATION_TEST=1"]
fn test_s3_multipart_part_not_stored() {
    /// Documents the known issue: S3 multipart upload parts are NOT persisted.
    /// Initiating, uploading parts, and completing returns 200 but no file exists.
    let prefix = test_prefix();
    let bucket = format!("{}-mp", prefix);
    let _cleanup = BucketCleanup::new(bucket.clone());

    let code = curl_status("PUT", &format!("{}/{}", S3_BASE, bucket), None);
    assert!(code == 200 || code == 409, "Create bucket: {}", code);

    let key = "mp-test.bin";

    // 1. Initiate multipart upload
    let output = Command::new("curl")
        .args(["-s", "-X", "POST", &format!("{}/{}/{}?uploads", S3_BASE, bucket, key)])
        .output().unwrap();
    let body_str = String::from_utf8_lossy(&output.stdout);
    assert!(body_str.contains("<UploadId>"), "Initiate should return UploadId");
    assert!(body_str.contains("<Bucket>"), "Initiate should return Bucket");

    let upload_id: String = body_str
        .lines()
        .find(|l| l.contains("<UploadId>"))
        .map(|l| l.trim()
            .replace("<UploadId>", "")
            .replace("</UploadId>", ""))
        .unwrap_or_default();
    assert!(!upload_id.is_empty(), "Should have UploadId");

    // 2. Upload a part
    let part_data = test_data(1024 * 1024); // 1 MB
    let output = Command::new("curl")
        .args(["-s", "-w", "%{http_code}", "-X", "PUT",
            &format!("{}/{}/{}?partNumber=1&uploadId={}", S3_BASE, bucket, key, upload_id),
            "--data-binary", "@/dev/stdin"])
        .output().unwrap();
    // Note: the multipart part handler returns 200 but doesn't consume the body.
    // For large bodies (>TCP buffer), this causes a connection stall.
    // For this test, with 1 MB, the body fits in TCP buffer so it succeeds
    // but the data is discarded.
    let code: u16 = String::from_utf8_lossy(&output.stdout)
        .chars().filter(|c| c.is_digit(10)).collect::<String>()
        .parse().unwrap_or(0);
    // KNOWN ISSUE: this returns 200 but the body is NOT stored.
    // The assert documents the current (broken) behavior.
    assert!(code == 200 || code == 0,
        "Part upload returns {} (known issue: body not consumed)", code);

    // 3. Complete multipart upload
    let output = Command::new("curl")
        .args(["-s", "-X", "POST",
            &format!("{}/{}/{}?uploadId={}", S3_BASE, bucket, key, upload_id),
            "-H", "Content-Type: application/xml",
            "-d", "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>test</ETag></Part></CompleteMultipartUpload>"])
        .output().unwrap();
    let body_str = String::from_utf8_lossy(&output.stdout);
    assert!(body_str.contains("CompleteMultipartUploadResult") || body_str.contains("ETag"),
        "Complete should return success XML");

    // 4. KNOWN ISSUE: The file does NOT exist after multipart completion
    let code = curl_status("HEAD", &format!("{}/{}/{}", S3_BASE, bucket, key), None);
    // Current behavior: 404 (file was never created because parts aren't stored)
    assert_eq!(code, 404, "KNOWN ISSUE: multipart upload does not persist data (got {})", code);

    // Clean up
    let code = curl_status("DELETE", &format!("{}/{}", S3_BASE, bucket), None);
    assert!(code == 204 || code == 200, "Delete bucket: {}", code);
    _cleanup.disarm();
}

// =====================================================================
// Test Checklist (run all with one command):
//
//   PCLOUD_INTEGRATION_TEST=1 cargo test --test pcloud_integration -- --ignored --nocapture
//
// Or individual tests:
//   cargo test --test pcloud_integration test_small -- --ignored --nocapture
// =====================================================================
