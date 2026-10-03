//! Unit names, as `src/basic/unit-name.c` has them.
//!
//! A name is a prefix, an optional `@instance`, and a suffix naming the
//! unit's [`UnitType`]: `sshd.service`, `getty@console.service`. A name whose
//! instance is empty, `getty@.service`, is a *template*: it is never started
//! itself, and `getty@console.service` is loaded from it when no file of its
//! own exists (§4.1).
//!
//! Slices name their place in the tree: `user-1000.slice` is a child of
//! `user.slice`, which is a child of the root, `-.slice`. Mount units name
//! the path they mount, escaped: `/sys/fs/cgroup` is `sys-fs-cgroup.mount`.
//! Device units name a node under `/dev` the same way: `/dev/dri/card0` is
//! `dev-dri-card0.device` ([`UnitName::device_path`]).

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// The longest unit name: systemd's `UNIT_NAME_MAX`, less its terminator.
pub const NAME_MAX: usize = 255;

/// What a unit is, named by its suffix (§4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnitType {
    /// Processes init starts, in a cgroup of their own.
    Service,
    /// A branch of the cgroup tree.
    Slice,
    /// Processes init did not start, grouped on request.
    Scope,
    /// A named point in boot.
    Target,
    /// A mount point.
    Mount,
    /// A listening socket; version 2.
    Socket,
    /// Something the kernel provides, always active (§7.2).
    Builtin,
    /// A node under `/dev`, active while it exists.
    Device,
}

impl UnitType {
    /// Every type, in suffix order.
    pub const ALL: [UnitType; 8] = [
        UnitType::Service,
        UnitType::Slice,
        UnitType::Scope,
        UnitType::Target,
        UnitType::Mount,
        UnitType::Socket,
        UnitType::Builtin,
        UnitType::Device,
    ];

    /// The suffix, without its dot.
    pub fn suffix(self) -> &'static str {
        match self {
            UnitType::Service => "service",
            UnitType::Slice => "slice",
            UnitType::Scope => "scope",
            UnitType::Target => "target",
            UnitType::Mount => "mount",
            UnitType::Socket => "socket",
            UnitType::Builtin => "builtin",
            UnitType::Device => "device",
        }
    }

    /// The type a suffix names, without its dot.
    pub fn from_suffix(suffix: &str) -> Option<UnitType> {
        UnitType::ALL
            .into_iter()
            .find(|kind| kind.suffix() == suffix)
    }
}

/// Why a string is not a unit name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    /// Empty, or longer than [`NAME_MAX`].
    Length,
    /// No suffix, or one that names no [`UnitType`].
    Suffix,
    /// A character outside `[A-Za-z0-9:_.\\-]`, `@` apart.
    Character,
    /// An empty prefix, as in `@x.service` or `.service`.
    Prefix,
    /// A slice name with a leading, trailing or doubled `-`.
    Slice,
    /// A path that cannot be a mount point's: relative, or with `..`; or a
    /// device unit's name that is not a node under `/dev`.
    Path,
    /// A template asked for where an instance or a plain name was needed,
    /// or the other way round.
    Template,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NameError::Length => "empty, or longer than 255 bytes",
            NameError::Suffix => "no known unit type suffix",
            NameError::Character => "a character a unit name may not have",
            NameError::Prefix => "an empty prefix",
            NameError::Slice => "a slice name with a stray '-'",
            NameError::Path => "not an absolute, normalized path",
            NameError::Template => "a template where a unit was needed, or the reverse",
        })
    }
}

/// A valid unit name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnitName {
    full: String,
    /// Where the prefix ends: the `@`, or the suffix's dot.
    prefix_end: usize,
    /// Where the suffix's dot is.
    dot: usize,
    kind: UnitType,
}

/// A character a unit name may have besides `@`: systemd's `VALID_CHARS`.
fn valid_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.' | '\\')
}

impl UnitName {
    /// Check `text` and make it a name.
    ///
    /// # Errors
    ///
    /// Why it is not one.
    pub fn parse(text: &str) -> Result<UnitName, NameError> {
        if text.is_empty() || text.len() > NAME_MAX {
            return Err(NameError::Length);
        }
        let (stem, suffix) = text.rsplit_once('.').ok_or(NameError::Suffix)?;
        let kind = UnitType::from_suffix(suffix).ok_or(NameError::Suffix)?;
        if stem.is_empty() {
            return Err(NameError::Prefix);
        }
        if !stem.chars().all(|c| c == '@' || valid_char(c)) {
            return Err(NameError::Character);
        }
        let prefix_end = stem.find('@').unwrap_or(stem.len());
        if prefix_end == 0 {
            return Err(NameError::Prefix);
        }
        let name = UnitName {
            full: text.to_owned(),
            prefix_end,
            dot: stem.len(),
            kind,
        };
        if kind == UnitType::Slice && !name.slice_is_valid() {
            return Err(NameError::Slice);
        }
        Ok(name)
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.full
    }

    /// Its type.
    pub fn unit_type(&self) -> UnitType {
        self.kind
    }

    /// Everything before the suffix's dot: `%N`.
    pub fn stem(&self) -> &str {
        self.full.get(..self.dot).unwrap_or_default()
    }

    /// Everything before the `@`, or the stem if there is none: `%p`.
    pub fn prefix(&self) -> &str {
        self.full.get(..self.prefix_end).unwrap_or_default()
    }

    /// The instance: `None` for a plain name, `Some("")` for a template.
    pub fn instance(&self) -> Option<&str> {
        if self.prefix_end == self.dot {
            return None;
        }
        self.full.get(self.prefix_end + 1..self.dot)
    }

    /// Whether this is a template, `name@.type`.
    pub fn is_template(&self) -> bool {
        self.instance() == Some("")
    }

    /// Whether this is an instance of a template, `name@instance.type`.
    pub fn is_instance(&self) -> bool {
        self.instance().is_some_and(|instance| !instance.is_empty())
    }

    /// The template an instance is made from.
    pub fn template(&self) -> Option<UnitName> {
        if !self.is_instance() {
            return None;
        }
        UnitName::parse(&format!("{}@.{}", self.prefix(), self.kind.suffix())).ok()
    }

    /// The instance `instance` of this template.
    ///
    /// # Errors
    ///
    /// [`NameError::Template`] if this is not a template, and whatever makes
    /// the result not a name.
    pub fn instantiate(&self, instance: &str) -> Result<UnitName, NameError> {
        if !self.is_template() || instance.is_empty() {
            return Err(NameError::Template);
        }
        UnitName::parse(&format!(
            "{}@{instance}.{}",
            self.prefix(),
            self.kind.suffix()
        ))
    }

    /// The same prefix and instance under another type's suffix: how a
    /// `.socket` finds its `.service`.
    pub fn with_type(&self, kind: UnitType) -> Option<UnitName> {
        UnitName::parse(&format!("{}.{}", self.stem(), kind.suffix())).ok()
    }

    /// systemd's `slice_name_is_valid`: `-.slice`, or dash-separated parts
    /// none of which is empty.
    fn slice_is_valid(&self) -> bool {
        let stem = self.stem();
        stem == "-" || (!stem.contains('@') && stem.split('-').all(|part| !part.is_empty()))
    }

    /// The root slice, `-.slice`.
    pub fn root_slice() -> UnitName {
        UnitName {
            full: "-.slice".to_owned(),
            prefix_end: 1,
            dot: 1,
            kind: UnitType::Slice,
        }
    }

    /// The slice a slice sits in: `a-b.slice` in `a.slice`, `a.slice` in
    /// `-.slice`, and `-.slice` in none.
    pub fn slice_parent(&self) -> Option<UnitName> {
        if self.kind != UnitType::Slice || self.stem() == "-" {
            return None;
        }
        match self.stem().rsplit_once('-') {
            Some((parent, _)) => UnitName::parse(&format!("{parent}.slice")).ok(),
            None => Some(UnitName::root_slice()),
        }
    }

    /// The mount unit for `path`: systemd's `unit_name_from_path`.
    ///
    /// # Errors
    ///
    /// [`NameError::Path`] for a relative path or one with `..`, and
    /// [`NameError::Length`] when the escaped name is too long.
    pub fn for_path(path: &str, kind: UnitType) -> Result<UnitName, NameError> {
        let escaped = escape_path(path)?;
        UnitName::parse(&format!("{escaped}.{}", kind.suffix()))
    }

    /// The node a device unit names: `dev-dri-card0.device` is
    /// `/dev/dri/card0`, and `dev-disk-by\x2dlabel-root.device` is
    /// `/dev/disk/by-label/root`. The name is the whole contract, so it must
    /// be the one [`UnitName::for_path`] makes of the path.
    ///
    /// # Errors
    ///
    /// [`NameError::Path`] for a name that is not a device unit's, that does
    /// not unescape to a path below `/dev`, or that is not that path's own
    /// escaping (`dev--dri.device`, `dev-a@b.device`).
    pub fn device_path(&self) -> Result<String, NameError> {
        if self.kind != UnitType::Device {
            return Err(NameError::Path);
        }
        let path = format!("/{}", unescape(self.stem()));
        let parts = path_components(&path)?;
        if parts.len() < 2 || parts.first() != Some(&"dev") {
            return Err(NameError::Path);
        }
        if UnitName::for_path(&path, UnitType::Device).as_ref() != Ok(self) {
            return Err(NameError::Path);
        }
        Ok(path)
    }
}

impl fmt::Display for UnitName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.full)
    }
}

/// The components of an absolute path, `.` and empty ones dropped.
///
/// # Errors
///
/// [`NameError::Path`] for a relative path or one with `..`.
pub fn path_components(path: &str) -> Result<Vec<&str>, NameError> {
    let rest = path.strip_prefix('/').ok_or(NameError::Path)?;
    let parts: Vec<&str> = rest
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.contains(&"..") {
        return Err(NameError::Path);
    }
    Ok(parts)
}

/// `path` as a unit name's stem: systemd's `unit_name_path_escape`.
///
/// Slashes become `-`, and every byte that is not alphanumeric, `:`, `_` or
/// a `.` other than the first is written `\xNN`; `/` alone is `-`.
///
/// # Errors
///
/// [`NameError::Path`] for a relative path or one with `..`.
pub fn escape_path(path: &str) -> Result<String, NameError> {
    let parts = path_components(path)?;
    if parts.is_empty() {
        return Ok("-".to_owned());
    }
    let joined = parts.join("/");
    let mut out = String::with_capacity(joined.len());
    for (index, byte) in joined.bytes().enumerate() {
        match byte {
            b'/' => out.push('-'),
            b'.' if index > 0 => out.push('.'),
            b':' | b'_' => out.push(char::from(byte)),
            _ if byte.is_ascii_alphanumeric() => out.push(char::from(byte)),
            _ => out.push_str(&format!("\\x{byte:02x}")),
        }
    }
    Ok(out)
}

/// Undo [`escape_path`]'s escaping of an instance or a stem: `-` becomes
/// `/` and `\xNN` the byte it names, as `%I` and `%P` print it. Bytes that
/// do not make UTF-8 are replaced.
pub fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let hex = bytes
            .get(index + 2..index + 4)
            .filter(|_| byte == b'\\' && bytes.get(index + 1) == Some(&b'x'))
            .filter(|digits| digits.iter().all(u8::is_ascii_hexdigit))
            .and_then(|digits| core::str::from_utf8(digits).ok())
            .and_then(|digits| u8::from_str_radix(digits, 16).ok());
        match (byte, hex) {
            (_, Some(value)) => {
                out.push(value);
                index += 4;
            }
            (b'-', None) => {
                out.push(b'/');
                index += 1;
            }
            (_, None) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
