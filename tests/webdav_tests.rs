use std::process::Command;

const S3_BASE: &str = "http://100.100.30.59:9000";
const WEBDAV_BASE: &str = "http://100.100.30.59:8080";

fn curl_bytes(args: &[&str]) -> Vec<u8> {
    let output = Command::new("curl")
        .args(args)
        .output()
        .expect("curl failed");
    assert!(output.status.success(), "curl failed (HTTP {})", String::from_utf8_lossy(&output.stderr));
    output.stdout
}

fn curl_status(args: &[&str]) -> u16 {
    let output = Command::new("curl")
        .args(args)
        .output()
        .expect("curl failed");
    let code: u16 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0);
    if code == 0 && !output.status.success() {
        panic!("curl HTTP error: {}", String::from_utf8_lossy(&output.stderr));
    }
    code
}

#[test]
fn test_webdav_root_propfind() {
    let body = curl_bytes(&["-s", "-X", "PROPFIND", &format!("{}/", WEBDAV_BASE), "-H", "Depth: 1"]);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("multistatus"), "Expected XML multistatus");
    assert!(body_str.contains("/video/"), "Expected video bucket");
}

#[test]
fn test_webdav_blor_folder() {
    let body = curl_bytes(&["-s", "-X", "PROPFIND",
        &format!("{}/video/BLOR-074/", WEBDAV_BASE), "-H", "Depth: 1"]);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("blor-074.zh.srt"), "Missing subtitle file");
    assert!(body_str.contains("blor-074.mkv"), "Missing MKV");
    assert!(body_str.contains("PLAN.md"), "Missing PLAN.md");
}

#[test]
fn test_webdav_upload_and_download() {
    let bucket = "dav-integration-test";
    let remote = format!("{}/{}/file.txt", WEBDAV_BASE, bucket);
    let put_bytes = b"hello dav";

    let url_create = format!("{S3_BASE}/{bucket}");
    let url_listing = format!("{}/{}/", WEBDAV_BASE, bucket);
    let url_delete = format!("{S3_BASE}/{bucket}");

    curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT", &url_create]);

    let code = curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}",
        "-X", "PUT", &remote, "--data-binary", &String::from_utf8_lossy(put_bytes)]);
    assert_eq!(code, 201, "PUT returned {}", code);

    let body = curl_bytes(&["-s", &remote]);
    assert_eq!(&body, put_bytes, "Downloaded content mismatch");

    let prop = curl_bytes(&["-s", "-X", "PROPFIND", &url_listing, "-H", "Depth: 1"]);
    let prop_str = String::from_utf8_lossy(&prop);
    assert!(prop_str.contains("file.txt"), "PROPFIND should list file.txt");

    curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE", &remote]);
    curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE", &url_delete]);
}

#[test]
fn test_webdav_mkcol() {
    let bucket = "dav-mkcol-test";
    let url_create = format!("{}/{}", WEBDAV_BASE, bucket);
    let url_delete = format!("{S3_BASE}/{bucket}");

    let code = curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}",
        "-X", "MKCOL", &url_create]);
    assert_eq!(code, 201, "MKCOL returned {}", code);

    let prop = curl_bytes(&["-s", "-X", "PROPFIND", &format!("{}/", WEBDAV_BASE), "-H", "Depth: 1"]);
    let prop_str = String::from_utf8_lossy(&prop);
    assert!(prop_str.contains(bucket), "PROPFIND should list new bucket");

    curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE", &url_delete]);
}

#[test]
fn test_webdav_delete() {
    let bucket = "dav-delete-test";
    let remote = format!("{}/{}/todelete.txt", WEBDAV_BASE, bucket);
    let url_create = format!("{S3_BASE}/{bucket}");
    let url_delete = format!("{S3_BASE}/{bucket}");

    curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT", &url_create]);
    curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}",
        "-X", "PUT", &remote, "-d", "delete me"]);

    let code = curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}",
        "-X", "DELETE", &remote]);
    assert_eq!(code, 204, "DELETE returned {}", code);

    let body = curl_bytes(&["-s", "-w", "\n%{http_code}", &remote]);
    let lines: Vec<&str> = std::str::from_utf8(&body).unwrap().lines().collect();
    let status = lines.last().unwrap_or(&"");
    assert_eq!(*status, "404", "Expected 404 after delete, got {}", status);

    curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE", &url_delete]);
}

#[test]
fn test_webdav_full_pipeline_with_generated_files() {
    let bucket = "inttest-dav-074";
    let folder = "generated";
    let small_file = "small-data.bin";
    let large_file = "large-data.bin";

    let url_create = format!("{S3_BASE}/{bucket}");
    let url_small_s3 = format!("{S3_BASE}/{bucket}/{folder}/{small_file}");
    let url_large_s3 = format!("{S3_BASE}/{bucket}/{folder}/{large_file}");
    let url_listing = format!("{WEBDAV_BASE}/{bucket}/{folder}/");
    let url_small_wd = format!("{WEBDAV_BASE}/{bucket}/{folder}/{small_file}");
    let url_large_wd = format!("{WEBDAV_BASE}/{bucket}/{folder}/{large_file}");

    let code = curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT", &url_create]);
    assert!(code == 200 || code == 409, "Create bucket returned {}", code);

    // Generate small file: 128 KB
    let small_bytes: Vec<u8> = (0..128 * 1024).map(|i| (i ^ (i >> 8)) as u8).collect();
    std::fs::write("/tmp/inttest-small.bin", &small_bytes).expect("write small");

    let shasum = Command::new("sh")
        .arg("-c")
        .arg("sha256sum /tmp/inttest-small.bin | awk '{print $1}'")
        .output().expect("sha256sum");
    let small_hash = String::from_utf8_lossy(&shasum.stdout).trim().to_string();

    let code = curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT",
        &url_small_s3, "--data-binary", "@/tmp/inttest-small.bin"]);
    assert_eq!(code, 200, "Upload small file returned {}", code);

    // Generate large file: 33 MB (over chunk threshold)
    let large_bytes: Vec<u8> = (0..33 * 1024 * 1024).map(|i| ((i * 17 + 31) % 256) as u8).collect();
    std::fs::write("/tmp/inttest-large.bin", &large_bytes).expect("write large");

    let shasum = Command::new("sh")
        .arg("-c")
        .arg("sha256sum /tmp/inttest-large.bin | awk '{print $1}'")
        .output().expect("sha256sum");
    let large_hash = String::from_utf8_lossy(&shasum.stdout).trim().to_string();

    let code = curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT",
        &url_large_s3, "--data-binary", "@/tmp/inttest-large.bin"]);
    assert_eq!(code, 200, "Upload large file returned {}", code);

    // List via WebDAV PROPFIND
    let body = curl_bytes(&["-s", "-X", "PROPFIND", &url_listing, "-H", "Depth: 1"]);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains(small_file), "PROPFIND missing small file");
    assert!(body_str.contains(large_file), "PROPFIND missing large file");
    assert!(body_str.contains("getcontentlength"), "PROPFIND missing sizes");

    // Download small file via WebDAV
    let body = curl_bytes(&["-s", &url_small_wd]);
    assert_eq!(body.len(), 128 * 1024, "Small file size mismatch");
    assert_eq!(&body[..], &small_bytes[..], "Small file content mismatch");

    std::fs::write("/tmp/inttest-small-dl.bin", &body).expect("write dl");
    let verify = Command::new("sh")
        .arg("-c")
        .arg("sha256sum /tmp/inttest-small-dl.bin | awk '{print $1}'")
        .output().expect("sha256sum");
    let dl_hash = String::from_utf8_lossy(&verify.stdout).trim().to_string();
    assert_eq!(dl_hash, small_hash, "Small file SHA256 mismatch");

    // Download large file range + full
    let body = curl_bytes(&["-s", "-H", "Range: bytes=0-99", &url_large_wd]);
    assert_eq!(body.len(), 100, "Range should return 100 bytes");
    assert_eq!(&body[..], &large_bytes[..100], "Large file first bytes mismatch");

    let body = curl_bytes(&["-s", &url_large_wd]);
    assert_eq!(body.len(), 33 * 1024 * 1024, "Large file size mismatch");

    std::fs::write("/tmp/inttest-large-dl.bin", &body).expect("write dl");
    let verify = Command::new("sh")
        .arg("-c")
        .arg("sha256sum /tmp/inttest-large-dl.bin | awk '{print $1}'")
        .output().expect("sha256sum");
    let dl_hash = String::from_utf8_lossy(&verify.stdout).trim().to_string();
    assert_eq!(dl_hash, large_hash, "Large file SHA256 mismatch");

    // Clean up
    let _ = curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE", &url_small_s3]);
    let _ = curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE", &url_large_s3]);
    let _ = curl_status(&["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE", &url_create]);

    for f in &["/tmp/inttest-small.bin", "/tmp/inttest-large.bin",
               "/tmp/inttest-small-dl.bin", "/tmp/inttest-large-dl.bin"] {
        let _ = std::fs::remove_file(f);
    }
}
