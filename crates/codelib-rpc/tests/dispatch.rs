//! Dispatch behaviour: typed handlers, failures, the empty-args rule, guards, observers,
//! nesting, batches and the built-in `methods`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use codelib_rpc::{BoxFuture, CallRecord, Failure, Json, MethodInfo, Router, codes};
use pollster::block_on;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Default)]
struct Doc {
    lines: Vec<Line>,
    read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Line {
    start: [f64; 2],
    end: [f64; 2],
    #[serde(default)]
    layer: Option<String>,
}

#[derive(Serialize)]
struct Id {
    id: usize,
}

fn add_line(doc: &mut Doc, line: Line) -> Result<Id, Failure> {
    doc.lines.push(line);
    Ok(Id {
        id: doc.lines.len() - 1,
    })
}

async fn count(doc: &mut Doc, _: ()) -> Result<usize, Failure> {
    Ok(doc.lines.len())
}

async fn take_line(doc: &mut Doc, id: usize) -> Result<Line, Failure> {
    if id >= doc.lines.len() {
        return Err(Failure::new("not-found", format!("no line {id}")));
    }
    Ok(doc.lines.remove(id))
}

fn router() -> Router<Doc> {
    let mut r = Router::new();
    r.method("addLine", add_line)
        .summary("Add a line")
        .tag("write")
        .args("{start, end, layer?}")
        .returns("{id}");
    r.method("line", |doc: &mut Doc, id: usize| {
        doc.lines
            .get(id)
            .cloned()
            .ok_or_else(|| Failure::new("not-found", format!("no line {id}")))
    })
    .summary("One line");
    r.method_async("count", count).summary("Number of lines");
    r.method_async("takeLine", take_line).tag("write");
    r
}

fn json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

#[test]
fn typed_sync_dispatch() {
    let r = router();
    let mut doc = Doc::default();
    let out = r
        .call_json_sync(
            &mut doc,
            "addLine",
            r#"{"start":[0,0],"end":[1,1],"layer":"a"}"#,
        )
        .unwrap();
    assert_eq!(out, r#"{"id":0}"#);
    let out = r.call_sync(&mut doc, "line", b"0").unwrap();
    assert_eq!(
        json(&out),
        json!({"start":[0.0,0.0],"end":[1.0,1.0],"layer":"a"})
    );
    // Sync methods also go through the async entry point.
    let out = block_on(r.call(&mut doc, "line", b"0")).unwrap();
    assert_eq!(json(&out)["layer"], "a");
}

#[test]
fn typed_async_dispatch() {
    let r = router();
    let mut doc = Doc::default();
    r.call_json_sync(&mut doc, "addLine", r#"{"start":[0,0],"end":[1,1]}"#)
        .unwrap();
    assert_eq!(block_on(r.call_json(&mut doc, "count", "")).unwrap(), "1");
    let line = block_on(r.call_json(&mut doc, "takeLine", "0")).unwrap();
    assert_eq!(
        serde_json::from_str::<Line>(&line).unwrap(),
        Line {
            start: [0.0, 0.0],
            end: [1.0, 1.0],
            layer: None
        }
    );
    let err = block_on(r.call_json(&mut doc, "takeLine", "0")).unwrap_err();
    assert_eq!(err.code, "not-found");
}

#[test]
fn async_handlers_that_really_suspend() {
    // A handler awaiting something that is not ready on the first poll.
    async fn wait(state: &mut Vec<u32>, n: u32) -> Result<u32, Failure> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move { tx.send(n * 2).unwrap() });
        let v = rx.await.map_err(|e| Failure::internal(e.to_string()))?;
        state.push(v);
        Ok(v)
    }
    let mut r = Router::<Vec<u32>>::new();
    r.method_async("wait", wait);
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut state = Vec::new();
    let out = rt.block_on(r.call_json(&mut state, "wait", "21")).unwrap();
    assert_eq!(out, "42");
    assert_eq!(state, vec![42]);
}

#[test]
fn boxed_async_closures() {
    let mut r = Router::<Vec<String>>::new();
    let suffix = String::from("!");
    r.method_async_boxed(
        "push",
        move |rows: &mut Vec<String>, row: String| -> BoxFuture<'_, _> {
            let suffix = suffix.clone();
            Box::pin(async move {
                rows.push(row + &suffix);
                Ok(rows.len())
            })
        },
    )
    .summary("Push a row");
    let mut rows = Vec::new();
    assert_eq!(
        block_on(r.call_json(&mut rows, "push", r#""a""#)).unwrap(),
        "1"
    );
    assert_eq!(rows, vec!["a!"]);
    assert!(r.method_info("push").unwrap().is_async);
}

#[test]
fn async_closures() {
    // Rust 2024 async closures satisfy the `AsyncHandler` helper trait when they capture
    // nothing by reference.
    let mut r = Router::<Vec<String>>::new();
    r.method_async("push", async |rows: &mut Vec<String>, row: String| {
        rows.push(row);
        Ok::<_, Failure>(rows.len())
    });
    let mut rows = Vec::new();
    assert_eq!(
        block_on(r.call_json(&mut rows, "push", r#""a""#)).unwrap(),
        "1"
    );
}

#[test]
fn bad_args() {
    let r = router();
    let mut doc = Doc::default();
    let err = r
        .call_json_sync(&mut doc, "addLine", r#"{"start":[0,0]}"#)
        .unwrap_err();
    assert_eq!(err.code, codes::BAD_ARGS);
    assert!(err.message.starts_with("addLine: "), "{}", err.message);
    assert!(
        err.message.contains("missing field `end`"),
        "{}",
        err.message
    );
    assert_eq!(err.hint.as_deref(), Some("expected {start, end, layer?}"));

    let err = r
        .call_json_sync(&mut doc, "addLine", r#"{"start":"x","end":[1,1]}"#)
        .unwrap_err();
    assert_eq!(err.code, codes::BAD_ARGS);
    assert!(err.message.contains("invalid type"), "{}", err.message);

    let err = r.call_json_sync(&mut doc, "line", "not json").unwrap_err();
    assert_eq!(err.code, codes::BAD_ARGS);
    assert_eq!(err.hint, None);

    // Async handlers decode the same way.
    let err = block_on(r.call_json(&mut doc, "takeLine", r#""zero""#)).unwrap_err();
    assert_eq!(err.code, codes::BAD_ARGS);
}

#[test]
fn unknown_method() {
    let r = router();
    let mut doc = Doc::default();
    let err = r.call_json_sync(&mut doc, "addline", "{}").unwrap_err();
    assert_eq!(err.code, codes::UNKNOWN_METHOD);
    assert_eq!(err.message, "no method \"addline\"");
    let hint = err.hint.unwrap();
    assert!(hint.contains("did you mean `addLine`"), "{hint}");
    assert!(hint.contains("`methods` lists every method"), "{hint}");

    let err = block_on(r.call(&mut doc, "nothing-like-it", b"")).unwrap_err();
    assert_eq!(err.code, codes::UNKNOWN_METHOD);
    assert_eq!(err.hint.as_deref(), Some("`methods` lists every method"));
}

#[test]
fn result_encode_error_is_internal() {
    let mut r = Router::<()>::new();
    r.method("weird", |_: &mut (), _: ()| {
        let mut m = HashMap::new();
        m.insert((1, 2), 3);
        Ok(m)
    });
    let err = r.call_sync(&mut (), "weird", b"").unwrap_err();
    assert_eq!(err.code, codes::INTERNAL);
    assert!(
        err.message.starts_with("weird: cannot encode the result"),
        "{}",
        err.message
    );
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
struct Filter {
    expand: Option<bool>,
    #[serde(default)]
    layer: String,
}

#[test]
fn empty_args_rule() {
    let mut r = Router::<()>::new();
    r.method("unit", |_: &mut (), _: ()| Ok("ok"));
    r.method("filter", |_: &mut (), f: Filter| Ok(f));
    r.method("maybe", |_: &mut (), n: Option<u32>| Ok(n));
    r.method("value", |_: &mut (), v: Value| Ok(v));
    r.method("line", |_: &mut (), l: Line| Ok(l));

    for empty in ["", "  ", "null", "{}", " { } "] {
        assert_eq!(
            r.call_json_sync(&mut (), "unit", empty).unwrap(),
            r#""ok""#,
            "{empty:?}"
        );
        assert_eq!(
            r.call_json_sync(&mut (), "filter", empty).unwrap(),
            r#"{"expand":null,"layer":""}"#,
            "{empty:?}"
        );
    }
    for empty in ["", "null"] {
        assert_eq!(
            r.call_json_sync(&mut (), "maybe", empty).unwrap(),
            "null",
            "{empty:?}"
        );
        assert_eq!(
            r.call_json_sync(&mut (), "value", empty).unwrap(),
            "null",
            "{empty:?}"
        );
    }
    assert_eq!(r.call_json_sync(&mut (), "value", "{}").unwrap(), "{}");
    assert_eq!(r.call_json_sync(&mut (), "maybe", "7").unwrap(), "7");
    assert_eq!(
        r.call_json_sync(&mut (), "filter", r#"{"expand":true}"#)
            .unwrap(),
        r#"{"expand":true,"layer":""}"#
    );

    // Only "no value" is rescued: other inputs still have to match.
    let err = r.call_json_sync(&mut (), "unit", "[]").unwrap_err();
    assert_eq!(err.code, codes::BAD_ARGS);
    // Required fields still fail, with serde's message rather than "EOF while parsing".
    for empty in ["", "null", "{}"] {
        let err = r.call_json_sync(&mut (), "line", empty).unwrap_err();
        assert_eq!(err.code, codes::BAD_ARGS);
        assert!(
            err.message.contains("missing field `start`"),
            "{empty:?}: {}",
            err.message
        );
    }
}

#[test]
fn guards_deny_in_order() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut r = router();
    let l = log.clone();
    r.guard(move |m, _| {
        l.lock().unwrap().push(format!("g1 {}", m.name));
        Ok(())
    });
    let l = log.clone();
    r.guard(move |m, doc: &Doc| {
        l.lock().unwrap().push(format!("g2 {}", m.name));
        if m.has_tag("write") && doc.read_only {
            Err(Failure::denied(format!("{} writes", m.name)).with_hint("ask for a write grant"))
        } else {
            Ok(())
        }
    });
    let l = log.clone();
    r.guard(move |m, _| {
        l.lock().unwrap().push(format!("g3 {}", m.name));
        Ok(())
    });

    let mut doc = Doc {
        read_only: true,
        ..Doc::default()
    };
    let err = r
        .call_json_sync(&mut doc, "addLine", r#"{"start":[0,0],"end":[1,1]}"#)
        .unwrap_err();
    assert_eq!(err.code, codes::DENIED);
    assert_eq!(err.hint.as_deref(), Some("ask for a write grant"));
    assert!(doc.lines.is_empty());
    assert_eq!(*log.lock().unwrap(), vec!["g1 addLine", "g2 addLine"]);

    // Guards run before argument decoding: a denied call with bad args is still "denied".
    let err = block_on(r.call_json(&mut doc, "takeLine", "bogus")).unwrap_err();
    assert_eq!(err.code, codes::DENIED);

    log.lock().unwrap().clear();
    assert_eq!(block_on(r.call_json(&mut doc, "count", "")).unwrap(), "0");
    assert_eq!(
        *log.lock().unwrap(),
        vec!["g1 count", "g2 count", "g3 count"]
    );
}

#[derive(Debug, PartialEq)]
struct Seen {
    method: String,
    args: String,
    result: Result<String, String>,
}

fn recorder(log: &Arc<Mutex<Vec<Seen>>>) -> impl Fn(&CallRecord<'_>) + Send + Sync + 'static {
    let log = log.clone();
    move |rec: &CallRecord<'_>| {
        log.lock().unwrap().push(Seen {
            method: rec.method.name.clone(),
            args: String::from_utf8(rec.args.to_vec()).unwrap(),
            result: match rec.result {
                Ok(v) => Ok(String::from_utf8(v.clone()).unwrap()),
                Err(f) => Err(f.code.clone()),
            },
        })
    }
}

fn seen(method: &str, args: &str, result: Result<&str, &str>) -> Seen {
    Seen {
        method: method.into(),
        args: args.into(),
        result: result.map(Into::into).map_err(Into::into),
    }
}

#[test]
fn observers_record_calls() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut r = router();
    r.guard(|m, doc: &Doc| {
        if m.has_tag("write") && doc.read_only {
            Err(Failure::denied("read-only"))
        } else {
            Ok(())
        }
    });
    r.observe(recorder(&log));
    let second = Arc::new(Mutex::new(0));
    let s = second.clone();
    r.observe(move |_| *s.lock().unwrap() += 1);

    let mut doc = Doc::default();
    r.call_json_sync(&mut doc, "addLine", r#"{"start":[0,0],"end":[1,1]}"#)
        .unwrap();
    block_on(r.call_json(&mut doc, "count", "")).unwrap();
    r.call_json_sync(&mut doc, "line", "true").unwrap_err();
    r.call_json_sync(&mut doc, "nope", "").unwrap_err();
    // call_sync on an async method never runs, so it is not observed.
    r.call_json_sync(&mut doc, "count", "").unwrap_err();
    doc.read_only = true;
    r.call_json_sync(&mut doc, "addLine", "{}").unwrap_err();

    assert_eq!(
        *log.lock().unwrap(),
        vec![
            seen(
                "addLine",
                r#"{"start":[0,0],"end":[1,1]}"#,
                Ok(r#"{"id":0}"#)
            ),
            seen("count", "", Ok("1")),
            seen("line", "true", Err("bad-args")),
            seen("addLine", "{}", Err("denied")),
        ]
    );
    assert_eq!(*second.lock().unwrap(), 4);
}

#[test]
fn batch_runs_in_request_order() {
    let r = router();
    let mut doc = Doc::default();
    let line = |x: i32| format!(r#"{{"start":[{x},0],"end":[1,1]}}"#).into_bytes();
    let calls = vec![
        ("addLine".to_owned(), line(1)),
        ("count".to_owned(), Vec::new()),
        ("addLine".to_owned(), line(2)),
        ("nope".to_owned(), Vec::new()),
        ("takeLine".to_owned(), b"0".to_vec()),
        ("count".to_owned(), Vec::new()),
        ("line".to_owned(), b"0".to_vec()),
    ];
    let out = block_on(r.batch(&mut doc, &calls));
    assert_eq!(out.len(), calls.len());
    assert_eq!(json(out[0].as_ref().unwrap()), json!({"id": 0}));
    assert_eq!(json(out[1].as_ref().unwrap()), json!(1));
    assert_eq!(json(out[2].as_ref().unwrap()), json!({"id": 1}));
    assert_eq!(out[3].as_ref().unwrap_err().code, codes::UNKNOWN_METHOD);
    assert_eq!(json(out[4].as_ref().unwrap())["start"], json!([1.0, 0.0]));
    assert_eq!(json(out[5].as_ref().unwrap()), json!(1));
    assert_eq!(json(out[6].as_ref().unwrap())["start"], json!([2.0, 0.0]));
}

#[test]
fn call_sync_on_async_method() {
    let r = router();
    let mut doc = Doc::default();
    let err = r.call_sync(&mut doc, "count", b"").unwrap_err();
    assert_eq!(err.code, codes::ASYNC_ONLY);
    assert!(err.message.contains("\"count\""), "{}", err.message);
    assert_eq!(block_on(r.call(&mut doc, "count", b"")).unwrap(), b"0");
}

#[test]
fn builtin_methods() {
    let r = router();
    let mut doc = Doc::default();
    let out = r.call_json_sync(&mut doc, "methods", "").unwrap();
    let list: Vec<MethodInfo> = serde_json::from_str(&out).unwrap();
    let names: Vec<&str> = list.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["addLine", "line", "count", "takeLine", "methods"]);
    assert_eq!(list[0].summary, "Add a line");
    assert_eq!(list[0].tags, ["write"]);
    assert_eq!(list[0].args.as_deref(), Some("{start, end, layer?}"));
    assert_eq!(list[0].returns.as_deref(), Some("{id}"));
    assert!(!list[0].is_async);
    assert!(list[2].is_async);
    assert_eq!(list, r.methods().cloned().collect::<Vec<_>>());

    // The raw shape: absent optional metadata is omitted.
    let raw: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        raw[1],
        json!({"name": "line", "summary": "One line", "tags": [], "is_async": false})
    );

    assert!(r.contains("methods"));
    assert_eq!(r.method_info("methods").unwrap().name, "methods");
    assert!(r.method_info("nope").is_none());
    // Also reachable asynchronously.
    assert_eq!(
        block_on(r.call_json(&mut doc, "methods", "{}")).unwrap(),
        out
    );
}

#[test]
fn builtin_methods_is_guarded_and_observed() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut r = Router::<bool>::new();
    r.guard(|m, allowed: &bool| {
        if m.name == "methods" && !allowed {
            Err(Failure::denied("no discovery"))
        } else {
            Ok(())
        }
    });
    r.observe(recorder(&log));
    assert_eq!(
        r.call_json_sync(&mut false, "methods", "")
            .unwrap_err()
            .code,
        codes::DENIED
    );
    assert_eq!(
        r.call_json_sync(&mut true, "methods", "").unwrap(),
        r#"[{"name":"methods","summary":"Every method with its summary, tags, arguments and result.","tags":[],"args":"{}","returns":"[{name, summary, tags, args?, returns?, args_schema?, returns_schema?, is_async}]","is_async":false}]"#
    );
    assert_eq!(log.lock().unwrap().len(), 2);
}

#[test]
fn override_builtin_methods() {
    let mut r = router();
    r.method("methods", |_: &mut Doc, _: ()| Ok(vec!["just", "these"]))
        .summary("Custom listing");
    let mut doc = Doc::default();
    assert_eq!(
        r.call_json_sync(&mut doc, "methods", "").unwrap(),
        r#"["just","these"]"#
    );
    let names: Vec<&str> = r.methods().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["addLine", "line", "count", "takeLine", "methods"]);
    assert_eq!(r.method_info("methods").unwrap().summary, "Custom listing");
}

#[test]
#[should_panic(expected = "method \"addLine\" is registered twice")]
fn duplicate_names_panic() {
    let mut r = router();
    r.method("addLine", add_line);
}

#[test]
#[should_panic(expected = "must not be empty")]
fn empty_names_panic() {
    let mut r = Router::<()>::new();
    r.method("", |_: &mut (), _: ()| Ok(()));
}

// ---------------------------------------------------------------------------
// Nesting
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Files {
    store: HashMap<String, String>,
    writable: bool,
}

#[derive(Default)]
struct Server {
    doc: Doc,
    files: Files,
    events: Vec<String>,
}

#[derive(Deserialize)]
struct Write {
    name: String,
    text: String,
}

#[derive(Deserialize)]
struct Read {
    name: String,
}

async fn read_file(files: &mut Files, args: Read) -> Result<String, Failure> {
    files
        .store
        .get(&args.name)
        .cloned()
        .ok_or_else(|| Failure::new("not-found", args.name))
}

fn files_router(log: &Arc<Mutex<Vec<String>>>) -> Router<Files> {
    let mut r = Router::new();
    r.method("write", |f: &mut Files, w: Write| {
        f.store.insert(w.name, w.text);
        Ok(())
    })
    .tag("write")
    .summary("Write a file");
    r.method_async("read", read_file).summary("Read a file");
    let l = log.clone();
    r.guard(move |m, f: &Files| {
        l.lock().unwrap().push(format!("inner guard {}", m.name));
        if m.has_tag("write") && !f.writable {
            return Err(Failure::denied("files are read-only"));
        }
        Ok(())
    });
    let l = log.clone();
    r.observe(move |rec| {
        l.lock().unwrap().push(format!(
            "inner observe {} {}",
            rec.method.name,
            rec.result.is_ok()
        ))
    });
    r
}

fn server(log: &Arc<Mutex<Vec<String>>>) -> Router<Server> {
    let mut r = Router::new();
    let l = log.clone();
    r.guard(move |m, _| {
        l.lock().unwrap().push(format!("outer guard {}", m.name));
        Ok(())
    });
    r.nest("files", files_router(log), |s: &mut Server| &mut s.files);
    r.nest("cad", router(), |s: &mut Server| &mut s.doc);
    r.method("events", |s: &mut Server, _: ()| Ok(s.events.clone()));
    let l = log.clone();
    r.observe(move |rec| {
        l.lock().unwrap().push(format!(
            "outer observe {} {}",
            rec.method.name,
            rec.result.is_ok()
        ))
    });
    r
}

#[test]
fn nest_with_projection() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let r = server(&log);
    let names: Vec<&str> = r.methods().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "files.write",
            "files.read",
            "cad.addLine",
            "cad.line",
            "cad.count",
            "cad.takeLine",
            "events",
            "methods"
        ]
    );
    // Metadata survives the move.
    let info = r.method_info("cad.addLine").unwrap();
    assert_eq!(info.summary, "Add a line");
    assert_eq!(info.tags, ["write"]);
    assert!(r.method_info("files.read").unwrap().is_async);
    assert!(r.method_info("files.methods").is_none());

    let mut s = Server::default();
    s.files.writable = true;
    r.call_json_sync(&mut s, "files.write", r#"{"name":"a.txt","text":"hi"}"#)
        .unwrap();
    assert_eq!(s.files.store["a.txt"], "hi");
    assert_eq!(
        block_on(r.call_json(&mut s, "files.read", r#"{"name":"a.txt"}"#)).unwrap(),
        r#""hi""#
    );
    r.call_json_sync(&mut s, "cad.addLine", r#"{"start":[0,0],"end":[1,1]}"#)
        .unwrap();
    assert_eq!(s.doc.lines.len(), 1);
    assert_eq!(block_on(r.call_json(&mut s, "cad.count", "")).unwrap(), "1");

    // Errors name the full method.
    let err = r.call_json_sync(&mut s, "cad.addLine", "{}").unwrap_err();
    assert!(err.message.starts_with("cad.addLine: "), "{}", err.message);
    let err = r.call_json_sync(&mut s, "read", "{}").unwrap_err();
    assert!(err.hint.unwrap().contains("`files.read`"));
}

#[test]
fn nested_guards_and_observers_apply() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let r = server(&log);
    let mut s = Server::default();

    let err = r
        .call_json_sync(&mut s, "files.write", r#"{"name":"a","text":"b"}"#)
        .unwrap_err();
    assert_eq!(err.message, "files are read-only");
    assert_eq!(
        std::mem::take(&mut *log.lock().unwrap()),
        [
            "outer guard files.write",
            "inner guard files.write",
            "inner observe files.write false",
            "outer observe files.write false",
        ]
    );

    block_on(r.call_json(&mut s, "files.read", r#"{"name":"a"}"#)).unwrap_err();
    assert_eq!(
        std::mem::take(&mut *log.lock().unwrap()),
        [
            "outer guard files.read",
            "inner guard files.read",
            "inner observe files.read false",
            "outer observe files.read false",
        ]
    );

    // The nested router's guards do not leak onto other methods.
    r.call_json_sync(&mut s, "events", "").unwrap();
    assert_eq!(
        std::mem::take(&mut *log.lock().unwrap()),
        ["outer guard events", "outer observe events true"]
    );
}

#[test]
fn nest_three_levels_and_prefix_forms() {
    struct A {
        b: B,
    }
    struct B {
        c: u32,
    }
    let mut leaf = Router::<u32>::new();
    leaf.method("inc", |c: &mut u32, by: u32| {
        *c += by;
        Ok(*c)
    });
    let mut mid = Router::<B>::new();
    mid.nest("c.", leaf, |b: &mut B| &mut b.c);
    let mut top = Router::<A>::new();
    top.nest("b", mid, |a: &mut A| &mut a.b);
    let mut a = A { b: B { c: 1 } };
    assert_eq!(top.call_json_sync(&mut a, "b.c.inc", "2").unwrap(), "3");
    assert_eq!(a.b.c, 3);

    let mut plain = Router::<u32>::new();
    plain.method("get", |c: &mut u32, _: ()| Ok(*c));
    let mut root = Router::<A>::new();
    root.nest("", plain, |a: &mut A| &mut a.b.c);
    assert_eq!(root.call_json_sync(&mut a, "get", "").unwrap(), "3");
}

#[test]
#[should_panic(expected = "method \"files.read\" is registered twice")]
fn nest_collisions_panic() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut r = Router::<Server>::new();
    r.method("files.read", |_: &mut Server, _: ()| Ok(()));
    r.nest("files", files_router(&log), |s: &mut Server| &mut s.files);
}

#[test]
fn merge_keeps_guard_scope() {
    let mut a = Router::<Vec<&'static str>>::new();
    a.method("a", |s: &mut Vec<&'static str>, _: ()| {
        s.push("a");
        Ok(())
    });
    let mut b = Router::<Vec<&'static str>>::new();
    b.method("b", |s: &mut Vec<&'static str>, _: ()| {
        s.push("b");
        Ok(())
    });
    b.guard(|_, _| Err(Failure::denied("b is closed")));
    let mut c = Router::<Vec<&'static str>>::new();
    c.method_async_boxed("c", |s: &mut Vec<&'static str>, _: ()| {
        Box::pin(async move {
            s.push("c");
            Ok(())
        })
    });
    a.merge(b).merge(c);
    let mut s = Vec::new();
    a.call_json_sync(&mut s, "a", "").unwrap();
    assert_eq!(
        a.call_json_sync(&mut s, "b", "").unwrap_err().code,
        codes::DENIED
    );
    block_on(a.call_json(&mut s, "c", "")).unwrap();
    assert_eq!(s, ["a", "c"]);
    let names: Vec<&str> = a.methods().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["a", "b", "c", "methods"]);
}

// ---------------------------------------------------------------------------
// Failure, Send/Sync
// ---------------------------------------------------------------------------

#[test]
fn failure_shape() {
    let f = Failure::denied("nope")
        .with_hint("ask")
        .with_detail(json!({"grant": "write"}));
    assert_eq!(f.to_string(), "denied: nope (ask)");
    assert!(f.is(codes::DENIED));
    assert_eq!(
        serde_json::to_value(&f).unwrap(),
        json!({"code": "denied", "message": "nope", "hint": "ask", "detail": {"grant": "write"}})
    );
    assert_eq!(
        serde_json::to_value(Failure::internal("x")).unwrap(),
        json!({"code": "internal", "message": "x"})
    );
    let back: Failure = serde_json::from_value(json!({"code": "c", "message": "m"})).unwrap();
    assert_eq!(back, Failure::new("c", "m"));
    let _: &dyn std::error::Error = &back;
    assert_eq!(Failure::bad_args("m").code, "bad-args");
    assert_eq!(Failure::unknown_method("x").code, "unknown-method");
    assert_eq!(Failure::async_only("x").code, "async-only");
}

fn assert_send_sync<T: Send + Sync>() {}
fn assert_send<T: Send>(_: &T) {}

#[test]
fn routers_are_send_and_sync() {
    // Whatever the state: handlers are `Send + Sync`, the state is only borrowed per call.
    assert_send_sync::<Router<Doc>>();
    assert_send_sync::<Router<std::rc::Rc<u8>>>();
    assert_send_sync::<Router<std::cell::Cell<u8>, Json>>();
    assert_send_sync::<Failure>();
    assert_send_sync::<MethodInfo>();

    let r = router();
    let mut doc = Doc::default();
    let fut = r.call(&mut doc, "count", b"");
    assert_send(&fut);
    drop(fut);
    let calls = Vec::new();
    let fut = r.batch(&mut doc, &calls);
    assert_send(&fut);
    drop(fut);
    let fut = r.call_json(&mut doc, "count", "");
    assert_send(&fut);
}

#[test]
fn shared_router_across_tasks() {
    let r = Arc::new(router());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut tasks = Vec::new();
        for i in 0..8 {
            let r = r.clone();
            tasks.push(tokio::spawn(async move {
                let mut doc = Doc::default();
                for _ in 0..i {
                    r.call_json(&mut doc, "addLine", r#"{"start":[0,0],"end":[1,1]}"#)
                        .await
                        .unwrap();
                }
                r.call_json(&mut doc, "count", "").await.unwrap()
            }));
        }
        for (i, t) in tasks.into_iter().enumerate() {
            assert_eq!(t.await.unwrap(), i.to_string());
        }
    });
}
