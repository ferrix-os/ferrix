//! Signing in CI with the CI development key (`docs/SIGNING.md` §6.5; the
//! consultant's OK IF (1)-(8), ledger 4005).
//!
//! GitHub runners share no state, so a CI key's leaves are not tracked in a
//! state file. Each workflow run attempt owns a range of leaves that is a
//! pure function of its run number and attempt ([`leaf_range`]): 16 leaves
//! per attempt, at most 4 attempts per run. Within its range a run signs
//! from a fresh log of its own, committing each leaf to that log, fsynced,
//! before the signature exists (D2). Before it signs anything it refuses a
//! range that any earlier run's log already used (a renamed workflow, a
//! second workflow or a reset run number would do that), and [`audit`]
//! refuses any `(I, q)` logged twice.
//!
//! The key's private part arrives in an environment variable, as the
//! secret text [`render_secret`] writes; nothing of it is printed or
//! written to disk by this module.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::ops::Range;
use std::path::Path;

use crate::lms::{Key, LMS_SHA256_M24_H10, LMS_SHA256_M24_H15, LMS_SHA256_M24_H20, N};

/// Leaves one run attempt may use.
pub const LEAVES_PER_ATTEMPT: u32 = 16;
/// Attempts of one run that may sign; a fifth is refused.
pub const MAX_ATTEMPTS: u64 = 4;
/// What the secret text begins with.
const SECRET_MAGIC: &str = "FXCIKEY-1";

/// Why CI signing refuses.
#[derive(Debug, PartialEq, Eq)]
pub enum CiError {
    /// A run number of zero, or an attempt outside 1..=4.
    Attempt,
    /// The range runs past the key's leaves: rotate the key.
    Exhausted,
    /// An earlier log already holds a leaf of this range.
    Overlap(u32),
    /// Every leaf of the range is used.
    RangeUsed,
    /// The same `(I, q)` logged twice: rotate the key at once.
    Repeated(String, u32),
    /// The secret text or a log line does not parse.
    Malformed(String),
    /// The key is one of the release anchors (D1).
    ReleaseKey,
    /// The log could not be written and synced before signing.
    Log(String),
    /// The anchors file names no such CI anchor, or no release table.
    Anchors(String),
    /// The key expired: rotate it (§6.5).
    Expired(String),
    /// The key is not the one the anchors file lists for this anchor id.
    WrongKey,
}

impl core::fmt::Display for CiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CiError::Attempt => f.write_str("a run number of zero or an attempt outside 1..=4"),
            CiError::Exhausted => {
                f.write_str("the range runs past the key's leaves: rotate the key")
            }
            CiError::Overlap(q) => write!(
                f,
                "an earlier run's log already holds leaf {q} of this range"
            ),
            CiError::RangeUsed => f.write_str("every leaf of this run attempt's range is used"),
            CiError::Repeated(id, q) => {
                write!(f, "(I {id}, q {q}) is logged twice: rotate the key at once")
            }
            CiError::Malformed(what) => write!(f, "malformed {what}"),
            CiError::ReleaseKey => f.write_str("the key is a release anchor (D1)"),
            CiError::Log(why) => write!(f, "the signature log: {why}"),
            CiError::Anchors(why) => write!(f, "the anchors file: {why}"),
            CiError::Expired(on) => write!(
                f,
                "the CI key expired on {on}: rotate it (docs/SIGNING.md 6.5)"
            ),
            CiError::WrongKey => {
                f.write_str("the key is not the anchors file's for this anchor id")
            }
        }
    }
}

/// The leaves run `run_number`'s attempt `run_attempt` owns, of a key of
/// `leaves` leaves: `(run_number × 4 + attempt − 1) × 16` and the 15 after.
///
/// # Errors
///
/// [`CiError::Attempt`] for run number 0 or an attempt outside 1..=4;
/// [`CiError::Exhausted`] past the key's leaves.
pub fn leaf_range(run_number: u64, run_attempt: u64, leaves: u32) -> Result<Range<u32>, CiError> {
    if run_number == 0 || !(1..=MAX_ATTEMPTS).contains(&run_attempt) {
        return Err(CiError::Attempt);
    }
    let slot = run_number
        .checked_mul(MAX_ATTEMPTS)
        .and_then(|n| n.checked_add(run_attempt - 1))
        .ok_or(CiError::Exhausted)?;
    let start = slot
        .checked_mul(u64::from(LEAVES_PER_ATTEMPT))
        .ok_or(CiError::Exhausted)?;
    let end = start + u64::from(LEAVES_PER_ATTEMPT);
    if end > u64::from(leaves) {
        return Err(CiError::Exhausted);
    }
    Ok(u32::try_from(start).map_err(|_| CiError::Exhausted)?
        ..u32::try_from(end).map_err(|_| CiError::Exhausted)?)
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
    if text.len() != 2 * L || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; L];
    for (n, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(text.get(2 * n..2 * n + 2)?, 16).ok()?;
    }
    Some(out)
}

/// The secret's text: magic, LMS type, `I` and seed, one line.
#[must_use]
pub fn render_secret(key: &Key) -> String {
    format!(
        "{SECRET_MAGIC} {:#010x} {} {}",
        key.lms_type,
        hex(&key.id),
        hex(&key.seed)
    )
}

/// A key from its secret text.
///
/// # Errors
///
/// [`CiError::Malformed`] for anything else.
pub fn parse_secret(text: &str) -> Result<Key, CiError> {
    let bad = || CiError::Malformed("CI key secret".to_owned());
    let words: Vec<&str> = text.split_whitespace().collect();
    let [magic, lms, id, seed] = words[..] else {
        return Err(bad());
    };
    if magic != SECRET_MAGIC {
        return Err(bad());
    }
    let lms_type = u32::from_str_radix(lms.trim_start_matches("0x"), 16).map_err(|_| bad())?;
    if ![LMS_SHA256_M24_H10, LMS_SHA256_M24_H15, LMS_SHA256_M24_H20].contains(&lms_type) {
        return Err(bad());
    }
    Ok(Key {
        lms_type,
        id: unhex::<16>(id).ok_or_else(bad)?,
        seed: unhex::<N>(seed).ok_or_else(bad)?,
    })
}

/// The `(I, q)` pairs of log text: one `I q digest` line each.
fn entries(log: &str) -> Result<Vec<(String, u32)>, CiError> {
    log.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut words = line.split_whitespace();
            match (
                words.next(),
                words.next().and_then(|q| q.parse().ok()),
                words.next(),
            ) {
                (Some(id), Some(q), Some(digest)) if id.len() == 32 && digest.len() == 64 => {
                    Ok((id.to_owned(), q))
                }
                _ => Err(CiError::Malformed(format!("log line `{line}`"))),
            }
        })
        .collect()
}

/// Refuse log text holding any `(I, q)` twice (condition 4).
///
/// # Errors
///
/// [`CiError::Repeated`], or a line that does not parse.
pub fn audit(log: &str) -> Result<usize, CiError> {
    let mut seen = BTreeSet::new();
    let all = entries(log)?;
    for (id, q) in &all {
        if !seen.insert((id.clone(), *q)) {
            return Err(CiError::Repeated(id.clone(), *q));
        }
    }
    Ok(all.len())
}

/// One run attempt's signer: its key, its tree, its range and its own log.
pub struct Signer<'a> {
    key: Key,
    tree: Vec<[u8; N]>,
    range: Range<u32>,
    log: &'a Path,
    context: String,
}

/// Which run signs: written after each log line's `I q digest` (ledger
/// 4188 (6)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunContext {
    /// `github.run_number`.
    pub run_number: u64,
    /// `github.run_attempt`.
    pub run_attempt: u64,
    /// The workflow's path.
    pub workflow: String,
}

/// Never the key: only which range and log a signer holds.
impl core::fmt::Debug for Signer<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Signer")
            .field("range", &self.range)
            .field("log", &self.log)
            .finish_non_exhaustive()
    }
}

impl<'a> Signer<'a> {
    /// A signer for `range`, refusing a release key and a range that
    /// `prior` (every earlier run's log) already used, before it signs.
    ///
    /// # Errors
    ///
    /// [`CiError::ReleaseKey`], [`CiError::Overlap`], or a malformed log.
    pub fn open(
        key: Key,
        range: Range<u32>,
        prior: &str,
        log: &'a Path,
        release_anchors: &[Vec<u8>],
        context: &RunContext,
    ) -> Result<Signer<'a>, CiError> {
        let id = hex(&key.id);
        if let Some((_, q)) = entries(prior)?
            .into_iter()
            .find(|(logged, q)| *logged == id && range.contains(q))
        {
            return Err(CiError::Overlap(q));
        }
        let tree = key.tree();
        let signer = Signer {
            key,
            tree,
            range,
            log,
            context: format!(
                "{} {} {}",
                context.run_number,
                context.run_attempt,
                context
                    .workflow
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join("_")
            ),
        };
        if release_anchors.contains(&signer.public_key()) {
            return Err(CiError::ReleaseKey);
        }
        Ok(signer)
    }

    /// The HSS public key (`L = 1`).
    #[must_use]
    pub fn public_key(&self) -> Vec<u8> {
        let mut out = 1u32.to_be_bytes().to_vec();
        out.extend(
            self.key
                .public_key(&self.tree.get(1).copied().unwrap_or_default()),
        );
        out
    }

    /// The next leaf of the range, committed to the log, fsynced, before the
    /// HSS signature of `data` (`Nspk = 0`) is computed (D2).
    ///
    /// # Errors
    ///
    /// [`CiError::RangeUsed`] when the range is spent, or a log that cannot
    /// be read, written or synced: then no signature exists.
    pub fn sign(&mut self, data: &[u8]) -> Result<Vec<u8>, CiError> {
        let own = match std::fs::read_to_string(self.log) {
            Ok(text) => entries(&text)?.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(CiError::Log(error.to_string())),
        };
        let q = u32::try_from(own)
            .ok()
            .and_then(|own| self.range.start.checked_add(own))
            .filter(|q| self.range.contains(q))
            .ok_or(CiError::RangeUsed)?;
        self.commit(q, data)?;
        let mut out = 0u32.to_be_bytes().to_vec();
        out.extend(self.key.sign(q, data, &self.tree));
        Ok(out)
    }
}

impl Signer<'_> {
    /// Append leaf `q`'s line to the run's log and sync it: the commit
    /// that comes before the signature (D2).
    fn commit(&self, q: u32, data: &[u8]) -> Result<(), CiError> {
        let mut log = OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.log)
            .map_err(|error| CiError::Log(error.to_string()))?;
        writeln!(
            log,
            "{} {q} {} {}",
            hex(&self.key.id),
            hex(&ferrix_sha256::digest(data)),
            self.context
        )
        .and_then(|()| log.sync_all())
        .map_err(|error| CiError::Log(error.to_string()))
    }
}

/// What the anchors file says of one CI anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiAnchor {
    /// Its HSS public key.
    pub public_key: Vec<u8>,
    /// The day it expires, `YYYY-MM-DD`.
    pub expires: String,
}

/// The `[section.name]` tables of `signing-anchors.toml`, as the file is
/// written: `key = "text"` or `key = number` lines.
fn tables(toml: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut out: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in toml.lines().map(str::trim) {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            out.push((name.to_owned(), Vec::new()));
        } else if let (Some((key, value)), Some((_, fields))) =
            (line.split_once('='), out.last_mut())
        {
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            fields.push((key.trim().to_owned(), value.to_owned()));
        }
    }
    out
}

fn unhex_vec(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len() / 2)
        .map(|n| u8::from_str_radix(text.get(2 * n..2 * n + 2)?, 16).ok())
        .collect()
}

/// The release anchors' keys and the CI anchor `anchor_id`'s entry, from
/// the anchors file's text (ledger 4188 (4), (5)).
///
/// # Errors
///
/// [`CiError::Anchors`] when a release anchor has no key, or no
/// `made = "CI"` slot anchor of that id has a key and an expiry. An empty
/// release table is the file's state until the HSMs exist (§12 Q2).
pub fn read_anchors(toml: &str, anchor_id: u16) -> Result<(Vec<Vec<u8>>, CiAnchor), CiError> {
    let all = tables(toml);
    let field = |fields: &[(String, String)], name: &str| {
        fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    let release = all
        .iter()
        .filter(|(name, _)| name.starts_with("release."))
        .map(|(name, fields)| {
            field(fields, "public_key")
                .and_then(|key| unhex_vec(&key))
                .ok_or_else(|| CiError::Anchors(format!("[{name}] has no public key")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let ci = all
        .iter()
        .find(|(name, fields)| {
            name.starts_with("development.")
                && field(fields, "anchor_id").as_deref() == Some(anchor_id.to_string().as_str())
        })
        .ok_or_else(|| CiError::Anchors(format!("no development anchor {anchor_id}")))?;
    let (name, fields) = ci;
    if field(fields, "made").as_deref() != Some("CI")
        || field(fields, "kind").as_deref() != Some("slot")
    {
        return Err(CiError::Anchors(format!("[{name}] is not a CI slot key")));
    }
    let public_key = field(fields, "public_key")
        .and_then(|key| unhex_vec(&key))
        .ok_or_else(|| CiError::Anchors(format!("[{name}] has no public key")))?;
    let expires = field(fields, "expires")
        .filter(|day| day.len() == 10)
        .ok_or_else(|| CiError::Anchors(format!("[{name}] has no expires day")))?;
    Ok((
        release,
        CiAnchor {
            public_key,
            expires,
        },
    ))
}

/// Today in UTC as `YYYY-MM-DD` (the civil-from-days rule).
#[must_use]
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    day_of(i64::try_from(secs / 86_400).unwrap_or(0))
}

/// Day `days` after 1970-01-01 as `YYYY-MM-DD`.
#[must_use]
pub fn day_of(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Refuse an expired key: on or after its `expires` day (ledger 4188 (4)).
///
/// # Errors
///
/// [`CiError::Expired`].
pub fn refuse_expired(anchor: &CiAnchor, today: &str) -> Result<(), CiError> {
    if today >= anchor.expires.as_str() {
        Err(CiError::Expired(anchor.expires.clone()))
    } else {
        Ok(())
    }
}

/// The anchors file's release keys and CI anchor, from the file `path`
/// names: `ci-sign` does not run without it (ledger 4188 (5)).
///
/// # Errors
///
/// [`CiError::Anchors`] with no path or an unreadable file, and as
/// [`read_anchors`].
pub fn anchors_from(
    path: Option<&str>,
    anchor_id: u16,
) -> Result<(Vec<Vec<u8>>, CiAnchor), CiError> {
    let Some(path) = path else {
        return Err(CiError::Anchors(
            "--anchors is required: tools/common/data/signing-anchors.toml".to_owned(),
        ));
    };
    let toml =
        std::fs::read_to_string(path).map_err(|e| CiError::Anchors(format!("{path}: {e}")))?;
    read_anchors(&toml, anchor_id)
}
