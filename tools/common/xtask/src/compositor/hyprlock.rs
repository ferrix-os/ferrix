//! The hyprlock boots: `hyprlock`, where a lock taken with `L` refuses a
//! wrong password through `authd` and lets the right one through, and
//! `hyprlock-unset`, where with no password set hyprlock refuses to lock at
//! all (decision 4), both `docs/AUTH.md` P1.5's; and `hyprlock-session`,
//! P2.5's, where the session is the user `ferrix`'s and a lock goes only on
//! `authd`'s grant.
//!
//! All run `/bin/hyprlock` against a real `authd` in the image, as a
//! desktop does. The first two are root's session, as every judged boot's
//! is, so the password is root's.

use std::path::Path;
use std::time::{Duration, Instant};

use super::boot::{Wanted, boot_and_dump_carrying, judge_still_running, judged_image, press};
use super::{Carried, EITHER, MARKER, Programs, SETTLE, build};
use crate::args::Args;
use crate::display::{Qmp, free_port};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The hyprlock boot's configuration: the two windows, the German layout
/// the customer types on, and the key that runs hyprlock.
const HYPRLOCK_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
input:kb_layout = de
input:kb_variant = nodeadkeys
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
bind = , L, exec, /bin/hyprlock -c /etc/hypr/hyprlock.conf
";

/// Root's password in the hyprlock boot's image: seeded into `authd`'s store
/// by this boot alone. Typed on a German keyboard, its last key is the one
/// an American keyboard calls Y, so it only matches if the layout is German.
const HYPRLOCK_PASSWORD: &str = "gatez";

/// What the hyprlock boot's screen must show, in order.
const HYPRLOCK_EXPECTED: [(&str, &str); 5] = [
    (
        "tiled",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
    ),
    (
        "locked by hyprlock, its field empty",
        "app:hyprlock/tests/data/hyprlock-locked.xrle",
    ),
    (
        "five characters typed, five dots",
        "app:hyprlock/tests/data/hyprlock-dots.xrle",
    ),
    (
        "a wrong password refused, the field in fail_color",
        "app:hyprlock/tests/data/hyprlock-failed.xrle",
    ),
    (
        "the windows again, once the right password let the lock go",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
    ),
];

/// The keys between them. Each is pressed as a chord and let go in reverse,
/// which types the letters in order; no letter twice in one, since a key
/// already held does not go down again. `y` is where a German keyboard has
/// `z`, so the password `gatez` only matches if the layout is German.
const HYPRLOCK_BINDS: [(&str, &[&str]); 4] = [
    ("L, which runs hyprlock", &["l"]),
    ("w r o n g", &["w", "r", "o", "n", "g"]),
    ("Return", &["ret"]),
    (
        "g a t e z and Return, on a German keyboard",
        &["g", "a", "t", "e", "y", "ret"],
    ),
];

/// A boot of hyprlock on the customer's layout: lock, a wrong password and
/// its failure, the right one, unlock.
///
/// It runs `/bin/hyprlock` as a desktop does, against `authd` with a
/// password for root seeded into the image (`docs/AUTH.md` §5.3): the
/// session is root in phase 1 (decision 3), so root's password is what
/// unlocks it. The seed is the gate's own and only this boot carries it;
/// no other image gets a password it did not ask for. The configuration is
/// the crate's own test data; the pictures are drawn from it on the host by
/// the hyprlock app's `tests/gate.rs`, with the same code, and
/// composited as hyprix composites a lock surface.
pub(super) fn test_hyprlock(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let carried = Carried {
        ports: hyprlock_files(arch, Some(("root", HYPRLOCK_PASSWORD)))?,
        ..Carried::none()
    };
    let (screens, said) = boot_and_dump_carrying(
        arch,
        programs,
        HYPRLOCK_CONFIG,
        (carried, None),
        &Wanted {
            states: &HYPRLOCK_EXPECTED,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &["hyprlock: unlocked"],
        },
        &HYPRLOCK_BINDS,
        args,
    )?;
    if screens.len() != HYPRLOCK_EXPECTED.len() {
        return Err(Error::new(format!(
            "{arch}: {} of {} pictures were taken",
            screens.len(),
            HYPRLOCK_EXPECTED.len()
        )));
    }
    let has = |wanted: &str| said.iter().any(|line| line.contains(wanted));
    for wanted in [
        "hyprix: the session is locked",
        "hyprlock: locked",
        // authd's audit lines: the refusal and the acceptance were its.
        "service=hyprlock account=root",
        "result=failed",
        "hyprlock: Authentication failed",
        "result=accepted",
        "hyprlock: authenticated",
        "hyprlock: unlocked",
        "hyprix: the session is unlocked",
    ] {
        if !has(wanted) {
            return Err(Error::new(format!(
                "{arch}: the hyprlock boot did not say `{wanted}`"
            )));
        }
    }
    println!(
        "  {arch}: hyprlock locked the screen, authd refused a wrong password and hyprlock showed \
         its fail colour, authd took the right one typed on a German keyboard, and the screen \
         came back"
    );
    Ok(())
}

/// The second hyprlock boot: the same image with no password seeded, where
/// `L` must not lock (`docs/AUTH.md` §5.4, decision 4) and hyprlock must say
/// why.
pub(super) fn test_hyprlock_unset(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let carried = Carried {
        ports: hyprlock_files(arch, None)?,
        ..Carried::none()
    };
    let (_, said) = boot_and_dump_carrying(
        arch,
        programs,
        HYPRLOCK_CONFIG,
        (carried, None),
        &Wanted {
            states: &HYPRLOCK_UNSET_EXPECTED,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &["not locking"],
        },
        &HYPRLOCK_UNSET_BINDS,
        args,
    )?;
    let has = |wanted: &str| said.iter().any(|line| line.contains(wanted));
    if !has("not locking: no password is set for root: run `passwd` first") {
        return Err(Error::new(format!(
            "{arch}: hyprlock with no password did not say why it did not lock"
        )));
    }
    if has("hyprix: the session is locked") {
        return Err(Error::new(format!(
            "{arch}: hyprlock locked an account with no password"
        )));
    }
    println!("  {arch}: with no password set, hyprlock did not lock, and said why");
    Ok(())
}

/// What the no-password boot must show: the windows, before and after `L`.
const HYPRLOCK_UNSET_EXPECTED: [(&str, &str); 2] = [
    (
        "tiled",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
    ),
    (
        "still tiled after L, since nothing could unlock a lock",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
    ),
];

/// Its one key.
const HYPRLOCK_UNSET_BINDS: [(&str, &[&str]); 1] = [("L, which runs hyprlock", &["l"])];

/// What the hyprlock boots carry: hyprlock and its configuration, authd,
/// root's, `ferrix`'s and authd's accounts, the font, and one account's
/// password where one is given.
fn hyprlock_files(arch: Arch, seed: Option<(&str, &str)>) -> Result<Vec<crate::ports::File>> {
    let data = crate::apps::folder("hyprlock")?.join("tests/data");
    let read = |path: &Path| -> Result<Vec<u8>> {
        std::fs::read(path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
    };
    let file = |path: &str, mode: u32, bytes: Vec<u8>| crate::ports::File {
        path: path.to_owned(),
        mode,
        content: crate::ports::Content::Bytes(bytes),
    };
    let fonts = paths::workspace_root().join("assets/fonts/liberation");
    let hyprlock = crate::apps::program(arch, "hyprlock", "hyprlock")?;
    let mut ports = crate::auth::carried(arch, None)?;
    if ports.is_empty() {
        return Err(Error::new(format!("{arch}: authd is not built for it")));
    }
    ports.extend([
        file("bin/hyprlock", 0o755, read(&hyprlock)?),
        file(
            "etc/hypr/hyprlock.conf",
            0o644,
            read(&data.join("gate.conf"))?,
        ),
        // Root, whom the lock is for, and authd's own account.
        file(
            "etc/passwd",
            0o644,
            format!(
                "root:x:0:0:root:/:/bin/sh\nferrix:x:1000:1000:ferrix:/home/ferrix:/bin/sh\n{}",
                crate::auth::PASSWD_LINE
            )
            .into_bytes(),
        ),
        file(
            "etc/group",
            0o644,
            format!("root:x:0:\nferrix:x:1000:\n{}", crate::auth::GROUP_LINE).into_bytes(),
        ),
        file(
            "usr/share/ferrix/fonts/LiberationSans-Regular.ttf",
            0o644,
            read(&fonts.join("LiberationSans-Regular.ttf"))?,
        ),
    ]);
    if let Some((account, password)) = seed {
        ports.push(crate::auth::seed(account, password)?);
    }
    Ok(ports)
}

/// The session boot's configuration: as [`HYPRLOCK_CONFIG`], with hyprlock
/// on a `bindl`, which works while the screen is locked -- how a person at a
/// locked screen whose locker died starts a new one (`docs/AUTH.md` §3.7) --
/// and the attacker started beside the windows.
const SESSION_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor --boot hyprlock-session`.
input:kb_layout = de
input:kb_variant = nodeadkeys
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
exec-once = /bin/sh /etc/attack.sh
bindl = , L, exec, /bin/hyprlock -c /etc/hypr/hyprlock.conf
";

/// A program of the session's user, as any could be: it lets the person's
/// first hyprlock come and go, kills the second once it holds the lock,
/// takes the lock over with `/bin/lock` and asks to unlock at once (the
/// certification consultant's G6), then tries to open the compositor's lock
/// channel through `/proc` (G5).
const ATTACK: &str = "\
#!/bin/sh
exec 2>&1
echo attack-started
# The pid of the process called $1, by /proc's comm: what pidof does.
named() {
\tfor dir in /proc/[0-9]*; do
\t\t[ \"$(cat $dir/comm 2>/dev/null)\" = \"$1\" ] && echo ${dir#/proc/}
\tdone
}
# The person's own first hyprlock, by its pid: the next may start the
# moment it ends.
first=
while [ -z \"$first\" ]; do first=$(named hyprlock); sleep 1; done
while [ -d /proc/$first ]; do sleep 1; done
echo attack-waiting
while [ -z \"$(named hyprlock)\" ]; do sleep 1; done
sleep 4
kill -9 $(named hyprlock)
echo hyprlock-killed
sleep 1
/bin/lock 0
sleep 3
cat /proc/$(named hyprix)/fd/4 </dev/null >/dev/null 2>&1
echo \"fd4-probe: $? as $(id -u)\"
";

/// What the session boot is waited for after each step, and what it must
/// not say in between: the line that ends a step, its lines that must come
/// with it, and the lines that may not.
struct Step {
    what: &'static str,
    keys: &'static [&'static [&'static str]],
    until: &'static str,
    wanted: &'static [&'static str],
    unwanted: &'static [&'static str],
}

/// `ferrix`'s password, typed key by key on a German keyboard: `y` is `z`.
const SESSION_PASSWORD: [&[&str]; 6] = [&["g"], &["a"], &["t"], &["e"], &["y"], &["ret"]];

/// The steps, in order.
const SESSION_STEPS: [Step; 6] = [
    Step {
        what: "L: hyprlock locks",
        keys: &[&["l"]],
        until: "hyprlock: locked",
        wanted: &["hyprix: the session is locked"],
        unwanted: &[],
    },
    Step {
        what: "ferrix's password, granted by authd for lock 1, unlocks",
        keys: &SESSION_PASSWORD,
        until: "hyprix: the session is unlocked",
        wanted: &[
            "service=hyprlock account=ferrix",
            "result=granted",
            "seat-epoch=1",
        ],
        unwanted: &[],
    },
    Step {
        what: "L: hyprlock locks again",
        keys: &[&["l"]],
        until: "hyprlock: locked",
        wanted: &[],
        unwanted: &[],
    },
    Step {
        what: "a program of ferrix's killed hyprlock, took the lock over and asked to unlock at \
               once: the screen stayed locked",
        keys: &[],
        // The attacker's `/bin/lock` believes it unlocked, and exits: its
        // unlock waited for a grant, and its going orphans the lock again.
        // A refusal by the wait's timeout is the host tests' case.
        until: "lock: locked 1 screen(s) and unlocked again",
        wanted: &[
            "hyprlock-killed",
            "the program holding the lock went; the screen stays locked",
            "a new program took over the lock; the screen stays locked",
            "an unlock waits for authd's grant",
        ],
        unwanted: &["hyprix: the session is unlocked"],
    },
    Step {
        what: "the same program could not open the compositor's lock channel through /proc",
        keys: &[],
        until: "fd4-probe: ",
        wanted: &["as 1000"],
        unwanted: &["fd4-probe: 0 ", "hyprix: the session is unlocked"],
    },
    Step {
        what: "L, bound with bindl: a new hyprlock took the lock over",
        keys: &[&["l"]],
        until: "hyprlock: locked",
        wanted: &["a new program took over the lock"],
        unwanted: &["hyprix: the session is unlocked"],
    },
];

/// P2.5's boot (`docs/AUTH.md` §3.7): the desktop as `ferrix`, whose
/// locks go on `authd`'s grant through `sessiond`. The certification
/// consultant's G5 and G6 are steps of it: a uid-1000 program cannot open the
/// compositor's lock channel, and killing hyprlock to take its lock over and
/// unlock at once leaves the screen locked.
pub(super) fn test_hyprlock_session(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let password = Step {
        what: "the password, to the hyprlock that took the lock over",
        keys: &SESSION_PASSWORD,
        until: "hyprix: the session is unlocked",
        wanted: &["result=granted", "seat-epoch=4"],
        unwanted: &[],
    };
    let mut steps: Vec<&Step> = SESSION_STEPS.iter().collect();
    // The last step's password, typed once its lock is up.
    steps.push(&password);
    let Some(said) = session_boot(arch, programs, args, SESSION_CONFIG, ATTACK, &steps)? else {
        return Ok(());
    };
    judge_still_running(arch, &said)?;
    for wanted in [
        "authd: offering ferrix.auth.seat",
        "ferrix.auth.seat is up",
        "hyprix: the session's locks go on authd's grant",
        "hyprix: authd can grant the session's locks",
    ] {
        if !said.iter().any(|line| line.contains(wanted)) {
            return Err(Error::new(format!(
                "{arch}: the session boot never said `{wanted}`"
            )));
        }
    }
    println!(
        "  {arch}: as ferrix, a lock went only on authd's grant; killing hyprlock and taking its \
         lock over with an immediate unlock left the screen locked; the lock channel would not \
         open through /proc; and a new hyprlock took the lock over and the password let it go"
    );
    Ok(())
}

/// A desktop as `ferrix` from `config`, with `script` at `/etc/attack.sh`,
/// `/bin/lock` and `ferrix`'s password seeded, driven through `steps`: the
/// transcript, or `None` where the machine has no busybox for the keys.
fn session_boot(
    arch: Arch,
    programs: &Programs,
    args: &Args,
    config: &str,
    script: &str,
    steps: &[&Step],
) -> Result<Option<Vec<String>>> {
    let Some(busybox) = crate::busybox::installed_program(arch) else {
        println!("  {arch}: no busybox on this machine for the boot's keys; skipped");
        return Ok(None);
    };
    let mut ports = hyprlock_files(arch, Some(("ferrix", HYPRLOCK_PASSWORD)))?;
    ports.push(crate::ports::File {
        path: "etc/attack.sh".to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(script.as_bytes().to_vec()),
    });
    let lock = build(arch, "compositor-lock", "lock")?;
    ports.push(crate::ports::File {
        path: "bin/lock".to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(
            std::fs::read(&lock)
                .map_err(|error| Error::new(format!("reading {}: {error}", lock.display())))?,
        ),
    });
    let carried = Carried {
        busybox: Some(busybox),
        ports,
        ..Carried::none()
    };
    let session = Args {
        session_user: Some(crate::session::USER.to_owned()),
        ..args.clone()
    };
    let (image, kernel) = judged_image(arch, programs, config, carried, None, &session)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let mut said = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        said = drive_session(arch, port, watching, steps)?;
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    Ok(Some(said))
}

fn drive_session(
    arch: Arch,
    port: u16,
    watching: &mut Watching<'_>,
    steps: &[&Step],
) -> Result<Vec<String>> {
    let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
    let up = watching.read_more(Instant::now() + SETTLE, |lines| {
        lines.iter().any(|line| line.contains(MARKER))
    })?;
    if !up {
        return Err(Error::new(format!(
            "{arch}: the compositor never printed `{MARKER}`"
        )));
    }
    // The seat channel comes up beside the compositor; a lock taken
    // before it would be refused, which is right but not this boot.
    let _ = watching.read_more(Instant::now() + Duration::from_secs(30), |lines| {
        lines
            .iter()
            .any(|line| line.contains("authd can grant the session's locks"))
    })?;
    for step in steps {
        // Only what comes after this step's keys counts for it: the same
        // line said by an earlier step is not this one's.
        let from = watching.after().len();
        for keys in step.keys {
            press(&mut qmp, keys)?;
            std::thread::sleep(Duration::from_millis(300));
        }
        let done = watching.read_more(Instant::now() + Duration::from_secs(60), |lines| {
            lines
                .iter()
                .skip(from)
                .any(|line| line.contains(step.until))
        })?;
        let after = watching.after();
        let these = after.get(from.min(after.len())..).unwrap_or_default();
        if !done {
            return Err(Error::new(format!(
                "{arch}: {}: `{}` never came",
                step.what, step.until
            )));
        }
        // What must not happen is said first: an attack that worked is
        // reported as itself, not as a line it made go missing.
        if let Some(line) = these
            .iter()
            .find(|line| step.unwanted.iter().any(|bad| line.contains(bad)))
        {
            return Err(Error::new(format!(
                "{arch}: {}: {}",
                step.what,
                line.trim()
            )));
        }
        for wanted in step.wanted {
            if !these.iter().any(|line| line.contains(wanted)) {
                return Err(Error::new(format!(
                    "{arch}: {}: nothing said `{wanted}`",
                    step.what
                )));
            }
        }
        println!("  {arch}: {}", step.what);
    }
    Ok(watching
        .lines()
        .iter()
        .chain(watching.after())
        .cloned()
        .collect())
}

/// P2.7's boot's configuration: hyprlock on a `bindl`, and the
/// compromised client started beside the windows.
const END_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor --boot session-end`.
input:kb_layout = de
input:kb_variant = nodeadkeys
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
exec-once = /bin/sh /etc/attack.sh
bindl = , L, exec, /bin/hyprlock -c /etc/hypr/hyprlock.conf
";

/// The compromised client of P2.7 (`docs/AUTH.md` §6.4), a program of
/// ferrix's as any could be: a heartbeat in the session, a process that
/// moves itself into a scope of its own to outlive the session, a lock it
/// is refused while hyprlock holds the screen and an unlock of it, a read
/// of `authd`'s store, and then `kill -9` of the compositor.
const END_SCRIPT: &str = "\
#!/bin/sh
exec 2>&1
named() {
\tfor dir in /proc/[0-9]*; do
\t\t[ \"$(cat $dir/comm 2>/dev/null)\" = \"$1\" ] && echo ${dir#/proc/}
\tdone
}
( while :; do echo heartbeat; sleep 1; done ) &
compositor=$(named hyprix)
# Out of the session's scope, into one of ferrix's own, as a user may: it
# outlives the session, and then asks the init for the session back.
sh -c 'svc scope --unit ferrix-linger.scope --slice user-1000.slice $$ >/dev/null 2>&1
echo \"linger-scope: $?\"
while [ -d /proc/$1 ]; do sleep 1; done
sleep 4
out=$(svc start hyprix.service 2>&1)
echo \"linger-svc-start: $? $out\"
out=$(svc restart hyprix.service 2>&1)
echo \"linger-svc-restart: $? $out\"
seen=no
for dir in /proc/[0-9]*; do [ \"$(cat $dir/comm 2>/dev/null)\" = login ] && seen=yes; done
echo \"console-login: $seen\"' linger \"$compositor\" &
while [ -z \"$(named hyprlock)\" ]; do sleep 1; done
sleep 3
/bin/lock 0
echo refused-lock-done
out=$(cat /var/lib/ferrix/auth/users/ferrix 2>&1)
echo \"store-probe: $? $out\"
sleep 1
kill -9 $compositor
echo hyprix-killed
";

/// P2.7's steps.
const END_STEPS: [Step; 6] = [
    Step {
        what: "L: hyprlock locks",
        keys: &[&["l"]],
        until: "hyprlock: locked",
        wanted: &[],
        unwanted: &[],
    },
    Step {
        what: "a program of ferrix's asked for a lock while hyprlock held it, was refused, and its \
               unlock let nothing go",
        keys: &[],
        until: "refused-lock-done",
        wanted: &["a second program asked to lock the session and was refused"],
        unwanted: &["hyprix: the session is unlocked"],
    },
    Step {
        what: "it could not read authd's store: the record is there, and not ferrix's to read",
        keys: &[],
        until: "store-probe: ",
        // On the probe's own line: cat's refusal of that very path.
        wanted: &["can't open '/var/lib/ferrix/auth/users/ferrix': Permission denied"],
        unwanted: &["store-probe: 0", "No such file"],
    },
    Step {
        what: "it killed the compositor, and the session ended: every process of it stopped",
        keys: &[],
        until: "the session's processes were stopped",
        wanted: &["hyprix-killed", "the session ended"],
        unwanted: &["hyprix: the session is unlocked"],
    },
    Step {
        what: "a process of ferrix's that had left the session asked the init for it back with \
               svc start and svc restart, and was refused both",
        keys: &[],
        until: "linger-svc-restart: ",
        wanted: &[
            "linger-svc-start: ",
            "Permission denied: only root may change the system",
        ],
        unwanted: &["linger-svc-start: 0", "linger-svc-restart: 0"],
    },
    Step {
        what: "the console is a login, the seat the session went back to",
        keys: &[],
        until: "console-login: ",
        wanted: &["console-login: yes"],
        unwanted: &[],
    },
];

/// P2.7's boot (`docs/AUTH.md` §6.4): a compromised client of the session
/// as ferrix kills the compositor, and lands at the console's login, not on
/// a fresh desktop. The certification consultant's E2 to E4.
pub(super) fn test_session_end(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let steps: Vec<&Step> = END_STEPS.iter().collect();
    let Some(said) = session_boot(arch, programs, args, END_CONFIG, END_SCRIPT, &steps)? else {
        return Ok(());
    };
    let Some(end) = said
        .iter()
        .position(|line| line.contains("the session's processes were stopped"))
    else {
        return Err(Error::new(format!("{arch}: the session never ended")));
    };
    let after = said.get(end..).unwrap_or_default();
    // Said at the start, before any key: the lingering process left the
    // session's scope, so what it does after the end is the residual's.
    if !said.iter().any(|line| line.contains("linger-scope: 0")) {
        return Err(Error::new(format!(
            "{arch}: the lingering process could not make a scope of its own"
        )));
    }
    let sessions = said
        .iter()
        .filter(|line| line.contains("sessiond: seat0's session for ferrix"))
        .count();
    if sessions != 1 {
        return Err(Error::new(format!(
            "{arch}: the session started {sessions} times: a killed compositor came back"
        )));
    }
    // The most direct sign of a compositor that came back is said first;
    // a restarted session's own programs would trip the heartbeat too.
    if let Some(line) = after
        .iter()
        .find(|line| line.contains("hyprix.service: active"))
    {
        return Err(Error::new(format!(
            "{arch}: the compositor's unit was started again: {}",
            line.trim()
        )));
    }
    // E4: the end shown, not only said.
    if let Some(line) = after.iter().find(|line| line.contains("heartbeat")) {
        return Err(Error::new(format!(
            "{arch}: a program of the session outlived its end: {}",
            line.trim()
        )));
    }
    println!(
        "  {arch}: as ferrix, a refused lock unlocked nothing, the store stayed closed, and \
         killing the compositor ended the session at the console's login, no process of it left \
         and nothing of ferrix's able to start it again"
    );
    Ok(())
}
