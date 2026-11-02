//! Core quarantine's policy (`docs/CORE-QUARANTINE.md` §5): which
//! machine-check records count against a physical core, when a core has
//! had enough, and the bounds on taking cores out. The kernel only does
//! what it is asked through the processor service token; every choice is
//! here, in ring 3.
//!
//! `/etc/ferrix/lode.d/quarantine.conf`:
//!
//! ```text
//! action = quarantine      # quarantine, warn, or off
//! count = 10               # records in the window that take a core out
//! window = 600             # seconds
//! cap = half               # at most this many processors per boot
//! keep = 1                 # never leave fewer than this many in service
//! ```
//!
//! A file lode cannot read, or with any value out of range, means `warn`:
//! lode never quarantines on a file it could not read.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

use crate::mce::MceLine;

/// `MESSAGE_ID` of lode's records about a core: counted enough, taken out,
/// refused, brought back.
pub const MESSAGE_ID_QUARANTINE: &str = "3f6a9c1e7b2d4e80a5c4d3b2e1f09a87";

/// The processors a sysfs list names (`0-1,3`), ascending, or `None` for
/// text that is not one. Bounded: a range past 4096 processors is refused.
#[must_use]
pub fn cpu_list(text: &str) -> Option<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in text.trim().split(',').filter(|part| !part.is_empty()) {
        let (first, last) = match part.split_once('-') {
            Some((first, last)) => (first.parse().ok()?, last.parse().ok()?),
            None => {
                let only: usize = part.parse().ok()?;
                (only, only)
            }
        };
        if first > last || last >= 4096 {
            return None;
        }
        cpus.extend(first..=last);
    }
    cpus.sort_unstable();
    cpus.dedup();
    Some(cpus)
}

/// The processor a kernel line says root brought back,
/// `cpu: <n> back in service (by root)`.
#[must_use]
pub fn brought_back_by_root(text: &[u8]) -> Option<usize> {
    let text = core::str::from_utf8(text).ok()?;
    let cpu = text
        .strip_prefix("cpu: ")?
        .strip_suffix(" back in service (by root)")?;
    cpu.bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| cpu.parse().ok())
        .flatten()
}

/// What a core that has had enough gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Taken out of service, with a warning.
    Quarantine,
    /// A warning alone.
    Warn,
    /// Nothing: no record is counted.
    Off,
}

/// The most processors taken out in a boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    /// Half the processors, rounded down.
    Half,
    /// This many.
    Count(usize),
}

/// The policy, read from `quarantine.conf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// What a core that has had enough gets.
    pub action: Action,
    /// How many counted records in the window are enough, at least 1.
    pub count: usize,
    /// The window, in microseconds, at least a second.
    pub window_us: u64,
    /// The most processors taken out in a boot.
    pub cap: Cap,
    /// The fewest processors left in service, at least 1.
    pub keep: usize,
}

impl Config {
    /// The defaults: the customer chose `quarantine` (2026-10-30); ten in
    /// ten minutes is judgement, not a cited rate.
    pub const DEFAULT: Config = Config {
        action: Action::Quarantine,
        count: 10,
        window_us: 600 * 1_000_000,
        cap: Cap::Half,
        keep: 1,
    };

    /// What lode does on a file it could not read or did not accept.
    pub const UNREAD: Config = Config {
        action: Action::Warn,
        ..Config::DEFAULT
    };

    /// The configuration `text` says, keys it leaves out at their defaults,
    /// or why it is refused: an unknown key, a line that is not `key =
    /// value`, or a value out of range.
    ///
    /// # Errors
    ///
    /// The first line refused, said in words.
    pub fn parse(text: &str) -> Result<Config, &'static str> {
        let mut config = Config::DEFAULT;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or("a line that is not `key = value`")?;
            let value = value.trim();
            let number = || {
                value
                    .parse::<u64>()
                    .map_err(|_| "a value that is not a number")
            };
            match key.trim() {
                "action" => {
                    config.action = match value {
                        "quarantine" => Action::Quarantine,
                        "warn" => Action::Warn,
                        "off" => Action::Off,
                        _ => return Err("an action other than quarantine, warn or off"),
                    };
                }
                "count" => {
                    config.count = usize::try_from(number()?)
                        .ok()
                        .filter(|&count| (1..=1_000_000).contains(&count))
                        .ok_or("a count of 0 or past a million")?;
                }
                "window" => {
                    config.window_us = Some(number()?)
                        .filter(|&seconds| (1..=7 * 24 * 3600).contains(&seconds))
                        .ok_or("a window of 0 or past a week")?
                        * 1_000_000;
                }
                "cap" => {
                    config.cap = match value {
                        "half" => Cap::Half,
                        _ => Cap::Count(
                            usize::try_from(number()?).map_err(|_| "a cap past the word")?,
                        ),
                    };
                }
                "keep" => {
                    config.keep = usize::try_from(number()?)
                        .ok()
                        .filter(|&keep| keep >= 1)
                        .ok_or("a keep of 0: the last processor is never taken")?;
                }
                _ => return Err("an unknown key"),
            }
        }
        Ok(config)
    }
}

/// Whether a machine-check line reports an error of a core's own: on AMD
/// SMCA a bank of the load-store, instruction fetch, L2, decode, execution
/// or floating-point unit; elsewhere a TLB or cache error at level 0 to 2
/// by its MCA error code (SDM vol. 3B 17.9.2, the compound codes
/// `0000 0000 0001 TTLL` and `0000 0001 RRRR TTLL`, bit 12 filtered).
/// Memory-controller, last-level cache and interconnect errors are not: a
/// DIMM's corrected errors are polled by whichever processor got there
/// first, so they say nothing about the core that read them.
#[must_use]
pub fn core_scoped(line: &MceLine<'_>) -> bool {
    if let Some(bank_type) = line.bank_type {
        return matches!(
            bank_type,
            "load_store"
                | "insn_fetch"
                | "l2_cache"
                | "decode_unit"
                | "execution_unit"
                | "floating_point"
        );
    }
    let Some(status) = line.status else {
        return false;
    };
    let code = (status & 0xFFFF) as u16 & !(1 << 12);
    let tlb = code & 0xFFF0 == 0x0010;
    let cache = code & 0xFF00 == 0x0100;
    (tlb || cache) && code & 0b11 != 0b11
}

/// What one counted record asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Not enough yet.
    Counted,
    /// Enough, and the action is `warn`: say so.
    Warn,
    /// Enough: take the core out, if the bounds allow ([`Judge::allows`]).
    TakeOut,
}

/// The counts, per physical core.
#[derive(Debug)]
pub struct Judge {
    config: Config,
    /// When each core's counted records came, oldest first, within the
    /// window.
    seen: BTreeMap<u64, VecDeque<u64>>,
    /// Processors taken out this boot.
    taken: usize,
    /// When each core's refusal was last said.
    refused: BTreeMap<u64, u64>,
}

impl Judge {
    /// A judge with nothing counted.
    #[must_use]
    pub fn new(config: Config) -> Judge {
        Judge {
            config,
            seen: BTreeMap::new(),
            taken: 0,
            refused: BTreeMap::new(),
        }
    }

    /// The configuration it judges by.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// Count a core-scoped record on physical core `core` at `now_us`.
    /// Enough of them in the window empties the core's count, so the next
    /// verdict needs as many again.
    pub fn count(&mut self, core: u64, now_us: u64) -> Verdict {
        if self.config.action == Action::Off {
            return Verdict::Counted;
        }
        let window = self.config.window_us;
        let times = self.seen.entry(core).or_default();
        while times
            .front()
            .is_some_and(|&at| now_us.saturating_sub(at) >= window)
        {
            let _ = times.pop_front();
        }
        times.push_back(now_us);
        if times.len() < self.config.count {
            return Verdict::Counted;
        }
        times.clear();
        match self.config.action {
            Action::Quarantine => Verdict::TakeOut,
            Action::Warn | Action::Off => Verdict::Warn,
        }
    }

    /// Whether `threads` more processors may go out, with `in_service` of
    /// `cpus` in service now: within the cap for the boot, and leaving at
    /// least `keep`.
    #[must_use]
    pub fn allows(&self, threads: usize, in_service: usize, cpus: usize) -> bool {
        let cap = match self.config.cap {
            Cap::Half => cpus / 2,
            Cap::Count(count) => count,
        };
        self.taken.saturating_add(threads) <= cap
            && in_service.saturating_sub(threads) >= self.config.keep
    }

    /// `threads` processors were taken out.
    pub fn took_out(&mut self, threads: usize) {
        self.taken = self.taken.saturating_add(threads);
    }

    /// Root brought a processor of `core` back: its window starts empty,
    /// so it is not taken out again on its old errors, and the return
    /// counts against no cap.
    pub fn returned(&mut self, core: u64, threads: usize) {
        let _ = self.seen.remove(&core);
        self.taken = self.taken.saturating_sub(threads);
    }

    /// Whether a refusal for `core` at `now_us` is to be said: once a
    /// window.
    pub fn say_refusal(&mut self, core: u64, now_us: u64) -> bool {
        let window = self.config.window_us;
        match self.refused.get(&core) {
            Some(&at) if now_us.saturating_sub(at) < window => false,
            _ => {
                let _ = self.refused.insert(core, now_us);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, Cap, Config, Judge, Verdict, brought_back_by_root, core_scoped, cpu_list};
    use crate::mce;

    #[test]
    fn the_defaults_and_an_empty_file_are_quarantine_ten_in_ten_minutes() {
        assert_eq!(Config::parse(""), Ok(Config::DEFAULT));
        assert_eq!(Config::DEFAULT.action, Action::Quarantine);
        assert_eq!(Config::UNREAD.action, Action::Warn);
        let text = "action = warn # just say\ncount=3\nwindow = 60\ncap = 2\nkeep = 2\n";
        assert_eq!(
            Config::parse(text),
            Ok(Config {
                action: Action::Warn,
                count: 3,
                window_us: 60_000_000,
                cap: Cap::Count(2),
                keep: 2,
            })
        );
    }

    #[test]
    fn a_value_out_of_range_refuses_the_file() {
        for text in [
            "count = 0",
            "keep = 0",
            "window = 0",
            "action = reboot",
            "colour = red",
            "count",
            "count = ten",
        ] {
            assert!(Config::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn only_a_cores_own_errors_count() {
        let scoped = |text: &str| core_scoped(&mce::read(text.as_bytes()).unwrap());
        // Intel: a level 1 cache error with bit 12 (filtering) set, a
        // level 2 one, and a TLB error at level 0.
        assert!(scoped(
            "mce: cpu 2 bank 1 corrected status=0x9000000000001145"
        ));
        assert!(scoped(
            "mce: cpu 2 bank 1 corrected status=0x9000000000000136"
        ));
        assert!(scoped(
            "mce: cpu 2 bank 0 corrected status=0x9000000000000010"
        ));
        // The generic level, a memory controller (0x009f) and a bus error.
        assert!(!scoped(
            "mce: cpu 2 bank 3 corrected status=0x900000000000014b"
        ));
        assert!(!scoped(
            "mce: cpu 2 bank 7 corrected status=0x900000000000009f"
        ));
        assert!(!scoped(
            "mce: cpu 2 bank 4 corrected status=0x9000000000000800"
        ));
        assert!(!scoped("mce: cpu 2 bank 4 corrected"));
        // AMD SMCA by the bank's block: load-store counts, L3 and UMC do not.
        let ipid = |hwid: u64, mcatype: u64| (mcatype << 48) | (hwid << 32);
        let line = |id: u64| alloc::format!("mce: cpu 3 bank 0 corrected ipid={id:#x}");
        assert!(scoped(&line(ipid(0xb0, 0))));
        assert!(scoped(&line(ipid(0xb0, 6))));
        assert!(!scoped(&line(ipid(0xb0, 7))));
        assert!(!scoped(&line(ipid(0x96, 0))));
    }

    #[test]
    fn enough_in_the_window_takes_a_core_out_and_starts_it_again() {
        let mut judge = Judge::new(Config {
            count: 3,
            window_us: 10,
            ..Config::DEFAULT
        });
        assert_eq!(judge.count(1, 0), Verdict::Counted);
        assert_eq!(judge.count(1, 5), Verdict::Counted);
        // The first fell out of the window: still two.
        assert_eq!(judge.count(1, 12), Verdict::Counted);
        // Another core's count is its own.
        assert_eq!(judge.count(2, 12), Verdict::Counted);
        assert_eq!(judge.count(1, 13), Verdict::TakeOut);
        assert_eq!(judge.count(1, 14), Verdict::Counted);
        judge.returned(2, 0);
        assert_eq!(judge.count(2, 15), Verdict::Counted);
        let mut warning = Judge::new(Config {
            action: Action::Warn,
            count: 1,
            ..Config::DEFAULT
        });
        assert_eq!(warning.count(0, 0), Verdict::Warn);
        let mut off = Judge::new(Config {
            action: Action::Off,
            count: 1,
            ..Config::DEFAULT
        });
        assert_eq!(off.count(0, 0), Verdict::Counted);
    }

    #[test]
    fn the_cap_and_keep_bound_what_goes_out() {
        let mut judge = Judge::new(Config::DEFAULT);
        // Eight processors: half is four.
        assert!(judge.allows(2, 8, 8));
        judge.took_out(2);
        assert!(judge.allows(2, 6, 8));
        judge.took_out(2);
        assert!(!judge.allows(2, 4, 8));
        // A return gives the cap back.
        judge.returned(5, 2);
        assert!(judge.allows(2, 6, 8));
        // Keep: never below one in service.
        let two = Judge::new(Config {
            cap: Cap::Count(8),
            ..Config::DEFAULT
        });
        assert!(!two.allows(2, 2, 2));
        assert!(two.allows(1, 2, 2));
    }

    #[test]
    fn sysfs_lists_and_the_kernels_return_line_read() {
        assert_eq!(cpu_list("0-1,3\n"), Some(alloc::vec![0, 1, 3]));
        assert_eq!(cpu_list("2"), Some(alloc::vec![2]));
        assert_eq!(cpu_list("\n"), Some(alloc::vec![]));
        assert_eq!(cpu_list("3-1"), None);
        assert_eq!(cpu_list("0-9999"), None);
        assert_eq!(cpu_list("a"), None);
        assert_eq!(
            brought_back_by_root(b"cpu: 3 back in service (by root)"),
            Some(3)
        );
        assert_eq!(
            brought_back_by_root(b"cpu: 3 back in service (by lode)"),
            None
        );
        assert_eq!(
            brought_back_by_root(b"cpu: x back in service (by root)"),
            None
        );
    }

    #[test]
    fn a_refusal_is_said_once_a_window() {
        let mut judge = Judge::new(Config {
            window_us: 10,
            ..Config::DEFAULT
        });
        assert!(judge.say_refusal(1, 0));
        assert!(!judge.say_refusal(1, 9));
        assert!(judge.say_refusal(2, 9));
        assert!(judge.say_refusal(1, 10));
    }
}
