//! `HyperTransport` against real loopback servers.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use codelib_http::{
    Egress, EgressError, HyperTransport, Rule, StaticCredential, Transport, http::Method,
};
use common::*;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;

type ServerBody = http_body_util::combinators::BoxBody<Bytes, codelib_http::BoxError>;

/// Echo server: replies `"<METHOD> <path> host=<Host> auth=<Authorization>"`;
/// `/stream` streams 3 x 1000 bytes without a Content-Length.
async fn handle(req: hyper::Request<Incoming>) -> Result<hyper::Response<ServerBody>, Infallible> {
    if req.uri().path() == "/stream" {
        static CHUNK: [u8; 1000] = [b'x'; 1000];
        let body = Chunks::new([CHUNK.as_slice(), CHUNK.as_slice(), CHUNK.as_slice()]).boxed();
        return Ok(hyper::Response::new(body));
    }
    let header = |name: &str| {
        req.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    };
    let text = format!(
        "{} {} host={} auth={}",
        req.method(),
        req.uri().path(),
        header("host"),
        header("authorization")
    );
    let body = Full::new(Bytes::from(text))
        .map_err(|never| match never {})
        .boxed();
    Ok(hyper::Response::new(body))
}

/// Start a server on 127.0.0.1; returns its address and a connection counter.
async fn start(tls: Option<TlsAcceptor>) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let conns = Arc::new(AtomicUsize::new(0));
    let counter = conns.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let tls = tls.clone();
            tokio::spawn(async move {
                let svc = service_fn(handle);
                match tls {
                    Some(acceptor) => {
                        if let Ok(s) = acceptor.accept(stream).await {
                            let _ = http1::Builder::new()
                                .serve_connection(TokioIo::new(s), svc)
                                .await;
                        }
                    }
                    None => {
                        let _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), svc)
                            .await;
                    }
                }
            });
        }
    });
    (addr, conns)
}

/// Self-signed certificate for `localhost` and a TLS acceptor using it.
fn tls_fixture() -> (Vec<u8>, TlsAcceptor) {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert = ck.cert.der().clone();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        ck.key_pair.serialize_der(),
    ));
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone()], key)
    .unwrap();
    (cert.to_vec(), TlsAcceptor::from(Arc::new(config)))
}

async fn text(resp: codelib_http::Response) -> String {
    String::from_utf8(read_body(resp).await.unwrap().to_vec()).unwrap()
}

#[tokio::test]
async fn loopback_refused_when_private_ips_denied() {
    let (addr, conns) = start(None).await;
    let t = HyperTransport::new();
    for url in [
        format!("http://127.0.0.1:{}/", addr.port()),
        format!("http://localhost:{}/", addr.port()),
        format!("http://[::ffff:127.0.0.1]:{}/", addr.port()),
    ] {
        let r = t.send(get(&url)).await;
        assert!(matches!(r, Err(EgressError::Denied { .. })), "{url}: {r:?}");
    }

    // Through the full pipeline, with the default builder setting.
    let e = Egress::builder()
        .allow(Rule::host("localhost").https_only(false))
        .build();
    let r = e
        .send(get(&format!("http://localhost:{}/", addr.port())))
        .await;
    let reason = assert_denied(r);
    assert!(reason.contains("public"), "{reason}");
    assert_eq!(conns.load(Ordering::SeqCst), 0, "no connection may be made");
}

#[tokio::test]
async fn loopback_reachable_when_allowed() {
    let (addr, conns) = start(None).await;
    let e = Egress::builder()
        .allow(
            Rule::host("127.0.0.1")
                .https_only(false)
                .credential(StaticCredential::bearer("tok")),
        )
        .deny_private_ips(false)
        .build();
    let url = format!("http://127.0.0.1:{}/hello", addr.port());
    let resp = e.send(with_body(Method::POST, &url, "body")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        text(resp).await,
        format!("POST /hello host=127.0.0.1:{} auth=Bearer tok", addr.port())
    );
    assert_eq!(conns.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rewritten_requests_are_exempt_from_private_ip_check() {
    let (addr, _) = start(None).await;
    let e = Egress::builder()
        .allow(Rule::host("api.example.test"))
        .rewrite(
            "https://api.example.test/",
            format!("http://127.0.0.1:{}/mock/", addr.port()),
        )
        .build();
    let resp = e.send(get("https://api.example.test/v1/x")).await.unwrap();
    assert_eq!(
        text(resp).await,
        format!("GET /mock/v1/x host=127.0.0.1:{} auth=", addr.port())
    );
}

#[tokio::test]
async fn custom_root_ca_over_tls() {
    let (cert, acceptor) = tls_fixture();
    let (addr, _) = start(Some(acceptor)).await;
    let url = format!("https://localhost:{}/tls", addr.port());

    let trusted = HyperTransport::builder()
        .add_root_certificate(cert)
        .deny_private_ips(false)
        .build()
        .unwrap();
    let e = Egress::builder()
        .allow(Rule::host("localhost"))
        .transport(trusted)
        .build();
    let resp = e.send(get(&url)).await.unwrap();
    assert_eq!(
        text(resp).await,
        format!("GET /tls host=localhost:{} auth=", addr.port())
    );

    let untrusted = HyperTransport::builder()
        .deny_private_ips(false)
        .build()
        .unwrap();
    let r = untrusted.send(get(&url)).await;
    assert!(matches!(r, Err(EgressError::Tls(_))), "{r:?}");
}

#[tokio::test]
async fn http1_only_still_works() {
    let (addr, _) = start(None).await;
    let t = HyperTransport::builder()
        .http2(false)
        .deny_private_ips(false)
        .connect_timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let resp = t
        .send(get(&format!("http://127.0.0.1:{}/x", addr.port())))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn response_cap_over_real_stream() {
    let (addr, _) = start(None).await;
    let e = Egress::builder()
        .allow(Rule::host("127.0.0.1").https_only(false))
        .deny_private_ips(false)
        .max_response_bytes(1500)
        .build();
    let resp = e
        .send(get(&format!("http://127.0.0.1:{}/stream", addr.port())))
        .await
        .unwrap();
    match read_body(resp).await {
        Err(EgressError::BodyTooLarge) => {}
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn connection_refused_maps_to_connect() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let t = HyperTransport::builder()
        .deny_private_ips(false)
        .build()
        .unwrap();
    let r = t.send(get(&format!("http://127.0.0.1:{port}/"))).await;
    assert!(matches!(r, Err(EgressError::Connect(_))), "{r:?}");
}

#[tokio::test]
async fn dns_failure_maps_to_dns() {
    let t = HyperTransport::new();
    let r = t.send(get("https://does-not-exist.invalid/")).await;
    assert!(matches!(r, Err(EgressError::Dns(_))), "{r:?}");
}

#[test]
fn invalid_root_certificate_is_rejected() {
    let r = HyperTransport::builder()
        .add_root_certificate(b"not a certificate".to_vec())
        .build();
    assert!(matches!(r, Err(EgressError::Tls(_))), "{r:?}");
}
