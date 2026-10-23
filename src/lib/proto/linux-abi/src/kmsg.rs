//! The kernel log's two record formats, as Linux v7.0's `kernel/printk/printk.c`
//! writes them, and its reading of a record a program writes.
//!
//! * `syslog(2)`'s `READ`, `READ_ALL` and `READ_CLEAR` give lines of text:
//!   `<priority>[seconds.micros] text\n`, the seconds right-aligned in five
//!   columns (`print_syslog`, `print_time`, `info_print_prefix`). busybox's
//!   and util-linux's `dmesg` read the time from the brackets.
//! * `/dev/kmsg` gives one record per `read`: `priority,seq,usec,flag;text\n`
//!   (`info_print_ext_header`), the text with every byte below a space, from
//!   DEL up, and the backslash escaped as `\xNN` (`msg_add_ext_text`), so that
//!   a record is one line whatever it holds. The flag is `-`, or `c` for a
//!   continued line, which a whole record never is here.
//! * A write to `/dev/kmsg` may begin with `<priority>`: its level is taken,
//!   and its facility when that is not the kernel's, so a program cannot pass
//!   its line off as the kernel's (`devkmsg_write`).
//!
//! Every writer here fills a caller's buffer and answers how much it wrote,
//! or `None` when it does not fit: no allocation, nothing to panic on.

/// `LOG_USER`, the facility a program's record is given when it names none.
pub const LOG_USER: u8 = 1;

/// `LOG_DAEMON`, the facility a system service's record names: what Ferrix's
/// ring-3 drivers and `devmgr` write theirs at. A program's console output is
/// always `LOG_USER` at info, so a record of any other facility is one whose
/// writer stated its level (`docs/LOGGING.md` §4.1).
pub const LOG_DAEMON: u8 = 3;

/// A record's level, syslog's eight (`<sys/syslog.h>`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Level {
    /// The system is unusable.
    Emerg = 0,
    /// Action must be taken at once.
    Alert = 1,
    /// A critical condition: a program ending on a panic.
    Crit = 2,
    /// An error: something failed and what it was for did not happen.
    Err = 3,
    /// A warning: something was refused, lost or worked around.
    Warning = 4,
    /// A normal but significant condition.
    Notice = 5,
    /// Information: a step done, a device found.
    Info = 6,
    /// Detail for whoever is debugging: per-frame or per-call figures.
    Debug = 7,
}

impl Level {
    /// The `<N>` priority a `/dev/kmsg` write names this level with at
    /// `facility`.
    #[must_use]
    pub const fn priority(self, facility: u8) -> u8 {
        (facility << 3) | self as u8
    }
}

/// `default_message_loglevel`, the level of a record that names none:
/// `CONFIG_MESSAGE_LOGLEVEL_DEFAULT`, 4 (`KERN_WARNING`).
pub const DEFAULT_LEVEL: u8 = 4;

/// The most text one record keeps, and the most one write to `/dev/kmsg` may
/// carry: `PRINTKRB_RECORD_MAX`. Linux drops the rest of a longer message.
pub const RECORD_MAX: usize = 1024;

/// The most one record takes formatted, prefix, escapes and newline included:
/// `PRINTK_MESSAGE_MAX`. A `/dev/kmsg` read with less room than its record
/// needs is `EINVAL`.
pub const MESSAGE_MAX: usize = 2048;

/// A buffer being filled, which refuses to overflow.
struct Out<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl Out<'_> {
    /// Append `bytes`, or answer `None` and append nothing.
    fn put(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.len.checked_add(bytes.len())?;
        self.buf.get_mut(self.len..end)?.copy_from_slice(bytes);
        self.len = end;
        Some(())
    }

    /// Append `value` in decimal, right-aligned in `width` columns with
    /// spaces, or zero-padded to `width` when `zeros`.
    fn decimal(&mut self, value: u64, width: usize, zeros: bool) -> Option<()> {
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        let mut rest = value;
        loop {
            at -= 1;
            *digits.get_mut(at)? = b'0' + u8::try_from(rest % 10).ok()?;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        let shown = digits.get(at..)?;
        for _ in shown.len()..width {
            self.put(if zeros { b"0" } else { b" " })?;
        }
        self.put(shown)
    }
}

/// Write `syslog(2)`'s prefix for a record of `priority` stamped `usec`:
/// `<6>[   12.345678] `. Answers its length.
#[must_use]
pub fn syslog_prefix(priority: u8, usec: u64, buf: &mut [u8]) -> Option<usize> {
    let mut out = Out { buf, len: 0 };
    out.put(b"<")?;
    out.decimal(u64::from(priority), 0, false)?;
    out.put(b">[")?;
    out.decimal(usec / 1_000_000, 5, false)?;
    out.put(b".")?;
    out.decimal(usec % 1_000_000, 6, true)?;
    out.put(b"] ")?;
    Some(out.len)
}

/// Write one `syslog(2)` line: the prefix, `text` as it is, and a newline.
/// `text` is cut to [`RECORD_MAX`] first. Answers its length.
#[must_use]
pub fn syslog_line(priority: u8, usec: u64, text: &[u8], buf: &mut [u8]) -> Option<usize> {
    let prefix = syslog_prefix(priority, usec, buf)?;
    let mut out = Out { buf, len: prefix };
    out.put(text.get(..RECORD_MAX).unwrap_or(text))?;
    out.put(b"\n")?;
    Some(out.len)
}

/// Write one `/dev/kmsg` record: `priority,seq,usec,-;`, `text` escaped,
/// and a newline. `text` is cut to [`RECORD_MAX`] first, and its escaped
/// form to what leaves room for the newline, as Linux's bounded `scnprintf`
/// cuts it. Answers its length; `None` when not even the header fits.
#[must_use]
pub fn devkmsg_record(
    priority: u8,
    seq: u64,
    usec: u64,
    text: &[u8],
    buf: &mut [u8],
) -> Option<usize> {
    let mut out = Out { buf, len: 0 };
    out.decimal(u64::from(priority), 0, false)?;
    out.put(b",")?;
    out.decimal(seq, 0, false)?;
    out.put(b",")?;
    out.decimal(usec, 0, false)?;
    out.put(b",-;")?;
    let room = out.buf.len().checked_sub(1)?;
    for &byte in text.get(..RECORD_MAX).unwrap_or(text) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let escaped;
        let piece: &[u8] = if !(b' '..127).contains(&byte) || byte == b'\\' {
            let high = HEX.get(usize::from(byte >> 4))?;
            let low = HEX.get(usize::from(byte & 0xF))?;
            escaped = [b'\\', b'x', *high, *low];
            &escaped
        } else {
            core::slice::from_ref(&byte)
        };
        if out.len.saturating_add(piece.len()) > room {
            break;
        }
        out.put(piece)?;
    }
    out.put(b"\n")?;
    Some(out.len)
}

/// Read a record a program wrote to `/dev/kmsg`: its priority, from a leading
/// `<N>` if it has one -- the level always, the facility unless it is the
/// kernel's, which becomes `LOG_USER` -- and the text after it.
#[must_use]
pub fn parse_write(record: &[u8]) -> (u8, &[u8]) {
    let mut level = DEFAULT_LEVEL;
    let mut facility = LOG_USER;
    let mut text = record;
    if let Some(rest) = record.strip_prefix(b"<") {
        let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if digits > 0 && rest.get(digits) == Some(&b'>') {
            // `simple_strtoul` into an `unsigned int`: what does not fit wraps.
            let value = rest
                .get(..digits)
                .unwrap_or_default()
                .iter()
                .fold(0u32, |n, d| {
                    n.wrapping_mul(10).wrapping_add(u32::from(d - b'0'))
                });
            level = u8::try_from(value & 7).unwrap_or(DEFAULT_LEVEL);
            let named = (value >> 3) & 0xFF;
            if named != 0 {
                facility = u8::try_from(named).unwrap_or(LOG_USER);
            }
            text = rest.get(digits + 1..).unwrap_or_default();
        }
    }
    (facility.wrapping_shl(3) | level, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(priority: u8, usec: u64, text: &[u8]) -> Vec<u8> {
        let mut buf = [0u8; MESSAGE_MAX];
        let len = syslog_line(priority, usec, text, &mut buf).unwrap();
        buf[..len].to_vec()
    }

    fn record(priority: u8, seq: u64, usec: u64, text: &[u8]) -> Vec<u8> {
        let mut buf = [0u8; MESSAGE_MAX];
        let len = devkmsg_record(priority, seq, usec, text, &mut buf).unwrap();
        buf[..len].to_vec()
    }

    extern crate std;
    use std::vec::Vec;

    #[test]
    fn a_syslog_line_is_linuxs_with_five_columns_of_seconds() {
        assert_eq!(line(6, 0, b"Booting"), b"<6>[    0.000000] Booting\n");
        assert_eq!(
            line(14, 12_345_678, b"hello"),
            b"<14>[   12.345678] hello\n"
        );
        // Wider than five columns: printed whole, as `%5lu` does.
        assert_eq!(
            line(3, 123_456_789_000_001, b"x"),
            b"<3>[123456789.000001] x\n"
        );
    }

    #[test]
    fn a_kmsg_record_escapes_what_would_break_its_line() {
        assert_eq!(
            record(6, 42, 1_000, b"plain text"),
            b"6,42,1000,-;plain text\n"
        );
        assert_eq!(
            record(12, 0, 7, b"a\tb\\c\x1b[0m\xff"),
            b"12,0,7,-;a\\x09b\\x5cc\\x1b[0m\\xff\n"
        );
    }

    #[test]
    fn a_record_is_cut_at_record_max_and_refused_where_it_does_not_fit() {
        let long = [b'a'; RECORD_MAX + 100];
        assert_eq!(
            line(6, 0, &long).len(),
            b"<6>[    0.000000] ".len() + RECORD_MAX + 1
        );
        let mut small = [0u8; 12];
        let len = devkmsg_record(6, 1, 2, b"too\x01long for it", &mut small).unwrap();
        assert_eq!(&small[..len], b"6,1,2,-;too\n");
        assert_eq!(devkmsg_record(6, 1, 2, b"", &mut small[..4]), None);
        assert_eq!(syslog_line(6, 1, b"", &mut small), None);
    }

    #[test]
    fn a_write_takes_its_level_and_never_the_kernels_facility() {
        assert_eq!(parse_write(b"hello"), ((1 << 3) | 4, &b"hello"[..]));
        assert_eq!(parse_write(b"<3>err"), ((1 << 3) | 3, &b"err"[..]));
        // systemd's `<30>`: daemon.info.
        assert_eq!(
            parse_write(b"<30>systemd[1]: up"),
            (30, &b"systemd[1]: up"[..])
        );
        // Not a prefix: kept as text, at the default.
        assert_eq!(parse_write(b"<x>y"), (12, &b"<x>y"[..]));
        assert_eq!(parse_write(b"<>y"), (12, &b"<>y"[..]));
        assert_eq!(parse_write(b"<5"), (12, &b"<5"[..]));
    }
}
