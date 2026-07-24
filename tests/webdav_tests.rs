use std::process::Command;

const WEBDAV_URL: &str = "http://100.100.30.59:8080";

fn dav_put(path: &str, data: &str) -> Result<u16, String> {
    let output = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT",
               &format!("{}{}", WEBDAV_URL, path), "--data-binary", data])
        .output().map_err(|e| e.to_string())?;
    String::from_utf8_lossy(&output.stdout).trim().parse().map_err(|e| e.to_string())
}

fn dav_get(path: &str) -> Result<String, String> {
    let output = Command::new("curl")
        .args(["-s", &format!("{}{}", WEBDAV_URL, path)])
        .output().map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn dav_delete(path: &str) -> Result<u16, String> {
    let output = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE",
               &format!("{}{}", WEBDAV_URL, path)])
        .output().map_err(|e| e.to_string())?;
    String::from_utf8_lossy(&output.stdout).trim().parse().map_err(|e| e.to_string())
}

fn dav_options(path: &str) -> Result<u16, String> {
    let output = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "OPTIONS",
               &format!("{}{}", WEBDAV_URL, path)])
        .output().map_err(|e| e.to_string())?;
    String::from_utf8_lossy(&output.stdout).trim().parse().map_err(|e| e.to_string())
}

#[test]
fn test_webdav_options() {
    let code = dav_options("/").unwrap();
    assert_eq!(code, 200);
}

#[test]
fn test_webdav_upload_and_download() {
    let code = dav_put("/dav-integration-test/file.txt", "hello dav").unwrap();
    assert_eq!(code, 201);

    let content = dav_get("/dav-integration-test/file.txt").unwrap();
    assert_eq!(content, "hello dav");
}

#[test]
fn test_webdav_delete() {
    let _ = dav_put("/dav-integration-test/todelete.txt", "delete me");
    let code = dav_delete("/dav-integration-test/todelete.txt").unwrap();
    assert_eq!(code, 204);

    // Verify it's gone
    let content = dav_get("/dav-integration-test/todelete.txt").unwrap();
    assert!(content.is_empty() || content.contains("Error"));
}

#[test]
fn test_webdav_mkcol() {
    let output = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "MKCOL",
               &format!("{}/{}", WEBDAV_URL, "dav-new-bucket")])
        .output().map_err(|e| e.to_string()).unwrap();
    let code = String::from_utf8_lossy(&output.stdout).trim().parse::<u16>().unwrap();
    assert_eq!(code, 201);
}
