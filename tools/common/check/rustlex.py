#!/usr/bin/env python3
"""Tell Rust code from Rust comments and literals, for the gates that read it.

`check-item-boundary.py` and `check-complexity.py` both measure code, and both
used to find it with the same three regular expressions: `//.*$`, `/\\*.*?\\*/`
and `"(?:[^"\\\\]|\\\\.)*"`. Each is wrong on input the kernel contains:

  * the string pattern cannot cross a `\\`-newline continuation (its `.` does
    not match a newline), so it gives up at the first one and pairs the next
    `"` with the wrong partner -- from there to the end of the file code is
    string and string is code. `main.rs::say_booted` measured 102 lines
    (it has ten) and `register_load`, below it, was never found at all;
  * it starts a string at the `"` inside a char literal `'"'`;
  * block comments nest in Rust and the pattern stops at the first `*/`;
  * a `//` inside a string is not a comment, and a `"` inside a comment does
    not open a string -- but a pattern applied to the whole file cannot know
    which of the two it is inside.

The cure is to scan from the start of the file, deciding at each token start
what kind of token it is, the way the compiler does. That needs no parser: the
lexical grammar is small, and a function that gets it right is shorter than
the list of special cases the regular expressions were accumulating.

`mask(source)` returns the source with every comment blanked and every literal's
contents blanked, *same length, same newlines* -- so an offset into the mask is
an offset into the file, and a line number is a line number. Code is untouched,
lifetimes and labels included. `tokens(source)` splits the mask into what a
path resolver needs: identifiers, `::` and single punctuation.

Not handled, because the kernel has none and the self-test would say if it
grew some: `#[path]`-renamed modules (a gate question, asked in
check-item-boundary.py), and reserved-prefix syntax beyond `b`, `c`, `r`, `br`
and `cr`.

    python3 tools/common/check/rustlex.py --self-test
"""

from __future__ import annotations

import functools
import re
import sys

# Where something other than code may begin. Everything between two matches is
# code and is copied as is. The prefix letters only open a literal at the
# start of a token, hence the look-behind.
_START = re.compile(
    r"""//|/\*|(?<!\w)(?:br|cr|r)\#*"|(?<!\w)[bc]?"|(?<!\w)b'|'"""
)
# The body of an ordinary (non-raw) string, from just after its opening quote
# to its closing one. DOTALL is the whole fix for continuations: `\\.` must be
# able to consume the newline after a backslash.
_STRING_BODY = re.compile(r'(?:[^"\\]|\\.)*"', re.DOTALL)
# A char literal starting at its quote. One code point, or one escape. Tried
# before deciding the quote is a lifetime or a label: `'a'` is a char, `'a` is
# a lifetime, and the difference is whether a quote closes it right away.
_CHAR = re.compile(r"'(?:[^'\\\n]|\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]{1,8}\}|[^\n]))'")
_BLOCK_EDGE = re.compile(r"/\*|\*/")
_NOT_NEWLINE = re.compile(r"[^\n]")
_TOKEN = re.compile(r"(?:r#)?[^\W\d]\w*|\d\w*|::|\S")


class LexError(ValueError):
    """The source is not lexically valid Rust, or this lexer is wrong."""


def _blank(text: str) -> str:
    return _NOT_NEWLINE.sub(" ", text)


def _literal(text: str, delimiter: str) -> str:
    """A literal reduced to its delimiters, same length, same newlines."""
    return delimiter + _blank(text[1:-1]) + delimiter


def spans(source: str):
    """Yield `(kind, start, end)` for every comment and literal, in order.

    Kinds are `comment`, `string` and `char`. Lifetimes and labels are code
    and are not yielded.
    """
    pos = 0
    length = len(source)
    while pos < length:
        match = _START.search(source, pos)
        if match is None:
            return
        start = match.start()
        lead = match.group()

        if lead == "//":
            end = source.find("\n", start)
            end = length if end < 0 else end
            yield "comment", start, end
        elif lead == "/*":
            depth, scan = 0, start
            while True:
                edge = _BLOCK_EDGE.search(source, scan)
                if edge is None:
                    raise LexError(f"unterminated block comment at offset {start}")
                depth += 1 if edge.group() == "/*" else -1
                scan = edge.end()
                if depth == 0:
                    break
            end = scan
            yield "comment", start, end
        elif lead.endswith('"') and lead[0] in "bcr" and ("r" in lead):
            hashes = lead.count("#")
            close = '"' + "#" * hashes
            found = source.find(close, match.end())
            if found < 0:
                raise LexError(f"unterminated raw string at offset {start}")
            end = found + len(close)
            yield "string", start, end
        elif lead.endswith('"'):
            body = _STRING_BODY.match(source, match.end())
            if body is None:
                raise LexError(f"unterminated string at offset {start}")
            end = body.end()
            yield "string", start, end
        else:
            # `'` or `b'`: a char literal if one closes it, else a lifetime or
            # a label, which is code.
            quote = match.end() - 1
            char = _CHAR.match(source, quote)
            if char is not None:
                end = char.end()
                yield "char", start, end
            elif lead == "b'":
                raise LexError(f"malformed byte literal at offset {start}")
            else:
                end = quote + 1
        pos = end


@functools.lru_cache(maxsize=None)
def mask(source: str) -> str:
    """The source with comments and literal contents blanked, length kept.

    Remembered by the source's text: a pure function, and `check-item-boundary.py`
    asks it 11,555 times for 1,062 different sources.
    """
    out: list[str] = []
    pos = 0
    for kind, start, end in spans(source):
        out.append(source[pos:start])
        text = source[start:end]
        if kind == "comment":
            out.append(_blank(text))
        elif kind == "string":
            out.append(_literal(text, '"'))
        else:
            out.append(_literal(text, "'"))
        pos = end
    out.append(source[pos:])
    return "".join(out)


@functools.lru_cache(maxsize=None)
def _tokens(masked: str) -> tuple[tuple[str, int], ...]:
    return tuple((m.group(), m.start()) for m in _TOKEN.finditer(masked))


def tokens(masked: str) -> list[tuple[str, int]]:
    """Identifiers, numbers, `::` and single punctuation, with offsets.

    Takes a *masked* source: run `mask` first, or a comment full of paths
    becomes a dependency. A literal comes out as its two delimiters. A fresh list
    each call, from a remembered result: callers may keep or change theirs.
    """
    return list(_tokens(masked))


# --- self-test ---------------------------------------------------------------
#
# Each case is (name, source, the code that must survive, the text that must
# not). "Survive" is checked on the masked source with runs of whitespace
# collapsed, so the cases read as what a gate would see.

_CASES = [
    (
        "continuation",
        'fn a() {\n    panic!("one \\\n     for two");\n    if x { y }\n}\n',
        ["if x { y }", 'panic!(" '],
        ["for two"],
    ),
    (
        "char literal holding a double quote",
        "let q = '\"'; if a { b } let s = \"x\";",
        ["if a { b }", "let s ="],
        ["x"],
    ),
    (
        "escaped quotes in chars",
        r"let a = '\''; let b = '\\'; let c = '\u{1F600}'; while d {}",
        ["while d {}"],
        ["1F600"],
    ),
    (
        "lifetimes and labels are code",
        "fn f<'a>(x: &'a str) -> &'a str { 'outer: loop { break 'outer; } }",
        ["fn f<'a>(x: &'a str) -> &'a str", "'outer: loop", "break 'outer;"],
        [],
    ),
    (
        "lifetime then a string",
        "fn g<'a>(x: &'a u8) { h(\"for\"); if y {} }",
        ["if y {}", "&'a u8"],
        ["for"],
    ),
    (
        "nested block comments",
        "/* outer /* inner */ still comment for */ if z {}",
        ["if z {}"],
        ["still", "for"],
    ),
    (
        "raw strings",
        'let r = r#"he said "hi" for"#; let s = r"\\"; let t = br##"a"#b"##; if w {}',
        ["if w {}", "let s =", "let t ="],
        ["hi", "for", "a\"#b"],
    ),
    (
        "byte strings, byte chars, C strings",
        "let a = b\"\\x00\\\"for\"; let b = b'\\''; let c = c\"if\"; loop {}",
        ["loop {}", "let b =", "let c ="],
        ["for", "if"],
    ),
    (
        "comment markers inside strings",
        'let u = "http://x/*"; if v {}',
        ["if v {}"],
        ["http"],
    ),
    (
        "quotes inside comments",
        "// don't \"\nif a {}\n/* it's \" */ while b {}",
        ["if a {}", "while b {}"],
        ["don"],
    ),
    (
        "raw identifiers are not raw strings",
        'let r#type = 1; let s = "for";',
        ["let r#type = 1;"],
        ["for"],
    ),
    (
        "doc comments",
        "/// crate::fs::x\n//! crate::net\n/** crate::a /* b */ */ fn f() {}",
        ["fn f() {}"],
        ["crate::"],
    ),
    (
        "a string that spans lines",
        'let s = "line one\nline two"; for i in j {}',
        ["for i in j {}"],
        ["line"],
    ),
]


def self_test() -> list[str]:
    """Run the cases; return a list of failures, empty when all pass."""
    failures: list[str] = []
    for name, source, keep, drop in _CASES:
        try:
            masked = mask(source)
        except LexError as error:
            failures.append(f"{name}: {error}")
            continue
        if len(masked) != len(source):
            failures.append(f"{name}: length {len(source)} became {len(masked)}")
        if [i for i, c in enumerate(masked) if c == "\n"] != [
            i for i, c in enumerate(source) if c == "\n"
        ]:
            failures.append(f"{name}: newlines moved")
        flat = " ".join(masked.split())
        for text in keep:
            if " ".join(text.split()) not in flat:
                failures.append(f"{name}: lost code {text!r}; saw {flat!r}")
        for text in drop:
            if text in masked:
                failures.append(f"{name}: kept literal/comment text {text!r}; saw {flat!r}")

    tokens_seen = [t for t, _ in tokens(mask("use crate::a::{b, c}; // crate::d"))]
    if tokens_seen != ["use", "crate", "::", "a", "::", "{", "b", ",", "c", "}", ";"]:
        failures.append(f"tokens: {tokens_seen}")
    # What is remembered is not handed out: a caller's list is its own to change.
    first = tokens("fn a")
    second = tokens("fn a")
    if first is second:
        failures.append("tokens: two calls on one text gave the same list")
    first.append(("changed", 0))
    if tokens("fn a") != [("fn", 0), ("a", 3)]:
        failures.append(f"tokens: a caller's change reached the next answer: {tokens('fn a')}")
    return failures


def main() -> int:
    if sys.argv[1:] != ["--self-test"]:
        print(__doc__)
        return 2
    failures = self_test()
    for failure in failures:
        print(f"rustlex: {failure}", file=sys.stderr)
    if failures:
        return 1
    print(f"rustlex: {len(_CASES)} lexing cases pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
