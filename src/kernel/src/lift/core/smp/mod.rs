pub mod service;
use crate::arch;
use crate::sync::SpinLock;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::hint::spin_loop;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
use ferrix_bootinfo::BootView;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_sched::CpuSet;
use ferrix_sched::MAX_CPUS;
use ferrix_sync::IrqControl;
use ferrix_sync::Once;
#[repr(C)]
#[derive(Debug)]
pub struct PerCpu {
    pub logical: usize,
    pub hardware_id: u64,
    pub syscalls: OwnCount,
}
#[derive(Debug)]
pub struct OwnCount {}
impl OwnCount {
    pub fn read(&self) -> u64 {
        0
    }
}
pub const SYSCALLS_WORD_OFFSET: usize = 0;
impl PerCpu {
    pub fn in_service(&self) -> bool {
        true
    }
}
#[derive(Debug)]
pub struct Topology {}
impl Topology {
    pub fn count(&self) -> usize {
        count()
    }
    pub fn cpus(&self) -> &'static [PerCpu] {
        records()
    }
    pub fn online(&self) -> usize {
        count()
    }
}
static RECORDS: Once<Vec<PerCpu>> = Once::new();
/// The boot page's count: once, at linuxd's start.
pub fn set_processors(processors: usize) {
    let _ = RECORDS.call_once(|| {
        (0..processors.max(1))
            .map(|logical| PerCpu {
                logical,
                hardware_id: logical as u64,
                syscalls: OwnCount {},
            })
            .collect()
    });
}
fn records() -> &'static [PerCpu] {
    RECORDS.get().map_or(&[], Vec::as_slice)
}
static TOPOLOGY: Topology = Topology {};
pub fn topology() -> Option<&'static Topology> {
    Some(&TOPOLOGY)
}
pub fn count() -> usize {
    records().len().max(1)
}
/// The processor this thread was on a moment ago: the kernel keeps its number in
/// `IA32_TSC_AUX`, and a program reads it the same way.
pub fn this_cpu() -> Option<&'static PerCpu> {
    let mut aux = 0_u32;
    // SAFETY: reads the counter and the number; CR4.TSD is clear.
    let _ = unsafe { core::arch::x86_64::__rdtscp(&raw mut aux) };
    records().get(aux as usize & 0xfff)
}

/// A set of processors joined and left: nothing asks it in ring 3.
#[derive(Debug)]
pub struct CpuMask;
impl CpuMask {
    pub const fn new() -> CpuMask {
        CpuMask
    }
    pub fn join(&self, cpu: usize) {}
    pub fn leave(&self, cpu: usize) {}
    pub fn snapshot(&self) -> ferrix_sched::CpuSet {
        ferrix_sched::CpuSet::empty()
    }
}
pub fn add_cpus(into: &mut ferrix_sched::CpuSet, from: &ferrix_sched::CpuSet) {}
/// A shootdown: `mm::unmap_in` asked the kernel, which ran it.
#[derive(Debug)]
pub struct TlbPages {
    tables: crate::mm::UnlinkedTables,
}
impl TlbPages {
    pub const fn new() -> TlbPages {
        TlbPages {
            tables: crate::mm::UnlinkedTables::new(),
        }
    }
    pub fn tables(&mut self) -> &mut crate::mm::UnlinkedTables {
        &mut self.tables
    }
    pub fn is_empty(&self) -> bool {
        true
    }
    pub fn is_everything(&self) -> bool {
        false
    }
    pub fn addresses(&self) -> &[u64] {
        &[]
    }
    pub fn everything(&mut self) {}
    pub fn add(&mut self, address: u64) {}
    pub fn add_range(&mut self, start: u64, len: u64) {}
    pub fn add_all(&mut self, other: &mut TlbPages) {}
}
pub fn flush_tlb_pages(cpus: &ferrix_sched::CpuSet, pages: &mut TlbPages) {}
