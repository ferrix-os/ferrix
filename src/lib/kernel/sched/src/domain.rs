//! Scheduling domains and the class stack: which CPUs are scheduled together,
//! and what runs first on them.
//!
//! `docs/ARCHITECTURE.md` §5 partitions the CPUs into domains, each running
//! one of three modes, and gives each mode a stack of scheduling classes that
//! are asked in order for something to run. Stage 5 implements one mode and
//! builds one domain holding every CPU. What it does not do is pretend the
//! others will never exist: the mode is a property of the domain from the
//! first line, the class stack is looked up from the mode rather than written
//! into the scheduler, and the two modes stage 14 will write are named here
//! and refused. When they arrive there is a type to extend and a check to
//! satisfy, rather than a scheduler to take apart to find where the policy
//! was.

use crate::SchedError;

/// How a domain schedules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// EEVDF for everything, preemption points deferred for cache locality,
    /// idle CPUs stealing work. What `rustc` runs in.
    Throughput,
    /// Fixed-priority real-time classes above the fair one, a fully
    /// preemptible kernel, threaded interrupts. Stage 14.
    SoftRt,
    /// Earliest-deadline-first with admission control, and nothing else.
    /// Stage 14.
    HardRt,
}

/// A scheduling class: one policy in a mode's stack.
///
/// Stage 14 adds the fixed-priority and deadline classes above [`Class::Fair`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    /// EEVDF.
    Fair,
    /// The per-CPU idle task, which runs when no class above it has anything.
    Idle,
}

/// The Throughput mode's stack.
const THROUGHPUT: [Class; 2] = [Class::Fair, Class::Idle];

impl Mode {
    /// The classes asked for work, highest first.
    ///
    /// # Errors
    ///
    /// [`SchedError::ModeUnavailable`] for a mode that is not written yet —
    /// which is a refusal, not a fallback. A domain asked to run hard
    /// real-time that quietly ran Throughput instead would be the kind of
    /// statement that gets believed.
    pub const fn classes(self) -> Result<&'static [Class], SchedError> {
        match self {
            Mode::Throughput => Ok(&THROUGHPUT),
            Mode::SoftRt | Mode::HardRt => Err(SchedError::ModeUnavailable(self)),
        }
    }

    /// Whether an idle CPU in this mode takes work from a busy one.
    ///
    /// Only in Throughput. In the real-time modes partitioning is what makes
    /// admission control mean anything, and a task that wandered to another
    /// CPU would take its reserved bandwidth with it.
    #[must_use]
    pub const fn steals_work(self) -> bool {
        matches!(self, Mode::Throughput)
    }

    /// The mode's name, for messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Mode::Throughput => "Throughput",
            Mode::SoftRt => "SoftRt",
            Mode::HardRt => "HardRt",
        }
    }
}

/// The most CPUs a [`CpuSet`] can name.
pub const MAX_CPUS: usize = 256;

/// Words of [`CpuSet`]'s bitmap.
pub const WORDS: usize = MAX_CPUS / 64;

/// A set of logical CPU numbers.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CpuSet {
    /// One bit per CPU.
    words: [u64; WORDS],
}

impl CpuSet {
    /// No CPUs.
    #[must_use]
    pub const fn empty() -> CpuSet {
        CpuSet { words: [0; WORDS] }
    }

    /// Exactly one CPU, for a task confined to it.
    ///
    /// Answers the empty set for a CPU past [`MAX_CPUS`] rather than an error,
    /// because a set that names a processor the machine does not have is a set
    /// nothing can run on — which is what the empty set already means, and the
    /// caller handles it in the same place either way.
    #[must_use]
    pub fn of(cpu: usize) -> CpuSet {
        let mut set = CpuSet::empty();
        let _ = set.insert(cpu);
        set
    }

    /// CPUs `0..count`.
    ///
    /// # Errors
    ///
    /// [`SchedError::NoSuchCpu`] if `count` is past [`MAX_CPUS`].
    pub fn first(count: usize) -> Result<CpuSet, SchedError> {
        let mut set = CpuSet::empty();
        for cpu in 0..count {
            set.insert(cpu)?;
        }
        Ok(set)
    }

    /// Add `cpu`.
    ///
    /// # Errors
    ///
    /// [`SchedError::NoSuchCpu`] past [`MAX_CPUS`].
    pub fn insert(&mut self, cpu: usize) -> Result<(), SchedError> {
        let word = self
            .words
            .get_mut(cpu / 64)
            .ok_or(SchedError::NoSuchCpu(cpu))?;
        *word |= 1 << (cpu % 64);
        Ok(())
    }

    /// Whether `cpu` is in the set.
    #[must_use]
    pub fn contains(&self, cpu: usize) -> bool {
        self.words
            .get(cpu / 64)
            .is_some_and(|word| word & (1 << (cpu % 64)) != 0)
    }

    /// How many CPUs are in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }

    /// The CPUs in the set, lowest first.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        (0..MAX_CPUS).filter(|cpu| self.contains(*cpu))
    }

    /// The set as its [`WORDS`] words, CPU `64 * i + b` at bit `b` of word
    /// `i`: what a task's affinity is stored as, one atomic a word.
    #[must_use]
    pub const fn words(&self) -> [u64; WORDS] {
        self.words
    }

    /// The set whose words are `words`, as [`CpuSet::words`] gives them.
    #[must_use]
    pub const fn from_words(words: [u64; WORDS]) -> CpuSet {
        CpuSet { words }
    }

    /// The CPUs in both sets.
    #[must_use]
    pub fn intersection(&self, other: &CpuSet) -> CpuSet {
        let mut words = self.words;
        for (word, theirs) in words.iter_mut().zip(other.words) {
            *word &= theirs;
        }
        CpuSet { words }
    }
}

/// CPUs scheduled together, in one mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Domain {
    /// Which CPUs.
    cpus: CpuSet,
    /// How they are scheduled.
    mode: Mode,
}

impl Domain {
    /// A domain of `cpus` running `mode`.
    ///
    /// # Errors
    ///
    /// [`SchedError::EmptyDomain`] for no CPUs, and
    /// [`SchedError::ModeUnavailable`] for a mode not written yet.
    pub fn new(cpus: CpuSet, mode: Mode) -> Result<Domain, SchedError> {
        if cpus.is_empty() {
            return Err(SchedError::EmptyDomain);
        }
        let _ = mode.classes()?;
        Ok(Domain { cpus, mode })
    }

    /// Its CPUs.
    #[must_use]
    pub const fn cpus(&self) -> &CpuSet {
        &self.cpus
    }

    /// Its mode.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }
}

/// Check that `domains` partitions CPUs `0..online`: every one of them in
/// exactly one domain, and no domain naming a CPU past them.
///
/// # Errors
///
/// The first CPU that is in none, or in two, or does not exist.
pub fn check_partition(domains: &[Domain], online: usize) -> Result<(), SchedError> {
    if online > MAX_CPUS {
        return Err(SchedError::NoSuchCpu(online));
    }
    for cpu in 0..online {
        match domains
            .iter()
            .filter(|domain| domain.cpus.contains(cpu))
            .count()
        {
            0 => return Err(SchedError::Uncovered(cpu)),
            1 => {}
            _ => return Err(SchedError::Overlap(cpu)),
        }
    }
    let stray = domains
        .iter()
        .flat_map(|domain| domain.cpus.iter())
        .find(|cpu| *cpu >= online);
    match stray {
        Some(cpu) => Err(SchedError::NoSuchCpu(cpu)),
        None => Ok(()),
    }
}
