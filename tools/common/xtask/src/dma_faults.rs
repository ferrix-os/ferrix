//! Every DMA fault QEMU's VT-d unit records over a run, held to the ones the
//! kernel says it provoked.
//!
//! # The line this is about
//!
//! Every x86-64 boot used to end its stage 10 with this on QEMU's stderr:
//!
//! ```text
//! qemu-system-x86_64: vtd_iova_to_slpte: detected slpte permission error (iova=0x1000, level=0x2, slpte=0x0, write=1, pasid=0xffffffff)
//! qemu-system-x86_64: Interrupt Mask set, irq is not generated
//! qemu-system-x86_64: vtd_iommu_translate: detected translation failure (dev=00:02:00, iova=0x1000)
//! qemu-system-x86_64: New fault is not recorded due to compression of faults
//! ```
//!
//! It is the kernel's doing, and on purpose. Stage 10's exit criterion is a
//! device's write outside its domain, faulted by its unit: `pci/virtio.rs`'s
//! out-of-domain probe gives the virtio-rng function, `00:02.0`, stream
//! `0x10`, a descriptor for page `0x1000`, which its domain does not map, and
//! requires the unit's fault record for it. QEMU's device then writes the 64
//! bytes it was asked for four at a time, and each write faults: sixteen
//! faults, and sometimes a seventeenth for the write-back QEMU drops, all for
//! that page and that stream, in the same instant, and none anywhere else in
//! a boot. `level` is 3 when the domain maps nothing in the first GiB and 2
//! when it maps something there but not in the first 2 MiB: the walk stops at
//! the first empty level, and which that is depends on where the frames the
//! domain does map happen to lie.
//!
//! None of the four can be avoided while the probe is made. QEMU's
//! `vtd_iommu_translate` reports every translation it refuses, whatever the
//! context entry's fault-processing-disable bit says, and the other three
//! follow from the kernel leaving the fault event masked and reading the
//! record itself. And each is `error_report_once`: printed for the first fault
//! of a run and never again. So the line said nothing about the rest of the
//! run: a driver that later gave its device an address it never pinned would
//! have been faulted in silence.
//!
//! # What is done instead
//!
//! A run this tool judges asks QEMU to trace `vtd_dmar_fault`, which fires
//! for every fault, and [`crate::noise`] takes those lines and the four
//! remarks out of stderr. The kernel says, before the probe's doorbell, which
//! stream it is making write which page. When the run is over [`problem`]
//! requires every traced fault to be one of those; the remarks are then
//! redundant and counted rather than shown. A fault that is not the kernel's
//! fails the run and prints the remarks too. The kernel holds its own boot to
//! the same rule by reading the unit's records (`iommu::audit_faults`); this
//! covers the rest of the run as well, a shell's and its drivers'.
//!
//! A QEMU built without the `log` trace backend traces nothing. The remarks
//! are then all there is, and they are printed and not judged.

use std::path::Path;

use crate::paths::Arch;
use crate::{Error, Result};

/// The trace event QEMU's VT-d unit emits for every fault it reports.
const EVENT: &str = "vtd_dmar_fault";

/// The trace event QEMU's AMD-Vi unit emits for every DMA page fault: it
/// carries the address alone, no requester and no access, so an AMD-Vi
/// row's faults are counted and held to the pages the kernel provoked, not
/// attributed to a stream (ledger 3867, condition 6).
const AMDVI_EVENT: &str = "amdvi_page_fault";

/// The source a traced AMD-Vi fault is given: none it names.
const UNATTRIBUTED: u32 = u32::MAX;

/// The arguments that make QEMU trace every VT-d and AMD-Vi fault, for an
/// `arch` whose machine this tool gives either unit.
pub(crate) fn trace_args(arch: Arch) -> &'static [&'static str] {
    if arch == Arch::X86_64 {
        &["-trace", EVENT, "-trace", AMDVI_EVENT]
    } else {
        &[]
    }
}

/// What the kernel prints, before the write, for each access it makes fault on
/// purpose: `iommu    stream 0x10 is made to write page 0x1000 outside its
/// domain, on purpose`.
const PROVOKED: &str = " outside its domain, on purpose";

/// The remarks QEMU's VT-d unit makes about a fault, each once per run. The
/// first two carry the faulting address; the last two are what a probe that
/// masks the fault event and faults several times in a row is told.
///
/// The last two are interrupt remapping's: the patched QEMU's report of a
/// compatibility-format message it blocked (checks R1 and R2) and its
/// report of a message whose source ID failed its entry's check (R3). The
/// kernel judges those faults itself, by their reasons, and test-boot holds
/// them to the three the checks provoke (`qemu::remap_problem`); here they
/// are remarks like the rest.
const REMARKS: [&str; 7] = [
    "vtd_iova_to_slpte: detected",
    "vtd_iommu_translate: detected translation failure",
    "Interrupt Mask set, irq is not generated",
    "New fault is not recorded due to compression of faults",
    "vtd_interrupt_remap_msi: compatibility format interrupt blocked",
    "vtd_irte_get: invalid IRTE SID",
    // QEMU's AMD-Vi unit on an interrupt request it refused (checks R1 to
    // R3 on the AMD-Vi row), which the kernel reads as an event.
    "failed to remap interrupt from devid",
];

/// One fault QEMU traced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fault {
    /// The source ID: bus, device and function.
    pub(crate) source: u32,
    /// The address the device used.
    pub(crate) address: u64,
    /// Whether it was a write.
    pub(crate) write: bool,
}

/// The fault a trace line reports, if it is one:
/// `1610532@1790384790.551512:vtd_dmar_fault sid 0x10 fault 5 addr 0x1000 write 1`.
pub(crate) fn traced(line: &str) -> Option<Fault> {
    if let Some((_, rest)) = line.split_once(AMDVI_EVENT) {
        // `amdvi_page_fault error: page fault accessing guest physical
        // address 0x1000`: the address alone.
        let address = rest.split_whitespace().last()?;
        return Some(Fault {
            source: UNATTRIBUTED,
            address: hex(address)?,
            write: true,
        });
    }
    let (_, rest) = line.split_once(EVENT)?;
    let mut words = rest.split_whitespace();
    let mut source = None;
    let mut address = None;
    let mut write = None;
    while let Some(word) = words.next() {
        let value = words.next()?;
        match word {
            "sid" => source = Some(u32::try_from(hex(value)?).ok()?),
            "addr" => address = Some(hex(value)?),
            "write" => write = Some(value != "0"),
            _ => {}
        }
    }
    Some(Fault {
        source: source?,
        address: address?,
        write: write?,
    })
}

/// Whether `line` is one of QEMU's VT-d remarks about a fault.
pub(crate) fn remark(line: &str) -> bool {
    REMARKS.iter().any(|remark| line.contains(remark))
}

/// A number written `0x...`.
fn hex(text: &str) -> Option<u64> {
    u64::from_str_radix(text.trim_start_matches("0x"), 16).ok()
}

/// The stream and page each access the kernel said it provoked.
fn provoked(lines: &[String]) -> Vec<(u32, u64)> {
    lines
        .iter()
        .filter(|line| line.contains(PROVOKED))
        .filter_map(|line| {
            let stream = line.split("stream ").nth(1)?.split_whitespace().next()?;
            let page = line.split("page ").nth(1)?.split_whitespace().next()?;
            Some((u32::try_from(hex(stream)?).ok()?, hex(page)?))
        })
        .collect()
}

/// What the sieve kept of QEMU's stderr about DMA faults.
#[derive(Debug, Default)]
pub(crate) struct Seen {
    /// Every fault traced.
    pub(crate) faults: Vec<Fault>,
    /// The remarks, as QEMU wrote them.
    pub(crate) remarks: Vec<String>,
}

/// Why a run's DMA faults are not all ones the kernel provoked, if they are
/// not: the first few that are not, by source and address.
pub(crate) fn problem(seen: &Seen, lines: &[String]) -> Option<String> {
    let provoked = provoked(lines);
    let stray: Vec<&Fault> = seen
        .faults
        .iter()
        .filter(|fault| {
            !(fault.write
                && provoked.iter().any(|&(stream, page)| {
                    (fault.source == stream || fault.source == UNATTRIBUTED)
                        && fault.address & !0xFFF == page
                }))
        })
        .collect();
    let first = stray.first()?;
    Some(format!(
        "QEMU's VT-d unit faulted {} DMA accesses the kernel did not say it provoked, the first \
         from {} to {:#x}, a {} (provoked: {})",
        stray.len(),
        source_name(first.source),
        first.address,
        if first.write { "write" } else { "read" },
        if provoked.is_empty() {
            "none".to_owned()
        } else {
            provoked
                .iter()
                .map(|(stream, page)| format!("stream {stream:#x} page {page:#x}"))
                .collect::<Vec<_>>()
                .join(", ")
        },
    ))
}

/// A source ID as `bus:device.function`, and the number itself.
fn source_name(source: u32) -> String {
    format!(
        "{:02x}:{:02x}.{} (source {source:#x})",
        source >> 8,
        (source >> 3) & 0x1F,
        source & 7
    )
}

impl Seen {
    /// Judge a finished run by its DMA faults.
    ///
    /// Only a run that got where it was going: one that panicked or stalled
    /// has a better reason to give and gives it, and QEMU's remarks are shown
    /// beside it, with any fault the kernel did not provoke. That fault may be
    /// the reason: a device that read its own ring through a translation gone
    /// wrong fails as a driver would, and the remarks alone say only that
    /// something faulted, once, which the probe always has.
    ///
    /// # Errors
    ///
    /// A fault the kernel did not say it provoked.
    pub(crate) fn judge(
        &self,
        arch: Arch,
        reached: bool,
        lines: &[String],
        log: &Path,
    ) -> Result<()> {
        if !reached {
            self.show_remarks();
            if let Some(problem) = problem(self, lines) {
                eprintln!("  {arch}: beside that, {problem}");
            }
            return Ok(());
        }
        if let Some(problem) = problem(self, lines) {
            self.show_remarks();
            return Err(Error::new(format!(
                "{arch}: {problem}.\n  Serial output is in {}",
                log.display()
            )));
        }
        report(self, lines);
        Ok(())
    }

    /// Print QEMU's remarks as it wrote them.
    fn show_remarks(&self) {
        for remark in &self.remarks {
            eprintln!("{remark}");
        }
    }
}

/// Print what became of the run's faults, once [`problem`] has passed them,
/// and the remarks unless every fault they could be about was accounted for.
fn report(seen: &Seen, lines: &[String]) {
    if seen.faults.is_empty() {
        // No trace: either nothing faulted, or this QEMU cannot trace. Only
        // the remarks tell, and they are shown as QEMU wrote them.
        seen.show_remarks();
        if !seen.remarks.is_empty() {
            println!(
                "  QEMU remarked on a VT-d fault but traced none: it lacks the log trace \
                 backend, so the run's DMA faults were not accounted (tools/common/xtask/src/dma_faults.rs)"
            );
        }
        return;
    }
    let provoked = provoked(lines)
        .iter()
        .map(|(stream, page)| format!("stream {stream:#x} page {page:#x}"))
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "  {} VT-d faults traced, every one the kernel's out-of-domain probe ({provoked}); \
         QEMU's {} remarks on them hidden (tools/common/xtask/src/dma_faults.rs says why)",
        seen.faults.len(),
        seen.remarks.len(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROBE: &str = "  iommu    stream 0x10 is made to write page 0x1000 outside its domain, \
                         on purpose";

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    fn seen(faults: &[&str]) -> Seen {
        Seen {
            faults: faults.iter().filter_map(|line| traced(line)).collect(),
            remarks: Vec::new(),
        }
    }

    #[test]
    fn a_trace_line_reads_as_its_fault() {
        let line = "1610532@1790384790.551523:vtd_dmar_fault sid 0x10 fault 5 addr 0x1004 write 1";
        assert_eq!(
            traced(line),
            Some(Fault {
                source: 0x10,
                address: 0x1004,
                write: true
            })
        );
        assert_eq!(traced("vtd_frr_new index 0 high 0x1 low 0x1000"), None);
        assert_eq!(traced("qemu-system-x86_64: something else"), None);
    }

    #[test]
    fn the_probes_faults_pass_and_anything_else_fails() {
        let boot = lines(&[PROBE]);
        let probe = seen(&[
            "1@1.0:vtd_dmar_fault sid 0x10 fault 5 addr 0x1000 write 1",
            "1@1.0:vtd_dmar_fault sid 0x10 fault 5 addr 0x103c write 1",
        ]);
        assert_eq!(problem(&probe, &boot), None);

        let other_device = seen(&["1@1.0:vtd_dmar_fault sid 0x18 fault 5 addr 0x1000 write 1"]);
        assert!(problem(&other_device, &boot).is_some_and(|why| why.contains("00:03.0")));
        let other_page = seen(&["1@1.0:vtd_dmar_fault sid 0x10 fault 5 addr 0x2000 write 1"]);
        assert!(problem(&other_page, &boot).is_some());
        let a_read = seen(&["1@1.0:vtd_dmar_fault sid 0x10 fault 6 addr 0x1000 write 0"]);
        assert!(problem(&a_read, &boot).is_some());
    }

    /// QEMU's AMD-Vi trace carries the address alone: its faults are held to
    /// the pages provoked, whatever stream provoked them.
    ///
    /// Verifies: L.iommu.90
    #[test]
    fn amdvi_faults_are_counted_by_page() {
        let line =
            "1@1.0:amdvi_page_fault error: page fault accessing guest physical address 0x1004";
        assert_eq!(
            traced(line),
            Some(Fault {
                source: UNATTRIBUTED,
                address: 0x1004,
                write: true
            })
        );
        let boot = lines(&[PROBE]);
        assert_eq!(problem(&seen(&[line]), &boot), None);
        let other_page = seen(&[
            "1@1.0:amdvi_page_fault error: page fault accessing guest physical address 0x9000",
        ]);
        assert!(problem(&other_page, &boot).is_some());
        assert!(remark(
            "qemu-system-x86_64: failed to remap interrupt from devid 0x200"
        ));
    }

    #[test]
    fn a_fault_with_no_probe_said_fails() {
        let probe = seen(&["1@1.0:vtd_dmar_fault sid 0x10 fault 5 addr 0x1000 write 1"]);
        assert!(problem(&probe, &[]).is_some());
    }

    #[test]
    fn the_remarks_are_recognised_and_nothing_else_is() {
        assert!(remark(
            "qemu-system-x86_64: vtd_iova_to_slpte: detected slpte permission error (iova=0x1000, \
             level=0x2, slpte=0x0, write=1, pasid=0xffffffff)"
        ));
        assert!(remark(
            "qemu-system-x86_64: vtd_iommu_translate: detected translation failure (dev=00:02:00, \
             iova=0x1000)"
        ));
        assert!(!remark(
            "qemu-system-x86_64: -device ide-hd: Failed to get \"write\" lock"
        ));
    }
}
