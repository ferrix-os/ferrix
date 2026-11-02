//! `svc`'s control records (`docs/INIT.md` §10).
//!
//! A record is a little-endian `u32` length, then that many bytes: a tag
//! byte and the fields the tag has. A string is a `u32` length and UTF-8; a
//! list a `u32` count and its items; an optional value a byte, 0 for none
//! and 1 before the value. A record longer than [`MAX_RECORD`] is refused
//! before it is read, so a client cannot make init hold more than that for
//! it.
//!
//! The client sends one [`Call`]. Init answers with [`Answer`]s until one
//! that [`Answer::is_final`], then closes the connection.

use alloc::string::String;
use alloc::vec::Vec;

/// The longest record either side takes, header excluded.
pub const MAX_RECORD: usize = 1 << 20;

/// Where init listens.
pub const SOCKET: &str = "/run/ferrix/control";

/// What a client asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `svc status [unit]`: one unit, or every unit init has loaded.
    Status(Option<String>),
    /// `svc list [--failed]`.
    List {
        /// Only the failed ones.
        failed: bool,
    },
    /// `svc start unit`.
    Start(String),
    /// `svc stop unit`.
    Stop(String),
    /// `svc restart unit`.
    Restart(String),
    /// `svc reload unit`: its `ExecReload=`.
    Reload(String),
    /// `svc isolate target`.
    Isolate(String),
    /// `svc reset-failed [unit]`.
    ResetFailed(Option<String>),
    /// `svc poweroff`.
    Poweroff,
    /// `svc reboot`.
    Reboot,
    /// `svc log unit`: the last lines it wrote, at most `lines` of them.
    Log {
        /// The unit.
        unit: String,
        /// How many lines, from the end.
        lines: u32,
    },
    /// `svc daemon-reload`: read the unit directories again.
    DaemonReload,
    /// `svc enable unit`: the links its `[Install]` names.
    Enable(String),
    /// `svc disable unit`.
    Disable(String),
    /// `svc mask unit`.
    Mask(String),
    /// `svc unmask unit`.
    Unmask(String),
    /// `svc set-property unit Key=value… [--persistent]`.
    SetProperty {
        /// The unit.
        unit: String,
        /// `Key=value`, in order.
        assignments: Vec<String>,
        /// Kept in `/etc/ferrix/units` across boots, not in `/run`.
        persistent: bool,
    },
    /// `svc scope`: group processes init did not start (§5.6).
    Scope {
        /// The scope's name.
        unit: String,
        /// The slice it goes under.
        slice: Option<String>,
        /// The processes.
        pids: Vec<u32>,
        /// Whether the scope is a session of the machine's own seat: the
        /// graphical session `sessiond` starts, or `login`'s on the console.
        /// Root's word alone (`docs/INIT.md` §10.1); its processes, and the
        /// scopes a user makes of them, may then turn the machine off.
        seat: bool,
    },
}

impl Call {
    /// Whether the call changes something, and so is root's alone
    /// (§10), a user's own scope aside.
    pub fn changes_state(&self) -> bool {
        !matches!(self, Call::Status(_) | Call::List { .. } | Call::Log { .. })
    }
}

/// One unit, as `svc status` shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitStatus {
    /// Its name.
    pub name: String,
    /// `Description=`.
    pub description: Option<String>,
    /// systemd's load state.
    pub load: String,
    /// Its active state.
    pub active: String,
    /// Its kind's own state.
    pub sub: String,
    /// Its main process.
    pub main: Option<u32>,
    /// How it last ended.
    pub result: Option<String>,
    /// A `notify` service's `STATUS=`.
    pub status: Option<String>,
    /// Its cgroup, below the cgroup2 mount.
    pub cgroup: Option<String>,
    /// Its current or last run's id, `INVOCATION_ID` (`docs/LOGGING.md`
    /// §9.2): what `svc status` reads its lines by.
    pub invocation: Option<String>,
}

/// What init says back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The operation ended so: systemd's job result, `done`, `failed`….
    Done(String),
    /// The call was refused, and why.
    Refused(String),
    /// Units, for `status` and `list`.
    Units(Vec<UnitStatus>),
    /// Lines, for `log`.
    Lines(Vec<String>),
    /// A note along the way: a link made, a file written. Not final.
    Note(String),
}

impl Answer {
    /// Whether nothing follows it.
    pub fn is_final(&self) -> bool {
        !matches!(self, Answer::Note(_))
    }
}

/// Why bytes are not a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// It ends before its fields do.
    Short,
    /// Its length is over [`MAX_RECORD`].
    TooLong,
    /// A tag no record has.
    Tag(u8),
    /// A string that is not UTF-8.
    Utf8,
    /// Bytes after the last field.
    Trailing,
    /// A flag -- a boolean, or whether an optional field is there -- that is
    /// neither 0 nor 1, which no encoding writes.
    Flag(u8),
}

/// A record's writer.
#[derive(Debug, Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn len(&mut self, len: usize) {
        self.u32(u32::try_from(len).unwrap_or(u32::MAX));
    }

    fn str(&mut self, text: &str) {
        self.len(text.len());
        self.bytes.extend_from_slice(text.as_bytes());
    }

    fn opt_str(&mut self, text: Option<&str>) {
        match text {
            None => self.u8(0),
            Some(text) => {
                self.u8(1);
                self.str(text);
            }
        }
    }

    fn opt_u32(&mut self, value: Option<u32>) {
        match value {
            None => self.u8(0),
            Some(value) => {
                self.u8(1);
                self.u32(value);
            }
        }
    }

    fn strs(&mut self, list: &[String]) {
        self.len(list.len());
        for text in list {
            self.str(text);
        }
    }

    /// The record: its length, then its bytes.
    fn finish(self) -> Vec<u8> {
        let mut record = Vec::with_capacity(self.bytes.len() + 4);
        record.extend_from_slice(
            &u32::try_from(self.bytes.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        record.extend_from_slice(&self.bytes);
        record
    }
}

/// A record's reader.
#[derive(Debug)]
struct Reader<'a> {
    rest: &'a [u8],
}

impl Reader<'_> {
    fn take(&mut self, count: usize) -> Result<&[u8], DecodeError> {
        if self.rest.len() < count {
            return Err(DecodeError::Short);
        }
        let (taken, rest) = self.rest.split_at(count);
        self.rest = rest;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        self.take(1)?.first().copied().ok_or(DecodeError::Short)
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.take(4)?;
        let array: [u8; 4] = bytes.try_into().map_err(|_| DecodeError::Short)?;
        Ok(u32::from_le_bytes(array))
    }

    fn len(&mut self) -> Result<usize, DecodeError> {
        let len = usize::try_from(self.u32()?).map_err(|_| DecodeError::TooLong)?;
        // Nothing in a record is longer than what is left of it, so a
        // length past that is short, and never an allocation.
        if len > self.rest.len() {
            return Err(DecodeError::Short);
        }
        Ok(len)
    }

    fn str(&mut self) -> Result<String, DecodeError> {
        let len = self.len()?;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes)
            .map(String::from)
            .map_err(|_| DecodeError::Utf8)
    }

    /// A flag, 0 or 1 as the writer writes one: anything else is refused,
    /// so that every record that decodes encodes back to the same bytes.
    fn flag(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(DecodeError::Flag(other)),
        }
    }

    fn opt_str(&mut self) -> Result<Option<String>, DecodeError> {
        if self.flag()? {
            self.str().map(Some)
        } else {
            Ok(None)
        }
    }

    fn opt_u32(&mut self) -> Result<Option<u32>, DecodeError> {
        if self.flag()? {
            self.u32().map(Some)
        } else {
            Ok(None)
        }
    }

    fn strs(&mut self) -> Result<Vec<String>, DecodeError> {
        let count = self.len()?;
        let mut list = Vec::new();
        for _ in 0..count {
            list.push(self.str()?);
        }
        Ok(list)
    }

    fn end(&self) -> Result<(), DecodeError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::Trailing)
        }
    }
}

impl Call {
    /// The record.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        match self {
            Call::Status(unit) => {
                w.u8(1);
                w.opt_str(unit.as_deref());
            }
            Call::List { failed } => {
                w.u8(2);
                w.u8(u8::from(*failed));
            }
            Call::Start(unit) => {
                w.u8(3);
                w.str(unit);
            }
            Call::Stop(unit) => {
                w.u8(4);
                w.str(unit);
            }
            Call::Restart(unit) => {
                w.u8(5);
                w.str(unit);
            }
            Call::Reload(unit) => {
                w.u8(6);
                w.str(unit);
            }
            Call::Isolate(unit) => {
                w.u8(7);
                w.str(unit);
            }
            Call::ResetFailed(unit) => {
                w.u8(8);
                w.opt_str(unit.as_deref());
            }
            Call::Poweroff => w.u8(9),
            Call::Reboot => w.u8(10),
            Call::Log { unit, lines } => {
                w.u8(11);
                w.str(unit);
                w.u32(*lines);
            }
            Call::DaemonReload => w.u8(12),
            Call::Enable(unit) => {
                w.u8(13);
                w.str(unit);
            }
            Call::Disable(unit) => {
                w.u8(14);
                w.str(unit);
            }
            Call::Mask(unit) => {
                w.u8(15);
                w.str(unit);
            }
            Call::Unmask(unit) => {
                w.u8(16);
                w.str(unit);
            }
            Call::SetProperty {
                unit,
                assignments,
                persistent,
            } => {
                w.u8(17);
                w.str(unit);
                w.strs(assignments);
                w.u8(u8::from(*persistent));
            }
            Call::Scope {
                unit,
                slice,
                pids,
                seat,
            } => {
                // A seat's scope has a tag of its own, so a plain scope is
                // the bytes it always was.
                w.u8(if *seat { 19 } else { 18 });
                w.str(unit);
                w.opt_str(slice.as_deref());
                w.len(pids.len());
                for pid in pids {
                    w.u32(*pid);
                }
            }
        }
        w.finish()
    }

    /// A call from a record's body, the length already taken off.
    ///
    /// # Errors
    ///
    /// When the bytes are not a call.
    pub fn decode(body: &[u8]) -> Result<Call, DecodeError> {
        let mut r = Reader { rest: body };
        let call = match r.u8()? {
            1 => Call::Status(r.opt_str()?),
            2 => Call::List { failed: r.flag()? },
            3 => Call::Start(r.str()?),
            4 => Call::Stop(r.str()?),
            5 => Call::Restart(r.str()?),
            6 => Call::Reload(r.str()?),
            7 => Call::Isolate(r.str()?),
            8 => Call::ResetFailed(r.opt_str()?),
            9 => Call::Poweroff,
            10 => Call::Reboot,
            11 => Call::Log {
                unit: r.str()?,
                lines: r.u32()?,
            },
            12 => Call::DaemonReload,
            13 => Call::Enable(r.str()?),
            14 => Call::Disable(r.str()?),
            15 => Call::Mask(r.str()?),
            16 => Call::Unmask(r.str()?),
            17 => Call::SetProperty {
                unit: r.str()?,
                assignments: r.strs()?,
                persistent: r.flag()?,
            },
            tag @ (18 | 19) => {
                let unit = r.str()?;
                let slice = r.opt_str()?;
                let count = r.len()?;
                let mut pids = Vec::new();
                for _ in 0..count {
                    pids.push(r.u32()?);
                }
                Call::Scope {
                    unit,
                    slice,
                    pids,
                    seat: tag == 19,
                }
            }
            tag => return Err(DecodeError::Tag(tag)),
        };
        r.end()?;
        Ok(call)
    }
}

impl Answer {
    /// The record.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        match self {
            Answer::Done(result) => {
                w.u8(1);
                w.str(result);
            }
            Answer::Refused(why) => {
                w.u8(2);
                w.str(why);
            }
            Answer::Units(units) => {
                w.u8(3);
                w.len(units.len());
                for unit in units {
                    w.str(&unit.name);
                    w.opt_str(unit.description.as_deref());
                    w.str(&unit.load);
                    w.str(&unit.active);
                    w.str(&unit.sub);
                    w.opt_u32(unit.main);
                    w.opt_str(unit.result.as_deref());
                    w.opt_str(unit.status.as_deref());
                    w.opt_str(unit.cgroup.as_deref());
                    w.opt_str(unit.invocation.as_deref());
                }
            }
            Answer::Lines(lines) => {
                w.u8(4);
                w.strs(lines);
            }
            Answer::Note(note) => {
                w.u8(5);
                w.str(note);
            }
        }
        w.finish()
    }

    /// An answer from a record's body.
    ///
    /// # Errors
    ///
    /// When the bytes are not an answer.
    pub fn decode(body: &[u8]) -> Result<Answer, DecodeError> {
        let mut r = Reader { rest: body };
        let answer = match r.u8()? {
            1 => Answer::Done(r.str()?),
            2 => Answer::Refused(r.str()?),
            3 => {
                let count = r.len()?;
                let mut units = Vec::new();
                for _ in 0..count {
                    units.push(UnitStatus {
                        name: r.str()?,
                        description: r.opt_str()?,
                        load: r.str()?,
                        active: r.str()?,
                        sub: r.str()?,
                        main: r.opt_u32()?,
                        result: r.opt_str()?,
                        status: r.opt_str()?,
                        cgroup: r.opt_str()?,
                        invocation: r.opt_str()?,
                    });
                }
                Answer::Units(units)
            }
            4 => Answer::Lines(r.strs()?),
            5 => Answer::Note(r.str()?),
            tag => return Err(DecodeError::Tag(tag)),
        };
        r.end()?;
        Ok(answer)
    }
}

/// Records as they arrive on a stream: bytes go in as they are read, whole
/// records' bodies come out.
#[derive(Debug, Default)]
pub struct Framer {
    buffer: Vec<u8>,
}

impl Framer {
    /// An empty one.
    pub fn new() -> Framer {
        Framer::default()
    }

    /// Bytes read from the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next whole record's body, if one has arrived.
    ///
    /// # Errors
    ///
    /// A length over [`MAX_RECORD`]: the stream is not speaking this
    /// protocol, and nothing after it can be trusted.
    pub fn next_record(&mut self) -> Result<Option<Vec<u8>>, DecodeError> {
        let Some(header) = self.buffer.get(..4) else {
            return Ok(None);
        };
        let array: [u8; 4] = header.try_into().map_err(|_| DecodeError::Short)?;
        let len = usize::try_from(u32::from_le_bytes(array)).map_err(|_| DecodeError::TooLong)?;
        if len > MAX_RECORD {
            return Err(DecodeError::TooLong);
        }
        let Some(body) = self.buffer.get(4..4 + len) else {
            return Ok(None);
        };
        let body = body.to_vec();
        let _ = self.buffer.drain(..4 + len);
        Ok(Some(body))
    }

    /// Whether bytes of an unfinished record are held.
    pub fn is_partial(&self) -> bool {
        !self.buffer.is_empty()
    }
}
