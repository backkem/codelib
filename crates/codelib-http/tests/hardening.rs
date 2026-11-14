//! Hardening regression tests: each test tries an attack a sandboxed script
//! (or a server it talks to) can mount and checks that it fails.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use codelib_http::http::header::{
    AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HOST,
};
use codelib_http::http::{HeaderMap, Method, Uri};
use codelib_http::{
    Credential, CredentialProvider, Egress, EgressError, HyperTransport, Redirects, Request,
    RequestInfo, Response, Rule, Signer, StaticCredential, StatusCode, async_trait, full_body,
    is_private_ip, status_response, text_response,
};
use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const OK_REPLY: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

type Log = Arc<Mutex<Vec<String>>>;

/// Read one request head from `s`; `None` if the peer closed first.
async fn read_head(s: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = match s.read(&mut tmp).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Some(String::from_utf8_lossy(&buf).into_owned());
        }
    }
}

/// Raw HTTP/1 loopback server: records each request head and writes `reply`.
/// With `hold`, keeps the connection open (without writing more) afterwards.
async fn raw_server_on(listener: TcpListener, reply: Vec<u8>, hold: bool) -> Log {
    let log: Log = Arc::default();
    let l2 = log.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let log = l2.clone();
            let reply = reply.clone();
            tokio::spawn(async move {
                let Some(head) = read_head(&mut s).await else {
                    return;
                };
                log.lock().unwrap().push(head);
                let _ = s.write_all(&reply).await;
                if hold {
                    tokio::time::sleep(Duration::from_secs(120)).await;
                }
            });
        }
    });
    log
}

async fn raw_server(reply: &[u8], hold: bool) -> (SocketAddr, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (addr, raw_server_on(listener, reply.to_vec(), hold).await)
}

fn mock_egress(mock: &Mock) -> codelib_http::EgressBuilder {
    Egress::builder().transport(mock.clone())
}

fn uris(mock: &Mock) -> Vec<String> {
    mock.seen().iter().map(|s| s.uri.to_string()).collect()
}

// ============================================================ rewrites

/// The script cannot turn a rewrite into a request to a host of its choice.
/// Gluing the path onto a target without a trailing slash would make
/// `/@127.0.0.1:PORT/...` the user info and host of the result, which is
/// then exempt from private-IP blocking and carries the rule's credential.
/// Checked end-to-end with the default transport against a loopback server.
#[tokio::test]
async fn rewrite_target_without_slash_lets_script_choose_authority() {
    let (addr, log) = raw_server(OK_REPLY, false).await;
    let e = Egress::builder()
        .allow(
            Rule::host("api.example.test").credential(StaticCredential::bearer("rewrite-secret")),
        )
        .rewrite("https://api.example.test/", "http://upstream.invalid")
        .request_timeout(Duration::from_secs(5))
        .build();
    let url = format!("https://api.example.test/@127.0.0.1:{}/steal", addr.port());
    let _ = e.send(get(&url)).await;
    let reqs = log.lock().unwrap().clone();
    assert!(
        reqs.is_empty(),
        "a script-chosen loopback authority received the request (private-IP \
         blocking skipped, credential attached): {reqs:?}"
    );
}

/// Whatever the path, a rewritten request goes to the configured host and
/// port: no `@` user-info trick, no host suffix, no extra port digits.
#[tokio::test]
async fn rewrite_target_without_slash_host_is_script_controlled() {
    let mock = Mock::ok();
    let e = mock_egress(&mock)
        .allow(Rule::host("api.example.com").credential(StaticCredential::bearer("s")))
        .rewrite("https://api.example.com/", "http://10.0.0.5")
        .rewrite("https://other.example.com/", "http://10.0.0.6:80")
        .build();
    for path in ["/@attacker.example/x", "/.attacker.example/x", "/0/x"] {
        let _ = e.send(get(&format!("https://api.example.com{path}"))).await;
    }
    let hosts: Vec<String> = mock
        .seen()
        .iter()
        .map(|s| s.uri.host().unwrap_or("").to_owned())
        .collect();
    assert_eq!(hosts.len(), 3, "{:?}", uris(&mock));
    assert!(
        hosts.iter().all(|h| h == "10.0.0.5"),
        "rewritten requests went to script-chosen hosts: {hosts:?} (uris: {:?})",
        uris(&mock)
    );
    assert_eq!(
        uris(&mock),
        [
            "http://10.0.0.5/@attacker.example/x",
            "http://10.0.0.5/.attacker.example/x",
            "http://10.0.0.5/0/x"
        ]
    );
}

/// Same-origin spellings cannot dodge a rewrite: an explicit default port is
/// the same origin and is rewritten, and URLs with user info are refused
/// outright, so the request never goes to the real host instead.
#[tokio::test]
async fn rewrite_skipped_for_equivalent_authority() {
    let mock = Mock::ok();
    let e = mock_egress(&mock)
        .allow(Rule::host("api.example.com").credential(StaticCredential::bearer("s")))
        .rewrite("https://api.example.com/", "http://10.0.0.5/")
        .build();
    e.send(get("https://api.example.com:443/x")).await.unwrap();
    e.send(get("HTTPS://API.EXAMPLE.COM/y")).await.unwrap();
    match e.send(get("https://someone@api.example.com/x")).await {
        Err(EgressError::InvalidRequest(_)) => {}
        other => panic!("user info must be refused: {other:?}"),
    }
    let not_rewritten: Vec<String> = mock
        .seen()
        .iter()
        .filter(|s| s.rewritten_from.is_none())
        .map(|s| s.uri.to_string())
        .collect();
    assert!(
        not_rewritten.is_empty(),
        "same-origin URLs bypassed the rewrite and went to the real host with the credential: {not_rewritten:?}"
    );
    assert_eq!(uris(&mock), ["http://10.0.0.5/x", "http://10.0.0.5/y"]);
}

/// A non-default port is a different origin: it does not match the rewrite
/// and goes to the real host under the rule.
#[tokio::test]
async fn rewrite_other_port_is_not_rewritten() {
    let mock = Mock::ok();
    let e = mock_egress(&mock)
        .allow(Rule::host("api.example.com"))
        .rewrite("https://api.example.com/", "http://10.0.0.5/")
        .build();
    e.send(get("https://api.example.com:8443/x")).await.unwrap();
    let seen = mock.seen();
    assert!(seen[0].rewritten_from.is_none());
    assert_eq!(seen[0].uri, "https://api.example.com:8443/x");
}

/// Slashes on either side of a rewrite, base paths and queries combine into
/// one well-formed URL on the configured host.
#[tokio::test]
async fn rewrite_joins_paths_and_keeps_query() {
    for (from, to, url, want) in [
        (
            "https://a.example/",
            "http://10.0.0.5:8080",
            "https://a.example/x?q=1&r=%2F",
            "http://10.0.0.5:8080/x?q=1&r=%2F",
        ),
        (
            "https://a.example",
            "http://10.0.0.5:8080/",
            "https://a.example?q=1",
            "http://10.0.0.5:8080/?q=1",
        ),
        (
            "https://a.example/v1/",
            "http://10.0.0.5/base",
            "https://a.example/v1/items/7?full=1",
            "http://10.0.0.5/base/items/7?full=1",
        ),
        (
            "https://a.example/v1",
            "http://10.0.0.5/base/",
            "https://a.example/v1",
            "http://10.0.0.5/base",
        ),
        (
            "https://a.example/v1",
            "https://b.example:9443/",
            "https://a.example/v1/x",
            "https://b.example:9443/x",
        ),
    ] {
        let mock = Mock::ok();
        let e = mock_egress(&mock)
            .allow(Rule::host("a.example"))
            .rewrite(from, to)
            .build();
        e.send(get(url)).await.unwrap();
        let seen = mock.seen();
        assert_eq!(seen[0].uri, want, "{from} -> {to}: {url}");
        assert_eq!(seen[0].rewritten_from.as_ref().unwrap(), url);
        let host = want.split("://").nth(1).unwrap().split('/').next().unwrap();
        assert_eq!(seen[0].headers[HOST], host.split('?').next().unwrap());
    }
    // `/v1` does not capture `/v1x`.
    let mock = Mock::ok();
    let e = mock_egress(&mock)
        .allow(Rule::host("a.example"))
        .rewrite("https://a.example/v1", "http://10.0.0.5/")
        .build();
    e.send(get("https://a.example/v1x")).await.unwrap();
    assert!(mock.seen()[0].rewritten_from.is_none());
}

/// The script cannot address paths above the rewrite target's base path with
/// dot segments (plain, `;`-decorated or behind an encoded slash), which the
/// internal upstream would resolve.
#[tokio::test]
async fn rewrite_target_path_escape() {
    let mock = Mock::ok();
    let e = mock_egress(&mock)
        .allow(Rule::host("api.example.com"))
        .rewrite("https://api.example.com/", "http://10.0.0.5:8080/base/")
        .build();
    for path in [
        "/../admin",
        "/..;/admin",
        "/%2e%2e%2fadmin",
        "/x/..%5c..%5cadmin",
    ] {
        let r = e.send(get(&format!("https://api.example.com{path}"))).await;
        assert_denied(r);
    }
    let escaped: Vec<String> = mock
        .seen()
        .iter()
        .map(|s| s.uri.path().to_owned())
        .collect();
    assert!(
        escaped.is_empty(),
        "rewritten paths that resolve outside /base/ reached the upstream: {escaped:?}"
    );
}

/// An error handed to the script does not reveal the embedder's rewrite
/// destination.
#[tokio::test]
async fn rewrite_target_not_revealed_in_errors() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let e = Egress::builder()
        .allow(Rule::host("api.example.test"))
        .rewrite(
            "https://api.example.test/",
            format!("http://127.0.0.1:{port}/"),
        )
        .build();
    let err = e.send(get("https://api.example.test/x")).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        !msg.contains("127.0.0.1") && !msg.contains(&port.to_string()),
        "error handed to the script reveals the rewrite target: {msg}"
    );
}

// ======================================================== path scoping

/// A credential scoped to a path prefix does not go to paths that servers
/// resolve outside it: `..;` (Tomcat/Spring strip `;params`), `%2f` (decoded
/// by some proxies before normalising), `\` and `%5c` (IIS).
#[tokio::test]
async fn path_prefix_escape_via_encoded_or_param_dot_segments() {
    let mock = Mock::ok();
    let e = mock_egress(&mock)
        .allow(
            Rule::host("api.example.com/ex/jira/")
                .credential(StaticCredential::bearer("jira-only")),
        )
        .build();
    let mut leaked = Vec::new();
    for path in [
        "/ex/jira/..;/confluence/x",
        "/ex/jira/%2e%2e%2fconfluence/x",
        "/ex/jira/..%2Fconfluence/x",
        "/ex/jira/..%5cconfluence/x",
        "/ex/jira/..\\confluence/x",
        "/ex/jira/.%2e;x=1/confluence/x",
    ] {
        if e.send(get(&format!("https://api.example.com{path}")))
            .await
            .is_ok()
        {
            leaked.push(path);
        }
    }
    assert!(
        leaked.is_empty(),
        "credential scoped to /ex/jira/ was sent to paths that escape it: {leaked:?}"
    );
    // Ordinary paths, including `;` matrix parameters, still work.
    e.send(get("https://api.example.com/ex/jira/a;v=1/b.json"))
        .await
        .unwrap();
    assert_eq!(mock.count(), 1);
}

/// A prefix without a trailing slash does not capture sibling paths.
#[tokio::test]
async fn path_prefix_has_no_segment_boundary() {
    let mock = Mock::ok();
    let e = mock_egress(&mock)
        .allow(
            Rule::host("api.example.com")
                .path_prefix("/api")
                .credential(StaticCredential::bearer("api-only")),
        )
        .build();
    e.send(get("https://api.example.com/api/ok")).await.unwrap();
    e.send(get("https://api.example.com/api")).await.unwrap();
    e.send(get("https://api.example.com/api?x=1"))
        .await
        .unwrap();
    let mut leaked = Vec::new();
    for path in ["/apiv2/admin", "/api-internal/keys", "/api.json"] {
        if e.send(get(&format!("https://api.example.com{path}")))
            .await
            .is_ok()
        {
            leaked.push(path);
        }
    }
    assert!(
        leaked.is_empty(),
        "path_prefix(\"/api\") also matched sibling paths: {leaked:?}"
    );
}

// ======================================================== credentials

/// TRACE makes a server echo the request, injected `Authorization`
/// included, back to the caller; it is refused before any credential is
/// resolved.
#[tokio::test]
async fn trace_reflects_injected_credential() {
    let mock = Mock::new(|s, _| {
        let auth = s
            .headers
            .get(AUTHORIZATION)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        text_response(StatusCode::OK, format!("authorization: {auth}\r\n"))
    });
    let e = mock_egress(&mock)
        .allow(Rule::host("api.example.com").credential(StaticCredential::bearer("trace-secret")))
        .build();
    for m in ["TRACE", "trace", "TRACK", "CONNECT"] {
        let r = e
            .send(req(
                Method::from_bytes(m.as_bytes()).unwrap(),
                "https://api.example.com/",
            ))
            .await;
        let reason = assert_denied(r);
        assert!(reason.contains("never allowed"), "{m}: {reason}");
    }
    assert_eq!(mock.count(), 0);
}

/// Even the development policy refuses TRACE and CONNECT, before any
/// connection is made.
#[tokio::test]
async fn trace_and_connect_refused_by_allow_all() {
    let (addr, log) = raw_server(OK_REPLY, false).await;
    for m in [Method::TRACE, Method::CONNECT] {
        let r = Egress::allow_all()
            .send(req(m, &format!("http://127.0.0.1:{}/", addr.port())))
            .await;
        assert_denied(r);
    }
    assert!(log.lock().unwrap().is_empty());
    // A virtual host is the embedder's own code and may accept any method.
    let e = Egress::builder()
        .virtual_host("files.internal", |_req| async {
            text_response(StatusCode::OK, "vhost")
        })
        .build();
    assert!(
        e.send(req(Method::TRACE, "http://files.internal/"))
            .await
            .is_ok()
    );
}

/// Static rule headers (often API keys) are sensitive: redacted in `Debug`
/// and never indexed by HTTP/2 header compression.
#[tokio::test]
async fn static_rule_headers_not_marked_sensitive() {
    let mock = Mock::ok();
    let e = mock_egress(&mock)
        .allow(Rule::host("api.example.com").header("x-api-key", "static-secret"))
        .build();
    e.send(get("https://api.example.com/")).await.unwrap();
    let seen = mock.seen();
    let v = &seen[0].headers["x-api-key"];
    assert!(
        v.is_sensitive(),
        "static header not sensitive; Debug prints: {:?}",
        seen[0].headers
    );
    assert!(!format!("{:?}", seen[0].headers).contains("static-secret"));
}

/// Records what a credential provider and a signer are shown.
#[derive(Clone, Default)]
struct Observer(Arc<Mutex<Vec<HeaderMap>>>);

#[async_trait]
impl CredentialProvider for Observer {
    async fn credential(&self, req: &RequestInfo<'_>) -> Result<Credential, EgressError> {
        self.0.lock().unwrap().push(req.headers.clone());
        Ok(Credential::Bearer("tok".into()))
    }
}

#[async_trait]
impl Signer for Observer {
    async fn sign(&self, req: Request) -> Result<Request, EgressError> {
        self.0.lock().unwrap().push(req.headers().clone());
        Ok(req)
    }
}

/// Credential providers and signers never see a script-chosen `Host` (which
/// a provider could use to pick a tenant, or a signer could sign), and
/// providers do not see the rule's static headers.
#[tokio::test]
async fn providers_and_signers_see_host_from_url_only() {
    let obs = Observer::default();
    let mock = Mock::ok();
    let e = mock_egress(&mock)
        .allow(
            Rule::host("api.example.com")
                .header("x-static", "static-secret")
                .credential(obs.clone())
                .signer(obs.clone()),
        )
        .build();
    let mut r = get("https://api.example.com:8443/x");
    r.headers_mut()
        .insert(HOST, "victim-tenant.example".parse().unwrap());
    e.send(r).await.unwrap();
    let seen = obs.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    let (provider, signer) = (&seen[0], &seen[1]);
    assert_eq!(provider[HOST], "api.example.com:8443");
    assert!(!provider.contains_key("x-static"));
    assert!(!provider.contains_key(AUTHORIZATION));
    assert_eq!(signer[HOST], "api.example.com:8443");
    assert_eq!(signer[AUTHORIZATION], "Bearer tok");
    assert_eq!(signer["x-static"], "static-secret");
    assert_eq!(mock.seen()[0].headers[HOST], "api.example.com:8443");
}

/// Host confusion through user info and fragments is impossible: user info
/// is refused outright, and fragments end the authority for the checker and
/// the dialler alike.
#[tokio::test]
async fn userinfo_and_fragment_confusion_denied() {
    let mock = Mock::ok();
    let e = mock_egress(&mock)
        .allow(Rule::host("api.example.com").credential(StaticCredential::bearer("s")))
        .build();
    for url in [
        "https://api.example.com@evil.example/",
        "https://user:pw@api.example.com/",
        "https://evil.example@api.example.com/",
    ] {
        match e.send(get(url)).await {
            Err(EgressError::InvalidRequest(msg)) => assert!(!msg.contains("pw"), "{msg}"),
            other => panic!("{url}: {other:?}"),
        }
    }
    for url in [
        "https://evil.example#@api.example.com/",
        "https://evil.example/#api.example.com",
        "https://api.example.com.evil.example/",
        "https://api.example.com./", // a trailing dot fails closed
        "https://evilapi.example.com/",
    ] {
        assert_denied(e.send(get(url)).await);
    }
    assert_eq!(mock.count(), 0);
}

/// User info is refused in Follow mode too, before the body is buffered.
#[tokio::test]
async fn userinfo_refused_when_following() {
    let mock = Mock::ok();
    let e = mock_egress(&mock)
        .redirects(Redirects::Follow(3))
        .allow(Rule::host("api.example.com"))
        .build();
    let r = e
        .send(with_body(Method::POST, "https://u:p@api.example.com/", "x"))
        .await;
    assert!(matches!(r, Err(EgressError::InvalidRequest(_))), "{r:?}");
    assert_eq!(mock.count(), 0);
}

// ======================================================== redirects

/// 301/302 change only POST to GET (Fetch); a PUT or DELETE is not silently
/// replayed as a GET whose 200 the script would take for success.
#[tokio::test]
async fn redirect_301_302_rewrite_put_and_delete_to_get() {
    for status in [301u16, 302] {
        let mock = Mock::new(move |s, _| {
            if s.uri.path() == "/start" {
                redirect(status, "/moved")
            } else {
                text_response(StatusCode::OK, "ok")
            }
        });
        let e = mock_egress(&mock)
            .redirects(Redirects::Follow(3))
            .allow(Rule::host("api.example.com"))
            .build();
        for m in [Method::PUT, Method::DELETE] {
            e.send(with_body(
                m.clone(),
                "https://api.example.com/start",
                "data",
            ))
            .await
            .unwrap();
        }
        let seen = mock.seen();
        let methods: Vec<Method> = seen.iter().map(|s| s.method.clone()).collect();
        assert_eq!(
            methods,
            [Method::PUT, Method::PUT, Method::DELETE, Method::DELETE],
            "status {status}: method rewritten on redirect"
        );
        assert_eq!(seen[1].body, "data");
        assert_eq!(seen[3].body, "data");
    }
}

/// When a redirect turns the request into a GET, the body and every header
/// describing it are dropped.
#[tokio::test]
async fn redirect_to_get_drops_body_headers() {
    for (status, method) in [
        (302u16, Method::POST),
        (303, Method::PUT),
        (303, Method::PATCH),
    ] {
        let mock = Mock::new(move |s, _| {
            if s.uri.path() == "/start" {
                redirect(status, "/moved")
            } else {
                text_response(StatusCode::OK, "ok")
            }
        });
        let e = mock_egress(&mock)
            .redirects(Redirects::Follow(3))
            .allow(Rule::host("api.example.com"))
            .build();
        let mut r = with_body(method.clone(), "https://api.example.com/start", "data");
        r.headers_mut()
            .insert(CONTENT_ENCODING, "identity".parse().unwrap());
        r.headers_mut()
            .insert("content-language", "en".parse().unwrap());
        r.headers_mut().insert("x-keep", "1".parse().unwrap());
        e.send(r).await.unwrap();
        let s = &mock.seen()[1];
        assert_eq!(s.method, Method::GET, "{status} {method}");
        assert!(s.body.is_empty());
        for h in [
            CONTENT_LENGTH.as_str(),
            CONTENT_TYPE.as_str(),
            CONTENT_ENCODING.as_str(),
            "content-language",
        ] {
            assert!(!s.headers.contains_key(h), "{status} {method}: {h} kept");
        }
        assert_eq!(s.headers["x-keep"], "1");
    }
}

/// A redirect `Location` with user info is a protocol error, not a request.
#[tokio::test]
async fn redirect_location_with_userinfo_refused() {
    let mock = Mock::new(|_, _| redirect(302, "https://user:pw@api.example.com/next"));
    let e = mock_egress(&mock)
        .redirects(Redirects::Follow(3))
        .allow(Rule::host("api.example.com"))
        .build();
    let r = e.send(get("https://api.example.com/")).await;
    assert!(matches!(r, Err(EgressError::Protocol(_))), "{r:?}");
    assert_eq!(mock.count(), 1);
}

/// In Follow mode a request the policy refuses is refused before its body is
/// buffered (so the refusal is `Denied`, and nothing is held in memory).
#[tokio::test]
async fn follow_buffers_body_before_policy_check() {
    let mock = Mock::ok();
    let e = mock_egress(&mock)
        .redirects(Redirects::Follow(3))
        .max_buffered_request_bytes(4)
        .allow(Rule::host("api.example.com").read_only())
        .build();
    for (method, url) in [
        (Method::POST, "https://not-allowed.example/"),
        (Method::POST, "https://api.example.com/"),
        (Method::POST, "http://api.example.com/"),
    ] {
        let chunks = Chunks::new([b"too ".as_slice(), b"long".as_slice(), b"!".as_slice()]);
        let finished = chunks.finished.clone();
        let r = codelib_http::http::Request::builder()
            .method(method)
            .uri(url)
            .body(chunks.body())
            .unwrap();
        let r = e.send(r).await;
        assert!(
            matches!(r, Err(EgressError::Denied { .. })),
            "{url}: expected Denied before buffering, got {r:?}"
        );
        assert!(
            finished.load(std::sync::atomic::Ordering::SeqCst),
            "{url}: not drained"
        );
    }
    // An allowed request is still capped while buffering.
    let r = e
        .send(with_body(
            Method::GET,
            "https://api.example.com/",
            "too long",
        ))
        .await;
    assert!(matches!(r, Err(EgressError::BodyTooLarge)), "{r:?}");
    assert_eq!(mock.count(), 0);
}

fn uppercase_https(authority: &str) -> Uri {
    Uri::builder()
        .scheme("HTTPS")
        .authority(authority)
        .path_and_query("/")
        .build()
        .unwrap()
}

/// A URI whose scheme was built from parts as `HTTPS` (as a wasi-http host
/// hook does) is normalised up front, so the https->http downgrade is
/// detected and script cookies do not follow it.
#[tokio::test]
async fn uppercase_scheme_defeats_downgrade_strip() {
    let mock = Mock::new(|s, _| {
        if s.uri.path() == "/" {
            redirect(302, "http://a.example/plain")
        } else {
            text_response(StatusCode::OK, "ok")
        }
    });
    let e = mock_egress(&mock)
        .redirects(Redirects::Follow(3))
        .allow(Rule::host("a.example").https_only(false))
        .build();
    let r = codelib_http::http::Request::builder()
        .uri(uppercase_https("a.example"))
        .header(COOKIE, "sid=1")
        .body(codelib_http::empty_body())
        .unwrap();
    e.send(r).await.unwrap();
    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].uri.scheme_str(), Some("https"));
    assert!(
        !seen[1].headers.contains_key(COOKIE),
        "Cookie survived an HTTPS->http downgrade"
    );
}

/// The transport sees the normalised scheme too (Manual mode), and an
/// unknown scheme is refused.
#[tokio::test]
async fn scheme_normalised_before_transport() {
    let mock = Mock::ok();
    let e = mock_egress(&mock).allow(Rule::host("a.example")).build();
    let r = codelib_http::http::Request::builder()
        .uri(uppercase_https("a.example"))
        .body(codelib_http::empty_body())
        .unwrap();
    e.send(r).await.unwrap();
    assert_eq!(mock.seen()[0].uri, "https://a.example/");
    let odd = Uri::builder()
        .scheme("httpx")
        .authority("a.example")
        .path_and_query("/")
        .build()
        .unwrap();
    let r = codelib_http::http::Request::builder()
        .uri(odd)
        .body(codelib_http::empty_body())
        .unwrap();
    assert!(matches!(
        e.send(r).await,
        Err(EgressError::InvalidRequest(_))
    ));
}

/// An uppercase-scheme URI never makes `HyperTransport` send the credential
/// in cleartext to an https-only rule.
#[tokio::test]
async fn uppercase_scheme_not_sent_in_cleartext() {
    let (addr, log) = raw_server(OK_REPLY, false).await;
    let t = HyperTransport::builder()
        .deny_private_ips(false)
        .build()
        .unwrap();
    let e = Egress::builder()
        .allow(Rule::host("127.0.0.1").credential(StaticCredential::bearer("tls-only")))
        .transport(t)
        .request_timeout(Duration::from_secs(2))
        .build();
    let r = codelib_http::http::Request::builder()
        .uri(uppercase_https(&format!("127.0.0.1:{}", addr.port())))
        .body(codelib_http::empty_body())
        .unwrap();
    let res = e.send(r).await;
    let reqs = log.lock().unwrap().clone();
    assert!(res.is_err(), "{res:?}");
    assert!(
        reqs.iter().all(|r| !r.contains("tls-only")),
        "credential sent in cleartext: {reqs:?} / {res:?}"
    );
}

/// A rewrite's private-IP exemption does not carry over to a followed
/// redirect hop that is not itself rewritten.
#[tokio::test]
async fn rewrite_exemption_not_inherited_by_redirect_hop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let reply = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{}/second\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        addr.port()
    );
    let log = raw_server_on(listener, reply.into_bytes(), false).await;
    let e = Egress::builder()
        .redirects(Redirects::Follow(3))
        .allow(Rule::host("api.example.test"))
        .allow(Rule::host("127.0.0.1").https_only(false))
        .rewrite(
            "https://api.example.test/",
            format!("http://127.0.0.1:{}/", addr.port()),
        )
        .build();
    let reason = assert_denied(e.send(get("https://api.example.test/first")).await);
    assert!(reason.contains("public"), "{reason}");
    assert_eq!(log.lock().unwrap().len(), 1);
}

// ======================================================== limits & timeouts

/// A server that sends headers and then goes quiet cannot hang the reader:
/// the body fails with `Timeout` after the idle timeout (by default the
/// request timeout).
#[tokio::test]
async fn slow_response_body_has_no_timeout() {
    let (addr, _log) =
        raw_server(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nhello", true).await;
    let e = Egress::builder()
        .allow(Rule::host("127.0.0.1").https_only(false))
        .deny_private_ips(false)
        .request_timeout(Duration::from_millis(300))
        .max_response_bytes(1000)
        .build();
    let resp = e
        .send(get(&format!("http://127.0.0.1:{}/", addr.port())))
        .await
        .unwrap();
    let outer = tokio::time::timeout(Duration::from_secs(3), read_body(resp)).await;
    match outer {
        Ok(Err(EgressError::Timeout)) => {}
        Ok(other) => panic!("expected Timeout, got {other:?}"),
        Err(_) => panic!("body read still hanging 3 s after headers with request_timeout = 300 ms"),
    }
}

/// The idle timer restarts on every frame: a slow body that keeps flowing
/// completes even though it takes far longer than the idle timeout, and a
/// separately configured idle timeout applies.
#[tokio::test]
async fn idle_timer_resets_while_body_flows() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                if read_head(&mut s).await.is_none() {
                    return;
                }
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n")
                    .await;
                for _ in 0..10 {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    if s.write_all(b"x").await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    let e = Egress::builder()
        .allow(Rule::host("127.0.0.1").https_only(false))
        .deny_private_ips(false)
        .request_timeout(Duration::from_millis(50))
        .body_idle_timeout(Duration::from_millis(600))
        .build();
    let start = std::time::Instant::now();
    let resp = e
        .send(get(&format!("http://127.0.0.1:{}/", addr.port())))
        .await
        .unwrap();
    let body = read_body(resp).await.unwrap();
    assert_eq!(body, "xxxxxxxxxx");
    assert!(start.elapsed() > Duration::from_millis(1000));
}

/// A reader that is slow to come back is not blamed on the server: the timer
/// only runs while the reader is waiting.
#[tokio::test]
async fn idle_timer_ignores_slow_reader() {
    let mock =
        Mock::new(|_, _| Response::new(Chunks::new([b"a".as_slice(), b"b".as_slice()]).body()));
    let e = mock_egress(&mock)
        .allow(Rule::host("api.example.com"))
        .body_idle_timeout(Duration::from_millis(20))
        .build();
    let resp = e.send(get("https://api.example.com/")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(read_body(resp).await.unwrap(), "ab");
}

/// `Content-Length` on a response that has no body (to HEAD, or 1xx, 204,
/// 304) is not the body size and does not trip the response cap.
#[tokio::test]
async fn bodyless_response_content_length_trips_cap() {
    let mock = Mock::new(|s, _| {
        let status = match (s.method.clone(), s.uri.path()) {
            (Method::HEAD, _) => StatusCode::OK,
            (_, "/204") => StatusCode::NO_CONTENT,
            _ => StatusCode::NOT_MODIFIED,
        };
        let mut r: Response = status_response(status);
        r.headers_mut()
            .insert(CONTENT_LENGTH, "1000000".parse().unwrap());
        r
    });
    let e = mock_egress(&mock)
        .allow(Rule::host("api.example.com"))
        .max_response_bytes(10)
        .build();
    let head = e
        .send(req(Method::HEAD, "https://api.example.com/big"))
        .await;
    let not_modified = e.send(get("https://api.example.com/big")).await;
    let no_content = e.send(get("https://api.example.com/204")).await;
    assert!(
        head.is_ok() && not_modified.is_ok() && no_content.is_ok(),
        "bodyless responses rejected: HEAD {:?}, 304 {:?}, 204 {:?}",
        head.err(),
        not_modified.err(),
        no_content.err()
    );
    // A GET with the same declared length is still refused up front.
    let mock = Mock::new(|_, _| {
        let mut r = text_response(StatusCode::OK, "x");
        r.headers_mut()
            .insert(CONTENT_LENGTH, "1000000".parse().unwrap());
        r
    });
    let e = mock_egress(&mock)
        .allow(Rule::host("api.example.com"))
        .max_response_bytes(10)
        .build();
    assert!(matches!(
        e.send(get("https://api.example.com/")).await,
        Err(EgressError::BodyTooLarge)
    ));
}

/// Without a `Content-Length`, the cap is enforced while streaming.
#[tokio::test]
async fn cap_holds_without_content_length() {
    let mock = Mock::new(|_, _| Response::new(full_body("x".repeat(64))));
    let e = mock_egress(&mock)
        .allow(Rule::host("api.example.com"))
        .max_response_bytes(10)
        .build();
    let r = e.send(get("https://api.example.com/")).await;
    // Either rejected up front (exact size hint) or while streaming.
    match r {
        Err(EgressError::BodyTooLarge) => {}
        Ok(resp) => assert!(matches!(
            read_body(resp).await,
            Err(EgressError::BodyTooLarge)
        )),
        other => panic!("{other:?}"),
    }
}

// ======================================================== private IPs

/// IPv6 forms that embed or translate to a blocked IPv4 address are blocked:
/// 6to4 (2002::/16), local-use NAT64 (64:ff9b:1::/48) and Teredo (2001::/32).
#[test]
fn private_ip_misses_embedded_ipv4_prefixes() {
    let missed: Vec<&str> = [
        "64:ff9b:1::a9fe:a9fe", // local-use NAT64 -> 169.254.169.254
        "64:ff9b:1::7f00:1",    // local-use NAT64 -> 127.0.0.1
        "2002:a9fe:a9fe::1",    // 6to4 of 169.254.169.254
        "2002:7f00:1::1",       // 6to4 of 127.0.0.1
        "2002:a00:1::1",        // 6to4 of 10.0.0.1
        "2001:0:a9fe:a9fe::1",  // Teredo
        "64:ff9b::a9fe:a9fe",   // well-known NAT64 -> 169.254.169.254
    ]
    .into_iter()
    .filter(|s| !is_private_ip(s.parse().unwrap()))
    .collect();
    assert!(missed.is_empty(), "not treated as private: {missed:?}");
    for public in [
        "2002:808:808::1",
        "64:ff9b::808:808",
        "2001:4860:4860::8888",
    ] {
        assert!(!is_private_ip(public.parse().unwrap()), "{public}");
    }
}

/// Alternative IPv4 spellings and literal forms never reach a loopback
/// server with private-IP blocking on (resolver results are vetted).
#[tokio::test]
async fn numeric_ip_forms_blocked() {
    let (addr, log) = raw_server(OK_REPLY, false).await;
    let p = addr.port();
    let t = HyperTransport::builder()
        .connect_timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    for host in [
        "2130706433",
        "0x7f.1",
        "0x7f000001",
        "127.1",
        "0177.0.0.1",
        "127.0.0.1.",
        "localhost.",
        "[127.0.0.1]",
        "[::ffff:7f00:1]",
        "[::ffff:127.0.0.1]",
        "[0:0:0:0:0:ffff:7f00:1]",
        "[::1]",
        "0.0.0.0",
    ] {
        let url = format!("http://{host}:{p}/");
        let Ok(r) = url.parse::<Uri>() else { continue };
        let req = codelib_http::http::Request::get(r)
            .body(codelib_http::empty_body())
            .unwrap();
        let res = codelib_http::Transport::send(&t, req).await;
        assert!(res.is_err(), "{url}: {res:?}");
    }
    assert!(
        log.lock().unwrap().is_empty(),
        "loopback server was reached"
    );
}

// ======================================================== TLS

/// Certificate hostname verification is on: a certificate for `localhost`
/// is rejected for `127.0.0.1` even when its root is trusted.
#[tokio::test]
async fn tls_hostname_mismatch_rejected() {
    use tokio_rustls::rustls;
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
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            let a = acceptor.clone();
            tokio::spawn(async move {
                let _ = a.accept(s).await;
            });
        }
    });
    let t = HyperTransport::builder()
        .add_root_certificate(cert.to_vec())
        .deny_private_ips(false)
        .build()
        .unwrap();
    let r = codelib_http::Transport::send(&t, get(&format!("https://127.0.0.1:{}/", addr.port())))
        .await;
    assert!(matches!(r, Err(EgressError::Tls(_))), "{r:?}");
}

/// A signer that moves the request to another URL.
struct Redirector(&'static str);

#[async_trait]
impl Signer for Redirector {
    async fn sign(&self, mut req: Request) -> Result<Request, EgressError> {
        *req.uri_mut() = self.0.parse().unwrap();
        Ok(req)
    }
}

/// The rule matched the original destination only: a signer cannot send the
/// credential-bearing request to another scheme, host or port, but may
/// change the path and query.
#[tokio::test]
async fn signer_cannot_change_scheme_host_or_port() {
    for moved in [
        "https://evil.example/x",
        "http://api.example.com/x",
        "https://api.example.com:8443/x",
    ] {
        let mock = Mock::ok();
        let e = mock_egress(&mock)
            .allow(
                Rule::host("api.example.com")
                    .credential(StaticCredential::bearer("s"))
                    .signer(Redirector(moved)),
            )
            .build();
        let err = e.send(get("https://api.example.com/x")).await.unwrap_err();
        assert!(
            matches!(err, EgressError::InvalidRequest(_)),
            "{moved}: {err:?}"
        );
        assert!(mock.seen().is_empty(), "{moved} reached the transport");
    }

    // The default port made explicit is the same destination; so is a new path.
    for same in [
        "https://api.example.com:443/y?z=1",
        "https://API.example.com/other",
    ] {
        let mock = Mock::ok();
        let e = mock_egress(&mock)
            .allow(Rule::host("api.example.com").signer(Redirector(same)))
            .build();
        e.send(get("https://api.example.com/x")).await.unwrap();
        assert_eq!(mock.seen().len(), 1, "{same}");
    }
}
