//! S3 request authentication — AWS Signature Version 4 (SigV4).
//!
//! Implements header-based SigV4 verification for the S3 API:
//!
//! 1. Parse the `Authorization: AWS4-HMAC-SHA256 Credential=…/date/region/service/aws4_request,
//!    SignedHeaders=…, Signature=…` header.
//! 2. Look the secret up by access-key id.
//! 3. Rebuild the canonical request from the **actual** request (only the headers
//!    the client signed) and recompute the signature with the derived signing key.
//! 4. Constant-time compare, plus an `x-amz-date` clock-skew window.
//!
//! Dependency-light: `hmac` + the existing `sha2`/`hex`, no AWS SDK.
//!
//! **Safe default:** when no access keys are configured the middleware is a
//! pass-through (the endpoint is unauthenticated — for local/dev use). Configure
//! keys to require signing.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, Method, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Allowed clock skew for `x-amz-date`, in seconds (AWS allows 15 minutes).
const MAX_CLOCK_SKEW_SECS: i64 = 900;

/// A parsed access key pair (`ACCESS_KEY_ID` → secret).
#[derive(Clone)]
pub struct AccessKeys {
    keys: BTreeMap<String, String>,
}

impl AccessKeys {
    /// Parse a `AKID:SECRET,AKID2:SECRET2` list. Whitespace around entries and
    /// fields is trimmed; empty entries are ignored. A key without a colon or
    /// with an empty id/secret is a hard error (fail loudly rather than silently
    /// disabling auth).
    pub fn parse(spec: &str) -> anyhow::Result<Self> {
        let mut keys = BTreeMap::new();
        for raw in spec.split(',') {
            let entry = raw.trim();
            if entry.is_empty() {
                continue;
            }
            let (id, secret) = entry
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("access key entry '{entry}' must be AKID:SECRET"))?;
            let (id, secret) = (id.trim(), secret.trim());
            if id.is_empty() || secret.is_empty() {
                anyhow::bail!("access key entry '{entry}' has an empty id or secret");
            }
            keys.insert(id.to_string(), secret.to_string());
        }
        Ok(Self { keys })
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn secret_for(&self, id: &str) -> Option<&str> {
        self.keys.get(id).map(|s| s.as_str())
    }
}

/// Authentication mode for the S3 endpoint.
#[derive(Clone)]
pub enum AuthMode {
    /// No keys configured → accept every request (unauthenticated; local/dev).
    Disabled,
    /// Require a valid SigV4 signature from one of these keys.
    SigV4(Arc<AccessKeys>),
}

impl AuthMode {
    /// Build from a raw `access_keys` config string.
    pub fn from_config(spec: &str) -> anyhow::Result<Self> {
        let keys = AccessKeys::parse(spec)?;
        if keys.is_empty() {
            Ok(AuthMode::Disabled)
        } else {
            Ok(AuthMode::SigV4(Arc::new(keys)))
        }
    }

    pub fn is_enabled(&self) -> bool {
        matches!(self, AuthMode::SigV4(_))
    }
}

/// Why a request was rejected (maps to an S3 error code + HTTP status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// No/invalid `Authorization` header, or unknown access key.
    AccessDenied(String),
    /// Signature computed by the client doesn't match ours.
    SignatureDoesNotMatch(String),
    /// `x-amz-date` outside the skew window.
    RequestTimeTooSkewed(String),
    /// `Authorization` header present but malformed / unsupported scheme.
    InvalidRequest(String),
}

impl AuthError {
    /// The S3 `<Code>` value.
    pub fn code(&self) -> &'static str {
        match self {
            AuthError::AccessDenied(_) => "AccessDenied",
            AuthError::SignatureDoesNotMatch(_) => "SignatureDoesNotMatch",
            AuthError::RequestTimeTooSkewed(_) => "RequestTimeTooSkewed",
            AuthError::InvalidRequest(_) => "InvalidRequest",
        }
    }

    /// The HTTP status.
    pub fn status(&self) -> StatusCode {
        match self {
            AuthError::AccessDenied(_) => StatusCode::FORBIDDEN,
            AuthError::SignatureDoesNotMatch(_) => StatusCode::FORBIDDEN,
            AuthError::RequestTimeTooSkewed(_) => StatusCode::FORBIDDEN,
            AuthError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            AuthError::AccessDenied(m)
            | AuthError::SignatureDoesNotMatch(m)
            | AuthError::RequestTimeTooSkewed(m)
            | AuthError::InvalidRequest(m) => m,
        }
    }
}

// ---------------------------------------------------------------------------
// SigV4 primitives
// ---------------------------------------------------------------------------

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// `kSigning = HMAC(HMAC(HMAC(HMAC("AWS4"+secret, date), region), service), "aws4_request")`.
fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// Constant-time byte comparison.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Canonical query string: sort the raw `k=v` pairs by key then value and rejoin.
/// We use the wire form as-is (clients sign what they send) — the pragmatic,
/// real-client-compatible approach.
fn canonical_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (p.to_string(), String::new()),
        })
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Trim and collapse runs of whitespace in a header value (SigV4 rule).
fn canonical_header_value(v: &str) -> String {
    v.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Split an `Authorization` header into (credential-scope fields, signed headers, signature).
struct ParsedAuth {
    access_key_id: String,
    date: String,      // YYYYMMDD
    region: String,
    service: String,
    signed_headers: Vec<String>,
    signature: String,
}

fn parse_authorization(header: &str) -> Result<ParsedAuth, AuthError> {
    let rest = header
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .ok_or_else(|| AuthError::InvalidRequest("unsupported authorization scheme".into()))?;

    let mut credential = None;
    let mut signed_headers = None;
    let mut signature = None;
    for part in rest.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("Credential=") {
            credential = Some(v.trim().to_string());
        } else if let Some(v) = part.strip_prefix("SignedHeaders=") {
            signed_headers = Some(v.trim().to_string());
        } else if let Some(v) = part.strip_prefix("Signature=") {
            signature = Some(v.trim().to_string());
        }
    }

    let credential = credential
        .ok_or_else(|| AuthError::InvalidRequest("missing Credential".into()))?;
    let signed_headers = signed_headers
        .ok_or_else(|| AuthError::InvalidRequest("missing SignedHeaders".into()))?;
    let signature = signature
        .ok_or_else(|| AuthError::InvalidRequest("missing Signature".into()))?;

    // Credential = AKID/date/region/service/aws4_request
    let fields: Vec<&str> = credential.split('/').collect();
    if fields.len() != 5 || fields[4] != "aws4_request" {
        return Err(AuthError::InvalidRequest(
            "malformed Credential scope".into(),
        ));
    }
    if fields[3] != "s3" {
        return Err(AuthError::InvalidRequest(format!(
            "unsupported signing service '{}' (expected s3)",
            fields[3]
        )));
    }

    Ok(ParsedAuth {
        access_key_id: fields[0].to_string(),
        date: fields[1].to_string(),
        region: fields[2].to_string(),
        service: fields[3].to_string(),
        signed_headers: signed_headers
            .split(';')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
        signature: signature.to_ascii_lowercase(),
    })
}

/// Verify a single request against the configured keys.
///
/// `payload_hash_override` lets the caller supply the body digest when the
/// `x-amz-content-sha256` header is absent (e.g. the handler buffered the body).
/// In practice S3 clients always send `x-amz-content-sha256`.
pub fn verify(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    keys: &AccessKeys,
    payload_hash_override: Option<&str>,
) -> Result<(), AuthError> {
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AuthError::AccessDenied("missing Authorization header".into()))?;
    let parsed = parse_authorization(auth_header)?;

    let secret = keys
        .secret_for(&parsed.access_key_id)
        .ok_or_else(|| AuthError::AccessDenied(format!("unknown access key '{}'", parsed.access_key_id)))?;

    // Required headers.
    let amz_date = headers
        .get("x-amz-date")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AuthError::InvalidRequest("missing x-amz-date".into()))?;

    // Date the client signed must match the credential scope date.
    let (date8, _full) = split_amz_date(amz_date)
        .ok_or_else(|| AuthError::InvalidRequest("malformed x-amz-date".into()))?;
    if date8 != parsed.date {
        return Err(AuthError::InvalidRequest(
            "x-amz-date does not match credential scope date".into(),
        ));
    }
    check_skew(amz_date)?;

    // Payload hash: prefer the header the client signed; else the override; else empty.
    let payload_hash = headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| payload_hash_override.map(|s| s.to_string()))
        .unwrap_or_else(|| sha256_hex(b""));

    // Canonical request.
    let canonical_uri = uri.path(); // already percent-encoded on the wire
    let can_query = canonical_query(uri.query().unwrap_or(""));

    let mut canonical_headers = String::new();
    for name in &parsed.signed_headers {
        let value = headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                AuthError::InvalidRequest(format!("signed header '{name}' missing from request"))
            })?;
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(&canonical_header_value(value));
        canonical_headers.push('\n');
    }
    let signed_headers_joined = parsed.signed_headers.join(";");

    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{can_query}\n{canonical_headers}\n{signed_headers_joined}\n{payload_hash}"
    );

    // String to sign.
    let scope = format!(
        "{}/{}/{}/aws4_request",
        parsed.date, parsed.region, parsed.service
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    // Signature.
    let key = signing_key(secret, &parsed.date, &parsed.region, &parsed.service);
    let expected = hex::encode(hmac_sha256(&key, string_to_sign.as_bytes()));

    if ct_eq(expected.as_bytes(), parsed.signature.as_bytes()) {
        Ok(())
    } else {
        Err(AuthError::SignatureDoesNotMatch(
            "the request signature we calculated does not match the signature you provided".into(),
        ))
    }
}

/// Parse an `x-amz-date` of the form `YYYYMMDDTHHMMSSZ`; returns (date8, full).
fn split_amz_date(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    // Basic ISO8601: YYYYMMDDTHHMMSSZ
    if s.len() == 16 && s.as_bytes().get(8) == Some(&b'T') && s.ends_with('Z') {
        Some((s[..8].to_string(), s.to_string()))
    } else {
        None
    }
}

/// Reject requests whose `x-amz-date` is outside the allowed skew window.
fn check_skew(amz_date: &str) -> Result<(), AuthError> {
    let naive = chrono::NaiveDateTime::parse_from_str(amz_date.trim(), "%Y%m%dT%H%M%SZ")
        .map_err(|_| AuthError::InvalidRequest("malformed x-amz-date".into()))?;
    let signed = naive.and_utc().timestamp();
    let now = chrono::Utc::now().timestamp();
    if (now - signed).abs() > MAX_CLOCK_SKEW_SECS {
        return Err(AuthError::RequestTimeTooSkewed(
            "the difference between the request time and the current time is too large".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// axum middleware
// ---------------------------------------------------------------------------

/// axum middleware enforcing SigV4 when keys are configured. A pass-through when
/// auth is disabled.
pub async fn require_sigv4(
    State(mode): State<AuthMode>,
    req: Request,
    next: Next,
) -> Response {
    if let AuthMode::SigV4(keys) = &mode {
        if let Err(e) = verify(req.method(), req.uri(), req.headers(), keys, None) {
            return auth_error_response(&e);
        }
    }
    next.run(req).await
}

fn auth_error_response(e: &AuthError) -> Response {
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>{}</Code>
  <Message>{}</Message>
  <RequestId>multifs</RequestId>
</Error>"#,
        e.code(),
        xml_escape(e.message())
    );
    Response::builder()
        .status(e.status())
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(xml))
        .unwrap_or_else(|_| StatusCode::FORBIDDEN.into_response())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The official AWS SigV4 test-suite vector `get-vanilla`.
    /// Secret + expected signature are the published constants from the suite.
    #[test]
    fn test_official_get_vanilla_vector() {
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let expected =
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31";

        // Rebuild the canonical request the suite specifies, then the STS + sig,
        // exercising the same primitives `verify` uses.
        let canonical_request = concat!(
            "GET\n",
            "/\n",
            "\n",
            "host:example.amazonaws.com\n",
            "x-amz-date:20150830T123600Z\n",
            "\n",
            "host;x-amz-date\n",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let scope = "20150830/us-east-1/service/aws4_request";
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n20150830T123600Z\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let key = signing_key(secret, "20150830", "us-east-1", "service");
        let sig = hex::encode(hmac_sha256(&key, string_to_sign.as_bytes()));
        assert_eq!(sig, expected, "SigV4 signing key derivation is wrong");
    }

    /// End-to-end: sign a request with our own primitives, then verify it.

    #[test]
    fn test_verify_accepts_well_formed_request() {
        // Use a current timestamp so skew passes.
        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date8 = now.format("%Y%m%d").to_string();
        let akid = "AKIDEXAMPLE";
        let secret = "topsecret";

        let host = "s3.local:9000";
        let map: BTreeMap<String, String> = [
            ("host".to_string(), host.to_string()),
            ("x-amz-date".to_string(), amz_date.clone()),
            (
                "x-amz-content-sha256".to_string(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
            ),
        ]
        .into_iter()
        .collect();
        let signed_headers: Vec<String> = map.keys().cloned().collect();
        let signed_headers_joined = signed_headers.join(";");
        let mut canonical_headers = String::new();
        for (k, v) in &map {
            canonical_headers.push_str(&format!("{}:{}\n", k, canonical_header_value(v)));
        }
        let payload_hash = map.get("x-amz-content-sha256").unwrap();
        let canonical_request = format!(
            "GET\n/bucket/key\n\n{canonical_headers}\n{signed_headers_joined}\n{payload_hash}"
        );
        let scope = format!("{date8}/us-east-1/s3/aws4_request");
        let sts = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let key = signing_key(secret, &date8, "us-east-1", "s3");
        let sig = hex::encode(hmac_sha256(&key, sts.as_bytes()));
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={akid}/{scope}, SignedHeaders={signed_headers_joined}, Signature={sig}"
        );

        let mut hm = HeaderMap::new();
        hm.insert("host", header::HeaderValue::from_str(host).unwrap());
        hm.insert("x-amz-date", header::HeaderValue::from_str(&amz_date).unwrap());
        hm.insert(
            "x-amz-content-sha256",
            header::HeaderValue::from_str(payload_hash).unwrap(),
        );
        hm.insert(header::AUTHORIZATION, header::HeaderValue::from_str(&auth).unwrap());

        let keys = AccessKeys::parse(&format!("{akid}:{secret}")).unwrap();
        let uri: Uri = "/bucket/key".parse().unwrap();
        assert!(verify(&Method::GET, &uri, &hm, &keys, None).is_ok());
    }

    #[test]
    fn test_verify_rejects_tampered_signature() {
        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date8 = now.format("%Y%m%d").to_string();
        let keys = AccessKeys::parse("AKIDEXAMPLE:topsecret").unwrap();
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/{date8}/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=deadbeef"
        );
        let mut hm = HeaderMap::new();
        hm.insert("host", header::HeaderValue::from_str("s3.local").unwrap());
        hm.insert("x-amz-date", header::HeaderValue::from_str(&amz_date).unwrap());
        hm.insert(header::AUTHORIZATION, header::HeaderValue::from_str(&auth).unwrap());
        let uri: Uri = "/bucket/key".parse().unwrap();
        let err = verify(&Method::GET, &uri, &hm, &keys, None).unwrap_err();
        assert!(matches!(err, AuthError::SignatureDoesNotMatch(_)));
    }

    #[test]
    fn test_verify_rejects_unknown_key() {
        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date8 = now.format("%Y%m%d").to_string();
        let keys = AccessKeys::parse("KNOWN:topsecret").unwrap();
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential=OTHER/{date8}/us-east-1/s3/aws4_request, SignedHeaders=host, Signature=abcd"
        );
        let mut hm = HeaderMap::new();
        hm.insert("host", header::HeaderValue::from_str("s3.local").unwrap());
        hm.insert("x-amz-date", header::HeaderValue::from_str(&amz_date).unwrap());
        hm.insert(header::AUTHORIZATION, header::HeaderValue::from_str(&auth).unwrap());
        let uri: Uri = "/bucket/key".parse().unwrap();
        let err = verify(&Method::GET, &uri, &hm, &keys, None).unwrap_err();
        assert!(matches!(err, AuthError::AccessDenied(_)));
    }

    #[test]
    fn test_verify_rejects_skewed_date() {
        let keys = AccessKeys::parse("AKIDEXAMPLE:topsecret").unwrap();
        let auth = "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=abcd";
        let mut hm = HeaderMap::new();
        hm.insert("host", header::HeaderValue::from_str("s3.local").unwrap());
        hm.insert("x-amz-date", header::HeaderValue::from_str("20150830T123600Z").unwrap());
        hm.insert(header::AUTHORIZATION, header::HeaderValue::from_str(auth).unwrap());
        let uri: Uri = "/".parse().unwrap();
        let err = verify(&Method::GET, &uri, &hm, &keys, None).unwrap_err();
        assert!(matches!(err, AuthError::RequestTimeTooSkewed(_)));
    }

    #[test]
    fn test_access_keys_parse() {
        assert!(AccessKeys::parse("").unwrap().is_empty());
        assert!(AccessKeys::parse("  , ").unwrap().is_empty());
        let k = AccessKeys::parse("A:1, B:2 ").unwrap();
        assert_eq!(k.secret_for("A"), Some("1"));
        assert_eq!(k.secret_for("B"), Some("2"));
        assert!(AccessKeys::parse("nocolon").is_err());
        assert!(AccessKeys::parse("A:").is_err());
    }

    #[test]
    fn test_auth_mode_from_config() {
        assert!(!AuthMode::from_config("").unwrap().is_enabled());
        assert!(AuthMode::from_config("A:1").unwrap().is_enabled());
        assert!(AuthMode::from_config("bad").is_err());
    }

    #[test]
    fn test_canonical_query_sorts() {
        assert_eq!(canonical_query(""), "");
        assert_eq!(canonical_query("b=2&a=1"), "a=1&b=2");
        assert_eq!(canonical_query("uploads"), "uploads=");
        assert_eq!(canonical_query("prefix=a/&marker=x"), "marker=x&prefix=a/");
    }

    /// Self-signed round-trip with a real query string + multiple signed headers.
    #[test]
    fn test_verify_roundtrip_with_query() {
        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date8 = now.format("%Y%m%d").to_string();
        let (akid, secret) = ("AKIDEXAMPLE", "topsecret");
        let host = "s3.local:9000";
        let payload_hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

        let map: BTreeMap<String, String> = [
            ("host".to_string(), host.to_string()),
            ("x-amz-content-sha256".to_string(), payload_hash.to_string()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ]
        .into_iter()
        .collect();
        let signed_headers_joined = map.keys().cloned().collect::<Vec<_>>().join(";");
        let mut canonical_headers = String::new();
        for (k, v) in &map {
            canonical_headers.push_str(&format!("{}:{}\n", k, canonical_header_value(v)));
        }
        let query = "list-type=2&max-keys=10&prefix=x/";
        let canonical_request = format!(
            "GET\n/b\n{query}\n{canonical_headers}\n{signed_headers_joined}\n{payload_hash}"
        );
        let scope = format!("{date8}/us-east-1/s3/aws4_request");
        let sts = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let key = signing_key(secret, &date8, "us-east-1", "s3");
        let sig = hex::encode(hmac_sha256(&key, sts.as_bytes()));
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={akid}/{scope}, SignedHeaders={signed_headers_joined}, Signature={sig}"
        );

        let mut hm = HeaderMap::new();
        hm.insert("host", header::HeaderValue::from_str(host).unwrap());
        hm.insert("x-amz-date", header::HeaderValue::from_str(&amz_date).unwrap());
        hm.insert("x-amz-content-sha256", header::HeaderValue::from_str(payload_hash).unwrap());
        hm.insert(header::AUTHORIZATION, header::HeaderValue::from_str(&auth).unwrap());

        let keys = AccessKeys::parse(&format!("{akid}:{secret}")).unwrap();
        let uri: Uri = format!("/b?{query}").parse().unwrap();
        assert!(verify(&Method::GET, &uri, &hm, &keys, None).is_ok());

        // Tampering the query (e.g. changing max-keys) must break it.
        let uri2: Uri = "/b?list-type=2&max-keys=999&prefix=x/".parse().unwrap();
        assert!(verify(&Method::GET, &uri2, &hm, &keys, None).is_err());
    }
}
