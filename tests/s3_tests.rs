use std::process::Command;

const S3_URL: &str = "http://100.100.30.59:9000";

fn s3_put(path: &str, data: &str) -> Result<u16, String> {
    let output = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT",
               &format!("{}{}", S3_URL, path), "--data-binary", data])
        .output().map_err(|e| e.to_string())?;
    String::from_utf8_lossy(&output.stdout).trim().parse().map_err(|e: std::num::ParseIntError| e.to_string())
}

fn s3_get(path: &str) -> Result<String, String> {
    let output = Command::new("curl")
        .args(["-s", &format!("{}{}", S3_URL, path)])
        .output().map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn s3_delete(path: &str) -> Result<u16, String> {
    let output = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE",
               &format!("{}{}", S3_URL, path)])
        .output().map_err(|e| e.to_string())?;
    String::from_utf8_lossy(&output.stdout).trim().parse().map_err(|e: std::num::ParseIntError| e.to_string())
}

fn s3_head(path: &str) -> Result<u16, String> {
    let output = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-I",
               &format!("{}{}", S3_URL, path)])
        .output().map_err(|e| e.to_string())?;
    String::from_utf8_lossy(&output.stdout).trim().parse().map_err(|e: std::num::ParseIntError| e.to_string())
}

#[test]
fn test_s3_list_buckets() {
    let result = s3_get("/").unwrap();
    assert!(result.contains("ListAllMyBucketsResult"));
}

#[test]
fn test_s3_create_bucket() {
    let code = s3_put("/s3-integration-test", "").unwrap();
    assert_eq!(code, 200);
}

#[test]
fn test_s3_upload_and_download() {
    let _ = s3_put("/s3-integration-test", ""); // ensure bucket exists
    let code = s3_put("/s3-integration-test/hello.txt", "hello test").unwrap();
    assert_eq!(code, 200);

    let content = s3_get("/s3-integration-test/hello.txt").unwrap();
    assert_eq!(content, "hello test");
}

#[test]
fn test_s3_head_object() {
    let code = s3_head("/s3-integration-test/hello.txt").unwrap();
    assert_eq!(code, 200);
}

#[test]
fn test_s3_delete_object() {
    let _ = s3_put("/s3-integration-test/todelete.txt", "delete me");
    let code = s3_delete("/s3-integration-test/todelete.txt").unwrap();
    assert_eq!(code, 204);

    let head_code = s3_head("/s3-integration-test/todelete.txt").unwrap();
    assert_eq!(head_code, 404);
}

#[test]
fn test_s3_list_objects() {
    let result = s3_get("/s3-integration-test").unwrap();
    assert!(result.contains("ListBucketResult"));
    assert!(result.contains("hello.txt"));
}
