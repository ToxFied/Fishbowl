//! Row encoding: how a row of values becomes the bytes stored in a leaf.
//!
//! ```text
//! column count (u16), then for each value a type tag and its bytes:
//!   0  NULL      no bytes
//!   1  INTEGER   8 bytes, big-endian two's complement
//!   2  REAL      8 bytes, big-endian IEEE 754
//!   3  TEXT      length (u32), then that many UTF-8 bytes
//! ```
//!
//! A row `(7, NULL, 'hi')` is 19 bytes:
//! `00 03 | 01 00 00 00 00 00 00 00 07 | 00 | 03 00 00 00 02 68 69`.

use crate::error::{Error, Result};
use crate::storage::Row;
use crate::value::Value;

const NULL: u8 = 0;
const INTEGER: u8 = 1;
const REAL: u8 = 2;
const TEXT: u8 = 3;

pub fn encode(row: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + row.len() * 9);
    out.extend_from_slice(&(row.len() as u16).to_be_bytes());
    for value in row {
        match value {
            Value::Null => out.push(NULL),
            Value::Integer(i) => {
                out.push(INTEGER);
                out.extend_from_slice(&i.to_be_bytes());
            }
            Value::Real(f) => {
                out.push(REAL);
                out.extend_from_slice(&f.to_be_bytes());
            }
            Value::Text(s) => {
                out.push(TEXT);
                out.extend_from_slice(&(s.len() as u32).to_be_bytes());
                out.extend_from_slice(s.as_bytes());
            }
        }
    }
    out
}

pub fn decode(bytes: &[u8]) -> Result<Row> {
    let mut rest = bytes;
    let count = u16::from_be_bytes(take(&mut rest)?) as usize;
    let mut row = Vec::with_capacity(count);
    for _ in 0..count {
        let [tag] = take(&mut rest)?;
        row.push(match tag {
            NULL => Value::Null,
            INTEGER => Value::Integer(i64::from_be_bytes(take(&mut rest)?)),
            REAL => Value::Real(f64::from_be_bytes(take(&mut rest)?)),
            TEXT => {
                let len = u32::from_be_bytes(take(&mut rest)?) as usize;
                let text = rest
                    .get(..len)
                    .ok_or_else(|| corrupt("text runs past the end"))?;
                rest = &rest[len..];
                Value::Text(
                    String::from_utf8(text.to_vec()).map_err(|_| corrupt("text is not UTF-8"))?,
                )
            }
            other => return Err(corrupt(&format!("unknown type tag {other}"))),
        });
    }
    if !rest.is_empty() {
        return Err(corrupt("bytes left over after the last value"));
    }
    Ok(row)
}

/// Takes the next `N` bytes off the front of `rest`.
fn take<const N: usize>(rest: &mut &[u8]) -> Result<[u8; N]> {
    let (head, tail) = rest
        .split_first_chunk::<N>()
        .ok_or_else(|| corrupt("a value runs past the end"))?;
    *rest = tail;
    Ok(*head)
}

fn corrupt(what: &str) -> Error {
    Error::Storage(format!("corrupt row: {what}"))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn encoding_matches_the_documented_example() {
        let row = vec![Value::Integer(7), Value::Null, Value::Text("hi".into())];
        let bytes = encode(&row);
        assert_eq!(
            bytes,
            [
                0, 3, 1, 0, 0, 0, 0, 0, 0, 0, 7, 0, 3, 0, 0, 0, 2, b'h', b'i'
            ]
        );
        assert_eq!(decode(&bytes).unwrap(), row);
    }

    #[test]
    fn truncated_rows_are_errors() {
        let bytes = encode(&[Value::Text("hello".into())]);
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len]).is_err(), "prefix of {len} bytes");
        }
    }

    fn value() -> impl Strategy<Value = Value> {
        prop_oneof![
            Just(Value::Null),
            any::<i64>().prop_map(Value::Integer),
            // NaN never equals itself, and SQL has no NaN values to store.
            any::<f64>()
                .prop_filter("not NaN", |f| !f.is_nan())
                .prop_map(Value::Real),
            ".{0,40}".prop_map(Value::Text),
        ]
    }

    proptest! {
        #[test]
        fn any_row_survives_encoding(row in prop::collection::vec(value(), 0..12)) {
            prop_assert_eq!(decode(&encode(&row)).unwrap(), row);
        }
    }
}
