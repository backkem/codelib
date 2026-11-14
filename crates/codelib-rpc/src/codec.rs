//! Wire encodings of arguments and results.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The wire encoding of arguments and results.
///
/// Handlers are typed through serde and never see the encoding; a [`Router`](crate::Router)
/// is fixed to one codec through its second type parameter. All functions are associated
/// functions (no `self`), so a codec is a zero-sized marker type.
pub trait Codec: Send + Sync + 'static {
    /// Short name of the encoding (`"json"`, `"cbor"`).
    const NAME: &'static str;

    /// Decodes a value.
    fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String>;

    /// Encodes a value.
    fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, String>;

    /// Whether `bytes` carry "no arguments": an empty input, the encoding's null, or an empty
    /// map. Drives the empty-args rule (see the crate docs). The default only recognizes empty
    /// input.
    fn is_empty_value(bytes: &[u8]) -> bool {
        bytes.is_empty()
    }
}

/// JSON via `serde_json`. The default codec.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Json;

impl Codec for Json {
    const NAME: &'static str = "json";

    fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
    }

    fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, String> {
        serde_json::to_vec(value).map_err(|e| e.to_string())
    }

    /// Empty or whitespace-only text, `null`, or `{}` (whitespace allowed inside). Whitespace is
    /// what JSON allows: space, tab, LF and CR (not form feed or vertical tab).
    fn is_empty_value(bytes: &[u8]) -> bool {
        let t = trim_json(bytes);
        if t.is_empty() || t == b"null" {
            return true;
        }
        t.len() >= 2
            && t[0] == b'{'
            && t[t.len() - 1] == b'}'
            && trim_json(&t[1..t.len() - 1]).is_empty()
    }
}

/// Trims JSON whitespace only; `trim_ascii` would also strip form feed.
fn trim_json(mut b: &[u8]) -> &[u8] {
    let ws = |c: &u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r');
    while let [first, rest @ ..] = b {
        if !ws(first) {
            break;
        }
        b = rest;
    }
    while let [rest @ .., last] = b {
        if !ws(last) {
            break;
        }
        b = rest;
    }
    b
}

/// CBOR via `ciborium` (feature `cbor`).
#[cfg(feature = "cbor")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cbor;

#[cfg(feature = "cbor")]
impl Codec for Cbor {
    const NAME: &'static str = "cbor";

    fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
        // `from_reader` stops after one value; anything left over is a malformed request.
        let mut rest = bytes;
        let value = ciborium::from_reader(&mut rest).map_err(|e| e.to_string())?;
        if rest.is_empty() {
            Ok(value)
        } else {
            Err(format!(
                "{} trailing byte(s) after the CBOR value",
                rest.len()
            ))
        }
    }

    fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        ciborium::into_writer(value, &mut out).map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// Empty input, `null` (0xf6), `undefined` (0xf7) or an empty map (0xa0).
    fn is_empty_value(bytes: &[u8]) -> bool {
        matches!(bytes, [] | [0xf6] | [0xf7] | [0xa0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_empty_values() {
        for s in ["", "  ", "null", " null\n", "{}", "{ }", " {\n} "] {
            assert!(Json::is_empty_value(s.as_bytes()), "{s:?}");
        }
        for s in ["0", "[]", "{\"a\":1}", "\"\"", "nul"] {
            assert!(!Json::is_empty_value(s.as_bytes()), "{s:?}");
        }
    }

    #[cfg(feature = "cbor")]
    #[test]
    fn cbor_round_trip() {
        let bytes = Cbor::encode(&vec![1u8, 2, 3]).unwrap();
        let back: Vec<u8> = Cbor::decode(&bytes).unwrap();
        assert_eq!(back, vec![1, 2, 3]);
        assert!(Cbor::is_empty_value(&Cbor::encode(&()).unwrap()));
        assert!(Cbor::is_empty_value(&[]));
    }
}
