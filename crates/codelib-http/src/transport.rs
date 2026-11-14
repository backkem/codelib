//! The terminal network client: hyper + rustls (ring), with private-IP blocking.

use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use http::{Uri, header};
use http_body_util::BodyExt;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::rt::{TokioExecutor, TokioTimer};

use crate::body::{Body, BoxError, Request, Response};
use crate::error::{EgressError, error_chain};

/// Terminal network client. [`HyperTransport`] is the default; replace it for
/// tests or custom TLS with [`EgressBuilder::transport`](crate::EgressBuilder::transport).
///
/// A transport receives an absolute-URI request whose `Host` header has been
/// set from the URI. A request that went through a URL rewrite carries a
/// [`Rewritten`] extension.
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// Send the request and return the response once its headers arrive.
    async fn send(&self, req: Request) -> Result<Response, EgressError>;
}

#[async_trait]
impl<T: Transport + ?Sized> Transport for Arc<T> {
    async fn send(&self, req: Request) -> Result<Response, EgressError> {
        (**self).send(req).await
    }
}

/// Request extension set by [`Egress`](crate::Egress) when a URL rewrite
/// applied. Rewritten requests target embedder-chosen destinations and are
/// exempt from private-IP blocking. Only this crate can create it.
#[derive(Clone, Debug)]
pub struct Rewritten {
    original: Uri,
}

impl Rewritten {
    pub(crate) fn new(original: Uri) -> Self {
        Self { original }
    }

    /// The URL before the rewrite.
    pub fn original(&self) -> &Uri {
        &self.original
    }
}

/// Is `ip` an address the sandbox must not reach when private-IP blocking is
/// on? Covers unspecified, loopback, RFC 1918, link-local (including the
/// cloud metadata address 169.254.169.254), CGNAT 100.64/10, multicast,
/// broadcast and reserved IPv4; unspecified, loopback, unique-local,
/// link-local, site-local and multicast IPv6; IPv4-mapped, IPv4-compatible,
/// well-known NAT64 (64:ff9b::/96) and 6to4 (2002::/16) forms of blocked
/// IPv4 addresses; and all of local-use NAT64 (64:ff9b:1::/48, whose
/// translation is network-specific) and Teredo (2001::/32).
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => is_private_v6(v6),
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_unspecified()
        || a == 0 // 0.0.0.0/8 "this network"
        || ip.is_loopback() // 127/8
        || ip.is_private() // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local() // 169.254/16
        || (a == 100 && (b & 0xc0) == 64) // 100.64/10 CGNAT
        || (a == 192 && b == 0 && c == 0) // 192.0.0/24 IETF protocol assignments
        || (a == 198 && (b & 0xfe) == 18) // 198.18/15 benchmarking
        || ip.is_multicast() // 224/4
        || a >= 240 // 240/4 reserved + broadcast
}

fn is_private_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_private_v4(v4);
    }
    let seg = ip.segments();
    let embedded =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    // NAT64 well-known prefix 64:ff9b::/96 and deprecated IPv4-compatible ::/96.
    if (seg[0] == 0x64 && seg[1] == 0xff9b && seg[2..6] == [0, 0, 0, 0])
        || (seg[..6] == [0, 0, 0, 0, 0, 0] && !ip.is_loopback() && !ip.is_unspecified())
    {
        return is_private_v4(embedded(seg[6], seg[7]));
    }
    // 6to4 2002::/16 carries the IPv4 address in bits 16..48.
    if seg[0] == 0x2002 {
        return is_private_v4(embedded(seg[1], seg[2]));
    }
    ip.is_unspecified()
        || ip.is_loopback()
        || (seg[0] == 0x64 && seg[1] == 0xff9b && seg[2] == 1) // 64:ff9b:1::/48 local-use NAT64
        || (seg[0] == 0x2001 && seg[1] == 0) // 2001::/32 Teredo
        || (seg[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
        || (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 link local
        || (seg[0] & 0xffc0) == 0xfec0 // fec0::/10 site local (deprecated)
        || ip.is_multicast()
}

/// Connection refused because the destination is not a public address.
#[derive(Debug)]
struct BlockedAddress {
    host: String,
}

impl fmt::Display for BlockedAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} does not resolve to a public address (private-IP blocking is on)",
            self.host
        )
    }
}

impl std::error::Error for BlockedAddress {}

/// Name resolution failed.
#[derive(Debug)]
struct DnsFailure(String);

impl fmt::Display for DnsFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DnsFailure {}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Resolver that only returns public addresses. The connector dials exactly
/// the addresses returned here, so there is no second lookup to rebind.
#[derive(Clone, Debug)]
struct VettingResolver;

impl tower_service::Service<Name> for VettingResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = BoxError;
    type Future = BoxFuture<Result<Self::Response, BoxError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| Box::new(DnsFailure(format!("{host}: {e}"))) as BoxError)?
                .collect();
            if resolved.is_empty() {
                return Err(Box::new(DnsFailure(format!("{host}: no addresses"))) as BoxError);
            }
            let public: Vec<SocketAddr> = resolved
                .iter()
                .copied()
                .filter(|a| !is_private_ip(a.ip()))
                .collect();
            if public.is_empty() {
                tracing::warn!(
                    host = %host,
                    addr = %resolved[0].ip(),
                    "egress: refused connection to non-public address"
                );
                return Err(Box::new(BlockedAddress { host }) as BoxError);
            }
            Ok(public.into_iter())
        })
    }
}

/// Connector that rejects IP-literal hosts in blocked ranges (hostnames are
/// vetted by [`VettingResolver`]).
#[derive(Clone, Debug)]
struct GuardedConnector {
    inner: HttpConnector<VettingResolver>,
}

impl tower_service::Service<Uri> for GuardedConnector {
    type Response = <HttpConnector<VettingResolver> as tower_service::Service<Uri>>::Response;
    type Error = BoxError;
    type Future = BoxFuture<Result<Self::Response, BoxError>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        if let Some(host) = uri.host() {
            let bare = host
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host);
            if let Ok(ip) = bare.parse::<IpAddr>()
                && is_private_ip(ip)
            {
                tracing::warn!(addr = %ip, "egress: refused connection to non-public address");
                let err = BlockedAddress {
                    host: host.to_owned(),
                };
                return Box::pin(async move { Err(Box::new(err) as BoxError) });
            }
        }
        let fut = self.inner.call(uri);
        Box::pin(async move { fut.await.map_err(Into::into) })
    }
}

type GuardedClient = Client<HttpsConnector<GuardedConnector>, Body>;
type OpenClient = Client<HttpsConnector<HttpConnector>, Body>;

/// Default [`Transport`]: hyper (HTTP/1.1 and, via ALPN, HTTP/2) over rustls
/// with the `ring` provider and webpki roots, with connection pooling.
///
/// Requires a Tokio runtime when sending. Cheap to clone (shares the pool).
#[derive(Clone)]
pub struct HyperTransport {
    /// `None` when private-IP blocking is off.
    guarded: Option<GuardedClient>,
    open: OpenClient,
}

impl fmt::Debug for HyperTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HyperTransport")
            .field("deny_private_ips", &self.guarded.is_some())
            .finish()
    }
}

impl Default for HyperTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HyperTransport {
    /// Default settings: webpki roots, 10 s connect timeout, private IPs
    /// denied, HTTP/2 enabled.
    pub fn new() -> Self {
        Self::builder()
            .build()
            .expect("default HyperTransport configuration is valid")
    }

    /// Configure a transport.
    pub fn builder() -> HyperTransportBuilder {
        HyperTransportBuilder {
            extra_roots: Vec::new(),
            connect_timeout: Duration::from_secs(10),
            deny_private_ips: true,
            http2: true,
        }
    }
}

/// Builder for [`HyperTransport`].
#[derive(Clone, Debug)]
pub struct HyperTransportBuilder {
    extra_roots: Vec<Vec<u8>>,
    connect_timeout: Duration,
    deny_private_ips: bool,
    http2: bool,
}

impl HyperTransportBuilder {
    /// Trust an extra root CA (DER), in addition to the webpki roots.
    pub fn add_root_certificate(mut self, der: impl Into<Vec<u8>>) -> Self {
        self.extra_roots.push(der.into());
        self
    }

    /// Trust several extra root CAs (DER).
    pub fn root_certificates<I, C>(mut self, ders: I) -> Self
    where
        I: IntoIterator<Item = C>,
        C: Into<Vec<u8>>,
    {
        self.extra_roots.extend(ders.into_iter().map(Into::into));
        self
    }

    /// TCP connect timeout (default 10 s). Elapsing yields
    /// [`EgressError::Timeout`].
    pub fn connect_timeout(mut self, d: Duration) -> Self {
        self.connect_timeout = d;
        self
    }

    /// Refuse non-public destination addresses (default `true`). Requests
    /// carrying the [`Rewritten`] extension are exempt.
    pub fn deny_private_ips(mut self, yes: bool) -> Self {
        self.deny_private_ips = yes;
        self
    }

    /// Offer HTTP/2 via ALPN (default `true`).
    pub fn http2(mut self, yes: bool) -> Self {
        self.http2 = yes;
        self
    }

    /// Build the transport. Fails if an extra root certificate is not a
    /// valid DER certificate.
    pub fn build(self) -> Result<HyperTransport, EgressError> {
        let mut roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        for der in self.extra_roots {
            roots
                .add(rustls::pki_types::CertificateDer::from(der))
                .map_err(|e| EgressError::Tls(format!("invalid root certificate: {e}")))?;
        }
        // Always an explicit provider: the final binary may enable a second
        // rustls provider, which makes the process default ambiguous.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| EgressError::Tls(e.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth();

        let mut open_http = HttpConnector::new();
        open_http.enforce_http(false);
        open_http.set_connect_timeout(Some(self.connect_timeout));
        let open = Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .build(https(tls.clone(), self.http2, open_http));

        let guarded = self.deny_private_ips.then(|| {
            let mut http = HttpConnector::new_with_resolver(VettingResolver);
            http.enforce_http(false);
            http.set_connect_timeout(Some(self.connect_timeout));
            Client::builder(TokioExecutor::new())
                .pool_timer(TokioTimer::new())
                .build(https(tls, self.http2, GuardedConnector { inner: http }))
        });

        Ok(HyperTransport { guarded, open })
    }
}

fn https<H>(tls: rustls::ClientConfig, http2: bool, http: H) -> HttpsConnector<H> {
    let b = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http();
    if http2 {
        b.enable_http1().enable_http2().wrap_connector(http)
    } else {
        b.enable_http1().wrap_connector(http)
    }
}

#[async_trait]
impl Transport for HyperTransport {
    async fn send(&self, mut req: Request) -> Result<Response, EgressError> {
        let exempt = req.extensions().get::<Rewritten>().is_some();
        // hyper derives Host (HTTP/1) or :authority (HTTP/2) from the URI.
        req.headers_mut().remove(header::HOST);
        let fut = match (&self.guarded, exempt) {
            (Some(guarded), false) => guarded.request(req),
            _ => self.open.request(req),
        };
        let resp = fut.await.map_err(map_client_error)?;
        Ok(resp.map(|b| b.map_err(|e| Box::new(e) as BoxError).boxed_unsync()))
    }
}

/// First error of type `T` in the source chain, also looking inside
/// `io::Error` payloads (whose `source()` skips the payload itself).
fn find_in_chain<'a, T: std::error::Error + 'static>(
    err: &'a (dyn std::error::Error + 'static),
) -> Option<&'a T> {
    let mut cur: Option<&'a (dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(t) = e.downcast_ref::<T>() {
            return Some(t);
        }
        if let Some(inner) = e.downcast_ref::<io::Error>().and_then(|io| io.get_ref()) {
            cur = Some(inner);
            continue;
        }
        cur = e.source();
    }
    None
}

fn map_client_error(err: hyper_util::client::legacy::Error) -> EgressError {
    let e: &(dyn std::error::Error + 'static) = &err;
    if let Some(b) = find_in_chain::<BlockedAddress>(e) {
        return EgressError::denied(b.to_string());
    }
    if let Some(d) = find_in_chain::<DnsFailure>(e) {
        return EgressError::Dns(d.to_string());
    }
    if let Some(t) = find_in_chain::<rustls::Error>(e) {
        return EgressError::Tls(t.to_string());
    }
    if err.is_connect() {
        if find_in_chain::<io::Error>(e).is_some_and(|io| io.kind() == io::ErrorKind::TimedOut) {
            return EgressError::Timeout;
        }
        return EgressError::Connect(error_chain(e));
    }
    if find_in_chain::<hyper::Error>(e).is_some() {
        return EgressError::Protocol(error_chain(e));
    }
    EgressError::Other(Box::new(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private(s: &str) -> bool {
        is_private_ip(s.parse().unwrap())
    }

    #[test]
    fn private_ranges() {
        for ip in [
            "127.0.0.1",
            "127.255.0.1",
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b::a00:1",
            "::127.0.0.1",
            "2002:a9fe:a9fe::1",
            "2002:7f00:1::1",
            "2002:c0a8:101::",
            "64:ff9b:1::808:808",
            "64:ff9b:1:ffff::1",
            "2001::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
        ] {
            assert!(private(ip), "{ip} should be blocked");
        }
    }

    #[test]
    fn public_ranges() {
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "172.32.0.1",
            "100.128.0.1",
            "100.63.255.255",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
            "64:ff9b:2::1",
            "2001:db9::1",
            "2001:4860:4860::8888",
        ] {
            assert!(!private(ip), "{ip} should be allowed");
        }
    }
}
