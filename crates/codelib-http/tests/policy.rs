//! Policy pipeline tests against a mock transport.

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use codelib_http::http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HOST};
use codelib_http::http::{HeaderMap, Method};
use codelib_http::{
    Credential, Egress, EgressError, EnvBasic, EnvBearer, EnvHeader, Middleware, Next, Redirects,
    Request, RequestInfo, Response, Rule, Signer, StaticCredential, StatusCode, VirtualHost,
    async_trait, full_body, middleware, text_response,
};
use common::*;
use http_body_util::BodyExt;

fn egress(mock: &Mock, rules: Vec<Rule>) -> Egress {
    rules
        .into_iter()
        .fold(Egress::builder().transport(mock.clone()), |b, r| b.allow(r))
        .build()
}

// ---------------------------------------------------------------- allowlist

#[tokio::test]
async fn deny_by_default() {
    let mock = Mock::ok();
    let e = egress(&mock, vec![]);
    let reason = assert_denied(e.send(get("https://api.github.com/user")).await);
    assert!(reason.contains("not allowed"), "{reason}");
    assert_eq!(mock.count(), 0);

    // The real deny-all constructor refuses before any network activity.
    assert_denied(Egress::deny_all().send(get("https://example.com/")).await);
}

#[tokio::test]
async fn exact_and_wildcard_hosts() {
    let mock = Mock::ok();
    let e = egress(
        &mock,
        vec![Rule::host("api.github.com"), Rule::host("*.atlassian.net")],
    );
    assert!(e.send(get("https://api.github.com/x")).await.is_ok());
    assert!(e.send(get("https://API.GITHUB.COM:443/x")).await.is_ok());
    assert!(e.send(get("https://myorg.atlassian.net/x")).await.is_ok());
    assert_denied(e.send(get("https://atlassian.net/x")).await);
    assert_denied(e.send(get("https://github.com/x")).await);
    assert_denied(e.send(get("https://api.github.com.evil.example/x")).await);
    assert_eq!(mock.count(), 3);
    // The port is ignored for matching but kept on the wire.
    assert_eq!(mock.seen()[1].uri.port_u16(), Some(443));
}

#[tokio::test]
async fn path_scoped_rules() {
    let mock = Mock::ok();
    let e = egress(&mock, vec![Rule::host("api.atlassian.com/ex/jira/")]);
    assert!(
        e.send(get("https://api.atlassian.com/ex/jira/1/rest"))
            .await
            .is_ok()
    );
    assert_denied(
        e.send(get("https://api.atlassian.com/ex/confluence/1"))
            .await,
    );
    assert_denied(
        e.send(get("https://api.atlassian.com/ex/jira/../confluence/1"))
            .await,
    );
}

#[tokio::test]
async fn first_match_wins() {
    let mock = Mock::ok();
    let e = egress(
        &mock,
        vec![
            Rule::host("api.example.com")
                .read_only()
                .credential(StaticCredential::bearer("first")),
            Rule::host("*.example.com").credential(StaticCredential::bearer("second")),
            Rule::host("api.example.com").credential(StaticCredential::bearer("third")),
        ],
    );
    // The read-only rule matched first; no fallthrough to the permissive one.
    assert_denied(e.send(req(Method::POST, "https://api.example.com/x")).await);
    e.send(get("https://api.example.com/x")).await.unwrap();
    e.send(req(Method::POST, "https://other.example.com/x"))
        .await
        .unwrap();
    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].headers[AUTHORIZATION], "Bearer first");
    assert_eq!(seen[1].headers[AUTHORIZATION], "Bearer second");
}

#[tokio::test]
async fn read_only_and_methods() {
    let mock = Mock::ok();
    let e = egress(
        &mock,
        vec![
            Rule::host("ro.example").read_only(),
            Rule::host("rw.example").methods([Method::GET, Method::POST]),
        ],
    );
    assert!(e.send(get("https://ro.example/")).await.is_ok());
    assert!(
        e.send(req(Method::HEAD, "https://ro.example/"))
            .await
            .is_ok()
    );
    for m in [
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::OPTIONS,
    ] {
        let reason = assert_denied(e.send(req(m.clone(), "https://ro.example/")).await);
        assert!(reason.contains(m.as_str()), "{reason}");
    }
    assert!(
        e.send(req(Method::POST, "https://rw.example/"))
            .await
            .is_ok()
    );
    assert_denied(e.send(req(Method::DELETE, "https://rw.example/")).await);

    // Method-override headers cannot smuggle a write through a read-only rule.
    let mut r = get("https://ro.example/");
    r.headers_mut()
        .insert("x-http-method-override", "DELETE".parse().unwrap());
    assert_denied(e.send(r).await);
    assert_eq!(mock.count(), 3);
}

#[tokio::test]
async fn https_only() {
    let mock = Mock::ok();
    let e = egress(
        &mock,
        vec![
            Rule::host("secure.example"),
            Rule::host("plain.example").https_only(false),
        ],
    );
    let reason = assert_denied(e.send(get("http://secure.example/")).await);
    assert!(reason.contains("https"), "{reason}");
    assert!(e.send(get("https://secure.example/")).await.is_ok());
    assert!(e.send(get("http://plain.example/")).await.is_ok());
    assert!(e.send(get("https://plain.example/")).await.is_ok());
}

#[tokio::test]
async fn invalid_requests() {
    let mock = Mock::ok();
    let e = egress(&mock, vec![Rule::host("*").https_only(false)]);
    for url in ["/relative", "ftp://files.example/x"] {
        match e.send(get(url)).await {
            Err(EgressError::InvalidRequest(_)) => {}
            other => panic!("{url}: {other:?}"),
        }
    }
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn denied_body_is_drained() {
    let mock = Mock::ok();
    let e = egress(&mock, vec![]);
    let chunks = Chunks::new([b"a".as_slice(), b"b".as_slice()]);
    let finished = chunks.finished.clone();
    let r = codelib_http::http::Request::post("https://nope.example/")
        .body(chunks.body())
        .unwrap();
    assert_denied(e.send(r).await);
    assert!(finished.load(Ordering::SeqCst), "body was not drained");
}

// -------------------------------------------------------------- credentials

#[tokio::test]
async fn credential_overwrites_script_headers() {
    let mock = Mock::ok();
    let e = egress(
        &mock,
        vec![
            Rule::host("bearer.example")
                .header("x-static", "host-static")
                .credential(StaticCredential::bearer("host-token")),
            Rule::host("key.example").credential(StaticCredential::header("x-api-key", "host-key")),
        ],
    );
    let mut r = get("https://bearer.example/");
    r.headers_mut()
        .append(AUTHORIZATION, "Bearer script".parse().unwrap());
    r.headers_mut()
        .append(AUTHORIZATION, "Basic c2NyaXB0".parse().unwrap());
    r.headers_mut()
        .insert("x-static", "script-static".parse().unwrap());
    e.send(r).await.unwrap();

    let mut r = get("https://key.example/");
    r.headers_mut()
        .insert("X-Api-Key", "script-key".parse().unwrap());
    e.send(r).await.unwrap();

    let seen = mock.seen();
    let auth: Vec<_> = seen[0].headers.get_all(AUTHORIZATION).iter().collect();
    assert_eq!(auth, ["Bearer host-token"]);
    assert!(seen[0].headers[AUTHORIZATION].is_sensitive());
    assert_eq!(seen[0].headers["x-static"], "host-static");
    let keys: Vec<_> = seen[1].headers.get_all("x-api-key").iter().collect();
    assert_eq!(keys, ["host-key"]);
}

#[tokio::test]
async fn env_credentials_read_at_request_time() {
    let mock = Mock::ok();
    let e = egress(
        &mock,
        vec![
            Rule::host("bearer.example").credential(EnvBearer::new("CODELIB_HTTP_TEST_TOKEN")),
            Rule::host("basic.example").credential(EnvBasic::new(
                "CODELIB_HTTP_TEST_USER",
                "CODELIB_HTTP_TEST_PASS",
            )),
            Rule::host("header.example")
                .credential(EnvHeader::new("x-api-key", "CODELIB_HTTP_TEST_KEY")),
        ],
    );

    match e.send(get("https://bearer.example/")).await {
        Err(EgressError::Credential(msg)) => assert!(msg.contains("CODELIB_HTTP_TEST_TOKEN")),
        other => panic!("{other:?}"),
    }
    assert_eq!(mock.count(), 0);

    // SAFETY: these variable names are unique to this test.
    unsafe {
        std::env::set_var("CODELIB_HTTP_TEST_TOKEN", "tok123");
        std::env::set_var("CODELIB_HTTP_TEST_USER", "Aladdin");
        std::env::set_var("CODELIB_HTTP_TEST_PASS", "open sesame");
        std::env::set_var("CODELIB_HTTP_TEST_KEY", "k-1");
    }
    e.send(get("https://bearer.example/")).await.unwrap();
    e.send(get("https://basic.example/")).await.unwrap();
    e.send(get("https://header.example/")).await.unwrap();
    let seen = mock.seen();
    assert_eq!(seen[0].headers[AUTHORIZATION], "Bearer tok123");
    assert_eq!(
        seen[1].headers[AUTHORIZATION],
        "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
    );
    assert_eq!(seen[2].headers["x-api-key"], "k-1");
}

/// Provider that records what it was shown.
struct Recording(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl codelib_http::CredentialProvider for Recording {
    async fn credential(&self, req: &RequestInfo<'_>) -> Result<Credential, EgressError> {
        self.0
            .lock()
            .unwrap()
            .push(format!("{} {}", req.method, req.uri));
        Ok(Credential::Headers(vec![
            ("x-a".parse().unwrap(), "1".into()),
            ("x-b".parse().unwrap(), "2".into()),
        ]))
    }
}

#[tokio::test]
async fn provider_sees_request_info() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mock = Mock::ok();
    let e = egress(
        &mock,
        vec![Rule::host("api.example").credential(Recording(log.clone()))],
    );
    e.send(get("https://api.example/p?q=1")).await.unwrap();
    assert_eq!(*log.lock().unwrap(), ["GET https://api.example/p?q=1"]);
    let s = &mock.seen()[0];
    assert_eq!(s.headers["x-a"], "1");
    assert_eq!(s.headers["x-b"], "2");
}

struct BodySigner;

#[async_trait]
impl Signer for BodySigner {
    async fn sign(&self, req: Request) -> Result<Request, EgressError> {
        let (mut parts, body) = req.into_parts();
        let bytes = body.collect().await.map_err(EgressError::Other)?.to_bytes();
        let auth = parts.headers[AUTHORIZATION].to_str().unwrap().to_owned();
        let sig = format!("len={};auth={}", bytes.len(), auth);
        parts.headers.insert("x-signature", sig.parse().unwrap());
        Ok(Request::from_parts(parts, full_body(bytes)))
    }
}

#[tokio::test]
async fn signer_runs_after_credential_and_sees_body() {
    let mock = Mock::ok();
    let e = egress(
        &mock,
        vec![
            Rule::host("aws.example")
                .credential(StaticCredential::bearer("t"))
                .signer(BodySigner),
        ],
    );
    e.send(with_body(Method::PUT, "https://aws.example/obj", "hello"))
        .await
        .unwrap();
    let s = &mock.seen()[0];
    assert_eq!(s.headers["x-signature"], "len=5;auth=Bearer t");
    assert_eq!(s.body, "hello");
}

// -------------------------------------------------------------- middleware

#[tokio::test]
async fn middleware_never_sees_credentials() {
    let seen_by_mw: Arc<Mutex<Vec<HeaderMap>>> = Arc::default();
    let log = seen_by_mw.clone();
    let mock = Mock::ok();
    let e = Egress::builder()
        .transport(mock.clone())
        .allow(Rule::host("api.example").credential(StaticCredential::bearer("secret-token")))
        .layer(middleware::from_fn(move |req, next| {
            let log = log.clone();
            async move {
                log.lock().unwrap().push(req.headers().clone());
                let resp = next.run(req).await?;
                // Nor on the way back.
                Ok(resp)
            }
        }))
        .build();
    let mut r = get("https://api.example/");
    r.headers_mut()
        .insert(AUTHORIZATION, "Bearer from-script".parse().unwrap());
    e.send(r).await.unwrap();
    e.send(get("https://api.example/")).await.unwrap();

    let mw = seen_by_mw.lock().unwrap();
    assert_eq!(mw[0][AUTHORIZATION], "Bearer from-script");
    assert!(!mw[1].contains_key(AUTHORIZATION));
    for h in mw.iter() {
        for v in h.values() {
            assert!(!v.to_str().unwrap().contains("secret-token"));
        }
    }
    for s in mock.seen() {
        assert_eq!(s.headers[AUTHORIZATION], "Bearer secret-token");
    }
}

struct Tag(&'static str);

#[async_trait]
impl Middleware for Tag {
    async fn handle(&self, mut req: Request, next: Next<'_>) -> Result<Response, EgressError> {
        req.headers_mut().append("x-order", self.0.parse().unwrap());
        let mut resp = next.run(req).await?;
        resp.headers_mut()
            .append("x-order", self.0.parse().unwrap());
        Ok(resp)
    }
}

#[tokio::test]
async fn middlewares_run_in_order() {
    let mock = Mock::ok();
    let e = Egress::builder()
        .transport(mock.clone())
        .allow(Rule::host("api.example"))
        .layer(Tag("a"))
        .layer(Arc::new(Tag("b")))
        .layer(middleware::from_fn(|mut req, next| async move {
            req.headers_mut().append("x-order", "c".parse().unwrap());
            next.run(req).await
        }))
        .build();
    let resp = e.send(get("https://api.example/")).await.unwrap();
    let sent: Vec<_> = mock.seen()[0]
        .headers
        .get_all("x-order")
        .iter()
        .cloned()
        .collect();
    assert_eq!(sent, ["a", "b", "c"]);
    let back: Vec<_> = resp.headers().get_all("x-order").iter().cloned().collect();
    assert_eq!(back, ["b", "a"]);
}

#[tokio::test]
async fn middleware_short_circuit_and_deny() {
    let mock = Mock::ok();
    let e = Egress::builder()
        .transport(mock.clone())
        .allow(Rule::host("api.example"))
        .layer(middleware::from_fn(|req, next| async move {
            match req.uri().path() {
                "/cached" => Ok(text_response(StatusCode::OK, "from cache")),
                "/admin" => Err(EgressError::denied("admin is off limits")),
                _ => next.run(req).await,
            }
        }))
        .build();
    let resp = e.send(get("https://api.example/cached")).await.unwrap();
    assert_eq!(read_body(resp).await.unwrap(), "from cache");
    assert_eq!(
        assert_denied(e.send(get("https://api.example/admin")).await),
        "admin is off limits"
    );
    assert_eq!(mock.count(), 0);
    e.send(get("https://api.example/other")).await.unwrap();
    assert_eq!(mock.count(), 1);
}

#[tokio::test]
async fn middleware_cannot_redirect_credentials_elsewhere() {
    let mock = Mock::ok();
    let e = Egress::builder()
        .transport(mock.clone())
        .allow(Rule::host("api.example").credential(StaticCredential::bearer("secret")))
        .allow(Rule::host("other.example"))
        .layer(middleware::from_fn(|mut req, next| async move {
            *req.uri_mut() = "https://other.example/steal".parse().unwrap();
            next.run(req).await
        }))
        .build();
    let reason = assert_denied(e.send(get("https://api.example/")).await);
    assert!(reason.contains("middleware"), "{reason}");
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn middlewares_do_not_run_for_denied_or_virtual() {
    let calls = Arc::new(Mutex::new(0));
    let c = calls.clone();
    let mock = Mock::ok();
    let e = Egress::builder()
        .transport(mock.clone())
        .allow(Rule::host("api.example"))
        .virtual_host("files.internal", |_req| async {
            text_response(StatusCode::OK, "file")
        })
        .layer(middleware::from_fn(move |req, next| {
            *c.lock().unwrap() += 1;
            next.run(req)
        }))
        .build();
    assert_denied(e.send(get("https://nope.example/")).await);
    e.send(get("http://files.internal/x")).await.unwrap();
    assert_eq!(*calls.lock().unwrap(), 0);
}

// ----------------------------------------------------------- virtual hosts

struct Echo(Arc<Mutex<Vec<HeaderMap>>>);

#[async_trait]
impl VirtualHost for Echo {
    async fn handle(&self, req: Request) -> Response {
        self.0.lock().unwrap().push(req.headers().clone());
        let path = req.uri().path().to_owned();
        codelib_http::drain(req.into_body()).await;
        text_response(StatusCode::OK, format!("vhost {path}"))
    }
}

#[tokio::test]
async fn virtual_host_bypasses_allowlist_and_gets_no_credentials() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mock = Mock::ok();
    let e = Egress::builder()
        .transport(mock.clone())
        // Even a catch-all rule with a credential does not apply to virtual hosts.
        .allow(Rule::host("*").credential(StaticCredential::bearer("secret")))
        .virtual_host("Files.Internal", Echo(log.clone()))
        .build();
    let resp = e
        .send(with_body(
            Method::PUT,
            "http://files.internal:1234/out/a.txt",
            "data",
        ))
        .await
        .unwrap();
    assert_eq!(read_body(resp).await.unwrap(), "vhost /out/a.txt");
    assert!(!log.lock().unwrap()[0].contains_key(AUTHORIZATION));
    assert_eq!(mock.count(), 0);

    // A virtual host alone needs no rule at all.
    let e = Egress::builder()
        .transport(mock.clone())
        .virtual_host("files.internal", |_req| async {
            text_response(StatusCode::OK, "x")
        })
        .build();
    assert!(e.send(get("http://files.internal/")).await.is_ok());
    assert_denied(e.send(get("https://files.internal.example/")).await);
}

// ----------------------------------------------------------------- rewrite

#[tokio::test]
async fn rewrite_changes_url_and_host_header() {
    let mock = Mock::ok();
    let e = Egress::builder()
        .transport(mock.clone())
        .allow(Rule::host("api.example.com").credential(StaticCredential::bearer("t")))
        .allow(Rule::host("*").https_only(false))
        .rewrite("HTTPS://API.Example.com/", "http://10.0.0.5:8080/base/")
        .rewrite("https://api.example.com/", "http://never.example/")
        .rewrite("https://short.example", "http://10.0.0.6/")
        .build();

    let mut r = get("https://api.example.com/v1/x?y=1");
    r.headers_mut()
        .insert(HOST, "evil.example".parse().unwrap());
    e.send(r).await.unwrap();
    e.send(get("https://untouched.example/p")).await.unwrap();
    e.send(get("https://short.example/a")).await.unwrap();
    e.send(get("https://short.example.evil/a")).await.unwrap();

    let seen = mock.seen();
    assert_eq!(seen[0].uri, "http://10.0.0.5:8080/base/v1/x?y=1");
    assert_eq!(seen[0].headers[HOST], "10.0.0.5:8080");
    assert_eq!(seen[0].headers[AUTHORIZATION], "Bearer t");
    assert_eq!(
        seen[0].rewritten_from.as_ref().unwrap(),
        "https://api.example.com/v1/x?y=1"
    );

    assert_eq!(seen[1].uri, "https://untouched.example/p");
    assert_eq!(seen[1].headers[HOST], "untouched.example");
    assert!(seen[1].rewritten_from.is_none());

    assert_eq!(seen[2].uri, "http://10.0.0.6/a");
    // No match without a path boundary.
    assert_eq!(seen[3].uri, "https://short.example.evil/a");
}

#[tokio::test]
async fn rules_match_the_url_before_rewrite() {
    let mock = Mock::ok();
    let e = Egress::builder()
        .transport(mock.clone())
        .allow(Rule::host("public.example"))
        .rewrite("https://public.example/", "http://internal.example/")
        .rewrite("https://other.example/", "https://public.example/")
        .build();
    // Rewritten target is plain http on a host with no rule: still allowed.
    e.send(get("https://public.example/x")).await.unwrap();
    // A URL that would rewrite onto an allowed host is still denied.
    assert_denied(e.send(get("https://other.example/x")).await);
    assert_eq!(mock.count(), 1);
}

// --------------------------------------------------------------- redirects

#[tokio::test]
async fn manual_redirects_are_returned() {
    let mock = Mock::new(|_, _| redirect(302, "https://elsewhere.example/"));
    let e = egress(&mock, vec![Rule::host("api.example")]);
    let resp = e.send(get("https://api.example/")).await.unwrap();
    assert_eq!(resp.status(), 302);
    assert_eq!(mock.count(), 1);
}

fn follow(mock: &Mock, n: usize, rules: Vec<Rule>) -> Egress {
    rules
        .into_iter()
        .fold(
            Egress::builder()
                .transport(mock.clone())
                .redirects(Redirects::Follow(n)),
            |b, r| b.allow(r),
        )
        .build()
}

#[tokio::test]
async fn see_other_turns_post_into_get() {
    for status in [301, 302, 303] {
        let mock = Mock::new(move |s, _| {
            if s.uri.path() == "/start" {
                redirect(status, "/done")
            } else {
                text_response(StatusCode::OK, "done")
            }
        });
        let e = follow(&mock, 5, vec![Rule::host("api.example")]);
        let resp = e
            .send(with_body(
                Method::POST,
                "https://api.example/start",
                "payload",
            ))
            .await
            .unwrap();
        assert_eq!(read_body(resp).await.unwrap(), "done");
        let seen = mock.seen();
        assert_eq!(seen[0].method, Method::POST);
        assert_eq!(seen[0].body, "payload");
        assert_eq!(seen[1].method, Method::GET, "{status}");
        assert_eq!(seen[1].uri, "https://api.example/done");
        assert!(seen[1].body.is_empty());
        assert!(!seen[1].headers.contains_key(CONTENT_TYPE));
        assert!(!seen[1].headers.contains_key(CONTENT_LENGTH));
    }
}

#[tokio::test]
async fn temporary_redirect_resends_body() {
    for status in [307, 308] {
        let mock = Mock::new(move |s, _| {
            if s.uri.path() == "/start" {
                redirect(status, "https://api.example/again")
            } else {
                text_response(StatusCode::OK, "done")
            }
        });
        let e = follow(&mock, 5, vec![Rule::host("api.example")]);
        let chunks = Chunks::new([b"pay".as_slice(), b"load".as_slice()]);
        let r = codelib_http::http::Request::put("https://api.example/start")
            .header(CONTENT_TYPE, "text/plain")
            .body(chunks.body())
            .unwrap();
        e.send(r).await.unwrap();
        let seen = mock.seen();
        assert_eq!(seen.len(), 2);
        for s in &seen {
            assert_eq!(s.method, Method::PUT);
            assert_eq!(s.body, "payload");
            assert_eq!(s.headers[CONTENT_TYPE], "text/plain");
        }
    }
}

#[tokio::test]
async fn redirect_to_unlisted_host_is_denied() {
    let mock = Mock::new(|_, _| redirect(302, "https://evil.example/collect"));
    let e = follow(&mock, 5, vec![Rule::host("api.example")]);
    let reason = assert_denied(e.send(get("https://api.example/")).await);
    assert!(reason.contains("not allowed"), "{reason}");
    assert_eq!(mock.count(), 1);
}

#[tokio::test]
async fn redirect_hops_get_their_own_credentials_only() {
    let mock = Mock::new(|s, _| match s.uri.host().unwrap() {
        "a.example" => redirect(302, "https://b.example/"),
        "b.example" => redirect(302, "https://c.example/"),
        _ => text_response(StatusCode::OK, "ok"),
    });
    let e = follow(
        &mock,
        5,
        vec![
            Rule::host("a.example").credential(StaticCredential::bearer("for-a")),
            Rule::host("b.example"),
            Rule::host("c.example").credential(StaticCredential::bearer("for-c")),
        ],
    );
    e.send(get("https://a.example/")).await.unwrap();
    let seen = mock.seen();
    assert_eq!(seen[0].headers[AUTHORIZATION], "Bearer for-a");
    assert!(!seen[1].headers.contains_key(AUTHORIZATION));
    assert_eq!(seen[2].headers[AUTHORIZATION], "Bearer for-c");
}

#[tokio::test]
async fn script_secrets_stripped_when_leaving_host() {
    let mock = Mock::new(|s, _| match s.uri.host().unwrap() {
        "a.example" if s.uri.path() == "/" => redirect(302, "https://api.a.example/"),
        "api.a.example" => redirect(302, "https://b.example/"),
        "b.example" => redirect(302, "https://a.example/back"),
        _ => text_response(StatusCode::OK, "ok"),
    });
    let mock2 = mock.clone();
    let e = follow(&mock, 5, vec![Rule::host("*")]);
    let mut r = get("https://a.example/");
    r.headers_mut()
        .insert(AUTHORIZATION, "Bearer script".parse().unwrap());
    r.headers_mut().insert(COOKIE, "sid=1".parse().unwrap());
    r.headers_mut()
        .insert("proxy-authorization", "Basic eA==".parse().unwrap());
    r.headers_mut().insert("x-keep", "1".parse().unwrap());
    let _ = e.send(r).await.unwrap();
    let seen = mock2.seen();
    assert_eq!(seen.len(), 4);
    // Same host and subdomain keep them.
    for s in &seen[..2] {
        assert_eq!(s.headers[AUTHORIZATION], "Bearer script");
        assert_eq!(s.headers[COOKIE], "sid=1");
        assert!(s.headers.contains_key("proxy-authorization"));
    }
    // Leaving drops them, and they do not come back on return.
    for s in &seen[2..] {
        assert!(!s.headers.contains_key(AUTHORIZATION));
        assert!(!s.headers.contains_key(COOKIE));
        assert!(!s.headers.contains_key("proxy-authorization"));
        assert_eq!(s.headers["x-keep"], "1");
    }
}

#[tokio::test]
async fn script_secrets_stripped_on_downgrade() {
    let mock = Mock::new(|s, _| {
        if s.uri.scheme_str() == Some("https") {
            redirect(301, "http://a.example/plain")
        } else {
            text_response(StatusCode::OK, "ok")
        }
    });
    let e = follow(&mock, 5, vec![Rule::host("a.example").https_only(false)]);
    let mut r = get("https://a.example/");
    r.headers_mut().insert(COOKIE, "sid=1".parse().unwrap());
    e.send(r).await.unwrap();
    let seen = mock.seen();
    assert_eq!(seen[0].headers[COOKIE], "sid=1");
    assert!(!seen[1].headers.contains_key(COOKIE));
}

#[tokio::test]
async fn redirect_limit() {
    let mock = Mock::new(|_, i| redirect(302, &format!("/hop{}", i + 1)));
    let e = follow(&mock, 2, vec![Rule::host("api.example")]);
    match e.send(get("https://api.example/")).await {
        Err(EgressError::TooManyRedirects) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(mock.count(), 3);

    // Exactly n redirects is fine.
    let mock = Mock::new(|_, i| {
        if i < 2 {
            redirect(302, &format!("/hop{}", i + 1))
        } else {
            text_response(StatusCode::OK, "end")
        }
    });
    let e = follow(&mock, 2, vec![Rule::host("api.example")]);
    let resp = e.send(get("https://api.example/")).await.unwrap();
    assert_eq!(read_body(resp).await.unwrap(), "end");
    assert_eq!(mock.seen()[2].uri, "https://api.example/hop2");
}

#[tokio::test]
async fn location_resolves_against_pre_rewrite_url() {
    let mock = Mock::new(|s, _| {
        if s.uri.path() == "/start" {
            redirect(302, "next?a=1")
        } else {
            text_response(StatusCode::OK, "ok")
        }
    });
    let e = Egress::builder()
        .transport(mock.clone())
        .redirects(Redirects::Follow(3))
        .allow(Rule::host("api.example"))
        .rewrite("https://api.example/", "http://10.1.1.1:9000/")
        .build();
    e.send(get("https://api.example/start")).await.unwrap();
    let seen = mock.seen();
    assert_eq!(seen[1].uri, "http://10.1.1.1:9000/next?a=1");
    assert_eq!(
        seen[1].rewritten_from.as_ref().unwrap(),
        "https://api.example/next?a=1"
    );
}

#[tokio::test]
async fn non_followed_statuses_pass_through() {
    let mock = Mock::new(|_, _| redirect(304, "/x"));
    let e = follow(&mock, 3, vec![Rule::host("api.example")]);
    assert_eq!(
        e.send(get("https://api.example/")).await.unwrap().status(),
        304
    );
}

#[tokio::test]
async fn buffered_request_body_is_capped() {
    let mock = Mock::ok();
    let e = Egress::builder()
        .transport(mock.clone())
        .redirects(Redirects::Follow(3))
        .max_buffered_request_bytes(4)
        .allow(Rule::host("api.example"))
        .build();
    match e
        .send(with_body(Method::POST, "https://api.example/", "too long"))
        .await
    {
        Err(EgressError::BodyTooLarge) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(mock.count(), 0);
}

// ------------------------------------------------------- limits & timeouts

#[tokio::test]
async fn max_response_bytes_streaming() {
    let mock = Mock::new(|s, _| {
        let chunks: Vec<&'static [u8]> = match s.uri.path() {
            "/small" => vec![b"12345", b"67890"],
            _ => vec![b"12345", b"67890", b"x"],
        };
        Response::new(Chunks::new(chunks).body())
    });
    let e = Egress::builder()
        .transport(mock.clone())
        .allow(Rule::host("api.example"))
        .max_response_bytes(10)
        .build();
    let resp = e.send(get("https://api.example/small")).await.unwrap();
    assert_eq!(read_body(resp).await.unwrap(), "1234567890");
    let resp = e.send(get("https://api.example/big")).await.unwrap();
    match read_body(resp).await {
        Err(EgressError::BodyTooLarge) => {}
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn max_response_bytes_declared_length() {
    let mock = Mock::new(|_, _| {
        let mut r = Response::new(full_body("x".repeat(100)));
        r.headers_mut()
            .insert(CONTENT_LENGTH, "100".parse().unwrap());
        r
    });
    let e = Egress::builder()
        .transport(mock)
        .allow(Rule::host("api.example"))
        .max_response_bytes(10)
        .build();
    match e.send(get("https://api.example/")).await {
        Err(EgressError::BodyTooLarge) => {}
        other => panic!("{other:?}"),
    }
}

struct Slow;

#[async_trait]
impl codelib_http::Transport for Slow {
    async fn send(&self, _req: Request) -> Result<Response, EgressError> {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(text_response(StatusCode::OK, "late"))
    }
}

#[tokio::test]
async fn request_timeout() {
    let e = Egress::builder()
        .transport(Slow)
        .allow(Rule::host("api.example"))
        .request_timeout(Duration::from_millis(50))
        .build();
    let start = std::time::Instant::now();
    match e.send(get("https://api.example/")).await {
        Err(EgressError::Timeout) => {}
        other => panic!("{other:?}"),
    }
    assert!(start.elapsed() < Duration::from_secs(5));
}

// ------------------------------------------------------------------- misc

#[test]
fn send_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let e = Egress::deny_all();
    let fut = e.send(get("https://a.example/"));
    assert_send(&fut);
}

#[tokio::test]
async fn allow_all_allows_plain_http() {
    // allow_all uses the real transport; just check policy lets the request
    // through to it (connection to a closed port fails at the transport).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let r = Egress::allow_all()
        .send(get(&format!("http://127.0.0.1:{port}/")))
        .await;
    assert!(matches!(r, Err(EgressError::Connect(_))), "{r:?}");
}

#[test]
fn errors_display_without_secrets() {
    let e = EgressError::denied("host not allowed");
    assert_eq!(e.to_string(), "request denied: host not allowed");
    let boxed: codelib_http::BoxError = Box::new(EgressError::BodyTooLarge);
    assert!(matches!(
        EgressError::from_body_error(boxed),
        EgressError::BodyTooLarge
    ));
    let other: codelib_http::BoxError = "boom".into();
    assert!(matches!(
        EgressError::from_body_error(other),
        EgressError::Protocol(_)
    ));
}
