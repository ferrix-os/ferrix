//! seccomp, proved at boot: the registered filter is asked about every call at
//! every entry, first, with the entry's own architecture token
//! (`docs/SECCOMP.md` §3.2, §3.3; SR1, SR2, SR3).
//!
//! The calls are driven through the core's own entries -- `SYSCALL`, `int
//! $0x80`, `svc` -- with a frame of the check's making (`arch::drive_system_call`),
//! not through `linux::handle`, which would skip exactly the hook under test. A
//! test-only rule stands in for a program's filter ([`seccomp::arm_probe`]); it
//! judges this task's calls and no other's.

use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use ferrix_linux_abi::nr::Syscall;
use ferrix_seccomp::SeccompData;

use crate::arch;
use crate::syscall::seccomp::{self, NATIVE_ARCH};
use crate::trap::{Abi, MAX_ERRNO, SyscallArgs, SyscallFilter, Verdict};

/// What the check saw, for the boot line.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Calls driven through an entry.
    pub(crate) calls: usize,
    /// Of them, calls an entry answers itself, before the dispatcher, that the
    /// filter answered instead.
    pub(crate) early: usize,
    /// Entries whose architecture token was checked against `e_machine`.
    pub(crate) tokens: usize,
    /// Native-range calls judged under the token of their own.
    pub(crate) native: usize,
    /// What the hook alone costs a thread with no filter, in tenths of a
    /// nanosecond a call.
    pub(crate) hook: u64,
    /// What the dispatcher costs a call no table has, in tenths of a
    /// nanosecond, for the hook's cost to be read against.
    pub(crate) dispatch: u64,
    /// What such a call costs through the whole entry, hook included.
    pub(crate) entry: u64,
    /// Errno ranges the core was shown a filter answer, each bounded as it
    /// should be.
    pub(crate) clamped: usize,
    /// What one interpreted instruction of a filter costs, in tenths of a
    /// nanosecond, from the longest program the verifier admits: the figure
    /// the bound of a call's filtering (32,768 steps) is read with.
    pub(crate) step: u64,
}

/// `AUDIT_ARCH_LE`.
const LITTLE_ENDIAN: u32 = 0x4000_0000;
/// `__AUDIT_ARCH_64BIT`.
const WIDE: u32 = 0x8000_0000;

/// The value a rule answers with, or this when it lets the call go on.
const CONTINUE: i64 = i64::MIN;

/// What the rule answers with.
static ANSWER: AtomicI64 = AtomicI64::new(CONTINUE);
/// The token the rule is about, or zero for every call.
static ABOUT: AtomicU32 = AtomicU32::new(0);
/// Whether the rule is about every token but [`ABOUT`].
static EXCEPT: AtomicBool = AtomicBool::new(false);
/// How many calls the rule has judged.
static JUDGED: AtomicU32 = AtomicU32::new(0);
/// The number, token, instruction pointer and first argument of the last.
static SAW_NR: AtomicI64 = AtomicI64::new(0);
/// See [`SAW_NR`].
static SAW_ARCH: AtomicU32 = AtomicU32::new(0);
/// See [`SAW_NR`].
static SAW_IP: AtomicU64 = AtomicU64::new(0);
/// See [`SAW_NR`].
static SAW_ARG0: AtomicU64 = AtomicU64::new(0);

/// The test-only filter: remember what it was shown, and answer if the call is
/// one it is about.
fn rule(data: &SeccompData) -> Verdict {
    let _ = JUDGED.fetch_add(1, Ordering::Relaxed);
    SAW_NR.store(i64::from(data.nr), Ordering::Relaxed);
    SAW_ARCH.store(data.arch, Ordering::Relaxed);
    SAW_IP.store(data.instruction_pointer, Ordering::Relaxed);
    SAW_ARG0.store(data.args[0], Ordering::Relaxed);
    let about = ABOUT.load(Ordering::Relaxed);
    let matches = about == 0 || (data.arch == about) != EXCEPT.load(Ordering::Relaxed);
    let answer = ANSWER.load(Ordering::Relaxed);
    if matches && answer != CONTINUE {
        return Verdict::Errno(answer as u32);
    }
    Verdict::Continue
}

/// What one driven call came to.
#[derive(Debug, Clone, Copy)]
struct Shot {
    /// The return register after the entry.
    result: isize,
    /// How many times the filter was asked.
    asked: u32,
    /// What it was shown last.
    nr: i64,
    /// See `nr`.
    arch: u32,
    /// See `nr`.
    ip: u64,
    /// See `nr`.
    arg0: u64,
}

/// The rule's terms for one call.
#[derive(Debug, Clone, Copy)]
struct Terms {
    /// The errno the rule fails the call with, or `None` to let it go on.
    answer: Option<u32>,
    /// The token the rule is about, or zero.
    about: u32,
    /// Whether it is about every other token.
    except: bool,
}

impl Terms {
    /// Answer `value` to every call.
    const fn always(errno: u32) -> Terms {
        Terms {
            answer: Some(errno),
            about: 0,
            except: false,
        }
    }

    /// Answer nothing: watch.
    const fn watch() -> Terms {
        Terms {
            answer: None,
            about: 0,
            except: false,
        }
    }
}

/// Drive one call through its entry, judged by `terms`. `None` when this
/// architecture has no such entry.
fn shoot(abi: Abi, number: usize, args: [u64; 6], ip: u64, terms: Terms) -> Option<Shot> {
    ANSWER.store(terms.answer.map_or(CONTINUE, i64::from), Ordering::Relaxed);
    ABOUT.store(terms.about, Ordering::Relaxed);
    EXCEPT.store(terms.except, Ordering::Relaxed);
    JUDGED.store(0, Ordering::Relaxed);
    let result = arch::drive_system_call(abi, number, args, ip)?;
    Some(Shot {
        result,
        asked: JUDGED.load(Ordering::Relaxed),
        nr: SAW_NR.load(Ordering::Relaxed),
        arch: SAW_ARCH.load(Ordering::Relaxed),
        ip: SAW_IP.load(Ordering::Relaxed),
        arg0: SAW_ARG0.load(Ordering::Relaxed),
    })
}

/// The number `call` has in `abi`'s table on this architecture, found by asking
/// the decoder: the tables run one way only. The Arm private range starts at
/// `0xf0000`.
fn number_of(abi: Abi, call: Syscall) -> Option<usize> {
    let decode = |number| match abi {
        Abi::Native => arch::decode_syscall(number),
        Abi::Compat => arch::decode_compat_syscall(number),
    };
    (0..=1024)
        .chain(0xf_0000..0xf_0020)
        .find(|&number| decode(number) == Some(call))
}

/// The token an entry's calls carry, worked out from the ELF machine number of
/// the code it runs rather than asked of the kernel's own table of them.
fn token_for(abi: Abi) -> u32 {
    /// `EM_386`.
    const EM_386: u32 = 3;
    match abi {
        Abi::Native => {
            let wide = if size_of::<usize>() == 8 { WIDE } else { 0 };
            u32::from(arch::ARCH.elf_machine()) | LITTLE_ENDIAN | wide
        }
        Abi::Compat => EM_386 | LITTLE_ENDIAN,
    }
}

/// A call an entry answers itself, before the dispatcher, and what it says when
/// the filter does not get there first.
const EARLY: [(Abi, Syscall, &str); 6] = [
    (
        Abi::Native,
        Syscall::ArchPrctl,
        "arch_prctl was answered before the filter",
    ),
    (
        Abi::Native,
        Syscall::ArmSetTls,
        "set_tls was answered before the filter",
    ),
    (
        Abi::Native,
        Syscall::Sigreturn,
        "sigreturn was answered before the filter",
    ),
    (
        Abi::Native,
        Syscall::RtSigreturn,
        "rt_sigreturn was answered before the filter",
    ),
    (
        Abi::Compat,
        Syscall::Sigreturn,
        "i386 sigreturn was answered before the filter",
    ),
    (
        Abi::Compat,
        Syscall::RtSigreturn,
        "i386 rt_sigreturn was answered before the filter",
    ),
];

/// A distinctive instruction pointer for a driven call: what `seccomp_data`
/// must show unchanged.
const IP: u64 = 0x0040_1234;

/// The errno the rule fails a call with when it answers: `EPERM`, which is
/// not what any call these checks drive returns.
const REFUSED_ERRNO: u32 = 1;
/// What a call the rule refused returns.
const REFUSED: isize = -1;

/// Drive every entry and every call an entry keeps for itself, and require the
/// filter to have judged each first, once, with the entry's own token.
///
/// Verifies: H.TRAP.16, L.trap.7, `L.x86_64.124`, `L.x86_64.125`, L.aarch64.51, `L.armv7a.1`, `L.armv7a.2`
///
/// # Errors
///
/// A call no filter saw, or seen twice, or seen under another entry's token
/// or number or instruction pointer; an early answer given before the filter's;
/// a filter's answer that did not come back; the native range judged as a Linux
/// number or not judged at all.
pub(crate) fn run() -> Result<Report, &'static str> {
    crate::println!("NEGATIVE CONTROL k10-strict applied: strict mode allows every call");
    seccomp::arm_probe(rule);
    let checked = all();
    seccomp::disarm_probe();
    let mut report = checked?;
    measure(&mut report);
    Ok(report)
}

/// Calls each measurement makes.
const ROUNDS: u64 = 200_000;

/// What the hook costs a call of a thread that has no filter -- every call of
/// every program -- against what the dispatcher and the whole entry cost a
/// call no table has. The probe is disarmed, so the hook is what a program
/// pays: one `Once` load, an indirect call, the running task and its thread's
/// flag. Measured in the guest, to be read and not judged: a virtual machine's
/// clock is not a bound.
fn measure(report: &mut Report) {
    use core::hint::black_box;

    let args = SyscallArgs {
        abi: Abi::Native,
        number: 0x7777,
        args: [0; 6],
        ip: IP,
    };
    let per_call = |each: &dyn Fn()| {
        let start = crate::timer::now_nanos();
        for _ in 0..ROUNDS {
            each();
        }
        crate::timer::now_nanos().saturating_sub(start) * 10 / ROUNDS
    };
    report.hook = per_call(&|| {
        let _ = black_box(crate::trap::filter_system_call(black_box(&args)));
    });
    report.dispatch = per_call(&|| {
        let _ = black_box(crate::trap::system_call(black_box(&args), None));
    });
    report.entry = per_call(&|| {
        let _ = black_box(arch::drive_system_call(Abi::Native, 0x7777, [0; 6], IP));
    });
    report.step = step_cost();
}

/// What one interpreted instruction costs: the longest program the verifier
/// admits, 4,095 loads of `seccomp_data`'s first word and an `ALLOW`, run
/// against a call's data. Measured in the guest, to be read and not judged.
fn step_cost() -> u64 {
    use core::hint::black_box;

    /// `BPF_LD | BPF_W | BPF_ABS`.
    const LOAD: u16 = 0x20;
    /// `BPF_RET | BPF_K`.
    const RETURN: u16 = 0x06;
    /// Programs run.
    const RUNS: u64 = 500;

    let mut insns =
        alloc::vec![ferrix_seccomp::Insn::new(LOAD, 0, 0, 0); ferrix_seccomp::MAX_INSNS - 1];
    insns.push(ferrix_seccomp::Insn::new(
        RETURN,
        0,
        0,
        ferrix_seccomp::ALLOW,
    ));
    let Ok(program) = ferrix_seccomp::verify(&insns) else {
        return 0;
    };
    let data = SeccompData {
        nr: 1,
        arch: 0,
        instruction_pointer: 0,
        args: [0; 6],
    };
    let steps = program.len() as u64 * RUNS;
    let start = crate::timer::now_nanos();
    for _ in 0..RUNS {
        let _ = black_box(ferrix_seccomp::run(black_box(&program), black_box(&data)));
    }
    crate::timer::now_nanos().saturating_sub(start) * 10 / steps
}

/// [`run`], with the probe armed.
fn all() -> Result<Report, &'static str> {
    let mut report = Report::default();
    early_answers(&mut report)?;
    tokens(&mut report)?;
    numbers(&mut report)?;
    native_range(&mut report)?;
    errnos(&mut report)?;
    registration()?;
    rollback()?;
    Ok(report)
}

/// The core, not the filter, bounds what a filter can make a call return: an
/// errno up to Linux's 4095 comes back as it is, one above it is cut to 4095
/// (`ERRNO | 5000` answers `-4095`, as Linux's does), and 512, which the
/// dispatcher keeps for restarting a call, is an ordinary errno here.
fn errnos(report: &mut Report) -> Result<(), &'static str> {
    // A number no table has, so that nothing but the filter answers it.
    let unknown = 0x7777;
    for (errno, expected) in [
        (0, 0),
        (13, -13),
        (512, -512),
        (MAX_ERRNO, -(MAX_ERRNO as isize)),
        (MAX_ERRNO + 1, -(MAX_ERRNO as isize)),
        (5000, -(MAX_ERRNO as isize)),
        (u32::MAX, -(MAX_ERRNO as isize)),
    ] {
        let Some(shot) = shoot(Abi::Native, unknown, [0; 6], IP, Terms::always(errno)) else {
            return Ok(());
        };
        report.calls += 1;
        if shot.result != expected {
            return Err("the core let a filter's errno out of 0 to 4095 through");
        }
    }
    report.clamped += 1;
    Ok(())
}

/// A filter slot with nothing registered lets every call go on, and a second
/// registration does not replace the first (`trap::ask`, `trap::register`):
/// checked on a slot of the check's own, because the core's is registered
/// before anything can run.
fn registration() -> Result<(), &'static str> {
    use ferrix_sync::Once;

    fn first(_: &SyscallArgs) -> Verdict {
        Verdict::Errno(11)
    }
    fn second(_: &SyscallArgs) -> Verdict {
        Verdict::Errno(22)
    }
    let args = SyscallArgs {
        abi: Abi::Native,
        number: 0x7777,
        args: [0; 6],
        ip: IP,
    };
    let slot: Once<SyscallFilter> = Once::new();
    if crate::trap::ask(&slot, &args).is_some() {
        return Err("a call was answered with no filter registered");
    }
    crate::trap::register(&slot, first);
    crate::trap::register(&slot, second);
    match crate::trap::ask(&slot, &args) {
        Some(crate::trap::Outcome::Return(-11)) => Ok(()),
        _ => Err("a later registration replaced the first, or the first was not asked"),
    }
}

/// The value a trapped call's return register is given so that the frame reads
/// as it did at the call: x86's `RAX` held the number, and on the Arm pair the
/// number stays in `r7`/`x8` and the first argument in `r0`/`x0`
/// (`docs/SECCOMP.md` §3.6). Nothing uses it before the landing that delivers
/// `SIGSYS`, so this is its only reader until then.
fn rollback() -> Result<(), &'static str> {
    /// `EM_X86_64`.
    const EM_X86_64: u16 = 62;
    let args = [0x1111, 2, 3, 4, 5, 6];
    for abi in [Abi::Native, Abi::Compat] {
        let value = arch::syscall_rollback_value(abi, 0x2222, &args);
        let expected = if arch::ARCH.elf_machine() == EM_X86_64 {
            0x2222
        } else {
            0x1111
        };
        if value != expected {
            return Err("a rolled-back frame would not read as it did at the call");
        }
    }
    Ok(())
}

/// SR3: every call an entry answers itself reaches the filter first, and the
/// filter's answer is the one the program gets.
fn early_answers(report: &mut Report) -> Result<(), &'static str> {
    for (abi, call, problem) in EARLY {
        let Some(number) = number_of(abi, call) else {
            continue;
        };
        // The arguments are ones the early answer would act on, were it reached:
        // `arch_prctl(ARCH_SET_FS, 0x7777)`, and a thread pointer for `set_tls`.
        let args = [0x1002, 0x7777, 0, 0, 0, 0];
        let Some(shot) = shoot(abi, number, args, IP, Terms::always(REFUSED_ERRNO)) else {
            continue;
        };
        report.calls += 1;
        if shot.asked != 1 || shot.result != REFUSED {
            return Err(problem);
        }
        if shot.nr != number as i64 || shot.ip != IP || shot.arg0 != args[0] {
            return Err("the filter was shown another call than the program made");
        }
        report.early += 1;
    }
    Ok(())
}

/// SR1: the token is the entry's, whatever the image: a 64-bit program's `int
/// $0x80` is an i386 call with i386's numbers, and a filter written for x86-64
/// alone does not see it as one.
fn tokens(report: &mut Report) -> Result<(), &'static str> {
    // A number no table has, so that nothing answers it but the dispatcher.
    let unknown = 0x7777;
    for abi in [Abi::Native, Abi::Compat] {
        let Some(shot) = shoot(abi, unknown, [1, 2, 3, 4, 5, 6], IP, Terms::watch()) else {
            continue;
        };
        report.calls += 1;
        if shot.asked != 1 {
            return Err("a call reached the filter other than once");
        }
        if shot.arch != token_for(abi) {
            return Err(match abi {
                Abi::Native => "a native call was filtered under another token than its entry's",
                Abi::Compat => "an int 0x80 call was filtered as x86-64",
            });
        }
        if shot.nr != unknown as i64 || shot.ip != IP || shot.arg0 != 1 {
            return Err("the filter was shown another call than the program made");
        }
        if shot.result != -38 {
            return Err("a call the filter let go on was not answered by the dispatcher");
        }
        report.tokens += 1;
    }
    // A filter for one architecture, about the other entry's call: the same
    // number, answered only when the token is x86-64's.
    let x86_64 = u32::from(arch::ARCH.elf_machine()) | LITTLE_ENDIAN | WIDE;
    let for_x86_64 = Terms {
        answer: Some(77),
        about: x86_64,
        except: false,
    };
    if let Some(shot) = shoot(Abi::Compat, unknown, [0; 6], IP, for_x86_64) {
        report.calls += 1;
        if shot.result == -77 {
            return Err("an int 0x80 call was filtered as x86-64");
        }
    }
    Ok(())
}

/// SR2: the number a filter judges is the number dispatched. The filter sees
/// the low 32 bits; the dispatcher uses the register, so a number with upper
/// bits set is in no table, and is refused whatever the filter let through.
fn numbers(report: &mut Report) -> Result<(), &'static str> {
    // A 32-bit register has no bit above the 32nd.
    let Ok(high) = usize::try_from(1_u64 << 32) else {
        return Ok(());
    };
    let Some(getpid) = number_of(Abi::Native, Syscall::Getpid) else {
        return Err("the decoder has no getpid");
    };
    let wide = getpid | high;
    let Some(shot) = shoot(Abi::Native, wide, [0; 6], IP, Terms::watch()) else {
        return Ok(());
    };
    report.calls += 1;
    if shot.nr != getpid as i64 {
        return Err("the filter was not shown the low 32 bits of the number");
    }
    if shot.result != -38 {
        return Err("a number with bits above the 32nd set was dispatched as a call");
    }
    Ok(())
}

/// Q2: a native call carries an `arch` of its own, which a filter that reads
/// `arch` first refuses as it refuses any foreign one, and which a filter that
/// wants these calls can allow by name.
fn native_range(report: &mut Report) -> Result<(), &'static str> {
    let Some(getpid) = number_of(Abi::Native, Syscall::Getpid) else {
        return Err("the decoder has no getpid");
    };
    // A number in the native range that no native call has.
    let gap = ferrix_native_abi::nr::LAST;
    if ferrix_native_abi::nr::decode(gap).is_some() {
        return Err("the last number of the native range is a call");
    }
    let linux = token_for(Abi::Native);

    // A filter that refuses every `arch` but Linux's own, as Chromium's does.
    let linux_only = Terms {
        answer: Some(REFUSED_ERRNO),
        about: linux,
        except: true,
    };
    let Some(shot) = shoot(Abi::Native, gap, [0; 6], IP, linux_only) else {
        return Ok(());
    };
    report.calls += 1;
    if shot.arch != NATIVE_ARCH || shot.nr != gap as i64 {
        return Err("a native call was not filtered under the native token");
    }
    if shot.arch == linux {
        return Err("a native call was filtered as a Linux call");
    }
    if shot.result != REFUSED {
        return Err("a filter that refuses every foreign arch let a native call go on");
    }

    // One that refuses every `arch` but the native one: the native call goes on
    // to its dispatcher, and a Linux call is refused.
    let native_only = Terms {
        answer: Some(REFUSED_ERRNO),
        about: NATIVE_ARCH,
        except: true,
    };
    let Some(native) = shoot(Abi::Native, gap, [0; 6], IP, native_only) else {
        return Ok(());
    };
    let Some(other) = shoot(Abi::Native, getpid, [0; 6], IP, native_only) else {
        return Ok(());
    };
    report.calls += 2;
    if native.result != -38 {
        return Err("a filter that allows the native arch by name refused a native call");
    }
    if other.result != REFUSED {
        return Err("a filter that allows only the native arch let a Linux call go on");
    }
    report.native += 1;
    Ok(())
}
