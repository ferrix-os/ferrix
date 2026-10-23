//! The command line, as util-linux's `dmesg` takes it.

use crate::record::{FACILITIES, LEVELS};

/// What `--help` prints.
pub const USAGE: &str = "Usage: dmesg [options]

Display or control the kernel ring buffer.

Options:
 -C, --clear                 clear the kernel ring buffer
 -c, --read-clear            read and clear all messages
 -D, --console-off           disable printing messages to console
 -E, --console-on            enable printing messages to console
 -f, --facility <list>       restrict output to defined facilities
 -H, --human                 human readable output
 -k, --kernel                display kernel messages
 -l, --level <list>          restrict output to defined levels
 -n, --console-level <level> set level of messages printed to console
 -r, --raw                   print the raw message buffer
 -S, --syslog                force to use syslog(2) rather than /dev/kmsg
 -s, --buffer-size <size>    buffer size to query the kernel ring buffer
 -u, --userspace             display userspace messages
 -w, --follow                wait for new messages
 -W, --follow-new            wait and print only new messages
 -x, --decode                decode facility and level to readable string
 -T, --ctime                 show human-readable timestamp
 -t, --notime                don't show any timestamp with messages
 -h, --help                  display this help
 -V, --version               display version

Supported log facilities:
    kern, user, mail, daemon, auth, syslog, lpr, news, uucp, cron, authpriv,
    ftp, res0-res3, local0-local7

Supported log levels (priorities):
   emerg, alert, crit, err, warn, notice, info, debug
";

/// What `--version` prints.
pub const VERSION: &str = concat!("dmesg from Ferrix ", env!("CARGO_PKG_VERSION"));

/// What the program is to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Print the log, and clear it afterwards if `clear`.
    Print {
        /// `-c`.
        clear: bool,
    },
    /// `-C`: clear it and print nothing.
    Clear,
    /// `-n`: set the console's level.
    ConsoleLevel(u8),
    /// `-D`.
    ConsoleOff,
    /// `-E`.
    ConsoleOn,
    /// `-h`.
    Help,
    /// `-V`.
    Version,
}

/// How a record's time is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Time {
    /// `[    1.234567]`.
    Monotonic,
    /// `-T`: `[Thu Oct 23 14:01:02 2026]`.
    Ctime,
    /// `-H`: the minute once, then the time since the record before.
    Human,
    /// `-t`: none.
    None,
}

/// How long to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Follow {
    /// Until the log is read.
    No,
    /// `-w`: what is there, then every new record as it comes.
    All,
    /// `-W`: only new records, as they come.
    New,
}

/// Everything the command line says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// What to do.
    pub action: Action,
    /// `-S`: through `syslog(2)` even when `/dev/kmsg` opens.
    pub syslog: bool,
    /// How times are shown.
    pub time: Time,
    /// `-r`.
    pub raw: bool,
    /// `-x`.
    pub decode: bool,
    /// `-w` or `-W`.
    pub follow: Follow,
    /// The levels kept, one bit each; `None` keeps all.
    pub levels: Option<u8>,
    /// The facilities kept, one bit each; `None` keeps all.
    pub facilities: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            action: Action::Print { clear: false },
            syslog: false,
            time: Time::Monotonic,
            raw: false,
            decode: false,
            follow: Follow::No,
            levels: None,
            facilities: None,
        }
    }
}

impl Options {
    /// Whether a record of `priority` is shown; one with none always is.
    #[must_use]
    pub fn keeps(&self, priority: Option<u32>) -> bool {
        let Some(priority) = priority else {
            return true;
        };
        let level = priority & 7;
        let facility = priority >> 3;
        let level_kept = self.levels.is_none_or(|mask| mask & (1 << level) != 0);
        let facility_kept = self
            .facilities
            .is_none_or(|mask| facility < 32 && mask & (1 << facility) != 0);
        level_kept && facility_kept
    }
}

/// Where `name` is in `names`, or the number it is.
fn index_of(name: &str, names: &[&str]) -> Option<u32> {
    if let Ok(number) = name.parse::<u32>() {
        return usize::try_from(number)
            .ok()
            .filter(|&at| at < names.len())
            .map(|_| number);
    }
    names
        .iter()
        .position(|known| *known == name)
        .and_then(|at| u32::try_from(at).ok())
}

/// `-l`'s list: names or numbers, a `+` after one for it and every more
/// urgent level, before one for it and every less urgent.
fn parse_levels(list: &str) -> Result<u8, String> {
    let mut mask = 0u8;
    for item in list.split(',') {
        let (name, up, down) = if let Some(name) = item.strip_suffix('+') {
            (name, true, false)
        } else if let Some(name) = item.strip_prefix('+') {
            (name, false, true)
        } else {
            (item, false, false)
        };
        let level = index_of(name, &LEVELS).ok_or_else(|| format!("unknown level '{item}'"))?;
        let (from, to) = match (up, down) {
            (true, _) => (0, level),
            (_, true) => (level, 7),
            _ => (level, level),
        };
        for bit in from..=to {
            mask |= 1 << bit;
        }
    }
    Ok(mask)
}

/// `-f`'s list: names or numbers.
fn parse_facilities(list: &str) -> Result<u32, String> {
    let mut mask = 0u32;
    for item in list.split(',') {
        let facility =
            index_of(item, &FACILITIES).ok_or_else(|| format!("unknown facility '{item}'"))?;
        mask |= 1 << facility;
    }
    Ok(mask)
}

/// Read the command line, `args` without the program's name.
///
/// # Errors
///
/// A sentence for what it does not take, which the caller prints with the
/// usage.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        // `--name=value` and `--name value`; `-xyz` is `-x -y -z`, and a
        // short option that takes a value takes the rest of its word or the
        // next one.
        let flags: Vec<(String, Option<String>)> = if let Some(long) = arg.strip_prefix("--") {
            match long.split_once('=') {
                Some((name, value)) => vec![(name.to_owned(), Some(value.to_owned()))],
                None => vec![(long.to_owned(), None)],
            }
        } else if let Some(short) = arg.strip_prefix('-').filter(|short| !short.is_empty()) {
            let mut flags = Vec::new();
            for (at, flag) in short.char_indices() {
                if matches!(flag, 'f' | 'l' | 'n' | 's') {
                    let rest = short.get(at + flag.len_utf8()..).unwrap_or_default();
                    let value = (!rest.is_empty()).then(|| rest.to_owned());
                    flags.push((flag.to_string(), value));
                    break;
                }
                flags.push((flag.to_string(), None));
            }
            flags
        } else {
            return Err(format!("unexpected argument '{arg}'"));
        };
        for (flag, given) in flags {
            let mut value = |name: &str| -> Result<String, String> {
                match &given {
                    Some(value) => Ok(value.clone()),
                    None => args
                        .next()
                        .ok_or_else(|| format!("option '{name}' requires an argument")),
                }
            };
            match flag.as_str() {
                "C" | "clear" => options.action = Action::Clear,
                "c" | "read-clear" => options.action = Action::Print { clear: true },
                "D" | "console-off" => options.action = Action::ConsoleOff,
                "E" | "console-on" => options.action = Action::ConsoleOn,
                "f" | "facility" => {
                    options.facilities = Some(parse_facilities(&value("--facility")?)?);
                }
                "H" | "human" => options.time = Time::Human,
                "k" | "kernel" => options.facilities = Some(1),
                "l" | "level" => options.levels = Some(parse_levels(&value("--level")?)?),
                "n" | "console-level" => {
                    let level = value("--console-level")?;
                    let number = index_of(&level, &LEVELS)
                        .and_then(|level| u8::try_from(level + 1).ok())
                        .ok_or_else(|| format!("unknown level '{level}'"))?;
                    options.action = Action::ConsoleLevel(number);
                }
                "r" | "raw" => options.raw = true,
                "S" | "syslog" => options.syslog = true,
                // The size of the buffer is the kernel's to say; taken and
                // checked, as util-linux takes it, and otherwise not needed.
                "s" | "buffer-size" => {
                    let size = value("--buffer-size")?;
                    let _: u32 = size
                        .parse()
                        .map_err(|_| format!("invalid buffer size argument: '{size}'"))?;
                }
                "u" | "userspace" => options.facilities = Some(!1),
                "w" | "follow" => options.follow = Follow::All,
                "W" | "follow-new" => options.follow = Follow::New,
                "x" | "decode" => options.decode = true,
                "T" | "ctime" => options.time = Time::Ctime,
                "t" | "notime" => options.time = Time::None,
                "h" | "help" => options.action = Action::Help,
                "V" | "version" => options.action = Action::Version,
                other => return Err(format!("unrecognized option '{other}'")),
            }
        }
    }
    if options.follow != Follow::No && options.syslog {
        return Err("--follow needs /dev/kmsg, not --syslog".to_owned());
    }
    Ok(options)
}
