//! The CBOR codec (feature `cbor`).
#![cfg(feature = "cbor")]

use codelib_rpc::{Cbor, Codec, Failure, MethodInfo, Router, codes};
use pollster::block_on;
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Frame {
    #[serde(with = "serde_bytes_compat")]
    bytes: Vec<u8>,
    port: u16,
    note: Option<String>,
}

/// Bytes as a CBOR byte string rather than an array of integers.
mod serde_bytes_compat {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let v = ciborium::Value::deserialize(d)?;
        v.into_bytes()
            .map_err(|_| serde::de::Error::custom("expected a byte string"))
    }
}

#[derive(Default, Deserialize)]
struct Opts {
    #[serde(default)]
    upper: bool,
}

fn router() -> Router<u32, Cbor> {
    let mut r = Router::<u32, Cbor>::new();
    r.method("frame.fix", |calls: &mut u32, mut f: Frame| {
        *calls += 1;
        f.bytes.reverse();
        f.port += 1;
        Ok(f)
    });
    r.method_async("hello", hello);
    r
}

async fn hello(calls: &mut u32, opts: Opts) -> Result<String, Failure> {
    *calls += 1;
    Ok(if opts.upper { "HELLO" } else { "hello" }.to_owned())
}

#[test]
fn cbor_round_trip() {
    let r = router();
    let mut calls = 0;
    let frame = Frame {
        bytes: vec![1, 2, 3, 0xff],
        port: 80,
        note: None,
    };
    let args = Cbor::encode(&frame).unwrap();
    // A byte string (major type 2) is on the wire, not an array.
    assert!(args.windows(5).any(|w| w == [0x44, 1, 2, 3, 0xff]));
    let out = r.call_sync(&mut calls, "frame.fix", &args).unwrap();
    let back: Frame = Cbor::decode(&out).unwrap();
    assert_eq!(
        back,
        Frame {
            bytes: vec![0xff, 3, 2, 1],
            port: 81,
            note: None
        }
    );
    assert_eq!(calls, 1);
}

#[test]
fn cbor_empty_args_and_errors() {
    let r = router();
    let mut calls = 0;
    for empty in [vec![], vec![0xf6], vec![0xf7], vec![0xa0]] {
        let out = block_on(r.call(&mut calls, "hello", &empty)).unwrap();
        assert_eq!(Cbor::decode::<String>(&out).unwrap(), "hello", "{empty:?}");
    }
    #[derive(Serialize)]
    struct Upper {
        upper: bool,
    }
    let out = block_on(r.call(
        &mut calls,
        "hello",
        &Cbor::encode(&Upper { upper: true }).unwrap(),
    ))
    .unwrap();
    assert_eq!(Cbor::decode::<String>(&out).unwrap(), "HELLO");

    let err = r.call_sync(&mut calls, "frame.fix", &[0xf6]).unwrap_err();
    assert_eq!(err.code, codes::BAD_ARGS);
    let err = r.call_sync(&mut calls, "frame.fx", &[]).unwrap_err();
    assert_eq!(err.code, codes::UNKNOWN_METHOD);
    assert!(err.hint.unwrap().contains("`frame.fix`"));

    // The built-in `methods` answers in CBOR too.
    let out = r.call_sync(&mut calls, "methods", &[]).unwrap();
    let list: Vec<MethodInfo> = Cbor::decode(&out).unwrap();
    assert_eq!(list.len(), 3);
    assert!(list[1].is_async);

    // A failure encodes in CBOR as well.
    let f = Failure::denied("no").with_detail(serde_json::json!({"a": [1, 2]}));
    let back: Failure = Cbor::decode(&Cbor::encode(&f).unwrap()).unwrap();
    assert_eq!(back, f);
}
