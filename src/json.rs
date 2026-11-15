//! Utilities for parsing JSON with comments (JSONC) and deserializing to Rust types.
//!
//! This module provides convenient wrappers around `jsonc_parser` and `serde_json`
//! to reduce boilerplate and improve ergonomics when working with JSONC files.

use std::io::{self, Write};

use anyhow::Context as _;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::ser::{Formatter, PrettyFormatter};

const INTEGER_TOLERANCE: f64 = 0.000001;

/// An absolute tolerance avoids discarding meaningful fractions at large magnitudes.
pub(crate) fn rounded_integer(value: f64) -> Option<f64> {
    let rounded = value.round();
    (value.is_finite() && (value - rounded).abs() <= INTEGER_TOLERANCE)
        .then_some(if rounded == 0.0 { 0.0 } else { rounded })
}

struct FileFormatter(PrettyFormatter<'static>);

impl Formatter for FileFormatter {
    fn write_f32<W: ?Sized + Write>(&mut self, writer: &mut W, value: f32) -> io::Result<()> {
        if let Some(integer) = rounded_integer(f64::from(value)) {
            write!(writer, "{integer:.0}")
        } else {
            self.0.write_f32(writer, value)
        }
    }

    fn write_f64<W: ?Sized + Write>(&mut self, writer: &mut W, value: f64) -> io::Result<()> {
        if let Some(integer) = rounded_integer(value) {
            write!(writer, "{integer:.0}")
        } else {
            self.0.write_f64(writer, value)
        }
    }

    fn begin_array<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_array(writer)
    }

    fn end_array<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_array(writer)
    }

    fn begin_array_value<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.0.begin_array_value(writer, first)
    }

    fn end_array_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_array_value(writer)
    }

    fn begin_object<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_object(writer)
    }

    fn end_object<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_object(writer)
    }

    fn begin_object_key<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.0.begin_object_key(writer, first)
    }

    fn begin_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_object_value(writer)
    }

    fn end_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_object_value(writer)
    }
}

/// Serializes a value as pretty JSON using four-space indentation and a
/// trailing newline, which is the default format for files written by Rojo.
pub fn to_vec_pretty<T: Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    let mut output = Vec::new();
    let formatter = FileFormatter(PrettyFormatter::with_indent(b"    "));
    let mut serializer = serde_json::Serializer::with_formatter(&mut output, formatter);
    value.serialize(&mut serializer)?;
    output.push(b'\n');
    Ok(output)
}

/// Serializes a value as pretty JSON using Rojo's default file format.
pub fn to_string_pretty<T: Serialize>(value: &T) -> serde_json::Result<String> {
    Ok(String::from_utf8(to_vec_pretty(value)?).expect("JSON should always be valid UTF-8"))
}

/// Parse JSONC text into a `serde_json::Value`.
///
/// This handles the common pattern of calling `jsonc_parser::parse_to_serde_value`
/// and unwrapping the `Option` with a clear error message.
///
/// # Errors
///
/// Returns an error if:
/// - The text is not valid JSONC
/// - The text contains no JSON value
pub fn parse_value(text: &str) -> anyhow::Result<serde_json::Value> {
    jsonc_parser::parse_to_serde_value(text, &Default::default())
        .context("Failed to parse JSONC")?
        .ok_or_else(|| anyhow::anyhow!("File contains no JSON value"))
}

/// Parse JSONC text into a `serde_json::Value` with a custom context message.
///
/// This is useful when you want to provide a specific error message that includes
/// additional information like the file path.
///
/// # Errors
///
/// Returns an error if:
/// - The text is not valid JSONC
/// - The text contains no JSON value
pub fn parse_value_with_context(
    text: &str,
    context: impl Fn() -> String,
) -> anyhow::Result<serde_json::Value> {
    jsonc_parser::parse_to_serde_value(text, &Default::default())
        .with_context(|| format!("{}: JSONC parse error", context()))?
        .ok_or_else(|| anyhow::anyhow!("{}: File contains no JSON value", context()))
}

/// Parse JSONC text and deserialize it into a specific type.
///
/// This combines parsing JSONC and deserializing into a single operation,
/// eliminating the need to manually chain `parse_to_serde_value` and `from_value`.
///
/// # Errors
///
/// Returns an error if:
/// - The text is not valid JSONC
/// - The text contains no JSON value
/// - The value cannot be deserialized into type `T`
pub fn from_str<T: DeserializeOwned>(text: &str) -> anyhow::Result<T> {
    let value = parse_value(text)?;
    serde_json::from_value(value).context("Failed to deserialize JSON")
}

/// Parse JSONC text and deserialize it into a specific type with a custom context message.
///
/// This is useful when you want to provide a specific error message that includes
/// additional information like the file path.
///
/// # Errors
///
/// Returns an error if:
/// - The text is not valid JSONC
/// - The text contains no JSON value
/// - The value cannot be deserialized into type `T`
pub fn from_str_with_context<T: DeserializeOwned>(
    text: &str,
    context: impl Fn() -> String,
) -> anyhow::Result<T> {
    let value = parse_value_with_context(text, &context)?;
    serde_json::from_value(value).with_context(|| format!("{}: Invalid JSON structure", context()))
}

/// Parse JSONC bytes into a `serde_json::Value` with a custom context message.
///
/// This handles UTF-8 conversion and JSONC parsing in one step.
///
/// # Errors
///
/// Returns an error if:
/// - The bytes are not valid UTF-8
/// - The text is not valid JSONC
/// - The text contains no JSON value
pub fn parse_value_from_slice_with_context(
    slice: &[u8],
    context: impl Fn() -> String,
) -> anyhow::Result<serde_json::Value> {
    let text = std::str::from_utf8(slice)
        .with_context(|| format!("{}: File is not valid UTF-8", context()))?;
    parse_value_with_context(text, context)
}

/// Parse JSONC bytes and deserialize it into a specific type.
///
/// This handles UTF-8 conversion, JSONC parsing, and deserialization in one step.
///
/// # Errors
///
/// Returns an error if:
/// - The bytes are not valid UTF-8
/// - The text is not valid JSONC
/// - The text contains no JSON value
/// - The value cannot be deserialized into type `T`
pub fn from_slice<T: DeserializeOwned>(slice: &[u8]) -> anyhow::Result<T> {
    let text = std::str::from_utf8(slice).context("File is not valid UTF-8")?;
    from_str(text)
}

/// Parse JSONC bytes and deserialize it into a specific type with a custom context message.
///
/// This handles UTF-8 conversion, JSONC parsing, and deserialization in one step.
///
/// # Errors
///
/// Returns an error if:
/// - The bytes are not valid UTF-8
/// - The text is not valid JSONC
/// - The text contains no JSON value
/// - The value cannot be deserialized into type `T`
pub fn from_slice_with_context<T: DeserializeOwned>(
    slice: &[u8],
    context: impl Fn() -> String,
) -> anyhow::Result<T> {
    let text = std::str::from_utf8(slice)
        .with_context(|| format!("{}: File is not valid UTF-8", context()))?;
    from_str_with_context(text, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[test]
    fn pretty_json_uses_four_spaces_and_a_trailing_newline() {
        let value = serde_json::json!({"outer": {"inner": true}});
        let text = to_string_pretty(&value).unwrap();
        assert_eq!(
            text,
            "{\n    \"outer\": {\n        \"inner\": true\n    }\n}\n"
        );
        assert_eq!(to_vec_pretty(&value).unwrap(), text.as_bytes());
    }

    #[test]
    fn floats_near_integers_are_normalized() {
        for (input, expected) in [
            (2.0, "2"),
            (-2.0, "-2"),
            (-0.0, "0"),
            (1.0000005, "1"),
            (0.9999995, "1"),
            (-1.0000005, "-1"),
            (-0.9999995, "-1"),
            (0.000001, "0"),
            (-0.000001, "0"),
            (1.000002, "1.000002"),
            (0.999998, "0.999998"),
            (0.000002, "2e-6"),
            (0.25, "0.25"),
            (1_000_000_000.25, "1000000000.25"),
        ] {
            let expected = format!("{expected}\n");
            let actual = to_string_pretty(&input).unwrap();
            assert_eq!(actual, expected, "{input}");
            assert_eq!(to_string_pretty(&input).unwrap(), actual);
            let parsed: f64 = from_str(&actual).unwrap();
            assert_eq!(to_string_pretty(&parsed).unwrap(), actual);
        }
        assert_eq!(to_string_pretty(&2.0_f32).unwrap(), "2\n");
        assert_eq!(to_string_pretty(&1.0000005_f32).unwrap(), "1\n");
        assert_eq!(to_string_pretty(&1.000002_f32).unwrap(), "1.000002\n");
        assert_eq!(to_string_pretty(&0.6_f32).unwrap(), "0.6\n");
    }

    #[test]
    fn numeric_formatting_preserves_other_json_values_and_layout() {
        let value = serde_json::json!({
            "array": [1.0, {"Float64": -2.0000005}, [], {}],
            "integer": u64::MAX,
            "text": "1.0\n\"quoted\"",
        });
        assert_eq!(
            to_string_pretty(&value).unwrap(),
            concat!(
                "{\n",
                "    \"array\": [\n",
                "        1,\n",
                "        {\n",
                "            \"Float64\": -2\n",
                "        },\n",
                "        [],\n",
                "        {}\n",
                "    ],\n",
                "    \"integer\": 18446744073709551615,\n",
                "    \"text\": \"1.0\\n\\\"quoted\\\"\"\n",
                "}\n",
            )
        );
        assert_eq!(
            to_string_pretty(&i64::MIN).unwrap(),
            "-9223372036854775808\n"
        );
        assert_eq!(to_string_pretty(&f64::INFINITY).unwrap(), "null\n");
        assert_eq!(to_string_pretty(&f64::NAN).unwrap(), "null\n");
    }

    #[test]
    fn large_integral_floats_do_not_overflow_integer_casts() {
        for value in [f64::MAX, f64::MIN, 1e20, -1e20, f64::from(f32::MAX)] {
            let text = to_string_pretty(&value).unwrap();
            assert!(!text.contains('.'));
            assert_eq!(from_str::<f64>(&text).unwrap(), value);
            assert_eq!(
                to_string_pretty(&from_str::<f64>(&text).unwrap()).unwrap(),
                text
            );
        }
    }

    /// Metadata keeps the text of unchanged numbers by parsing and writing
    /// them again, so a parse that's off by one bit changes the file on every
    /// syncback. These came from real CFrames whose text alternated forever.
    #[test]
    fn numbers_round_trip_through_their_text() {
        for text in [
            "3.7252898543727042e-9",
            "3.725289854372704e-9",
            "-6.630593851753017e-10",
            "-6.630593851753016e-10",
        ] {
            let number: f64 = from_str(text).unwrap();
            assert_eq!(number, text.parse::<f64>().unwrap(), "{text}");
            assert_eq!(serde_json::to_string(&number).unwrap(), text, "{text}");
        }
    }

    #[test]
    fn test_parse_value() {
        let value = parse_value(r#"{"foo": "bar"}"#).unwrap();
        assert_eq!(value["foo"], "bar");
    }

    #[test]
    fn test_parse_value_with_comments() {
        let value = parse_value(
            r#"{
            // This is a comment
            "foo": "bar" // Inline comment
        }"#,
        )
        .unwrap();
        assert_eq!(value["foo"], "bar");
    }

    #[test]
    fn test_parse_value_with_trailing_comma() {
        let value = parse_value(
            r#"{
            "foo": "bar",
            "baz": 123,
        }"#,
        )
        .unwrap();
        assert_eq!(value["foo"], "bar");
        assert_eq!(value["baz"], 123);
    }

    #[test]
    fn test_parse_value_empty() {
        let err = parse_value("").unwrap_err();
        assert!(err.to_string().contains("no JSON value"));
    }

    #[test]
    fn test_parse_value_invalid() {
        let err = parse_value("{invalid}").unwrap_err();
        assert!(err.to_string().contains("parse"));
    }

    #[test]
    fn test_parse_value_with_context() {
        let err = parse_value_with_context("{invalid}", || "test.json".to_string()).unwrap_err();
        assert!(err.to_string().contains("test.json"));
        assert!(err.to_string().contains("parse"));
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct TestStruct {
        foo: String,
        bar: i32,
    }

    #[test]
    fn test_from_str() {
        let result: TestStruct = from_str(r#"{"foo": "hello", "bar": 42}"#).unwrap();
        assert_eq!(
            result,
            TestStruct {
                foo: "hello".to_string(),
                bar: 42
            }
        );
    }

    #[test]
    fn test_from_str_with_comments() {
        let result: TestStruct = from_str(
            r#"{
            // Comment
            "foo": "hello",
            "bar": 42, // Trailing comma is fine
        }"#,
        )
        .unwrap();
        assert_eq!(
            result,
            TestStruct {
                foo: "hello".to_string(),
                bar: 42
            }
        );
    }

    #[test]
    fn test_from_str_invalid_type() {
        let err = from_str::<TestStruct>(r#"{"foo": "hello"}"#).unwrap_err();
        assert!(err.to_string().contains("deserialize"));
    }

    #[test]
    fn test_from_str_with_context() {
        let err = from_str_with_context::<TestStruct>(r#"{"foo": "hello"}"#, || {
            "config.json".to_string()
        })
        .unwrap_err();
        assert!(err.to_string().contains("config.json"));
        assert!(err.to_string().contains("Invalid JSON structure"));
    }

    #[test]
    fn test_parse_value_from_slice_with_context() {
        let err = parse_value_from_slice_with_context(b"{invalid}", || "test.json".to_string())
            .unwrap_err();
        assert!(err.to_string().contains("test.json"));
        assert!(err.to_string().contains("parse"));
    }

    #[test]
    fn test_parse_value_from_slice_with_context_invalid_utf8() {
        let err = parse_value_from_slice_with_context(&[0xFF, 0xFF], || "test.json".to_string())
            .unwrap_err();
        assert!(err.to_string().contains("test.json"));
        assert!(err.to_string().contains("UTF-8"));
    }

    #[test]
    fn test_from_slice() {
        let result: TestStruct = from_slice(br#"{"foo": "hello", "bar": 42}"#).unwrap();
        assert_eq!(
            result,
            TestStruct {
                foo: "hello".to_string(),
                bar: 42
            }
        );
    }

    #[test]
    fn test_from_slice_with_comments() {
        let result: TestStruct = from_slice(
            br#"{
            // Comment
            "foo": "hello",
            "bar": 42, // Trailing comma is fine
        }"#,
        )
        .unwrap();
        assert_eq!(
            result,
            TestStruct {
                foo: "hello".to_string(),
                bar: 42
            }
        );
    }

    #[test]
    fn test_from_slice_invalid_utf8() {
        let err = from_slice::<TestStruct>(&[0xFF, 0xFF]).unwrap_err();
        assert!(err.to_string().contains("UTF-8"));
    }

    #[test]
    fn test_from_slice_with_context() {
        let err = from_slice_with_context::<TestStruct>(br#"{"foo": "hello"}"#, || {
            "config.json".to_string()
        })
        .unwrap_err();
        assert!(err.to_string().contains("config.json"));
        assert!(err.to_string().contains("Invalid JSON structure"));
    }

    #[test]
    fn test_from_slice_with_context_invalid_utf8() {
        let err =
            from_slice_with_context::<TestStruct>(&[0xFF, 0xFF], || "config.json".to_string())
                .unwrap_err();
        assert!(err.to_string().contains("config.json"));
        assert!(err.to_string().contains("UTF-8"));
    }
}
