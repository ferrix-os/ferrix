//! The command line: `udhcpc`'s options, as far as Ferrix's callers use
//! them.

use std::time::Duration;

/// The usage line.
pub const USAGE: &str = "usage: dhcpc -i IFACE [-n] [-q] [-f] [-t TRIES] [-T SECONDS] [-A SECONDS]";

/// What the command line asked.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Args {
    /// The interface (`-i`); `udhcpc`'s default is `eth0`, and so is this.
    pub interface: String,
    /// DISCOVERs per round (`-t`), 3 as `udhcpc`'s.
    pub tries: u32,
    /// Seconds to wait for each answer (`-T`), 3 as `udhcpc`'s.
    pub timeout: Duration,
    /// Seconds to wait after a round with no lease (`-A`), 20 as `udhcpc`'s.
    pub pause: Duration,
    /// Exit with status 1 after a round with no lease (`-n`).
    pub exit_without_lease: bool,
    /// Exit with status 0 once the lease is applied (`-q`).
    pub quit_with_lease: bool,
}

impl Default for Args {
    fn default() -> Args {
        Args {
            interface: "eth0".to_owned(),
            tries: 3,
            timeout: Duration::from_secs(3),
            pause: Duration::from_secs(20),
            exit_without_lease: false,
            quit_with_lease: false,
        }
    }
}

/// Read `args`, the arguments after the program's name.
///
/// Flags may be grouped (`-nq`) and a value may follow its flag in the same
/// word (`-ieth0`) or the next, as `getopt` reads them.
///
/// # Errors
///
/// What is wrong with the command line, in a line for standard error.
pub fn parse<I>(args: I) -> Result<Args, String>
where
    I: IntoIterator<Item = String>,
{
    let mut parsed = Args::default();
    let mut args = args.into_iter();
    while let Some(word) = args.next() {
        let Some(flags) = word.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            return Err(format!("unexpected argument {word:?}"));
        };
        for (at, flag) in flags.char_indices() {
            match flag {
                'n' => parsed.exit_without_lease = true,
                'q' => parsed.quit_with_lease = true,
                'f' => {}
                'i' | 't' | 'T' | 'A' => {
                    let rest = flags.get(at + flag.len_utf8()..).unwrap_or_default();
                    let value = if rest.is_empty() {
                        args.next()
                            .ok_or_else(|| format!("-{flag} needs a value"))?
                    } else {
                        rest.to_owned()
                    };
                    set(&mut parsed, flag, &value)?;
                    break;
                }
                _ => return Err(format!("unknown option -{flag}")),
            }
        }
    }
    Ok(parsed)
}

/// Set the option `flag` takes to `value`.
fn set(parsed: &mut Args, flag: char, value: &str) -> Result<(), String> {
    let number = || {
        value
            .parse::<u32>()
            .map_err(|_| format!("-{flag} needs a number, not {value:?}"))
    };
    match flag {
        'i' => {
            if value.is_empty() || value.len() > 15 || value.contains('/') {
                return Err(format!("{value:?} is not an interface name"));
            }
            parsed.interface = value.to_owned();
        }
        't' => parsed.tries = number()?.max(1),
        'T' => parsed.timeout = Duration::from_secs(u64::from(number()?.max(1))),
        _ => parsed.pause = Duration::from_secs(u64::from(number()?)),
    }
    Ok(())
}
