//! The filter language's core (`docs/LOGGING.md` §6): one expression used
//! by the command line now, and by ingest rules, retention and forwarding as
//! they come.
//!
//! ```text
//! expr  := or
//! or    := and ("or" and)*
//! and   := unary ("and" unary)*
//! unary := ("not" | "!") unary | "(" expr ")" | test
//! test  := field op value
//! op    := "=" | "!=" | "<" | "<=" | ">" | ">=" | "~" | "!~"
//! value := "quoted, with \" and \\" | a bare word
//! ```
//!
//! `=` and `!=` match a glob (`*`, `?`, `[...]`, `[!...]`) against the whole
//! value; `~` and `!~` look for it anywhere in the value (a regular
//! expression's place, which a later step fills). `priority` compares levels
//! by number or name (`priority <= err`). The short names `unit`, `id`,
//! `message`, `pid`, `transport`, `subsystem`, `mark`, `crash` and `rule` stand for the
//! fields they mean; any other name is a field's own, upper case. A test on
//! a field the record does not have is false for `=`, `<`, `>` and `~`, and
//! true for `!=` and `!~`.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::record::{Record, level};

/// A parsed expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// Both.
    And(Box<Expr>, Box<Expr>),
    /// Either.
    Or(Box<Expr>, Box<Expr>),
    /// Not.
    Not(Box<Expr>),
    /// A test.
    Test(Test),
    /// Every record.
    All,
}

/// One comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Test {
    /// What is compared.
    pub field: Selector,
    /// How.
    pub op: Op,
    /// With what.
    pub value: Vec<u8>,
}

/// What a test reads off a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// The level.
    Priority,
    /// The first of these fields the record has.
    Fields(Vec<String>),
}

/// A comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Matches the glob.
    Eq,
    /// Does not.
    Ne,
    /// Less.
    Lt,
    /// Less or equal.
    Le,
    /// Greater.
    Gt,
    /// Greater or equal.
    Ge,
    /// Contains the glob.
    Has,
    /// Does not contain it.
    HasNot,
}

impl Expr {
    /// `self` and `other`, where either may be [`Expr::All`].
    #[must_use]
    pub fn and(self, other: Expr) -> Expr {
        match (self, other) {
            (Expr::All, other) | (other, Expr::All) => other,
            (left, right) => Expr::And(Box::new(left), Box::new(right)),
        }
    }

    /// `self` or `other`.
    #[must_use]
    pub fn or(self, other: Expr) -> Expr {
        Expr::Or(Box::new(self), Box::new(other))
    }

    /// Whether `record` matches.
    #[must_use]
    pub fn matches(&self, record: &Record) -> bool {
        match self {
            Expr::All => true,
            Expr::And(left, right) => left.matches(record) && right.matches(record),
            Expr::Or(left, right) => left.matches(record) || right.matches(record),
            Expr::Not(inner) => !inner.matches(record),
            Expr::Test(test) => test.matches(record),
        }
    }
}

impl Test {
    /// A test that field `name` (or a short name) equals the glob `value`.
    #[must_use]
    pub fn field(name: &str, op: Op, value: &[u8]) -> Test {
        Test {
            field: selector(name),
            op,
            value: Vec::from(value),
        }
    }

    /// Whether `record` passes.
    #[must_use]
    pub fn matches(&self, record: &Record) -> bool {
        match &self.field {
            Selector::Priority => {
                let Some(wanted) = core::str::from_utf8(&self.value).ok().and_then(level) else {
                    return false;
                };
                compare(self.op, record.priority.cmp(&wanted), || {
                    record.priority == wanted
                })
            }
            Selector::Fields(names) => {
                let value = names.iter().find_map(|name| record.get(name));
                let Some(value) = value else {
                    return matches!(self.op, Op::Ne | Op::HasNot);
                };
                match self.op {
                    Op::Eq => glob(&self.value, value),
                    Op::Ne => !glob(&self.value, value),
                    Op::Has => contains(&self.value, value),
                    Op::HasNot => !contains(&self.value, value),
                    op => compare(op, order(value, &self.value), || false),
                }
            }
        }
    }
}

/// `op` applied to an ordering; `equal` decides `=` and `!=` where the
/// ordering is not what they mean.
fn compare(op: Op, ordering: core::cmp::Ordering, equal: impl Fn() -> bool) -> bool {
    use core::cmp::Ordering::{Equal, Greater, Less};
    match op {
        Op::Eq | Op::Has => equal() || ordering == Equal,
        Op::Ne | Op::HasNot => !(equal() || ordering == Equal),
        Op::Lt => ordering == Less,
        Op::Le => ordering != Greater,
        Op::Gt => ordering == Greater,
        Op::Ge => ordering != Less,
    }
}

/// Two values in order: as numbers when both are, else as bytes.
fn order(left: &[u8], right: &[u8]) -> core::cmp::Ordering {
    let number = |bytes: &[u8]| {
        core::str::from_utf8(bytes)
            .ok()
            .and_then(|text| text.trim().parse::<i128>().ok())
    };
    match (number(left), number(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

/// What a name in an expression reads.
fn selector(name: &str) -> Selector {
    let names: &[&str] = match name {
        "priority" | "PRIORITY" | "prio" => return Selector::Priority,
        "unit" => &["_SYSTEMD_UNIT", "UNIT"],
        "id" | "identifier" => &["SYSLOG_IDENTIFIER"],
        "message" | "msg" => &["MESSAGE"],
        "pid" => &["_PID", "SYSLOG_PID"],
        "transport" => &["_TRANSPORT"],
        "subsystem" => &["FERRIX_SUBSYSTEM"],
        "mark" => &["MARK"],
        "crash" => &["CRASH_SIGNAL"],
        "rule" => &["RULE"],
        "driver" => &["_FERRIX_DRIVER"],
        other => {
            return Selector::Fields(alloc::vec![String::from(other)]);
        }
    };
    Selector::Fields(names.iter().map(|name| String::from(*name)).collect())
}

/// Whether the glob `pattern` matches all of `text`: `*`, `?`, `[...]` and
/// `[!...]`, with `\` taking the next byte as it is.
#[must_use]
pub fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        let step = match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, t));
                p += 1;
                continue;
            }
            Some(_) => one(pattern, p, text.get(t).copied().unwrap_or(0)),
            None => None,
        };
        if let Some(next) = step {
            p = next;
            t += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern
        .get(p..)
        .unwrap_or_default()
        .iter()
        .all(|&byte| byte == b'*')
}

/// Whether the glob element at `p` takes `byte`: where the pattern goes on.
fn one(pattern: &[u8], p: usize, byte: u8) -> Option<usize> {
    match pattern.get(p)? {
        b'?' => Some(p + 1),
        b'\\' => (pattern.get(p + 1) == Some(&byte)).then_some(p + 2),
        b'[' => {
            let mut at = p + 1;
            let negated = matches!(pattern.get(at), Some(b'!' | b'^'));
            if negated {
                at += 1;
            }
            let mut hit = false;
            let mut first = true;
            loop {
                let &current = pattern.get(at)?;
                if current == b']' && !first {
                    break;
                }
                first = false;
                if pattern.get(at + 1) == Some(&b'-')
                    && let Some(&high) = pattern.get(at + 2)
                    && high != b']'
                {
                    hit |= (current..=high).contains(&byte);
                    at += 3;
                } else {
                    hit |= current == byte;
                    at += 1;
                }
            }
            (hit != negated).then_some(at + 1)
        }
        &literal => (literal == byte).then_some(p + 1),
    }
}

/// Whether the glob `pattern` matches some part of `text`.
#[must_use]
pub fn contains(pattern: &[u8], text: &[u8]) -> bool {
    let mut wrapped = Vec::with_capacity(pattern.len() + 2);
    wrapped.push(b'*');
    wrapped.extend_from_slice(pattern);
    wrapped.push(b'*');
    glob(&wrapped, text)
}

/// Why an expression did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// Where, in bytes.
    pub at: usize,
    /// What was wrong.
    pub why: &'static str,
}

/// Parse `text`.
///
/// # Errors
///
/// [`ParseError`] for text that is not an expression.
pub fn parse(text: &str) -> Result<Expr, ParseError> {
    let tokens = tokens(text)?;
    let mut parser = Parser { tokens, at: 0 };
    let expr = parser.or()?;
    match parser.tokens.get(parser.at) {
        None => Ok(expr),
        Some((at, _)) => Err(ParseError {
            at: *at,
            why: "more after the expression",
        }),
    }
}

/// A token, and where it began.
type Token = (usize, Lexeme);

/// A token.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Lexeme {
    /// `(`.
    Open,
    /// `)`.
    Close,
    /// An operator.
    Op(Op),
    /// `!` alone: not.
    Bang,
    /// A word or a quoted string.
    Word(Vec<u8>, bool),
}

/// Split `text` into tokens.
fn tokens(text: &str) -> Result<Vec<Token>, ParseError> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        let start = at;
        let two = bytes.get(at..at + 2).unwrap_or_default();
        let (lexeme, length) = match byte {
            b' ' | b'\t' | b'\n' => {
                at += 1;
                continue;
            }
            b'(' => (Lexeme::Open, 1),
            b')' => (Lexeme::Close, 1),
            _ if two == b"!=" => (Lexeme::Op(Op::Ne), 2),
            _ if two == b"!~" => (Lexeme::Op(Op::HasNot), 2),
            _ if two == b"<=" => (Lexeme::Op(Op::Le), 2),
            _ if two == b">=" => (Lexeme::Op(Op::Ge), 2),
            _ if two == b"==" => (Lexeme::Op(Op::Eq), 2),
            b'=' => (Lexeme::Op(Op::Eq), 1),
            b'<' => (Lexeme::Op(Op::Lt), 1),
            b'>' => (Lexeme::Op(Op::Gt), 1),
            b'~' => (Lexeme::Op(Op::Has), 1),
            b'!' => (Lexeme::Bang, 1),
            b'"' | b'\'' => quoted(bytes, at, byte)?,
            _ => {
                let end = bytes
                    .get(at..)
                    .unwrap_or_default()
                    .iter()
                    .position(|&b| {
                        matches!(
                            b,
                            b' ' | b'\t' | b'\n' | b'(' | b')' | b'=' | b'!' | b'<' | b'>' | b'~'
                        )
                    })
                    .map_or(bytes.len(), |end| at + end);
                (
                    Lexeme::Word(Vec::from(bytes.get(at..end).unwrap_or_default()), false),
                    end - at,
                )
            }
        };
        out.push((start, lexeme));
        at += length;
    }
    Ok(out)
}

/// A string in `quote`s from `at`, with `\` taking the next byte.
fn quoted(bytes: &[u8], at: usize, quote: u8) -> Result<(Lexeme, usize), ParseError> {
    let mut value = Vec::new();
    let mut place = at + 1;
    loop {
        match bytes.get(place) {
            None => {
                return Err(ParseError {
                    at,
                    why: "a quoted string that does not end",
                });
            }
            Some(&b'\\') => {
                if let Some(&next) = bytes.get(place + 1) {
                    if next != quote && next != b'\\' {
                        value.push(b'\\');
                    }
                    value.push(next);
                }
                place += 2;
            }
            Some(&byte) if byte == quote => break,
            Some(&byte) => {
                value.push(byte);
                place += 1;
            }
        }
    }
    Ok((Lexeme::Word(value, true), place + 1 - at))
}

/// A recursive-descent parser over tokens.
struct Parser {
    /// The tokens.
    tokens: Vec<Token>,
    /// The next one.
    at: usize,
}

impl Parser {
    /// Where the parser is, for an error.
    fn here(&self) -> usize {
        self.tokens.get(self.at).map_or(usize::MAX, |(at, _)| *at)
    }

    /// Whether the next token is the bare word `word`; takes it if so.
    fn keyword(&mut self, word: &[u8]) -> bool {
        let found = matches!(self.tokens.get(self.at), Some((_, Lexeme::Word(w, false))) if w.as_slice() == word);
        if found {
            self.at += 1;
        }
        found
    }

    /// `or`.
    fn or(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.and()?;
        while self.keyword(b"or") || self.keyword(b"||") {
            left = left.or(self.and()?);
        }
        Ok(left)
    }

    /// `and`.
    fn and(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.unary()?;
        while self.keyword(b"and") || self.keyword(b"&&") {
            left = Expr::And(Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    /// `unary`.
    fn unary(&mut self) -> Result<Expr, ParseError> {
        if self.keyword(b"not") {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        match self.tokens.get(self.at) {
            Some((_, Lexeme::Bang)) => {
                self.at += 1;
                Ok(Expr::Not(Box::new(self.unary()?)))
            }
            Some((_, Lexeme::Open)) => {
                self.at += 1;
                let inner = self.or()?;
                match self.tokens.get(self.at) {
                    Some((_, Lexeme::Close)) => {
                        self.at += 1;
                        Ok(inner)
                    }
                    _ => Err(ParseError {
                        at: self.here(),
                        why: "a `(` with no `)`",
                    }),
                }
            }
            _ => self.test(),
        }
    }

    /// `test`.
    fn test(&mut self) -> Result<Expr, ParseError> {
        let at = self.here();
        let Some((_, Lexeme::Word(name, false))) = self.tokens.get(self.at).cloned() else {
            return Err(ParseError {
                at,
                why: "a field name was expected",
            });
        };
        let Some((_, Lexeme::Op(op))) = self.tokens.get(self.at + 1).cloned() else {
            return Err(ParseError {
                at,
                why: "a comparison was expected after the field name",
            });
        };
        let Some((_, Lexeme::Word(value, _))) = self.tokens.get(self.at + 2).cloned() else {
            return Err(ParseError {
                at,
                why: "a value was expected after the comparison",
            });
        };
        self.at += 3;
        let name = String::from_utf8_lossy(&name).into_owned();
        Ok(Expr::Test(Test::field(&name, op, &value)))
    }
}
