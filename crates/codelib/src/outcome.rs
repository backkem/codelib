use std::time::Duration;

use codelib_sandbox::{Console, Stats};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::bindings::types;
use crate::host::HostPanic;

/// Everything a run produced. The console, the state and the counters are
/// there whether the run succeeded or not.
#[derive(Debug)]
pub struct Outcome<S = ()> {
    /// The program's return value as JSON (`null` if it returned nothing),
    /// or why there is none.
    pub result: Result<Value, RunError>,
    /// What the program wrote with `console.log` / `console.error`.
    pub console: Console,
    /// The state that was handed to the run, as the host calls left it.
    pub state: S,
    pub stats: Stats,
}

impl<S> Outcome<S> {
    /// The return value, deserialized.
    pub fn value<T: DeserializeOwned>(&self) -> Result<T, RunError> {
        match &self.result {
            Ok(value) => serde_json::from_value(value.clone())
                .map_err(|e| RunError::Sandbox(format!("unexpected return value: {e}"))),
            Err(error) => Err(error.clone()),
        }
    }

    /// Whether the program ran to completion.
    pub fn is_ok(&self) -> bool {
        self.result.is_ok()
    }
}

/// Why a run produced no value.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunError {
    /// The program threw.
    #[error("{0}")]
    Script(ScriptError),
    /// The program ran past its deadline.
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    /// The program failed because it was refused memory past its limit. A
    /// program that recovers from a refused allocation and returns a value
    /// is a success; [`Stats::memory_limit_hit`](crate::Stats::memory_limit_hit)
    /// records it.
    #[error("out of memory: exceeded the {0} byte limit")]
    OutOfMemory(usize),
    /// The program used up its fuel budget.
    #[error("out of fuel")]
    OutOfFuel,
    /// The sandbox itself failed: the guest trapped, or could not be started.
    #[error("sandbox failure: {0}")]
    Sandbox(String),
}

/// An uncaught JavaScript exception.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptError {
    /// The error's `name`, e.g. `TypeError` or `HostError`.
    pub name: String,
    pub message: String,
    pub stack: Option<String>,
}

/// Where a script error was thrown: the top frame of its stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// The name of the source, as set with `Script::name` (or a prelude's).
    pub source: String,
    /// 1-based line in that source.
    pub line: u32,
    /// 1-based column.
    pub column: u32,
}

impl ScriptError {
    /// The position of the first stack frame, if the stack has one in the
    /// engine's `function@source:line:column` format.
    pub fn location(&self) -> Option<Location> {
        self.frames().next()
    }

    /// The position of every stack frame, innermost first. Frames that are
    /// not in the engine's `function@source:line:column` format are skipped.
    ///
    /// An error raised inside a prelude (a client library, say) has its
    /// first frame there; to point at the program's own line, take the first
    /// frame whose source is not one of the preludes:
    ///
    /// ```
    /// # fn demo(error: &codelib::ScriptError) -> Option<codelib::Location> {
    /// error.frames().find(|frame| frame.source != "client.js")
    /// # }
    /// ```
    pub fn frames(&self) -> impl Iterator<Item = Location> + '_ {
        self.stack
            .as_deref()
            .unwrap_or_default()
            .lines()
            .filter_map(parse_frame)
    }
}

/// Parsed from the right: the function name may be empty and the source
/// name may itself contain `:` and `@`.
fn parse_frame(frame: &str) -> Option<Location> {
    let (rest, column) = frame.trim().rsplit_once(':')?;
    let (rest, line) = rest.rsplit_once(':')?;
    let (_function, source) = rest.split_once('@')?;
    Some(Location {
        source: source.to_string(),
        line: line.parse().ok()?,
        column: column.parse().ok()?,
    })
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.name, self.message)
    }
}

impl std::error::Error for ScriptError {}

impl From<types::ScriptError> for ScriptError {
    fn from(error: types::ScriptError) -> Self {
        Self {
            name: error.name,
            message: error.message,
            stack: error.stack,
        }
    }
}

impl From<codelib_sandbox::Error> for RunError {
    fn from(error: codelib_sandbox::Error) -> Self {
        use codelib_sandbox::Error;
        match error {
            Error::Timeout(timeout) => RunError::Timeout(timeout),
            Error::OutOfMemory(limit) => RunError::OutOfMemory(limit),
            Error::OutOfFuel => RunError::OutOfFuel,
            Error::Trap(trap) => {
                if let Some(panic) = trap.downcast_ref::<HostPanic>() {
                    RunError::Sandbox(panic.to_string())
                } else if event_loop_ran_dry(&trap) {
                    RunError::Sandbox(
                        "the program is waiting for something that can never happen (a promise \
                         with no pending timer, request or host call to settle it)"
                            .into(),
                    )
                } else {
                    RunError::Sandbox(Error::Trap(trap).to_string())
                }
            }
            Error::Config(message) => RunError::Sandbox(message),
            other => RunError::Sandbox(other.to_string()),
        }
    }
}

/// Whether a trap is the engine giving up because its event loop is empty
/// while the program still awaits a promise nothing can settle. The engine
/// then exits the process instead of returning, so the trap's backtrace
/// passes through `proc_exit`.
///
/// This is a heuristic: it matches the function name in wasmtime's rendering
/// of the trap, which depends on the guest keeping its name section and on
/// wasmtime's backtrace format. If either changes, such runs are reported as
/// a plain trap instead.
fn event_loop_ran_dry(trap: &codelib_sandbox::wasmtime::Error) -> bool {
    format!("{trap:?}").contains("!proc_exit")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(frame: &str) -> Option<Location> {
        parse_frame(frame)
    }

    #[test]
    fn parses_named_and_anonymous_frames() {
        let expected = Location {
            source: "script.js".into(),
            line: 3,
            column: 9,
        };
        assert_eq!(loc("inner@script.js:3:9"), Some(expected.clone()));
        assert_eq!(loc("@script.js:3:9"), Some(expected));
    }

    #[test]
    fn source_names_may_contain_colons_and_at_signs() {
        let parsed = loc("f@https://example.com/a@b.js:10:2").unwrap();
        assert_eq!(parsed.source, "https://example.com/a@b.js");
        assert_eq!((parsed.line, parsed.column), (10, 2));
        assert_eq!(loc("@@x:1:1").unwrap().source, "@x");
    }

    #[test]
    fn rejects_anything_else() {
        for frame in [
            "",
            "no frame here",
            "f@script.js",
            "f@script.js:1",
            "f@script.js:a:b",
            "script.js:1:2",
        ] {
            assert_eq!(loc(frame), None, "{frame:?}");
        }
    }

    #[test]
    fn location_uses_the_first_frame_only() {
        let error = |stack: Option<&str>| ScriptError {
            name: "Error".into(),
            message: String::new(),
            stack: stack.map(str::to_string),
        };
        let first = error(Some("a@one.js:1:7\nb@two.js:9:9\n"))
            .location()
            .unwrap();
        assert_eq!(first.source, "one.js");
        assert_eq!(error(None).location(), None);
    }

    fn error_with_stack(stack: Option<&str>) -> ScriptError {
        ScriptError {
            name: "Error".into(),
            message: String::new(),
            stack: stack.map(str::to_string),
        }
    }

    #[test]
    fn frames_yields_every_frame_in_order() {
        let error = error_with_stack(Some("a@one.js:1:7\n@two.js:9:3\nc@three.js:4:5\n"));
        let frames: Vec<_> = error
            .frames()
            .map(|frame| (frame.source, frame.line, frame.column))
            .collect();
        assert_eq!(
            frames,
            [
                ("one.js".to_string(), 1, 7),
                ("two.js".to_string(), 9, 3),
                ("three.js".to_string(), 4, 5),
            ]
        );
        assert_eq!(error_with_stack(None).frames().count(), 0);
    }

    #[test]
    fn frames_skips_lines_that_are_not_frames() {
        let error = error_with_stack(Some(
            "a@one.js:1:7\nsomething else\n\n    at native\nb@two.js:2:2",
        ));
        let sources: Vec<_> = error.frames().map(|frame| frame.source).collect();
        assert_eq!(sources, ["one.js", "two.js"]);
    }
}
