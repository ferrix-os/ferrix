//! The trusted prompt's channel (`docs/SETTINGS-WRITER.md` §12): how
//! `settingsd` asks the person at the seat, through `sessiond`, in a prompt
//! the compositor draws itself.
//!
//! Two hops, the same records on both:
//!
//! * `settingsd` (root) to `sessiond` (root) on a `SOCK_SEQPACKET` socket
//!   at [`SOCKET`], mode 0600, which each side checks the other's uid on;
//! * `sessiond` to the compositor on a third socket pair, inherited as
//!   descriptor [`PROMPT_FD`] and named by [`PROMPT_FD_VARIABLE`], beside
//!   the devices' and the lock's: no path, close-on-exec in the compositor
//!   at once.
//!
//! One record a packet, at most [`MAX_RECORD`] bytes: a word and its fields,
//! space-separated, each text field percent-escaped ([`escape`]) so that a
//! field holds no space, no control character and nothing that is not
//! UTF-8. A packet that is not exactly one record is no record.
//!
//! The answer's secret travels in a [`Secret`], whose bytes are zeroed when
//! it goes and which no `Debug` shows; a packet that held one is zeroed by
//! whoever read it.

use core::fmt;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixDatagram;

/// Where `sessiond` listens for `settingsd`.
pub const SOCKET: &str = "/run/ferrix/prompt";

/// The variable naming the prompt channel's descriptor in the compositor.
pub const PROMPT_FD_VARIABLE: &str = "FERRIX_PROMPT_FD";

/// The descriptor `sessiond` gives the prompt channel in the compositor.
pub const PROMPT_FD: i32 = 5;

/// The longest record, in bytes.
pub const MAX_RECORD: usize = 2048;

/// The longest text field, unescaped, in bytes.
pub const MAX_TEXT: usize = 512;

/// The longest secret, in bytes: `authd`'s own bound.
pub const MAX_SECRET: usize = 256;

/// A password on its way from the prompt to `authd`: zeroed when dropped,
/// never shown.
#[derive(PartialEq, Eq)]
pub struct Secret(Vec<u8>);

impl Secret {
    /// A secret of `bytes`, if they fit.
    #[must_use]
    pub fn new(bytes: &[u8]) -> Option<Secret> {
        (bytes.len() <= MAX_SECRET).then(|| Secret(bytes.to_vec()))
    }

    /// Its bytes.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        zero(&mut self.0);
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Overwrite `bytes` with zeroes in a way the optimiser keeps.
pub fn zero(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        *byte = 0;
    }
    let _ = std::hint::black_box(&*bytes);
}

/// What `settingsd` says, and `sessiond` passes on.
#[derive(Debug, PartialEq, Eq)]
pub enum ToSeat {
    /// `settingsd` to `sessiond` only, first: the account it asks for.
    Hello {
        /// The uid the prompt is for.
        uid: u32,
    },
    /// Put a prompt up.
    Ask {
        /// The prompt's number, `settingsd`'s.
        id: u64,
        /// The area: `hostname`, `time`, `services`.
        area: String,
        /// The program that asked, as the kernel recorded it.
        program: String,
        /// The program that started it.
        parent: String,
        /// The account whose request it is.
        account: String,
        /// What the request would do: the dry run's own line.
        what: String,
    },
    /// A field for `authd`'s question.
    Question {
        /// The prompt's number.
        id: u64,
        /// Whether what is typed may be shown.
        visible: bool,
        /// `authd`'s words: `Password: `.
        text: String,
    },
    /// `authd`'s words, under the field.
    Info {
        /// The prompt's number.
        id: u64,
        /// The words.
        text: String,
    },
    /// Allow and Cancel, with no field.
    Confirm {
        /// The prompt's number.
        id: u64,
    },
    /// The end: the prompt goes, its last words shown for a moment.
    Close {
        /// The prompt's number.
        id: u64,
        /// What came of it.
        text: String,
    },
}

/// What the compositor says, and `sessiond` passes on; and `sessiond`'s own
/// answer to [`ToSeat::Hello`].
#[derive(Debug, PartialEq, Eq)]
pub enum FromSeat {
    /// `sessiond`: the account is the session's, and the seat is free.
    Ready,
    /// `sessiond`: the prompt cannot be put up, and why: `busy`,
    /// `not-here` (another account's session), `no-screen`.
    Unable(String),
    /// The field's text, with Allow.
    Answer {
        /// The prompt's number.
        id: u64,
        /// What was typed.
        secret: Secret,
    },
    /// Allow, with no field.
    Allow {
        /// The prompt's number.
        id: u64,
    },
    /// Cancel, or the prompt could not stay: `cancelled`, `locked`,
    /// `timeout`, `busy`, `gone`.
    Deny {
        /// The prompt's number.
        id: u64,
        /// Why.
        why: String,
    },
}

impl FromSeat {
    /// The prompt it answers, if it is an answer.
    #[must_use]
    pub const fn id(&self) -> Option<u64> {
        match self {
            FromSeat::Answer { id, .. } | FromSeat::Allow { id } | FromSeat::Deny { id, .. } => {
                Some(*id)
            }
            FromSeat::Ready | FromSeat::Unable(_) => None,
        }
    }
}

impl ToSeat {
    /// The prompt it is about, if any.
    #[must_use]
    pub const fn id(&self) -> Option<u64> {
        match self {
            ToSeat::Hello { .. } => None,
            ToSeat::Ask { id, .. }
            | ToSeat::Question { id, .. }
            | ToSeat::Info { id, .. }
            | ToSeat::Confirm { id }
            | ToSeat::Close { id, .. } => Some(*id),
        }
    }
}

/// `text` with every byte that is not printable ASCII other than `%`, and
/// the space, as `%XX`: a field that holds no space and no control
/// character, whatever it said.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_graphic() && byte != b'%' {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The text [`escape`] made `field` from, if it made it: `%XX` with two
/// upper-case hex digits, no bare space or control byte, UTF-8, and at most
/// `limit` bytes.
#[must_use]
pub fn unescape(field: &str, limit: usize) -> Option<String> {
    let mut bytes = Vec::with_capacity(field.len());
    let mut rest = field.bytes();
    while let Some(byte) = rest.next() {
        if byte == b'%' {
            let high = hex(rest.next()?)?;
            let low = hex(rest.next()?)?;
            bytes.push(high << 4 | low);
        } else if byte.is_ascii_graphic() {
            bytes.push(byte);
        } else {
            return None;
        }
        if bytes.len() > limit {
            return None;
        }
    }
    String::from_utf8(bytes).ok()
}

fn hex(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// A plain decimal number: digits only, no sign, no leading zero but `0`.
fn number(text: &str) -> Option<u64> {
    let plain = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    plain.then(|| text.parse().ok()).flatten()
}

fn flag(text: &str) -> Option<bool> {
    match text {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

/// A record's words, if it is one of `count` of them.
fn words(packet: &[u8], count: usize) -> Option<Vec<&str>> {
    if packet.is_empty() || packet.len() > MAX_RECORD {
        return None;
    }
    let text = core::str::from_utf8(packet).ok()?;
    let words: Vec<&str> = text.split(' ').collect();
    (words.len() == count && words.iter().all(|word| !word.is_empty())).then_some(words)
}

/// A record's first word.
fn first(packet: &[u8]) -> Option<&str> {
    let text = core::str::from_utf8(packet.get(..packet.len().min(16))?).ok()?;
    text.split(' ').next()
}

impl ToSeat {
    /// The record as one packet's bytes, or `None` for one too long.
    #[must_use]
    pub fn encode(&self) -> Option<Vec<u8>> {
        let text = |text: &str| (text.len() <= MAX_TEXT).then(|| escape(text));
        let line = match self {
            ToSeat::Hello { uid } => format!("hello {uid}"),
            ToSeat::Ask {
                id,
                area,
                program,
                parent,
                account,
                what,
            } => format!(
                "ask {id} {} {} {} {} {}",
                text(area)?,
                text(program)?,
                text(parent)?,
                text(account)?,
                text(what)?
            ),
            ToSeat::Question {
                id,
                visible,
                text: words,
            } => {
                format!("question {id} {} {}", u8::from(*visible), text(words)?)
            }
            ToSeat::Info { id, text: words } => format!("info {id} {}", text(words)?),
            ToSeat::Confirm { id } => format!("confirm {id}"),
            ToSeat::Close { id, text: words } => format!("close {id} {}", text(words)?),
        };
        (line.len() <= MAX_RECORD).then(|| line.into_bytes())
    }

    /// The record `packet` holds, or `None` for anything else.
    #[must_use]
    pub fn decode(packet: &[u8]) -> Option<ToSeat> {
        let text = |field: &str| unescape(field, MAX_TEXT);
        Some(match first(packet)? {
            "hello" => {
                let w = words(packet, 2)?;
                ToSeat::Hello {
                    uid: u32::try_from(number(w.get(1)?)?).ok()?,
                }
            }
            "ask" => {
                let w = words(packet, 7)?;
                ToSeat::Ask {
                    id: number(w.get(1)?)?,
                    area: text(w.get(2)?)?,
                    program: text(w.get(3)?)?,
                    parent: text(w.get(4)?)?,
                    account: text(w.get(5)?)?,
                    what: text(w.get(6)?)?,
                }
            }
            "question" => {
                let w = words(packet, 4)?;
                ToSeat::Question {
                    id: number(w.get(1)?)?,
                    visible: flag(w.get(2)?)?,
                    text: text(w.get(3)?)?,
                }
            }
            "info" => {
                let w = words(packet, 3)?;
                ToSeat::Info {
                    id: number(w.get(1)?)?,
                    text: text(w.get(2)?)?,
                }
            }
            "confirm" => {
                let w = words(packet, 2)?;
                ToSeat::Confirm {
                    id: number(w.get(1)?)?,
                }
            }
            "close" => {
                let w = words(packet, 3)?;
                ToSeat::Close {
                    id: number(w.get(1)?)?,
                    text: text(w.get(2)?)?,
                }
            }
            _ => return None,
        })
    }
}

/// The words a [`FromSeat::Deny`] or [`FromSeat::Unable`] may give.
const WHYS: &[&str] = &[
    "cancelled",
    "locked",
    "timeout",
    "busy",
    "gone",
    "not-here",
    "no-screen",
];

impl FromSeat {
    /// The record as one packet's bytes. The caller zeroes them once sent:
    /// an answer's holds the secret, escaped.
    #[must_use]
    pub fn encode(&self) -> Option<Vec<u8>> {
        let line = match self {
            FromSeat::Ready => "ready".to_owned(),
            FromSeat::Unable(why) if WHYS.contains(&why.as_str()) => format!("unable {why}"),
            FromSeat::Unable(_) => return None,
            FromSeat::Answer { id, secret } => {
                let mut line = format!("answer {id} ");
                if secret.expose().is_empty() {
                    // An empty answer is one `%`-free byte that escape never
                    // makes alone: `-`, read back as nothing.
                    line.push('-');
                } else {
                    for byte in secret.expose() {
                        line.push_str(&format!("%{byte:02X}"));
                    }
                }
                line
            }
            FromSeat::Allow { id } => format!("allow {id}"),
            FromSeat::Deny { id, why } if WHYS.contains(&why.as_str()) => {
                format!("deny {id} {why}")
            }
            FromSeat::Deny { .. } => return None,
        };
        (line.len() <= MAX_RECORD).then(|| line.into_bytes())
    }

    /// The record `packet` holds, or `None` for anything else. The caller
    /// zeroes `packet` after.
    #[must_use]
    pub fn decode(packet: &[u8]) -> Option<FromSeat> {
        let why = |word: &str| WHYS.contains(&word).then(|| word.to_owned());
        Some(match first(packet)? {
            "ready" => {
                let _ = words(packet, 1)?;
                FromSeat::Ready
            }
            "unable" => FromSeat::Unable(why(words(packet, 2)?.get(1)?)?),
            "answer" => {
                let w = words(packet, 3)?;
                let id = number(w.get(1)?)?;
                let mut bytes = secret_bytes(w.get(2)?)?;
                let secret = Secret::new(&bytes);
                zero(&mut bytes);
                FromSeat::Answer {
                    id,
                    secret: secret?,
                }
            }
            "allow" => FromSeat::Allow {
                id: number(words(packet, 2)?.get(1)?)?,
            },
            "deny" => {
                let w = words(packet, 3)?;
                FromSeat::Deny {
                    id: number(w.get(1)?)?,
                    why: why(w.get(2)?)?,
                }
            }
            _ => return None,
        })
    }
}

/// An answer's field: `%XX` for every byte, or `-` for none.
fn secret_bytes(field: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(field.len() / 3);
    if field == "-" {
        return Some(bytes);
    }
    let mut rest = field.bytes();
    while let Some(byte) = rest.next() {
        let digits = (byte == b'%')
            .then(|| Some((rest.next().and_then(hex)?, rest.next().and_then(hex)?)))
            .flatten();
        let Some((high, low)) = digits else {
            zero(&mut bytes);
            return None;
        };
        bytes.push(high << 4 | low);
    }
    Some(bytes)
}

/// Take the prompt channel `sessiond` left this process, if it left one:
/// close-on-exec, its variable out of the environment, as
/// [`lock::adopt_lock`](crate::lock::adopt_lock) does.
///
/// `None` when the variable is not set; `Some(None)` when it names no
/// socket this process holds; `Some(Some(_))`: the channel.
///
/// Call it first thing in `main`, before any thread or child: it changes the
/// environment.
#[must_use]
pub fn adopt_prompt() -> Option<Option<UnixDatagram>> {
    let named = std::env::var(PROMPT_FD_VARIABLE).ok()?;
    #[expect(
        unsafe_code,
        reason = "AUDIT: the environment is changed before the compositor starts a thread; adopt_prompt's caller promises it"
    )]
    // SAFETY: called at the start of `main`, before any other thread exists
    // to read the environment at the same time.
    unsafe {
        std::env::remove_var(PROMPT_FD_VARIABLE);
    }
    let fd: Option<i32> = named.parse().ok().filter(|fd| *fd > 2);
    Some(fd.filter(|fd| crate::is_socket(*fd)).map(|fd| {
        crate::set_cloexec(fd);
        #[expect(
            unsafe_code,
            reason = "AUDIT: the descriptor sessiond gave this process, named by the variable just removed; nothing else here owns it"
        )]
        // SAFETY: `fd` is an open socket (fstat said so) and is the prompt
        // channel's, which no other owner in this process holds.
        unsafe {
            UnixDatagram::from_raw_fd(fd)
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_seat() -> Vec<ToSeat> {
        vec![
            ToSeat::Hello { uid: 1000 },
            ToSeat::Ask {
                id: 7,
                area: "hostname".to_owned(),
                program: "/bin/settings".to_owned(),
                parent: "/bin/hyprix".to_owned(),
                account: "ferrix".to_owned(),
                what: "would call the computer box; it is called ferrix".to_owned(),
            },
            ToSeat::Question {
                id: 7,
                visible: false,
                text: "Password: ".to_owned(),
            },
            ToSeat::Info {
                id: 7,
                text: "wait 16 s".to_owned(),
            },
            ToSeat::Confirm { id: 7 },
            ToSeat::Close {
                id: u64::MAX,
                text: "the computer is called box (was ferrix)".to_owned(),
            },
        ]
    }

    #[test]
    fn every_record_reads_back_as_itself() {
        for record in to_seat() {
            let bytes = record.encode().unwrap();
            assert!(bytes.len() <= MAX_RECORD);
            assert_eq!(ToSeat::decode(&bytes), Some(record));
        }
        for record in [
            FromSeat::Ready,
            FromSeat::Unable("busy".to_owned()),
            FromSeat::Allow { id: 3 },
            FromSeat::Deny {
                id: 3,
                why: "cancelled".to_owned(),
            },
            FromSeat::Answer {
                id: 3,
                secret: Secret::new(b"horse battery %20 staple\n").unwrap(),
            },
            FromSeat::Answer {
                id: 3,
                secret: Secret::new(b"").unwrap(),
            },
        ] {
            let bytes = record.encode().unwrap();
            assert_eq!(FromSeat::decode(&bytes), Some(record));
        }
    }

    #[test]
    fn a_field_holds_no_space_or_control_and_reads_back_whole() {
        for text in [
            "a b",
            "%",
            "line\nnext",
            "\u{202e}gnp.exe",
            "tab\there",
            "ä",
        ] {
            let escaped = escape(text);
            assert!(escaped.bytes().all(|b| b.is_ascii_graphic()), "{escaped}");
            assert_eq!(unescape(&escaped, MAX_TEXT).as_deref(), Some(text));
        }
        for bad in ["a b", "%2", "%zz", "%2g", "%20%", "\n", "%ff"] {
            assert_eq!(unescape(bad, MAX_TEXT), None, "{bad:?}");
        }
        // Not UTF-8.
        assert_eq!(unescape("%FF", MAX_TEXT), None);
        assert_eq!(unescape(&"a".repeat(MAX_TEXT + 1), MAX_TEXT), None);
    }

    #[test]
    fn anything_else_is_nothing() {
        for bad in [
            &b""[..],
            b"ask",
            b"ask 1 a b c d",
            b"ask 01 a b c d e",
            b"ask 1 a b c d e f",
            b"ask -1 a b c d e",
            b"question 1 2 x",
            b"confirm 1 ",
            b"confirm  1",
            b"close 1",
            b"hello 4294967296",
            b"answer 1 x",
            b"allow 1",
            &[0xff, 0x20, 0x31][..],
        ] {
            assert_eq!(ToSeat::decode(bad), None, "{bad:?}");
        }
        for bad in [
            &b""[..],
            b"ready now",
            b"unable why",
            b"deny 1 because",
            b"deny 1",
            b"answer 1",
            b"answer 1 abc",
            b"answer 1 %4",
            b"answer 1 %41 extra",
            b"allow x",
            b"confirm 1",
        ] {
            assert_eq!(FromSeat::decode(bad), None, "{bad:?}");
        }
        let long = format!("info 1 {}", "a".repeat(MAX_RECORD));
        assert_eq!(ToSeat::decode(long.as_bytes()), None);
        assert_eq!(
            FromSeat::Deny {
                id: 1,
                why: "because".to_owned()
            }
            .encode(),
            None
        );
    }

    #[test]
    fn a_secret_is_in_no_debug() {
        let answer = FromSeat::Answer {
            id: 1,
            secret: Secret::new(b"horse").unwrap(),
        };
        assert!(!format!("{answer:?}").contains("horse"));
        assert!(Secret::new(&[0; MAX_SECRET + 1]).is_none());
    }
}
