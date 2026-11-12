//! What the boot has said so far, and what of it the screen shows.
//!
//! Everything comes from two places a program can read: the kernel log,
//! which holds init's line for every unit that changes state, and
//! `devmgr-events`, which says when a driver was started again. Nothing here
//! opens either; `main.rs` hands the records in with the time it read them.

use std::collections::VecDeque;

use ferrix_splash::FULL;

/// Lines of the log kept for the log view.
const KEPT: usize = 400;
/// Milliseconds a driver's restart stays on the screen.
const SHOWN_MS: u64 = 2000;
/// A stage is said to run long once it has taken this many times its usual
/// time.
const PATIENCE: u64 = 2;

/// What the boot is doing while the splash is on the card. The loader and
/// the kernel are over by then, and so is the display's own driver. The log
/// is read from its first line, so a splash that starts late finds the stage
/// the boot is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// Up to devmgr's report: every device has its driver.
    Drivers,
    /// Up to init's word on where `/` is.
    Root,
    /// Up to `sysinit.target`.
    System,
    /// Up to `basic.target`: the log and what every service needs.
    Basics,
    /// Up to `multi-user.target`.
    Services,
    /// Up to the compositor's start.
    Desktop,
}

impl Stage {
    /// The slash's level when the stage begins.
    #[must_use]
    pub const fn level(self) -> u32 {
        match self {
            Self::Drivers => 300,
            Self::Root => 550,
            Self::System => 700,
            Self::Basics => 780,
            Self::Services => 860,
            Self::Desktop => 940,
        }
    }

    /// What the screen says the boot is doing when the stage runs long.
    const fn doing(self) -> &'static str {
        match self {
            Self::Drivers => "starting drivers",
            Self::Root => "mounting the root filesystem",
            Self::System => "starting the system",
            Self::Basics => "starting the log",
            Self::Services => "starting services",
            Self::Desktop => "starting the desktop",
        }
    }

    /// Milliseconds the stage usually takes, rounded up from the slower of
    /// two boots measured on 2026-11-12 (`docs/SPLASH.md` §3): the N150 box
    /// and QEMU under KVM. The root volume took 2.8 s and 4.4 s; the drivers
    /// left after the display's 0.8 s and under 0.1 s; the rest are tens of
    /// milliseconds, and are given what a unit or two may take.
    const fn usual_ms(self) -> u64 {
        match self {
            Self::System | Self::Basics => 2000,
            Self::Drivers | Self::Services | Self::Desktop => 4000,
            Self::Root => 5000,
        }
    }

    /// The stage that begins when the log says `said`, if that ends one:
    /// the kernel's line for devmgr's report, init's for `/`, and init's
    /// for each target reached.
    fn after(said: &str) -> Option<Self> {
        [
            ("devmgr   started by pid 1", Self::Root),
            ("init     / is the root volume now", Self::System),
            ("init     / stays in memory", Self::System),
            ("sysinit.target: active", Self::Basics),
            ("basic.target: active", Self::Services),
            ("multi-user.target: active", Self::Desktop),
        ]
        .into_iter()
        .find_map(|(line, stage)| said.contains(line).then_some(stage))
    }
}

/// The line under the mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// What it says.
    pub text: String,
    /// The smaller line under it: which keys do something.
    pub hint: &'static str,
    /// Whether the boot has stopped: drawn in the text colour, not the
    /// muted one.
    pub strong: bool,
}

/// The boot, as far as it has been read.
#[derive(Debug)]
pub struct Boot {
    /// What it is doing.
    stage: Stage,
    /// When that began, in milliseconds.
    since: u64,
    /// Whether the compositor is about to start.
    done: bool,
    /// Whether init gave up on the boot.
    failed: bool,
    /// Whether it went to the rescue target for that, which stops devmgr
    /// and with it the screen's driver and the keyboards'.
    rescue: bool,
    /// The last unit that failed or timed out: why, most likely.
    cause: Option<String>,
    /// The driver last started again, and when.
    restarted: Option<(String, u64)>,
    /// The newest lines of the log, oldest first.
    lines: VecDeque<String>,
    /// How many lines the log has had.
    seen: u64,
}

impl Boot {
    /// A boot the splash began to read at `now`.
    #[must_use]
    pub const fn new(now: u64) -> Self {
        Self {
            stage: Stage::Drivers,
            since: now,
            done: false,
            failed: false,
            rescue: false,
            cause: None,
            restarted: None,
            lines: VecDeque::new(),
            seen: 0,
        }
    }

    /// Take one record of `/dev/kmsg`, read at `now`.
    pub fn record(&mut self, record: &str, now: u64) {
        let text = text_of(record).trim_end();
        if text.is_empty() {
            return;
        }
        if self.lines.len() == KEPT {
            drop(self.lines.pop_front());
        }
        self.lines.push_back(text.to_owned());
        self.seen += 1;
        let said = text.trim_start();
        if let Some(stage) = Stage::after(said)
            && stage > self.stage
        {
            self.stage = stage;
            self.since = now;
        }
        // Init says a target failed, or that it is starting the rescue
        // target, which is what it does when a unit the boot requires fails.
        // That line is the last the screen can answer: the rescue target
        // stops devmgr, and the card's driver with it.
        let rescue = said.contains("starting rescue.target");
        if rescue || said.contains(".target: failed") {
            self.failed = true;
            self.rescue |= rescue;
            self.cause = self.cause.take().or_else(|| unit_of(said));
        } else if said.contains(".service: failed") || said.contains(": start timed out") {
            self.cause = unit_of(said);
        }
    }

    /// Take one line of `devmgr-events`, read at `now`.
    pub fn driver_event(&mut self, line: &str, now: u64) {
        let mut words = line.split_whitespace();
        if words.next() == Some("restarted")
            && let Some(name) = words.next()
        {
            self.restarted = Some((name.to_owned(), now));
        }
    }

    /// The compositor is about to start: the slash is whole.
    pub const fn finish(&mut self) {
        self.done = true;
    }

    /// The slash's level.
    #[must_use]
    pub const fn level(&self) -> u32 {
        if self.done { FULL } else { self.stage.level() }
    }

    /// How many lines the log has had: it changes when the log does.
    #[must_use]
    pub const fn seen(&self) -> u64 {
        self.seen
    }

    /// Whether init gave up on a target: the boot goes no further by itself.
    #[must_use]
    pub const fn failed(&self) -> bool {
        self.failed
    }

    /// The line under the mark at `now`, if the screen has something to say.
    #[must_use]
    pub fn note(&self, now: u64) -> Option<Note> {
        if self.failed {
            let text = match self.cause.as_deref() {
                None => "Ferrix cannot start".to_owned(),
                Some(unit) => format!("Ferrix cannot start: {unit} failed"),
            };
            return Some(Note {
                text,
                // After the rescue target no key reaches this program.
                hint: if self.rescue {
                    "the rescue shell is on the console"
                } else {
                    "Esc shows the log, Enter restarts"
                },
                strong: true,
            });
        }
        if let Some((name, at)) = &self.restarted
            && now.saturating_sub(*at) < SHOWN_MS
        {
            return Some(Note {
                text: format!("{name} driver restarted"),
                hint: "",
                strong: false,
            });
        }
        let taken = now.saturating_sub(self.since);
        (!self.done && taken >= self.stage.usual_ms() * PATIENCE).then(|| Note {
            text: format!("{} - {} s", self.stage.doing(), taken / 1000),
            hint: "Esc shows the log",
            strong: false,
        })
    }

    /// The newest `count` lines of the log, oldest first.
    pub fn newest(&self, count: usize) -> impl Iterator<Item = &str> {
        self.lines
            .iter()
            .skip(self.lines.len().saturating_sub(count))
            .map(String::as_str)
    }
}

/// The text of a `/dev/kmsg` record: after the `;` that ends its
/// `priority,sequence,time,flags` prefix, up to the end of its first line.
#[must_use]
pub fn text_of(record: &str) -> &str {
    let first = record.lines().next().unwrap_or("");
    first.split_once(';').map_or(first, |(_, text)| text)
}

/// The unit a line of init's is about: the word before its colon.
fn unit_of(said: &str) -> Option<String> {
    said.split_whitespace()
        .find_map(|word| word.strip_suffix(':').filter(|unit| unit.contains('.')))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{Boot, Stage, text_of};
    use ferrix_splash::FULL;

    /// A record as `/dev/kmsg` gives one.
    fn record(text: &str) -> String {
        format!("6,42,1234567,-;{text}\n SUBSYSTEM=x\n")
    }

    #[test]
    fn a_records_text_is_what_follows_its_prefix() {
        assert_eq!(
            text_of(&record("  init     a.target: active")),
            "  init     a.target: active"
        );
        assert_eq!(text_of("no prefix"), "no prefix");
        assert_eq!(text_of("6,1,2,-;a;b"), "a;b");
        assert_eq!(text_of(""), "");
    }

    #[test]
    fn the_slash_rises_with_init_s_targets_and_never_falls() {
        let mut boot = Boot::new(0);
        assert_eq!(boot.level(), Stage::Drivers.level());
        boot.record(
            &record("  devmgr   started by pid 1: 22 devices, 20 drivers, 11 started, 0 failed"),
            5,
        );
        assert_eq!(boot.level(), Stage::Root.level());
        boot.record(
            &record(
                "  init     / is the root volume now, and init with it; reading the units again",
            ),
            6,
        );
        assert_eq!(boot.level(), Stage::System.level());
        let mut live = Boot::new(0);
        live.record(&record("  init     / stays in memory"), 6);
        assert_eq!(live.level(), Stage::System.level());
        boot.record(&record("  init     sysinit.target: active"), 10);
        assert_eq!(boot.level(), Stage::Basics.level());
        boot.record(&record("  init     multi-user.target: active"), 20);
        assert_eq!(boot.level(), Stage::Desktop.level());
        boot.record(&record("  init     basic.target: active"), 30);
        assert_eq!(boot.level(), Stage::Desktop.level(), "an older target");
        boot.record(&record("  init     lode.service: active"), 40);
        assert_eq!(boot.level(), Stage::Desktop.level(), "a service");
        boot.finish();
        assert_eq!(boot.level(), FULL);
    }

    #[test]
    fn a_boot_on_time_says_nothing() {
        let mut boot = Boot::new(0);
        assert_eq!(boot.note(7999), None);
        boot.record(&record("  init     sysinit.target: active"), 7999);
        assert_eq!(boot.note(11_000), None, "the stage began again");
    }

    #[test]
    fn a_stage_past_twice_its_usual_time_is_named_with_its_seconds() {
        let mut boot = Boot::new(1000);
        boot.record(&record("  init     basic.target: active"), 2000);
        assert_eq!(boot.note(9999), None);
        let note = boot.note(16_500).expect("14.5 s into a 4 s stage");
        assert_eq!(note.text, "starting services - 14 s");
        assert_eq!(note.hint, "Esc shows the log");
        assert!(!note.strong);
        boot.finish();
        assert_eq!(boot.note(60_000), None, "nothing is waited for any more");
    }

    #[test]
    fn a_restarted_driver_is_named_for_two_seconds() {
        let mut boot = Boot::new(0);
        boot.driver_event("died intel 0x00000010 status 137", 100);
        assert_eq!(boot.note(100), None);
        boot.driver_event("restarted intel 0x00000010 1", 500);
        let note = boot.note(2400).expect("within two seconds");
        assert_eq!(note.text, "intel driver restarted");
        assert!(!note.strong);
        assert_eq!(boot.note(2500), None);
        assert_eq!(boot.level(), Stage::Drivers.level(), "the slash stays");
    }

    #[test]
    fn a_failed_target_stops_the_boot_and_names_the_unit_that_failed() {
        let mut boot = Boot::new(0);
        boot.record(&record("  init     lode.service: failed (exit-code)"), 10);
        assert!(!boot.failed(), "a service alone does not stop the boot");
        assert_eq!(boot.note(20), None);
        boot.record(
            &record("  init     multi-user.target: failed (dependency)"),
            30,
        );
        assert!(boot.failed());
        let note = boot.note(40).expect("the boot stopped");
        assert_eq!(note.text, "Ferrix cannot start: lode.service failed");
        assert_eq!(note.hint, "Esc shows the log, Enter restarts");
        assert!(note.strong);
        boot.driver_event("restarted snd 0x10 1", 50);
        assert_eq!(boot.note(60), Some(note), "it stays over anything else");
    }

    #[test]
    fn a_target_that_failed_alone_is_named_itself_and_a_timeout_is_a_cause() {
        let mut boot = Boot::new(0);
        boot.record(&record("  init     basic.target: failed (dependency)"), 1);
        assert_eq!(
            boot.note(2).map(|note| note.text),
            Some("Ferrix cannot start: basic.target failed".to_owned())
        );
        let mut slow = Boot::new(0);
        slow.record(&record("  init     pulsed.service: start timed out"), 1);
        slow.record(
            &record("  init     graphical.target: failed (dependency)"),
            2,
        );
        assert_eq!(
            slow.note(3).map(|note| note.text),
            Some("Ferrix cannot start: pulsed.service failed".to_owned())
        );
    }

    #[test]
    fn the_rescue_target_is_a_boot_that_stopped() {
        let mut boot = Boot::new(0);
        boot.record(&record("  init     x.service: failed (exit-code)"), 1);
        assert!(!boot.failed());
        boot.record(
            &record("  init     graphical.target: dependency; starting rescue.target"),
            2,
        );
        assert!(boot.failed());
        let note = boot.note(3).expect("the boot stopped");
        assert_eq!(note.text, "Ferrix cannot start: x.service failed");
        assert_eq!(note.hint, "the rescue shell is on the console");
        let mut bare = Boot::new(0);
        bare.record(&record("  init     starting rescue.target"), 2);
        assert_eq!(
            bare.note(3).map(|note| note.text),
            Some("Ferrix cannot start".to_owned())
        );
    }

    #[test]
    fn the_log_keeps_its_newest_lines() {
        let mut boot = Boot::new(0);
        for n in 0..450 {
            boot.record(&record(&format!("line {n}")), 0);
        }
        boot.record("6,1,1,-;\n", 0);
        let newest: Vec<&str> = boot.newest(2).collect();
        assert_eq!(newest, ["line 448", "line 449"]);
        assert_eq!(boot.newest(1000).count(), 400);
        assert_eq!(boot.newest(1000).next(), Some("line 50"));
    }
}
