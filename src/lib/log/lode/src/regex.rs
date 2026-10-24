//! Regular expressions for `-g`/`--grep` (`docs/LOGGING.md` §6, §9.1):
//! found anywhere in the message, in time linear in the message's length.
//!
//! journalctl's `-g` is PCRE2. This is the part of that syntax a person
//! writes at a shell, matched by a Pike VM (Thompson's construction, every
//! thread stepped together), so no pattern can make lode slow, which is why
//! §6 chose Rust's `regex` and why this is not a backtracking matcher:
//!
//! * literals, `\` before punctuation to take it literally, `\n`, `\t`,
//!   `\r`, `\xHH`, `\x{H...}`;
//! * `.` (any character but a newline), classes `[abc]`, `[^a-z]`, with
//!   `\d \w \s` and `[:alpha:]`-style names inside, and `\d \D \w \W \s \S`
//!   outside;
//! * `*`, `+`, `?`, `{n}`, `{n,}`, `{n,m}`, each optionally lazy (`*?`),
//!   which a match that only asks *whether* changes nothing;
//! * groups `( )` and `(?: )`, alternation `|`, `(?i)` at the start;
//! * `^`, `$` (the message's ends) and `\b`, `\B`.
//!
//! What PCRE2 has beyond that -- back-references, look-around, atomic
//! groups, possessive quantifiers -- is refused with the place and the
//! reason, never matched wrongly. Characters are UTF-8 scalars; a byte that
//! is not UTF-8 is one character no class but a negated one matches.
//! Case is folded per character (`char::to_lowercase`'s first), as
//! journalctl's smart case does for a pattern with no upper-case letter.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// The most instructions a pattern compiles to: `{n,m}` copies its atom.
const PROGRAM_MAX: usize = 20_000;

/// Why a pattern was refused, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// The byte of the pattern it was found at.
    pub at: usize,
    /// What is wrong.
    pub why: &'static str,
}

/// A compiled pattern.
#[derive(Debug, Clone)]
pub struct Regex {
    /// The program.
    program: Vec<Inst>,
    /// Whether case is folded.
    fold: bool,
}

/// A set of characters: ranges, and whether the set is their complement.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Class {
    /// Inclusive ranges.
    ranges: Vec<(char, char)>,
    /// Matches what the ranges do not.
    negated: bool,
}

impl Class {
    /// Whether `c` is in the set.
    fn has(&self, c: char) -> bool {
        self.ranges.iter().any(|&(low, high)| low <= c && c <= high) != self.negated
    }
}

/// One step of the program.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Inst {
    /// This character.
    Char(char),
    /// Any character but a newline.
    Any,
    /// A class.
    Class(Class),
    /// Go on at both.
    Split(usize, usize),
    /// Go on there.
    Jump(usize),
    /// The start of the message.
    Start,
    /// The end of the message.
    End,
    /// A word boundary (`\b`), or not one (`\B`).
    Boundary(bool),
    /// A match.
    Match,
}

/// A pattern as parsed.
#[derive(Debug, Clone)]
enum Node {
    /// Nothing: matches the empty string.
    Empty,
    /// One character.
    Char(char),
    /// Any but a newline.
    Any,
    /// A class.
    Class(Class),
    /// An anchor or a boundary.
    Assert(Inst),
    /// One after another.
    Concat(Vec<Node>),
    /// One of them.
    Alternate(Vec<Node>),
    /// `node{min,max}`, `max` `None` for no bound.
    Repeat(Box<Node>, u32, Option<u32>),
}

impl Regex {
    /// Compile `pattern`; with `fold`, case is ignored.
    ///
    /// # Errors
    ///
    /// When the pattern is not one this module matches, with where and why.
    pub fn new(pattern: &str, fold: bool) -> Result<Regex, Error> {
        let (pattern, fold, skipped) = match pattern.strip_prefix("(?i)") {
            Some(rest) => (rest, true, 4),
            None => (pattern, fold, 0),
        };
        let mut parser = Parser {
            text: pattern,
            at: 0,
            skipped,
            fold,
        };
        let node = parser.alternation()?;
        if parser.at < pattern.len() {
            return Err(parser.error("an unmatched `)`"));
        }
        let mut program = Vec::new();
        compile(&node, &mut program).map_err(|why| Error { at: 0, why })?;
        program.push(Inst::Match);
        Ok(Regex { program, fold })
    }

    /// Whether the pattern matches anywhere in `text`.
    #[must_use]
    pub fn is_match(&self, text: &[u8]) -> bool {
        let chars = decode(text, self.fold);
        let mut current = Threads::new(self.program.len());
        let mut next = Threads::new(self.program.len());
        let length = chars.len();
        for at in 0..=length {
            // Unanchored: a thread begins at every position.
            self.add(&mut current, 0, at, &chars);
            if current.has_match {
                return true;
            }
            let Some(&c) = chars.get(at) else {
                break;
            };
            next.clear();
            for index in 0..current.list.len() {
                let pc = current.list.get(index).copied().unwrap_or(0);
                let takes = match self.program.get(pc) {
                    Some(Inst::Char(want)) => *want == c,
                    Some(Inst::Any) => c != '\n',
                    Some(Inst::Class(class)) => class.has(c),
                    _ => false,
                };
                if takes {
                    self.add(&mut next, pc + 1, at + 1, &chars);
                }
            }
            core::mem::swap(&mut current, &mut next);
            if current.has_match {
                return true;
            }
        }
        current.has_match
    }

    /// Add the thread at `pc` to `threads`, following every step that
    /// takes no character, at position `at` of `chars`.
    fn add(&self, threads: &mut Threads, pc: usize, at: usize, chars: &[char]) {
        let mut stack = alloc::vec![pc];
        while let Some(pc) = stack.pop() {
            if !threads.mark(pc) {
                continue;
            }
            match self.program.get(pc) {
                Some(Inst::Split(first, second)) => {
                    stack.push(*second);
                    stack.push(*first);
                }
                Some(Inst::Jump(to)) => stack.push(*to),
                Some(Inst::Start) => {
                    if at == 0 {
                        stack.push(pc + 1);
                    }
                }
                Some(Inst::End) => {
                    if at == chars.len() {
                        stack.push(pc + 1);
                    }
                }
                Some(Inst::Boundary(wanted)) => {
                    let before = at > 0 && chars.get(at - 1).is_some_and(|&c| is_word(c));
                    let after = chars.get(at).is_some_and(|&c| is_word(c));
                    if (before != after) == *wanted {
                        stack.push(pc + 1);
                    }
                }
                Some(Inst::Match) => threads.has_match = true,
                Some(_) => threads.list.push(pc),
                None => {}
            }
        }
    }
}

/// The threads alive at one position: each program counter once.
struct Threads {
    /// The ones waiting for a character, in order.
    list: Vec<usize>,
    /// Which counters were added at this position.
    seen: Vec<bool>,
    /// Whether one reached a match.
    has_match: bool,
}

impl Threads {
    /// For a program of `size` steps.
    fn new(size: usize) -> Threads {
        Threads {
            list: Vec::new(),
            seen: alloc::vec![false; size],
            has_match: false,
        }
    }

    /// Empty again.
    fn clear(&mut self) {
        self.list.clear();
        self.seen.iter_mut().for_each(|seen| *seen = false);
        self.has_match = false;
    }

    /// Mark `pc` seen; whether it was new.
    fn mark(&mut self, pc: usize) -> bool {
        match self.seen.get_mut(pc) {
            Some(seen) if !*seen => {
                *seen = true;
                true
            }
            _ => false,
        }
    }
}

/// `text` as characters, a byte that is not UTF-8 as U+FFFD, each folded
/// with `fold`.
fn decode(text: &[u8], fold: bool) -> Vec<char> {
    let mut chars = Vec::with_capacity(text.len());
    for chunk in text.utf8_chunks() {
        chars.extend(
            chunk
                .valid()
                .chars()
                .map(|c| if fold { lower(c) } else { c }),
        );
        if !chunk.invalid().is_empty() {
            chars.push(char::REPLACEMENT_CHARACTER);
        }
    }
    chars
}

/// `c` in lower case: the first character of its lower case.
fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// What `\w` and `\b` count as a word's: letters, digits and `_`.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Append `node`'s program to `out`.
fn compile(node: &Node, out: &mut Vec<Inst>) -> Result<(), &'static str> {
    if out.len() > PROGRAM_MAX {
        return Err("the pattern is too large: a count of {n,m} too big");
    }
    match node {
        Node::Empty => {}
        Node::Char(c) => out.push(Inst::Char(*c)),
        Node::Any => out.push(Inst::Any),
        Node::Class(class) => out.push(Inst::Class(class.clone())),
        Node::Assert(inst) => out.push(inst.clone()),
        Node::Concat(nodes) => {
            for each in nodes {
                compile(each, out)?;
            }
        }
        Node::Alternate(nodes) => {
            // split L1, next; L1: a; jump end; next: split L2, ...; last
            let mut jumps = Vec::new();
            for (index, each) in nodes.iter().enumerate() {
                if index + 1 < nodes.len() {
                    let split = out.len();
                    out.push(Inst::Split(split + 1, 0));
                    compile(each, out)?;
                    jumps.push(out.len());
                    out.push(Inst::Jump(0));
                    let next = out.len();
                    if let Some(Inst::Split(_, second)) = out.get_mut(split) {
                        *second = next;
                    }
                } else {
                    compile(each, out)?;
                }
            }
            let end = out.len();
            for jump in jumps {
                if let Some(Inst::Jump(to)) = out.get_mut(jump) {
                    *to = end;
                }
            }
        }
        Node::Repeat(inner, min, max) => {
            for _ in 0..*min {
                compile(inner, out)?;
                if out.len() > PROGRAM_MAX {
                    return Err("the pattern is too large: a count of {n,m} too big");
                }
            }
            match max {
                None => {
                    // L: split body, end; body; jump L
                    let split = out.len();
                    out.push(Inst::Split(split + 1, 0));
                    compile(inner, out)?;
                    out.push(Inst::Jump(split));
                    let end = out.len();
                    if let Some(Inst::Split(_, second)) = out.get_mut(split) {
                        *second = end;
                    }
                }
                Some(max) => {
                    let mut splits = Vec::new();
                    for _ in *min..*max {
                        splits.push(out.len());
                        out.push(Inst::Split(out.len() + 1, 0));
                        compile(inner, out)?;
                        if out.len() > PROGRAM_MAX {
                            return Err("the pattern is too large: a count of {n,m} too big");
                        }
                    }
                    let end = out.len();
                    for split in splits {
                        if let Some(Inst::Split(_, second)) = out.get_mut(split) {
                            *second = end;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// The parser: recursive descent over the pattern's characters.
struct Parser<'a> {
    /// The pattern, after any `(?i)`.
    text: &'a str,
    /// Where it is, in bytes.
    at: usize,
    /// How many bytes were taken off the front, for errors' places.
    skipped: usize,
    /// Whether case is folded.
    fold: bool,
}

impl Parser<'_> {
    /// An error here.
    fn error(&self, why: &'static str) -> Error {
        Error {
            at: self.at + self.skipped,
            why,
        }
    }

    /// The next character, not taken.
    fn peek(&self) -> Option<char> {
        self.text
            .get(self.at..)
            .and_then(|rest| rest.chars().next())
    }

    /// Take the next character.
    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += c.len_utf8();
        Some(c)
    }

    /// Take `c` if it is next.
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.at += c.len_utf8();
            true
        } else {
            false
        }
    }

    /// `a|b|...`.
    fn alternation(&mut self) -> Result<Node, Error> {
        let mut branches = alloc::vec![self.concatenation()?];
        while self.eat('|') {
            branches.push(self.concatenation()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap_or(Node::Empty)
        } else {
            Node::Alternate(branches)
        })
    }

    /// Pieces one after another, up to `|`, `)` or the end.
    fn concatenation(&mut self) -> Result<Node, Error> {
        let mut nodes = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom()?;
            nodes.push(self.quantified(atom)?);
        }
        Ok(match nodes.len() {
            0 => Node::Empty,
            1 => nodes.pop().unwrap_or(Node::Empty),
            _ => Node::Concat(nodes),
        })
    }

    /// `atom` with what quantifies it.
    fn quantified(&mut self, atom: Node) -> Result<Node, Error> {
        let mut node = atom;
        loop {
            let start = self.at;
            let (min, max) = match self.peek() {
                Some('*') => {
                    self.at += 1;
                    (0, None)
                }
                Some('+') => {
                    self.at += 1;
                    (1, None)
                }
                Some('?') => {
                    self.at += 1;
                    (0, Some(1))
                }
                Some('{') => match self.counted()? {
                    Some(bounds) => bounds,
                    None => {
                        self.at = start;
                        return Ok(node);
                    }
                },
                _ => return Ok(node),
            };
            if matches!(node, Node::Assert(_) | Node::Empty) {
                return Err(self.error("a quantifier after nothing to repeat"));
            }
            if self.eat('+') {
                return Err(self.error("possessive quantifiers are PCRE2's, not lode's"));
            }
            // Lazy or greedy: whether it matches is the same.
            let _ = self.eat('?');
            node = Node::Repeat(Box::new(node), min, max);
        }
    }

    /// `{n}`, `{n,}`, `{n,m}`; `None` when the `{` is a literal (`{x`).
    fn counted(&mut self) -> Result<Option<(u32, Option<u32>)>, Error> {
        let rest = self.text.get(self.at + 1..).unwrap_or_default();
        let Some(close) = rest.find('}') else {
            return Ok(None);
        };
        let inside = rest.get(..close).unwrap_or_default();
        let number = |text: &str| -> Option<u32> {
            (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
                .then(|| text.parse().ok())
                .flatten()
        };
        let bounds = match inside.split_once(',') {
            None => number(inside).map(|n| (n, Some(n))),
            Some((low, "")) => number(low).map(|n| (n, None)),
            Some((low, high)) => number(low).zip(number(high)).map(|(l, h)| (l, Some(h))),
        };
        let Some((min, max)) = bounds else {
            return Ok(None);
        };
        if max.is_some_and(|max| max < min) {
            return Err(self.error("{n,m} with m below n"));
        }
        if min > 1000 || max.is_some_and(|max| max > 1000) {
            return Err(self.error("a count above 1000"));
        }
        self.at += close + 2;
        Ok(Some((min, max)))
    }

    /// One atom.
    fn atom(&mut self) -> Result<Node, Error> {
        let Some(c) = self.next() else {
            return Ok(Node::Empty);
        };
        Ok(match c {
            '.' => Node::Any,
            '^' => Node::Assert(Inst::Start),
            '$' => Node::Assert(Inst::End),
            '(' => {
                if self.eat('?') && !self.eat(':') {
                    return Err(self.error(
                        "only (?:...) groups and a leading (?i) are lode's; look-around and \
                         named groups are PCRE2's",
                    ));
                }
                let inner = self.alternation()?;
                if !self.eat(')') {
                    return Err(self.error("a `(` with no `)`"));
                }
                inner
            }
            '[' => Node::Class(self.class()?),
            '*' | '+' | '?' => return Err(self.error("a quantifier after nothing to repeat")),
            '\\' => self.escape(false)?,
            c => self.literal(c),
        })
    }

    /// One character, folded when case is.
    fn literal(&self, c: char) -> Node {
        Node::Char(if self.fold { lower(c) } else { c })
    }

    /// What follows a `\`: outside a class, `\b` is a boundary.
    fn escape(&mut self, in_class: bool) -> Result<Node, Error> {
        let Some(c) = self.next() else {
            return Err(self.error("a `\\` at the end"));
        };
        let class = |ranges: &[(char, char)], negated: bool| {
            Node::Class(Class {
                ranges: ranges.to_vec(),
                negated,
            })
        };
        Ok(match c {
            'd' => class(DIGIT, false),
            'D' => class(DIGIT, true),
            'w' => class(WORD, false),
            'W' => class(WORD, true),
            's' => class(SPACE, false),
            'S' => class(SPACE, true),
            'b' if !in_class => Node::Assert(Inst::Boundary(true)),
            'B' if !in_class => Node::Assert(Inst::Boundary(false)),
            'n' => Node::Char('\n'),
            't' => Node::Char('\t'),
            'r' => Node::Char('\r'),
            'f' => Node::Char('\u{c}'),
            'v' => Node::Char('\u{b}'),
            'e' => Node::Char('\u{1b}'),
            'x' => Node::Char(self.hex()?),
            '1'..='9' => return Err(self.error("back-references are PCRE2's, not lode's")),
            c if c.is_ascii_alphanumeric() => {
                return Err(self.error("an escape lode does not know"));
            }
            c => self.literal(c),
        })
    }

    /// `\xHH` or `\x{H...}`'s character.
    fn hex(&mut self) -> Result<char, Error> {
        let digits = if self.eat('{') {
            let rest = self.text.get(self.at..).unwrap_or_default();
            let close = rest
                .find('}')
                .ok_or_else(|| self.error("a `\\x{` with no `}`"))?;
            let digits = rest.get(..close).unwrap_or_default();
            self.at += close + 1;
            digits
        } else {
            let digits = self.text.get(self.at..self.at + 2).unwrap_or_default();
            self.at += digits.len();
            digits
        };
        u32::from_str_radix(digits, 16)
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(|| self.error("`\\x` needs hex digits that are a character"))
    }

    /// `[...]` after its `[`.
    fn class(&mut self) -> Result<Class, Error> {
        let negated = self.eat('^');
        let mut ranges = Vec::new();
        let mut first = true;
        loop {
            let Some(c) = self.next() else {
                return Err(self.error("a `[` with no `]`"));
            };
            if c == ']' && !first {
                break;
            }
            first = false;
            let low =
                match c {
                    '[' if self.peek() == Some(':') => {
                        let rest = self.text.get(self.at..).unwrap_or_default();
                        let close = rest
                            .find(":]")
                            .ok_or_else(|| self.error("a `[:` with no `:]`"))?;
                        let name = rest.get(1..close).unwrap_or_default();
                        ranges.extend_from_slice(named(name).ok_or_else(|| {
                            self.error("a character class name lode does not know")
                        })?);
                        self.at += close + 2;
                        continue;
                    }
                    '\\' => match self.escape(true)? {
                        Node::Char(c) => c,
                        Node::Class(class) if !class.negated => {
                            ranges.extend(class.ranges);
                            continue;
                        }
                        _ => return Err(self.error("a negated class inside a class")),
                    },
                    c => c,
                };
            let high = if self.peek() == Some('-')
                && self
                    .text
                    .get(self.at + 1..)
                    .is_some_and(|rest| !rest.starts_with(']'))
            {
                self.at += 1;
                match self.next() {
                    Some('\\') => match self.escape(true)? {
                        Node::Char(c) => c,
                        _ => return Err(self.error("a range ending in a class")),
                    },
                    Some(c) => c,
                    None => return Err(self.error("a `[` with no `]`")),
                }
            } else {
                low
            };
            if high < low {
                return Err(self.error("a range whose end is before its start"));
            }
            ranges.push((low, high));
        }
        if self.fold {
            // Each range's letters in lower case too: the text is folded.
            let folded: Vec<(char, char)> = ranges
                .iter()
                .filter(|(low, high)| u32::from(*high) - u32::from(*low) < 0x1000)
                .flat_map(|&(low, high)| (low..=high).map(|c| (lower(c), lower(c))))
                .filter(|(c, _)| !ranges.iter().any(|&(l, h)| l <= *c && *c <= h))
                .collect();
            ranges.extend(folded);
        }
        Ok(Class { ranges, negated })
    }
}

/// `\d`.
const DIGIT: &[(char, char)] = &[('0', '9')];
/// `\w`: ASCII's letters, digits and `_` (`\b` counts every letter).
const WORD: &[(char, char)] = &[('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')];
/// `\s`.
const SPACE: &[(char, char)] = &[('\t', '\r'), (' ', ' ')];

/// A POSIX class's ranges, by its name.
fn named(name: &str) -> Option<&'static [(char, char)]> {
    Some(match name {
        "alpha" => &[('A', 'Z'), ('a', 'z')],
        "digit" => DIGIT,
        "alnum" => &[('0', '9'), ('A', 'Z'), ('a', 'z')],
        "upper" => &[('A', 'Z')],
        "lower" => &[('a', 'z')],
        "space" => SPACE,
        "blank" => &[(' ', ' '), ('\t', '\t')],
        "xdigit" => &[('0', '9'), ('A', 'F'), ('a', 'f')],
        "punct" => &[('!', '/'), (':', '@'), ('[', '`'), ('{', '~')],
        "word" => WORD,
        "cntrl" => &[('\0', '\u{1f}'), ('\u{7f}', '\u{7f}')],
        "print" => &[(' ', '~')],
        "graph" => &[('!', '~')],
        _ => return None,
    })
}

/// Whether `pattern` has an upper-case letter, which turns journalctl's
/// smart case off.
#[must_use]
pub fn has_upper(pattern: &str) -> bool {
    pattern.chars().any(char::is_uppercase)
}

/// `pattern`, for an error's message: where it went wrong marked.
#[must_use]
pub fn show_error(pattern: &str, error: &Error) -> String {
    alloc::format!("{pattern:?}: {} (at byte {})", error.why, error.at)
}
