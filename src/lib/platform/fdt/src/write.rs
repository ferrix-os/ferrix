//! Writing a small device tree into caller-provided buffers, without
//! allocating: what the kernel's boot checks of generic publication build
//! their made-up trees with, and what the host tests and the fuzz target
//! build theirs with. Nothing in the kernel's product path writes a tree.

use crate::{FDT_BEGIN_NODE, FDT_END, FDT_END_NODE, FDT_MAGIC, FDT_PROP, HEADER_SIZE};

/// A tree under construction: the structure block and the strings block in
/// two buffers of the caller's, joined by [`Writer::finish`].
#[derive(Debug)]
pub struct Writer<'b> {
    structs: &'b mut [u8],
    used: usize,
    strings: &'b mut [u8],
    names: usize,
    depth: usize,
    failed: bool,
}

impl<'b> Writer<'b> {
    /// A writer filling `structs` and `strings`.
    pub fn new(structs: &'b mut [u8], strings: &'b mut [u8]) -> Self {
        Writer {
            structs,
            used: 0,
            strings,
            names: 0,
            depth: 0,
            failed: false,
        }
    }

    fn put(&mut self, bytes: &[u8]) {
        let end = self.used.saturating_add(bytes.len());
        match self.structs.get_mut(self.used..end) {
            Some(slot) => {
                slot.copy_from_slice(bytes);
                self.used = end;
            }
            None => self.failed = true,
        }
    }

    fn word(&mut self, value: u32) {
        self.put(&value.to_be_bytes());
    }

    fn pad(&mut self) {
        while !self.used.is_multiple_of(4) && !self.failed {
            self.put(&[0]);
        }
    }

    /// The strings-block offset of `name`, added if it is not there yet.
    fn intern(&mut self, name: &str) -> u32 {
        let mut at = 0;
        while at < self.names {
            let rest = self.strings.get(at..self.names).unwrap_or(&[]);
            let length = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
            if rest.get(..length) == Some(name.as_bytes()) {
                return at as u32;
            }
            at += length + 1;
        }
        let offset = self.names;
        let end = offset + name.len() + 1;
        match self.strings.get_mut(offset..end) {
            Some(slot) => {
                for (to, from) in slot.iter_mut().zip(name.bytes().chain(core::iter::once(0))) {
                    *to = from;
                }
                self.names = end;
            }
            None => self.failed = true,
        }
        offset as u32
    }

    /// Open a node called `name`; the first is the root, named "".
    pub fn begin(&mut self, name: &str) -> &mut Self {
        self.word(FDT_BEGIN_NODE);
        self.put(name.as_bytes());
        self.put(&[0]);
        self.pad();
        self.depth += 1;
        self
    }

    /// Close the node opened last.
    pub fn end(&mut self) -> &mut Self {
        self.word(FDT_END_NODE);
        match self.depth.checked_sub(1) {
            Some(depth) => self.depth = depth,
            None => self.failed = true,
        }
        self
    }

    /// A property with raw `value`.
    pub fn property(&mut self, name: &str, value: &[u8]) -> &mut Self {
        let offset = self.intern(name);
        self.word(FDT_PROP);
        self.word(value.len() as u32);
        self.word(offset);
        self.put(value);
        self.pad();
        self
    }

    /// A property of big-endian words.
    pub fn cells(&mut self, name: &str, words: &[u32]) -> &mut Self {
        let offset = self.intern(name);
        self.word(FDT_PROP);
        self.word((words.len() * 4) as u32);
        self.word(offset);
        for &word in words {
            self.word(word);
        }
        self
    }

    /// A property of NUL-terminated strings.
    pub fn strings(&mut self, name: &str, values: &[&str]) -> &mut Self {
        let offset = self.intern(name);
        let length: usize = values.iter().map(|value| value.len() + 1).sum();
        self.word(FDT_PROP);
        self.word(length as u32);
        self.word(offset);
        for value in values {
            self.put(value.as_bytes());
            self.put(&[0]);
        }
        self.pad();
        self
    }

    /// Join the blocks into a blob in `out`: header, an empty reservation
    /// map, the structure block, the strings. The bytes written, or `None`
    /// if a buffer was too small or a node is still open.
    pub fn finish(&mut self, out: &mut [u8]) -> Option<usize> {
        self.word(FDT_END);
        if self.failed || self.depth != 0 {
            return None;
        }
        let reservations = HEADER_SIZE;
        let structs = reservations + 16;
        let strings = structs + self.used;
        let total = strings + self.names;
        let blob = out.get_mut(..total)?;
        blob.fill(0);
        let header = [
            FDT_MAGIC,
            total as u32,
            structs as u32,
            strings as u32,
            reservations as u32,
            17,
            16,
            0,
            self.names as u32,
            self.used as u32,
        ];
        for (index, word) in header.iter().enumerate() {
            blob.get_mut(index * 4..index * 4 + 4)?
                .copy_from_slice(&word.to_be_bytes());
        }
        blob.get_mut(structs..strings)?
            .copy_from_slice(self.structs.get(..self.used)?);
        blob.get_mut(strings..total)?
            .copy_from_slice(self.strings.get(..self.names)?);
        Some(total)
    }
}
