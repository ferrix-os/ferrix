//! Passwords: checking one against a stored hash, and hashing a new one at
//! the cost this machine can bear (`docs/AUTH.md` §5.1).
//!
//! # The cost
//!
//! The first time a password is set, `authd` times one hash at the floor
//! (19 MiB, two passes, one lane: OWASP's minimum) and scales the memory up
//! toward the target time for one check: half a second, or a second on a
//! 32-bit processor. The memory is rounded down to a whole MiB, and held
//! between the floor and a ceiling. At 64 MiB and above, it takes three
//! passes, RFC 9106's second recommendation. The choice is said once, with
//! what the timing read, so a gate's log records it for each machine. A
//! hash carries its own costs, so one made elsewhere still checks here.
//!
//! # What is compared in constant time
//!
//! The tag, by the argon2 crate, and a `$5$`/`$6$` string, by `sha_crypt`.
//! An account with no credential is checked against a hash of the same
//! costs that nothing matches, so how long a FAILED takes does not say
//! whether the account exists.

use std::time::{Duration, Instant};

use ferrix_argon2::phc::Encoded;
use ferrix_argon2::{Block, Inputs, Params, hash};
use ferrix_auth_proto::Secret;

use crate::sha_crypt;

/// The least any hash `authd` makes may cost.
pub(crate) const FLOOR: Params = Params {
    memory_kib: 19_456,
    passes: 2,
    lanes: 1,
};

/// The most memory one check may take, in KiB.
#[cfg(target_pointer_width = "32")]
const CEILING_KIB: u32 = 64 * 1024;
#[cfg(not(target_pointer_width = "32"))]
const CEILING_KIB: u32 = 256 * 1024;
/// A check takes at most this share of what the machine has available.
const AVAILABLE_SHARE: u64 = 4;

/// How long one check should take.
#[cfg(target_pointer_width = "32")]
const TARGET: Duration = Duration::from_millis(1000);
#[cfg(not(target_pointer_width = "32"))]
const TARGET: Duration = Duration::from_millis(500);

/// The salt's length.
const SALT: usize = 16;

/// The tag's length.
const TAG: usize = 32;

/// What checking a password against a stored hash found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Checked {
    /// It matched.
    Match {
        /// It matched a hash of a kind `authd` no longer writes, or at a
        /// lower cost than it now uses: hash it again.
        rehash: bool,
    },
    /// It did not.
    Mismatch,
    /// The hash could not be checked: a kind not known, or memory refused.
    Unusable,
}

/// The costs new hashes get, chosen once.
#[derive(Debug, Default)]
pub(crate) struct Hasher {
    chosen: Option<Params>,
}

impl Hasher {
    /// A hasher that uses `params` and never times the machine.
    #[cfg(test)]
    pub(crate) fn fixed(params: Params) -> Hasher {
        Hasher {
            chosen: Some(params),
        }
    }

    /// The costs a new hash gets here, timing the machine the first time.
    pub(crate) fn params(&mut self, say: &mut dyn FnMut(String)) -> Params {
        if let Some(params) = self.chosen {
            return params;
        }
        let params = calibrate(say);
        self.chosen = Some(params);
        params
    }

    /// A new stored hash of `secret`.
    pub(crate) fn make(&mut self, secret: &Secret, say: &mut dyn FnMut(String)) -> Option<String> {
        let params = self.params(say);
        let mut salt = [0_u8; SALT];
        random(&mut salt).ok()?;
        let mut tag = [0_u8; TAG];
        run(&params, secret.expose(), &salt, &mut tag).ok()?;
        let encoded = Encoded::new(params, &salt, &tag).ok()?;
        tag.fill(0);
        Some(encoded.to_string())
    }

    /// Check `secret` against `stored`.
    pub(crate) fn check(
        &mut self,
        secret: &Secret,
        stored: &str,
        say: &mut dyn FnMut(String),
    ) -> Checked {
        if let Ok(encoded) = Encoded::parse(stored) {
            let Ok(blocks) = encoded.params.blocks() else {
                return Checked::Unusable;
            };
            let Some(mut memory) = memory(blocks) else {
                return Checked::Unusable;
            };
            return match encoded.verify(secret.expose(), &mut memory) {
                Ok(true) => Checked::Match {
                    rehash: weaker(&encoded.params, &self.params(say)),
                },
                Ok(false) => Checked::Mismatch,
                Err(_) => Checked::Unusable,
            };
        }
        match sha_crypt::verify(secret.expose(), stored) {
            Ok(true) => Checked::Match { rehash: true },
            Ok(false) => Checked::Mismatch,
            Err(_) => Checked::Unusable,
        }
    }

    /// Spend what a real check costs, on nothing: for an account that has no
    /// credential, so that its FAILED takes as long as a wrong password's.
    pub(crate) fn decoy(&mut self, secret: &Secret, say: &mut dyn FnMut(String)) {
        let params = self.params(say);
        let mut tag = [0_u8; TAG];
        let _ = run(&params, secret.expose(), b"no such account!", &mut tag);
        tag.fill(0);
        let _ = std::hint::black_box(&tag);
    }
}

/// Whether `stored` costs less than `now`, so a match should be rehashed.
fn weaker(stored: &Params, now: &Params) -> bool {
    stored.memory_kib < now.memory_kib || stored.passes < now.passes
}

/// Argon2id of `password` with `salt` into `tag`.
fn run(params: &Params, password: &[u8], salt: &[u8], tag: &mut [u8]) -> Result<(), ()> {
    let blocks = params.blocks().map_err(|_| ())?;
    let mut memory = memory(blocks).ok_or(())?;
    hash(
        params,
        &Inputs {
            password,
            salt,
            secret: &[],
            associated: &[],
        },
        &mut memory,
        tag,
    )
    .map_err(|_| ())
}

/// `blocks` of zeroed memory, or `None` when the allocator refuses it: a
/// check the machine cannot afford is UNAVAILABLE, not a crash.
fn memory(blocks: usize) -> Option<Vec<Block>> {
    let mut memory = Vec::new();
    memory.try_reserve_exact(blocks).ok()?;
    memory.resize(blocks, Block::ZERO);
    Some(memory)
}

/// Time the floor and choose the costs (the module documentation).
fn calibrate(say: &mut dyn FnMut(String)) -> Params {
    let mut tag = [0_u8; TAG];
    let start = Instant::now();
    if run(&FLOOR, b"calibration", b"calibration salt", &mut tag).is_err() {
        say("authd: argon2id could not be timed; new hashes use the floor".to_owned());
        return FLOOR;
    }
    let took = start.elapsed();
    let available = available_kib();
    let params = scaled(took, available);
    say(format!(
        "authd: argon2id at the floor (m={} t={}) took {} ms; {} KiB available; new hashes use m={} t={} p=1",
        FLOOR.memory_kib,
        FLOOR.passes,
        took.as_millis(),
        available.map_or_else(|| "unknown".to_owned(), |kib| kib.to_string()),
        params.memory_kib,
        params.passes
    ));
    params
}

/// What `/proc/meminfo` says a new workload could have, in KiB, if it says.
fn available_kib() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    available_in(&text)
}

/// `MemAvailable`'s figure in `/proc/meminfo`'s text.
fn available_in(meminfo: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// The costs for a machine on which the floor took `took` and `available` KiB are free
/// for a new workload. A check takes at most a quarter of that, never below the floor: a
/// fast processor in a small machine is otherwise told to take what the machine has not
/// got, and the fault's victim is whoever holds most (ledger 7166 P.3).
fn scaled(took: Duration, available: Option<u64>) -> Params {
    let took_us = took.as_micros().max(1);
    let wanted = u128::from(FLOOR.memory_kib) * TARGET.as_micros() / took_us;
    let affordable = available.map_or(u32::MAX, |kib| {
        u32::try_from(kib / AVAILABLE_SHARE).unwrap_or(u32::MAX)
    });
    let mut memory_kib = u32::try_from(wanted)
        .unwrap_or(u32::MAX)
        .min(CEILING_KIB)
        .min(affordable);
    memory_kib -= memory_kib % 1024;
    let memory_kib = memory_kib.max(FLOOR.memory_kib);
    Params {
        memory_kib,
        passes: if memory_kib >= 64 * 1024 {
            3
        } else {
            FLOOR.passes
        },
        lanes: 1,
    }
}

/// Fill `out` from the kernel's generator.
pub(crate) fn random(out: &mut [u8]) -> std::io::Result<()> {
    let mut filled = 0;
    while filled < out.len() {
        let rest = out.get_mut(filled..).unwrap_or(&mut []);
        // SAFETY: `rest` is valid for writes of its length, which is what is
        // passed; no flags.
        let got = unsafe { libc::getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) };
        let got = usize::try_from(got).map_err(|_| std::io::Error::last_os_error())?;
        filled += got;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fast processor in a small machine: at most a quarter of what is available, and
    /// a machine with less than four floors still pays the floor.
    #[test]
    fn a_check_takes_a_share_of_what_is_available() {
        let fast = Duration::from_micros(1);
        assert_eq!(scaled(fast, Some(100 * 1024)).memory_kib, 25 * 1024);
        assert_eq!(scaled(fast, Some(1024)), FLOOR);
        assert_eq!(scaled(fast, Some(u64::MAX)).memory_kib, CEILING_KIB);
        assert_eq!(
            available_in("MemTotal: 9 kB\nMemAvailable:    1234 kB\n"),
            Some(1234)
        );
        assert_eq!(available_in("MemTotal: 9 kB\n"), None);
    }

    #[test]
    fn the_cost_scales_between_floor_and_ceiling() {
        // A machine on which the floor takes the whole target stays there.
        assert_eq!(scaled(TARGET, None), FLOOR);
        // A slow one does too.
        assert_eq!(scaled(TARGET * 10, None), FLOOR);
        // A fast one gets more memory, rounded to a MiB, and three passes
        // once it reaches 64 MiB.
        let fast = scaled(TARGET / 4, None);
        assert_eq!(fast.memory_kib % 1024, 0);
        assert!(fast.memory_kib > FLOOR.memory_kib);
        assert_eq!(fast.passes, if fast.memory_kib >= 65_536 { 3 } else { 2 });
        // A very fast one stops at the ceiling.
        assert_eq!(
            scaled(Duration::from_micros(1), None).memory_kib,
            CEILING_KIB
        );
    }

    #[test]
    fn a_new_hash_checks_and_a_wrong_password_does_not() {
        let mut hasher = Hasher {
            chosen: Some(FLOOR),
        };
        let mut said = Vec::new();
        let secret = Secret::from_bytes(b"correct horse").unwrap();
        let stored = hasher.make(&secret, &mut |line| said.push(line)).unwrap();
        assert!(
            stored.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "{stored}"
        );
        assert_eq!(
            hasher.check(&secret, &stored, &mut |_| {}),
            Checked::Match { rehash: false }
        );
        let wrong = Secret::from_bytes(b"correct hors").unwrap();
        assert_eq!(
            hasher.check(&wrong, &stored, &mut |_| {}),
            Checked::Mismatch
        );
        // Two hashes of one password differ: the salt is fresh each time.
        assert_ne!(stored, hasher.make(&secret, &mut |_| {}).unwrap());
    }

    #[test]
    fn a_sha512_crypt_hash_checks_and_asks_to_be_rehashed() {
        let mut hasher = Hasher {
            chosen: Some(FLOOR),
        };
        // Drepper's test vector: "Hello world!" under the salt "saltstring".
        let stored = "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1";
        let secret = Secret::from_bytes(b"Hello world!").unwrap();
        assert_eq!(
            hasher.check(&secret, stored, &mut |_| {}),
            Checked::Match { rehash: true }
        );
        assert_eq!(
            hasher.check(&secret, "$y$j9T$salt$hash", &mut |_| {}),
            Checked::Unusable,
            "yescrypt is not known here, and is not a mismatch either"
        );
    }
}
