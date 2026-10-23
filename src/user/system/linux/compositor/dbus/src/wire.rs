//! D-Bus's wire format, little-endian: the basic types each aligned to its
//! own size, strings and object paths as a length, the bytes and a NUL,
//! signatures as a one-byte length, arrays as a length and their elements
//! aligned to the element's alignment, structs and dict entries aligned to
//! 8, variants as their signature and then the value (the D-Bus
//! Specification, "Marshaling (Wire Format)").

use core::fmt;

/// Why bytes could not be read as what was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// The bytes ended first.
    Short,
    /// A string that is not UTF-8 or not NUL-terminated.
    Text,
    /// A signature this crate does not read.
    Signature,
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Short => "the message ended early",
            Self::Text => "a string is not UTF-8 or not terminated",
            Self::Signature => "a signature this reader does not take",
        })
    }
}

/// Bytes being written, aligned from the start of the message.
#[derive(Clone, Debug, Default)]
pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    /// An empty body.
    #[must_use]
    pub const fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// What has been written.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// How many bytes so far.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether nothing is written.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Pad with zeros to a multiple of `to`.
    pub fn align(&mut self, to: usize) {
        while !self.bytes.len().is_multiple_of(to) {
            self.bytes.push(0);
        }
    }

    /// `y`.
    pub fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    /// `u`.
    pub fn u32(&mut self, value: u32) {
        self.align(4);
        self.bytes.extend(value.to_le_bytes());
    }

    /// `i`.
    pub fn i32(&mut self, value: i32) {
        self.u32(value.cast_unsigned());
    }

    /// `b`: a `u32` of 0 or 1.
    pub fn bool(&mut self, value: bool) {
        self.u32(u32::from(value));
    }

    /// `s` or `o`.
    pub fn string(&mut self, value: &str) {
        self.u32(u32::try_from(value.len()).unwrap_or(u32::MAX));
        self.bytes.extend(value.as_bytes());
        self.bytes.push(0);
    }

    /// `g`.
    pub fn signature(&mut self, value: &str) {
        self.bytes
            .push(u8::try_from(value.len()).unwrap_or(u8::MAX));
        self.bytes.extend(value.as_bytes());
        self.bytes.push(0);
    }

    /// `as`.
    pub fn strings(&mut self, values: &[&str]) {
        self.array(4, |writer| {
            for value in values {
                writer.string(value);
            }
        });
    }

    /// An array whose elements align to `element_alignment`, written by
    /// `elements`: its length is filled in afterwards.
    pub fn array(&mut self, element_alignment: usize, elements: impl FnOnce(&mut Self)) {
        self.align(4);
        let at = self.bytes.len();
        self.bytes.extend([0; 4]);
        self.align(element_alignment);
        let start = self.bytes.len();
        elements(self);
        let length = u32::try_from(self.bytes.len() - start).unwrap_or(u32::MAX);
        if let Some(slot) = self.bytes.get_mut(at..at + 4) {
            slot.copy_from_slice(&length.to_le_bytes());
        }
    }
}

/// Bytes being read, aligned from the start of the message.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    /// Read `bytes` from their start.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// Read `bytes`, whose first byte is at `offset` of the message, so
    /// that alignment counts from the message's start.
    #[must_use]
    pub const fn at(bytes: &'a [u8], offset: usize) -> Self {
        Self { bytes, at: offset }
    }

    /// Where the next read starts.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.at
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], Malformed> {
        let slice = self
            .bytes
            .get(self.at..self.at.checked_add(count).ok_or(Malformed::Short)?)
            .ok_or(Malformed::Short)?;
        self.at += count;
        Ok(slice)
    }

    /// Skip to a multiple of `to`.
    ///
    /// # Errors
    ///
    /// [`Malformed::Short`] past the end.
    pub fn align(&mut self, to: usize) -> Result<(), Malformed> {
        let padding = (to - self.at % to) % to;
        self.take(padding).map(|_| ())
    }

    /// `y`.
    ///
    /// # Errors
    ///
    /// [`Malformed::Short`].
    pub fn byte(&mut self) -> Result<u8, Malformed> {
        self.take(1)?.first().copied().ok_or(Malformed::Short)
    }

    /// `u`.
    ///
    /// # Errors
    ///
    /// [`Malformed::Short`].
    pub fn u32(&mut self) -> Result<u32, Malformed> {
        self.align(4)?;
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes(
            bytes.try_into().map_err(|_| Malformed::Short)?,
        ))
    }

    /// `i`.
    ///
    /// # Errors
    ///
    /// [`Malformed::Short`].
    pub fn i32(&mut self) -> Result<i32, Malformed> {
        self.u32().map(u32::cast_signed)
    }

    /// `s` or `o`.
    ///
    /// # Errors
    ///
    /// [`Malformed`].
    pub fn string(&mut self) -> Result<&'a str, Malformed> {
        let length = usize::try_from(self.u32()?).map_err(|_| Malformed::Short)?;
        let bytes = self.take(length)?;
        if self.byte()? != 0 {
            return Err(Malformed::Text);
        }
        core::str::from_utf8(bytes).map_err(|_| Malformed::Text)
    }

    /// `g`.
    ///
    /// # Errors
    ///
    /// [`Malformed`].
    pub fn signature(&mut self) -> Result<&'a str, Malformed> {
        let length = usize::from(self.byte()?);
        let bytes = self.take(length)?;
        if self.byte()? != 0 {
            return Err(Malformed::Text);
        }
        core::str::from_utf8(bytes).map_err(|_| Malformed::Text)
    }

    /// `as`.
    ///
    /// # Errors
    ///
    /// [`Malformed`].
    pub fn strings(&mut self) -> Result<Vec<&'a str>, Malformed> {
        let length = usize::try_from(self.u32()?).map_err(|_| Malformed::Short)?;
        self.align(4)?;
        let end = self.at + length;
        let mut out = Vec::new();
        while self.at < end {
            out.push(self.string()?);
        }
        Ok(out)
    }

    /// Skip one value of the complete type at the start of `signature`,
    /// and give the rest of the signature.
    ///
    /// # Errors
    ///
    /// [`Malformed`].
    pub fn skip<'s>(&mut self, signature: &'s str) -> Result<&'s str, Malformed> {
        let (first, rest) = split_one(signature)?;
        match first.as_bytes().first().copied() {
            Some(b'y') => {
                let _ = self.byte()?;
            }
            Some(b'n' | b'q') => {
                self.align(2)?;
                let _ = self.take(2)?;
            }
            Some(b'b' | b'i' | b'u' | b'h') => {
                let _ = self.u32()?;
            }
            Some(b'x' | b't' | b'd') => {
                self.align(8)?;
                let _ = self.take(8)?;
            }
            Some(b's' | b'o') => {
                let _ = self.string()?;
            }
            Some(b'g') => {
                let _ = self.signature()?;
            }
            Some(b'v') => {
                let inner = self.signature()?;
                let mut left = inner;
                while !left.is_empty() {
                    left = self.skip(left)?;
                }
            }
            Some(b'a') => {
                let length = usize::try_from(self.u32()?).map_err(|_| Malformed::Short)?;
                let element = first.get(1..).ok_or(Malformed::Signature)?;
                self.align(alignment(element)?)?;
                let _ = self.take(length)?;
            }
            Some(b'(' | b'{') => {
                self.align(8)?;
                let mut inner = first.get(1..first.len() - 1).ok_or(Malformed::Signature)?;
                while !inner.is_empty() {
                    inner = self.skip(inner)?;
                }
            }
            _ => return Err(Malformed::Signature),
        }
        Ok(rest)
    }
}

/// The alignment of a complete type's values.
///
/// # Errors
///
/// [`Malformed::Signature`] for one this crate does not know.
pub fn alignment(signature: &str) -> Result<usize, Malformed> {
    Ok(match signature.as_bytes().first().copied() {
        Some(b'y' | b'g' | b'v') => 1,
        Some(b'n' | b'q') => 2,
        Some(b'b' | b'i' | b'u' | b'h' | b's' | b'o' | b'a') => 4,
        Some(b'x' | b't' | b'd' | b'(' | b'{') => 8,
        _ => return Err(Malformed::Signature),
    })
}

/// The first complete type of `signature`, and what follows it.
///
/// # Errors
///
/// [`Malformed::Signature`] for an unbalanced or empty one.
pub fn split_one(signature: &str) -> Result<(&str, &str), Malformed> {
    let bytes = signature.as_bytes();
    let mut end = 0;
    loop {
        match bytes.get(end).copied() {
            Some(b'a') => end += 1,
            Some(b'(' | b'{') => {
                let mut depth = 0_i32;
                let close = bytes
                    .iter()
                    .enumerate()
                    .skip(end)
                    .find_map(|(index, byte)| {
                        match byte {
                            b'(' | b'{' => depth += 1,
                            b')' | b'}' => depth -= 1,
                            _ => {}
                        }
                        (depth == 0).then_some(index + 1)
                    });
                let Some(close) = close else {
                    return Err(Malformed::Signature);
                };
                end = close;
                break;
            }
            Some(_) => {
                end += 1;
                break;
            }
            None => return Err(Malformed::Signature),
        }
    }
    Ok((
        signature.get(..end).ok_or(Malformed::Signature)?,
        signature.get(end..).ok_or(Malformed::Signature)?,
    ))
}
