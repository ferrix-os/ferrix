//! The **cpu-number** step: on x86-64, `RDTSCP`'s `ECX` names the processor
//! `getcpu` names, as on Linux (`(node << 12) | cpu`) and Windows, whose
//! programs compare the two. Four threads each take 200,000 pairs of a
//! `getcpu` and an `RDTSCP`; a pair may straddle a migration, so up to 1 %
//! may differ (Linux 7.0 on 24 processors: 2 of 800,000). On Ferrix before
//! the kernel set `IA32_TSC_AUX`, `RDTSCP` said 0 on every processor and
//! three of the N150's four threads differed in nearly every pair. Elsewhere
//! the step has nothing to compare and says so.
#![allow(unsafe_code, reason = "the step's calls are libc's and RDTSCP")]

#[cfg(target_arch = "x86_64")]
use std::thread;

/// Pairs each thread takes.
#[cfg(target_arch = "x86_64")]
const PAIRS: u32 = 200_000;

/// The processor `getcpu` names.
#[cfg(target_arch = "x86_64")]
fn getcpu() -> u32 {
    let mut cpu = 0_u32;
    // SAFETY: getcpu writes one u32 through a live pointer.
    let _ = unsafe {
        libc::syscall(
            libc::SYS_getcpu,
            &raw mut cpu,
            core::ptr::null_mut::<u32>(),
            core::ptr::null_mut::<u8>(),
        )
    };
    cpu
}

/// Pairs that differed, and the processors `getcpu` named, as a bit set.
#[cfg(target_arch = "x86_64")]
fn sample() -> (u32, u64) {
    let mut differed = 0;
    let mut seen = 0_u64;
    for _ in 0..PAIRS {
        let cpu = getcpu();
        let mut aux = 0_u32;
        // SAFETY: RDTSCP reads the time stamp counter and TSC_AUX.
        let _ = unsafe { core::arch::x86_64::__rdtscp(&raw mut aux) };
        if aux & 0xfff != cpu {
            differed += 1;
        }
        seen |= 1_u64 << (cpu % 64);
    }
    (differed, seen)
}

/// The step; `fail` names what went wrong and ends the program.
pub(crate) fn check_cpu_number(fail: fn(&str) -> !) {
    #[cfg(target_arch = "x86_64")]
    {
        let rdtscp = core::arch::x86_64::__cpuid(0x8000_0001).edx & (1 << 27) != 0;
        if !rdtscp {
            println!("threads: cpu-number: no RDTSCP on this processor");
            return;
        }
        let workers: Vec<_> = (0..4).map(|_| thread::spawn(sample)).collect();
        let mut differed = 0_u32;
        let mut seen = 0_u64;
        for worker in workers {
            let Ok((d, s)) = worker.join() else {
                fail("a cpu-number thread panicked");
            };
            differed += d;
            seen |= s;
        }
        println!(
            "threads: cpu-number: {differed} of {} pairs differed, processors {seen:#x}",
            PAIRS * 4
        );
        if differed > PAIRS * 4 / 100 {
            fail("RDTSCP's processor number is not getcpu's");
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = fail;
        println!("threads: cpu-number: no RDTSCP on this architecture");
    }
}
