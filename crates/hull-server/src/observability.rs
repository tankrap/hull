//! Operational observability: structured request logging, lightweight Prometheus metrics, and the
//! tracing subscriber init. Deliberately dependency-light — the metrics are hand-rolled atomics (a
//! handful of counters + one gauge) rather than a full `metrics`/exporter stack, because the surface
//! is tiny and a global static keeps it out of the request path's hot allocations.
//!
//! ## Why a process-global metric registry
//! Prometheus metrics are inherently process-wide, and the request middleware ([`observe`]) must
//! reach them without threading extra state through every layer. A `LazyLock<Metrics>` gives a
//! zero-arg accessor that both the middleware and the `/metrics` handler share.
//!
//! ## Streaming safety (SSE / git / tar are NOT buffered)
//! [`observe`] is an `axum::middleware::from_fn` layer. `next.run(req).await` resolves as soon as the
//! **response head** is ready — for a streaming handler that is the moment it returns its `Sse`/body
//! stream, BEFORE any body bytes flow. We record latency + status and log at that point and hand the
//! response straight back untouched; the body streams through lazily as the client reads it. Nothing
//! here reads or collects the body, so SSE `/api/feed`, git smart-HTTP, and the tar endpoint keep
//! streaming exactly as before. (Contrast: a layer that awaited/collected the body would deadlock SSE.)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Process-wide metric registry. Hand-rolled atomics — see the module docs for why we avoid a metrics
/// crate here.
pub struct Metrics {
    /// `hull_http_requests_total` split by status class, index 0..=3 → 2xx/3xx/4xx/5xx.
    requests_total: [AtomicU64; 4],
    /// `hull_http_requests_in_flight` — incremented on entry, decremented on exit (drop-safe).
    in_flight: AtomicU64,
    /// Process start, for `hull_process_uptime_seconds`.
    start: Instant,
}

impl Metrics {
    fn new() -> Self {
        Metrics {
            requests_total: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
            in_flight: AtomicU64::new(0),
            start: Instant::now(),
        }
    }

    /// Bucket an HTTP status into a class index (2xx→0 … 5xx→3). 1xx and anything unexpected fall into
    /// the nearest sane bucket (1xx→2xx bucket is avoided; treat <200 as 2xx-ish is wrong, so map 1xx
    /// to index 0 only if ≥200). We only ever emit ≥200 here, so keep it simple: clamp into 2..=5.
    fn class_index(status: u16) -> usize {
        match status / 100 {
            2 => 0,
            3 => 1,
            4 => 2,
            _ => 3, // 5xx (and any stray 1xx/other, which our handlers never return)
        }
    }

    /// Render the current metrics as Prometheus text exposition (v0.0.4).
    pub fn render(&self) -> String {
        let uptime = self.start.elapsed().as_secs();
        let in_flight = self.in_flight.load(Ordering::Relaxed);
        let classes = ["2xx", "3xx", "4xx", "5xx"];
        let mut out = String::with_capacity(512);
        out.push_str("# HELP hull_http_requests_total Total HTTP requests handled, by status class.\n");
        out.push_str("# TYPE hull_http_requests_total counter\n");
        for (i, class) in classes.iter().enumerate() {
            let n = self.requests_total[i].load(Ordering::Relaxed);
            out.push_str(&format!("hull_http_requests_total{{class=\"{class}\"}} {n}\n"));
        }
        out.push_str("# HELP hull_http_requests_in_flight HTTP requests currently being served.\n");
        out.push_str("# TYPE hull_http_requests_in_flight gauge\n");
        out.push_str(&format!("hull_http_requests_in_flight {in_flight}\n"));
        out.push_str("# HELP hull_process_uptime_seconds Seconds since the server process started.\n");
        out.push_str("# TYPE hull_process_uptime_seconds gauge\n");
        out.push_str(&format!("hull_process_uptime_seconds {uptime}\n"));
        out
    }
}

/// The shared registry.
pub static METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::new);

/// RAII guard: bumps the in-flight gauge on construction and drops it on scope exit — including on an
/// unwind, so a handler panic (caught above by `CatchPanicLayer`) still releases the gauge.
struct InFlightGuard;
impl InFlightGuard {
    fn enter() -> Self {
        METRICS.in_flight.fetch_add(1, Ordering::Relaxed);
        InFlightGuard
    }
}
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        METRICS.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Request-observability middleware: one structured log line per request (method, path template,
/// status, latency_ms) plus the metric updates. Emits on the response head, so streaming bodies are
/// never buffered (see module docs).
pub async fn observe(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    // Prefer the matched route template (`/api/repos/:tenant/:repo/issues`) over the raw path so logs
    // and any future label cardinality stay bounded; fall back to the raw path if unmatched.
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());

    let _guard = InFlightGuard::enter();
    let started = Instant::now();
    let resp = next.run(req).await;
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    let status = resp.status().as_u16();

    METRICS.requests_total[Metrics::class_index(status)].fetch_add(1, Ordering::Relaxed);

    // One line per request. At info; drop to debug for the noisiest paths if needed later.
    tracing::info!(
        method = %method,
        path = %path,
        status = status,
        latency_ms = latency_ms,
        "request"
    );
    resp
}

/// `GET /metrics` — unauthenticated Prometheus scrape. Cheap: renders atomics, touches no store.
pub async fn metrics_handler() -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        METRICS.render(),
    )
        .into_response()
}

/// Initialize the global tracing subscriber ONCE. Idempotent: `try_init` returns `Err` if a
/// subscriber is already installed (e.g. a second `run` in a test process), which we ignore so nothing
/// panics. Level comes from `RUST_LOG` (default `info`); JSON formatting kicks in for the prod profile
/// (`HULL_PROFILE=prod`) or an explicit `HULL_LOG_FORMAT=json`, otherwise a compact human formatter.
pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let json = std::env::var("HULL_LOG_FORMAT").map(|v| v.trim().eq_ignore_ascii_case("json")).unwrap_or(false)
        || std::env::var("HULL_PROFILE").map(|v| v.trim().eq_ignore_ascii_case("prod")).unwrap_or(false);

    if json {
        let _ = fmt().json().with_env_filter(filter).try_init();
    } else {
        let _ = fmt().compact().with_env_filter(filter).try_init();
    }
}

/// `GET /ready` returns this JSON shape; small and cheap for orchestrator polling.
pub fn ready_response(ready: bool) -> Response {
    let code = if ready { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, axum::Json(serde_json::json!({ "ready": ready }))).into_response()
}
