//! Allow rules: which requests may leave the sandbox, and what is attached to them.

use std::fmt;
use std::sync::Arc;

use http::{HeaderMap, HeaderName, HeaderValue, Method};

use crate::credential::{CredentialProvider, Signer, header_name};

/// Headers some servers honour to act as another method.
const METHOD_OVERRIDE_HEADERS: [&str; 3] = [
    "x-http-method-override",
    "x-http-method",
    "x-method-override",
];

/// How the host part of a pattern matches.
#[derive(Clone, Debug, PartialEq, Eq)]
enum HostMatch {
    /// `*`: any host.
    Any,
    /// `api.example.com`.
    Exact(String),
    /// `*.example.com`, stored as `.example.com`.
    Suffix(String),
}

impl HostMatch {
    fn matches(&self, host: &str) -> bool {
        match self {
            HostMatch::Any => true,
            HostMatch::Exact(h) => h == host,
            HostMatch::Suffix(s) => host.len() > s.len() && host.ends_with(s.as_str()),
        }
    }
}

/// One host pattern, optionally scoped to a path prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HostPattern {
    host: HostMatch,
    path: Option<String>,
}

impl HostPattern {
    fn parse(pattern: &str) -> Self {
        let pattern = pattern.trim();
        let (host, path) = match pattern.find('/') {
            Some(i) => (&pattern[..i], Some(normalize_prefix(&pattern[i..]))),
            None => (pattern, None),
        };
        let host = normalize_host(host);
        let host = if host == "*" {
            HostMatch::Any
        } else if let Some(suffix) = host.strip_prefix("*.") {
            HostMatch::Suffix(format!(".{suffix}"))
        } else {
            HostMatch::Exact(host)
        };
        Self { host, path }
    }

    fn matches(&self, host: &str, path: &str) -> bool {
        self.host.matches(host) && self.path.as_deref().is_none_or(|p| path_matches(path, p))
    }
}

/// Lower-case a host and drop a `:port` suffix (ports are ignored for matching).
pub(crate) fn normalize_host(host: &str) -> String {
    let host = host.trim();
    let host = if host.starts_with('[') {
        // [v6]:port -> [v6]
        match host.find(']') {
            Some(i) => &host[..=i],
            None => host,
        }
    } else {
        match host.rsplit_once(':') {
            Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
            _ => host,
        }
    };
    host.to_ascii_lowercase()
}

fn normalize_prefix(prefix: &str) -> String {
    if prefix.starts_with('/') {
        prefix.to_owned()
    } else {
        format!("/{prefix}")
    }
}

/// Methods refused on every rule: `TRACE` (and IIS's `TRACK`) make the
/// server echo the request, injected credentials included, back to the
/// caller; `CONNECT` asks it to open a tunnel. Compared case-insensitively
/// because some servers accept lower-case methods.
const FORBIDDEN_METHODS: [&str; 3] = ["TRACE", "TRACK", "CONNECT"];

/// Is `method` refused regardless of rules?
pub(crate) fn forbidden_method(method: &Method) -> bool {
    FORBIDDEN_METHODS
        .iter()
        .any(|m| method.as_str().eq_ignore_ascii_case(m))
}

/// Is `path` under `prefix` at a segment boundary? A prefix ending in `/`
/// matches every path starting with it; any other prefix matches only the
/// path equal to it or continuing with `/` (so `/api` does not match
/// `/apiv2`). Does not look at dot segments; see [`path_is_safe`].
pub(crate) fn prefix_matches(path: &str, prefix: &str) -> bool {
    if prefix.ends_with('/') {
        path.starts_with(prefix)
    } else {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    }
}

/// Could a server resolve `path` to somewhere other than where it appears to
/// point? `false` for paths containing a backslash or an encoded `/` or `\`
/// (`%2f`, `%5c`; servers such as IIS treat `\` as `/`, and some proxies
/// decode `%2f` before normalising), and for paths with a `.` or `..`
/// segment, also when its dots are percent-encoded (`%2e`) or it carries
/// `;params` (`..;` is `..` to Tomcat and Spring).
pub(crate) fn path_is_safe(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.contains('\\') || lower.contains("%2f") || lower.contains("%5c") {
        return false;
    }
    !lower.split('/').any(|seg| {
        let seg = seg.split(';').next().unwrap_or_default();
        let seg = seg.replace("%2e", ".");
        seg == "." || seg == ".."
    })
}

/// Is `path` safely under `prefix` ([`path_is_safe`] and
/// [`prefix_matches`])?
fn path_matches(path: &str, prefix: &str) -> bool {
    let path = if path.is_empty() { "/" } else { path };
    path_is_safe(path) && prefix_matches(path, prefix)
}

/// One allow rule: which requests it matches and what happens to them.
///
/// ```
/// use codelib_http::{EnvBearer, Rule};
///
/// let rule = Rule::host("api.github.com")
///     .or_host("uploads.github.com")
///     .read_only()
///     .credential(EnvBearer::new("GITHUB_TOKEN"));
/// # let _ = rule;
/// ```
///
/// Host patterns are exact (`api.github.com`), wildcard (`*.atlassian.net`,
/// which matches any subdomain but not `atlassian.net` itself) or `*` (any
/// host). Matching is case-insensitive and ignores ports. A pattern may carry
/// a path prefix: `api.atlassian.com/ex/jira/`. Path prefixes (here and in
/// [`path_prefix`](Rule::path_prefix)) match at a segment boundary: a prefix
/// ending in `/` matches paths below it, any other prefix matches itself and
/// paths below it (`/api` matches `/api` and `/api/x`, not `/apiv2`). When a
/// rule has any path scoping, a path with a backslash, an encoded slash or
/// backslash (`%2f`, `%5c`), or a `.`/`..` segment (also as `%2e`, or with
/// `;params` such as `..;`) never matches, since the server may resolve it
/// outside the prefix.
///
/// `TRACE`, `TRACK` and `CONNECT` are refused on every rule, `*` included.
#[derive(Clone)]
pub struct Rule {
    patterns: Vec<HostPattern>,
    path_prefixes: Vec<String>,
    methods: Option<Vec<Method>>,
    https_only: bool,
    headers: Vec<(HeaderName, HeaderValue)>,
    credential: Option<Arc<dyn CredentialProvider>>,
    signer: Option<Arc<dyn Signer>>,
}

impl fmt::Debug for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rule")
            .field("patterns", &self.patterns)
            .field("path_prefixes", &self.path_prefixes)
            .field("methods", &self.methods)
            .field("https_only", &self.https_only)
            .field(
                "headers",
                &self.headers.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            )
            .field("credential", &self.credential.is_some())
            .field("signer", &self.signer.is_some())
            .finish()
    }
}

impl Rule {
    /// A rule matching `pattern` (see the type docs for the syntax).
    pub fn host(pattern: impl Into<String>) -> Rule {
        Rule {
            patterns: vec![HostPattern::parse(&pattern.into())],
            path_prefixes: Vec::new(),
            methods: None,
            https_only: true,
            headers: Vec::new(),
            credential: None,
            signer: None,
        }
    }

    /// Also match `pattern`.
    pub fn or_host(mut self, pattern: impl Into<String>) -> Rule {
        self.patterns.push(HostPattern::parse(&pattern.into()));
        self
    }

    /// Additionally require the path to lie under `prefix` at a segment
    /// boundary (see the type docs: `/api` matches `/api` and `/api/x`, not
    /// `/apiv2`; paths with dot segments, backslashes or encoded slashes
    /// never match). May be called several times; the path must then lie
    /// under at least one of them. Applies on top of any per-pattern path.
    pub fn path_prefix(mut self, prefix: impl Into<String>) -> Rule {
        self.path_prefixes.push(normalize_prefix(&prefix.into()));
        self
    }

    /// Allow only these methods (default: all). When restricted, requests
    /// carrying a method-override header (`X-HTTP-Method-Override` and
    /// friends) are denied too.
    pub fn methods(mut self, methods: impl IntoIterator<Item = Method>) -> Rule {
        self.methods = Some(methods.into_iter().collect());
        self
    }

    /// Allow only GET and HEAD.
    pub fn read_only(self) -> Rule {
        self.methods([Method::GET, Method::HEAD])
    }

    /// Require https (default `true`). Plain http is denied unless this is
    /// turned off.
    pub fn https_only(mut self, yes: bool) -> Rule {
        self.https_only = yes;
        self
    }

    /// Resolve and attach a credential to every request this rule allows.
    pub fn credential(mut self, provider: impl CredentialProvider) -> Rule {
        self.credential = Some(Arc::new(provider));
        self
    }

    /// Sign every request this rule allows (runs after the credential).
    pub fn signer(mut self, signer: impl Signer) -> Rule {
        self.signer = Some(Arc::new(signer));
        self
    }

    /// Attach a static header, overwriting any script-set value. Applied
    /// before the credential, so a credential header of the same name wins.
    /// The value is marked sensitive (redacted in `Debug`, never indexed by
    /// HTTP/2 header compression) and is not shown to credential providers.
    ///
    /// # Panics
    /// If `name` or `value` is invalid (this is static configuration).
    pub fn header<N, V>(mut self, name: N, value: V) -> Rule
    where
        N: TryInto<HeaderName>,
        V: TryInto<HeaderValue>,
    {
        let name = header_name(name, "Rule::header");
        let Ok(mut value) = value.try_into() else {
            panic!("Rule::header: invalid value for header {name}");
        };
        value.set_sensitive(true);
        self.headers.push((name, value));
        self
    }

    /// Does this rule's host pattern (and path prefix) match? `host` must be
    /// normalized with [`normalize_host`].
    pub(crate) fn matches(&self, host: &str, path: &str) -> bool {
        self.patterns.iter().any(|p| p.matches(host, path))
            && (self.path_prefixes.is_empty()
                || self.path_prefixes.iter().any(|p| path_matches(path, p)))
    }

    /// Method, scheme and method-override checks. `Err` is the denial reason.
    pub(crate) fn check(
        &self,
        method: &Method,
        scheme: &str,
        headers: &HeaderMap,
    ) -> Result<(), String> {
        if forbidden_method(method) {
            return Err(format!("method {method} is never allowed"));
        }
        if self.https_only && scheme != "https" {
            return Err(format!("scheme {scheme} not allowed (https only)"));
        }
        if let Some(allowed) = &self.methods {
            if !allowed.contains(method) {
                return Err(format!("method {method} not allowed"));
            }
            if let Some(h) = METHOD_OVERRIDE_HEADERS
                .iter()
                .find(|h| headers.contains_key(**h))
            {
                return Err(format!("method-override header {h} not allowed"));
            }
        }
        Ok(())
    }

    pub(crate) fn static_headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.headers
    }

    pub(crate) fn credential_provider(&self) -> Option<&Arc<dyn CredentialProvider>> {
        self.credential.as_ref()
    }

    pub(crate) fn signer_ref(&self) -> Option<&Arc<dyn Signer>> {
        self.signer.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(rule: &Rule, host: &str, path: &str) -> bool {
        rule.matches(&normalize_host(host), path)
    }

    #[test]
    fn exact_host() {
        let r = Rule::host("api.github.com");
        assert!(m(&r, "api.github.com", "/repos"));
        assert!(m(&r, "API.GitHub.COM", "/"));
        assert!(!m(&r, "github.com", "/"));
        assert!(!m(&r, "evil.api.github.com", "/"));
        assert!(!m(&r, "api.github.com.evil.example", "/"));
    }

    #[test]
    fn wildcard_host() {
        let r = Rule::host("*.atlassian.net");
        assert!(m(&r, "myorg.atlassian.net", "/"));
        assert!(m(&r, "deep.sub.atlassian.net", "/"));
        assert!(!m(&r, "atlassian.net", "/"));
        assert!(!m(&r, "evilatlassian.net", "/"));
        assert!(m(&r, "MyOrg.Atlassian.NET", "/"));
    }

    #[test]
    fn any_host() {
        let r = Rule::host("*");
        assert!(m(&r, "anything.example", "/"));
        assert!(m(&r, "127.0.0.1", "/"));
    }

    #[test]
    fn ports_ignored() {
        let r = Rule::host("localhost:8080");
        assert!(m(&r, "localhost:9999", "/"));
        assert!(m(&r, "localhost", "/"));
        let r = Rule::host("[::1]:8080");
        assert!(m(&r, "[::1]", "/"));
        assert_eq!(normalize_host("[::1]:443"), "[::1]");
        assert_eq!(normalize_host("Example.COM:443"), "example.com");
    }

    #[test]
    fn path_scoped_pattern() {
        let r = Rule::host("api.atlassian.com/ex/jira/");
        assert!(m(&r, "api.atlassian.com", "/ex/jira/abc/rest/api/3/myself"));
        assert!(!m(&r, "api.atlassian.com", "/ex/confluence/abc"));
        assert!(!m(&r, "other.example", "/ex/jira/x"));
    }

    #[test]
    fn dot_segments_never_match_a_prefix() {
        let r = Rule::host("api.atlassian.com/ex/jira/");
        assert!(!m(&r, "api.atlassian.com", "/ex/jira/../confluence/x"));
        assert!(!m(&r, "api.atlassian.com", "/ex/jira/%2e%2E/confluence/x"));
        assert!(!m(&r, "api.atlassian.com", "/ex/jira/./x"));
        // A dot inside a segment is fine.
        assert!(m(&r, "api.atlassian.com", "/ex/jira/file.json"));
        // Without a prefix there is nothing to escape from.
        assert!(m(
            &Rule::host("api.atlassian.com"),
            "api.atlassian.com",
            "/a/../b"
        ));
    }

    #[test]
    fn prefixes_match_at_segment_boundaries() {
        let r = Rule::host("a.example").path_prefix("/api");
        assert!(m(&r, "a.example", "/api"));
        assert!(m(&r, "a.example", "/api/"));
        assert!(m(&r, "a.example", "/api/x"));
        assert!(!m(&r, "a.example", "/apiv2"));
        assert!(!m(&r, "a.example", "/api-admin/x"));
        assert!(!m(&r, "a.example", "/api;x"));
        assert!(!m(&r, "a.example", "/ap"));
        let r = Rule::host("a.example").path_prefix("/api/");
        assert!(m(&r, "a.example", "/api/x"));
        assert!(!m(&r, "a.example", "/api"));
        // Pattern paths behave the same.
        let r = Rule::host("a.example/api");
        assert!(m(&r, "a.example", "/api/x"));
        assert!(!m(&r, "a.example", "/apiv2"));
        // The root prefix matches everything (safe).
        let r = Rule::host("a.example").path_prefix("/");
        assert!(m(&r, "a.example", "/anything"));
        assert!(m(&r, "a.example", ""));
    }

    #[test]
    fn unsafe_paths_never_match_a_prefix() {
        let r = Rule::host("a.example/ex/jira/");
        for p in [
            "/ex/jira/..;/x",
            "/ex/jira/..;a=b/x",
            "/ex/jira/.;/x",
            "/ex/jira/%2E%2e;x/y",
            "/ex/jira/%2e%2e%2fx",
            "/ex/jira/a%2Fb",
            "/ex/jira/a%5Cb",
            "/ex/jira/a\\b",
            "/ex/jira/.",
            "/ex/jira/..",
        ] {
            assert!(!m(&r, "a.example", p), "{p}");
        }
        // A plain `;` or dots inside a segment are fine.
        for p in [
            "/ex/jira/a;b=c/x",
            "/ex/jira/...",
            "/ex/jira/..x",
            "/ex/jira/x..",
            "/ex/jira/%2e%2e%2e",
        ] {
            assert!(m(&r, "a.example", p), "{p}");
        }
    }

    #[test]
    fn trace_track_connect_never_allowed() {
        let h = HeaderMap::new();
        let r = Rule::host("*").https_only(false);
        for name in ["TRACE", "trace", "TRACK", "CONNECT", "connect"] {
            let method = Method::from_bytes(name.as_bytes()).unwrap();
            assert!(r.check(&method, "https", &h).is_err(), "{name}");
        }
        assert!(r.check(&Method::GET, "https", &h).is_ok());
    }

    #[test]
    fn static_headers_are_sensitive() {
        let r = Rule::host("a.example").header("x-static", "v");
        assert!(r.static_headers()[0].1.is_sensitive());
    }

    #[test]
    fn or_host_and_rule_wide_prefix() {
        let r = Rule::host("a.example")
            .or_host("*.b.example")
            .path_prefix("/v1/")
            .path_prefix("v2/");
        assert!(m(&r, "a.example", "/v1/x"));
        assert!(m(&r, "x.b.example", "/v2/x"));
        assert!(!m(&r, "a.example", "/v3/x"));
        assert!(!m(&r, "b.example", "/v1/x"));
    }

    #[test]
    fn checks() {
        let h = HeaderMap::new();
        let r = Rule::host("a.example");
        assert!(r.check(&Method::DELETE, "https", &h).is_ok());
        assert!(r.check(&Method::GET, "http", &h).is_err());
        assert!(
            r.clone()
                .https_only(false)
                .check(&Method::GET, "http", &h)
                .is_ok()
        );

        let ro = Rule::host("a.example").read_only();
        assert!(ro.check(&Method::GET, "https", &h).is_ok());
        assert!(ro.check(&Method::HEAD, "https", &h).is_ok());
        for m in [
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ] {
            assert!(ro.check(&m, "https", &h).is_err(), "{m}");
        }
        let mut over = HeaderMap::new();
        over.insert("x-http-method-override", HeaderValue::from_static("DELETE"));
        assert!(ro.check(&Method::GET, "https", &over).is_err());
        assert!(r.check(&Method::GET, "https", &over).is_ok());
    }

    #[test]
    fn debug_hides_header_values() {
        let r = Rule::host("a.example").header("x-static", "topsecretvalue");
        assert!(!format!("{r:?}").contains("topsecretvalue"));
    }
}
