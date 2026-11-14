//! The policy pipeline: virtual hosts, allow rules, middlewares, credentials,
//! rewrites, timeout and size cap, in a fixed order.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use http::uri::Scheme;
use http::{HeaderValue, Method, StatusCode, Uri, header};
use http_body::Body as _;
use http_body_util::BodyExt;

use crate::body::{CappedBody, IdleTimeoutBody, Request, Response, drain};
use crate::credential::RequestInfo;
use crate::error::EgressError;
use crate::middleware::{Middleware, Next, VirtualHost};
use crate::redirect::Redirects;
use crate::rule::{Rule, forbidden_method, normalize_host, path_is_safe, prefix_matches};
use crate::transport::{HyperTransport, Rewritten, Transport};

/// Default for [`EgressBuilder::request_timeout`].
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default for [`EgressBuilder::max_buffered_request_bytes`].
pub const DEFAULT_MAX_BUFFERED_REQUEST_BYTES: u64 = 32 * 1024 * 1024;

/// The egress policy engine. Cheap to clone; `Send + Sync`.
///
/// Every request is first validated: it must be an absolute `http`/`https`
/// URL (the scheme is normalised to lower case) with a host and **without
/// user info** (`user:password@`); anything else fails with
/// [`EgressError::InvalidRequest`]. Then, for the request and again for every
/// followed redirect hop:
///
/// 1. A virtual host (exact host match) handles it; done. No allowlist, no
///    credentials.
/// 2. `TRACE`, `TRACK` and `CONNECT` are refused (TRACE echoes the request,
///    injected credentials included, back to the caller). Then the first
///    [`Rule`] whose host pattern (and path prefix) matches wins; none means
///    [`EgressError::Denied`]. Then the rule's method and https checks.
/// 3. Custom middlewares ([`EgressBuilder::layer`]), in order, before any
///    credential is attached.
/// 4. The `Host` header is set from the URL (so nothing below sees a
///    script-chosen `Host`); the rule's credential provider is consulted;
///    the rule's static headers and credential are written, overwriting
///    script-set headers of the same names; then its
///    [`Signer`](crate::Signer) runs.
/// 5. The first matching URL rewrite (rules matched on the URL before it).
/// 6. Transport send under the request timeout; response body capped at
///    `max_response_bytes` and bounded by `body_idle_timeout` while
///    streaming.
#[derive(Clone)]
pub struct Egress {
    inner: Arc<Inner>,
}

impl fmt::Debug for Egress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let i = &self.inner;
        f.debug_struct("Egress")
            .field("rules", &i.rules)
            .field("virtual_hosts", &i.virtual_hosts.keys().collect::<Vec<_>>())
            .field("rewrites", &i.rewrites.len())
            .field("redirects", &i.redirects)
            .field("request_timeout", &i.request_timeout)
            .field("body_idle_timeout", &i.body_idle_timeout)
            .field("max_response_bytes", &i.max_response_bytes)
            .field("layers", &i.layers.len())
            .finish()
    }
}

impl Egress {
    /// Start configuring an egress policy (deny by default).
    pub fn builder() -> EgressBuilder {
        EgressBuilder {
            rules: Vec::new(),
            virtual_hosts: HashMap::new(),
            rewrites: Vec::new(),
            redirects: Redirects::Manual,
            deny_private_ips: true,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            body_idle_timeout: None,
            max_response_bytes: None,
            max_buffered_request_bytes: DEFAULT_MAX_BUFFERED_REQUEST_BYTES,
            layers: Vec::new(),
            transport: None,
        }
    }

    /// Deny every request: the default for a sandbox with no network.
    pub fn deny_all() -> Egress {
        Egress::builder().build()
    }

    /// Allow every host over http and https, with no credentials and no
    /// private-IP blocking. For development only. `TRACE`, `TRACK` and
    /// `CONNECT` and URLs with user info are still refused.
    pub fn allow_all() -> Egress {
        Egress::builder()
            .allow(Rule::host("*").https_only(false))
            .deny_private_ips(false)
            .build()
    }

    /// Run one request through the policy and, if allowed, the transport.
    ///
    /// When the request is refused by policy before reaching the transport,
    /// its body is drained (bounded by the request timeout) before the error
    /// is returned. The returned future is `Send`.
    pub async fn send(&self, mut req: Request) -> Result<Response, EgressError> {
        if let Err(e) = validate(&mut req) {
            return Err(self.inner.refuse(req, e).await);
        }
        match self.inner.redirects {
            Redirects::Manual => self.inner.hop(req).await,
            Redirects::Follow(max) => self.inner.follow(req, max).await,
        }
    }
}

/// Builder for [`Egress`].
pub struct EgressBuilder {
    rules: Vec<Rule>,
    virtual_hosts: HashMap<String, Arc<dyn VirtualHost>>,
    rewrites: Vec<Rewrite>,
    redirects: Redirects,
    deny_private_ips: bool,
    request_timeout: Duration,
    body_idle_timeout: Option<Duration>,
    max_response_bytes: Option<u64>,
    max_buffered_request_bytes: u64,
    layers: Vec<Arc<dyn Middleware>>,
    transport: Option<Arc<dyn Transport>>,
}

impl fmt::Debug for EgressBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EgressBuilder")
            .field("rules", &self.rules)
            .field(
                "virtual_hosts",
                &self.virtual_hosts.keys().collect::<Vec<_>>(),
            )
            .field("redirects", &self.redirects)
            .finish_non_exhaustive()
    }
}

impl EgressBuilder {
    /// Add an allow rule. Rules are tried in the order added; the first whose
    /// host pattern matches decides.
    pub fn allow(mut self, rule: Rule) -> Self {
        self.rules.push(rule);
        self
    }

    /// Serve `host` (exact, case-insensitive, port ignored) in-process.
    pub fn virtual_host(mut self, host: impl Into<String>, handler: impl VirtualHost) -> Self {
        self.virtual_hosts
            .insert(normalize_host(&host.into()), Arc::new(handler));
        self
    }

    /// Send requests under `from` (e.g. `https://api.example.com/v1/`) to
    /// `to` (e.g. `http://10.0.0.5:8080/base/`) just before the transport.
    ///
    /// Both must be absolute `http`/`https` URLs with a host and an optional
    /// base path, and without user info, query or fragment. A request
    /// matches when its scheme, host (case-insensitive) and effective port
    /// (default ports made explicit, so `:443` on https equals no port) equal
    /// `from`'s, and its path lies under `from`'s path at a segment boundary
    /// (`/v1` matches `/v1` and `/v1/...`, not `/v1x`). The result keeps
    /// `to`'s scheme, host and port exactly; its path is `to`'s base path
    /// joined to the rest of the request path with a single `/`, and the
    /// query is preserved. Trailing slashes on either side do not change the
    /// result. A rest of path containing dot segments (also encoded or with
    /// `;params`), backslashes or encoded slashes is
    /// [`Denied`](EgressError::Denied), since the upstream could resolve it
    /// outside `to`'s base path.
    ///
    /// The first matching rewrite applies, once. Rules are matched against
    /// the URL before the rewrite. Rewritten requests are exempt from
    /// private-IP blocking.
    ///
    /// # Panics
    /// If `from` or `to` is not as described above (this is static
    /// configuration). The message does not repeat the URL.
    pub fn rewrite(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.rewrites.push(Rewrite {
            from: Endpoint::parse(&from.into(), "from"),
            to: Endpoint::parse(&to.into(), "to"),
        });
        self
    }

    /// Redirect handling (default [`Redirects::Manual`]).
    pub fn redirects(mut self, r: Redirects) -> Self {
        self.redirects = r;
        self
    }

    /// Refuse non-public destination addresses (default `true`). Configures
    /// the default [`HyperTransport`] only; a transport passed to
    /// [`transport`](Self::transport) carries its own setting.
    pub fn deny_private_ips(mut self, yes: bool) -> Self {
        self.deny_private_ips = yes;
        self
    }

    /// Time limit per hop from handing the request to the transport until the
    /// response headers arrive (default 30 s). Also bounds buffering of
    /// request bodies for redirect following and draining of denied bodies,
    /// and is the default for [`body_idle_timeout`](Self::body_idle_timeout).
    pub fn request_timeout(mut self, d: Duration) -> Self {
        self.request_timeout = d;
        self
    }

    /// Longest wait for the next piece of a response body from the transport
    /// (default: the request timeout). The timer only runs while the reader
    /// is waiting and restarts on every frame, so a large download that
    /// keeps flowing is never cut off; a server that goes quiet mid-body is.
    /// Elapsing makes the body yield a boxed [`EgressError::Timeout`] (see
    /// [`EgressError::from_body_error`]).
    pub fn body_idle_timeout(mut self, d: Duration) -> Self {
        self.body_idle_timeout = Some(d);
        self
    }

    /// Cap on response body bytes from the transport (default: unlimited). A
    /// larger declared `Content-Length` fails the request with
    /// [`EgressError::BodyTooLarge`] (except on responses that have no body:
    /// to HEAD, and 1xx, 204 and 304); otherwise the body yields a boxed
    /// `EgressError::BodyTooLarge` as soon as the cap is crossed (see
    /// [`EgressError::from_body_error`]).
    pub fn max_response_bytes(mut self, n: u64) -> Self {
        self.max_response_bytes = Some(n);
        self
    }

    /// Cap on request bodies buffered for [`Redirects::Follow`] (default
    /// 32 MiB). Larger bodies fail with [`EgressError::BodyTooLarge`]. Unused
    /// with [`Redirects::Manual`], where bodies stream.
    pub fn max_buffered_request_bytes(mut self, n: u64) -> Self {
        self.max_buffered_request_bytes = n;
        self
    }

    /// Add a custom middleware. Middlewares run in the order added, after
    /// rule matching and before credentials.
    pub fn layer(mut self, m: impl Middleware) -> Self {
        self.layers.push(Arc::new(m));
        self
    }

    /// Replace the default [`HyperTransport`].
    pub fn transport(mut self, t: impl Transport) -> Self {
        self.transport = Some(Arc::new(t));
        self
    }

    /// Finish.
    pub fn build(self) -> Egress {
        let transport = match self.transport {
            Some(t) => t,
            None => Arc::new(
                HyperTransport::builder()
                    .deny_private_ips(self.deny_private_ips)
                    .build()
                    .expect("default HyperTransport configuration is valid"),
            ),
        };
        Egress {
            inner: Arc::new(Inner {
                rules: self.rules,
                virtual_hosts: self.virtual_hosts,
                rewrites: self.rewrites,
                redirects: self.redirects,
                request_timeout: self.request_timeout,
                body_idle_timeout: self.body_idle_timeout.unwrap_or(self.request_timeout),
                max_response_bytes: self.max_response_bytes,
                max_buffered_request_bytes: self.max_buffered_request_bytes,
                layers: self.layers,
                transport,
            }),
        }
    }
}

/// Scheme, host and effective port of `uri`, for comparing a request before
/// and after a step that must not redirect it.
fn origin(uri: &Uri) -> Option<(String, String, u16)> {
    let scheme = uri.scheme()?;
    let host = normalize_host(uri.host()?);
    let port = uri.port_u16().unwrap_or_else(|| default_port(scheme));
    Some((scheme.as_str().to_ascii_lowercase(), host, port))
}

fn default_port(scheme: &Scheme) -> u16 {
    if *scheme == Scheme::HTTPS { 443 } else { 80 }
}

/// Configuration error in [`EgressBuilder::rewrite`]. Never repeats the URL,
/// which could carry something sensitive.
fn bad_rewrite(which: &str, why: &str) -> ! {
    panic!("EgressBuilder::rewrite: `{which}` {why}")
}

/// One side of a rewrite, parsed at configuration time.
#[derive(Debug)]
struct Endpoint {
    /// `Scheme::HTTP` or `Scheme::HTTPS`.
    scheme: Scheme,
    /// Normalised host (lower case, IPv6 brackets kept, no port).
    host: String,
    /// Effective port.
    port: u16,
    /// Authority as configured, lower-cased (host and optional port).
    authority: String,
    /// Base path, starting with `/`.
    path: String,
}

impl Endpoint {
    fn parse(url: &str, which: &str) -> Endpoint {
        let url = url.trim();
        if url.contains('#') {
            bad_rewrite(which, "must not have a fragment");
        }
        if url.contains('?') {
            bad_rewrite(which, "must not have a query");
        }
        let Ok(uri) = url.parse::<Uri>() else {
            bad_rewrite(which, "is not a valid URL");
        };
        let scheme = match uri.scheme_str() {
            Some(s) if s.eq_ignore_ascii_case("https") => Scheme::HTTPS,
            Some(s) if s.eq_ignore_ascii_case("http") => Scheme::HTTP,
            _ => bad_rewrite(which, "must be an absolute http or https URL"),
        };
        let Some(authority) = uri.authority() else {
            bad_rewrite(which, "has no host");
        };
        if authority.as_str().contains('@') {
            bad_rewrite(which, "must not contain user info");
        }
        let host = match uri.host() {
            Some(h) if !h.is_empty() => normalize_host(h),
            _ => bad_rewrite(which, "has no host"),
        };
        let port = uri.port_u16().unwrap_or_else(|| default_port(&scheme));
        let path = match uri.path() {
            "" => "/".to_owned(),
            p => p.to_owned(),
        };
        Endpoint {
            scheme,
            host,
            port,
            authority: authority.as_str().to_ascii_lowercase(),
            path,
        }
    }

    /// The base path without a trailing `/` (`""` for the root).
    fn base(&self) -> &str {
        self.path.strip_suffix('/').unwrap_or(&self.path)
    }

    /// The part of `path` below this endpoint's base path (`""` or starting
    /// with `/`), if `path` is under it at a segment boundary.
    fn remainder<'p>(&self, path: &'p str) -> Option<&'p str> {
        prefix_matches(path, &self.path).then(|| &path[self.base().len()..])
    }
}

#[derive(Debug)]
struct Rewrite {
    from: Endpoint,
    to: Endpoint,
}

/// Where a request that passed the policy goes next.
pub(crate) enum Route {
    Virtual(Arc<dyn VirtualHost>),
    Rule(usize),
}

pub(crate) struct Inner {
    rules: Vec<Rule>,
    virtual_hosts: HashMap<String, Arc<dyn VirtualHost>>,
    rewrites: Vec<Rewrite>,
    redirects: Redirects,
    pub(crate) request_timeout: Duration,
    body_idle_timeout: Duration,
    max_response_bytes: Option<u64>,
    pub(crate) max_buffered_request_bytes: u64,
    pub(crate) layers: Vec<Arc<dyn Middleware>>,
    transport: Arc<dyn Transport>,
}

/// Scheme and normalized host of a validated absolute http(s) URL.
pub(crate) struct Target {
    pub(crate) scheme: &'static str,
    pub(crate) host: String,
}

/// Check that `req` targets an absolute http(s) URL with a host and no user
/// info, and normalise its scheme to lower case in place, so every later
/// comparison (https-only, downgrade detection, rewrites, transport) sees
/// exactly `http` or `https`.
pub(crate) fn validate(req: &mut Request) -> Result<Target, EgressError> {
    let invalid = |msg: &str| EgressError::InvalidRequest(msg.to_owned());
    let uri = req.uri();
    let scheme = match uri.scheme_str() {
        Some(s) if s.eq_ignore_ascii_case("https") => Scheme::HTTPS,
        Some(s) if s.eq_ignore_ascii_case("http") => Scheme::HTTP,
        Some(s) => {
            return Err(EgressError::InvalidRequest(format!(
                "unsupported scheme {s}"
            )));
        }
        None => return Err(invalid("URL must be absolute")),
    };
    let Some(authority) = uri.authority() else {
        return Err(invalid("URL has no host"));
    };
    if authority.as_str().contains('@') {
        return Err(invalid("URL must not contain user info (user:password@)"));
    }
    let host = match uri.host() {
        Some(h) if !h.is_empty() => normalize_host(h),
        _ => return Err(invalid("URL has no host")),
    };
    if uri.scheme() != Some(&scheme) {
        let mut parts = std::mem::take(req.uri_mut()).into_parts();
        parts.scheme = Some(scheme.clone());
        *req.uri_mut() = Uri::from_parts(parts).map_err(|_| invalid("invalid URL"))?;
    }
    let scheme = if scheme == Scheme::HTTPS {
        "https"
    } else {
        "http"
    };
    Ok(Target { scheme, host })
}

/// Set `Host` from the request URL, replacing whatever was there.
fn set_host(req: &mut Request) {
    let value = req.uri().host().and_then(|h| {
        let v = match req.uri().port_u16() {
            Some(p) => format!("{h}:{p}"),
            None => h.to_owned(),
        };
        HeaderValue::from_str(&v).ok()
    });
    match value {
        Some(v) => {
            req.headers_mut().insert(header::HOST, v);
        }
        None => {
            req.headers_mut().remove(header::HOST);
        }
    }
}

impl Inner {
    /// Steps 1-2: decide where `req` goes, or why it may not go anywhere.
    /// Validates (and normalises) the URL. Logs denials.
    pub(crate) fn policy(&self, req: &mut Request) -> Result<Route, EgressError> {
        let target = validate(req)?;
        if let Some(vh) = self.virtual_hosts.get(&target.host) {
            return Ok(Route::Virtual(vh.clone()));
        }
        if forbidden_method(req.method()) {
            let reason = format!("method {} is never allowed", req.method());
            return Err(self.denied(req, &target.host, reason));
        }
        let path = req.uri().path();
        let Some(idx) = self
            .rules
            .iter()
            .position(|r| r.matches(&target.host, path))
        else {
            return Err(self.denied(req, &target.host, "host not allowed".to_owned()));
        };
        if let Err(reason) = self.rules[idx].check(req.method(), target.scheme, req.headers()) {
            return Err(self.denied(req, &target.host, reason));
        }
        Ok(Route::Rule(idx))
    }

    /// Steps 1-3 for one hop.
    pub(crate) async fn hop(self: &Arc<Self>, mut req: Request) -> Result<Response, EgressError> {
        let route = self.policy(&mut req);
        match route {
            Err(e) => Err(self.refuse(req, e).await),
            Ok(Route::Virtual(vh)) => {
                tracing::debug!(
                    method = %req.method(),
                    host = req.uri().host().unwrap_or(""),
                    path = req.uri().path(),
                    "egress: virtual host"
                );
                Ok(vh.handle(req).await)
            }
            Ok(Route::Rule(idx)) => {
                tracing::debug!(
                    method = %req.method(),
                    host = req.uri().host().unwrap_or(""),
                    path = req.uri().path(),
                    rule = idx,
                    "egress: allowed"
                );
                Next::new(self.clone(), idx).run(req).await
            }
        }
    }

    /// Steps 4-6, reached through [`Next::run`] after the last middleware.
    pub(crate) async fn dispatch(
        self: &Arc<Self>,
        idx: usize,
        mut req: Request,
    ) -> Result<Response, EgressError> {
        let rule = &self.rules[idx];

        // Middlewares may have changed the request; it must still fit its rule.
        let target = match validate(&mut req) {
            Ok(t) => t,
            Err(e) => return Err(self.refuse(req, e).await),
        };
        if !rule.matches(&target.host, req.uri().path()) {
            let reason = "request modified by middleware no longer matches its rule".to_owned();
            let e = self.denied(&req, &target.host, reason);
            return Err(self.refuse(req, e).await);
        }
        if let Err(reason) = rule.check(req.method(), target.scheme, req.headers()) {
            let e = self.denied(&req, &target.host, reason);
            return Err(self.refuse(req, e).await);
        }

        // From here on `Host` follows the URL: credential providers and
        // signers never see a script-chosen value.
        set_host(&mut req);
        let (mut parts, body) = req.into_parts();
        let resolved = match rule.credential_provider() {
            Some(provider) => {
                // Built before static headers are added: the provider sees
                // only the script's headers (after middlewares).
                let info = RequestInfo {
                    method: &parts.method,
                    uri: &parts.uri,
                    headers: &parts.headers,
                };
                Some(provider.credential(&info).await)
            }
            None => None,
        };
        for (name, value) in rule.static_headers() {
            parts.headers.insert(name.clone(), value.clone());
        }
        if let Some(resolved) = resolved
            && let Err(e) = resolved.and_then(|c| c.apply(&mut parts.headers))
        {
            tracing::warn!(
                method = %parts.method,
                host = %target.host,
                path = parts.uri.path(),
                error = %e,
                "egress: credential failed"
            );
            self.drain(body).await;
            return Err(e);
        }
        let mut req = Request::from_parts(parts, body);
        if let Some(signer) = rule.signer_ref() {
            // The rule matched, and its checks passed, for this destination
            // only: a signer may not send the request somewhere else.
            let before = origin(req.uri());
            req = signer.sign(req).await?;
            if origin(req.uri()) != before {
                return Err(self
                    .refuse(
                        req,
                        EgressError::InvalidRequest(
                            "a signer changed the request's scheme, host or port".to_owned(),
                        ),
                    )
                    .await);
            }
        }

        match self.rewrite(req.uri()) {
            Ok(None) => {}
            Ok(Some(uri)) => {
                tracing::debug!(
                    from_host = req.uri().host().unwrap_or(""),
                    to_host = uri.host().unwrap_or(""),
                    "egress: rewrote URL"
                );
                let original = std::mem::replace(req.uri_mut(), uri);
                req.extensions_mut().insert(Rewritten::new(original));
            }
            Err(e) => {
                if let EgressError::Denied { reason } = &e {
                    tracing::warn!(
                        method = %req.method(),
                        host = %target.host,
                        path = req.uri().path(),
                        reason = %reason,
                        "egress: denied"
                    );
                }
                return Err(self.refuse(req, e).await);
            }
        }
        self.transmit(req).await
    }

    /// The URL after the first matching rewrite, if any applies.
    fn rewrite(&self, uri: &Uri) -> Result<Option<Uri>, EgressError> {
        if self.rewrites.is_empty() {
            return Ok(None);
        }
        let Some(scheme) = uri.scheme() else {
            return Ok(None);
        };
        let host = normalize_host(uri.host().unwrap_or(""));
        let port = uri.port_u16().unwrap_or_else(|| default_port(scheme));
        let path = match uri.path() {
            "" => "/",
            p => p,
        };
        let Some((rw, rest)) = self.rewrites.iter().find_map(|rw| {
            let from = &rw.from;
            if from.scheme != *scheme || from.host != host || from.port != port {
                return None;
            }
            from.remainder(path).map(|rest| (rw, rest))
        }) else {
            return Ok(None);
        };
        if !path_is_safe(rest) {
            return Err(EgressError::denied(
                "path would leave the rewritten prefix (dot segment, backslash or encoded slash)",
            ));
        }

        let mut path_and_query = format!("{}{rest}", rw.to.base());
        if path_and_query.is_empty() {
            path_and_query.push('/');
        }
        if let Some(q) = uri.query() {
            path_and_query.push('?');
            path_and_query.push_str(q);
        }
        let invalid = || EgressError::InvalidRequest("rewrite produced an invalid URL".to_owned());
        let built = Uri::builder()
            .scheme(rw.to.scheme.clone())
            .authority(rw.to.authority.as_str())
            .path_and_query(path_and_query)
            .build()
            .map_err(|_| invalid())?;
        // The destination is the configured one, whatever the request path.
        let same_host = built.host().map(normalize_host).as_deref() == Some(rw.to.host.as_str());
        let same_port = built
            .port_u16()
            .unwrap_or_else(|| default_port(&rw.to.scheme))
            == rw.to.port;
        if !same_host || !same_port || built.scheme() != Some(&rw.to.scheme) {
            return Err(invalid());
        }
        Ok(Some(built))
    }

    /// Step 6: Host header, timeout, transport, idle timeout, size cap.
    async fn transmit(&self, mut req: Request) -> Result<Response, EgressError> {
        // Host always follows the (possibly rewritten) URL, never the script.
        set_host(&mut req);
        let head = req.method() == Method::HEAD;

        let send = self.transport.send(req);
        let resp = match tokio::time::timeout(self.request_timeout, send).await {
            Ok(r) => r?,
            Err(_) => return Err(EgressError::Timeout),
        };
        let idle = self.body_idle_timeout;
        let resp = resp.map(|b| IdleTimeoutBody::new(b, idle).boxed_unsync());

        let Some(max) = self.max_response_bytes else {
            return Ok(resp);
        };
        let status = resp.status();
        let bodyless = head
            || status.is_informational()
            || status == StatusCode::NO_CONTENT
            || status == StatusCode::NOT_MODIFIED;
        if !bodyless {
            let declared = resp
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .or(resp.body().size_hint().exact());
            if declared.is_some_and(|n| n > max) {
                return Err(EgressError::BodyTooLarge);
            }
        }
        Ok(resp.map(|b| CappedBody::new(b, max).boxed_unsync()))
    }

    /// Log a policy denial and return it.
    fn denied(&self, req: &Request, host: &str, reason: String) -> EgressError {
        tracing::warn!(
            method = %req.method(),
            host = %host,
            path = req.uri().path(),
            reason = %reason,
            "egress: denied"
        );
        EgressError::Denied { reason }
    }

    /// Drain the body, then return `err`.
    pub(crate) async fn refuse(&self, req: Request, err: EgressError) -> EgressError {
        self.drain(req.into_body()).await;
        err
    }

    async fn drain(&self, body: crate::body::Body) {
        let _ = tokio::time::timeout(self.request_timeout, drain(body)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewritten(from: &str, to: &str, url: &str) -> Result<Option<String>, EgressError> {
        let inner = Egress::builder().rewrite(from, to).build().inner;
        inner
            .rewrite(&url.parse().unwrap())
            .map(|u| u.map(|u| u.to_string()))
    }

    #[test]
    fn rewrite_slashes_and_base_paths() {
        for (from, to) in [
            ("https://a.example/", "http://b.example:8080/"),
            ("https://a.example", "http://b.example:8080"),
            ("https://a.example/", "http://b.example:8080"),
            ("https://a.example", "http://b.example:8080/"),
        ] {
            assert_eq!(
                rewritten(from, to, "https://a.example/x/y?q=1")
                    .unwrap()
                    .unwrap(),
                "http://b.example:8080/x/y?q=1",
                "{from} -> {to}"
            );
            assert_eq!(
                rewritten(from, to, "https://a.example/@evil.example/x")
                    .unwrap()
                    .unwrap(),
                "http://b.example:8080/@evil.example/x",
                "{from} -> {to}"
            );
        }
        for (from, to) in [
            ("https://a.example/v1/", "http://b.example/base/"),
            ("https://a.example/v1", "http://b.example/base"),
            ("https://a.example/v1/", "http://b.example/base"),
            ("https://a.example/v1", "http://b.example/base/"),
        ] {
            let r = |u| rewritten(from, to, u).unwrap();
            assert_eq!(
                r("https://a.example/v1/x").unwrap(),
                "http://b.example/base/x"
            );
            assert_eq!(
                r("https://a.example/v1/").unwrap(),
                "http://b.example/base/"
            );
            assert_eq!(r("https://a.example/v1x"), None, "{from}: no boundary");
            assert_eq!(r("https://a.example/v2/x"), None);
        }
        // `/v1` (no slash) also matches the bare `/v1`.
        assert_eq!(
            rewritten(
                "https://a.example/v1",
                "http://b.example/base",
                "https://a.example/v1"
            )
            .unwrap()
            .unwrap(),
            "http://b.example/base"
        );
    }

    #[test]
    fn rewrite_ports_scheme_and_case() {
        let r = |u| rewritten("https://A.Example/", "http://b.example/", u).unwrap();
        assert!(
            r("https://a.example:443/x").is_some(),
            "explicit default port"
        );
        assert!(r("HTTPS://A.EXAMPLE/x").is_some());
        assert!(r("https://a.example:8443/x").is_none(), "other port");
        assert!(r("http://a.example/x").is_none(), "other scheme");
        let r = |u| rewritten("https://a.example:8443/", "http://b.example/", u).unwrap();
        assert!(r("https://a.example:8443/x").is_some());
        assert!(r("https://a.example/x").is_none());
        let r = |u| rewritten("http://[::1]:8080/", "http://b.example/", u).unwrap();
        assert!(r("http://[::1]:8080/x").is_some());
    }

    #[test]
    fn rewrite_refuses_escaping_rest() {
        for url in [
            "https://a.example/v1/../admin",
            "https://a.example/v1/..;/admin",
            "https://a.example/v1/%2e%2e%2fadmin",
            "https://a.example/v1/x%5c..",
        ] {
            let r = rewritten("https://a.example/v1/", "http://b.example/base/", url);
            assert!(matches!(r, Err(EgressError::Denied { .. })), "{url}: {r:?}");
        }
    }

    #[test]
    fn rewrite_config_is_validated() {
        for (from, to) in [
            ("https://a.example/?x=1", "http://b.example/"),
            ("https://a.example/#f", "http://b.example/"),
            ("https://user:pw@a.example/", "http://b.example/"),
            ("ftp://a.example/", "http://b.example/"),
            ("/relative", "http://b.example/"),
            ("https://a.example/", "http://b.example/?x"),
            ("https://a.example/", "http://user:topsecret@b.example/"),
            ("https://a.example/", "not a url"),
        ] {
            let r = std::panic::catch_unwind(|| Egress::builder().rewrite(from, to));
            let err = r.expect_err(&format!("{from} -> {to} accepted"));
            let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
            assert!(msg.starts_with("EgressBuilder::rewrite"), "{msg}");
            assert!(!msg.contains("topsecret"), "{msg}");
        }
    }

    #[test]
    fn egress_is_send_sync_clone() {
        fn assert<T: Send + Sync + Clone>() {}
        assert::<Egress>();
    }
}
