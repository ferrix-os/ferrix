//! The kernel command-line words the driver process reads for the N150
//! box's runs (`docs/INTEL-GPU.md` §10), none of them i915's: where to stop,
//! whether to say every register access, and when to restart the machine.
//! Parsed here, apart from the process, so a host test holds them.

/// Stop before step `N` (the driver's `steps::At`).
pub const STOP: &[u8] = b"ferrix.intelgpu.stop=";
/// Say every register access before it is made.
pub const TRACE: &[u8] = b"ferrix.intelgpu.trace";
/// Restart the machine `S` seconds after boot.
pub const RESET: &[u8] = b"ferrix.intelgpu.reset=";

/// The longest a [`RESET`] may wait: an hour.
pub const RESET_MAX_SECONDS: u32 = 3600;

/// What the command line asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Stop before this step, at least 1.
    pub stop: Option<u32>,
    /// Say every register access before it is made.
    pub trace: bool,
    /// Restart the machine this many seconds after boot, 1 to
    /// [`RESET_MAX_SECONDS`].
    pub reset_seconds: Option<u32>,
    /// A [`RESET`] whose value was not a number in range: said by the
    /// driver, and no restart.
    pub reset_refused: bool,
}

/// The words of `line` the driver reads. A `stop=` that is not a number is
/// taken as 1, so a misspelt value touches nothing rather than everything;
/// a `reset=` that is not a number in range is refused, so a misspelt one
/// never restarts a machine nobody asked to.
#[must_use]
pub fn parse(line: &[u8]) -> Options {
    let mut options = Options::default();
    for word in line.split(u8::is_ascii_whitespace) {
        if word == TRACE {
            options.trace = true;
        } else if let Some(value) = word.strip_prefix(STOP) {
            options.stop = Some(number(value).unwrap_or(1).max(1));
        } else if let Some(value) = word.strip_prefix(RESET) {
            match number(value).filter(|s| (1..=RESET_MAX_SECONDS).contains(s)) {
                Some(seconds) => {
                    options.reset_seconds = Some(seconds);
                    options.reset_refused = false;
                }
                None => {
                    options.reset_seconds = None;
                    options.reset_refused = true;
                }
            }
        }
    }
    options
}

/// A decimal number of at most four digits.
fn number(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    digits.iter().try_fold(0_u32, |n, &byte| {
        byte.is_ascii_digit()
            .then(|| n * 10 + u32::from(byte - b'0'))
    })
}

#[cfg(test)]
mod tests {
    use super::{Options, parse};

    #[test]
    fn nothing_asked() {
        assert_eq!(
            parse(b"ferrix.intelgpu ferrix.fbcon ferrix.intelgpu.stopx=3\n"),
            Options::default()
        );
    }

    #[test]
    fn every_word() {
        let options = parse(
            b"ferrix.init=/etc/reset-after.sh ferrix.intelgpu ferrix.intelgpu.trace \
              ferrix.intelgpu.stop=9 ferrix.intelgpu.reset=60\n",
        );
        assert_eq!(
            options,
            Options {
                stop: Some(9),
                trace: true,
                reset_seconds: Some(60),
                reset_refused: false,
            }
        );
    }

    #[test]
    fn a_misspelt_stop_touches_nothing() {
        assert_eq!(parse(b"ferrix.intelgpu.stop=x").stop, Some(1));
        assert_eq!(parse(b"ferrix.intelgpu.stop=0").stop, Some(1));
        assert_eq!(parse(b"ferrix.intelgpu.stop=").stop, Some(1));
    }

    #[test]
    fn a_misspelt_reset_restarts_nothing() {
        for line in [
            &b"ferrix.intelgpu.reset=0"[..],
            b"ferrix.intelgpu.reset=3601",
            b"ferrix.intelgpu.reset=6O",
            b"ferrix.intelgpu.reset=",
            b"ferrix.intelgpu.reset=99999",
        ] {
            let options = parse(line);
            assert_eq!(options.reset_seconds, None, "{line:?}");
            assert!(options.reset_refused, "{line:?}");
        }
        assert_eq!(
            parse(b"ferrix.intelgpu.reset=3600").reset_seconds,
            Some(3600)
        );
    }
}
