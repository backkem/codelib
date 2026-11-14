//! Host-side credentials: secrets are resolved and attached outside the
//! sandbox, after the allowlist has matched, so they never enter it.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderMap, HeaderName, HeaderValue, header};

use crate::body::Request;
use crate::error::EgressError;

/// A secret string. `Debug` and `Display` print `[REDACTED]`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value. Do not log it.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// What a [`CredentialProvider`] attaches to a request.
///
/// Every header produced here overwrites any header of the same name the
/// script set, and is marked sensitive.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Credential {
    /// Attach nothing.
    None,
    /// `Authorization: Bearer <token>`.
    Bearer(Secret),
    /// `Authorization: Basic base64(username:password)`.
    Basic {
        /// User name (not secret).
        username: String,
        /// Password or API token.
        password: Secret,
    },
    /// One custom header, e.g. `X-Api-Key`.
    Header {
        /// Header name.
        name: HeaderName,
        /// Header value.
        value: Secret,
    },
    /// Several headers. A name may repeat to send multiple values.
    Headers(Vec<(HeaderName, Secret)>),
}

impl Credential {
    /// Write this credential into `headers`, replacing script-set values.
    pub(crate) fn apply(self, headers: &mut HeaderMap) -> Result<(), EgressError> {
        match self {
            Credential::None => {}
            Credential::Bearer(token) => {
                let v = sensitive(&format!("Bearer {}", token.expose_secret()))?;
                headers.insert(header::AUTHORIZATION, v);
            }
            Credential::Basic { username, password } => {
                let raw = format!("{username}:{}", password.expose_secret());
                let v = sensitive(&format!("Basic {}", base64(raw.as_bytes())))?;
                headers.insert(header::AUTHORIZATION, v);
            }
            Credential::Header { name, value } => {
                let v = sensitive(value.expose_secret())?;
                headers.insert(name, v);
            }
            Credential::Headers(list) => {
                // Validate everything before touching the map.
                let list = list
                    .into_iter()
                    .map(|(n, v)| Ok((n, sensitive(v.expose_secret())?)))
                    .collect::<Result<Vec<_>, EgressError>>()?;
                for (name, _) in &list {
                    headers.remove(name);
                }
                for (name, value) in list {
                    headers.append(name, value);
                }
            }
        }
        Ok(())
    }
}

fn sensitive(value: &str) -> Result<HeaderValue, EgressError> {
    let mut v = HeaderValue::from_str(value)
        .map_err(|_| EgressError::Credential("credential is not a valid header value".into()))?;
    v.set_sensitive(true);
    Ok(v)
}

/// The parts of a request a [`CredentialProvider`] may inspect. The headers
/// are the script's headers (after middlewares), with `Host` already set
/// from the URL, and without the rule's static headers or any credential.
#[derive(Debug, Clone, Copy)]
pub struct RequestInfo<'a> {
    /// Request method.
    pub method: &'a http::Method,
    /// Absolute request URL as the script asked for it (before any rewrite).
    pub uri: &'a http::Uri,
    /// Request headers.
    pub headers: &'a http::HeaderMap,
}

/// Resolves the credential for one request, on the host, after the allowlist
/// has matched. Called once per request and once per followed redirect hop
/// that matches the rule, so it may refresh tokens.
#[async_trait]
pub trait CredentialProvider: Send + Sync + 'static {
    /// The credential to attach. An error fails the request with
    /// [`EgressError::Credential`] (or whatever variant is returned).
    async fn credential(&self, req: &RequestInfo<'_>) -> Result<Credential, EgressError>;
}

#[async_trait]
impl<T: CredentialProvider + ?Sized> CredentialProvider for Arc<T> {
    async fn credential(&self, req: &RequestInfo<'_>) -> Result<Credential, EgressError> {
        (**self).credential(req).await
    }
}

/// For schemes that must see and possibly rewrite the whole request,
/// including the body (e.g. AWS SigV4). Runs after the rule's credential has
/// been attached. A signer that needs the body must buffer it and put a new
/// body back.
///
/// The signer may change anything except where the request goes: if the
/// scheme, host or effective port of the returned request differ from those
/// it was given, the request fails with [`EgressError::InvalidRequest`]. The
/// rule matched and its checks ran for the original destination only.
#[async_trait]
pub trait Signer: Send + Sync + 'static {
    /// Return the signed request.
    async fn sign(&self, req: Request) -> Result<Request, EgressError>;
}

#[async_trait]
impl<T: Signer + ?Sized> Signer for Arc<T> {
    async fn sign(&self, req: Request) -> Result<Request, EgressError> {
        (**self).sign(req).await
    }
}

/// Always returns the same credential.
#[derive(Clone, Debug)]
pub struct StaticCredential(Credential);

impl StaticCredential {
    /// Wrap a fixed credential.
    pub fn new(credential: Credential) -> Self {
        Self(credential)
    }

    /// `Authorization: Bearer <token>`.
    pub fn bearer(token: impl Into<Secret>) -> Self {
        Self(Credential::Bearer(token.into()))
    }

    /// `Authorization: Basic ...`.
    pub fn basic(username: impl Into<String>, password: impl Into<Secret>) -> Self {
        Self(Credential::Basic {
            username: username.into(),
            password: password.into(),
        })
    }

    /// One custom header.
    ///
    /// # Panics
    /// If `name` is not a valid header name.
    pub fn header<N>(name: N, value: impl Into<Secret>) -> Self
    where
        N: TryInto<HeaderName>,
    {
        Self(Credential::Header {
            name: header_name(name, "StaticCredential::header"),
            value: value.into(),
        })
    }
}

#[async_trait]
impl CredentialProvider for StaticCredential {
    async fn credential(&self, _req: &RequestInfo<'_>) -> Result<Credential, EgressError> {
        Ok(self.0.clone())
    }
}

/// `Authorization: Bearer $VAR`, reading the environment variable at request
/// time. A missing or empty variable fails the request with
/// [`EgressError::Credential`].
#[derive(Clone, Debug)]
pub struct EnvBearer(String);

impl EnvBearer {
    /// Read the token from environment variable `var`.
    pub fn new(var: impl Into<String>) -> Self {
        Self(var.into())
    }
}

#[async_trait]
impl CredentialProvider for EnvBearer {
    async fn credential(&self, _req: &RequestInfo<'_>) -> Result<Credential, EgressError> {
        Ok(Credential::Bearer(read_env(&self.0)?))
    }
}

/// `Authorization: Basic base64($USER_VAR:$PASSWORD_VAR)`, read at request time.
#[derive(Clone, Debug)]
pub struct EnvBasic {
    username_var: String,
    password_var: String,
}

impl EnvBasic {
    /// Read the user name and password from two environment variables.
    pub fn new(username_var: impl Into<String>, password_var: impl Into<String>) -> Self {
        Self {
            username_var: username_var.into(),
            password_var: password_var.into(),
        }
    }
}

#[async_trait]
impl CredentialProvider for EnvBasic {
    async fn credential(&self, _req: &RequestInfo<'_>) -> Result<Credential, EgressError> {
        let username = read_env(&self.username_var)?.expose_secret().to_owned();
        let password = read_env(&self.password_var)?;
        Ok(Credential::Basic { username, password })
    }
}

/// `<name>: $VAR`, read at request time (e.g. `X-Api-Key`).
#[derive(Clone, Debug)]
pub struct EnvHeader {
    name: HeaderName,
    var: String,
}

impl EnvHeader {
    /// Send environment variable `var` as header `name`.
    ///
    /// # Panics
    /// If `name` is not a valid header name.
    pub fn new<N>(name: N, var: impl Into<String>) -> Self
    where
        N: TryInto<HeaderName>,
    {
        Self {
            name: header_name(name, "EnvHeader::new"),
            var: var.into(),
        }
    }
}

#[async_trait]
impl CredentialProvider for EnvHeader {
    async fn credential(&self, _req: &RequestInfo<'_>) -> Result<Credential, EgressError> {
        Ok(Credential::Header {
            name: self.name.clone(),
            value: read_env(&self.var)?,
        })
    }
}

fn read_env(var: &str) -> Result<Secret, EgressError> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Ok(Secret(v)),
        Ok(_) => Err(EgressError::Credential(format!(
            "environment variable {var} is empty"
        ))),
        Err(_) => Err(EgressError::Credential(format!(
            "environment variable {var} is not set"
        ))),
    }
}

pub(crate) fn header_name<N: TryInto<HeaderName>>(name: N, what: &str) -> HeaderName {
    match name.try_into() {
        Ok(n) => n,
        Err(_) => panic!("{what}: invalid header name"),
    }
}

/// Standard base64 with padding (RFC 4648), for `Authorization: Basic`.
fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(chunk[0]) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(
            base64(b"Aladdin:open sesame"),
            "QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }

    #[test]
    fn secret_is_redacted() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s}"), "[REDACTED]");
        assert!(!format!("{s:?}").contains("hunter2"));
        let c = Credential::Basic {
            username: "u".into(),
            password: s.clone(),
        };
        assert!(!format!("{c:?}").contains("hunter2"));
        assert_eq!(s.expose_secret(), "hunter2");
    }

    #[test]
    fn apply_marks_sensitive_and_overwrites() {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer script"),
        );
        Credential::Bearer("host".into()).apply(&mut h).unwrap();
        let v = h.get(header::AUTHORIZATION).unwrap();
        assert_eq!(v, "Bearer host");
        assert!(v.is_sensitive());
        assert_eq!(h.get_all(header::AUTHORIZATION).iter().count(), 1);
    }

    #[test]
    fn apply_headers_replaces_all_values() {
        let mut h = HeaderMap::new();
        h.append("x-key", HeaderValue::from_static("a"));
        h.append("x-key", HeaderValue::from_static("b"));
        Credential::Headers(vec![
            (HeaderName::from_static("x-key"), "1".into()),
            (HeaderName::from_static("x-key"), "2".into()),
            (HeaderName::from_static("x-other"), "3".into()),
        ])
        .apply(&mut h)
        .unwrap();
        let vals: Vec<_> = h.get_all("x-key").iter().collect();
        assert_eq!(vals, ["1", "2"]);
        assert_eq!(h.get("x-other").unwrap(), "3");
    }

    #[test]
    fn invalid_value_is_credential_error_without_value() {
        let mut h = HeaderMap::new();
        let err = Credential::Bearer("bad\nvalue".into())
            .apply(&mut h)
            .unwrap_err();
        assert!(matches!(err, EgressError::Credential(_)));
        assert!(!err.to_string().contains("bad"));
        assert!(h.is_empty());
    }
}
