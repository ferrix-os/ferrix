//! M5's calls (`docs/microkernel/M5-LIFT.md` §2), answered under
//! `personality=user` alone: no other kernel has a linuxd to serve.
//! Two take a handle only the kernel's start of linuxd gives: a restricted
//! program's native number reaches this dispatch, and without the handle it
//! reaches neither the log nor the keyboard.

use super::{Errno, NativeCall, Process};

#[cfg(not(ferrix_personality = "user"))]
pub(super) fn call(_call: NativeCall, _process: &Process, _a: &[u64; 6]) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

#[cfg(ferrix_personality = "user")]
pub(super) use served::call;

#[cfg(ferrix_personality = "user")]
mod served {
    use ferrix_native_abi::handle::Handle;
    use ferrix_native_abi::rights::Rights;

    use super::{Errno, NativeCall, Process};
    use crate::object::Object;
    use crate::syscall::native::{copy_in, table_error};
    use crate::syscall::uaccess;
    use ferrix_native_abi::status;

    /// The most bytes one call moves: its time is bounded by this.
    const MOST: u64 = 4096;

    /// A line's text, as far as it is text: a holder's bytes go on the console.
    struct LogText<'a>(&'a [u8]);

    impl core::fmt::Display for LogText<'_> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            let text = self.0.strip_suffix(b"\n").unwrap_or(self.0);
            f.write_str(core::str::from_utf8(text).unwrap_or("(not text)"))
        }
    }

    fn holds(process: &Process, handle: u64, wanted: &Object, right: Rights) -> Result<(), Errno> {
        let handle = Handle(u32::try_from(handle).map_err(|_| status::BAD_HANDLE)?);
        process.with_handles(|table| {
            let (object, rights) = table.get(handle).map_err(table_error)?;
            if core::mem::discriminant(object) != core::mem::discriminant(wanted) {
                return Err(status::WRONG_TYPE);
            }
            if !rights.contains(right) {
                return Err(status::ACCESS_DENIED);
            }
            Ok(())
        })
    }

    /// The thread a `THREAD_AFFINITY` names, if its handle may manage it.
    fn managed_thread(
        object: &Object,
        rights: Rights,
    ) -> Result<alloc::sync::Arc<crate::object::bare::ThreadRef>, Errno> {
        let Object::Thread(thread) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(Rights::MANAGE) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(alloc::sync::Arc::clone(thread))
    }

    /// `THREAD_RESERVE`: the kernel's own authority, a job handle with `SET_LIMIT`, ahead of
    /// anything linuxd asks of a program (ledger 7088 E.2). Without a deadline tier the call
    /// has no body (docs/SCHED-CLASSES.md §9g).
    fn thread_reserve(process: &Process, a: &[u64; 6]) -> Result<usize, Errno> {
        use crate::sched::reserve::{self, Refusal, Reservation};
        if !crate::sched::DEADLINE_TIER {
            return Err(status::NOT_SUPPORTED);
        }
        let job = super::super::job_in(
            process,
            Handle(u32::try_from(a[0]).map_err(|_| status::BAD_HANDLE)?),
            Rights::SET_LIMIT,
        )?;
        let mut at = process.job();
        while !alloc::sync::Arc::ptr_eq(&at, &job) {
            let up = at.parent().cloned().ok_or(status::ACCESS_DENIED)?;
            at = up;
        }
        // On the job the handle names; the budget's words are in the kernel's own log of it.
        crate::audit::limit_set(
            crate::audit::LIMIT_SET,
            crate::audit::Subject::of(process),
            job.id(),
            ferrix_native_abi::nr::THREAD_RESERVE as u64,
            a[1],
        );
        let answer = if a[1] == 0 && a[2] == 0 && a[3] == 0 {
            reserve::give_back()
        } else {
            reserve::take(Reservation {
                runtime_ns: a[1],
                deadline_ns: a[2],
                period_ns: a[3],
            })
        };
        match answer {
            Ok(()) => Ok(0),
            Err(Refusal::Malformed) => Err(status::INVALID_ARGS),
            Err(Refusal::Unschedulable | Refusal::Full) => Err(status::LIMIT_REACHED),
            Err(Refusal::Already) => Err(status::BAD_STATE),
            Err(Refusal::Denied) => Err(status::ACCESS_DENIED),
        }
    }

    #[cfg_attr(
        ferrix_personality = "user",
        expect(
            clippy::too_many_lines,
            reason = "the lift's calls, one arm each, as the native table's dispatch"
        )
    )]
    pub(in super::super) fn call(
        call: NativeCall,
        process: &Process,
        a: &[u64; 6],
    ) -> Result<usize, Errno> {
        match call {
            NativeCall::DebugWrite => {
                holds(process, a[0], &Object::DebugLog, Rights::WRITE)?;
                if a[2] > MOST {
                    return Err(status::INVALID_ARGS);
                }
                if !process.debug_budget_takes(a[2]) {
                    return Err(status::SHOULD_WAIT);
                }
                let bytes = copy_in(process, a[1], a[2] as usize)?;
                // Who said a measured process's line is the kernel's to say: a QM
                // process's line can read as anything, and carries no such word.
                if process.own_space().is_measured() {
                    crate::console::println!("  [{}] {}", process.pid(), LogText(&bytes));
                } else {
                    crate::console::write_bytes(&bytes);
                }
                Ok(0)
            }
            NativeCall::ObjectKind => {
                use ferrix_native_abi::lift::{KIND_CHANNEL, KIND_OTHER, KIND_PORT, KIND_VMO};
                let handle = Handle(u32::try_from(a[0]).map_err(|_| status::BAD_HANDLE)?);
                process.with_handles(|table| {
                    let (object, rights) = table.get(handle).map_err(table_error)?;
                    let kind = match object {
                        Object::Vmo(_) => KIND_VMO,
                        Object::Port(_) => KIND_PORT,
                        Object::Channel(_) => KIND_CHANNEL,
                        _ => KIND_OTHER,
                    };
                    Ok(((kind << 32) | u64::from(rights.0)) as usize)
                })
            }
            NativeCall::ThreadAffinity => {
                let mut words = [0_u64; crate::sched::CPU_SET_WORDS];
                let wanted = a[2] as usize;
                if wanted > words.len() {
                    return Err(status::INVALID_ARGS);
                }
                let raw = copy_in(process, a[1], wanted * 8)?;
                for (word, bytes) in words.iter_mut().zip(raw.as_chunks::<8>().0) {
                    *word = u64::from_le_bytes(*bytes);
                }
                let task = if a[0] == 0 {
                    crate::sched::current().ok_or(status::BAD_STATE)?
                } else {
                    let handle = Handle(u32::try_from(a[0]).map_err(|_| status::BAD_HANDLE)?);
                    let thread = process.with_handles(|table| {
                        let (object, rights) = table.get(handle).map_err(table_error)?;
                        managed_thread(object, rights)
                    })?;
                    if thread.has_ended() || !thread.in_region() {
                        return Err(status::BAD_STATE);
                    }
                    thread.task().ok_or(status::BAD_STATE)?
                };
                let set = crate::sched::CpuSet::from_words(words);
                let _ = crate::sched::set_affinity(&task, &set).map_err(|why| match why {
                    crate::sched::AffinityRefusal::Reserved => status::ALREADY_BOUND,
                    _ => status::INVALID_ARGS,
                })?;
                Ok(0)
            }
            NativeCall::BootstrapMove => {
                let target = Handle(u32::try_from(a[0]).map_err(|_| status::BAD_HANDLE)?);
                let private = Handle(u32::try_from(a[1]).map_err(|_| status::BAD_HANDLE)?);
                let mine = process.region().ok_or(status::BAD_STATE)?;
                let to = process.with_handles(|table| {
                    let (object, rights) = table.get(target).map_err(table_error)?;
                    let Object::Process(to) = object else {
                        return Err(status::WRONG_TYPE);
                    };
                    // The creator's, not a mere look at it.
                    if !rights.contains(Rights::SUPERVISE) {
                        return Err(status::ACCESS_DENIED);
                    }
                    Ok(to.clone())
                })?;
                let to = to
                    .control()
                    .and_then(|control| control.host())
                    .ok_or(status::BAD_STATE)?;
                // Within its partition: never out of the region it was given in.
                if !to
                    .core()
                    .region()
                    .is_some_and(|theirs| alloc::sync::Arc::ptr_eq(theirs, mine))
                {
                    return Err(status::ACCESS_DENIED);
                }
                crate::syscall::native::give_bootstrap_from(process, to.core(), private, a[2] == 0)
            }
            NativeCall::SpaceWindow => {
                let space = crate::syscall::native::lift_space(process, a[0], Rights::MAP)?;
                space
                    .space()
                    .open_window()
                    .map(|()| 0)
                    .map_err(crate::syscall::native::lift_space_status)
            }
            NativeCall::SpacePlace => {
                use ferrix_native_abi::lift::{PLACE_EXECUTE, PLACE_MOST, PLACE_WRITE};
                let count = a[3] as usize;
                if count == 0 || count > PLACE_MOST {
                    return Err(status::INVALID_ARGS);
                }
                // Copied in before any lock (W5).
                let raw = copy_in(process, a[2], count * 16)?;
                let mut entries = [(0_u64, 0_u64, false, false); PLACE_MOST];
                for (slot, bytes) in entries.iter_mut().zip(raw.as_chunks::<16>().0) {
                    let (first, second) = bytes.split_at(8);
                    let first =
                        u64::from_le_bytes(first.try_into().map_err(|_| status::INVALID_ARGS)?);
                    let second =
                        u64::from_le_bytes(second.try_into().map_err(|_| status::INVALID_ARGS)?);
                    *slot = (
                        first & !0xfff,
                        second,
                        first & PLACE_WRITE != 0,
                        first & PLACE_EXECUTE != 0,
                    );
                }
                let entries = entries.get(..count).ok_or(status::INVALID_ARGS)?;
                let any_execute = entries.iter().any(|entry| entry.3);
                let on_space = if any_execute {
                    Rights::MAP | Rights::EXECUTE
                } else {
                    Rights::MAP
                };
                let space = crate::syscall::native::lift_space(process, a[0], on_space)?;
                let (vmo, held) =
                    crate::syscall::native::lift_vmo(process, a[1], Rights::MAP | Rights::READ)?;
                let (done, refusal) = space.charging_owner(|| {
                    space.space().place(
                        &vmo,
                        held.contains(Rights::WRITE),
                        held.contains(Rights::EXECUTE),
                        entries,
                    )
                });
                match refusal {
                    Some(why) if done == 0 => Err(crate::syscall::native::lift_space_status(why)),
                    _ => Ok(done),
                }
            }
            NativeCall::ThreadYield => {
                crate::sched::yield_now();
                Ok(0)
            }
            NativeCall::ClockRead => match a[0] {
                0 => Ok(crate::timer::now_nanos() as usize),
                2 => Ok(if crate::arch::vdso_can_read_counter() {
                    crate::timer::counter_hz() as usize
                } else {
                    0
                }),
                3 => Ok(crate::arch::user_hwcaps().0 as usize),
                4 => Ok(crate::arch::user_hwcaps().1 as usize),
                5 => crate::smp::this_cpu()
                    .map(|cpu| cpu.logical)
                    .ok_or(status::BAD_STATE),
                _ => Err(status::INVALID_ARGS),
            },
            NativeCall::ThreadReserve => thread_reserve(process, a),
            NativeCall::RandomFill => {
                if a[1] > MOST {
                    return Err(status::INVALID_ARGS);
                }
                let mut bytes = [0_u8; MOST as usize];
                let out = bytes.get_mut(..a[1] as usize).ok_or(status::INVALID_ARGS)?;
                crate::random::fill(out);
                uaccess::copy_to_user(process.caller_space(), a[0], out)
                    .map_err(|_| status::FAULT)?;
                Ok(0)
            }
            NativeCall::ConsoleRead => {
                holds(process, a[0], &Object::Console, Rights::READ)?;
                if a[2] > MOST {
                    return Err(status::INVALID_ARGS);
                }
                let mut bytes = [0_u8; MOST as usize];
                let mut got = 0;
                while got < a[2] as usize {
                    let Some(byte) = crate::arch::read_console_byte() else {
                        break;
                    };
                    let Some(slot) = bytes.get_mut(got) else {
                        break;
                    };
                    *slot = byte;
                    got += 1;
                }
                let read = bytes.get(..got).ok_or(status::INVALID_ARGS)?;
                uaccess::copy_to_user(process.caller_space(), a[1], read)
                    .map_err(|_| status::FAULT)?;
                Ok(got)
            }
            _ => Err(Errno::ENOSYS),
        }
    }
}
