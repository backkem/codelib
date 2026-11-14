//! The method registry and dispatcher.

use std::collections::BTreeMap;
use std::fmt;
use std::future::{Future, ready};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::codec::{Codec, Json};
use crate::failure::{Failure, truncate};
use crate::info::{CallRecord, MethodBuilder, MethodInfo};

/// A boxed, `Send` future, as returned by [`Router::method_async_boxed`] handlers.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The encoded result of a call.
pub type CallResult = Result<Vec<u8>, Failure>;

/// Name of the built-in discovery method.
pub const METHODS: &str = "methods";

type SyncFn<S> = dyn Fn(&mut S, &MethodInfo, &[u8]) -> CallResult + Send + Sync;
type AsyncFn<S> =
    dyn for<'a> Fn(&'a mut S, &'a MethodInfo, &'a [u8]) -> BoxFuture<'a, CallResult> + Send + Sync;
type GuardFn<S> = dyn Fn(&MethodInfo, &S) -> Result<(), Failure> + Send + Sync;
type ObserveFn = dyn Fn(&CallRecord<'_>) + Send + Sync;
type ProjectFn<S, T> = dyn for<'a> Fn(&'a mut S) -> &'a mut T + Send + Sync;

/// An async handler: `async fn handler(state: &mut S, args: A) -> Result<R, Failure>` whose
/// future is `Send`.
///
/// Implemented for every `F: Fn(&'a mut S, A) -> Fut` with `Fut: Future + Send + 'a`, for all
/// `'a`. Plain `async fn` items satisfy that; ordinary closures returning `async move {}` blocks
/// usually do not (their return type cannot depend on the borrow of the state), so use
/// [`Router::method_async_boxed`] for those.
pub trait AsyncHandler<'a, S: 'a, A, R>: Send + Sync + 'static {
    /// The handler's future.
    type Future: Future<Output = Result<R, Failure>> + Send + 'a;

    /// Starts the call.
    fn call(&self, state: &'a mut S, args: A) -> Self::Future;
}

impl<'a, S: 'a, A, R, F, Fut> AsyncHandler<'a, S, A, R> for F
where
    F: Fn(&'a mut S, A) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<R, Failure>> + Send + 'a,
{
    type Future = Fut;

    fn call(&self, state: &'a mut S, args: A) -> Fut {
        self(state, args)
    }
}

enum Handler<S> {
    Sync(Box<SyncFn<S>>),
    Async(Box<AsyncFn<S>>),
}

struct Entry<S> {
    info: MethodInfo,
    handler: Handler<S>,
}

/// The guards and observers of a router that was mounted into another one.
struct Layer<T> {
    guards: Vec<Box<GuardFn<T>>>,
    observers: Vec<Box<ObserveFn>>,
}

impl<T> Layer<T> {
    fn is_empty(&self) -> bool {
        self.guards.is_empty() && self.observers.is_empty()
    }

    fn check(&self, info: &MethodInfo, state: &T) -> Result<(), Failure> {
        self.guards.iter().try_for_each(|g| g(info, state))
    }

    fn notify(&self, info: &MethodInfo, args: &[u8], result: &CallResult) {
        if self.observers.is_empty() {
            return;
        }
        let record = CallRecord {
            method: info,
            args,
            result,
        };
        for o in &self.observers {
            o(&record);
        }
    }
}

/// A registry of typed methods with a byte-level dispatcher.
///
/// `S` is the state handlers get `&mut` access to; `C` is the wire [`Codec`] (JSON by
/// default). A router is `Send + Sync` whatever `S` is, so it can be built once at startup,
/// put in an `Arc` and shared by concurrent runs, each passing its own state.
///
/// See the [crate docs](crate) for the dispatch rules.
pub struct Router<S, C: Codec = Json> {
    entries: Vec<Entry<S>>,
    index: BTreeMap<String, usize>,
    layer: Layer<S>,
    builtin: MethodInfo,
    _codec: PhantomData<fn() -> C>,
}

impl<S: 'static, C: Codec> Default for Router<S, C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S, C: Codec> fmt::Debug for Router<S, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Router")
            .field("codec", &C::NAME)
            .field(
                "methods",
                &self
                    .entries
                    .iter()
                    .map(|e| &e.info.name)
                    .collect::<Vec<_>>(),
            )
            .field("guards", &self.layer.guards.len())
            .field("observers", &self.layer.observers.len())
            .finish()
    }
}

fn builtin_info() -> MethodInfo {
    MethodInfo {
        name: METHODS.to_owned(),
        summary: "Every method with its summary, tags, arguments and result.".to_owned(),
        tags: Vec::new(),
        args: Some("{}".to_owned()),
        returns: Some(
            "[{name, summary, tags, args?, returns?, args_schema?, returns_schema?, is_async}]"
                .to_owned(),
        ),
        args_schema: None,
        returns_schema: None,
        is_async: false,
    }
}

impl<S: 'static, C: Codec> Router<S, C> {
    /// An empty router (it still answers the built-in `methods`).
    pub fn new() -> Self {
        Router {
            entries: Vec::new(),
            index: BTreeMap::new(),
            layer: Layer {
                guards: Vec::new(),
                observers: Vec::new(),
            },
            builtin: builtin_info(),
            _codec: PhantomData,
        }
    }

    /// Registers a synchronous handler `Fn(&mut S, A) -> Result<R, Failure>`.
    ///
    /// # Panics
    ///
    /// If `name` is empty or already registered.
    pub fn method<A, R, F>(&mut self, name: &str, f: F) -> MethodBuilder<'_, A, R>
    where
        A: DeserializeOwned + 'static,
        R: Serialize + 'static,
        F: Fn(&mut S, A) -> Result<R, Failure> + Send + Sync + 'static,
    {
        let handler: Box<SyncFn<S>> = Box::new(move |state, info, bytes| {
            let args = decode_args::<C, A>(info, bytes)?;
            let result = f(state, args)?;
            encode_result::<C, R>(info, &result)
        });
        self.register(MethodInfo::new(name), Handler::Sync(handler))
    }

    /// Registers an async handler: a plain
    /// `async fn handler(state: &mut S, args: A) -> Result<R, Failure>` whose future is `Send`.
    ///
    /// For closures, use [`method_async_boxed`](Self::method_async_boxed).
    ///
    /// # Panics
    ///
    /// If `name` is empty or already registered.
    pub fn method_async<A, R, F>(&mut self, name: &str, f: F) -> MethodBuilder<'_, A, R>
    where
        A: DeserializeOwned + 'static,
        R: Serialize + 'static,
        F: for<'a> AsyncHandler<'a, S, A, R>,
    {
        let handler: Box<AsyncFn<S>> =
            Box::new(move |state, info, bytes| -> BoxFuture<'_, CallResult> {
                let args = match decode_args::<C, A>(info, bytes) {
                    Ok(args) => args,
                    Err(e) => return Box::pin(ready(Err(e))),
                };
                let fut = f.call(state, args);
                Box::pin(async move {
                    let result = fut.await?;
                    encode_result::<C, R>(info, &result)
                })
            });
        let mut info = MethodInfo::new(name);
        info.is_async = true;
        self.register(info, Handler::Async(handler))
    }

    /// Registers an async handler returning a boxed future; works with closures:
    ///
    /// ```
    /// # use codelib_rpc::{Router, Failure};
    /// # struct Db { rows: Vec<String> }
    /// let mut router = Router::<Db>::new();
    /// router.method_async_boxed("rows.add", |db: &mut Db, row: String| {
    ///     Box::pin(async move {
    ///         db.rows.push(row);
    ///         Ok(db.rows.len())
    ///     })
    /// });
    /// ```
    ///
    /// # Panics
    ///
    /// If `name` is empty or already registered.
    pub fn method_async_boxed<A, R, F>(&mut self, name: &str, f: F) -> MethodBuilder<'_, A, R>
    where
        A: DeserializeOwned + 'static,
        R: Serialize + 'static,
        F: for<'a> Fn(&'a mut S, A) -> BoxFuture<'a, Result<R, Failure>> + Send + Sync + 'static,
    {
        let handler: Box<AsyncFn<S>> =
            Box::new(move |state, info, bytes| -> BoxFuture<'_, CallResult> {
                let args = match decode_args::<C, A>(info, bytes) {
                    Ok(args) => args,
                    Err(e) => return Box::pin(ready(Err(e))),
                };
                let fut = f(state, args);
                Box::pin(async move {
                    let result = fut.await?;
                    encode_result::<C, R>(info, &result)
                })
            });
        let mut info = MethodInfo::new(name);
        info.is_async = true;
        self.register(info, Handler::Async(handler))
    }

    /// Mounts `router` under `prefix.`, giving its handlers the part of the state `project`
    /// returns: `nest("files", files, |s: &mut Server| &mut s.files)` makes its `read` callable
    /// as `files.read`.
    ///
    /// The nested router keeps its metadata (names get the prefix) and its guards and
    /// observers, which keep applying to its own methods only: on a call, this router's guards
    /// run first, then the nested router's (on the projected state), then the handler, then the
    /// nested observers, then this router's observers. All of them see the full, prefixed
    /// [`MethodInfo`]. The nested router's built-in `methods` is not mounted (the top-level one
    /// lists everything). An empty `prefix` mounts without a prefix; a trailing `.` is ignored.
    ///
    /// # Panics
    ///
    /// If a resulting name is already registered.
    pub fn nest<T: 'static>(
        &mut self,
        prefix: &str,
        router: Router<T, C>,
        project: impl Fn(&mut S) -> &mut T + Send + Sync + 'static,
    ) -> &mut Self {
        let project: Arc<ProjectFn<S, T>> = Arc::new(project);
        let prefix = prefix.strip_suffix('.').unwrap_or(prefix);
        let Router { entries, layer, .. } = router;
        let layer = Arc::new(layer);
        for entry in entries {
            let info = MethodInfo {
                name: prefixed(prefix, &entry.info.name),
                ..entry.info
            };
            let handler = wrap(entry.handler, project.clone(), layer.clone());
            self.insert(info, handler);
        }
        self
    }

    /// Adds every method of `other` (same state type, no prefix). `other`'s guards and
    /// observers keep applying to its own methods only, as with [`nest`](Self::nest).
    ///
    /// # Panics
    ///
    /// If a name is registered in both.
    pub fn merge(&mut self, other: Router<S, C>) -> &mut Self {
        if other.layer.is_empty() {
            for entry in other.entries {
                self.insert(entry.info, entry.handler);
            }
            self
        } else {
            self.nest("", other, |s: &mut S| s)
        }
    }

    /// Adds a guard. Guards run in registration order before every call (after the method is
    /// resolved, before arguments are decoded); the first `Err` denies the call with that
    /// failure (use [`Failure::denied`]). The built-in `methods` is guarded too.
    pub fn guard(
        &mut self,
        f: impl Fn(&MethodInfo, &S) -> Result<(), Failure> + Send + Sync + 'static,
    ) -> &mut Self {
        self.layer.guards.push(Box::new(f));
        self
    }

    /// Adds an observer. Observers run in registration order after every call to a resolved
    /// method, including denied calls and argument errors (not unknown methods, and not
    /// [`call_sync`](Self::call_sync) on an async method).
    pub fn observe(&mut self, f: impl Fn(&CallRecord<'_>) + Send + Sync + 'static) -> &mut Self {
        self.layer.observers.push(Box::new(f));
        self
    }

    /// Calls `method` with encoded `args`. The future is `Send` when `S` is.
    pub async fn call(&self, state: &mut S, method: &str, args: &[u8]) -> CallResult {
        let (info, handler) = self.resolve(method)?;
        let result = match self.layer.check(info, state) {
            Err(f) => Err(f),
            Ok(()) => match handler {
                None => self.builtin_methods(),
                Some(Handler::Sync(h)) => h(state, info, args),
                Some(Handler::Async(h)) => h(state, info, args).await,
            },
        };
        self.layer.notify(info, args, &result);
        result
    }

    /// Calls a synchronous method. An async method fails with
    /// [`codes::ASYNC_ONLY`](crate::codes::ASYNC_ONLY) without running guards or observers.
    pub fn call_sync(&self, state: &mut S, method: &str, args: &[u8]) -> CallResult {
        let (info, handler) = self.resolve(method)?;
        if let Some(Handler::Async(_)) = handler {
            return Err(Failure::async_only(&info.name));
        }
        let result = match self.layer.check(info, state) {
            Err(f) => Err(f),
            Ok(()) => match handler {
                None => self.builtin_methods(),
                Some(Handler::Sync(h)) => h(state, info, args),
                Some(Handler::Async(_)) => unreachable!(),
            },
        };
        self.layer.notify(info, args, &result);
        result
    }

    /// Runs `calls` one after another, in request order; one result per call. A failing call
    /// does not stop the batch.
    pub async fn batch(&self, state: &mut S, calls: &[(String, Vec<u8>)]) -> Vec<CallResult> {
        let mut out = Vec::with_capacity(calls.len());
        for (method, args) in calls {
            out.push(self.call(state, method, args).await);
        }
        out
    }

    /// Every method in registration order, followed by the built-in `methods` unless a
    /// registered method overrides it.
    pub fn methods(&self) -> impl Iterator<Item = &MethodInfo> {
        let builtin = (!self.index.contains_key(METHODS)).then_some(&self.builtin);
        self.entries.iter().map(|e| &e.info).chain(builtin)
    }

    /// The metadata of one method (including the built-in `methods`).
    pub fn method_info(&self, name: &str) -> Option<&MethodInfo> {
        match self.index.get(name) {
            Some(&i) => Some(&self.entries[i].info),
            None if name == METHODS => Some(&self.builtin),
            None => None,
        }
    }

    /// Whether `name` resolves to a method.
    pub fn contains(&self, name: &str) -> bool {
        self.method_info(name).is_some()
    }

    fn register<A, R>(&mut self, info: MethodInfo, handler: Handler<S>) -> MethodBuilder<'_, A, R> {
        let i = self.insert(info, handler);
        MethodBuilder::new(&mut self.entries[i].info)
    }

    fn insert(&mut self, info: MethodInfo, handler: Handler<S>) -> usize {
        assert!(
            !info.name.is_empty(),
            "codelib-rpc: method name must not be empty"
        );
        assert!(
            !self.index.contains_key(&info.name),
            "codelib-rpc: method {:?} is registered twice",
            info.name
        );
        let i = self.entries.len();
        self.index.insert(info.name.clone(), i);
        self.entries.push(Entry { info, handler });
        i
    }

    fn resolve(&self, method: &str) -> Result<(&MethodInfo, Option<&Handler<S>>), Failure> {
        match self.index.get(method) {
            Some(&i) => {
                let e = &self.entries[i];
                Ok((&e.info, Some(&e.handler)))
            }
            None if method == METHODS => Ok((&self.builtin, None)),
            None => Err(self.unknown(method)),
        }
    }

    fn builtin_methods(&self) -> CallResult {
        let list: Vec<&MethodInfo> = self.methods().collect();
        encode_result::<C, _>(&self.builtin, &list)
    }

    fn unknown(&self, method: &str) -> Failure {
        // Hints cost an edit distance per registered name; a long name is not a typo.
        let near = if method.len() > MAX_SUGGEST_LEN {
            Vec::new()
        } else {
            suggest(method, self.methods().map(|m| m.name.as_str()))
        };
        let hint = if near.is_empty() {
            format!("`{METHODS}` lists every method")
        } else {
            let list: Vec<String> = near.iter().map(|n| format!("`{n}`")).collect();
            format!(
                "did you mean {}? `{METHODS}` lists every method",
                list.join(", ")
            )
        };
        Failure::unknown_method(method).with_hint(hint)
    }
}

impl<S: 'static> Router<S, Json> {
    /// [`call`](Router::call) with JSON text, for the sandbox's `host.call(method, args)`
    /// import.
    pub async fn call_json(
        &self,
        state: &mut S,
        method: &str,
        args: &str,
    ) -> Result<String, Failure> {
        let out = self.call(state, method, args.as_bytes()).await?;
        into_string(out)
    }

    /// [`call_sync`](Router::call_sync) with JSON text.
    pub fn call_json_sync(
        &self,
        state: &mut S,
        method: &str,
        args: &str,
    ) -> Result<String, Failure> {
        let out = self.call_sync(state, method, args.as_bytes())?;
        into_string(out)
    }
}

fn into_string(bytes: Vec<u8>) -> Result<String, Failure> {
    String::from_utf8(bytes).map_err(|e| Failure::internal(format!("result is not UTF-8: {e}")))
}

fn prefixed(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

/// Lifts a handler on `T` to one on `S` through `project`, running `layer` around it.
fn wrap<S: 'static, T: 'static>(
    handler: Handler<T>,
    project: Arc<ProjectFn<S, T>>,
    layer: Arc<Layer<T>>,
) -> Handler<S> {
    let layered = !layer.is_empty();
    match handler {
        Handler::Sync(h) => Handler::Sync(Box::new(move |state, info, args| {
            let t = project(state);
            if !layered {
                return h(t, info, args);
            }
            let result = match layer.check(info, t) {
                Err(f) => Err(f),
                Ok(()) => h(t, info, args),
            };
            layer.notify(info, args, &result);
            result
        })),
        Handler::Async(h) => Handler::Async(Box::new(
            move |state, info, args| -> BoxFuture<'_, CallResult> {
                let t = project(state);
                if !layered {
                    return h(t, info, args);
                }
                if let Err(f) = layer.check(info, t) {
                    let result = Err(f);
                    layer.notify(info, args, &result);
                    return Box::pin(ready(result));
                }
                let fut = h(t, info, args);
                let layer = layer.clone();
                Box::pin(async move {
                    let result = fut.await;
                    layer.notify(info, args, &result);
                    result
                })
            },
        )),
    }
}

/// Decodes arguments, applying the empty-args rule (see the crate docs).
fn decode_args<C: Codec, A: DeserializeOwned>(
    info: &MethodInfo,
    bytes: &[u8],
) -> Result<A, Failure> {
    let first = match C::decode::<A>(bytes) {
        Ok(args) => return Ok(args),
        Err(e) => e,
    };
    let error = if C::is_empty_value(bytes) {
        if let Ok(args) = serde_json::from_value::<A>(serde_json::Value::Null) {
            return Ok(args);
        }
        match serde_json::from_value::<A>(serde_json::Value::Object(Default::default())) {
            Ok(args) => return Ok(args),
            // "missing field `x`" says more than "EOF while parsing".
            Err(e) => e.to_string(),
        }
    } else {
        first
    };
    // serde echoes input (unknown variants, field names), so its text can be attacker-sized.
    let shown = truncate(&error, MAX_ERROR_LEN);
    let ellipsis = if shown.len() < error.len() { "…" } else { "" };
    let mut failure = Failure::bad_args(format!("{}: {shown}{ellipsis}", info.name));
    if let Some(args) = &info.args {
        failure = failure.with_hint(format!("expected {args}"));
    }
    Err(failure)
}

fn encode_result<C: Codec, R: Serialize + ?Sized>(info: &MethodInfo, result: &R) -> CallResult {
    C::encode(result)
        .map_err(|e| Failure::internal(format!("{}: cannot encode the result: {e}", info.name)))
}

/// Longest unknown method name (in bytes) that gets "did you mean" hints.
const MAX_SUGGEST_LEN: usize = 64;

/// Longest argument-decoding error text (in bytes) kept in a bad-args message.
const MAX_ERROR_LEN: usize = 512;

/// Up to three registered names close to `wanted`.
fn suggest<'a>(wanted: &str, names: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let lower = wanted.to_lowercase();
    let last = |s: &str| s.rsplit('.').next().unwrap_or(s).to_lowercase();
    let wanted_last = last(wanted);
    let wanted_len = lower.chars().count();
    let limit = (wanted.chars().count() / 3).max(1);
    let mut scored: Vec<(usize, &str)> = names
        .filter_map(|name| {
            let name_lower = name.to_lowercase();
            let score = if name_lower == lower {
                0
            } else if last(name) == wanted_last {
                1
            } else {
                // The distance is at least the length difference: skip the quadratic work.
                if name_lower.chars().count().abs_diff(wanted_len) > limit {
                    return None;
                }
                let d = levenshtein(&lower, &name_lower);
                if d > limit {
                    return None;
                }
                1 + d
            };
            Some((score, name))
        })
        .collect();
    scored.sort_by_key(|&(score, _)| score);
    scored.into_iter().take(3).map(|(_, n)| n).collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_distances() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("addline", "addlines"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn suggestions() {
        let names = [
            "addLine",
            "addCircle",
            "files.read",
            "files.write",
            "methods",
        ];
        assert_eq!(suggest("addline", names.iter().copied()), vec!["addLine"]);
        assert_eq!(suggest("addLin", names.iter().copied()), vec!["addLine"]);
        assert_eq!(suggest("read", names.iter().copied()), vec!["files.read"]);
        assert!(suggest("zzzzzz", names.iter().copied()).is_empty());
    }
}
