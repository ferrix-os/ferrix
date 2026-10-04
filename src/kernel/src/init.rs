//! The first program: a shell, if one was built in, or a list of commands.
//!
//! Started after the boot marker rather than before it, which is what lets one
//! kernel serve both uses. `cargo xtask test-boot` stops QEMU the moment it
//! sees the marker, so the boot test is unchanged whether or not a shell is
//! embedded; `cargo xtask run` leaves the serial port attached to the terminal,
//! so the same image hands a person a prompt.
//!
//! # Why busybox and not something written for the purpose
//!
//! Because the point is somebody else's binary. A shell written against this
//! kernel would work by construction and prove nothing about the ABI; a static
//! `busybox` was linked against Linux by people who have never heard of
//! Ferrix, and every system call it makes is one this kernel either answers
//! the way Linux does or gets wrong in a way the shell will show.
//!
//! # A list of commands, for stage 8's exit
//!
//! Stage 8's exit criterion is several programs over one filesystem, and this
//! busybox's shell starts every program it does not have built in with
//! `clone`, `execve` and `wait4` (measured in
//! `docs/STAGE8-WHAT-THE-EXIT-NEEDS.md`). So a build can carry a list instead
//! of a script, and init starts each program in turn itself. Each command's
//! program is the name its `argv[0]` has in `/bin`, read from the initramfs
//! through the VFS rather than built into the kernel, because loading a
//! program from a file is part of what the stage is for. Which binary that
//! name belongs to -- busybox, uutils/coreutils, zinc -- is the image's
//! business and not this file's.
//!
//! Each command's start and end go on lines of their own, in a format
//! `tools/common/xtask/src/vfs.rs` parses; the two change together. While a command runs,
//! every call answered `ENOSYS` is reported too, up to a bound, which is what
//! turns a failing run into the name of the call that is missing.
//!
//! # A program named on the command line
//!
//! `ferrix.init=<path>` starts pid 1 from that file instead, in the `/` the
//! kernel switched to, as Linux's `init=` does (`docs/INIT.md` §8.1). It is
//! how a real init starts, and how an image that is not a gate boots without
//! a program built into its kernel. The file may be a `#!` script, which runs
//! under its interpreter as `execve` would run it. A file that is missing or
//! will not start is said on one line and the built-in program runs as if
//! nothing had been named, so a mistyped path costs a boot log line and not a
//! machine that does nothing.
//!
//! With nothing named and nothing built in, `/sbin/init` is started if the
//! image has one, which is §8.1's default. Built-in first, because every gate
//! builds its program in and none of them may change: no image carries a
//! `/sbin/init` today, and one that starts to must not take a gate's boot
//! from it.
//!
//! # A bootstrap channel
//!
//! Every program started here is started with a bootstrap channel, as
//! `devmgr` is (`docs/INIT.md` §6, K2). The kernel writes one message on its
//! end before the program runs -- `src/lib/proto/native-abi`'s `bootstrap` module says
//! what it holds -- and keeps that end for as long as the program runs, so a
//! later message can carry what the first does not. The program takes its end
//! with `process_bootstrap`; one that never does, which is every program a
//! gate runs today, leaves it to be closed when it ends.
//!
//! # Which program, and how
//!
//! This file decides *which* program runs as pid 1 and says how it ended.
//! *How* a program is opened and started -- the root filesystem it is read
//! from, `#!` lines, the dynamic linker, the Linux personality's process --
//! is above the certified item, so init names none of it: the load ring
//! registers a [`Launcher`] at bring-up, from `main.rs`, and the boot checks
//! that it did before the boot marker.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::{BootView, option_in};
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::bootstrap::{
    AUDIT_MAGIC, DEVMGR_STARTER_MAGIC, ROOT_IN_MEMORY, ROOT_MAGIC, ROOT_SWITCHED, after_hello,
    init_hello,
};
use ferrix_native_abi::rights::Rights;
use ferrix_sync::Once;

use crate::console::println;
use crate::fallible;
use crate::object::channel::Endpoint;
use crate::object::{Object, Transfer};
use crate::sync::SpinLock;
use crate::syscall;
use crate::syscall::program::ProgramFile;

/// A program opened to be started: its image, and the path it was read from
/// with its links resolved, which `/proc/self/exe` names.
#[derive(Debug)]
pub(crate) struct Opened {
    /// The file, with its headers read.
    pub(crate) program: ProgramFile,
    /// Where it was read from, resolved.
    pub(crate) exe: Vec<u8>,
}

/// An image to start: the one built into the kernel, or one opened from a
/// file.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Image<'a> {
    /// The built-in program, in the archive the kernel keeps.
    BuiltIn(&'static [u8]),
    /// A file [`Launcher::open`] opened.
    File(&'a ProgramFile),
}

/// Everything a program is started with, as `execve` would be told it.
#[derive(Debug)]
pub(crate) struct Start<'a> {
    /// What runs.
    pub(crate) image: Image<'a>,
    /// What `/proc/self/exe` names.
    pub(crate) exe: &'a [u8],
    /// What `AT_EXECFN` names: the path before it was resolved.
    pub(crate) exec_fn: &'a [u8],
    /// Its arguments.
    pub(crate) argv: &'a [&'a [u8]],
    /// Its environment.
    pub(crate) env: &'a [&'a [u8]],
    /// Its end of its bootstrap channel, for the launcher to hold in the new
    /// process for `process_bootstrap`; `None` when there was no memory for
    /// one.
    pub(crate) bootstrap: Option<Transfer>,
}

/// Why a program that opened would not start.
#[derive(Debug)]
pub(crate) enum Failure {
    /// The dynamic linker it names could not be read.
    Linker(Errno),
    /// It would not load or run, as the personality put it.
    Exec(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Linker(errno) => write!(f, "its linker: errno {}", errno.0),
            Failure::Exec(why) => f.write_str(why),
        }
    }
}

/// Read a `#!` line from the start of a file: its interpreter, and the one
/// argument it may carry.
pub(crate) type InterpreterLine = fn(&[u8]) -> Result<(Vec<u8>, Option<Vec<u8>>), Errno>;

/// How a program is opened and started as pid 1: what init needs from above
/// the certified item, registered by the load ring with
/// [`register_launcher`].
#[derive(Debug)]
pub(crate) struct Launcher {
    /// Open the program at an absolute path in `/`.
    pub(crate) open: fn(&[u8]) -> Result<Opened, Errno>,
    /// Read a `#!` line from the start of a file.
    pub(crate) interpreter_line: InterpreterLine,
    /// Start a program as pid 1 with a fresh `AT_RANDOM` and its bootstrap,
    /// and wait for it to end: its status.
    pub(crate) start: fn(Start<'_>) -> Result<i32, Failure>,
}

/// The registered [`Launcher`].
static LAUNCHER: Once<&'static Launcher> = Once::new();

/// Start programs with `launcher`. The first registration stands.
pub(crate) fn register_launcher(launcher: &'static Launcher) {
    let _ = LAUNCHER.call_once(|| launcher);
}

/// Whether a [`Launcher`] is registered: the boot's check that [`run`] will
/// have something to start init with.
pub(crate) fn has_launcher() -> bool {
    LAUNCHER.get().is_some()
}

/// The kernel's end of the bootstrap channel of the program started last,
/// kept while it runs so that its end does not read as closed, and so that a
/// later message has somewhere to be written.
static CHANNEL: SpinLock<Option<Arc<Endpoint>>> = SpinLock::new(None);

/// A bootstrap channel with the kernel's first message written on it: the
/// kernel's end, and the program's (K2). What [`run`] gives each program it
/// starts, and what the boot check reads. `None` when there was no memory
/// for the channel or the message.
pub(crate) fn bootstrap_channel() -> Option<(Arc<Endpoint>, Arc<Endpoint>)> {
    let (kernel_end, program_end) = Endpoint::pair().ok()?;
    let hello = fallible::try_to_vec(&init_hello()).ok()?;
    kernel_end
        .write(hello, 0, || Ok::<Vec<Transfer>, Infallible>(Vec::new()))
        .ok()?;
    Some((kernel_end, program_end))
}

/// A bootstrap for the next program: its end of a new channel, the kernel's
/// end kept in [`CHANNEL`] in place of the last program's. `None`, said on a
/// line, when there is no memory for one; the program starts without.
fn next_bootstrap() -> Option<Transfer> {
    let Some((kernel_end, program_end)) = bootstrap_channel() else {
        println!("  init     no memory for a bootstrap channel; starting the program without one");
        return None;
    };
    // Under `ferrix.devmgr=init`, the first program -- pid 1 -- is also given
    // devmgr's starter, after the hello, with MANAGE alone (`docs/INIT.md`
    // §7.3). Given once: a later program of a command list gets none.
    if let Some(starter) = crate::discovery::devmgr::starter() {
        let written = fallible::try_to_vec(&after_hello(DEVMGR_STARTER_MAGIC, 1))
            .and_then(|message| Ok((message, fallible::try_with_capacity(1)?)))
            .map_err(|_| ())
            .and_then(|(message, mut transfers): (Vec<u8>, Vec<Transfer>)| {
                let _ = fallible::push_within(&mut transfers, (starter, Rights::MANAGE));
                kernel_end
                    .write(message, 1, || Ok::<Vec<Transfer>, Infallible>(transfers))
                    .map_err(|_| ())
            });
        if written.is_err() {
            println!("  init     devmgr's starter could not be written; nothing will start devmgr");
        } else {
            crate::audit::record(
                crate::audit::STARTER_GIVEN,
                crate::audit::Outcome::Done,
                0,
                crate::audit::Subject::KERNEL,
                crate::audit::Target {
                    kind: crate::audit::target::PROCESS,
                    id: u64::from(crate::object::process::INIT_PID),
                },
                [0; 3],
            );
        }
    }
    give_audit(&kernel_end);
    let last = CHANNEL.lock().replace(kernel_end);
    // Through `dispose`, with the lock let go: what the last program sent the
    // kernel and nobody read may carry handles.
    crate::object::dispose(last.map(Object::Channel));
    Some((Object::Channel(program_end), Rights::CHANNEL))
}

/// Give the first program -- pid 1 -- the audit record's handle, with `READ`
/// alone, after the hello (`docs/certification/AUDIT.md` §4): never
/// duplicated or sent, so it stays in pid 1's table and goes with it. Given
/// once; a later program of a command list gets none.
fn give_audit(kernel_end: &Arc<Endpoint>) {
    if AUDIT_GIVEN.swap(true, Ordering::AcqRel) {
        return;
    }
    let written = fallible::try_to_vec(&after_hello(AUDIT_MAGIC, 1))
        .and_then(|message| Ok((message, fallible::try_with_capacity(1)?)))
        .map_err(|_| ())
        .and_then(|(message, mut transfers): (Vec<u8>, Vec<Transfer>)| {
            let _ = fallible::push_within(&mut transfers, (Object::Audit, AUDIT_RIGHTS));
            kernel_end
                .write(message, 1, || Ok::<Vec<Transfer>, Infallible>(transfers))
                .map_err(|_| ())
        });
    if written.is_err() {
        println!("  init     the audit record's handle could not be written; nothing will read it");
        return;
    }
    crate::audit::record(
        crate::audit::READER_GIVEN,
        crate::audit::Outcome::Done,
        0,
        crate::audit::Subject::KERNEL,
        crate::audit::Target {
            kind: crate::audit::target::PROCESS,
            id: u64::from(crate::object::process::INIT_PID),
        },
        [0; 3],
    );
}

/// Whether pid 1 has been given the audit record's handle.
static AUDIT_GIVEN: AtomicBool = AtomicBool::new(false);

/// The rights pid 1's audit handle carries: `READ`, and neither `DUPLICATE`
/// nor `TRANSFER`, so it never leaves pid 1's table. The native boot check
/// makes its handle with these, so a widening here fails it.
pub(crate) const AUDIT_RIGHTS: Rights = Rights::READ;

/// Tell pid 1 where `/` is, on its bootstrap channel: the root volume, with
/// pid 1 moved onto it, or still the tmpfs (`ferrix.devmgr=init`, L12). Said
/// once, by `fs::root_disk`, once it knows.
pub(crate) fn notify_root(switched: bool) {
    crate::audit::record(
        crate::audit::ROOT_SWITCHED,
        crate::audit::Outcome::Done,
        0,
        crate::audit::Subject::KERNEL,
        crate::audit::Target::NONE,
        [u32::from(switched), 0, 0],
    );
    let value = if switched {
        ROOT_SWITCHED
    } else {
        ROOT_IN_MEMORY
    };
    let Some(kernel_end) = CHANNEL.lock().clone() else {
        println!("  init     pid 1 has no bootstrap channel to be told where / is on");
        return;
    };
    let written = fallible::try_to_vec(&after_hello(ROOT_MAGIC, value))
        .map_err(|_| ())
        .and_then(|message| {
            kernel_end
                .write(message, 0, || Ok::<Vec<Transfer>, Infallible>(Vec::new()))
                .map_err(|_| ())
        });
    if written.is_err() {
        println!("  init     pid 1 could not be told where / is");
    }
}

/// The command-line option naming the file pid 1 is started from.
const OPTION: &str = "ferrix.init";

/// The file started when nothing is named and nothing is built in.
const DEFAULT_INIT: &[u8] = b"/sbin/init";

/// What `ferrix.init=` named, read once, early.
static NAMED: Once<Vec<u8>> = Once::new();

/// Read `ferrix.init` from the loader's command line or, on a machine
/// described by a device tree, from `/chosen/bootargs`, as `power::init`
/// reads its option.
///
/// Early rather than when init starts, for `power::init`'s reason: a typo is
/// better reported at the start of a log than at the end of one. A path that
/// is not absolute is refused here, since there is no working directory yet
/// to resolve it from.
pub(crate) fn read_option(view: &BootView<'_>) {
    let tree = crate::discovery::fdt::open(view).ok();
    let value = view.option(OPTION).or_else(|| {
        tree.as_ref()
            .and_then(|tree| option_in(tree.bootargs()?, OPTION))
    });
    match value {
        None => {}
        Some(path) if path.starts_with('/') => {
            // FATAL-ALLOC: boot only: the command line is read once, as the kernel comes up.
            let _ = NAMED.call_once(|| Vec::from(path.as_bytes()));
            println!("  init     {OPTION}={path}: pid 1 is started from that file");
        }
        Some(other) => println!(
            "  init     {OPTION}={other} is not an absolute path; the built-in program is started"
        ),
    }
}

/// One entry of the boot initramfs beneath `.ferrix`, as the filesystem's
/// load side hands it over (`crate::fs::init`), which is where the archive
/// is read: by the item's dependency rules this file names no archive or
/// filesystem crate.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InputEntry {
    /// Its name relative to the root, without a leading `./` or `/`.
    pub(crate) name: &'static [u8],
    /// Whether it is a regular file.
    pub(crate) regular: bool,
    /// Whether it is a directory.
    pub(crate) directory: bool,
    /// Its link count, as the archive gives it.
    pub(crate) links: u32,
    /// Its bytes, in the archive the kernel keeps for the whole boot.
    pub(crate) data: &'static [u8],
}

/// What pid 1 is started with when nothing is named on the command line:
/// a program and a script for its `sh -c`, or a list of commands. Each is
/// empty when the image carries none, as an unset variable left it when
/// `cargo xtask` compiled them into the kernel.
#[derive(Clone, Copy, Debug, Default)]
struct Inputs {
    /// The program, `sh -i` or `sh -c` [`Inputs::script`].
    program: &'static [u8],
    /// A script for the program's `sh -c`, or nothing for an interactive one.
    script: &'static [u8],
    /// Commands to run in turn instead of the program; see
    /// [`run_commands`] for the encoding.
    commands: &'static [u8],
}

/// Pid 1's inputs, taken once from the boot initramfs by [`set_inputs`].
static INPUTS: Once<Inputs> = Once::new();

/// The directories the inputs are carried under, which are expected and say
/// nothing, and the three names an input may have.
const INPUT_DIRECTORIES: [&[u8]; 2] = [b".ferrix", b".ferrix/init"];
const PROGRAM_INPUT: &[u8] = b".ferrix/init/program";
const SCRIPT_INPUT: &[u8] = b".ferrix/init/script";
const COMMANDS_INPUT: &[u8] = b".ferrix/init/commands";

/// Take pid 1's inputs from `entries`, the boot initramfs's entries beneath
/// `.ferrix` in the archive's order. Called once, by `crate::fs::init`; a
/// second call changes nothing and answers `false`.
///
/// An entry that cannot be an input is refused with a line that names it and
/// says why, and the input it would have been is taken as absent: a second
/// entry of one name (both are refused), one that is not a regular file or
/// has more than one link, any other name, a script with a NUL in it, which
/// cannot survive being an argument, and a list of commands that does not
/// end its last command with an empty argument.
pub(crate) fn set_inputs(entries: impl Iterator<Item = InputEntry>) -> bool {
    if INPUTS.get().is_some() {
        return false;
    }
    let inputs = judge(entries, &mut |name, why| refuse(name, why));
    let mut first = false;
    let _ = INPUTS.call_once(|| {
        first = true;
        inputs
    });
    first
}

/// An input's name, what was found for it, and whether it was refused.
type Slot = (&'static [u8], Option<&'static [u8]>, bool);

/// A case of [`check`]: the entries, the program, script and commands they
/// should give, and how many of them should be refused.
type Case<'a> = (&'a [InputEntry], &'a [u8], &'a [u8], &'a [u8], u32);

/// [`set_inputs`]'s judgement, telling `refuse` of each entry refused and why.
fn judge(entries: impl Iterator<Item = InputEntry>, refuse: &mut dyn FnMut(&[u8], &str)) -> Inputs {
    // Each input's name, what was found for it, and whether it was refused.
    let mut slots: [Slot; 3] = [
        (PROGRAM_INPUT, None, false),
        (SCRIPT_INPUT, None, false),
        (COMMANDS_INPUT, None, false),
    ];
    for entry in entries {
        if INPUT_DIRECTORIES.contains(&entry.name) {
            if !entry.directory {
                refuse(entry.name, "is not a directory");
            }
            continue;
        }
        let Some(slot) = slots.iter_mut().find(|slot| slot.0 == entry.name) else {
            refuse(entry.name, "is not one of init's inputs");
            continue;
        };
        if slot.1.is_some() || slot.2 {
            refuse(entry.name, "is in the archive twice");
            slot.1 = None;
            slot.2 = true;
        } else if !entry.regular {
            refuse(entry.name, "is not a regular file");
            slot.2 = true;
        } else if entry.links > 1 {
            refuse(entry.name, "has more than one link");
            slot.2 = true;
        } else {
            slot.1 = Some(entry.data);
        }
    }
    let [program, script, commands] = slots.map(|(_, found, _)| found.unwrap_or_default());
    let script = if script.contains(&0) {
        refuse(
            SCRIPT_INPUT,
            "holds a NUL, which cannot survive being an argument",
        );
        b""
    } else {
        script
    };
    let commands = if !commands.is_empty() && !commands.ends_with(b"\0\0") {
        refuse(
            COMMANDS_INPUT,
            "does not end its last command with an empty argument",
        );
        b""
    } else {
        commands
    };
    Inputs {
        program,
        script,
        commands,
    }
}

/// What [`check`] found.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CheckReport {
    /// Sets of entries judged.
    pub(crate) cases: u32,
    /// Entries refused across them, each as the case expected.
    pub(crate) refusals: u32,
}

/// An entry for [`check`].
const fn made_up(
    name: &'static [u8],
    regular: bool,
    directory: bool,
    links: u32,
    data: &'static [u8],
) -> InputEntry {
    InputEntry {
        name,
        regular,
        directory,
        links,
        data,
    }
}

/// [`judge`] over `entries`, and how many it refused.
fn judged(entries: &[InputEntry]) -> (Inputs, u32) {
    let mut refused = 0_u32;
    let inputs = judge(entries.iter().copied(), &mut |_, _| {
        refused = refused.saturating_add(1);
    });
    (inputs, refused)
}

/// Judge sets of entries made up here, as an archive could carry them, and
/// see that each input is taken or refused as `set_inputs` says, then that a
/// second `set_inputs` changes nothing. Prints nothing of its own: a refusal
/// here is counted, not said.
///
/// Verifies: L.init.2, L.init.3
pub(crate) fn check() -> Result<CheckReport, &'static str> {
    const DIRS: [InputEntry; 2] = [
        made_up(b".ferrix", false, true, 2, b""),
        made_up(b".ferrix/init", false, true, 2, b""),
    ];
    let program = made_up(PROGRAM_INPUT, true, false, 1, b"P");
    let script = made_up(SCRIPT_INPUT, true, false, 1, b"S");
    let commands = made_up(COMMANDS_INPUT, true, false, 1, b"a\0\0");
    // (entries, program, script, commands, refusals) expected.
    let cases: [Case<'_>; 9] = [
        (
            &[DIRS[0], DIRS[1], program, script, commands],
            b"P",
            b"S",
            b"a\0\0",
            0,
        ),
        (&[], b"", b"", b"", 0),
        (
            &[program, made_up(PROGRAM_INPUT, true, false, 1, b"Q")],
            b"",
            b"",
            b"",
            1,
        ),
        (
            &[made_up(PROGRAM_INPUT, false, true, 2, b"")],
            b"",
            b"",
            b"",
            1,
        ),
        (
            &[made_up(PROGRAM_INPUT, true, false, 2, b"P")],
            b"",
            b"",
            b"",
            1,
        ),
        (
            &[
                made_up(b".ferrix/init/other", true, false, 1, b"x"),
                program,
            ],
            b"P",
            b"",
            b"",
            1,
        ),
        (
            &[made_up(b".ferrix/init", true, false, 1, b"x"), script],
            b"",
            b"S",
            b"",
            1,
        ),
        (
            &[made_up(SCRIPT_INPUT, true, false, 1, b"a\0b")],
            b"",
            b"",
            b"",
            1,
        ),
        (
            &[made_up(COMMANDS_INPUT, true, false, 1, b"a\0")],
            b"",
            b"",
            b"",
            1,
        ),
    ];
    let mut refusals = 0_u32;
    for (entries, program, script, commands, expected) in cases {
        let (inputs, refused) = judged(entries);
        if inputs.program != program || inputs.script != script || inputs.commands != commands {
            return Err("a set of entries gave other inputs than set_inputs says");
        }
        if refused != expected {
            return Err("a set of entries was refused other than set_inputs says");
        }
        refusals = refusals.saturating_add(refused);
    }
    let before = inputs();
    if set_inputs(core::iter::once(program)) {
        return Err("a second set_inputs was taken");
    }
    let after = inputs();
    if !core::ptr::eq(before.program, after.program)
        || !core::ptr::eq(before.script, after.script)
        || !core::ptr::eq(before.commands, after.commands)
    {
        return Err("a second set_inputs changed pid 1's inputs");
    }
    Ok(CheckReport {
        cases: 10,
        refusals,
    })
}

/// Say that the entry `name` is not taken as an input, and why.
fn refuse(name: &[u8], why: &str) {
    println!(
        "  init     /{} refused: {why}; taken as absent",
        Argv(&[name])
    );
}

/// Pid 1's inputs, or none before [`set_inputs`] ran.
fn inputs() -> Inputs {
    INPUTS.get().copied().unwrap_or_default()
}

/// What `/proc/self/exe` names for the built-in program, which has no file of
/// its own.
///
/// Absolute, because glibc's static start-up reads that link back and asserts
/// it is (`_dl_get_origin`): named after its first argument, `sh`, Ubuntu's
/// static busybox aborted with 134 before `main`. It is named where busybox
/// lives; `AT_EXECFN` keeps the name it was started by.
const BUILT_IN_EXE: &[u8] = b"/bin/busybox";

/// Where a command's program is looked for: `PATH`, which is one directory.
///
/// A command's `argv[0]` names its program, and the program is whatever that
/// name is in `/bin` -- a link to busybox, to uutils/coreutils, or to zinc.
/// Init resolves it the way the shell would, rather than knowing which binary
/// owns which name, so that moving a name from one to the other is a change
/// to the image and not to the kernel.
const PROGRAM_DIR: &[u8] = b"/bin/";

/// The environment every command starts with.
const ENVIRONMENT: &[&[u8]] = &[b"PATH=/bin", b"HOME=/", b"TERM=dumb"];

/// How many unanswered calls each command may report. Enough to name the
/// first few missing calls; few enough that a program retrying one forever
/// does not bury the rest of the log.
const UNANSWERED_LINES: u32 = 16;

/// The longest argument printed as itself. A script is longer, and a line of
/// the log is not the place to read it.
const SHOWN_BYTES: usize = 60;

/// Start the program `ferrix.init=` named, or the shell or the commands built
/// in, or `/sbin/init`, and report how each ended.
///
/// Returns when the last program exits, which on an interactive session is
/// when somebody types `exit`.
pub(crate) fn run() {
    let Some(&launcher) = LAUNCHER.get() else {
        println!("  init     nothing is registered to start a program with");
        return;
    };
    if let Some(path) = NAMED.get() {
        match run_file(launcher, path) {
            Ok(status) => {
                println!("  init     {} exited with {status}", Argv(&[path]));
                return;
            }
            Err(why) => println!(
                "  init     {OPTION}={} could not be started: {why}; falling back to the \
                 built-in program",
                Argv(&[path])
            ),
        }
    }
    let inputs = inputs();
    if !inputs.commands.is_empty() {
        run_commands(launcher, inputs.commands);
        return;
    }
    if inputs.program.is_empty() {
        run_default(launcher);
        return;
    }
    run_built_in(launcher, inputs);
}

/// Why a file could not be started as pid 1.
#[derive(Debug)]
enum Refusal {
    /// Opening it, or its `#!` interpreter, was refused.
    Open(Errno),
    /// It opened, and would not load or run.
    Start(Failure),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Open(errno) => write!(f, "errno {}", errno.0),
            Refusal::Start(failure) => write!(f, "{failure}"),
        }
    }
}

/// A refusal for want of memory.
fn no_memory(_: fallible::AllocError) -> Refusal {
    Refusal::Open(Errno::ENOMEM)
}

/// Start the file at `path` as pid 1, and wait for it to end.
///
/// A `#!` script runs under its interpreter with its own path as the last
/// argument, one level deep, as `execve` runs one; any other file runs with
/// its path as its only argument, as Linux starts `init=`.
fn run_file(launcher: &Launcher, path: &[u8]) -> Result<i32, Refusal> {
    let mut opened = (launcher.open)(path).map_err(Refusal::Open)?;
    let mut argv: Vec<Vec<u8>> = Vec::new();
    if opened.program.head().starts_with(b"#!") {
        let (interpreter, argument) =
            (launcher.interpreter_line)(opened.program.head()).map_err(Refusal::Open)?;
        opened = (launcher.open)(&interpreter).map_err(Refusal::Open)?;
        if opened.program.head().starts_with(b"#!") {
            return Err(Refusal::Open(Errno::ENOEXEC));
        }
        fallible::try_push(&mut argv, interpreter).map_err(no_memory)?;
        fallible::try_extend(&mut argv, argument).map_err(no_memory)?;
    }
    let own = fallible::try_to_vec(path).map_err(no_memory)?;
    fallible::try_push(&mut argv, own).map_err(no_memory)?;
    let args: Vec<&[u8]> =
        fallible::try_collect(argv.iter().map(Vec::as_slice)).map_err(no_memory)?;
    println!("  init     starting {}", Argv(&args));
    start(launcher, &opened, path, &args).map_err(Refusal::Start)
}

/// §8.1's default: `/sbin/init`, when nothing was named or built in. An image
/// without one is every image today, and says nothing.
fn run_default(launcher: &Launcher) {
    match run_file(launcher, DEFAULT_INIT) {
        Ok(status) => println!("  init     {} exited with {status}", Argv(&[DEFAULT_INIT])),
        Err(Refusal::Open(Errno::ENOENT)) => {}
        Err(why) => println!(
            "  init     {} could not be started: {why}",
            Argv(&[DEFAULT_INIT])
        ),
    }
}

/// Start the program built into the kernel: `sh -i`, or `sh -c` with the
/// built-in script.
fn run_built_in(launcher: &Launcher, inputs: Inputs) {
    let interactive: [&[u8]; 2] = [b"sh", b"-i"];
    let scripted: [&[u8]; 3] = [b"sh", b"-c", inputs.script];
    let (args, how): (&[&[u8]], _) = if inputs.script.is_empty() {
        (&interactive, "`sh -i`")
    } else {
        (&scripted, "`sh -c` with a built-in script")
    };
    println!(
        "  init     {} KiB program built in, starting {how}",
        inputs.program.len() / 1024
    );

    let name = args.first().copied().unwrap_or(b"");
    // A built-in program may be dynamically linked too, as a distribution's
    // shell is: its linker and libraries are not built in but read from the
    // initramfs, where `cargo xtask test-shell --interpreter` put them, and
    // the launcher reads them from there.
    let status = (launcher.start)(Start {
        image: Image::BuiltIn(inputs.program),
        exe: BUILT_IN_EXE,
        exec_fn: name,
        argv: args,
        // `ENV` is what an interactive POSIX shell reads before its first
        // prompt; xtask's initramfs puts the network setup there.
        env: &[
            b"PATH=/bin",
            b"HOME=/",
            b"TERM=dumb",
            b"PS1=ferrix# ",
            b"ENV=/etc/profile",
        ],
        bootstrap: next_bootstrap(),
    });
    match status {
        Ok(status) => println!("  init     the shell exited with {status}"),
        Err(problem) => println!("  init     the shell could not be started: {problem}"),
    }
}

/// Run each command in `list`, each with the program its `argv[0]` names in
/// [`PROGRAM_DIR`].
fn run_commands(launcher: &Launcher, list: &[u8]) {
    let Ok(commands) = parse(list) else {
        println!("  init     no memory to read the commands");
        return;
    };
    println!("  init     running {} commands", commands.len());
    // The programs the commands have needed so far, each with the path it was
    // asked for by and the name it resolved to, so that twenty commands over
    // three programs open three files: uutils/coreutils is one binary
    // answering to a hundred names. Since 2026-09-24 an open program is its
    // headers and its file, which the loader maps, so keeping one costs
    // nothing; before, each was the whole file read into memory.
    let mut loaded: Vec<(Vec<u8>, Opened)> = Vec::new();
    for (index, argv) in commands.iter().enumerate() {
        println!("  init     command {index}: {}", Argv(argv));
        syscall::report_unanswered(UNANSWERED_LINES);
        let Some(name) = argv.first() else {
            println!("  init     command {index} names no program");
            continue;
        };
        let Ok(mut path) = fallible::try_to_vec(PROGRAM_DIR) else {
            println!("  init     command {index}: no memory for its path");
            continue;
        };
        if fallible::try_extend_from_slice(&mut path, name).is_err() {
            println!("  init     command {index}: no memory for its path");
            continue;
        }

        let known = loaded.iter().position(|(seen, ..)| *seen == path);
        let at = match known {
            Some(at) => at,
            None => match (launcher.open)(&path) {
                Ok(opened) => {
                    if fallible::try_push(&mut loaded, (path, opened)).is_err() {
                        println!("  init     command {index}: no memory to keep its program");
                        continue;
                    }
                    loaded.len().saturating_sub(1)
                }
                Err(errno) => {
                    println!(
                        "  init     command {index} could not be read: errno {}",
                        errno.0
                    );
                    continue;
                }
            },
        };
        let Some((path, opened)) = loaded.get(at) else {
            continue;
        };
        match start(launcher, opened, path, argv) {
            Ok(status) => println!("  init     command {index} exited with {status}"),
            Err(problem) => {
                println!("  init     command {index} could not be started: {problem}");
            }
        }
    }
    syscall::report_unanswered(0);
    println!("  init     every command has run");
}

/// Start one program, and wait for it to end.
///
/// The one place that knows how a program is started, so that the loop above
/// does not change when that does.
///
/// `exe` is where the program was read from, resolved, which is what
/// `/proc/self/exe` must say: the name in `/bin` is a symbolic link, and
/// glibc's static startup asserts the link it reads back is absolute.
///
/// `path` is the name before it was resolved, which is what `AT_EXECFN` says:
/// a multicall binary reads it, or `argv[0]`, to know which of its programs
/// it has been asked for.
///
/// init comes out of the initramfs like any other program, so it may be
/// dynamically linked like any other program; the launcher reads the linker
/// it names from the same place. There is no process to fail back to here,
/// which is why this is the one caller that reports the failure itself.
fn start(
    launcher: &Launcher,
    opened: &Opened,
    path: &[u8],
    argv: &[&[u8]],
) -> Result<i32, Failure> {
    (launcher.start)(Start {
        image: Image::File(&opened.program),
        exe: &opened.exe,
        exec_fn: path,
        argv,
        env: ENVIRONMENT,
        bootstrap: next_bootstrap(),
    })
}

/// The commands in a list `src/kernel/build.rs` embedded: each argument ends in a
/// NUL, and each command in an empty argument.
///
/// A command left unterminated at the end is dropped rather than run with
/// arguments missing; the build script refuses such a list, so this is the
/// second line of defence rather than the first.
///
/// # Errors
///
/// [`fallible::AllocError`] when there is no memory for the list.
fn parse(list: &[u8]) -> Result<Vec<Vec<&[u8]>>, fallible::AllocError> {
    let mut commands = Vec::new();
    let mut current = Vec::new();
    for word in list.split(|byte| *byte == 0) {
        if !word.is_empty() {
            fallible::try_push(&mut current, word)?;
        } else if !current.is_empty() {
            fallible::try_push(&mut commands, core::mem::take(&mut current))?;
        }
    }
    Ok(commands)
}

/// An argument vector as one line of the log.
#[derive(Debug)]
struct Argv<'a>(&'a [&'a [u8]]);

impl fmt::Display for Argv<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (at, arg) in self.0.iter().enumerate() {
            if at > 0 {
                f.write_str(" ")?;
            }
            match core::str::from_utf8(arg) {
                Ok(text) if text.len() <= SHOWN_BYTES && !text.contains('\n') => {
                    f.write_str(text)?;
                }
                _ => write!(f, "<{} bytes>", arg.len())?,
            }
        }
        Ok(())
    }
}
