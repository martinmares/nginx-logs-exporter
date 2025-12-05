use std::{
    collections::VecDeque,
    env,
    sync::Arc,
    time::{Duration, SystemTime},
};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{StatusCode, header::CONTENT_TYPE},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use lazy_static::lazy_static;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, Opts, Registry, TextEncoder,
    linear_buckets,
};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncSeekExt, BufReader},
    sync::RwLock,
    time::sleep,
};

const MAX_LINES_PER_LOG: usize = 2000;

#[derive(Clone)]
struct AppState {
    log_state: Arc<RwLock<LogState>>,
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
 *  METRIKY
 * ===========================
 */

lazy_static! {
    // HTTP status kódy (z pohledu klienta)
    static ref HTTP_STATUS_TOTAL: IntCounterVec = IntCounterVec::new(
        Opts::new(
            "nginx_http_status_total",
            "Total number of responses by HTTP status code"
        ),
        &["status"]
    )
    .unwrap();

    // HTTP status třídy (2xx, 4xx, 5xx…)
    static ref HTTP_STATUS_CLASS_TOTAL: IntCounterVec = IntCounterVec::new(
        Opts::new(
            "nginx_http_status_class_total",
            "Total number of responses by HTTP status class"
        ),
        &["class"]
    )
    .unwrap();

    // Všechny 502 z pohledu klienta
    static ref HTTP_502_TOTAL: IntCounter =
        IntCounter::new("nginx_http_502_total", "Number of HTTP 502 Bad Gateway responses")
            .unwrap();

    // Metriky per upstream: kolik odpovědí a jaké upstream statusy
    static ref UPSTREAM_STATUS_TOTAL: IntCounterVec = IntCounterVec::new(
        Opts::new(
            "nginx_upstream_status_total",
            "Total upstream responses by upstream_addr and status"
        ),
        &["upstream_addr", "status"]
    )
    .unwrap();

    // Histogram request_time podle upstream_addr
    // request_time je v sekundách (float), buckets: 10ms..~1s
    static ref HTTP_REQUEST_DURATION_SECONDS: HistogramVec = {
        let buckets = linear_buckets(0.01, 0.05, 20)  // 0.01, 0.06, 0.11, ... ~1.0s
            .expect("invalid histogram buckets");
        let opts = HistogramOpts::new(
            "nginx_http_request_duration_seconds",
            "Histogram of Nginx request_time in seconds, labelled by upstream_addr"
        ).buckets(buckets);
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

    // Prefix pro UI (např. "/es-proxy")
    let ui_prefix_raw = env::var("NGINX_UI_PUBLIC_PREFIX").unwrap_or_default();
    let ui_prefix = normalize_prefix(&ui_prefix_raw);

    let state = AppState {
        log_state: Arc::new(RwLock::new(LogState::default())),
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

    // Router – základní cesty
    let mut app = Router::new()
        .route("/", get(ui_handler))
        .route("/ui", get(ui_handler))
        .route("/healthz", get(healthz_handler))
        .route("/metrics", get(metrics_handler));

    // Pokud máme prefix (např. "/es-proxy"), přidáme i prefixované UI routy
    if let Some(prefix) = ui_prefix {
        let prefix_root = prefix.clone();
        let prefix_ui = format!("{}/ui", prefix_root);
        println!(
            "Registering UI routes with prefix: '{}' and '{}'",
            prefix_root, prefix_ui
        );
        app = app
            .route(&prefix_root, get(ui_handler))
            .route(&prefix_ui, get(ui_handler));
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

                // Chceme chování jako tail -F: začít od konce
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
                            // EOF – zkontrolujeme rotaci / truncnutí
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
        // update ring bufferu pro UI + healthz
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

    // Access log -> parsujeme a plníme metriky
    if let LogKind::Access = kind {
        let parsed = parser::parse_access_line(line);
        metrics_layer::record_access(&parsed);
    }
}

/* ===========================
 *  PARSER
 * ===========================
 */

mod parser {
    #[allow(unused_imports)]
    use super::*;

    #[derive(Debug, Clone)]
    pub struct ParsedAccess {
        #[allow(dead_code)]
        pub raw: String,
        pub status: Option<u16>,
        pub upstream_addr: Option<String>,
        pub upstream_status: Option<u16>,
        pub request_time: Option<f64>,
    }

    pub fn parse_access_line(line: &str) -> ParsedAccess {
        let status = parse_status_code_from_access(line);

        // upstream_status=200,502,404 → bereme první (200)
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

        ParsedAccess {
            raw: line.to_string(),
            status,
            upstream_addr,
            upstream_status,
            request_time,
        }
    }

    // Status z Nginx combined formátu:
    // <ip> - <user> [time] "<request>" <status> ...
    fn parse_status_code_from_access(line: &str) -> Option<u16> {
        let first_quote = line.find('"')?;
        let rest = &line[first_quote + 1..];
        let second_quote_rel = rest.find('"')?;
        let after_request = &rest[second_quote_rel + 1..];
        let mut parts = after_request.split_whitespace();
        let status_str = parts.next()?;
        status_str.parse::<u16>().ok()
    }

    fn extract_kv_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
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

        // Upstream label
        let upstream_label = parsed.upstream_addr.as_deref().unwrap_or("<none>");

        // Upstream status
        if let Some(us) = parsed.upstream_status {
            let s = us.to_string();
            UPSTREAM_STATUS_TOTAL
                .with_label_values(&[upstream_label, &s])
                .inc();
        }

        // Histogram request_time (sekundy) podle upstream_addr
        if let Some(rt) = parsed.request_time {
            HTTP_REQUEST_DURATION_SECONDS
                .with_label_values(&[upstream_label])
                .observe(rt);
        }
    }
}

/* ===========================
 *  UI (/ a /ui [+ prefix])
 * ===========================
 */

#[derive(Deserialize)]
struct UiQuery {
    limit: Option<usize>,
}

async fn ui_handler(State(state): State<AppState>, Query(params): Query<UiQuery>) -> Html<String> {
    let limit = params.limit.unwrap_or(50).min(MAX_LINES_PER_LOG).max(1);

    let log_state = state.log_state.read().await;

    let access_text = tail_as_string(&log_state.access_lines, limit);
    let error_text = tail_as_string(&log_state.error_lines, limit);

    let html = render_ui_page(limit, &access_text, &error_text);
    Html(html)
}

fn tail_as_string(lines: &VecDeque<String>, limit: usize) -> String {
    let len = lines.len();
    let start = len.saturating_sub(limit);
    lines
        .iter()
        .skip(start)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_ui_page(limit: usize, access: &str, error: &str) -> String {
    let escaped_access = html_escape(access);
    let escaped_error = html_escape(error);
    let options = render_limit_options(limit);

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <title>Nginx log viewer</title>
  <style>
    body {{ font-family: sans-serif; margin: 1rem; }}
    h1 {{ margin-bottom: 0.5rem; }}
    .logs-container {{ display: flex; gap: 1rem; }}
    .log-box {{ flex: 1; min-width: 0; }}
    pre {{ background: #111; color: #eee; padding: 0.5rem; border-radius: 4px;
          max-height: 70vh; overflow-y: auto; font-size: 12px; }}
    form {{ margin-bottom: 1rem; }}
    label {{ margin-right: 0.5rem; }}
    select {{ padding: 0.2rem; }}
    button {{ padding: 0.2rem 0.6rem; }}
  </style>
</head>
<body>
  <h1>Nginx logs</h1>
  <form method="get" action="">
    <label for="limit">Lines per log:</label>
    <select name="limit" id="limit">
      {options}
    </select>
    <button type="submit">Reload</button>
  </form>
  <div class="logs-container">
    <div class="log-box">
      <h2>access.log (last {limit} lines)</h2>
      <pre>{access}</pre>
    </div>
    <div class="log-box">
      <h2>error.log (last {limit} lines)</h2>
      <pre>{error}</pre>
    </div>
  </div>
</body>
</html>
"#,
        limit = limit,
        access = escaped_access,
        error = escaped_error,
        options = options,
    )
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
 *  /metrics
 * ===========================
 */

async fn metrics_handler() -> Response {
    // Tady se *jen* sahá do metrik v paměti – žádné čtení logů.
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
