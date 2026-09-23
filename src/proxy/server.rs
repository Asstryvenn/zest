//! Transparent HTTP proxy. Point a client's base URL at it
//! (`ANTHROPIC_BASE_URL=http://127.0.0.1:8080`, `OPENAI_BASE_URL=http://127.0.0.1:8080/v1`);
//! request bodies for `/v1/messages` and `/v1/chat/completions` are compressed,
//! everything else — including streamed responses — passes through byte for byte.

use crate::compressor::{CompressorEngine, Mode};
use crate::proxy::handlers::{self, ApiFormat};
use crate::tui::stats::{self, Stats};
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::Router;
use http::{header, HeaderMap, HeaderName, Method, StatusCode};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub mode: Mode,
    pub openai_upstream: String,
    pub anthropic_upstream: String,
    /// Fallback input price when the model isn't in the price table.
    pub default_price_per_mtok: f64,
    pub keep_verbose_logs: bool,
    pub quiet: bool,
}

pub struct AppState {
    pub cfg: ProxyConfig,
    pub engine: CompressorEngine,
    pub client: reqwest::Client,
    pub stats: Stats,
}

impl AppState {
    pub fn new(cfg: ProxyConfig) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("HTTP client");
        let engine = CompressorEngine::new(cfg.mode).with_verbose_logs(cfg.keep_verbose_logs);
        Arc::new(Self { cfg, engine, client, stats: Stats::default() })
    }
}

const MAX_BODY: usize = 256 * 1024 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new().fallback(proxy).with_state(state)
}

pub async fn serve(addr: SocketAddr, cfg: ProxyConfig) -> std::io::Result<()> {
    let state = AppState::new(cfg);
    stats::warm_up();
    // Compile regexes and build the tree-sitter parsers before the first request.
    let warm = "INFO a\nINFO b\nINFO c\nINFO d\nERROR e\nfix `f`\n```py\ndef f():\n    x = 1\n    y = 2\n    return x\n```\n```ts\nfunction g() {\n  a();\n  b();\n  c();\n}\n```\nTraceback (most recent call last):\n";
    let engine = &state.engine;
    let _ = engine.compress(warm, crate::compressor::BlockKind::Human, &engine.focus_for([warm]));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!(
        "[TokSqueeze] listening on http://{} (mode: {:?})\n  Anthropic -> {}\n  OpenAI    -> {}\n  export ANTHROPIC_BASE_URL=http://{0}\n  export OPENAI_BASE_URL=http://{0}/v1",
        listener.local_addr()?,
        state.cfg.mode,
        state.cfg.anthropic_upstream,
        state.cfg.openai_upstream
    );
    let summary_state = state.clone();
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    eprintln!("\n{}", summary_state.stats.summary_line());
    Ok(())
}

fn classify(path: &str, headers: &HeaderMap) -> (Option<ApiFormat>, bool) {
    let anthropic_headers = headers.contains_key("anthropic-version") || headers.contains_key("x-api-key");
    match path {
        "/v1/messages" | "/v1/messages/count_tokens" => (Some(ApiFormat::Anthropic), true),
        p if p.ends_with("/chat/completions") => (Some(ApiFormat::OpenAI), false),
        _ => (None, anthropic_headers),
    }
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection" | "keep-alive" | "proxy-authenticate" | "proxy-authorization" | "te" | "trailer"
            | "transfer-encoding" | "upgrade" | "host" | "content-length"
    )
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    let body = serde_json::json!({"type": "error", "error": {"type": "toksqueeze_proxy_error", "message": msg}});
    (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

async fn proxy(State(st): State<Arc<AppState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    if path == "/toksqueeze/stats" {
        return axum::Json(st.stats.snapshot()).into_response();
    }
    if path == "/toksqueeze/health" {
        return "ok".into_response();
    }

    let original = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(e) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, &format!("reading request body: {e}")),
    };

    let (format, to_anthropic) = classify(&path, &parts.headers);
    let base = if to_anthropic { &st.cfg.anthropic_upstream } else { &st.cfg.openai_upstream };
    let path_q = parts.uri.path_and_query().map_or(path.as_str(), |p| p.as_str());
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
        if !is_hop_by_hop(k) && k != header::ACCEPT_ENCODING {
            headers.append(k.clone(), v.clone());
        }
    }

    let send = |body: Bytes| {
        st.client.request(parts.method.clone(), &url).headers(headers.clone()).body(body).send()
    };
    let mut resp = match send(compressed.clone().unwrap_or_else(|| original.clone())).await {
        Ok(r) => r,
        Err(e) => return json_error(StatusCode::BAD_GATEWAY, &format!("upstream {url}: {e}")),
    };

    // Tier 4 fallback: if the provider rejects the rewritten body, retry it verbatim.
    if resp.status() == StatusCode::BAD_REQUEST && compressed.is_some() {
        let detail = resp.text().await.unwrap_or_default();
        eprintln!(
            "[TokSqueeze] upstream rejected compressed body (400), retrying uncompressed: {}",
            detail.chars().take(200).collect::<String>()
        );
        resp = match send(original.clone()).await {
            Ok(r) => r,
            Err(e) => return json_error(StatusCode::BAD_GATEWAY, &format!("upstream {url}: {e}")),
        };
    } else if let Some(rw) = rewrite {
        if path != "/v1/messages/count_tokens" {
            report(st.clone(), rw, path.clone());
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
fn report(st: Arc<AppState>, rw: handlers::Rewrite, path: String) {
    tokio::task::spawn_blocking(move || {
        let before = stats::count_tokens(&rw.text_before);
        let after = if rw.body.is_some() { stats::count_tokens(&rw.text_after) } else { before };
        let price = stats::input_price_per_mtok(rw.model.as_deref(), st.cfg.default_price_per_mtok);
        let usd = before.saturating_sub(after) as f64 * price / 1e6;
        st.stats.record(before, after, usd);
        if !st.cfg.quiet {
            let suffix = format!("{} {}", rw.model.as_deref().unwrap_or("?"), path);
            eprintln!("{}", stats::format_line(before, after, usd, rw.elapsed.as_secs_f64() * 1e3, &suffix));
        }
    });
}
