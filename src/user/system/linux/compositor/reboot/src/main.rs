//! `reboot`, as systemd's reads its arguments: nothing restarts the machine,
//! `--firmware-setup` restarts it into the firmware, and a word restarts it
//! with that word handed to the firmware through `reboot(2)`'s
//! `LINUX_REBOOT_CMD_RESTART2`.
//!
//! busybox's `reboot` has no way to pass the word, which is the only way a
//! program has to say where the machine should come back up. On an STM32MP15
//! DK board the kernel turns it into the forced boot mode U-Boot reads at its
//! next start (`src/kernel/src/platform/st/stm32mp1.rs`): `firmware` stops at U-Boot's
//! prompt, `ums` puts the card on USB for flashing, `fastboot` starts
//! fastboot. Elsewhere the word is printed and the machine restarts as it
//! would without it, which is what Linux does with a word nothing reads.
//!
//! It restarts the machine directly, as `reboot -f` does, and takes the name
//! from init's `svc` on the desktop it is carried on: `svc reboot` is the
//! restart that stops the services in order first, and has no word to pass.
//! What has been written is synced before it goes.
//!
//! Run by anyone but root with no word, it is `svc reboot`: the init's
//! ordered restart, which the seat's person may ask for (`docs/INIT.md`
//! §10.1) and reboot(2) would refuse.

/// The init's client, which restarts the machine for a person who is not
/// root.
#[cfg(target_os = "linux")]
const SVC: &str = "/bin/svc";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Asked {
    /// Restart.
    Restart,
    /// Restart with a word for the firmware.
    With(String),
    /// Print the usage.
    Help,
}

/// The word `--firmware-setup` hands the firmware.
const FIRMWARE: &str = "firmware";

const USAGE: &str = "usage: reboot [-f] [--firmware-setup | WORD]\n\
    \n\
    Restart the machine now. --firmware-setup comes back up at the\n\
    firmware's prompt; WORD is passed to the firmware as reboot(2)'s\n\
    RESTART2 command. On an STM32MP15 DK board U-Boot understands\n\
    firmware (or recovery), ums (the SD card on USB) and fastboot.\n";

/// Read the arguments after the program's name.
fn parse(args: &[String]) -> Result<Asked, String> {
    let mut word: Option<String> = None;
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Asked::Help),
            // Always forced: see the module's comment.
            "-f" | "--force" => {}
            "--firmware-setup" | "--firmware" => {
                if word.replace(FIRMWARE.to_owned()).is_some() {
                    return Err("only one of --firmware-setup and a word".to_owned());
                }
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown option {option}"));
            }
            given => {
                if word.replace(given.to_owned()).is_some() {
                    return Err("only one word for the firmware".to_owned());
                }
            }
        }
    }
    Ok(word.map_or(Asked::Restart, Asked::With))
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use std::ffi::CString;
    use std::io::Write;
    use std::process::ExitCode;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let asked = match parse(&args) {
        Ok(asked) => asked,
        Err(why) => {
            let _ = write!(std::io::stderr(), "reboot: {why}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let word = match asked {
        Asked::Help => {
            let _ = write!(std::io::stdout(), "{USAGE}");
            return ExitCode::SUCCESS;
        }
        Asked::Restart => None,
        Asked::With(word) => match CString::new(word) {
            Ok(word) => Some(word),
            Err(_) => {
                let _ = writeln!(std::io::stderr(), "reboot: the word has a NUL in it");
                return ExitCode::from(2);
            }
        },
    };
    // A person who is not root asks the init, which restarts the machine in
    // order and lets the seat's person do it (`docs/INIT.md` §10.1);
    // reboot(2) would refuse them. A word for the firmware is root's.
    // SAFETY: `geteuid` takes nothing and cannot fail.
    if word.is_none() && unsafe { libc::geteuid() } != 0 {
        use std::os::unix::process::CommandExt as _;
        let error = std::process::Command::new(SVC).arg("reboot").exec();
        let _ = writeln!(std::io::stderr(), "reboot: {SVC}: {error}");
        return ExitCode::FAILURE;
    }
    // SAFETY: `sync` takes nothing and cannot fail.
    unsafe { libc::sync() };
    let result = match &word {
        // SAFETY: `reboot` with a command that takes no argument.
        None => unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_RESTART) },
        // SAFETY: the magic numbers `reboot(2)` requires, RESTART2, and a
        // NUL-terminated string that lives across the call.
        Some(word) => unsafe {
            libc::syscall(
                libc::SYS_reboot,
                libc::LINUX_REBOOT_MAGIC1,
                libc::LINUX_REBOOT_MAGIC2,
                libc::LINUX_REBOOT_CMD_RESTART2,
                word.as_ptr(),
            ) as libc::c_int
        },
    };
    // Only a refusal returns.
    let error = std::io::Error::last_os_error();
    let _ = writeln!(std::io::stderr(), "reboot: {error} ({result})");
    ExitCode::FAILURE
}

/// Only Linux, and Ferrix through its Linux ABI, have `reboot(2)`; elsewhere
/// the crate builds so its argument parsing is tested on any host.
#[cfg(not(target_os = "linux"))]
fn main() {
    let _ = (parse(&[]), USAGE);
}

#[cfg(test)]
mod tests {
    use super::{Asked, parse};

    fn read(args: &[&str]) -> Result<Asked, String> {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        parse(&args)
    }

    #[test]
    fn nothing_restarts() {
        assert_eq!(read(&[]), Ok(Asked::Restart), "plain reboot");
        assert_eq!(read(&["-f"]), Ok(Asked::Restart), "forced is the same");
    }

    #[test]
    fn firmware_setup_is_the_word_firmware() {
        for spelling in ["--firmware-setup", "--firmware"] {
            assert_eq!(
                read(&[spelling]),
                Ok(Asked::With("firmware".to_owned())),
                "{spelling}"
            );
        }
    }

    #[test]
    fn a_word_is_passed_on() {
        assert_eq!(read(&["ums"]), Ok(Asked::With("ums".to_owned())), "ums");
        assert_eq!(
            read(&["-f", "fastboot"]),
            Ok(Asked::With("fastboot".to_owned())),
            "with -f"
        );
    }

    #[test]
    fn two_words_or_an_unknown_option_are_refused() {
        assert!(read(&["ums", "fastboot"]).is_err(), "two words");
        assert!(read(&["--firmware-setup", "ums"]).is_err(), "both");
        assert!(read(&["--poweroff"]).is_err(), "unknown option");
        assert_eq!(read(&["--help", "ums"]), Ok(Asked::Help), "help wins");
    }
}
