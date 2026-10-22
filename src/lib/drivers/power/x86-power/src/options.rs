//! The boot words the power driver reads from `/proc/cmdline`
//! (`docs/POWERCTL.md` §8). The kernel's own word, `ferrix.powerctl`, is
//! not one of them: it decides whether the driver runs at all.

use crate::plan::{self, Plan, PlanError};

/// The words, as the driver spells them: a library names no boot option
/// (`check-build-flags.py`, ledger 2208 (2)); the process does, and hands
/// them here, as the Intel driver does its own.
#[derive(Clone, Copy, Debug)]
pub struct Words<'a> {
    /// The step plan: `A1,...,An@SECONDS`.
    pub vr: &'a [u8],
    /// The soft limit, degrees Celsius.
    pub hot: &'a [u8],
    /// A test mode: `hang@S` or `exit@S`.
    pub test: &'a [u8],
}

/// The soft limit without its word.
pub const DEFAULT_HOT_CELSIUS: u32 = 85;
/// The lowest and highest soft limit the word may give.
pub const HOT_RANGE: (u32, u32) = (50, 99);
/// The longest a test mode may wait before acting, in seconds.
pub const MAX_TEST_SECONDS: u32 = 3600;

/// What a test mode does once its time has come.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestMode {
    /// Stop sending anything, and stay alive: the kernel's lease lapses.
    Hang,
    /// Exit: the kernel sees the holder go.
    Exit,
}

/// What the command line asked of the driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// The step plan, if any; without one the driver writes nothing.
    pub plan: Option<Plan>,
    /// A plan word that did not parse, said by the driver.
    pub plan_refused: Option<PlanError>,
    /// The soft limit, degrees Celsius.
    pub hot: u32,
    /// A soft limit word that did not parse or was out of range.
    pub hot_refused: bool,
    /// A test mode and the seconds after the first step it acts.
    pub test: Option<(TestMode, u32)>,
    /// A test word that did not parse.
    pub test_refused: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            plan: None,
            plan_refused: None,
            hot: DEFAULT_HOT_CELSIUS,
            hot_refused: false,
            test: None,
            test_refused: false,
        }
    }
}

/// One to `width` ASCII digits as a number; `None` for anything else.
#[must_use]
pub fn decimal(text: &[u8], width: usize) -> Option<u32> {
    if text.is_empty() || text.len() > width || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        text.iter()
            .fold(0u32, |number, digit| number * 10 + u32::from(digit - b'0')),
    )
}

/// The value of `word=` on `line`, the last one given winning; `None` when
/// the word is absent or bare.
#[must_use]
pub fn value<'a>(line: &'a [u8], word: &[u8]) -> Option<&'a [u8]> {
    line.split(u8::is_ascii_whitespace)
        .filter_map(|item| {
            let rest = item.strip_prefix(word)?;
            rest.strip_prefix(b"=")
        })
        .next_back()
}

/// The options a command line asks for, its words spelt as `words`.
#[must_use]
pub fn parse(line: &[u8], words: &Words<'_>) -> Options {
    let mut options = Options::default();
    if let Some(text) = value(line, words.vr) {
        match plan::parse(text) {
            Ok(plan) => options.plan = Some(plan),
            Err(why) => options.plan_refused = Some(why),
        }
    }
    if let Some(text) = value(line, words.hot) {
        match decimal(text, 2) {
            Some(hot) if (HOT_RANGE.0..=HOT_RANGE.1).contains(&hot) => options.hot = hot,
            _ => options.hot_refused = true,
        }
    }
    if let Some(text) = value(line, words.test) {
        options.test = test(text);
        options.test_refused = options.test.is_none();
    }
    options
}

/// `hang@S` or `exit@S`, S from 1 to [`MAX_TEST_SECONDS`].
fn test(text: &[u8]) -> Option<(TestMode, u32)> {
    let at = text.iter().position(|&byte| byte == b'@')?;
    let mode = match text.get(..at)? {
        b"hang" => TestMode::Hang,
        b"exit" => TestMode::Exit,
        _ => return None,
    };
    let seconds = decimal(text.get(at + 1..)?, 4)?;
    (1..=MAX_TEST_SECONDS)
        .contains(&seconds)
        .then_some((mode, seconds))
}
