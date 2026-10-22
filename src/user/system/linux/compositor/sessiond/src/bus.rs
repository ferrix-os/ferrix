//! The session's bus (`docs/GNOME.md`, stage G1; the certification
//! consultant's conditions 4 and 5, ledger lines 3100 and 3123).
//!
//! With `--session-bus DAEMON CONFIG`, `sessiond` starts the account's
//! session bus beside the compositor, at the same point and under the same
//! rules: as the account, through the same `become_account`, in the same
//! `session-N.scope`, asked of the init before either execs (E1), and never
//! started again (E2). It ends with the session, as everything in the scope
//! does, and every service it starts is its child and in the scope too.
//!
//! The daemon runs as `DAEMON --config-file=CONFIG --nofork --nopidfile
//! --address=unix:path=$XDG_RUNTIME_DIR/bus --print-address=3`: its socket
//! in the account's `0700` runtime directory, where the D-Bus libraries look
//! for it when `DBUS_SESSION_BUS_ADDRESS` is not set -- so the compositor's
//! environment does not change, and a session whose bus never came has no
//! address that names nothing. Its descriptors are 0 to 2, `sessiond`'s
//! own output, and 3, the write end of a pipe it prints its address on;
//! every other descriptor `sessiond` holds is close-on-exec. `sessiond`
//! reads the address before it lets the compositor exec, for at most
//! [`ADDRESS_PATIENCE`], and says when none came; it never passes the
//! pipe on.
//!
//! Its environment is cleared to `HOME`, `USER`, `LOGNAME`, `SHELL`, `PATH`
//! and `XDG_RUNTIME_DIR`, and each `--session-env NAME=VALUE` given.

use std::io::Read as _;
use std::os::fd::AsRawFd as _;
use std::time::{Duration, Instant};

/// How long the bus has to print its address before the compositor is
/// started without waiting for it.
pub(crate) const ADDRESS_PATIENCE: Duration = Duration::from_secs(10);

/// The descriptor the bus prints its address on.
pub(crate) const ADDRESS_FD: i32 = 3;

/// The bus a session is asked to have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bus {
    /// The daemon's program.
    pub(crate) daemon: String,
    /// Its configuration file.
    pub(crate) config: String,
    /// `NAME=VALUE` pairs for its environment.
    pub(crate) env: Vec<(String, String)>,
}

/// The `--session-bus DAEMON CONFIG` and `--session-env NAME=VALUE` words,
/// taken out of `args` before the `--`; the rest is left in order.
pub(crate) fn split(args: &[String]) -> Result<(Option<Bus>, Vec<String>), String> {
    let mut bus: Option<(String, String)> = None;
    let mut env = Vec::new();
    let mut rest = Vec::new();
    let mut words = args.iter();
    let mut options = true;
    while let Some(word) = words.next() {
        if options && word == "--session-bus" {
            let (Some(daemon), Some(config)) = (words.next(), words.next()) else {
                return Err("--session-bus takes a daemon and a configuration file".to_owned());
            };
            bus = Some((daemon.clone(), config.clone()));
            continue;
        }
        if options && word == "--session-env" {
            let pair = words
                .next()
                .and_then(|pair| pair.split_once('='))
                .filter(|(name, _)| !name.is_empty())
                .ok_or_else(|| "--session-env takes NAME=VALUE".to_owned())?;
            env.push((pair.0.to_owned(), pair.1.to_owned()));
            continue;
        }
        options &= word != "--";
        rest.push(word.clone());
    }
    if bus.is_none() && !env.is_empty() {
        return Err("--session-env is for --session-bus".to_owned());
    }
    Ok((
        bus.map(|(daemon, config)| Bus {
            daemon,
            config,
            env,
        }),
        rest,
    ))
}

/// The bus's socket, in the account's runtime directory.
pub(crate) fn socket(runtime: &str) -> String {
    format!("{runtime}/bus")
}

/// The daemon's arguments, after its program.
pub(crate) fn arguments(bus: &Bus, runtime: &str) -> Vec<String> {
    vec![
        format!("--config-file={}", bus.config),
        "--nofork".to_owned(),
        "--nopidfile".to_owned(),
        format!("--address=unix:path={}", socket(runtime)),
        format!("--print-address={ADDRESS_FD}"),
    ]
}

/// The first line the bus printed on `pipe`, the read end of its address
/// pipe, within `patience`; `None` when it printed none, closed it, or
/// took longer.
pub(crate) fn address(pipe: &std::fs::File, patience: Duration) -> Option<String> {
    let until = Instant::now() + patience;
    let mut said = Vec::new();
    let mut chunk = [0_u8; 256];
    loop {
        if let Some(end) = said.iter().position(|&byte| byte == b'\n') {
            let line = said.get(..end).unwrap_or_default();
            return Some(String::from_utf8_lossy(line).into_owned());
        }
        let left = until.checked_duration_since(Instant::now())?;
        let mut polled = libc::pollfd {
            fd: pipe.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = libc::c_int::try_from(left.as_millis()).unwrap_or(libc::c_int::MAX);
        #[expect(
            unsafe_code,
            reason = "AUDIT: poll of one entry that lives for the call, with a timeout"
        )]
        // SAFETY: as the reason says.
        let ready = unsafe { libc::poll(&raw mut polled, 1, millis) };
        if ready <= 0 {
            return None;
        }
        let got = (&*pipe).read(&mut chunk).ok()?;
        if got == 0 {
            return None;
        }
        said.extend_from_slice(chunk.get(..got).unwrap_or_default());
        if said.len() > 4096 {
            return None;
        }
    }
}

/// Wait for the bus to have gone after the session's end, for at most
/// `patience`: the scope's stop has killed it, and this reaps it. Whether
/// it had.
pub(crate) fn reap(bus: &mut std::process::Child, patience: Duration) -> bool {
    let until = Instant::now() + patience;
    loop {
        if matches!(bus.try_wait(), Ok(Some(_)) | Err(_)) {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn the_bus_options_come_out_before_the_program() {
        let (bus, rest) = split(&words(
            "--user ferrix --session-bus /data/usr/bin/dbus-daemon /etc/ferrix/session-bus.conf \
             --session-env XDG_DATA_DIRS=/data/usr/share:/usr/share -- /bin/hyprix --session-bus x y",
        ))
        .unwrap();
        assert_eq!(
            bus,
            Some(Bus {
                daemon: "/data/usr/bin/dbus-daemon".to_owned(),
                config: "/etc/ferrix/session-bus.conf".to_owned(),
                env: vec![(
                    "XDG_DATA_DIRS".to_owned(),
                    "/data/usr/share:/usr/share".to_owned()
                )],
            })
        );
        // The program's own words, after `--`, are its own.
        assert_eq!(
            rest,
            words("--user ferrix -- /bin/hyprix --session-bus x y")
        );
        let (none, rest) = split(&words("--user ferrix -- /bin/hyprix")).unwrap();
        assert_eq!(none, None);
        assert_eq!(rest.len(), 4);
        assert!(split(&words("--session-bus /bin/x")).is_err());
        assert!(split(&words("--session-env A=b -- /bin/hyprix")).is_err());
        assert!(split(&words("--session-bus a b --session-env =b")).is_err());
    }

    #[test]
    fn the_daemon_listens_in_the_runtime_directory_and_prints_on_three() {
        let bus = Bus {
            daemon: "/bin/fakebus".to_owned(),
            config: "/etc/c".to_owned(),
            env: Vec::new(),
        };
        assert_eq!(
            arguments(&bus, "/run/user/1000"),
            words(
                "--config-file=/etc/c --nofork --nopidfile \
                 --address=unix:path=/run/user/1000/bus --print-address=3"
            )
        );
    }

    #[test]
    fn the_address_is_the_first_line_or_nothing_in_time() {
        let (read, mut write) = crate::gate::pipe().unwrap();
        std::io::Write::write_all(&mut write, b"unix:path=/run/user/1000/bus,guid=1\nmore")
            .unwrap();
        assert_eq!(
            address(&read, Duration::from_secs(1)).as_deref(),
            Some("unix:path=/run/user/1000/bus,guid=1")
        );
        // Nothing said: none, after the patience.
        let (read, _write) = crate::gate::pipe().unwrap();
        assert_eq!(address(&read, Duration::from_millis(50)), None);
        // Closed without a line: none, at once.
        let (read, write) = crate::gate::pipe().unwrap();
        drop(write);
        assert_eq!(address(&read, Duration::from_secs(5)), None);
    }
}
