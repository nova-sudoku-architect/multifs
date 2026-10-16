//! Observability: in-process metrics (Prometheus text exposition) + health
//! endpoints (`/healthz`, `/readyz`).
//!
//! Deliberately dependency-free: a handful of atomics plus the existing
//! metadata DB / backend config are rendered into the Prometheus text format
//! by hand. This keeps the build lean (no global metrics recorder, no
//! `metrics`/`prometheus` crate) and matches the codebase's preference for
//! minimal dependencies.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

use crate::storage::engine::StorageEngine;

/// In-process counters for the daemon. Cheap relaxed atomics; reset on restart
/// (Prometheus `counter` semantics — use `rate()`/`increase()` on the scrape).
#[derive(Default)]
pub struct AppMetrics {
    /// Requests by response status class.
    pub req_2xx: AtomicU64,
    pub req_3xx: AtomicU64,
    pub req_4xx: AtomicU64,
    pub req_5xx: AtomicU64,
    /// Request / response body bytes (where a `Content-Length` is known).
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    /// Vacuum GC: number of runs and items (versions + abandoned uploads) reclaimed.
    pub gc_runs: AtomicU64,
    pub gc_reclaimed_items: AtomicU64,
}

impl AppMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn record_status(&self, status: u16) {
        let counter = match status / 100 {
            2 => &self.req_2xx,
            3 => &self.req_3xx,
            4 => &self.req_4xx,
            _ => &self.req_5xx,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn add_bytes_in(&self, n: u64) {
        if n > 0 {
            self.bytes_in.fetch_add(n, Ordering::Relaxed);
        }
    }

    fn add_bytes_out(&self, n: u64) {
        if n > 0 {
            self.bytes_out.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Record one vacuum GC run that reclaimed `items` versions/uploads.
    pub fn record_gc(&self, items: u64) {
        self.gc_runs.fetch_add(1, Ordering::Relaxed);
        if items > 0 {
            self.gc_reclaimed_items.fetch_add(items, Ordering::Relaxed);
        }
    }
}

/// Best-effort body length from a `Content-Length` header.
fn content_length(headers: &HeaderMap) -> u64 {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// axum middleware: count requests, their status class, and body bytes moved.
pub async fn track_metrics(
    State(metrics): State<Arc<AppMetrics>>,
    req: Request,
    next: Next,
) -> Response {
    let in_len = content_length(req.headers());
    let resp = next.run(req).await;
    let out_len = content_length(resp.headers());
    metrics.record_status(resp.status().as_u16());
    metrics.add_bytes_in(in_len);
    metrics.add_bytes_out(out_len);
    resp
}

#[derive(Clone)]
struct MetricsState {
    engine: Arc<StorageEngine>,
    metrics: Arc<AppMetrics>,
}

/// Build the metrics/health router: `GET /metrics`, `/healthz`, `/readyz`.
pub fn build_router(engine: Arc<StorageEngine>, metrics: Arc<AppMetrics>) -> Router {
    let state = MetricsState { engine, metrics };
    Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state)
}

/// Liveness: the process is up and serving.
async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Readiness: the metadata DB is reachable and at least one backend is configured.
async fn readyz(State(state): State<MetricsState>) -> StatusCode {
    if state.engine.backend_count() == 0 {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    match state.engine.db_ping() {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Render the Prometheus text exposition format.
async fn metrics_handler(State(state): State<MetricsState>) -> Response {
    let mut out = String::with_capacity(2048);
    render(&state, &mut out);
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
        .into_response()
}

/// Escape a Prometheus label value (only `\`, `"` and newline need escaping).
fn escape_label(v: &str) -> String {
    let mut s = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            '\n' => s.push_str("\\n"),
            _ => s.push(c),
        }
    }
    s
}

fn render(state: &MetricsState, out: &mut String) {
    let m = &state.metrics;
    let r2 = m.req_2xx.load(Ordering::Relaxed);
    let r3 = m.req_3xx.load(Ordering::Relaxed);
    let r4 = m.req_4xx.load(Ordering::Relaxed);
    let r5 = m.req_5xx.load(Ordering::Relaxed);

    out.push_str("# HELP multifs_requests_total HTTP requests handled, by status class.\n");
    out.push_str("# TYPE multifs_requests_total counter\n");
    out.push_str(&format!(
        "multifs_requests_total{{status_class=\"2xx\"}} {}\n",
        r2
    ));
    out.push_str(&format!(
        "multifs_requests_total{{status_class=\"3xx\"}} {}\n",
        r3
    ));
    out.push_str(&format!(
        "multifs_requests_total{{status_class=\"4xx\"}} {}\n",
        r4
    ));
    out.push_str(&format!(
        "multifs_requests_total{{status_class=\"5xx\"}} {}\n",
        r5
    ));

    out.push_str("# HELP multifs_errors_total HTTP requests that returned 4xx or 5xx.\n");
    out.push_str("# TYPE multifs_errors_total counter\n");
    out.push_str(&format!("multifs_errors_total {}\n", r4 + r5));

    out.push_str("# HELP multifs_bytes_in_total Request body bytes received.\n");
    out.push_str("# TYPE multifs_bytes_in_total counter\n");
    out.push_str(&format!(
        "multifs_bytes_in_total {}\n",
        m.bytes_in.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP multifs_bytes_out_total Response body bytes sent.\n");
    out.push_str("# TYPE multifs_bytes_out_total counter\n");
    out.push_str(&format!(
        "multifs_bytes_out_total {}\n",
        m.bytes_out.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP multifs_gc_runs_total Vacuum GC runs.\n");
    out.push_str("# TYPE multifs_gc_runs_total counter\n");
    out.push_str(&format!(
        "multifs_gc_runs_total {}\n",
        m.gc_runs.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP multifs_gc_reclaimed_items_total Versions/abandoned uploads reclaimed by vacuum.\n");
    out.push_str("# TYPE multifs_gc_reclaimed_items_total counter\n");
    out.push_str(&format!(
        "multifs_gc_reclaimed_items_total {}\n",
        m.gc_reclaimed_items.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP multifs_backends Number of configured backends.\n");
    out.push_str("# TYPE multifs_backends gauge\n");
    out.push_str(&format!("multifs_backends {}\n", state.engine.backend_count()));

    // Per-backend gauges from the metadata DB (no backend API calls).
    if let Ok(backends) = state.engine.backend_metrics() {
        out.push_str("# HELP multifs_backend_objects Objects stored on a backend.\n");
        out.push_str("# TYPE multifs_backend_objects gauge\n");
        for b in &backends {
            out.push_str(&format!(
                "multifs_backend_objects{{backend=\"{}\"}} {}\n",
                escape_label(&b.label),
                b.object_count
            ));
        }

        out.push_str("# HELP multifs_backend_parts Multipart parts stored on a backend.\n");
        out.push_str("# TYPE multifs_backend_parts gauge\n");
        for b in &backends {
            out.push_str(&format!(
                "multifs_backend_parts{{backend=\"{}\"}} {}\n",
                escape_label(&b.label),
                b.part_count
            ));
        }

        out.push_str("# HELP multifs_backend_used_bytes Bytes stored on a backend (metadata, local).\n");
        out.push_str("# TYPE multifs_backend_used_bytes gauge\n");
        for b in &backends {
            out.push_str(&format!(
                "multifs_backend_used_bytes{{backend=\"{}\"}} {}\n",
                escape_label(&b.label),
                b.used_bytes.max(0)
            ));
        }

        out.push_str("# HELP multifs_backend_quota_bytes Configured backend quota.\n");
        out.push_str("# TYPE multifs_backend_quota_bytes gauge\n");
        for b in &backends {
            out.push_str(&format!(
                "multifs_backend_quota_bytes{{backend=\"{}\"}} {}\n",
                escape_label(&b.label),
                b.quota_bytes.max(0)
            ));
        }

        out.push_str("# HELP multifs_backend_fill_ratio Backend utilization (used/quota).\n");
        out.push_str("# TYPE multifs_backend_fill_ratio gauge\n");
        for b in &backends {
            let ratio = if b.quota_bytes > 0 {
                b.used_bytes as f64 / b.quota_bytes as f64
            } else {
                0.0
            };
            out.push_str(&format!(
                "multifs_backend_fill_ratio{{backend=\"{}\"}} {:.6}\n",
                escape_label(&b.label),
                ratio
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::storage::engine::BackendHandle;
    use crate::storage::metadata::MetadataDb;
    use crate::storage::test_utils::MockBackend;

    fn engine_with(backends: Vec<BackendHandle>) -> Arc<StorageEngine> {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("metrics.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        // Keep the tempdir alive for the DB's lifetime: leak it (test process).
        std::mem::forget(dir);
        Arc::new(StorageEngine::from_backends(backends, db))
    }

    fn one_backend() -> Vec<BackendHandle> {
        vec![BackendHandle::new(
            Box::new(MockBackend::new("mock-a")),
            "/mnt/mock-a".to_string(),
            "mock-a".to_string(),
            10,
        )]
    }

    async fn body_string(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn test_metrics_exposition_renders_counters_and_gauges() {
        let metrics = AppMetrics::new();
        metrics.record_status(200);
        metrics.record_status(404);
        metrics.record_gc(3);
        let app = build_router(engine_with(one_backend()), metrics);

        let resp = app
            .oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(ct.starts_with("text/plain"));
        let body = body_string(resp).await;

        assert!(body.contains("multifs_requests_total{status_class=\"2xx\"} 1"));
        assert!(body.contains("multifs_requests_total{status_class=\"4xx\"} 1"));
        assert!(body.contains("multifs_errors_total 1"));
        assert!(body.contains("multifs_backends 1"));
        assert!(body.contains("multifs_backend_objects{backend=\"mock-a\"} 0"));
        assert!(body.contains("multifs_backend_quota_bytes{backend=\"mock-a\"} 10737418240"));
        assert!(body.contains("multifs_bytes_in_total 0"));
        assert!(body.contains("multifs_gc_runs_total 1"));
        assert!(body.contains("multifs_gc_reclaimed_items_total 3"));
        // Prometheus exposition: every sample line is preceded by TYPE/HELP.
        assert!(body.contains("# TYPE multifs_requests_total counter"));
    }

    #[tokio::test]
    async fn test_healthz_is_ok() {
        let app = build_router(engine_with(one_backend()), AppMetrics::new());
        let resp = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_readyz_ok_with_backend_and_db() {
        let app = build_router(engine_with(one_backend()), AppMetrics::new());
        let resp = app
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_readyz_503_without_backends() {
        let app = build_router(engine_with(vec![]), AppMetrics::new());
        let resp = app
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_track_metrics_counts_requests_and_status() {
        let metrics = AppMetrics::new();
        let app = Router::new()
            .route("/x", get(|| async { StatusCode::OK }))
            .route("/boom", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
            .layer(axum::middleware::from_fn_with_state(
                metrics.clone(),
                track_metrics,
            ));

        let _ = app
            .clone()
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let _ = app
            .oneshot(Request::builder().uri("/boom").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(metrics.req_2xx.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.req_5xx.load(Ordering::Relaxed), 1);
    }
}
