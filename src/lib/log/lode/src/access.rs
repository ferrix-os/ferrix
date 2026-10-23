//! Who reads which records (`docs/LOGGING.md` §12.1), and how a user is
//! handed their own.
//!
//! A record whose trusted `_UID` is a person's -- [`USER_MIN`] or more,
//! never `nobody` -- is stored in that user's own segments,
//! `user-<uid>/<class>@<boot>-<seq>.fxj` beside the system's, so every
//! segment holds exactly one owner's records. All of them are lode's, group
//! `adm`, mode 0640: root, `adm` and the uploader read every one directly.
//!
//! A user reads their own through lode: Unix permissions name one group,
//! and a user's segments must be readable by `adm` too, so no mode gives
//! the user theirs and nobody else's. The reader connects to [`SOCKET`]
//! (`SOCK_SEQPACKET`, mode 0666), sends [`REQUEST`], and lode -- having read
//! the caller's uid with `SO_PEERCRED` at `accept` -- answers with one
//! packet a file, `F <name>\n` with the file open for reading passed beside
//! it (`SCM_RIGHTS`), for each of that user's segments and summaries, then
//! `END <count>\n`. The reader then reads the files itself, as a reader of
//! the whole store does: lode opens, it never reads for anyone.

use alloc::string::String;
use alloc::vec::Vec;

use crate::record::Record;

/// Where a user asks for their segments.
pub const SOCKET: &str = "/run/lode/read";

/// What the reader sends.
pub const REQUEST: &[u8] = b"OPEN\n";

/// The lowest uid that is a person's (`/etc/login.defs`' `UID_MIN`).
pub const USER_MIN: u32 = 1000;

/// `nobody`, which is nobody's.
pub const NOBODY: u32 = 65_534;

/// The user whose segments `record` goes to, if it is a person's.
#[must_use]
pub fn owner(record: &Record) -> Option<u32> {
    let uid: u32 = record.get_str("_UID")?.parse().ok()?;
    (uid >= USER_MIN && uid != NOBODY && uid != u32::MAX).then_some(uid)
}

/// A user's directory in a store, `user-<uid>`.
#[must_use]
pub fn user_dir(uid: u32) -> String {
    alloc::format!("user-{uid}")
}

/// The user a store directory's name says it is, `user-<uid>`.
#[must_use]
pub fn user_of_dir(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("user-")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The owner a segment's path in a store says: `user-1000/system@...` is
/// uid 1000's, a bare name the system's.
#[must_use]
pub fn owner_of_path(path: &str) -> Option<u32> {
    path.split_once('/').and_then(|(dir, _)| user_of_dir(dir))
}

/// A path's last part: a segment's file name without its user directory.
#[must_use]
pub fn base_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// A temporary file beside `path`, for a new file that is renamed over
/// it: `.` before its file name, `suffix` after.
#[must_use]
pub fn temporary(path: &str, suffix: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, name)) => alloc::format!("{dir}/.{name}{suffix}"),
        None => alloc::format!(".{path}{suffix}"),
    }
}

/// One packet of lode's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    /// A file, passed beside the packet; its name in the store.
    File(String),
    /// The end, and how many files were sent.
    End(u64),
}

impl Packet {
    /// The packet's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Packet::File(name) => alloc::format!("F {name}\n").into_bytes(),
            Packet::End(count) => alloc::format!("END {count}\n").into_bytes(),
        }
    }

    /// A packet's bytes read; `None` for one lode does not send.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Packet> {
        let text = core::str::from_utf8(bytes).ok()?.strip_suffix('\n')?;
        if let Some(name) = text.strip_prefix("F ") {
            let ok =
                !name.is_empty() && !name.split('/').any(|part| part.is_empty() || part == "..");
            return ok.then(|| Packet::File(String::from(name)));
        }
        text.strip_prefix("END ")?.parse().ok().map(Packet::End)
    }
}
