//! Serialise JSON byte-for-byte like Python's `json.dumps(value, indent=2)`:
//! two-space indent, `", "`-free newlines, and `ensure_ascii=True` escaping
//! (every non-ASCII char becomes `\uXXXX`, astral chars as surrogate pairs).

use serde::Serialize;
use serde_json::ser::{Formatter, PrettyFormatter, Serializer};
use serde_json::Value;
use std::io;

struct PyFormatter<'a>(PrettyFormatter<'a>);

impl Formatter for PyFormatter<'_> {
    fn begin_array<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.0.begin_array(w)
    }
    fn end_array<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.0.end_array(w)
    }
    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.0.begin_array_value(w, first)
    }
    fn end_array_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.0.end_array_value(w)
    }
    fn begin_object<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.0.begin_object(w)
    }
    fn end_object<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.0.end_object(w)
    }
    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.0.begin_object_key(w, first)
    }
    fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.0.begin_object_value(w)
    }
    fn end_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.0.end_object_value(w)
    }

    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let mut buf = [0u16; 2];
        for ch in fragment.chars() {
            if ch.is_ascii() {
                w.write_all(&[ch as u8])?;
            } else {
                for unit in ch.encode_utf16(&mut buf) {
                    write!(w, "\\u{unit:04x}")?;
                }
            }
        }
        Ok(())
    }
}

/// Pretty-print `value` the way Python's `json.dumps(value, indent=2)` does.
pub fn to_string_pretty<T: Serialize>(value: &T) -> String {
    let mut out = Vec::new();
    let fmt = PyFormatter(PrettyFormatter::with_indent(b"  "));
    let mut ser = Serializer::with_formatter(&mut out, fmt);
    value
        .serialize(&mut ser)
        .expect("serialising a JSON value cannot fail");
    String::from_utf8(out).expect("serde_json emits UTF-8")
}

/// Like `json.dumps(value, indent=2, sort_keys=True)`.
pub fn to_string_pretty_sorted(value: &Value) -> String {
    to_string_pretty(&sorted(value))
}

fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sorted(&map[k])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn indents_two_spaces_and_keeps_key_order() {
        let v = json!({"b": 1, "a": [1, 2]});
        assert_eq!(
            to_string_pretty(&v),
            "{\n  \"b\": 1,\n  \"a\": [\n    1,\n    2\n  ]\n}"
        );
    }

    #[test]
    fn escapes_non_ascii_like_python() {
        assert_eq!(to_string_pretty(&json!("é")), "\"\\u00e9\"");
        assert_eq!(to_string_pretty(&json!("😀")), "\"\\ud83d\\ude00\"");
    }

    #[test]
    fn sorted_orders_keys_recursively() {
        let v = json!({"b": {"y": 1, "x": 2}, "a": [{"d": 1, "c": 2}]});
        assert_eq!(
            to_string_pretty_sorted(&v),
            to_string_pretty(&json!({"a": [{"c": 2, "d": 1}], "b": {"x": 2, "y": 1}}))
        );
    }

    #[test]
    fn empty_containers_are_compact() {
        assert_eq!(to_string_pretty(&json!({})), "{}");
        assert_eq!(to_string_pretty(&json!([])), "[]");
    }
}
