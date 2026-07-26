use std::process::Command;

const S3_BASE: &str = "http://100.100.30.59:9000";
const WEBDAV_BASE: &str = "http://100.100.30.59:8080";
const TEST_BUCKET: &str = "inttest-folder-listing";

fn curl_bytes(args: &[&str]) -> Vec<u8> {
    let output = Command::new("curl").args(args).output().expect("curl failed");
    assert!(output.status.success(), "curl failed: {}", String::from_utf8_lossy(&output.stderr));
    output.stdout
}

fn curl_status(args: &[&str]) -> u16 {
    let output = Command::new("curl").args(args).output().expect("curl failed");
    let code: u16 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0);
    if code == 0 && !output.status.success() {
        panic!("curl HTTP error: {}", String::from_utf8_lossy(&output.stderr));
    }
    code
}

fn setup_bucket(bucket: &str) {
    let code = curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT",
        &format!("{S3_BASE}/{bucket}")]);
    assert!(code == 200 || code == 409, "Create bucket returned {code}");
}

fn teardown_bucket(bucket: &str) {
    let body = curl_bytes(&["-s", &format!("{S3_BASE}/{bucket}?list-type=2")]);
    let body_str = String::from_utf8_lossy(&body);
    for key in body_str.split("Key>").skip(1) {
        if let Some(end) = key.find("</Key") {
            let k = &key[..end];
            curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE",
                &format!("{S3_BASE}/{bucket}/{k}")]);
        }
    }
    curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE",
        &format!("{S3_BASE}/{bucket}")]);
}

fn s3_put(bucket: &str, key: &str, data: &str) -> u16 {
    curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT",
        &format!("{S3_BASE}/{bucket}/{key}"), "-d", data])
}

// ============================================================
// Folder-style HTML listing tests
// ============================================================

#[test]
fn test_folder_listing_shows_folders_not_flat_files() {
    setup_bucket(TEST_BUCKET);

    assert_eq!(s3_put(TEST_BUCKET, "alpha/file1.txt", "alpha one"), 200);
    assert_eq!(s3_put(TEST_BUCKET, "alpha/file2.txt", "alpha two"), 200);
    assert_eq!(s3_put(TEST_BUCKET, "beta/file3.txt", "beta one"), 200);
    assert_eq!(s3_put(TEST_BUCKET, "rootfile.txt", "root"), 200);

    let body = curl_bytes(&["-s", &format!("{WEBDAV_BASE}/{TEST_BUCKET}/")]);
    let html = String::from_utf8_lossy(&body);

    assert!(html.contains("alpha/"), "Should show alpha/ folder entry");
    assert!(html.contains("beta/"), "Should show beta/ folder entry");
    assert!(html.contains("📁"), "Should use folder emoji");
    assert!(html.contains("rootfile.txt"), "Should show root-level file");
    assert!(!html.contains("alpha/file1.txt"), "Should NOT show flat path in bucket root");

    // Navigate into alpha/
    let body = curl_bytes(&["-s", &format!("{WEBDAV_BASE}/{TEST_BUCKET}/alpha/")]);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("file1.txt"), "alpha/ should show file1.txt");
    assert!(html.contains("file2.txt"), "alpha/ should show file2.txt");
    assert!(!html.contains("beta"), "alpha/ should not show beta files");

    teardown_bucket(TEST_BUCKET);
}

#[test]
fn test_folder_listing_empty_bucket_shows_no_files() {
    setup_bucket(TEST_BUCKET);
    let body = curl_bytes(&["-s", &format!("{WEBDAV_BASE}/{TEST_BUCKET}/")]);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("No files"), "Empty bucket shows 'No files'");
    teardown_bucket(TEST_BUCKET);
}

#[test]
fn test_folder_listing_nested_subdirs() {
    setup_bucket(TEST_BUCKET);

    assert_eq!(s3_put(TEST_BUCKET, "a/b/c/deep.txt", "deep"), 200);
    assert_eq!(s3_put(TEST_BUCKET, "a/b/shallow.txt", "shallow"), 200);
    assert_eq!(s3_put(TEST_BUCKET, "a/x/file.txt", "other"), 200);

    let body = curl_bytes(&["-s", &format!("{WEBDAV_BASE}/{TEST_BUCKET}/")]);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("a/"), "Root shows a/ folder");

    let body = curl_bytes(&["-s", &format!("{WEBDAV_BASE}/{TEST_BUCKET}/a/")]);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("b/"), "a/ shows b/ folder");
    assert!(html.contains("x/"), "a/ shows x/ folder");

    teardown_bucket(TEST_BUCKET);
}

#[test]
fn test_folder_listing_existing_video_folders() {
    let body = curl_bytes(&["-s", &format!("{WEBDAV_BASE}/video/")]);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("BLOR-074/"), "video/ shows BLOR-074/");
    assert!(html.contains("BLOR-085/"), "video/ shows BLOR-085/");
    assert!(html.contains("BLOR-162/"), "video/ shows BLOR-162/");

    let body = curl_bytes(&["-s", &format!("{WEBDAV_BASE}/video/BLOR-074/")]);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("blor-074.mkv"), "BLOR-074 has mkv");
    assert!(html.contains("blor-074.zh.srt"), "BLOR-074 has .zh.srt");

    let body = curl_bytes(&["-s", &format!("{WEBDAV_BASE}/video/BLOR-085/")]);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("blor-085.mkv"), "BLOR-085 has mkv");
}

#[test]
fn test_folder_listing_streaming_works() {
    // Verify streaming: the mkv should stream properly
    let body = curl_bytes(&["-s", "-o", "/dev/null", "-w", "%{http_code}",
        &format!("{WEBDAV_BASE}/video/BLOR-074/blor-074.mkv")]);
    // HTTP 200 is OK; we don't want to download the whole 743MB file
    // Just check first bytes via Range
    let headers = Command::new("curl")
        .args(&["-s", "-I", &format!("{WEBDAV_BASE}/video/BLOR-074/blor-074.mkv")])
        .output().expect("curl failed");
    let head = String::from_utf8_lossy(&headers.stdout);
    assert!(head.contains("200"), "HEAD should return 200");
    assert!(head.contains("Accept-Ranges"), "Should support range requests");
}

// ============================================================
// CLI command tests
// ============================================================

#[test]
fn test_cli_account_help() {
    let output = Command::new("multifs").arg("account").arg("--help").output().expect("multifs failed");
    assert!(output.status.success());
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("add"), "Help shows add: got:\n{out}");
    assert!(out.contains("remove"), "Help shows remove");
    assert!(out.contains("list"), "Help shows list");
    assert!(out.contains("check"), "Help shows check");
}

#[test]
fn test_cli_audit_help() {
    let output = Command::new("multifs").arg("audit").arg("--help").output().expect("multifs failed");
    assert!(output.status.success());
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("scan"), "Audit help shows scan: got:\n{out}");
    assert!(out.contains("list-files"), "Audit help shows list-files");
}

#[test]
fn test_cli_account_list() {
    let output = Command::new("multifs").args(["account", "list"]).output().expect("multifs failed");
    let out = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        assert!(
            out.contains("No accounts") || err.contains("config"),
            "Unexpected fail: {err}"
        );
    } else {
        assert!(
            out.contains("nova") || out.contains("Email"),
            "Account list should show accounts, got:\n{out}"
        );
    }
}

#[test]
fn test_cli_add_help_shows_token_param() {
    let output = Command::new("multifs").args(["account", "add", "--help"]).output().expect("multifs failed");
    assert!(output.status.success());
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("TOKEN"), "Add help should mention TOKEN arg");
}
