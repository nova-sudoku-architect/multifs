//! S3 integration tests covering previously-failed scenarios.
//!
//! These tests reproduce the bugs found during the empty-body/EOF investigation
//! and the S3 multipart MD5-ETag fix:
//!
//!   1. Whole-file GET must return the actual bytes (not an empty 200 + EOF).
//!   2. Chunked (large, >32 MiB) GET must return the full byte range.
//!   3. Multipart UploadPart must return an MD5-based part ETag, and
//!      CompleteMultipartUpload must return the S3 standard multipart ETag
//!      (MD5 of concatenated per-part MD5s), so rclone verifies without retry churn.
//!   4. ListObjects must include both whole-file and chunked objects.
//!
//! Run against a LIVE MultiFS S3 gateway (port 9000):
//!
//!     cargo test --test s3_failed_scenarios -- --nocapture
//!
//! All temp data lives under a unique `/tmp/inttest-s3fs-<pid>-<ts>/` dir and a
//! unique bucket, and is removed in teardown (Drop guard) even on panic/failure.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const S3_BASE: &str = "http://100.100.30.59:9000";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Unique test bucket, e.g. `inttest-s3fs-<pid>-<nanos>`. Nanos avoids collisions
/// when tests from the same process run within the same wall-clock second.
fn bucket() -> String {
    let nano = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("inttest-s3fs-{}-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed), nano)
}

/// Temp dir under /tmp, unique per process+test run.
fn tmp_dir() -> std::path::PathBuf {
    let nano = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    std::path::Path::new("/tmp").join(format!(
        "inttest-s3fs-{}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        nano
    ))
}

/// Teardown guard: deletes the temp dir and the test bucket's objects + the
/// bucket itself when dropped (runs on both success and panic via Drop).
struct Cleanup {
    tmp: Option<std::path::PathBuf>,
    bucket: Option<String>,
    objects: Vec<String>,
}
impl Cleanup {
    fn new(tmp: std::path::PathBuf, bucket: String) -> Self {
        Cleanup { tmp: Some(tmp), bucket: Some(bucket), objects: Vec::new() }
    }
    fn add_object(&mut self, key: &str) {
        let b = self.bucket.clone().unwrap();
        self.objects.push(format!("{b}/{key}"));
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        // 1. Delete the test objects we created.
        for obj in &self.objects {
            curl_delete(obj);
        }
        // 2. Delete the bucket itself (force — deletes any leftovers).
        if let Some(b) = &self.bucket {
            curl_delete(&format!("{S3_BASE}/{b}"));
        }
        // 3. Remove the temp dir + any test files copied under /tmp.
        if let Some(t) = &self.tmp {
            let _ = std::fs::remove_dir_all(t);
        }
    }
}

// ---------------------------------------------------------------------------
// curl helpers (like tests/s3_tests.rs)
// ---------------------------------------------------------------------------
fn curl_put(url: &str, data: &[u8]) -> u16 {
    use std::io::Write;
    let mut child = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT", url, "--data-binary", "@-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn curl PUT");
    child.stdin.as_mut().unwrap().write_all(data).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

fn curl_post(url: &str, data: &[u8]) -> String {
    use std::io::Write;
    let mut child = Command::new("curl")
        .args(["-s", "-X", "POST", url, "--data-binary", "@-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn curl POST");
    child.stdin.as_mut().unwrap().write_all(data).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// GET body as bytes.
fn curl_get_body(url: &str) -> Vec<u8> {
    let out = Command::new("curl")
        .args(["-s", url])
        .output()
        .expect("curl GET failed");
    out.stdout
}

/// HEAD status code.
fn curl_head(url: &str) -> u16 {
    let out = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-I", url])
        .output()
        .expect("curl HEAD failed");
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

/// DELETE status code.
fn curl_delete(url: &str) -> u16 {
    let out = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE", url])
        .output()
        .expect("curl DELETE failed");
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

/// Verify a GET returns the exact expected bytes (not an empty 200 + EOF).
fn assert_get_returns(url: &str, expected: &[u8], what: &str) {
    let body = curl_get_body(url);
    assert_eq!(
        body, expected,
        "{what}: GET {url} did not return expected bytes (got {} bytes, expected {})",
        body.len(), expected.len()
    );
}

/// Verify a HEAD returns 200 (object metadata resolvable).
fn assert_head_ok(url: &str, what: &str) {
    let code = curl_head(url);
    assert_eq!(code, 200, "{what}: HEAD {url} expected 200, got {code}");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test 1: Whole-file PUT → GET returns the exact bytes.
/// Regression for: rclone cat returned 0 bytes (empty 200 + EOF) on whole-file reads.
#[test]
fn whole_file_roundtrip() {
    let b = bucket();
    let tmp = tmp_dir();
    std::fs::create_dir_all(&tmp).unwrap();
    let mut cleanup = Cleanup::new(tmp.clone(), b.clone());
    cleanup.add_object("hello.txt");
    curl_put(&format!("{S3_BASE}/{b}"), b""); // create bucket
    curl_put(&format!("{S3_BASE}/{b}/hello.txt"), b"hello s3fs whole-file");

    assert_get_returns(&format!("{S3_BASE}/{b}/hello.txt"), b"hello s3fs whole-file", "whole-file GET");
    assert_head_ok(&format!("{S3_BASE}/{b}/hello.txt"), "whole-file HEAD");
}

/// Test 2: Chunked (large >32 MiB) PUT → GET returns the full bytes.
/// Regression for: large-object GET returned an empty body on the read path.
#[test]
fn chunked_roundtrip() {
    let b = bucket();
    let tmp = tmp_dir();
    std::fs::create_dir_all(&tmp).unwrap();
    let mut cleanup = Cleanup::new(tmp.clone(), b.clone());
    cleanup.add_object("large.bin");
    curl_put(&format!("{S3_BASE}/{b}"), b""); // create bucket

    // Build a 33 MiB deterministic payload (> 32 MiB chunk threshold).
    let size = 33 * 1024 * 1024 + 12345;
    let payload: Vec<u8> = (0..size).map(|i| ((i * 13 + 7) % 256) as u8).collect();
    curl_put(&format!("{S3_BASE}/{b}/large.bin"), &payload);

    assert_get_returns(&format!("{S3_BASE}/{b}/large.bin"), &payload, "chunked GET (33 MiB)");
    let code = curl_head(&format!("{S3_BASE}/{b}/large.bin"));
    assert_eq!(code, 200, "chunked HEAD expected 200, got {code}");
}

/// Test 3: Multipart round-trip — part ETag must be MD5-based and the assembled
/// object must match the uploaded parts. Regression for the SHA256-part-ETag bug
/// that caused rclone checksum/verify retry churn.
#[test]
fn multipart_etag_and_roundtrip() {
    let b = bucket();
    let tmp = tmp_dir();
    std::fs::create_dir_all(&tmp).unwrap();
    let mut cleanup = Cleanup::new(tmp.clone(), b.clone());
    cleanup.add_object("mp.bin");
    curl_put(&format!("{S3_BASE}/{b}"), b""); // create bucket

    let payload: Vec<u8> = (0..1024 * 1024).map(|i| ((i * 7 + 3) % 256) as u8).collect(); // 1 MiB
    let half = payload.len() / 2;
    let part1 = &payload[..half];
    let part2 = &payload[half..];

    // Initiate
    let init = curl_post(&format!("{S3_BASE}/{b}/mp.bin?uploads"), b"");
    let upload_id = init
        .split("<UploadId>")
        .nth(1)
        .and_then(|s| s.split("</UploadId>").next())
        .expect("multipart initiate returned UploadId")
        .to_string();

    // Upload part 1, capture its ETag.
    let r1 = Command::new("curl")
        .args(["-s", "-D", "-", "-o", "/dev/null",
               "-X", "PUT", &format!("{S3_BASE}/{b}/mp.bin?partNumber=1&uploadId={upload_id}"),
               "--data-binary", "@-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn().expect("spawn part1");
    {
        use std::io::Write;
        r1.stdin.as_ref().unwrap().write_all(part1).unwrap();
    }
    let out = r1.wait_with_output().unwrap();
    let headers1 = String::from_utf8_lossy(&out.stdout).to_string();
    let etag1 = headers1
        .lines().find(|l| l.to_lowercase().starts_with("etag:"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| s.trim().trim_matches('"').to_string())
        .unwrap_or_default();

    // Part ETag must be the MD5 hex of the part (32 hex chars), NOT a SHA256 prefix (first 16 chars of 64).
    let expected_md5_part1 = md5_hex(part1);
    assert_eq!(
        etag1.to_lowercase(), expected_md5_part1,
        "UploadPart ETag must be MD5(part bytes), got {etag1}"
    );

    // Upload part 2.
    {
        use std::io::Write;
        let r2 = Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}",
                   "-X", "PUT", &format!("{S3_BASE}/{b}/mp.bin?partNumber=2&uploadId={upload_id}"),
                   "--data-binary", "@-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn().expect("spawn part2");
        r2.stdin.as_ref().unwrap().write_all(part2).unwrap();
        let out = r2.wait_with_output().unwrap();
        let code: u16 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0);
        assert_eq!(code, 200, "UploadPart 2 expected 200, got {code}");
    }

    // Complete → capture the multipart ETag (must be MD5 of concat of part MD5 bins).
    let comp = curl_post(&format!("{S3_BASE}/{b}/mp.bin?uploadId={upload_id}"), b"");
    assert!(
        comp.contains("CompleteMultipartUploadResult"),
        "Complete returned result XML, got: {comp}"
    );
    let comp_etag = comp
        .lines().find(|l| l.contains("<ETag>"))
        .and_then(|l| l.split("<ETag>").nth(1))
        .and_then(|s| s.split("</ETag>").next())
        .map(|s| s.replace("&quot;", "").to_string())
        .unwrap_or_default();

    // Expected S3 multipart ETag: MD5( MD5(part1).bin || MD5(part2).bin ).
    let expected = multipart_etag(&[part1, part2]);
    assert_eq!(
        comp_etag.to_lowercase(), expected,
        "CompleteMultipartUpload ETag must be MD5-of-part-MD5s, got {comp_etag}"
    );

    // Download must equal the full payload.
    assert_get_returns(&format!("{S3_BASE}/{b}/mp.bin"), &payload, "multipart GET");
}

/// Test 4: ListObjects must include both whole-file and chunked objects.
#[test]
fn list_includes_both_storage_types() {
    let b = bucket();
    let tmp = tmp_dir();
    std::fs::create_dir_all(&tmp).unwrap();
    let mut cleanup = Cleanup::new(tmp.clone(), b.clone());
    curl_put(&format!("{S3_BASE}/{b}"), b""); // create bucket

    // Small whole-file object.
    cleanup.add_object("small.txt");
    curl_put(&format!("{S3_BASE}/{b}/small.txt"), b"small");

    // Large chunked object (>32 MiB).
    cleanup.add_object("large.bin");
    let payload: Vec<u8> = (0..33 * 1024 * 1024).map(|i| ((i * 3 + 1) % 256) as u8).collect();
    curl_put(&format!("{S3_BASE}/{b}/large.bin"), &payload);

    let listing = curl_get_body(&format!("{S3_BASE}/{b}"));
    let listing = String::from_utf8_lossy(&listing).to_string();
    assert!(listing.contains("small.txt"), "ListObjects missing whole-file object: {listing}");
    assert!(listing.contains("large.bin"), "ListObjects missing chunked object: {listing}");
}

// ---------------------------------------------------------------------------
// Helpers (pure, deterministic — no external deps beyond the md-5 crate)
// ---------------------------------------------------------------------------
/// Hex MD5 of data, lowercased.
fn md5_hex(data: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(data);
    let out = h.finalize();
    out.iter().map(|b| format!("{:02x}", b)).collect()
}

/// S3 multipart ETag: hex(MD5( MD5(p1) || MD5(p2) || ... ))
fn multipart_etag(parts: &[&[u8]]) -> String {
    use md5::{Digest, Md5};
    let mut concat: Vec<u8> = Vec::new();
    for p in parts {
        let mut h = Md5::new();
        h.update(p);
        concat.extend_from_slice(&h.finalize()[..]);
    }
    md5_hex(&concat)
}
