//! `CONNECT` handling: TLS interception for the LLM API hosts, raw tunnels for
//! everything else.
//!
//! For `api.anthropic.com` / `api.openai.com` (when interception is on), zest
//! answers the TLS handshake itself with a certificate signed by the local
//! Zest Root CA, reads the HTTP requests inside, compresses them like
//! base-URL mode does, and sends them on to the real API over a new, fully
//! verified TLS connection.
//!
//! Every other `CONNECT` becomes a plain TCP tunnel: bytes are copied in both
//! directions and never decrypted, parsed or logged.

use crate::certs::{is_intercepted, Ca};
use crate::proxy::server::{self, AppState};
use crate::tui::theme::{self, FLAME};
use axum::body::Body;
use axum::response::Response;
use http::{Request, StatusCode};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use rustls::ServerConfig;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;

pub struct Interceptor {
    ca: Ca,
    configs: Mutex<HashMap<String, Arc<ServerConfig>>>,
    upstream_override: Option<String>,
    warned: Mutex<HashSet<String>>,
}

impl Interceptor {
    /// Load the CA from `dir`. Fails if it hasn't been created (`zest cert install`).
    pub fn new(dir: &Path, upstream_override: Option<String>) -> io::Result<Self> {
        if !Ca::exists(dir) {
            return Err(io::Error::other(format!(
                "no Zest Root CA in {}. Run `zest cert install` first",
                dir.display()
            )));
        }
        Ok(Interceptor {
            ca: Ca::load(dir)?,
            configs: Mutex::default(),
            upstream_override,
            warned: Mutex::default(),
        })
    }

    /// TLS server config for `host`, with a leaf certificate made on first use.
    pub fn server_config(&self, host: &str) -> io::Result<Arc<ServerConfig>> {
        if let Some(c) = self.configs.lock().unwrap().get(host) {
            return Ok(c.clone());
        }
        let (chain, key) = self.ca.leaf(host)?;
        let mut cfg = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(io::Error::other)?;
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        let cfg = Arc::new(cfg);
        self.configs.lock().unwrap().insert(host.to_string(), cfg.clone());
        Ok(cfg)
    }

    fn upstream_for(&self, host: &str) -> String {
        self.upstream_override.clone().unwrap_or_else(|| format!("https://{host}"))
    }

    /// Print the "app doesn't trust the CA" hint once per host.
    fn warn_once(&self, host: &str, e: &io::Error) {
        if self.warned.lock().unwrap().insert(host.to_string()) {
            eprintln!(
                "{} {}",
                theme::tag(),
                theme::paint(
                    &format!(
                        "an app refused zest's certificate for {host} ({e}). It doesn't trust Zest Root CA: run `zest cert install`, or use base-URL mode for that app."
                    ),
                    FLAME
                )
            );
        }
    }
}

fn empty(status: StatusCode) -> Response {
    Response::builder().status(status).body(Body::empty()).unwrap()
}

/// Split `host:port`, defaulting to 443.
fn host_port(authority: &str) -> (String, u16) {
    match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.trim_matches(['[', ']']).to_string(), p.parse().unwrap_or(443))
        }
        _ => (authority.to_string(), 443),
    }
}

pub async fn connect(st: Arc<AppState>, req: Request<Body>) -> Response {
    let Some(authority) = req.uri().authority().map(|a| a.as_str().to_string()) else {
        return empty(StatusCode::BAD_REQUEST);
    };
    let (host, port) = host_port(&authority);

    if let Some(mitm) = st.mitm.as_ref().filter(|_| is_intercepted(&authority)) {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let cfg = match mitm.server_config(&host) {
            Ok(c) => c,
            Err(e) => return server::json_error(StatusCode::BAD_GATEWAY, &format!("certificate for {host}: {e}")),
        };
        tokio::spawn(async move {
            if let Ok(upgraded) = hyper::upgrade::on(req).await {
                intercept(st, TokioIo::new(upgraded), host, cfg).await;
            }
        });
        return empty(StatusCode::OK);
    }

    // Not an API host: dial it first so failures surface as a 502, then splice bytes.
    let upstream = match TcpStream::connect((host.as_str(), port)).await {
        Ok(s) => s,
        Err(_) => return empty(StatusCode::BAD_GATEWAY),
    };
    let _ = upstream.set_nodelay(true);
    tokio::spawn(async move {
        if let Ok(upgraded) = hyper::upgrade::on(req).await {
            let mut client = TokioIo::new(upgraded);
            let mut upstream = upstream;
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        }
    });
    empty(StatusCode::OK)
}

/// Terminate TLS for an intercepted host and serve the HTTP requests inside it.
async fn intercept<IO>(st: Arc<AppState>, io: IO, host: String, cfg: Arc<ServerConfig>)
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let tls = match TlsAcceptor::from(cfg).accept(io).await {
        Ok(t) => t,
        Err(e) => {
            if let Some(m) = &st.mitm {
                m.warn_once(&host, &e);
            }
            return;
        }
    };
    let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
        let st = st.clone();
        let host = host.clone();
        async move {
            let base = st.mitm.as_ref().map(|m| m.upstream_for(&host)).unwrap_or_else(|| format!("https://{host}"));
            let format = server::format_for(req.uri().path(), req.headers());
            let label = format!("{host}{}", req.uri().path());
            Ok::<_, Infallible>(server::forward(st, req.map(Body::new), &base, format, label).await)
        }
    });
    let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tls), svc).await;
}

#[cfg(test)]
mod tests {
    use super::host_port;

    #[test]
    fn splits_authorities() {
        assert_eq!(host_port("api.anthropic.com:443"), ("api.anthropic.com".into(), 443));
        assert_eq!(host_port("example.com"), ("example.com".into(), 443));
        assert_eq!(host_port("[::1]:8443"), ("::1".into(), 8443));
    }
}
