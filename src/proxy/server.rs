//! The zest proxy. One port, three ways in:
//!
//! 1. **Base-URL mode**: clients set `ANTHROPIC_BASE_URL=http://127.0.0.1:8080` or
//!    `OPENAI_BASE_URL=http://127.0.0.1:8080/v1`; `/v1/messages` and
//!    `/v1/chat/completions` bodies are compressed and sent to the configured upstream.
//! 2. **HTTPS proxy (CONNECT)**: with interception on, `CONNECT api.anthropic.com:443`
//!    and `CONNECT api.openai.com:443` are decrypted with the local Zest Root CA,
//!    compressed and re-encrypted to the real API. Every other host is a raw TCP
//!    tunnel: zest never sees inside it (see `mitm.rs`).
//! 3. **Plain HTTP proxy**: absolute-URL requests (`GET http://example.com/`) are
//!    forwarded unchanged.
//!
//! Responses, including SSE streams, are streamed back untouched.

use crate::certs::INTERCEPT_HOSTS;
use crate::compressor::{CompressorEngine, Mode};
use crate::proxy::handlers::{self, ApiFormat};
use crate::proxy::mitm::{self, Interceptor};
use crate::tui::banner::{self, BannerInfo};
use crate::tui::stats::{self, Stats};
use crate::tui::theme::{self, ELECTRIC, FLAME, MUTED, SPARK};
use axum::body::{Body, Bytes};
use axum::response::{IntoResponse, Response};
use http::{header, HeaderMap, HeaderName, Method, Request, StatusCode};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub mode: Mode,
    pub openai_upstream: String,
    pub anthropic_upstream: String,
    /// Fallback input price when the model isn't in the price table.
    pub default_price_per_mtok: f64,
    pub keep_verbose_logs: bool,
    pub quiet: bool,
    /// Decrypt HTTPS to the API hosts using the CA in this directory. `None`
    /// means every CONNECT is a plain tunnel.
    pub mitm_ca_dir: Option<PathBuf>,
    /// Test hook: send intercepted requests here instead of `https://<host>`.
    pub mitm_upstream_override: Option<String>,
    /// Point the macOS system proxy at zest while it runs, restoring it on exit.
    pub system_proxy: bool,
}

impl ProxyConfig {
    pub fn new(mode: Mode) -> Self {
        ProxyConfig {
            mode,
            openai_upstream: "https://api.openai.com".into(),
            anthropic_upstream: "https://api.anthropic.com".into(),
            default_price_per_mtok: 3.0,
            keep_verbose_logs: false,
            quiet: false,
            mitm_ca_dir: None,
            mitm_upstream_override: None,
            system_proxy: false,
        }
    }
}

pub struct AppState {
    pub cfg: ProxyConfig,
    pub engine: CompressorEngine,
    pub client: reqwest::Client,
    pub stats: Stats,
    pub mitm: Option<Interceptor>,
}

impl AppState {
    pub fn new(cfg: ProxyConfig) -> std::io::Result<Arc<Self>> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(90))
            // zest may itself be the system proxy: never route our own upstream calls through one.
            .no_proxy()
            .build()
            .map_err(std::io::Error::other)?;
        let engine = CompressorEngine::new(cfg.mode).with_verbose_logs(cfg.keep_verbose_logs);
        let mitm = match &cfg.mitm_ca_dir {
            Some(dir) => Some(Interceptor::new(dir, cfg.mitm_upstream_override.clone())?),
            None => None,
        };
        Ok(Arc::new(Self { cfg, engine, client, stats: Stats::default(), mitm }))
    }
}

const MAX_BODY: usize = 256 * 1024 * 1024;

/// Accept connections until `shutdown` resolves.
pub async fn run_listener(listener: TcpListener, st: Arc<AppState>, shutdown: impl Future<Output = ()>) {
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                tokio::spawn(serve_connection(stream, st.clone()));
            }
        }
    }
}

async fn serve_connection(stream: TcpStream, st: Arc<AppState>) {
    let _ = stream.set_nodelay(true);
    let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
        let st = st.clone();
        async move { Ok::<_, Infallible>(dispatch(st, req.map(Body::new)).await) }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .serve_connection(TokioIo::new(stream), svc)
        .with_upgrades()
        .await;
}

async fn dispatch(st: Arc<AppState>, req: Request<Body>) -> Response {
    if req.method() == Method::CONNECT {
        return mitm::connect(st, req).await;
    }
    if let (Some(scheme), Some(authority)) = (req.uri().scheme_str(), req.uri().authority()) {
        let base = format!("{scheme}://{authority}");
        let label = base.clone();
        return forward(st, req, &base, None, label).await;
    }
    local(st, req).await
}

/// Ctrl+C, `kill` (SIGTERM) or closing the terminal (SIGHUP).
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub async fn serve(addr: SocketAddr, cfg: ProxyConfig) -> std::io::Result<()> {
    let state = AppState::new(cfg)?;
    stats::warm_up();
    // Compile regexes and build the tree-sitter parsers before the first request.
    let warm = "INFO a\nINFO b\nINFO c\nINFO d\nERROR e\nfix `f`\n```py\ndef f():\n    x = 1\n    y = 2\n    return x\n```\n```ts\nfunction g() {\n  a();\n  b();\n  c();\n}\n```\nTraceback (most recent call last):\n";
    let engine = &state.engine;
    let _ = engine.compress(warm, crate::compressor::BlockKind::Human, &engine.focus_for([warm]));
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let mode = format!("{:?}", state.cfg.mode).to_lowercase();
    eprintln!(
        "{}",
        banner::render(&BannerInfo {
            addr: local_addr.to_string(),
            mode: &mode,
            anthropic: &state.cfg.anthropic_upstream,
            openai: &state.cfg.openai_upstream,
        })
    );

    let hosts = INTERCEPT_HOSTS.join(", ");
    let mut guard = None;
    if state.cfg.system_proxy {
        match enable_system_proxy(local_addr.port()).await {
            Ok((g, services)) => {
                guard = Some(g);
                eprintln!(
                    "{} {} {} {}",
                    theme::tag(),
                    theme::paint("System Proxy:", MUTED),
                    theme::bold("ACTIVE", ELECTRIC),
                    theme::paint(&format!("({hosts})"), SPARK)
                );
                eprintln!(
                    "{} {}",
                    theme::tag(),
                    theme::paint(&format!("routing {} through zest; restored when zest stops", services.join(", ")), MUTED)
                );
            }
            Err(e) => eprintln!("{} {}", theme::tag(), theme::paint(&format!("System Proxy: not enabled ({e})"), FLAME)),
        }
    }
    if state.mitm.is_some() && guard.is_none() {
        eprintln!(
            "{} {} {}",
            theme::tag(),
            theme::paint("HTTPS interception: ON for", MUTED),
            theme::paint(&format!("{hosts} (for apps using HTTPS_PROXY=http://{local_addr})"), SPARK)
        );
    }

    run_listener(listener, state.clone(), shutdown_signal()).await;

    if let Some(mut g) = guard {
        match tokio::task::spawn_blocking(move || g.restore()).await {
            Ok(Ok(_)) => eprintln!("\n{} {}", theme::tag(), theme::paint("System Proxy: restored your previous settings", SPARK)),
            _ => eprintln!("\n{} {}", theme::tag(), theme::paint("could not restore the system proxy; run `zest system-proxy off`", FLAME)),
        }
    }
    eprintln!("\n{}", state.stats.summary_line());
    Ok(())
}

#[cfg(target_os = "macos")]
async fn enable_system_proxy(
    port: u16,
) -> std::io::Result<(crate::sysproxy::Guard<crate::platform::SystemRunner>, Vec<String>)> {
    let path = crate::sysproxy::state_path().ok_or_else(|| std::io::Error::other("no config directory"))?;
    tokio::task::spawn_blocking(move || {
        crate::sysproxy::Guard::enable(crate::platform::SystemRunner, "127.0.0.1", port, path)
    })
    .await
    .map_err(std::io::Error::other)?
}

#[cfg(not(target_os = "macos"))]
async fn enable_system_proxy(
    _port: u16,
) -> std::io::Result<(crate::sysproxy::Guard<crate::platform::SystemRunner>, Vec<String>)> {
    Err(std::io::Error::other("the system proxy switch is macOS-only; set HTTPS_PROXY instead"))
}

fn classify(path: &str, headers: &HeaderMap) -> (Option<ApiFormat>, bool) {
    let anthropic_headers = headers.contains_key("anthropic-version") || headers.contains_key("x-api-key");
    match path {
        "/v1/messages" | "/v1/messages/count_tokens" => (Some(ApiFormat::Anthropic), true),
        p if p.ends_with("/chat/completions") => (Some(ApiFormat::OpenAI), false),
        _ => (None, anthropic_headers),
    }
}

/// Which request format a path on an intercepted host uses, if any.
pub(crate) fn format_for(path: &str, headers: &HeaderMap) -> Option<ApiFormat> {
    classify(path, headers).0
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection" | "keep-alive" | "proxy-authenticate" | "proxy-authorization" | "proxy-connection" | "te"
            | "trailer" | "transfer-encoding" | "upgrade" | "host" | "content-length"
    )
}

pub(crate) fn json_error(status: StatusCode, msg: &str) -> Response {
    let body = serde_json::json!({"type": "error", "error": {"type": "zest_proxy_error", "message": msg}});
    (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

/// Requests addressed to zest itself (base-URL mode and zest's own endpoints).
async fn local(st: Arc<AppState>, req: Request<Body>) -> Response {
    let path = req.uri().path().to_string();
    if path == "/zest/stats" {
        return axum::Json(st.stats.snapshot()).into_response();
    }
    if path == "/zest/health" {
        return "ok".into_response();
    }
    let (format, to_anthropic) = classify(&path, req.headers());
    let base = if to_anthropic { st.cfg.anthropic_upstream.clone() } else { st.cfg.openai_upstream.clone() };
    forward(st, req, &base, format, path).await
}

/// Send `req` to `base` + its path, compressing the body when `format` says
/// it's an LLM request.
pub(crate) async fn forward(
    st: Arc<AppState>,
    req: Request<Body>,
    base: &str,
    format: Option<ApiFormat>,
    label: String,
) -> Response {
    let (parts, body) = req.into_parts();
    let original = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(e) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, &format!("reading request body: {e}")),
    };
    let path_q = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    let url = format!("{}{}", base.trim_end_matches('/'), path_q);

    // Compress only plain JSON POST bodies.
    let encoded = parts.headers.contains_key(header::CONTENT_ENCODING);
    let rewrite = match format {
        Some(fmt) if parts.method == Method::POST && !encoded && !original.is_empty() => {
            let st2 = st.clone();
            let bytes = original.clone();
            tokio::task::spawn_blocking(move || handlers::rewrite(&bytes, fmt, &st2.engine)).await.ok().flatten()
        }
        _ => None,
    };
    let compressed: Option<Bytes> = rewrite.as_ref().and_then(|r| r.body.clone()).map(Bytes::from);

    let mut headers = HeaderMap::new();
    for (k, v) in parts.headers.iter() {
        // Ask upstream for an identity-encoded response so it can be streamed through untouched.
        if !is_hop_by_hop(k) && (format.is_none() || k != header::ACCEPT_ENCODING) {
            headers.append(k.clone(), v.clone());
        }
    }

    let send = |body: Bytes| st.client.request(parts.method.clone(), &url).headers(headers.clone()).body(body).send();
    let mut resp = match send(compressed.clone().unwrap_or_else(|| original.clone())).await {
        Ok(r) => r,
        Err(e) => return json_error(StatusCode::BAD_GATEWAY, &format!("upstream {url}: {e}")),
    };

    // Tier 4 fallback: if the provider rejects the rewritten body, retry it verbatim.
    if resp.status() == StatusCode::BAD_REQUEST && compressed.is_some() {
        let detail = resp.text().await.unwrap_or_default();
        eprintln!(
            "{} {} {}",
            theme::tag(),
            theme::paint("upstream rejected the compressed body (400), retrying uncompressed:", FLAME),
            detail.chars().take(200).collect::<String>()
        );
        resp = match send(original.clone()).await {
            Ok(r) => r,
            Err(e) => return json_error(StatusCode::BAD_GATEWAY, &format!("upstream {url}: {e}")),
        };
    } else if let Some(rw) = rewrite {
        if !label.ends_with("/v1/messages/count_tokens") {
            report(st.clone(), rw, label);
        }
    }

    let mut out = Response::builder().status(resp.status());
    for (k, v) in resp.headers() {
        if !is_hop_by_hop(k) {
            out = out.header(k, v);
        }
    }
    out.body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|e| json_error(StatusCode::BAD_GATEWAY, &e.to_string()))
}

/// Count tokens off the request path and print the one-line readout.
fn report(st: Arc<AppState>, rw: handlers::Rewrite, label: String) {
    tokio::task::spawn_blocking(move || {
        let before = stats::count_tokens(&rw.text_before);
        let after = if rw.body.is_some() { stats::count_tokens(&rw.text_after) } else { before };
        let price = stats::input_price_per_mtok(rw.model.as_deref(), st.cfg.default_price_per_mtok);
        let usd = before.saturating_sub(after) as f64 * price / 1e6;
        st.stats.record(before, after, usd);
        if !st.cfg.quiet {
            let suffix = format!("{} {}", rw.model.as_deref().unwrap_or("?"), label);
            eprintln!("{}", stats::format_line(before, after, usd, rw.elapsed.as_secs_f64() * 1e3, &suffix));
        }
    });
}
