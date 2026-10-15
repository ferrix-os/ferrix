//! Naming the functions in a kernel panic's backtrace.
//!
//! The kernel prints return addresses and nothing else. It carries no symbol
//! table, and a panic is the worst moment to go looking for one. The image it
//! was built from is on this machine with its symbols intact, though — both
//! profiles keep them for exactly this — so the boot test looks each address
//! up as the report arrives and prints the function beside it.

use std::cell::Cell;
use std::path::Path;

use ferrix_elf::Elf;

/// What a backtrace line in a panic report begins with, after indentation.
///
/// `src/kernel/src/panic.rs` writes it, and the test
/// `the_kernel_writes_the_label_this_reads` holds the two together.
pub(crate) const TRACE_LABEL: &str = "trace";

/// A kernel image held open for lookups.
///
/// And how far the image it describes had moved in the boot being read: a
/// backtrace gives the addresses the kernel ran at, and with KASLR those are
/// not the addresses in the ELF. The panic report says the slide on a line
/// before its trace (`src/kernel/src/panic.rs`), and [`Symbolizer::annotate`]
/// takes it from there.
#[derive(Debug)]
pub(crate) struct Symbolizer {
    image: Vec<u8>,
    slide: Cell<u64>,
}

impl Symbolizer {
    /// Read the kernel at `path`. `None` if it cannot be read or has no symbol
    /// table, in which case backtraces are shown as they arrived.
    pub(crate) fn open(path: &Path) -> Option<Self> {
        let image = std::fs::read(path).ok()?;
        let _ = Elf::parse(&image).ok()?.symbols()?;
        Some(Symbolizer {
            image,
            slide: Cell::new(0),
        })
    }

    /// `line` with the function its return address is in appended, if it is a
    /// backtrace line and the address is inside a known function.
    ///
    /// A line giving the boot's slide is remembered, and the address on every
    /// trace line after it is taken back to where it was linked first.
    pub(crate) fn annotate(&self, line: &str) -> Option<String> {
        if let Some(slide) = crate::kaslr::slide_in(line) {
            self.slide.set(slide);
            return None;
        }
        let address = trace_address(line)?.wrapping_sub(self.slide.get());
        let elf = Elf::parse(&self.image).ok()?;
        // A return address is the instruction after the call. The byte before
        // it is the call itself, which names the right function even when the
        // call was that function's last instruction and the return address is
        // the first byte of the next one.
        let (symbol, offset) = elf.function_at(address.checked_sub(1)?)?;
        Some(format!(
            "{line}  {}+{:#x}",
            demangle(symbol.name),
            offset.saturating_add(1)
        ))
    }
}

/// The address on a backtrace line: the first `0x` word after the label.
fn trace_address(line: &str) -> Option<u64> {
    line.trim_start()
        .strip_prefix(TRACE_LABEL)?
        .split_whitespace()
        .find_map(|word| u64::from_str_radix(word.strip_prefix("0x")?, 16).ok())
}

/// A v0-mangled name as its path, without generic arguments.
///
/// This toolchain mangles with the v0 scheme, whose grammar carries generics,
/// lifetimes, and back-references to earlier substrings of the same symbol. A
/// backtrace wants the function's name, so the path is decoded and the rest
/// skipped: `ferrix_kernel_core::backtrace::walk`, not the closure type it was
/// instantiated with. Anything the grammar does not cover gives `None`, and
/// the raw symbol is printed instead, which is still searchable.
fn v0(name: &str) -> Option<String> {
    let body = name.strip_prefix("_R")?;
    let path = V0 {
        body,
        at: 0,
        depth: 0,
    }
    .path()?;
    (!path.is_empty()).then(|| path.join("::"))
}

/// A parser over a v0 symbol's body, tracked by byte offset because a
/// back-reference names one.
struct V0<'a> {
    body: &'a str,
    at: usize,
    depth: u32,
}

impl V0<'_> {
    /// What is left to parse.
    fn rest(&self) -> &str {
        self.body.get(self.at..).unwrap_or_default()
    }

    /// The next character, without consuming it.
    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    /// Consume the next character.
    fn bump(&mut self) -> Option<char> {
        let next = self.peek()?;
        self.at = self.at.checked_add(next.len_utf8())?;
        Some(next)
    }

    /// Consume `want` if it is next, and say whether it was.
    fn eat(&mut self, want: char) -> bool {
        let found = self.peek() == Some(want);
        if found {
            let _ = self.bump();
        }
        found
    }

    /// A base-62 number, terminated by `_`.
    fn base62(&mut self) -> Option<u64> {
        if self.eat('_') {
            return Some(0);
        }
        let mut value: u64 = 0;
        loop {
            let digit = match self.bump()? {
                '_' => return value.checked_add(1),
                c @ '0'..='9' => u64::from(c as u8 - b'0'),
                c @ 'a'..='z' => u64::from(c as u8 - b'a') + 10,
                c @ 'A'..='Z' => u64::from(c as u8 - b'A') + 36,
                _ => return None,
            };
            value = value.checked_mul(62)?.checked_add(digit)?;
        }
    }

    /// The `s<base62>` that distinguishes same-named items, if one is here.
    fn disambiguator(&mut self) {
        if self.peek() == Some('s') {
            let _ = self.bump();
            let _ = self.base62();
        }
    }

    /// A length-prefixed identifier.
    fn ident(&mut self) -> Option<String> {
        self.disambiguator();
        let punycode = self.eat('u');
        let digits = self.rest().find(|c: char| !c.is_ascii_digit())?;
        let length: usize = self.rest().get(..digits)?.parse().ok()?;
        self.at = self.at.checked_add(digits)?;
        // A `_` separates the length from an identifier that starts with a digit.
        let _ = self.eat('_');
        let text = self.rest().get(..length)?.to_owned();
        self.at = self.at.checked_add(length)?;
        // Punycode names are rare and non-ASCII; showing the encoded form
        // beats failing the symbol.
        Some(if punycode {
            format!("{text}(punycode)")
        } else {
            text
        })
    }

    /// A path, as its segments.
    fn path(&mut self) -> Option<Vec<String>> {
        self.depth = self.depth.checked_add(1)?;
        if self.depth > 64 {
            return None;
        }
        let path = match self.bump()? {
            // A crate root.
            'C' => Some(vec![self.ident()?]),
            // An item inside something else; the namespace byte says which
            // kind, which a backtrace does not need.
            'N' => {
                let _namespace = self.bump()?;
                let mut parent = self.path()?;
                parent.push(self.ident()?);
                Some(parent)
            }
            // A generic instance: the path, then arguments nobody prints.
            'I' => {
                let path = self.path()?;
                self.skip_generics()?;
                Some(path)
            }
            // An inherent impl: named by the type it is on.
            'M' => {
                self.disambiguator();
                let _impl = self.path()?;
                self.path()
            }
            // A trait impl, and a trait definition: likewise the type.
            'X' => {
                self.disambiguator();
                let _impl = self.path()?;
                let subject = self.path()?;
                let _trait = self.path()?;
                Some(subject)
            }
            'Y' => {
                let subject = self.path()?;
                let _trait = self.path()?;
                Some(subject)
            }
            // A back-reference to a path already spelled out, by byte offset.
            'B' => {
                let at = usize::try_from(self.base62()?).ok()?;
                V0 {
                    body: self.body,
                    at,
                    depth: self.depth,
                }
                .path()
            }
            _ => None,
        };
        self.depth = self.depth.saturating_sub(1);
        path
    }

    /// Skip a generic argument list, up to the `E` that closes it.
    ///
    /// The arguments are types, lifetimes and constants, and this walks past
    /// them by their opening character rather than decoding them. Identifiers
    /// are skipped by their length and paths by parsing them, so an `E` inside
    /// a name is never mistaken for the end of the list.
    fn skip_generics(&mut self) -> Option<()> {
        let mut depth = 1_u32;
        while depth > 0 {
            match self.peek()? {
                'E' => {
                    let _ = self.bump();
                    depth = depth.saturating_sub(1);
                }
                // The constructs that are themselves closed by an `E`.
                'I' | 'D' | 'F' | 'T' => {
                    let _ = self.bump();
                    depth = depth.checked_add(1)?;
                }
                'B' => {
                    let _ = self.bump();
                    let _ = self.base62()?;
                }
                'C' | 'N' | 'M' | 'X' | 'Y' => {
                    let _ = self.path()?;
                }
                digit if digit.is_ascii_digit() => {
                    let _ = self.ident()?;
                }
                // A lifetime, a basic type, or a namespace byte: one character.
                _ => {
                    let _ = self.bump();
                }
            }
        }
        Some(())
    }
}

/// A legacy-mangled Rust symbol name as its path, without the hash.
///
/// `_ZN6kernel5kmain17h0123456789abcdefE` becomes `kernel::kmain`, and a v0
/// name becomes its path. Anything else — a C name, a label from assembly —
/// comes back as it went in, which is still something a person can search the
/// source for.
pub(crate) fn demangle(name: &[u8]) -> String {
    let text = String::from_utf8_lossy(name);
    v0(&text)
        .or_else(|| legacy(&text))
        .unwrap_or_else(|| text.into_owned())
}

/// Decode `_ZN` followed by length-prefixed segments and `E`, or `None` if
/// `name` is not that. Whatever follows the `E` — `.llvm.1234` from LTO, say —
/// is dropped.
fn legacy(name: &str) -> Option<String> {
    let mut rest = name.strip_prefix("_ZN")?;
    let mut segments = Vec::new();
    while !rest.starts_with('E') {
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        let (length, tail) = rest.split_at(digits);
        let length: usize = length.parse().ok()?;
        segments.push(tail.get(..length)?);
        rest = tail.get(length..)?;
    }
    if segments.last().is_some_and(|last| is_hash(last)) {
        let _ = segments.pop();
    }
    if segments.is_empty() {
        return None;
    }
    let decoded: Vec<String> = segments.iter().map(|segment| unescape(segment)).collect();
    Some(decoded.join("::"))
}

/// Whether `segment` is the hash legacy mangling ends a path with: `h` and
/// sixteen hex digits.
fn is_hash(segment: &str) -> bool {
    segment.len() == 17
        && segment
            .strip_prefix('h')
            .is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Undo legacy mangling's escapes within one path segment.
fn unescape(segment: &str) -> String {
    // A segment that would start with `$` is written `_$`.
    let mut rest = match segment.strip_prefix('_') {
        Some(tail) if tail.starts_with('$') => tail,
        _ => segment,
    };
    let mut out = String::new();
    while !rest.is_empty() {
        let (piece, tail) = next_piece(rest);
        out.push_str(&piece);
        rest = tail;
    }
    out
}

/// The first piece of a mangled segment, decoded, and what follows it.
fn next_piece(rest: &str) -> (String, &str) {
    if let Some(tail) = rest.strip_prefix("..") {
        return ("::".to_owned(), tail);
    }
    let escape = rest
        .strip_prefix('$')
        .and_then(|escape| escape.split_once('$'));
    if let Some((code, tail)) = escape
        && let Some(decoded) = escaped(code)
    {
        return (decoded.to_string(), tail);
    }
    let mut chars = rest.chars();
    let first = chars.next().map(String::from).unwrap_or_default();
    (first, chars.as_str())
}

/// The character a `$code$` escape stands for.
fn escaped(code: &str) -> Option<char> {
    let named = match code {
        "SP" => '@',
        "BP" => '*',
        "RF" => '&',
        "LT" => '<',
        "GT" => '>',
        "LP" => '(',
        "RP" => ')',
        "C" => ',',
        _ => '\0',
    };
    if named != '\0' {
        return Some(named);
    }
    let hex = code.strip_prefix('u')?;
    char::from_u32(u32::from_str_radix(hex, 16).ok()?)
}

#[cfg(test)]
mod tests;
