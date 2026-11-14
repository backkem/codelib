//! Hardening tests for the router and the code generators: hostile method names and
//! arguments, resource bounds, and output that must stay valid for odd inputs.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use codelib_rpc::codegen::{self, JsOptions};
use codelib_rpc::{CallRecord, Failure, MethodInfo, Router, codes};
use pollster::block_on;
use serde::Deserialize;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Attacker-controlled method names and arguments
// ---------------------------------------------------------------------------

fn big_router() -> Router<()> {
    let mut r = Router::<()>::new();
    for i in 0..100 {
        r.method(
            &format!("namespace{i}.someLongMethodName{i}"),
            |_: &mut (), _: ()| Ok(()),
        );
    }
    r
}

/// The near-miss hint compares the caller's name with every registered name, so its cost has to
/// stay bounded however long the name is.
#[test]
fn huge_unknown_method_name_is_cheap() {
    let r = big_router();
    let mut state = ();
    let name = "x".repeat(1 << 16);
    let t = Instant::now();
    let err = r.call_json_sync(&mut state, &name, "").unwrap_err();
    let took = t.elapsed();
    eprintln!(
        "64 KiB unknown method name: {took:?}; message len {}",
        err.message.len()
    );
    assert_eq!(err.code, codes::UNKNOWN_METHOD);
    assert!(
        took.as_millis() < 250,
        "an unknown 64 KiB method name took {took:?} (hint computation); it should be bounded"
    );
}

/// The failure for an unknown method does not echo a caller-sized method name in full.
#[test]
fn unknown_method_failure_is_bounded() {
    let r = Router::<()>::new();
    let mut state = ();
    let name = "y".repeat(1 << 20);
    let err = r.call_json_sync(&mut state, &name, "").unwrap_err();
    assert!(
        err.message.len() < 4096,
        "the failure message echoes the {}-byte method name in full",
        err.message.len()
    );
}

#[test]
fn unknown_method_name_is_escaped_in_the_message() {
    let r = Router::<()>::new();
    let mut state = ();
    for name in ["a\nb", "a\u{0}b", "\u{202e}x", "`x`", "\"quoted\"", ""] {
        let err = r.call_json_sync(&mut state, name, "").unwrap_err();
        assert_eq!(err.code, codes::UNKNOWN_METHOD);
        assert!(
            !err.message.contains('\n') && !err.message.contains('\u{0}'),
            "{:?}",
            err.message
        );
        assert!(!err.message.contains('\u{202e}'), "{:?}", err.message);
    }
}

#[test]
fn odd_method_names_never_panic() {
    let mut r = Router::<()>::new();
    r.method("a.b", |_: &mut (), _: ()| Ok(1));
    r.method("é.ü", |_: &mut (), _: ()| Ok(2));
    let mut state = ();
    for name in [
        "",
        ".",
        "..",
        ".a",
        "a.",
        "a..b",
        "A.B",
        "é",
        "É.Ü",
        "\u{1F600}",
        "a\u{0}",
        "methods ",
        "METHODS",
        "\u{130}",
        "ß",
        "x".repeat(10_000).as_str(),
    ] {
        let _ = r.call_json_sync(&mut state, name, "");
        let _ = block_on(r.call_json(&mut state, name, "{}"));
    }
    assert_eq!(r.call_json_sync(&mut state, "é.ü", "").unwrap(), "2");
}

#[derive(Deserialize)]
struct Anything {
    #[allow(dead_code)]
    a: Value,
}

#[test]
fn deeply_nested_json_args_hit_a_recursion_limit_not_the_stack() {
    let mut r = Router::<()>::new();
    r.method("v", |_: &mut (), _: Value| Ok(()));
    r.method("s", |_: &mut (), _: Anything| Ok(()));
    r.method("u", |_: &mut (), _: ()| Ok(()));
    let mut state = ();
    for depth in [200usize, 100_000, 3_000_000] {
        let nested = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let obj = format!(r#"{{"a":{nested}}}"#);
        let unclosed = "[".repeat(depth);
        for (m, args) in [
            ("v", &nested),
            ("s", &obj),
            ("u", &nested),
            ("v", &unclosed),
        ] {
            let res = r.call_json_sync(&mut state, m, args);
            if depth > 200 {
                let e = res.unwrap_err();
                assert_eq!(e.code, codes::BAD_ARGS);
                assert!(e.message.len() < 500, "{}", e.message.len());
            }
        }
    }
}

#[test]
fn bad_args_message_echo_is_small_for_big_input() {
    #[derive(Deserialize)]
    #[allow(dead_code)]
    enum E {
        A,
    }
    let mut r = Router::<()>::new();
    r.method("e", |_: &mut (), _: E| Ok(()));
    let mut state = ();
    let big = format!("\"{}\"", "z".repeat(1 << 20));
    let err = r.call_json_sync(&mut state, "e", &big).unwrap_err();
    assert_eq!(err.code, codes::BAD_ARGS);
    // serde echoes the unknown variant: attacker-sized text lands in the message.
    eprintln!(
        "bad-args message length for a 1 MiB variant name: {}",
        err.message.len()
    );
    assert!(
        err.message.len() < 4096,
        "message is {} bytes",
        err.message.len()
    );
}

// ---------------------------------------------------------------------------
// The empty-args rule
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, PartialEq)]
struct Req {
    id: u32,
}

#[derive(Debug, Deserialize, PartialEq)]
enum Kind {
    One,
    Two { n: u32 },
}

#[test]
fn empty_args_rule_never_hides_required_inputs() {
    let mut r = Router::<()>::new();
    r.method("strukt", |_: &mut (), a: Req| Ok(a.id));
    r.method("enm", |_: &mut (), a: Kind| Ok(format!("{a:?}")));
    r.method("vec", |_: &mut (), a: Vec<u32>| Ok(a.len()));
    r.method("tuple", |_: &mut (), a: (u32, u32)| Ok(a.0 + a.1));
    r.method("string", |_: &mut (), a: String| Ok(a));
    r.method("num", |_: &mut (), a: u32| Ok(a));
    r.method("unit", |_: &mut (), _: ()| Ok("ok"));
    let mut state = ();
    for m in ["strukt", "enm", "vec", "tuple", "string", "num"] {
        for args in ["", " ", "null", "{}", " { } ", "\n{\n}\n"] {
            let e = r
                .call_json_sync(&mut state, m, args)
                .expect_err(&format!("{m} with {args:?} must fail"));
            assert_eq!(e.code, codes::BAD_ARGS, "{m} {args:?}");
        }
    }
    for args in ["", " ", "null", "{}", " { } "] {
        assert_eq!(
            r.call_json_sync(&mut state, "unit", args).unwrap(),
            "\"ok\""
        );
    }
    for args in [
        "[]",
        "0",
        "false",
        r#"{"a":1}"#,
        "nul",
        "{",
        "{} x",
        "null null",
    ] {
        assert!(
            r.call_json_sync(&mut state, "unit", args).is_err(),
            "unit {args:?}"
        );
    }
}

/// `Option<Struct>` given `{}`: the struct's "missing field" error is swallowed and the handler
/// sees `None` (documented: "an `Option<T>` handler gets `None`"), so a caller that forgot the
/// fields is silently treated as having passed nothing.
#[test]
fn option_struct_given_empty_object_is_none() {
    let mut r = Router::<()>::new();
    r.method("opt", |_: &mut (), a: Option<Req>| Ok(a.map(|r| r.id)));
    let mut state = ();
    assert_eq!(r.call_json_sync(&mut state, "opt", "{}").unwrap(), "null");
    assert_eq!(
        r.call_json_sync(&mut state, "opt", r#"{"id":3}"#).unwrap(),
        "3"
    );
    assert!(
        r.call_json_sync(&mut state, "opt", r#"{"id":"x"}"#)
            .is_err()
    );
}

/// Form feed (0x0C) is not JSON whitespace, so arguments made of it are invalid JSON, not
/// "no arguments".
#[test]
fn form_feed_is_not_empty_json() {
    let mut r = Router::<()>::new();
    r.method("unit", |_: &mut (), _: ()| Ok(1));
    let mut state = ();
    for args in ["\u{c}", "{\u{c}}", "\u{c}null"] {
        assert!(
            r.call_json_sync(&mut state, "unit", args).is_err(),
            "{args:?} is not valid JSON and must not be accepted as empty arguments"
        );
    }
}

#[test]
fn unicode_and_nul_whitespace_is_not_empty() {
    let mut r = Router::<()>::new();
    r.method("unit", |_: &mut (), _: ()| Ok(1));
    let mut state = ();
    for args in ["\u{a0}", "\u{feff}", "\0", "\u{b}", "{\u{a0}}"] {
        assert!(
            r.call_json_sync(&mut state, "unit", args).is_err(),
            "{args:?}"
        );
    }
}

#[test]
fn result_encode_errors_are_internal_on_every_path() {
    use std::collections::HashMap;
    let mut r = Router::<()>::new();
    r.method("bad", |_: &mut (), _: ()| {
        Ok(HashMap::from([((1, 2), 3)])) // tuple keys do not serialize to JSON
    });
    r.method_async_boxed("abad", |_: &mut (), _: ()| {
        Box::pin(async { Ok(HashMap::from([((1, 2), 3)])) })
    });
    let mut state = ();
    assert_eq!(
        r.call_json_sync(&mut state, "bad", "").unwrap_err().code,
        codes::INTERNAL
    );
    assert_eq!(
        block_on(r.call_json(&mut state, "abad", ""))
            .unwrap_err()
            .code,
        codes::INTERNAL
    );
}

// ---------------------------------------------------------------------------
// Guards and observers: every path
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Server {
    files: Files,
    calls: u32,
}

#[derive(Default)]
struct Files {
    secret: bool,
    reads: u32,
}

type Log = Arc<Mutex<Vec<String>>>;

fn files(log: &Log) -> Router<Files> {
    let mut r = Router::<Files>::new();
    r.method("read", |f: &mut Files, _: ()| {
        f.reads += 1;
        Ok(f.reads)
    })
    .tag("fs");
    r.method_async("aread", async_read).tag("fs");
    r.method("methods", |_: &mut Files, _: ()| Ok("inner override"));
    let l = log.clone();
    r.guard(move |m: &MethodInfo, f: &Files| {
        l.lock()
            .unwrap()
            .push(format!("inner-guard {} secret={}", m.name, f.secret));
        if f.secret {
            Err(Failure::denied("inner"))
        } else {
            Ok(())
        }
    });
    let l = log.clone();
    r.observe(move |c: &CallRecord<'_>| {
        l.lock().unwrap().push(format!(
            "inner-obs {} ok={}",
            c.method.name,
            c.result.is_ok()
        ));
    });
    r
}

async fn async_read(f: &mut Files, _: ()) -> Result<u32, Failure> {
    f.reads += 10;
    Ok(f.reads)
}

fn server(log: &Log, deny_outer: bool) -> Router<Server> {
    let mut r = Router::<Server>::new();
    r.nest("files", files(log), |s: &mut Server| &mut s.files);
    r.method("top", |s: &mut Server, _: ()| {
        s.calls += 1;
        Ok(s.calls)
    });
    let l = log.clone();
    r.guard(move |m: &MethodInfo, _: &Server| {
        l.lock().unwrap().push(format!("outer-guard {}", m.name));
        if deny_outer && m.name == "files.read" {
            Err(Failure::denied("outer"))
        } else {
            Ok(())
        }
    });
    let l = log.clone();
    r.observe(move |c: &CallRecord<'_>| {
        l.lock()
            .unwrap()
            .push(format!("outer-obs {}", c.method.name));
    });
    r
}

fn take(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.lock().unwrap())
}

#[test]
fn nested_layers_run_once_in_documented_order_on_every_entry_point() {
    let log = Log::default();
    let r = server(&log, false);
    let mut s = Server::default();

    let expected_sync = vec![
        "outer-guard files.read",
        "inner-guard files.read secret=false",
        "inner-obs files.read ok=true",
        "outer-obs files.read",
    ];
    r.call_sync(&mut s, "files.read", b"").unwrap();
    assert_eq!(take(&log), expected_sync);
    block_on(r.call(&mut s, "files.read", b"")).unwrap();
    assert_eq!(take(&log), expected_sync);
    block_on(r.batch(&mut s, &[("files.read".into(), vec![])]))
        .pop()
        .unwrap()
        .unwrap();
    assert_eq!(take(&log), expected_sync);

    // async nested method
    block_on(r.call(&mut s, "files.aread", b"")).unwrap();
    assert_eq!(
        take(&log),
        vec![
            "outer-guard files.aread",
            "inner-guard files.aread secret=false",
            "inner-obs files.aread ok=true",
            "outer-obs files.aread",
        ]
    );
    // call_sync on an async nested method runs nothing.
    assert_eq!(
        r.call_sync(&mut s, "files.aread", b"").unwrap_err().code,
        codes::ASYNC_ONLY
    );
    assert!(take(&log).is_empty());

    // The nested override of `methods` is mounted as `files.methods` and is guarded.
    assert_eq!(
        r.call_json_sync(&mut s, "files.methods", "").unwrap(),
        "\"inner override\""
    );
    assert_eq!(take(&log).len(), 4);

    // Bad args in a nested method: both observer layers still run exactly once.
    #[derive(Deserialize)]
    struct N {
        #[allow(dead_code)]
        n: u32,
    }
    let mut rr = Router::<Server>::new();
    let l2 = log.clone();
    let mut inner = Router::<Files>::new();
    inner.method("need", |_: &mut Files, _: N| Ok(()));
    inner.observe(move |c| {
        l2.lock()
            .unwrap()
            .push(format!("inner-obs {} {}", c.method.name, c.result.is_err()))
    });
    rr.nest("f", inner, |s: &mut Server| &mut s.files);
    let l3 = log.clone();
    rr.observe(move |c| {
        l3.lock()
            .unwrap()
            .push(format!("outer-obs {}", c.method.name))
    });
    assert_eq!(
        rr.call_sync(&mut s, "f.need", b"{}").unwrap_err().code,
        codes::BAD_ARGS
    );
    assert_eq!(
        take(&log),
        vec!["inner-obs f.need true", "outer-obs f.need"]
    );
}

#[test]
fn outer_denial_skips_inner_layers_and_inner_denial_is_observed() {
    let log = Log::default();
    let r = server(&log, true);
    let mut s = Server::default();
    let e = r.call_sync(&mut s, "files.read", b"").unwrap_err();
    assert_eq!(e.message, "outer");
    assert_eq!(
        take(&log),
        vec!["outer-guard files.read", "outer-obs files.read"]
    );
    assert_eq!(s.files.reads, 0);

    let r = server(&log, false);
    s.files.secret = true;
    let e = r.call_sync(&mut s, "files.read", b"").unwrap_err();
    assert_eq!(e.message, "inner");
    assert_eq!(s.files.reads, 0);
    let e = block_on(r.call(&mut s, "files.aread", b"")).unwrap_err();
    assert_eq!(e.message, "inner");
    assert_eq!(
        s.files.reads, 0,
        "async handler must not run when the inner guard denies"
    );
    take(&log);
}

#[test]
fn merged_router_guards_do_not_leak_to_the_host_and_still_apply() {
    let log = Log::default();
    let mut other = Router::<Server>::new();
    other.method("guarded", |_: &mut Server, _: ()| Ok(1));
    other.guard(|_, _| Err(Failure::denied("other")));
    let mut r = Router::<Server>::new();
    r.method("free", |_: &mut Server, _: ()| Ok(2));
    r.merge(other);
    let mut s = Server::default();
    assert_eq!(r.call_json_sync(&mut s, "free", "").unwrap(), "2");
    assert_eq!(
        r.call_json_sync(&mut s, "guarded", "").unwrap_err().message,
        "other"
    );
    // A router merged without layers keeps nothing special.
    let mut plain = Router::<Server>::new();
    plain.method("p", |_: &mut Server, _: ()| Ok(3));
    r.merge(plain);
    assert_eq!(r.call_json_sync(&mut s, "p", "").unwrap(), "3");
    // A guard added to the outer router after merging also covers the merged methods.
    r.guard(|m, _| {
        if m.name == "p" {
            Err(Failure::denied("late"))
        } else {
            Ok(())
        }
    });
    assert_eq!(
        r.call_json_sync(&mut s, "p", "").unwrap_err().message,
        "late"
    );
    drop(log);
}

#[test]
fn guards_cover_the_builtin_and_an_overridden_methods() {
    let mut r = Router::<u8>::new();
    r.method("a", |_: &mut u8, _: ()| Ok(1));
    r.guard(|m, s: &u8| {
        if *s == 1 && m.name == "methods" {
            Err(Failure::denied("no"))
        } else {
            Ok(())
        }
    });
    let mut s = 1u8;
    assert_eq!(
        r.call_json_sync(&mut s, "methods", "").unwrap_err().code,
        codes::DENIED
    );
    assert_eq!(
        block_on(r.batch(&mut s, &[("methods".into(), vec![])]))[0]
            .as_ref()
            .unwrap_err()
            .code,
        codes::DENIED
    );
    r.method("methods", |_: &mut u8, _: ()| Ok("mine"));
    assert_eq!(
        r.call_json_sync(&mut s, "methods", "").unwrap_err().code,
        codes::DENIED
    );
    s = 0;
    assert_eq!(r.call_json_sync(&mut s, "methods", "").unwrap(), "\"mine\"");
    assert_eq!(r.methods().filter(|m| m.name == "methods").count(), 1);
}

#[test]
fn nested_guard_sees_projected_state_and_full_name() {
    type Seen = Vec<(String, Vec<String>, bool, u32)>;
    let seen: Arc<Mutex<Seen>> = Default::default();
    let mut inner = Router::<Files>::new();
    inner
        .method("read", |_: &mut Files, _: ()| Ok(()))
        .tag("fs")
        .summary("s");
    let sc = seen.clone();
    inner.guard(move |m, f: &Files| {
        sc.lock()
            .unwrap()
            .push((m.name.clone(), m.tags.clone(), m.is_async, f.reads));
        Ok(())
    });
    let mut r = Router::<Server>::new();
    let mut deeper = Router::<Server>::new();
    deeper.nest("files.", inner, |s: &mut Server| &mut s.files);
    r.nest("srv", deeper, |s: &mut Server| s);
    let mut s = Server::default();
    s.files.reads = 41;
    r.call_sync(&mut s, "srv.files.read", b"").unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(
            "srv.files.read".to_string(),
            vec!["fs".to_string()],
            false,
            41
        )]
    );
}

#[test]
fn builtin_methods_lists_nested_with_prefix_and_not_inner_builtin() {
    let log = Log::default();
    let r = server(&log, false);
    let mut s = Server::default();
    let out = r.call_json_sync(&mut s, "methods", "").unwrap();
    let list: Vec<MethodInfo> = serde_json::from_str(&out).unwrap();
    let names: Vec<_> = list.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "files.read",
            "files.aread",
            "files.methods",
            "top",
            "methods"
        ]
    );
}

#[test]
#[should_panic(expected = "registered twice")]
fn duplicate_via_merge_panics() {
    let mut a = Router::<()>::new();
    a.method("x", |_: &mut (), _: ()| Ok(1));
    let mut b = Router::<()>::new();
    b.method("x", |_: &mut (), _: ()| Ok(1));
    a.merge(b);
}

#[test]
#[should_panic(expected = "registered twice")]
fn registering_methods_twice_panics() {
    let mut a = Router::<()>::new();
    a.method("methods", |_: &mut (), _: ()| Ok(1));
    a.method("methods", |_: &mut (), _: ()| Ok(1));
}

#[test]
fn batch_applies_guards_per_call_and_continues() {
    let mut r = Router::<u32>::new();
    r.method("inc", |s: &mut u32, _: ()| {
        *s += 1;
        Ok(*s)
    });
    r.guard(|_, s: &u32| {
        if *s >= 2 {
            Err(Failure::denied("limit"))
        } else {
            Ok(())
        }
    });
    let mut s = 0;
    let calls: Vec<(String, Vec<u8>)> = ["inc", "nope", "inc", "inc", "methods"]
        .iter()
        .map(|m| (m.to_string(), vec![]))
        .collect();
    let out = block_on(r.batch(&mut s, &calls));
    let codes_: Vec<_> = out
        .iter()
        .map(|r| r.as_ref().err().map(|e| e.code.clone()))
        .collect();
    assert_eq!(
        codes_,
        vec![
            None,
            Some("unknown-method".into()),
            None,
            Some("denied".into()),
            Some("denied".into())
        ]
    );
}

// ---------------------------------------------------------------------------
// Codegen (pure output checks; the JS-engine run is in crates/codelib/tests/rpc_client.rs)
// ---------------------------------------------------------------------------

fn infos(names: &[&str]) -> Vec<MethodInfo> {
    names.iter().map(|n| MethodInfo::new(*n)).collect()
}

/// In TypeScript, `new(args): R;` inside a type literal is a construct signature, not a method
/// named `new`; the key must be quoted.
#[test]
fn typescript_method_named_new_is_quoted() {
    let ts = codegen::typescript(&infos(&["new", "ns.new"]), &JsOptions::new("api"));
    assert!(
        !ts.lines().any(|l| l.trim_start().starts_with("new(")),
        "a method named `new` is emitted as a construct signature:\n{ts}"
    );
}

/// An array of a union whose parts are both parenthesized (`(A & B) | (C & D)`) is wrapped as
/// a whole, so `[]` applies to the union and not just its last part.
#[test]
fn schema_to_ts_array_of_union_of_intersections() {
    let part = |a: &str, b: &str| {
        json!({"allOf": [
            {"type": "object", "properties": {a: {"type": "string"}}},
            {"type": "object", "properties": {b: {"type": "string"}}}
        ]})
    };
    let s = json!({"type": "array", "items": {"anyOf": [part("a", "b"), part("c", "d")]}});
    let ts = codegen::schema_to_ts(&s);
    eprintln!("{ts}");
    let elem = ts.strip_suffix("[]").expect("an array type");
    let mut depth = 0i32;
    let mut whole = elem.starts_with('(');
    for (i, c) in elem.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth == 0 && i + 1 < elem.len() {
            whole = false;
        }
    }
    assert!(
        whole,
        "the array brackets bind only to the last union member: {ts}"
    );
}

/// A schema that shares sub-schemas without recursing is rendered in time linear in its size.
#[test]
fn schema_to_ts_shared_refs_are_not_exponential() {
    let mut defs = serde_json::Map::new();
    let n = 18;
    for i in 0..n {
        let next = json!({"$ref": format!("#/$defs/D{}", i + 1)});
        defs.insert(
            format!("D{i}"),
            json!({"type": "object", "properties": {"a": next.clone(), "b": next}}),
        );
    }
    defs.insert(format!("D{n}"), json!({"type": "string"}));
    let s = json!({"$ref": "#/$defs/D0", "$defs": defs});
    let t = Instant::now();
    let out = codegen::schema_to_ts(&s);
    let took = t.elapsed();
    eprintln!("depth {n}: {took:?}, {} bytes", out.len());
    assert!(
        out.len() < 100_000 && took.as_millis() < 500,
        "{} bytes in {took:?} for a {n}-level schema",
        out.len()
    );
}

#[test]
fn schema_to_ts_never_panics_on_odd_schemas() {
    for s in [
        json!(null),
        json!(1),
        json!("x"),
        json!([]),
        json!({"$ref": 3}),
        json!({"$ref": "#/nope"}),
        json!({"$ref": "http://x/y"}),
        json!({"$ref": "#/$defs/A", "$defs": {"A": {"$ref": "#/$defs/B"}, "B": {"$ref": "#/$defs/A"}}}),
        json!({"type": ["string", 1, null]}),
        json!({"type": "array", "minItems": 3, "maxItems": 3, "items": {"type": "string"}}),
        json!({"type": "array", "minItems": 0, "maxItems": 0, "items": {"type": "string"}}),
        json!({"type": "array", "minItems": u64::MAX, "maxItems": u64::MAX, "items": {"type": "string"}}),
        json!({"enum": []}),
        json!({"anyOf": []}),
        json!({"allOf": []}),
        json!({"properties": {"a": true, "b": false}}),
        json!({"type": "object", "properties": {}, "additionalProperties": {"type": "string"}}),
        json!({"const": {"a": 1}}),
        json!({"const": "a\u{2028}\"b"}),
        json!({"type": "object", "required": "x"}),
        json!({"items": [{"type": "string"}]}),
    ] {
        let _ = codegen::schema_to_ts(&s);
        let mut m = MethodInfo::new("m");
        m.args_schema = Some(s.clone());
        m.returns_schema = Some(s);
        let _ = codegen::typescript([&m], &JsOptions::default());
        let _ = codegen::markdown([&m], "t");
    }
    // deep inline nesting (JSON-parsed schemas are capped at 128 levels by serde_json)
    let mut s = json!({"type": "string"});
    for _ in 0..120 {
        s = json!({"type": "array", "items": s});
    }
    let _ = codegen::schema_to_ts(&s);
}

/// A global name that is a TypeScript reserved word is not emitted as `declare const class:`.
#[test]
fn typescript_global_reserved_word() {
    let ts = codegen::typescript(&infos(&["a"]), &JsOptions::new("class"));
    assert!(
        !ts.contains("declare const class:"),
        "invalid TypeScript:\n{ts}"
    );
}

#[test]
fn markdown_survives_hostile_text() {
    let mut m = MethodInfo::new("a|b`c\nd");
    m.summary = "line1\n\n# Heading\n| x | y |".into();
    m.args = Some("{a |b`}\n- injected".into());
    m.tags = vec!["t`ag".into(), "".into()];
    let md = codegen::markdown([&m], "T");
    for line in md.lines() {
        assert!(!line.starts_with("- injected"), "{md}");
    }
    eprintln!("{md}");
}

#[test]
fn js_client_text_is_safe_for_adversarial_names() {
    let names = [
        "a\"b'c\\d",
        "line\nbreak",
        "</script><script>alert(1)",
        "__proto__",
        "__proto__.x",
        "constructor",
        "prototype",
        "delete",
        "class",
        "1abc",
        "é.ünï",
        "a..b",
        ".lead",
        "trail.",
        "files",
        "files.read",
        "u\u{2028}\u{2029}",
        "*/ evil() /*",
        "new",
        "get",
        "async",
    ];
    let mut ms = infos(&names);
    for m in &mut ms {
        m.summary = "sum */ evil(); /* \u{2028} \r\n x*\n/ y".into();
    }
    let js = codegen::js_client(&ms, &JsOptions::new("api"));
    // Raw newlines from names must never appear inside a string literal (they would be a
    // syntax error on older engines); serde_json escapes \n but passes U+2028 through, which
    // is valid since ES2019.
    for line in js.lines() {
        assert_eq!(line.matches('"').count() % 2, 0, "{line}");
    }
    eprintln!("{js}");
}

#[test]
fn js_client_global_proto_is_not_assigned_directly() {
    let js = codegen::js_client(&infos(&["a"]), &JsOptions::new("__proto__"));
    assert!(
        !js.contains("globalThis.__proto__ ="),
        "`globalThis.__proto__ = {{...}}` replaces the global object's prototype instead of defining a global:\n{js}"
    );
}

// ---------------------------------------------------------------------------
// Send / Sync / lifetimes (compile-time)
// ---------------------------------------------------------------------------

#[test]
fn router_is_send_sync_for_non_send_state_and_calls_are_send_for_send_state() {
    use std::rc::Rc;
    fn ss<T: Send + Sync>() {}
    ss::<Router<Rc<u8>>>();
    fn send<T: Send>(_: T) {}
    let r = Router::<Vec<u8>>::new();
    let mut st = Vec::new();
    send(r.call(&mut st, "x", b""));
}

// ---------------------------------------------------------------------------
// CBOR
// ---------------------------------------------------------------------------

#[cfg(feature = "cbor")]
mod cbor {
    use super::*;
    use codelib_rpc::Cbor;

    fn router() -> Router<(), Cbor> {
        let mut r = Router::<(), Cbor>::new();
        r.method("u8", |_: &mut (), a: u8| Ok(a));
        r.method("unit", |_: &mut (), _: ()| Ok(1u8));
        r.method("req", |_: &mut (), a: Req2| Ok(a.id));
        r.method("opt", |_: &mut (), a: Option<Req2>| Ok(a.map(|r| r.id)));
        r.method("vec", |_: &mut (), a: Vec<u32>| Ok(a.len()));
        r.method("val", |_: &mut (), _: Value| Ok(1u8));
        r
    }

    #[derive(Deserialize)]
    struct Req2 {
        id: u32,
    }

    fn call(r: &Router<(), Cbor>, m: &str, b: &[u8]) -> Result<Vec<u8>, Failure> {
        r.call_sync(&mut (), m, b)
    }

    /// Bytes after the first CBOR value are rejected, as the JSON codec rejects trailing
    /// garbage.
    #[test]
    fn cbor_trailing_bytes_are_rejected() {
        let r = router();
        let e = call(&r, "u8", &[0x01, 0xff, 0xff, 0xff]);
        assert!(
            e.is_err(),
            "trailing bytes after the CBOR value were accepted"
        );
    }

    #[test]
    fn cbor_empty_args_rule() {
        let r = router();
        for b in [&[][..], &[0xf6], &[0xf7], &[0xa0]] {
            assert!(call(&r, "unit", b).is_ok(), "{b:?}");
            assert_eq!(
                call(&r, "req", b).unwrap_err().code,
                codes::BAD_ARGS,
                "{b:?}"
            );
            assert_eq!(
                call(&r, "vec", b).unwrap_err().code,
                codes::BAD_ARGS,
                "{b:?}"
            );
            assert_eq!(
                call(&r, "u8", b).unwrap_err().code,
                codes::BAD_ARGS,
                "{b:?}"
            );
            assert!(call(&r, "opt", b).is_ok());
        }
        // Indefinite-length empty map is the same value but is not recognised as empty.
        let indefinite = [0xbf, 0xff];
        eprintln!(
            "unit with indefinite empty map: {:?}",
            call(&r, "unit", &indefinite)
        );
    }

    #[test]
    fn cbor_hostile_input_does_not_panic_or_overallocate() {
        let r = router();
        let mut deep = vec![0x81u8; 1_000_000];
        deep.push(0x00);
        assert!(call(&r, "val", &deep).is_err());
        assert!(call(&r, "unit", &deep).is_err());
        // Claimed lengths of 2^63 elements / bytes with no data behind them.
        for head in [0x9bu8, 0x5b, 0x7b, 0xbb] {
            let mut b = vec![head];
            b.extend_from_slice(&[0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
            assert!(call(&r, "vec", &b).is_err());
            assert!(call(&r, "val", &b).is_err());
        }
        // Truncated and reserved encodings.
        for b in [
            &[0x18][..],
            &[0x1c],
            &[0xff],
            &[0x9f],
            &[0xc0],
            &[0x5f, 0x01],
        ] {
            let _ = call(&r, "val", b);
            let _ = call(&r, "unit", b);
        }
    }

    #[test]
    fn cbor_round_trip_methods_builtin() {
        let r = router();
        let out = call(&r, "methods", &[]).unwrap();
        let list: Vec<MethodInfo> = ciborium::from_reader(&out[..]).unwrap();
        assert_eq!(list.len(), 7);
    }
}

// ---------------------------------------------------------------------------
// schemars (feature `schema`)
// ---------------------------------------------------------------------------

#[cfg(feature = "schema")]
mod schema {
    use super::*;
    use schemars::JsonSchema;

    #[derive(Deserialize, serde::Serialize, JsonSchema)]
    struct Tree {
        kids: Vec<Tree>,
        next: Option<Box<Tree>>,
    }

    #[derive(Deserialize, serde::Serialize, JsonSchema)]
    #[serde(tag = "t")]
    enum Expr {
        Num { v: f64 },
        Add { l: Box<Expr>, r: Box<Expr> },
    }

    #[derive(Deserialize, serde::Serialize, JsonSchema)]
    struct Even(Option<Box<Odd>>);
    #[derive(Deserialize, serde::Serialize, JsonSchema)]
    struct Odd(Option<Box<Even>>);

    #[derive(Deserialize, serde::Serialize, JsonSchema)]
    struct Gen<T> {
        v: T,
        m: std::collections::BTreeMap<String, Vec<T>>,
        t: (u8, String),
    }

    #[test]
    fn recursive_and_unusual_schemas_render_without_panicking() {
        let mut r = Router::<()>::new();
        r.method("tree", |_: &mut (), a: Tree| Ok(a)).schema();
        r.method("expr", |_: &mut (), a: Expr| Ok(a)).schema();
        r.method("even", |_: &mut (), a: Even| Ok(a)).schema();
        r.method("gen", |_: &mut (), a: Gen<Tree>| Ok(a)).schema();
        r.method("unit", |_: &mut (), a: ()| Ok(a)).schema();
        r.method("val", |_: &mut (), a: Value| Ok(a)).schema();
        let ts = codegen::typescript(r.methods(), &JsOptions::new("api").with_promise(true));
        let md = codegen::markdown(r.methods(), "t");
        eprintln!("{ts}\n{md}");
        assert!(ts.contains("tree"));
    }
}
