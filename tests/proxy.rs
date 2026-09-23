//! End-to-end: client -> toksqueeze proxy -> mock upstream.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::Router;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use toksqueeze::compressor::Mode;
use toksqueeze::proxy::server::{router, AppState, ProxyConfig};

const CI_LOG: &str = include_str!("fixtures/ci_run.log");

#[derive(Default)]
struct Seen {
    bodies: Vec<(String, HeaderMap, Bytes)>,
    reject_compressed: bool,
}

async fn upstream(State(seen): State<Arc<Mutex<Seen>>>, uri: Uri, headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    let mut s = seen.lock().unwrap();
    let compressed = String::from_utf8_lossy(&body).contains("TOKSQUEEZE");
    s.bodies.push((uri.to_string(), headers, body));
    if s.reject_compressed && compressed {
        return (StatusCode::BAD_REQUEST, [("content-type", "application/json")], r#"{"error":"nope"}"#.to_string());
    }
    // Stand-in for an SSE stream: the proxy must pass it through untouched.
    (StatusCode::OK, [("content-type", "text/event-stream")], "event: message_stop\ndata: {}\n\n".to_string())
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn setup(reject_compressed: bool) -> (String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen { reject_compressed, ..Default::default() }));
    let upstream_url = spawn(Router::new().fallback(upstream).with_state(seen.clone())).await;
    let state = AppState::new(ProxyConfig {
        mode: Mode::Balanced,
        openai_upstream: upstream_url.clone(),
        anthropic_upstream: upstream_url,
        default_price_per_mtok: 3.0,
        keep_verbose_logs: false,
        quiet: true,
    });
    (spawn(router(state)).await, seen)
}

fn anthropic_body() -> Value {
    json!({
        "model": "claude-opus-5",
        "max_tokens": 1024,
        "stream": true,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "Why did CI fail?"},
            {"type": "tool_result", "tool_use_id": "t1", "content": CI_LOG}
        ]}]
    })
}

#[tokio::test]
async fn compresses_anthropic_messages_and_streams_the_response_back() {
    let (proxy, seen) = setup(false).await;
    let original = serde_json::to_vec(&anthropic_body()).unwrap();
    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/messages?beta=true"))
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(original.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    assert_eq!(resp.text().await.unwrap(), "event: message_stop\ndata: {}\n\n");

    let s = seen.lock().unwrap();
    let (uri, headers, body) = &s.bodies[0];
    assert_eq!(uri, "/v1/messages?beta=true");
    assert_eq!(headers["x-api-key"], "test-key");
    assert_eq!(headers["content-length"], body.len().to_string().as_str());
    assert!(body.len() * 2 < original.len(), "forwarded {} of {} bytes", body.len(), original.len());
    let v: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(v["max_tokens"], 1024);
    assert!(v["messages"][0]["content"][1]["content"].as_str().unwrap().contains("ValueError: available cannot go negative"));
}

#[tokio::test]
async fn openai_chat_completions_are_compressed_too() {
    let (proxy, seen) = setup(false).await;
    let body = json!({"model": "gpt-5", "messages": [{"role": "tool", "tool_call_id": "c", "content": CI_LOG}]});
    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .header("authorization", "Bearer sk-test")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let s = seen.lock().unwrap();
    assert!(s.bodies[0].2.len() * 4 < serde_json::to_vec(&body).unwrap().len());
    assert_eq!(s.bodies[0].1["authorization"], "Bearer sk-test");
}

#[tokio::test]
async fn upstream_400_on_compressed_body_retries_the_original() {
    let (proxy, seen) = setup(true).await;
    let original = serde_json::to_vec(&anthropic_body()).unwrap();
    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .body(original.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let s = seen.lock().unwrap();
    assert_eq!(s.bodies.len(), 2);
    assert_eq!(s.bodies[1].2.as_ref(), original.as_slice());
}

#[tokio::test]
async fn other_paths_pass_through_and_stats_are_served() {
    let (proxy, seen) = setup(false).await;
    let client = reqwest::Client::new();
    let resp = client.get(format!("{proxy}/v1/models")).header("anthropic-version", "2023-06-01").send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(seen.lock().unwrap().bodies[0].0, "/v1/models");

    let stats: Value = client.get(format!("{proxy}/toksqueeze/stats")).send().await.unwrap().json().await.unwrap();
    assert!(stats.get("tokens_saved").is_some());
}
