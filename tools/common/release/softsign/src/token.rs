//! The PKCS #11 3.1 surface the signing tooling uses (`docs/SIGNING.md`
//! §6.4, (D3)), and the soft token behind it.
//!
//! [`Token`] is the subset of PKCS #11 3.1 an HSM is driven through for
//! HSS: `C_GenerateKeyPair` with `CKM_HSS_KEY_PAIR_GEN`,
//! `C_GetAttributeValue` for `CKA_VALUE` and `CKA_HSS_KEYS_REMAINING`, and
//! `C_Sign` with `CKM_HSS`. [`SoftToken`] implements it with keys in files.
//! A PKCS #11 module behind the same trait is slice S6's, so moving to an
//! HSM changes the configured module and the anchors, not the tooling.
//!
//! The soft token keeps the HSM's state rule in software (D2): the next
//! leaf is written, `fsync`ed and renamed into place, and the directory
//! `fsync`ed, before a signature is computed; the leaf and the message's
//! digest are appended to the key's own log, `fsync`ed, before the
//! signature is returned; and a state whose next leaf is not above every
//! leaf the log holds -- an older state file restored -- is refused.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::lms::{self, Key, LMOTS_SHA256_N24_W4, N};

/// `CKM_HSS_KEY_PAIR_GEN` (pkcs11t.h 3.01).
pub const CKM_HSS_KEY_PAIR_GEN: u64 = 0x0000_4032;
/// `CKM_HSS`.
pub const CKM_HSS: u64 = 0x0000_4033;
/// `CKK_HSS`.
pub const CKK_HSS: u64 = 0x0000_0046;
/// `CKA_VALUE`.
pub const CKA_VALUE: u64 = 0x0000_0011;
/// `CKA_HSS_LEVELS`.
pub const CKA_HSS_LEVELS: u64 = 0x0000_0617;
/// `CKA_HSS_LMS_TYPE`.
pub const CKA_HSS_LMS_TYPE: u64 = 0x0000_0618;
/// `CKA_HSS_LMOTS_TYPE`.
pub const CKA_HSS_LMOTS_TYPE: u64 = 0x0000_0619;
/// `CKA_HSS_KEYS_REMAINING`.
pub const CKA_HSS_KEYS_REMAINING: u64 = 0x0000_061C;

/// The first line of every soft-token state file. `check-no-dev-keys.py`
/// refuses any tracked file that holds it (D5).
pub const STATE_MAGIC: &str = "FXSOFTSIGN-STATE-1";

/// Why the token refused.
#[derive(Debug)]
pub enum Error {
    /// A mechanism, key type or attribute outside the subset.
    Mechanism(u64),
    /// Parameters the soft token does not make (only `L = 1`,
    /// `LMOTS_SHA256_N24_W4` and `LMS_SHA256_M24_H10`, `_H15` and `_H20`).
    Parameters,
    /// No such key.
    NoKey(String),
    /// The key already exists.
    Exists(String),
    /// The state file cannot be read, parsed, locked or advanced.
    State(String),
    /// The state's next leaf is not above every leaf its log holds: an
    /// older state file was restored (D2).
    Reuse {
        /// The leaf the state would use.
        next: u32,
        /// The highest leaf the log holds.
        logged: u32,
    },
    /// Every leaf is used.
    Exhausted,
    /// The key's public key is a release anchor (D1).
    ReleaseKey,
    /// The key directory is inside a git work tree (D5).
    InRepository(PathBuf),
    /// An I/O error.
    Io(io::Error),
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Error {
        Error::Io(error)
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Mechanism(m) => write!(f, "mechanism or attribute {m:#x} is not in the subset"),
            Error::Parameters => write!(
                f,
                "only L = 1, LMOTS_SHA256_N24_W4 and LMS_SHA256_M24_H10/H15/H20"
            ),
            Error::NoKey(k) => write!(f, "no key {k}"),
            Error::Exists(k) => write!(f, "key {k} exists"),
            Error::State(why) => write!(f, "state refused: {why}"),
            Error::Reuse { next, logged } => write!(
                f,
                "state's next leaf {next} is not above leaf {logged} in the log: an older state was restored; refusing"
            ),
            Error::Exhausted => write!(f, "every leaf is used"),
            Error::ReleaseKey => write!(
                f,
                "the key is a release anchor; the soft signer never signs with one"
            ),
            Error::InRepository(p) => write!(
                f,
                "{} is inside a git work tree; development keys live outside it",
                p.display()
            ),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

/// A key on a token, by its label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectHandle(pub String);

/// The PKCS #11 3.1 HSS subset (D3).
pub trait Token {
    /// `C_GenerateKeyPair(CKM_HSS_KEY_PAIR_GEN)` with `CKA_HSS_LEVELS`,
    /// `CKA_HSS_LMS_TYPE` and `CKA_HSS_LMOTS_TYPE`.
    fn generate_key_pair(
        &mut self,
        mechanism: u64,
        label: &str,
        levels: u64,
        lms_type: u32,
        lmots_type: u32,
    ) -> Result<ObjectHandle, Error>;

    /// `C_GetAttributeValue`: `CKA_VALUE` (the HSS public key) or
    /// `CKA_HSS_KEYS_REMAINING` (eight bytes, big-endian).
    fn get_attribute(&self, key: &ObjectHandle, attribute: u64) -> Result<Vec<u8>, Error>;

    /// `C_Sign(CKM_HSS)`: an HSS signature of `data`.
    fn sign(&mut self, key: &ObjectHandle, mechanism: u64, data: &[u8]) -> Result<Vec<u8>, Error>;
}

/// Keys in files, under one directory outside any git work tree.
#[derive(Debug)]
pub struct SoftToken {
    root: PathBuf,
    release_anchors: Vec<Vec<u8>>,
}

/// One key's state.
struct State {
    key: Key,
    next: u32,
}

fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn unhex<const L: usize>(text: &str) -> Option<[u8; L]> {
    let text = text.trim();
    if text.len() != 2 * L || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; L];
    for (n, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(text.get(2 * n..2 * n + 2)?, 16).ok()?;
    }
    Some(out)
}

fn sync_dir(dir: &Path) -> Result<(), Error> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn inside_git_tree(dir: &Path) -> Option<PathBuf> {
    let mut at = Some(dir);
    while let Some(d) = at {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        at = d.parent();
    }
    None
}

impl State {
    fn render(&self) -> String {
        format!(
            "{STATE_MAGIC}\nlms_type {:#010x}\nlmots_type {LMOTS_SHA256_N24_W4:#010x}\nid {}\nseed {}\nnext {}\n",
            self.key.lms_type,
            hex(&self.key.id),
            hex(&self.key.seed),
            self.next
        )
    }

    fn parse(text: &str) -> Option<State> {
        let mut lines = text.lines();
        if lines.next()? != STATE_MAGIC {
            return None;
        }
        let mut field = |name: &str| -> Option<String> {
            let line = lines.next()?;
            line.strip_prefix(name)?
                .strip_prefix(' ')
                .map(str::to_owned)
        };
        let lms_type = u32::from_str_radix(field("lms_type")?.trim_start_matches("0x"), 16).ok()?;
        let lmots = u32::from_str_radix(field("lmots_type")?.trim_start_matches("0x"), 16).ok()?;
        let id = unhex::<16>(&field("id")?)?;
        let seed = unhex::<N>(&field("seed")?)?;
        let next = field("next")?.parse().ok()?;
        if lms::height(lms_type).is_none() || lmots != LMOTS_SHA256_N24_W4 {
            return None;
        }
        Some(State {
            key: Key { lms_type, id, seed },
            next,
        })
    }
}

impl SoftToken {
    /// A token on `root`, which must be outside any git work tree (D5),
    /// refusing to sign with any key whose HSS public key is one of
    /// `release_anchors` (D1).
    pub fn open(root: &Path, release_anchors: Vec<Vec<u8>>) -> Result<SoftToken, Error> {
        fs::create_dir_all(root)?;
        owner_only_directory(root)?;
        let root = root.canonicalize()?;
        if let Some(tree) = inside_git_tree(&root) {
            return Err(Error::InRepository(tree));
        }
        Ok(SoftToken {
            root,
            release_anchors,
        })
    }

    fn dir(&self, label: &str) -> Result<PathBuf, Error> {
        if label.is_empty()
            || !label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(Error::NoKey(label.to_owned()));
        }
        Ok(self.root.join(label))
    }

    fn read_state(dir: &Path) -> Result<State, Error> {
        let mut text = String::new();
        let _ = File::open(dir.join("state"))
            .map_err(|e| Error::State(format!("cannot open: {e}")))?
            .read_to_string(&mut text)
            .map_err(|e| Error::State(format!("cannot read: {e}")))?;
        State::parse(&text).ok_or_else(|| Error::State("cannot parse".to_owned()))
    }

    fn write_state(dir: &Path, state: &State) -> Result<(), Error> {
        let tmp = dir.join("state.tmp");
        {
            let mut file = owner_only(OpenOptions::new())
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .map_err(|e| Error::State(format!("cannot write: {e}")))?;
            file.write_all(state.render().as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, dir.join("state"))
            .map_err(|e| Error::State(format!("cannot advance: {e}")))?;
        sync_dir(dir)
    }

    /// The highest leaf the key's log holds, if any.
    fn logged_max(dir: &Path) -> Result<Option<u32>, Error> {
        let text = match fs::read_to_string(dir.join("log")) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::State(format!("cannot read the log: {e}"))),
        };
        let mut max = None;
        for line in text.lines() {
            let q: u32 = line
                .split_whitespace()
                .nth(1)
                .and_then(|q| q.parse().ok())
                .ok_or_else(|| Error::State("cannot parse the log".to_owned()))?;
            max = Some(max.map_or(q, |m: u32| m.max(q)));
        }
        Ok(max)
    }

    fn lock(dir: &Path) -> Result<File, Error> {
        let lock = owner_only(OpenOptions::new())
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("lock"))
            .map_err(|e| Error::State(format!("cannot lock: {e}")))?;
        lock.lock()
            .map_err(|e| Error::State(format!("cannot lock: {e}")))?;
        Ok(lock)
    }

    fn tree(dir: &Path, key: &Key) -> Result<Vec<[u8; N]>, Error> {
        let bytes =
            fs::read(dir.join("tree")).map_err(|e| Error::State(format!("no tree: {e}")))?;
        let nodes: Vec<[u8; N]> = bytes
            .chunks_exact(N)
            .filter_map(|c| <[u8; N]>::try_from(c).ok())
            .collect();
        let expect = usize::try_from(2 * key.leaves()).unwrap_or(0);
        if nodes.len() != expect {
            return Err(Error::State("the tree is not the key's size".to_owned()));
        }
        Ok(nodes)
    }

    fn hss_public(key: &Key, tree: &[[u8; N]]) -> Vec<u8> {
        let mut out = 1u32.to_be_bytes().to_vec();
        out.extend(key.public_key(&tree.get(1).copied().unwrap_or_default()));
        out
    }

    /// Generate a key from a given seed and `I` (tests reproduce published
    /// vectors with it). [`Token::generate_key_pair`] draws them from the
    /// operating system.
    pub fn generate_from(
        &mut self,
        label: &str,
        lms_type: u32,
        id: [u8; 16],
        seed: [u8; N],
    ) -> Result<ObjectHandle, Error> {
        if lms::height(lms_type).is_none() {
            return Err(Error::Parameters);
        }
        let dir = self.dir(label)?;
        if dir.exists() {
            return Err(Error::Exists(label.to_owned()));
        }
        #[expect(
            clippy::create_dir,
            reason = "a key directory that already exists must not be reused, so only its own level is created"
        )]
        fs::create_dir(&dir)?;
        owner_only_directory(&dir)?;
        let _lock = SoftToken::lock(&dir)?;
        let key = Key { lms_type, id, seed };
        let tree = key.tree();
        let mut bytes = Vec::with_capacity(tree.len() * N);
        for node in &tree {
            bytes.extend_from_slice(node);
        }
        fs::write(dir.join("tree"), bytes)?;
        fs::write(
            dir.join("public"),
            format!("{}\n", hex(&SoftToken::hss_public(&key, &tree))),
        )?;
        SoftToken::write_state(&dir, &State { key, next: 0 })?;
        Ok(ObjectHandle(label.to_owned()))
    }
}

fn random<const L: usize>() -> Result<[u8; L], Error> {
    let mut out = [0u8; L];
    File::open("/dev/urandom")?.read_exact(&mut out)?;
    Ok(out)
}

impl Token for SoftToken {
    fn generate_key_pair(
        &mut self,
        mechanism: u64,
        label: &str,
        levels: u64,
        lms_type: u32,
        lmots_type: u32,
    ) -> Result<ObjectHandle, Error> {
        if mechanism != CKM_HSS_KEY_PAIR_GEN {
            return Err(Error::Mechanism(mechanism));
        }
        if levels != 1 || lmots_type != LMOTS_SHA256_N24_W4 {
            return Err(Error::Parameters);
        }
        self.generate_from(label, lms_type, random::<16>()?, random::<N>()?)
    }

    fn get_attribute(&self, key: &ObjectHandle, attribute: u64) -> Result<Vec<u8>, Error> {
        let dir = self.dir(&key.0)?;
        if !dir.exists() {
            return Err(Error::NoKey(key.0.clone()));
        }
        let state = SoftToken::read_state(&dir)?;
        match attribute {
            CKA_VALUE => {
                let tree = SoftToken::tree(&dir, &state.key)?;
                Ok(SoftToken::hss_public(&state.key, &tree))
            }
            CKA_HSS_KEYS_REMAINING => {
                let left = u64::from(state.key.leaves().saturating_sub(state.next));
                Ok(left.to_be_bytes().to_vec())
            }
            other => Err(Error::Mechanism(other)),
        }
    }

    fn sign(&mut self, key: &ObjectHandle, mechanism: u64, data: &[u8]) -> Result<Vec<u8>, Error> {
        if mechanism != CKM_HSS {
            return Err(Error::Mechanism(mechanism));
        }
        let dir = self.dir(&key.0)?;
        if !dir.exists() {
            return Err(Error::NoKey(key.0.clone()));
        }
        let _lock = SoftToken::lock(&dir)?;
        let state = SoftToken::read_state(&dir)?;
        let tree = SoftToken::tree(&dir, &state.key)?;
        if self
            .release_anchors
            .contains(&SoftToken::hss_public(&state.key, &tree))
        {
            return Err(Error::ReleaseKey);
        }
        if let Some(logged) = SoftToken::logged_max(&dir)?
            && state.next <= logged
        {
            return Err(Error::Reuse {
                next: state.next,
                logged,
            });
        }
        let q = state.next;
        if q >= state.key.leaves() {
            return Err(Error::Exhausted);
        }
        // Commit the advanced state before anything of the signature exists.
        SoftToken::write_state(
            &dir,
            &State {
                key: state.key.clone(),
                next: q + 1,
            },
        )?;
        let signature = state.key.sign(q, data, &tree);
        {
            let mut log = owner_only(OpenOptions::new())
                .append(true)
                .create(true)
                .open(dir.join("log"))?;
            writeln!(
                log,
                "{} {q} {}",
                hex(&state.key.id),
                hex(&ferrix_sha256::digest(data))
            )?;
            log.sync_all()?;
        }
        sync_dir(&dir)?;
        let mut out = 0u32.to_be_bytes().to_vec();
        out.extend(signature);
        Ok(out)
    }
}

/// `options` making a file only its owner may read or write: mode 0600 on
/// Unix. Elsewhere -- the host-test job on Windows builds every host crate
/// -- the file takes its directory's access list; the soft signer holds
/// development keys only (D1), and its keys and state are made on Unix.
fn owner_only(options: OpenOptions) -> OpenOptions {
    #[cfg(unix)]
    let options = {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = options;
        let _ = options.mode(0o600);
        options
    };
    options
}

/// Make `dir` only its owner's: mode 0700 on Unix, as [`owner_only`] says
/// of files.
fn owner_only_directory(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}
