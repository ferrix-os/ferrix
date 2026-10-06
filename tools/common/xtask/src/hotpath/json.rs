//! JSON for hot-path records: a value, its canonical text, and a parser.
//!
//! A hardware fingerprint is named by the SHA-256 of its text, so two
//! machines that describe the same hardware must write the same bytes, and
//! one machine must write the same bytes every time. The canonical form
//! (`docs/HOTPATHS.md` §5) is what makes that so:
//!
//! - object keys in byte order, each once (a [`BTreeMap`]);
//! - no white space between tokens;
//! - whole numbers only: no fractions, no exponents, no `-0`, so a number
//!   has exactly one spelling (a ratio is kept in thousandths);
//! - strings escaped only where JSON must (`"`, `\` and the control
//!   characters, the short forms where JSON has one, else `\u00XX` in lower
//!   case), everything else as its UTF-8.
//!
//! That is RFC 8785 restricted to integers, where the two agree. The parser
//! takes any JSON within those limits, in any key order and spacing, so a
//! record read back and written again is the same text.
//!
//! Here rather than from crates.io for the reason `Cargo.toml` gives.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A parse result: the value, or where and why the text is refused.
type Result<T> = core::result::Result<T, String>;

/// One JSON value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A whole number.
    Int(i64),
    /// A string.
    Str(String),
    /// An array.
    List(Vec<Value>),
    /// An object, its keys in byte order.
    Object(BTreeMap<String, Value>),
}

impl Value {
    /// An object from `(key, value)` pairs; a later key replaces an earlier.
    pub(crate) fn object<K: Into<String>>(pairs: impl IntoIterator<Item = (K, Value)>) -> Value {
        Value::Object(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// A string value.
    pub(crate) fn str(text: impl Into<String>) -> Value {
        Value::Str(text.into())
    }

    /// A whole number, or `null` when it does not fit.
    pub(crate) fn int(number: impl TryInto<i64>) -> Value {
        number.try_into().map_or(Value::Null, Value::Int)
    }

    /// The member `key` of an object.
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(members) => members.get(key),
            _ => None,
        }
    }

    /// This as a string.
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(text) => Some(text),
            _ => None,
        }
    }

    /// This as a whole number.
    pub(crate) fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(number) => Some(*number),
            _ => None,
        }
    }

    /// The canonical text: what a hash is taken over.
    pub(crate) fn canonical(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, None, 0);
        out
    }

    /// The same value laid out for a person and a diff: two spaces an
    /// indent, one member a line, keys in the canonical order, a newline at
    /// the end. Parsing it gives back the same value, and so the same
    /// canonical text.
    pub(crate) fn pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, Some(2), 0);
        out.push('\n');
        out
    }

    /// Write this to `out`, indented by `indent` spaces a level if given.
    fn write(&self, out: &mut String, indent: Option<usize>, depth: usize) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
            Value::Int(number) => {
                let _ = write!(out, "{number}");
            }
            Value::Str(text) => write_string(out, text),
            Value::List(items) => {
                write_sequence(out, ('[', ']'), items.iter().map(|v| (None, v)), indent, depth);
            }
            Value::Object(members) => {
                let pairs = members.iter().map(|(k, v)| (Some(k.as_str()), v));
                write_sequence(out, ('{', '}'), pairs, indent, depth);
            }
        }
    }

    /// Parse JSON text, which must hold one value and nothing after it.
    ///
    /// # Errors
    ///
    /// Text that is not JSON, or JSON outside the canonical form's limits: a
    /// number with a fraction or an exponent, one that does not fit 64 bits,
    /// or an object that names a key twice.
    pub(crate) fn parse(text: &str) -> Result<Value> {
        let mut parser = Parser {
            bytes: text.as_bytes(),
            at: 0,
            depth: 0,
        };
        let value = parser.value()?;
        parser.space();
        if parser.at != parser.bytes.len() {
            return Err(format!("byte {}: text after the value", parser.at));
        }
        Ok(value)
    }
}

/// An array's or an object's members, between `brackets`.
fn write_sequence<'a>(
    out: &mut String,
    brackets: (char, char),
    members: impl Iterator<Item = (Option<&'a str>, &'a Value)>,
    indent: Option<usize>,
    depth: usize,
) {
    out.push(brackets.0);
    let mut any = false;
    for (index, (key, value)) in members.enumerate() {
        any = true;
        if index > 0 {
            out.push(',');
        }
        if let Some(width) = indent {
            out.push('\n');
            out.push_str(&" ".repeat(width * (depth + 1)));
        }
        if let Some(key) = key {
            write_string(out, key);
            out.push(':');
            if indent.is_some() {
                out.push(' ');
            }
        }
        value.write(out, indent, depth + 1);
    }
    if let (Some(width), true) = (indent, any) {
        out.push('\n');
        out.push_str(&" ".repeat(width * depth));
    }
    out.push(brackets.1);
}

/// A string in quotes, escaped only where JSON must.
fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            control if u32::from(control) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// How deep arrays and objects may nest: a record is four or five levels,
/// and a limit keeps a malformed file from exhausting the stack.
const MAX_DEPTH: usize = 64;

/// A recursive-descent reader over the text's bytes.
struct Parser<'a> {
    /// The text.
    bytes: &'a [u8],
    /// The next byte to read.
    at: usize,
    /// How many arrays and objects are open.
    depth: usize,
}

impl Parser<'_> {
    /// The byte at the cursor.
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    /// Skip white space.
    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.at += 1;
        }
    }

    /// An error at the cursor.
    fn error(&self, what: &str) -> String {
        format!("byte {}: {what}", self.at)
    }

    /// Take `word` at the cursor.
    fn word(&mut self, word: &str, value: Value) -> Result<Value> {
        if self.bytes.get(self.at..self.at + word.len()) == Some(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(self.error("not a JSON value"))
        }
    }

    /// One value, after any white space.
    fn value(&mut self) -> Result<Value> {
        self.space();
        match self.peek() {
            Some(b'n') => self.word("null", Value::Null),
            Some(b't') => self.word("true", Value::Bool(true)),
            Some(b'f') => self.word("false", Value::Bool(false)),
            Some(b'"') => self.string().map(Value::Str),
            Some(b'[') => self.nested(Self::list),
            Some(b'{') => self.nested(Self::members),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("not a JSON value")),
            None => Err(self.error("the text ends where a value should be")),
        }
    }

    /// An array or an object, within [`MAX_DEPTH`].
    fn nested(
        &mut self,
        read: fn(&mut Self) -> Result<Value>,
    ) -> Result<Value> {
        if self.depth == MAX_DEPTH {
            return Err(self.error("nested too deep"));
        }
        self.depth += 1;
        let value = read(self);
        self.depth -= 1;
        value
    }

    /// An array; the cursor is on its `[`.
    fn list(&mut self) -> Result<Value> {
        self.at += 1;
        let mut items = Vec::new();
        self.space();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(Value::List(items));
        }
        loop {
            items.push(self.value()?);
            self.space();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(Value::List(items));
                }
                _ => return Err(self.error("an array wants `,` or `]`")),
            }
        }
    }

    /// An object; the cursor is on its `{`.
    fn members(&mut self) -> Result<Value> {
        self.at += 1;
        let mut members = BTreeMap::new();
        self.space();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(Value::Object(members));
        }
        loop {
            self.space();
            if self.peek() != Some(b'"') {
                return Err(self.error("an object's key is a string"));
            }
            let key = self.string()?;
            self.space();
            if self.peek() != Some(b':') {
                return Err(self.error("a key wants `:`"));
            }
            self.at += 1;
            let value = self.value()?;
            if members.insert(key.clone(), value).is_some() {
                return Err(self.error(&format!("the key `{key}` twice")));
            }
            self.space();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Value::Object(members));
                }
                _ => return Err(self.error("an object wants `,` or `}`")),
            }
        }
    }

    /// A whole number; fractions and exponents are refused.
    fn number(&mut self) -> Result<Value> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        let digits = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        if self.at == digits {
            return Err(self.error("a number wants digits"));
        }
        if self.bytes.get(digits) == Some(&b'0') && self.at > digits + 1 {
            return Err(self.error("a number with a leading zero"));
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return Err(self.error("only whole numbers: the canonical form has no fractions"));
        }
        let text = std::str::from_utf8(self.bytes.get(start..self.at).unwrap_or_default())
            .map_err(|_| self.error("not UTF-8"))?;
        if text == "-0" {
            return Err(self.error("`-0`: the canonical form spells zero `0`"));
        }
        text.parse()
            .map(Value::Int)
            .map_err(|_| self.error("a number that does not fit 64 bits"))
    }

    /// A string; the cursor is on its opening quote.
    fn string(&mut self) -> Result<String> {
        self.at += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.error("a string that never ends"));
            };
            self.at += 1;
            match byte {
                b'"' => break,
                b'\\' => self.escape(&mut out)?,
                0..0x20 => return Err(self.error("a control character inside a string")),
                other => out.push(other),
            }
        }
        String::from_utf8(out).map_err(|_| self.error("a string that is not UTF-8"))
    }

    /// One escape, after its backslash.
    fn escape(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let Some(byte) = self.peek() else {
            return Err(self.error("a string that never ends"));
        };
        self.at += 1;
        let character = match byte {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'u' => self.unicode()?,
            _ => return Err(self.error("an unknown escape")),
        };
        let mut buffer = [0; 4];
        out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
        Ok(())
    }

    /// `\uXXXX`, and its low half when it is a high surrogate.
    fn unicode(&mut self) -> Result<char> {
        let high = self.hex4()?;
        let code = if (0xD800..0xDC00).contains(&high) {
            if self.bytes.get(self.at..self.at + 2) != Some(b"\\u") {
                return Err(self.error("a high surrogate alone"));
            }
            self.at += 2;
            let low = self.hex4()?;
            if !(0xDC00..0xE000).contains(&low) {
                return Err(self.error("a high surrogate without its low half"));
            }
            0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
        } else {
            high
        };
        char::from_u32(code).ok_or_else(|| self.error("not a character"))
    }

    /// Four hex digits.
    fn hex4(&mut self) -> Result<u32> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| u32::from_str_radix(digits, 16).ok())
            .ok_or_else(|| self.error("`\\u` wants four hex digits"))?;
        self.at += 4;
        Ok(digits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_sort_and_space_goes() {
        let value = Value::parse(r#" { "b" : 1, "a" : [ true, null, "x" ] } "#).unwrap();
        assert_eq!(value.canonical(), r#"{"a":[true,null,"x"],"b":1}"#);
    }

    #[test]
    fn two_spellings_of_one_value_hash_alike() {
        let one = Value::parse(r#"{"z":{"q":-5,"p":"é"},"a":[]}"#).unwrap();
        let two = Value::parse("{\n  \"a\": [ ],\n  \"z\": {\"p\": \"é\", \"q\": -5}\n}").unwrap();
        assert_eq!(one, two, "the same value parsed from two layouts");
        assert_eq!(one.canonical(), two.canonical(), "and so one canonical text");
    }

    #[test]
    fn pretty_reads_back_as_the_same_value() {
        let value = Value::object([
            ("list", Value::List(vec![Value::int(1), Value::str("two")])),
            ("empty", Value::object::<&str>([])),
            ("nested", Value::object([("k", Value::Bool(false))])),
        ]);
        let pretty = value.pretty();
        assert!(pretty.ends_with("}\n"), "a newline at the end: {pretty}");
        assert_eq!(Value::parse(&pretty).unwrap(), value, "pretty reads back");
    }

    #[test]
    fn strings_escape_only_what_json_must() {
        let value = Value::str("a\"b\\c\nd\u{1}é/");
        assert_eq!(value.canonical(), "\"a\\\"b\\\\c\\nd\\u0001é/\"");
        assert_eq!(Value::parse(&value.canonical()).unwrap(), value, "round trip");
        assert_eq!(
            Value::parse(r#""😀""#).unwrap(),
            Value::str("\u{1F600}"),
            "a surrogate pair"
        );
    }

    #[test]
    fn numbers_have_one_spelling() {
        for refused in ["1.5", "1e3", "-0", "01", "99999999999999999999", "-"] {
            assert!(Value::parse(refused).is_err(), "{refused} refused");
        }
        assert_eq!(Value::parse("-12").unwrap(), Value::Int(-12));
        assert_eq!(Value::parse("0").unwrap().canonical(), "0");
    }

    #[test]
    fn malformed_text_is_refused() {
        for refused in [
            "",
            "{",
            "[1,]",
            r#"{"a":1,}"#,
            r#"{"a":1,"a":2}"#,
            r#"{"a" 1}"#,
            "[1] x",
            "\"\u{1}\"",
            r#""\x""#,
            r#""\ud83d""#,
            "nul",
        ] {
            assert!(Value::parse(refused).is_err(), "{refused:?} refused");
        }
        let deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
        assert!(Value::parse(&deep).is_err(), "nesting past the limit refused");
    }
}
