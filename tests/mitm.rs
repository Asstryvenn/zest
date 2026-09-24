//! HTTPS interception: CA generation, domain matching, TLS termination for the
//! API hosts, and untouched pass-through for everything else. Nothing here
//! touches the system trust store or network settings: every client trusts
//! the test CA explicitly.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::Router;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use zest::certs::{is_intercepted, Ca};
use zest::compressor::Mode;
use zest::proxy::server::{run_listener, AppState, ProxyConfig};

const CI_LOG: &str = include_str!("fixtures/ci_run.log");

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("zest-mitm-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

// ---------------------------------------------------------------- the CA

#[test]
fn ca_is_written_with_private_permissions_and_reloads_identically() {
    let dir = tmp("files");
    let ca = Ca::generate(&dir).unwrap();
    assert!(ca.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir.join("rootCA-key.pem")), 0o600, "private key must be owner-only");
        assert_eq!(mode(dir.clone()), 0o700);
    }
    let again = Ca::load(&dir).unwrap();
    assert_eq!(again.cert_pem, ca.cert_pem);
    assert_eq!(again.sha256_hex(), ca.sha256_hex());
    let (reused, created) = Ca::load_or_generate(&dir).unwrap();
    assert!(!created);
    assert_eq!(reused.cert_pem, ca.cert_pem);
    std::fs::remove_dir_all(dir).unwrap();
}

/// TLS handshake between a server presenting `ca.leaf(cert_host)` and a client
/// that trusts only `ca` and expects `sni`.
async fn handshake(ca: &Ca, cert_host: &str, sni: &str) -> Result<(), String> {
    let (chain, key) = ca.leaf(cert_host).unwrap();
    let server = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    let client = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let (a, b) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = TlsAcceptor::from(Arc::new(server)).accept(a).await;
    });
    TlsConnector::from(Arc::new(client))
        .connect(ServerName::try_from(sni.to_string()).unwrap(), b)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tokio::test]
async fn leaf_certificates_verify_for_the_api_hosts() {
    let dir = tmp("leaf");
    let ca = Ca::generate(&dir).unwrap();
    handshake(&ca, "api.anthropic.com", "api.anthropic.com").await.unwrap();
    handshake(&ca, "api.openai.com", "api.openai.com").await.unwrap();
    let wrong = handshake(&ca, "api.openai.com", "api.anthropic.com").await;
    assert!(wrong.is_err(), "a certificate for one host must not verify for the other");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn name_constraints_stop_the_ca_vouching_for_any_other_site() {
    // Even with the CA's private key, a certificate for another domain is rejected
    // by a verifier that enforces name constraints (webpki here; macOS and Chromium too).
    let dir = tmp("constraints");
    let ca = Ca::generate(&dir).unwrap();
    for other in ["example.com", "accounts.google.com", "anthropic.com", "evil.api.anthropic.com.attacker.io"] {
        let r = handshake(&ca, other, other).await;
        let e = r.expect_err(other);
        assert!(e.contains("NameConstraintViolation"), "{other}: {e}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn only_the_two_api_hosts_are_intercepted() {
    assert!(is_intercepted("api.anthropic.com:443"));
    assert!(is_intercepted("api.openai.com:443"));
    for host in ["claude.ai:443", "chatgpt.com:443", "api2.cursor.sh:443", "github.com:443", "127.0.0.1:8443"] {
        assert!(!is_intercepted(host), "{host}");
    }
}

#[test]
fn interception_requires_an_existing_ca() {
    let mut cfg = ProxyConfig::new(Mode::Balanced);
    cfg.mitm_ca_dir = Some(tmp("missing"));
    let err = AppState::new(cfg).err().expect("must refuse without a CA").to_string();
    assert!(err.contains("zest cert install"), "{err}");
}

// ---------------------------------------------------------------- end to end

#[derive(Default)]
struct Seen {
    requests: Vec<(String, HeaderMap, Bytes)>,
}

async fn upstream(State(seen): State<Arc<Mutex<Seen>>>, uri: Uri, headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    seen.lock().unwrap().requests.push((uri.to_string(), headers, body));
    (StatusCode::OK, [("content-type", "text/event-stream")], "event: message_stop\ndata: {}\n\n")
}

async fn spawn_upstream() -> (String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let app = Router::new().fallback(upstream).with_state(seen.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

async fn spawn_proxy(ca_dir: Option<PathBuf>, upstream_override: Option<String>) -> String {
    let mut cfg = ProxyConfig::new(Mode::Balanced);
    cfg.quiet = true;
    cfg.mitm_ca_dir = ca_dir;
    cfg.mitm_upstream_override = upstream_override;
    let state = AppState::new(cfg).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(run_listener(listener, state, std::future::pending()));
    format!("http://{addr}")
}

fn anthropic_body() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": "claude-opus-5",
        "max_tokens": 1024,
        "stream": true,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "Why did CI fail?"},
            {"type": "tool_result", "tool_use_id": "t1", "content": CI_LOG}
        ]}]
    }))
    .unwrap()
}

#[tokio::test]
async fn https_to_api_anthropic_com_is_decrypted_compressed_and_forwarded() {
    let dir = tmp("e2e");
    let ca = Ca::generate(&dir).unwrap();
    let (upstream_url, seen) = spawn_upstream().await;
    let proxy = spawn_proxy(Some(dir.clone()), Some(upstream_url)).await;

    // A client that uses zest as its HTTPS proxy and trusts only the Zest Root CA,
    // the way an app does once the CA is in the system trust store.
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::https(&proxy).unwrap())
        .tls_certs_only([reqwest::Certificate::from_pem(ca.cert_pem.as_bytes()).unwrap()])
        .build()
        .unwrap();
    let original = anthropic_body();
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", "sk-test")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(original.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "event: message_stop\ndata: {}\n\n");

    let s = seen.lock().unwrap();
    let (uri, headers, body) = &s.requests[0];
    assert_eq!(uri, "/v1/messages");
    assert_eq!(headers["x-api-key"], "sk-test");
    assert!(body.len() * 2 < original.len(), "forwarded {} of {} bytes", body.len(), original.len());
    let v: serde_json::Value = serde_json::from_slice(body).unwrap();
    let tool_result = v["messages"][0]["content"][1]["content"].as_str().unwrap();
    assert!(tool_result.contains("ValueError: available cannot go negative for 'A-1'"));
    assert!(tool_result.contains("[ZEST:"));
    std::fs::remove_dir_all(dir).unwrap();
}

/// Open a CONNECT tunnel through the proxy and return the raw stream.
async fn connect_tunnel(proxy: &str, target: &str) -> TcpStream {
    let mut s = TcpStream::connect(proxy.trim_start_matches("http://")).await.unwrap();
    s.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    s
}

#[tokio::test]
async fn other_hosts_get_a_raw_byte_for_byte_tunnel() {
    // An echo server stands in for any non-API site.
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = echo.accept().await.unwrap();
        let (mut r, mut w) = s.split();
        let _ = tokio::io::copy(&mut r, &mut w).await;
    });
    let dir = tmp("tunnel");
    Ca::generate(&dir).unwrap();
    let proxy = spawn_proxy(Some(dir.clone()), None).await; // interception on, but not for this host

    let mut tunnel = connect_tunnel(&proxy, &echo_addr.to_string()).await;
    let payload = CI_LOG.as_bytes();
    tunnel.write_all(payload).await.unwrap();
    let mut back = vec![0u8; payload.len()];
    tunnel.read_exact(&mut back).await.unwrap();
    assert_eq!(back, payload, "tunneled bytes must be untouched (no compression, no TLS)");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn plain_http_proxy_requests_pass_through_unchanged() {
    let (upstream_url, seen) = spawn_upstream().await;
    let proxy = spawn_proxy(None, None).await;
    let client = reqwest::Client::builder().proxy(reqwest::Proxy::http(&proxy).unwrap()).build().unwrap();
    // Even an LLM-shaped path on another host is not compressed.
    let original = anthropic_body();
    let resp = client.post(format!("{upstream_url}/v1/messages")).body(original.clone()).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let s = seen.lock().unwrap();
    assert_eq!(s.requests[0].0, "/v1/messages");
    assert_eq!(s.requests[0].2.as_ref(), original.as_slice());
}

#[tokio::test]
async fn base_url_mode_still_works_alongside_interception() {
    let dir = tmp("both");
    Ca::generate(&dir).unwrap();
    let (upstream_url, seen) = spawn_upstream().await;
    let mut cfg = ProxyConfig::new(Mode::Balanced);
    cfg.quiet = true;
    cfg.mitm_ca_dir = Some(dir.clone());
    cfg.anthropic_upstream = upstream_url;
    let state = AppState::new(cfg).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(run_listener(listener, state, std::future::pending()));

    let original = anthropic_body();
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .body(original.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(seen.lock().unwrap().requests[0].2.len() * 2 < original.len());
    std::fs::remove_dir_all(dir).unwrap();
}
