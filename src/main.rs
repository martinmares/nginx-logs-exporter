use std::{
    collections::VecDeque,
    env,
    io::Write,
    sync::Arc,
    time::{Duration, SystemTime},
};

use async_stream::stream;
use axum::response::sse::{Event, Sse};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{StatusCode, header::CONTENT_TYPE},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use flate2::{Compression, write::GzEncoder};
use lazy_static::lazy_static;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, Opts, Registry, TextEncoder,
    linear_buckets,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncSeekExt, BufReader},
    sync::RwLock,
    time::sleep,
};

const MAX_LINES_PER_LOG: usize = 2000;
const UI_TEMPLATE: &str = include_str!("../templates/ui.html");

#[derive(Clone)]
struct AppState {
    log_state: Arc<RwLock<LogState>>,
    access_log_path: String,
    error_log_path: String,
    ui_title: String,
}

#[derive(Default)]
struct LogState {
    access_lines: VecDeque<String>,
    error_lines: VecDeque<String>,
    access_lines_seen: u64,
    error_lines_seen: u64,
    last_access_update: Option<SystemTime>,
    last_error_update: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug)]
enum LogKind {
    Access,
    Error,
}

fn log_kind_name(kind: LogKind) -> &'static str {
    match kind {
        LogKind::Access => "access.log",
        LogKind::Error => "error.log",
    }
}

/* ===========================
 *  METRICS - s prefixem
 * ===========================
 */

/// Vrátí název metriky s ohledem na METRICS_PREFIX:
/// - <nil> / prázdné → původní název
/// - jinak: prefix (bez koncových "_") + "_" + base
fn metric_name(base: &str) -> String {
    match env::var("METRICS_PREFIX") {
        Ok(val) => {
            let v = val.trim();
            if v.is_empty() {
                base.to_string()
            } else {
                let cleaned = v.trim_end_matches('_');
                if cleaned.is_empty() {
                    base.to_string()
                } else {
                    format!("{}_{}", cleaned, base)
                }
            }
        }
        Err(_) => base.to_string(),
    }
}

lazy_static! {
    static ref HTTP_STATUS_TOTAL: IntCounterVec = IntCounterVec::new(
        Opts::new(
            &metric_name("nginx_http_status_total"),
            "Total number of responses by HTTP status code"
        ),
        &["status"]
    )
    .unwrap();

    static ref HTTP_STATUS_CLASS_TOTAL: IntCounterVec = IntCounterVec::new(
        Opts::new(
            &metric_name("nginx_http_status_class_total"),
            "Total number of responses by HTTP status class"
        ),
        &["class"]
    )
    .unwrap();

    static ref HTTP_502_TOTAL: IntCounter = IntCounter::new(
        &metric_name("nginx_http_502_total"),
        "Number of HTTP 502 Bad Gateway responses"
    )
    .unwrap();

    static ref UPSTREAM_STATUS_TOTAL: IntCounterVec = IntCounterVec::new(
        Opts::new(
            &metric_name("nginx_upstream_status_total"),
            "Total upstream responses by upstream_addr and status"
        ),
        &["upstream_addr", "status"]
    )
    .unwrap();

    // Upstream-related errors parsed from error.log
    static ref UPSTREAM_ERROR_TOTAL: IntCounterVec = IntCounterVec::new(
        Opts::new(
            &metric_name("nginx_upstream_error_total"),
            "Upstream-related errors from error.log, classified by reason"
        ),
        &["reason", "upstream"]
    )
    .unwrap();

    static ref HTTP_REQUEST_DURATION_SECONDS: HistogramVec = {
        let buckets = linear_buckets(0.01, 0.05, 20).expect("invalid histogram buckets");
        let opts = HistogramOpts::new(
            &metric_name("nginx_http_request_duration_seconds"),
            "Histogram of Nginx request_time in seconds, labelled by upstream_addr",
        )
        .buckets(buckets);
        HistogramVec::new(opts, &["upstream_addr"]).unwrap()
    };

    static ref REGISTRY: Registry = {
        let registry = Registry::new();
        registry
            .register(Box::new(HTTP_STATUS_TOTAL.clone()))
            .expect("register http_status_total");
        registry
            .register(Box::new(HTTP_STATUS_CLASS_TOTAL.clone()))
            .expect("register http_status_class_total");
        registry
            .register(Box::new(HTTP_502_TOTAL.clone()))
            .expect("register http_502_total");
        registry
            .register(Box::new(UPSTREAM_STATUS_TOTAL.clone()))
            .expect("register upstream_status_total");
        registry
            .register(Box::new(UPSTREAM_ERROR_TOTAL.clone()))
            .expect("register upstream_error_total");
        registry
            .register(Box::new(HTTP_REQUEST_DURATION_SECONDS.clone()))
            .expect("register http_request_duration_seconds");
        registry
    };
}

/* ===========================
 *  MAIN
 * ===========================
 */

#[tokio::main]
async fn main() {
    let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:9100".to_string());

    let access_log_path =
        env::var("ACCESS_LOG_PATH").unwrap_or_else(|_| "/app/logs/access.log".to_string());
    let error_log_path =
        env::var("ERROR_LOG_PATH").unwrap_or_else(|_| "/app/logs/error.log".to_string());

    let ui_prefix_raw = env::var("NGINX_UI_PUBLIC_PREFIX").unwrap_or_default();
    let ui_prefix = normalize_prefix(&ui_prefix_raw);

    let ui_title = env::var("NGINX_UI_TITLE").unwrap_or_else(|_| "Nginx logs".to_string());

    let state = AppState {
        log_state: Arc::new(RwLock::new(LogState::default())),
        access_log_path: access_log_path.clone(),
        error_log_path: error_log_path.clone(),
        ui_title,
    };

    // tail access.log
    let state_for_access = state.clone();
    tokio::spawn(async move {
        tail_file_loop(access_log_path, LogKind::Access, state_for_access).await;
    });

    // tail error.log
    let state_for_error = state.clone();
    tokio::spawn(async move {
        tail_file_loop(error_log_path, LogKind::Error, state_for_error).await;
    });

    let mut app = Router::new()
        .route("/", get(ui_handler))
        .route("/ui", get(ui_handler))
        .route("/healthz", get(healthz_handler))
        .route("/metrics", get(metrics_handler))
        .route("/access.log.gz", get(download_access_log_handler))
        .route("/error.log.gz", get(download_error_log_handler))
        .route("/logs/stream", get(logs_sse_handler));

    // prefixované routy - např. /es-proxy/ui
    if let Some(prefix) = ui_prefix {
        let prefix_root = prefix.clone();
        let prefix_ui = format!("{}/ui", prefix_root);
        let prefix_access = format!("{}/access.log.gz", prefix_root);
        let prefix_error = format!("{}/error.log.gz", prefix_root);
        let prefix_stream = format!("{}/logs/stream", prefix_root);
        println!(
            "Registering UI routes with prefix: '{}' and '{}'",
            prefix_root, prefix_ui
        );
        app = app
            .route(&prefix_root, get(ui_handler))
            .route(&prefix_ui, get(ui_handler))
            .route(&prefix_access, get(download_access_log_handler))
            .route(&prefix_error, get(download_error_log_handler))
            .route(&prefix_stream, get(logs_sse_handler));
    }

    let app = app.with_state(state);

    println!("Listening on {}", listen_addr);
    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .expect("failed to bind");
    axum::serve(listener, app).await.expect("server error");
}

fn normalize_prefix(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() || s == "/" {
        return None;
    }

    let mut pref = s.to_string();

    if !pref.starts_with('/') {
        pref = format!("/{}", pref);
    }
    while pref.ends_with('/') {
        pref.pop();
    }

    if pref.is_empty() || pref == "/" {
        None
    } else {
        Some(pref)
    }
}

/* ===========================
 *  FILE TAILER
 * ===========================
 */

async fn tail_file_loop(path: String, kind: LogKind, state: AppState) {
    loop {
        match File::open(&path).await {
            Ok(mut file) => {
                println!("Opened log file {:?} for {:?}", path, log_kind_name(kind));

                if let Err(e) = file.seek(std::io::SeekFrom::End(0)).await {
                    eprintln!("Failed to seek to end of {}: {}", path, e);
                }

                let mut reader = BufReader::new(file);
                let mut line = String::new();

                let mut position: u64 = match tokio::fs::metadata(&path).await {
                    Ok(meta) => meta.len(),
                    Err(_) => 0,
                };

                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => {
                            match tokio::fs::metadata(&path).await {
                                Ok(meta) => {
                                    let len = meta.len();
                                    if len < position {
                                        println!(
                                            "Detected rotation/truncation of {}, reopening",
                                            path
                                        );
                                        break;
                                    }
                                }
                                Err(e) => {
                                    eprintln!("metadata() failed for {}: {}. Reopening.", path, e);
                                    break;
                                }
                            }
                            sleep(Duration::from_millis(500)).await;
                        }
                        Ok(bytes_read) => {
                            position += bytes_read as u64;
                            let trimmed = line.trim_end_matches(&['\n', '\r'][..]).to_string();
                            if trimmed.is_empty() {
                                continue;
                            }
                            handle_log_line(&trimmed, kind, &state).await;
                        }
                        Err(e) => {
                            eprintln!("Error reading {}: {}. Reopening in 1s.", path, e);
                            sleep(Duration::from_secs(1)).await;
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("Could not open log file {}: {}. Retrying in 5s.", path, e);
                sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn handle_log_line(line: &str, kind: LogKind, state: &AppState) {
    let now = SystemTime::now();

    {
        let mut log_state = state.log_state.write().await;
        match kind {
            LogKind::Access => {
                log_state.access_lines.push_back(line.to_string());
                if log_state.access_lines.len() > MAX_LINES_PER_LOG {
                    log_state.access_lines.pop_front();
                }
                log_state.access_lines_seen += 1;
                log_state.last_access_update = Some(now);
            }
            LogKind::Error => {
                log_state.error_lines.push_back(line.to_string());
                if log_state.error_lines.len() > MAX_LINES_PER_LOG {
                    log_state.error_lines.pop_front();
                }
                log_state.error_lines_seen += 1;
                log_state.last_error_update = Some(now);
            }
        }
    }

    // Access / error log -> metriky
    match kind {
        LogKind::Access => {
            let parsed = parser::parse_access_line(line);
            metrics_layer::record_access(&parsed);
        }
        LogKind::Error => {
            metrics_layer::record_error(line);
        }
    }
}

/* ===========================
 *  PARSER
 * ===========================
 */

mod parser {

    #[derive(Debug, Clone)]
    pub struct ParsedAccess {
        #[allow(dead_code)]
        pub raw: String,
        pub status: Option<u16>,
        pub upstream_addr: Option<String>,
        pub upstream_status: Option<u16>,
        pub request_time: Option<f64>,
        pub upstream_label: Option<String>, // nově: logické jméno upstreamu
    }

    pub fn parse_access_line(line: &str) -> ParsedAccess {
        let status = parse_status_code_from_access(line);

        // upstream_status=200,502 nebo "-"
        let upstream_status = extract_kv_value(line, "upstream_status=")
            .and_then(|v| v.split(',').next())
            .and_then(parse_status_field);

        // upstream_addr=10.0.0.10:443,10.0.0.11:443 → bereme první
        let upstream_addr = extract_kv_value(line, "upstream_addr=")
            .and_then(|v| v.split(',').next())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty() && s != "-");

        // request_time=0.123
        let request_time =
            extract_kv_value(line, "request_time=").and_then(|v| v.parse::<f64>().ok());

        // upstream_label=tsm-gateway / tsm-internals-proxy / static-files
        let upstream_label = extract_kv_value(line, "upstream_label=")
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty() && s != "-");

        ParsedAccess {
            raw: line.to_string(),
            status,
            upstream_addr,
            upstream_status,
            request_time,
            upstream_label,
        }
    }

    fn parse_status_code_from_access(line: &str) -> Option<u16> {
        let first_quote = line.find('"')?;
        let rest = &line[first_quote + 1..];
        let second_quote_rel = rest.find('"')?;
        let after_request = &rest[second_quote_rel + 1..];
        let mut parts = after_request.split_whitespace();
        let status_str = parts.next()?;
        status_str.parse::<u16>().ok()
    }

    pub fn extract_kv_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
        let idx = line.find(key)?;
        let rest = &line[idx + key.len()..];
        let end = rest.find(' ').unwrap_or(rest.len());
        let val = &rest[..end];
        if val.is_empty() || val == "-" {
            None
        } else {
            Some(val)
        }
    }

    fn parse_status_field(s: &str) -> Option<u16> {
        if s == "-" {
            return None;
        }
        s.trim().parse::<u16>().ok()
    }
}

/* ===========================
 *  METRICS LAYER
 * ===========================
 */

mod metrics_layer {
    use super::*;
    use crate::parser::ParsedAccess;

    pub fn record_access(parsed: &ParsedAccess) {
        // HTTP status (klientský)
        if let Some(status) = parsed.status {
            let status_str = status.to_string();
            HTTP_STATUS_TOTAL.with_label_values(&[&status_str]).inc();

            let class = format!("{}xx", status / 100);
            HTTP_STATUS_CLASS_TOTAL.with_label_values(&[&class]).inc();

            if status == 502 {
                HTTP_502_TOTAL.inc();
            }
        }

        // Preferuj logické jméno upstreamu, pak IP:port, pak <none>
        let upstream = parsed
            .upstream_label
            .as_deref()
            .or(parsed.upstream_addr.as_deref())
            .unwrap_or("<none>");

        // Upstream status
        if let Some(us) = parsed.upstream_status {
            let s = us.to_string();
            UPSTREAM_STATUS_TOTAL
                .with_label_values(&[upstream, &s])
                .inc();
        }

        // Histogram request_time (sekundy) podle upstream (label "upstream_addr")
        if let Some(rt) = parsed.request_time {
            HTTP_REQUEST_DURATION_SECONDS
                .with_label_values(&[upstream])
                .observe(rt);
        }
    }

    /// Zpracování řádků z error.log - upstream chyby podle reason + upstream.
    pub fn record_error(line: &str) {
        // Zajímá nás jen něco, co se týká upstreamu
        if !line.contains("upstream") {
            return;
        }

        // Vytáhneme "reason" podle textu zprávy
        let reason = if line.contains("no live upstreams") {
            "no_live_upstream"
        } else if line.contains("upstream timed out") {
            "timeout"
        } else if line.contains("connect() failed") || line.contains("Connection refused") {
            "connect_failed"
        } else if line.contains("SSL_do_handshake() failed") || line.contains("SSL handshake") {
            "ssl_handshake"
        } else if line.contains("upstream prematurely closed connection") {
            // typicky SSE / nginx-ui-logs/logs/stream
            "upstream_closed"
        } else if line.contains("an upstream response is buffered to a temporary file") {
            // velký response -> šel do temp souboru, není to "error" ve smyslu incidentu
            "buffered_temp_file"
        } else {
            "other"
        };

        let upstream = extract_upstream_from_error(line);
        UPSTREAM_ERROR_TOTAL
            .with_label_values(&[reason, upstream.as_str()])
            .inc();
    }

    fn extract_upstream_from_error(line: &str) -> String {
        if let Some(idx) = line.find("upstream: \"") {
            let rest = &line[idx + "upstream: \"".len()..];
            if let Some(end) = rest.find('"') {
                return rest[..end].to_string();
            }
        }
        "<unknown>".to_string()
    }
}

/* ===========================
 *  UI + HTML
 * ===========================
 */

#[derive(Deserialize)]
struct UiQuery {
    limit: Option<usize>,
    refresh: Option<u64>,
}

#[derive(Deserialize)]
struct LogsSseQuery {
    limit: Option<usize>,
    interval: Option<u64>,
}

async fn ui_handler(State(state): State<AppState>, Query(params): Query<UiQuery>) -> Html<String> {
    let limit = params.limit.unwrap_or(50).min(MAX_LINES_PER_LOG).max(1);
    let refresh = params.refresh.unwrap_or(10).clamp(1, 3600);

    let log_state = state.log_state.read().await;
    let access_text = tail_as_string(&log_state.access_lines, limit);
    let error_text = tail_as_string(&log_state.error_lines, limit);

    let html = render_ui_page(&state.ui_title, limit, refresh, &access_text, &error_text);
    Html(html)
}

fn tail_as_vec(lines: &VecDeque<String>, limit: usize) -> Vec<String> {
    let len = lines.len();
    let start = len.saturating_sub(limit);
    lines.iter().skip(start).cloned().collect()
}

fn tail_as_string(lines: &VecDeque<String>, limit: usize) -> String {
    tail_as_vec(lines, limit).join("\n")
}

fn render_ui_page(title: &str, limit: usize, refresh: u64, access: &str, error: &str) -> String {
    let escaped_title = html_escape(title);
    let escaped_access = html_escape(access);
    let escaped_error = html_escape(error);
    let limit_options = render_limit_options(limit);
    let refresh_options = render_refresh_options(refresh);

    UI_TEMPLATE
        .replace("__TITLE__", &escaped_title)
        .replace("__LIMIT__", &limit.to_string())
        .replace("__ACCESS__", &escaped_access)
        .replace("__ERROR__", &escaped_error)
        .replace("__LIMIT_OPTIONS__", &limit_options)
        .replace("__REFRESH_OPTIONS__", &refresh_options)
}

fn render_limit_options(current: usize) -> String {
    let choices = [50usize, 100, 200, 500, 1000];
    let mut out = String::new();
    for &value in &choices {
        let selected = if value == current { " selected" } else { "" };
        out.push_str(&format!(
            r#"<option value="{value}"{selected}>{value}</option>"#,
            value = value,
            selected = selected
        ));
    }
    out
}

fn render_refresh_options(current: u64) -> String {
    let choices = [5u64, 10, 30, 60];
    let mut out = String::new();
    for &value in &choices {
        let selected = if value == current { " selected" } else { "" };
        out.push_str(&format!(
            r#"<option value="{value}"{selected}>{value}s</option>"#,
            value = value,
            selected = selected
        ));
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/* ===========================
 *  /healthz
 * ===========================
 */

#[derive(Serialize)]
struct HealthResponse {
    access_log: FileHealth,
    error_log: FileHealth,
}

#[derive(Serialize)]
struct FileHealth {
    last_update_unix: Option<u64>,
    seconds_since_last_update: Option<u64>,
    lines_seen: u64,
}

async fn healthz_handler(State(state): State<AppState>) -> Json<HealthResponse> {
    let now = SystemTime::now();
    let log_state = state.log_state.read().await;

    let access = build_file_health(
        log_state.last_access_update,
        log_state.access_lines_seen,
        now,
    );
    let error = build_file_health(log_state.last_error_update, log_state.error_lines_seen, now);

    Json(HealthResponse {
        access_log: access,
        error_log: error,
    })
}

fn build_file_health(
    last_update: Option<SystemTime>,
    lines_seen: u64,
    now: SystemTime,
) -> FileHealth {
    let (last_secs, diff_secs) = match last_update {
        Some(t) => {
            let last = system_time_to_unix_secs(t);
            let diff = now
                .duration_since(t)
                .unwrap_or_else(|_| Duration::from_secs(0))
                .as_secs();
            (Some(last), Some(diff))
        }
        None => (None, None),
    };
    FileHealth {
        last_update_unix: last_secs,
        seconds_since_last_update: diff_secs,
        lines_seen,
    }
}

fn system_time_to_unix_secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
}

/* ===========================
 *  Log downloads (gzipped)
 * ===========================
 */

async fn download_access_log_handler(State(state): State<AppState>) -> Response {
    download_log_handler_impl(state.access_log_path.clone(), "access.log.gz").await
}

async fn download_error_log_handler(State(state): State<AppState>) -> Response {
    download_log_handler_impl(state.error_log_path.clone(), "error.log.gz").await
}

async fn download_log_handler_impl(log_path: String, download_name: &'static str) -> Response {
    match tokio::fs::read(&log_path).await {
        Ok(content) => {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            if let Err(e) = encoder.write_all(&content) {
                eprintln!("failed to gzip {}: {}", log_path, e);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            let gz_bytes = match encoder.finish() {
                Ok(bytes) => bytes,
                Err(e) => {
                    eprintln!("failed to finish gzip {}: {}", log_path, e);
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };

            Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, "application/gzip")
                .header("Content-Encoding", "gzip")
                .header(
                    "Content-Disposition",
                    format!("attachment; filename=\"{}\"", download_name),
                )
                .body(gz_bytes.into())
                .unwrap_or_else(|e| {
                    eprintln!("failed to build response for {}: {}", log_path, e);
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                })
        }
        Err(e) => {
            eprintln!("failed to read log file {}: {}", log_path, e);
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/* ===========================
 *  SSE /logs/stream
 * ===========================
 */

async fn logs_sse_handler(
    State(state): State<AppState>,
    Query(params): Query<LogsSseQuery>,
) -> Response {
    let limit = params.limit.unwrap_or(50).min(MAX_LINES_PER_LOG).max(1);
    let interval_secs = params.interval.unwrap_or(10).max(1);

    let stream = stream! {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            ticker.tick().await;

            let (access, error) = {
                let log_state = state.log_state.read().await;
                let access = tail_as_vec(&log_state.access_lines, limit);
                let error = tail_as_vec(&log_state.error_lines, limit);
                (access, error)
            };

            let payload = json!({ "access": access, "error": error });
            let data = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());

            yield Ok::<Event, Infallible>(Event::default().data(data));
        }
    };

    Sse::new(stream).into_response()
}

/* ===========================
 *  /metrics
 * ===========================
 */

async fn metrics_handler() -> Response {
    let metric_families = REGISTRY.gather();
    let encoder = TextEncoder::new();
    let mut buffer = Vec::new();

    if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
        eprintln!("Error encoding metrics: {}", e);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let body = match String::from_utf8(buffer) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("metrics UTF-8 error: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, encoder.format_type())
        .body(body.into())
        .unwrap()
}
