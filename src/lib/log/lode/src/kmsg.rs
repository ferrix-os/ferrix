//! `/dev/kmsg`'s records, read (`src/kernel/src/fs/kmsg.rs` writes them).
//!
//! A read is one record: `prio,seq,usec,flags[,...];text`, a newline, and
//! then any `KEY=value` continuation lines, each beginning with a space,
//! which Ferrix's kernel does not write. `prio` is the facility shifted left
//! three and the level. The text's bytes outside printable ASCII, and the
//! backslash, are written `\xNN`, as Linux's `msg_add_ext_text` writes them.

use alloc::vec::Vec;

/// One kernel log record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelRecord {
    /// The facility, `prio >> 3`: 0 for the kernel, 1 (`LOG_USER`) for a
    /// program's console output.
    pub facility: u8,
    /// The level, `prio & 7`.
    pub level: u8,
    /// The kernel's sequence number.
    pub seq: u64,
    /// Microseconds since boot, on the kernel's monotonic clock.
    pub usec: u64,
    /// The text, unescaped, without its newline.
    pub text: Vec<u8>,
}

/// Read one record as `read(2)` of `/dev/kmsg` returned it.
#[must_use]
pub fn parse(bytes: &[u8]) -> Option<KernelRecord> {
    let semicolon = bytes.iter().position(|&byte| byte == b';')?;
    let prefix = core::str::from_utf8(bytes.get(..semicolon)?).ok()?;
    let mut parts = prefix.split(',');
    let prio: u32 = parts.next()?.parse().ok()?;
    let seq: u64 = parts.next()?.parse().ok()?;
    let usec: u64 = parts.next()?.parse().ok()?;
    let rest = bytes.get(semicolon + 1..)?;
    let line = rest
        .iter()
        .position(|&byte| byte == b'\n')
        .and_then(|end| rest.get(..end))
        .unwrap_or(rest);
    Some(KernelRecord {
        facility: u8::try_from((prio >> 3) & 0xff).unwrap_or(0),
        level: u8::try_from(prio & 7).unwrap_or(6),
        seq,
        usec,
        text: unescape(line),
    })
}

/// `text` with every `\xNN` turned back into its byte.
#[must_use]
pub fn unescape(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut at = 0;
    while let Some(&byte) = text.get(at) {
        if byte == b'\\'
            && text.get(at + 1) == Some(&b'x')
            && let Some(value) = text
                .get(at + 2..at + 4)
                .and_then(|pair| core::str::from_utf8(pair).ok())
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        {
            out.push(value);
            at += 4;
            continue;
        }
        out.push(byte);
        at += 1;
    }
    out
}
