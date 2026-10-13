//! The part of TOML the flag file and the profile files are written in.
//!
//! xtask takes nothing from crates.io (`Cargo.toml` says why), so it reads
//! `tools/common/data/build-flags.toml` and the profiles with this: tables
//! and dotted tables, `key = value` with bare, quoted and dotted keys, and
//! values that are strings, integers, booleans, arrays (over several lines,
//! with comments and a trailing comma) and inline tables. Anything else --
//! an array of tables, a date, a float, a multi-line string -- is refused
//! with its line, never guessed at. Python's `tomllib` reads the same files
//! for the gate (`tools/common/check/check-boot-line.py`), so a file that two
//! parsers read differently would show as a disagreement there.

use crate::{Error, Result};

/// A value, with a table's keys in the order the file gave them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    /// A basic or literal string.
    Str(String),
    /// A decimal integer.
    Int(i64),
    /// `true` or `false`.
    Bool(bool),
    /// An array.
    Array(Vec<Value>),
    /// A table: a `[header]`'s, a dotted key's or an inline one.
    Table(Vec<(String, Value)>),
}

impl Value {
    /// The value under `key`, when this is a table that has one.
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Table(entries) => entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// The string, when this is one.
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(text) => Some(text),
            _ => None,
        }
    }

    /// A table's entries, in order; none for anything else.
    pub(crate) fn entries(&self) -> &[(String, Value)] {
        match self {
            Value::Table(entries) => entries,
            _ => &[],
        }
    }

    /// An array of strings, when this is one.
    pub(crate) fn strings(&self) -> Option<Vec<String>> {
        match self {
            Value::Array(items) => items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect(),
            _ => None,
        }
    }
}

/// Parse `text`, named `name` in errors, into its root table.
pub(crate) fn parse(name: &str, text: &str) -> Result<Value> {
    let mut parser = Parser {
        name,
        chars: text.chars().collect(),
        at: 0,
        line: 1,
    };
    let mut root = Value::Table(Vec::new());
    let mut current: Vec<String> = Vec::new();
    loop {
        parser.skip_blank_lines();
        let Some(next) = parser.peek() else {
            return Ok(root);
        };
        if next == '[' {
            let _ = parser.bump();
            if parser.peek() == Some('[') {
                return Err(parser.error("an array of tables is not used in these files"));
            }
            current = parser.key()?;
            parser.expect(']')?;
            let _ = table_at(&mut root, &current, true).map_err(|why| parser.error(&why))?;
        } else {
            let key = parser.key()?;
            parser.expect('=')?;
            let value = parser.value()?;
            let Some((last, path)) = key.split_last() else {
                return Err(parser.error("a key is empty"));
            };
            let mut full = current.clone();
            full.extend_from_slice(path);
            let table = table_at(&mut root, &full, false).map_err(|why| parser.error(&why))?;
            insert(table, last, value).map_err(|why| parser.error(&why))?;
        }
        parser.end_of_line()?;
    }
}

/// The table at `path` under `root`, made when missing. A `[header]` naming a
/// table that holds anything already defined by another header is refused,
/// as TOML refuses it.
fn table_at<'a>(
    root: &'a mut Value,
    path: &[String],
    header: bool,
) -> std::result::Result<&'a mut Vec<(String, Value)>, String> {
    let Value::Table(entries) = root else {
        return Err("the root is not a table".to_owned());
    };
    let mut table = entries;
    for segment in path {
        let index = match table.iter().position(|(name, _)| name == segment) {
            Some(index) => index,
            None => {
                table.push((segment.clone(), Value::Table(Vec::new())));
                table.len() - 1
            }
        };
        table = match table.get_mut(index) {
            Some((_, Value::Table(entries))) => entries,
            _ => return Err(format!("`{segment}` is defined twice, once not as a table")),
        };
    }
    if header
        && table
            .iter()
            .any(|(_, value)| !matches!(value, Value::Table(_)))
    {
        return Err(format!("the table `{}` is defined twice", path.join(".")));
    }
    Ok(table)
}

/// Add `key = value` to `table`, refusing a key it already has.
fn insert(
    table: &mut Vec<(String, Value)>,
    key: &str,
    value: Value,
) -> std::result::Result<(), String> {
    if table.iter().any(|(name, _)| name == key) {
        return Err(format!("the key `{key}` is defined twice"));
    }
    table.push((key.to_owned(), value));
    Ok(())
}

/// Where the reader is.
struct Parser<'a> {
    /// The file's name, for errors.
    name: &'a str,
    /// Its characters.
    chars: Vec<char>,
    /// The next one's index.
    at: usize,
    /// Its line, from 1.
    line: usize,
}

impl Parser<'_> {
    fn error(&self, why: &str) -> Error {
        Error::new(format!("{}:{}: {why}", self.name, self.line))
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let next = self.peek()?;
        self.at += 1;
        if next == '\n' {
            self.line += 1;
        }
        Some(next)
    }

    /// Spaces and tabs.
    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            let _ = self.bump();
        }
    }

    /// Spaces, tabs, newlines and comments: what may come between entries,
    /// and inside an array.
    fn skip_blank_lines(&mut self) {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r' | '\n') => {
                    let _ = self.bump();
                }
                Some('#') => self.skip_comment(),
                _ => return,
            }
        }
    }

    fn skip_comment(&mut self) {
        while self.peek().is_some_and(|next| next != '\n') {
            let _ = self.bump();
        }
    }

    fn expect(&mut self, wanted: char) -> Result<()> {
        self.skip_spaces();
        if self.bump() == Some(wanted) {
            Ok(())
        } else {
            Err(self.error(&format!("`{wanted}` expected")))
        }
    }

    /// Nothing but a comment between an entry and the end of its line.
    fn end_of_line(&mut self) -> Result<()> {
        self.skip_spaces();
        if self.peek() == Some('#') {
            self.skip_comment();
        }
        match self.peek() {
            None | Some('\n') => Ok(()),
            Some('\r') => {
                let _ = self.bump();
                Ok(())
            }
            Some(other) => Err(self.error(&format!("`{other}` after an entry"))),
        }
    }

    /// A dotted key, each part bare or quoted.
    fn key(&mut self) -> Result<Vec<String>> {
        let mut parts = Vec::new();
        loop {
            self.skip_spaces();
            let part = match self.peek() {
                Some('"') => self.basic_string()?,
                Some('\'') => self.literal_string()?,
                _ => self.bare_key()?,
            };
            parts.push(part);
            self.skip_spaces();
            if self.peek() == Some('.') {
                let _ = self.bump();
            } else {
                return Ok(parts);
            }
        }
    }

    fn bare_key(&mut self) -> Result<String> {
        let mut key = String::new();
        while let Some(next) = self.peek() {
            if next.is_ascii_alphanumeric() || next == '_' || next == '-' {
                key.push(next);
                let _ = self.bump();
            } else {
                break;
            }
        }
        if key.is_empty() {
            return Err(self.error("a key expected"));
        }
        Ok(key)
    }

    fn value(&mut self) -> Result<Value> {
        self.skip_spaces();
        match self.peek() {
            Some('"') => self.basic_string().map(Value::Str),
            Some('\'') => self.literal_string().map(Value::Str),
            Some('[') => self.array(),
            Some('{') => self.inline_table(),
            Some('t' | 'f') => self.boolean(),
            Some(next) if next == '-' || next.is_ascii_digit() => self.integer(),
            _ => Err(self.error("a value expected")),
        }
    }

    fn basic_string(&mut self) -> Result<String> {
        let _ = self.bump();
        if self.peek() == Some('"') && self.chars.get(self.at + 1) == Some(&'"') {
            return Err(self.error("a multi-line string is not used in these files"));
        }
        let mut text = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => return Err(self.error("a string is not closed")),
                Some('"') => return Ok(text),
                Some('\\') => text.push(self.escape()?),
                Some(other) => text.push(other),
            }
        }
    }

    fn escape(&mut self) -> Result<char> {
        match self.bump() {
            Some('"') => Ok('"'),
            Some('\\') => Ok('\\'),
            Some('n') => Ok('\n'),
            Some('t') => Ok('\t'),
            Some('u') => {
                let mut code = 0_u32;
                for _ in 0..4 {
                    let digit = self
                        .bump()
                        .and_then(|digit| digit.to_digit(16))
                        .ok_or_else(|| self.error("`\\u` wants four hex digits"))?;
                    code = code * 16 + digit;
                }
                char::from_u32(code).ok_or_else(|| self.error("`\\u` names no character"))
            }
            _ => Err(self.error("an escape these files do not use")),
        }
    }

    fn literal_string(&mut self) -> Result<String> {
        let _ = self.bump();
        let mut text = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => return Err(self.error("a string is not closed")),
                Some('\'') => return Ok(text),
                Some(other) => text.push(other),
            }
        }
    }

    fn boolean(&mut self) -> Result<Value> {
        for (word, value) in [("true", true), ("false", false)] {
            let end = self.at + word.len();
            if self
                .chars
                .get(self.at..end)
                .is_some_and(|got| got.iter().copied().eq(word.chars()))
            {
                self.at = end;
                return Ok(Value::Bool(value));
            }
        }
        Err(self.error("a value expected"))
    }

    fn integer(&mut self) -> Result<Value> {
        let mut digits = String::new();
        if self.peek() == Some('-') {
            digits.push('-');
            let _ = self.bump();
        }
        while let Some(next) = self.peek() {
            if next.is_ascii_digit() {
                digits.push(next);
            } else if next != '_' {
                break;
            }
            let _ = self.bump();
        }
        if matches!(self.peek(), Some('.' | 'e' | 'E' | ':' | 'x' | 'o' | 'b')) {
            return Err(self.error("only decimal integers are used in these files"));
        }
        digits
            .parse()
            .map(Value::Int)
            .map_err(|_| self.error("an integer expected"))
    }

    fn array(&mut self) -> Result<Value> {
        let _ = self.bump();
        let mut items = Vec::new();
        loop {
            self.skip_blank_lines();
            if self.peek() == Some(']') {
                let _ = self.bump();
                return Ok(Value::Array(items));
            }
            items.push(self.value()?);
            self.skip_blank_lines();
            match self.bump() {
                Some(',') => {}
                Some(']') => return Ok(Value::Array(items)),
                _ => return Err(self.error("`,` or `]` expected in an array")),
            }
        }
    }

    fn inline_table(&mut self) -> Result<Value> {
        let _ = self.bump();
        let mut table = Value::Table(Vec::new());
        self.skip_spaces();
        if self.peek() == Some('}') {
            let _ = self.bump();
            return Ok(table);
        }
        loop {
            let key = self.key()?;
            self.expect('=')?;
            let value = self.value()?;
            let Some((last, path)) = key.split_last() else {
                return Err(self.error("a key is empty"));
            };
            let entries = table_at(&mut table, path, false).map_err(|why| self.error(&why))?;
            insert(entries, last, value).map_err(|why| self.error(&why))?;
            self.skip_spaces();
            match self.bump() {
                Some(',') => {}
                Some('}') => return Ok(table),
                _ => return Err(self.error("`,` or `}` expected in an inline table")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Value, parse};

    #[test]
    fn tables_dotted_keys_arrays_and_inline_tables() {
        let text = r#"
# a comment
name = "x" # trailing
count = 3
[a.b]
list = [
    "one", # first
    "two",
]
c.d = { e = true, "f.g" = 'lit' }
[a."quoted key"]
z = -1_000
"#;
        let root = parse("t", text).unwrap();
        assert_eq!(root.get("name"), Some(&Value::Str("x".into())));
        assert_eq!(root.get("count"), Some(&Value::Int(3)));
        let b = root.get("a").and_then(|a| a.get("b")).unwrap();
        assert_eq!(
            b.get("list").and_then(Value::strings),
            Some(vec!["one".to_owned(), "two".to_owned()])
        );
        let d = b.get("c").and_then(|c| c.get("d")).unwrap();
        assert_eq!(d.get("e"), Some(&Value::Bool(true)));
        assert_eq!(d.get("f.g"), Some(&Value::Str("lit".into())));
        let quoted = root.get("a").and_then(|a| a.get("quoted key")).unwrap();
        assert_eq!(quoted.get("z"), Some(&Value::Int(-1000)));
    }

    #[test]
    fn what_these_files_do_not_use_is_refused() {
        assert!(parse("t", "a = 1\na = 2\n").is_err());
        assert!(parse("t", "[[x]]\n").is_err());
        assert!(parse("t", "a = 1.5\n").is_err());
        assert!(parse("t", "a = \"open\n").is_err());
        assert!(parse("t", "a = 1 b = 2\n").is_err());
        assert!(parse("t", "[x]\na = 1\n[x]\nb = 2\n").is_err());
    }

    #[test]
    fn the_flag_file_and_every_profile_parse() {
        let data = crate::paths::workspace_root().join("tools/common/data");
        let mut files = vec![data.join("build-flags.toml")];
        for entry in std::fs::read_dir(data.join("profiles")).unwrap() {
            files.push(entry.unwrap().path());
        }
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            assert!(
                parse(&file.display().to_string(), &text).is_ok(),
                "{}",
                file.display()
            );
        }
    }
}
