//! Method metadata: [`MethodInfo`], [`MethodBuilder`] and [`CallRecord`].

use std::marker::PhantomData;

use serde::{Deserialize, Serialize};

use crate::Failure;

/// Self-description of one method.
///
/// Returned by [`Router::methods`](crate::Router::methods) and by the built-in `methods`
/// method, and consumed by the [`codegen`](crate::codegen) functions. It also deserializes, so a
/// client can fetch the list over a transport and generate bindings from it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MethodInfo {
    /// Full name, including the prefixes of nested routers (`files.read`).
    pub name: String,
    /// One-line description.
    #[serde(default)]
    pub summary: String,
    /// Free-form tags (`read`, `write`, `host-write`, ...), typically checked by guards.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Human-readable argument shape (`{start, end, layer?}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    /// Human-readable result shape (`{id}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns: Option<String>,
    /// JSON Schema of the arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_schema: Option<serde_json::Value>,
    /// JSON Schema of the result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns_schema: Option<serde_json::Value>,
    /// Registered with an async handler (cannot go through
    /// [`Router::call_sync`](crate::Router::call_sync)).
    #[serde(default)]
    pub is_async: bool,
}

impl MethodInfo {
    /// A method description with just a name.
    pub fn new(name: impl Into<String>) -> Self {
        MethodInfo {
            name: name.into(),
            ..MethodInfo::default()
        }
    }

    /// Whether the method carries `tag`.
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }
}

/// What an observer sees after a call.
#[derive(Clone, Copy, Debug)]
pub struct CallRecord<'a> {
    /// The method, as registered on the router that was called (full, prefixed name).
    pub method: &'a MethodInfo,
    /// The raw, encoded arguments.
    pub args: &'a [u8],
    /// The encoded result or the failure.
    pub result: &'a Result<Vec<u8>, Failure>,
}

/// Chainable metadata setters, returned by [`Router::method`](crate::Router::method) and
/// friends.
///
/// `A` and `R` are the handler's argument and result types; they only matter for
/// `schema()` (feature `schema`), which derives JSON Schemas from them.
pub struct MethodBuilder<'a, A = (), R = ()> {
    info: &'a mut MethodInfo,
    _types: PhantomData<fn() -> (A, R)>,
}

impl<'a, A, R> MethodBuilder<'a, A, R> {
    pub(crate) fn new(info: &'a mut MethodInfo) -> Self {
        MethodBuilder {
            info,
            _types: PhantomData,
        }
    }

    /// One-line description.
    pub fn summary(self, summary: impl Into<String>) -> Self {
        self.info.summary = summary.into();
        self
    }

    /// Adds a tag (duplicates are ignored).
    pub fn tag(self, tag: impl Into<String>) -> Self {
        let tag = tag.into();
        if !self.info.has_tag(&tag) {
            self.info.tags.push(tag);
        }
        self
    }

    /// Adds several tags.
    pub fn tags<I, T>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        for t in tags {
            self = self.tag(t);
        }
        self
    }

    /// Human-readable argument shape, e.g. `{start, end, layer?}`.
    pub fn args(self, args: impl Into<String>) -> Self {
        self.info.args = Some(args.into());
        self
    }

    /// Human-readable result shape, e.g. `{id}`.
    pub fn returns(self, returns: impl Into<String>) -> Self {
        self.info.returns = Some(returns.into());
        self
    }

    /// Sets the argument JSON Schema by hand.
    pub fn args_schema_value(self, schema: serde_json::Value) -> Self {
        self.info.args_schema = Some(schema);
        self
    }

    /// Sets the result JSON Schema by hand.
    pub fn returns_schema_value(self, schema: serde_json::Value) -> Self {
        self.info.returns_schema = Some(schema);
        self
    }

    /// The metadata so far.
    pub fn info(&self) -> &MethodInfo {
        self.info
    }
}

#[cfg(feature = "schema")]
impl<'a, A, R> MethodBuilder<'a, A, R> {
    /// Derives both JSON Schemas from the handler's argument and result types.
    pub fn schema(self) -> Self
    where
        A: schemars::JsonSchema,
        R: schemars::JsonSchema,
    {
        self.args_schema().returns_schema()
    }

    /// Derives the argument JSON Schema from the handler's argument type.
    pub fn args_schema(self) -> Self
    where
        A: schemars::JsonSchema,
    {
        self.args_schema_value(schema_value::<A>())
    }

    /// Derives the result JSON Schema from the handler's result type.
    pub fn returns_schema(self) -> Self
    where
        R: schemars::JsonSchema,
    {
        self.returns_schema_value(schema_value::<R>())
    }

    /// Derives both JSON Schemas from explicitly named types (when the handler's own types
    /// are not the ones to document).
    pub fn schema_of<A2, R2>(self) -> Self
    where
        A2: schemars::JsonSchema,
        R2: schemars::JsonSchema,
    {
        self.args_schema_value(schema_value::<A2>())
            .returns_schema_value(schema_value::<R2>())
    }
}

/// The JSON Schema of `T` as a value, without the `$schema` meta key.
#[cfg(feature = "schema")]
pub fn schema_value<T: schemars::JsonSchema>() -> serde_json::Value {
    let mut value = schemars::schema_for!(T).to_value();
    if let serde_json::Value::Object(map) = &mut value {
        map.remove("$schema");
    }
    value
}
