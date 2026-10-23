//! `/proc/cpuinfo`: a block per online processor, as Linux writes it.
//!
//! ```text
//! processor    : 0
//! vendor_id    : GenuineIntel
//! cpu family    : 6
//! model        : 190
//! model name    : Intel(R) N150
//! ...
//! flags        : fpu vme de pse tsc msr pae mce cx8 apic sep ...
//! ```
//!
//! Programs read it with `sscanf` and `strstr`: glibc's `get_nprocs`
//! fallback counts `processor` lines, Steam's hardware survey and a game's
//! launcher look for `model name` and for `avx2` among the `flags`, a fetch
//! tool prints the model. So each label, tab and order is Linux's
//! (`arch/x86/kernel/cpu/proc.c`, `arch/arm64/kernel/cpuinfo.c`,
//! `arch/arm/kernel/setup.c`), and each feature name is Linux's own
//! spelling, copied from its tables by `tools/common/gen/gen-cpuinfo-names.py`
//! into its `names` module.
//!
//! On x86-64 everything Linux prints from `CPUID` is decoded here from a
//! `CPUID` the kernel hands in ([`x86::Identity::read`]), so the host tests
//! can drive it with any processor's leaves. What Linux prints from model
//! specific registers -- `microcode`, `bugs`, `vmx flags`, and the flags it
//! sets from `IA32_MISC_ENABLE` or `IA32_ARCH_CAPABILITIES` (`rep_good`,
//! `pebs`, `bts`, `ibrs_enhanced`, ...) -- is left out rather than guessed.
//! On Arm the kernel hands in each processor's `MIDR` and the `AT_HWCAP`
//! words it gives programs.

use alloc::vec::Vec;

use crate::text::put;

mod names;
#[cfg(test)]
mod tests;

/// `BogoMIPS` from a counter's rate in kHz, as Linux prints it: Linux's
/// `loops_per_jiffy` is the counter's ticks per jiffy, and `bogomips` is
/// `lpj / (500000 / HZ)` with two decimals of `lpj / (5000 / HZ)`, which is
/// `khz / 500` and `khz / 5 % 100` whatever `HZ` is.
fn bogomips(out: &mut Vec<u8>, label: &str, khz: u64) {
    put(
        out,
        format_args!("{label}: {}.{:02}\n", khz / 500, khz / 5 % 100),
    );
}

/// x86-64's blocks.
pub mod x86 {
    use alloc::vec::Vec;

    use super::names::{X86_DEPENDENCIES, X86_FLAGS, X86_SCATTERED, X86_WORDS, feature};
    use crate::text::put;

    /// One `CPUID`: leaf and subleaf in, `EAX`, `EBX`, `ECX` and `EDX` out.
    pub type Cpuid<'a> = &'a dyn Fn(u32, u32) -> [u32; 4];

    /// What the kernel let programs use, where a feature the processor has
    /// needs the kernel's say-so as well. Linux clears such a feature's flag
    /// when it does not enable it, and a flag here must mean the same.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Enabled {
        /// `XCR0`: the state components the kernel saves for a program;
        /// zero where it uses no `XSAVE`. Without AVX's, `avx` and every
        /// flag that needs it go; without AVX-512's three, `avx512f` and its
        /// kin; without AMX's tile data, `amx_tile` and its kin.
        pub xcr0: u64,
        /// `CR4.FSGSBASE`: a program may run `RDFSBASE` and its kin.
        pub fsgsbase: bool,
        /// `CR4.PKE`: protection keys.
        pub protection_keys: bool,
        /// Five-level paging.
        pub five_level_paging: bool,
    }

    /// `XCR0`'s AVX state component.
    const XSTATE_AVX: u64 = 1 << 2;
    /// `XCR0`'s AVX-512 components: the opmask, and the upper and high `ZMM`
    /// registers.
    const XSTATE_AVX512: u64 = 0b111 << 5;
    /// `XCR0`'s AMX tile data component.
    const XSTATE_AMX_TILE: u64 = 1 << 18;

    /// The leaf with the TSC's and its crystal's rates.
    const LEAF_TSC: u32 = 0x15;
    /// `CPUID` 1 `EDX` bit 19: `CLFLUSH`, and `EBX` gives its line size.
    const CLFLUSH: u32 = 1 << 19;
    /// `CPUID` 1 `EDX` bit 28: `HTT`, and `EBX` gives the logical processors
    /// a package has.
    const HTT: u32 = 1 << 28;

    /// Who made the processor, as far as Linux's decoding differs by it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Vendor {
        /// Intel, and Centaur and Zhaoxin, which Linux decodes as Intel.
        Intel,
        /// AMD and Hygon.
        Amd,
        /// Anyone else.
        Other,
    }

    /// The bits Linux decodes from `CPUID` into `struct cpuinfo_x86`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Identity {
        /// Leaf 0's twelve bytes, `GenuineIntel`.
        vendor_id: [u8; 12],
        /// Leaf 0 `EAX`: the highest basic leaf.
        cpuid_level: u32,
        /// Leaf 1 `EAX`'s family, model and stepping, extended as Linux's
        /// `x86_family` and `x86_model` do.
        family: u32,
        /// See `family`.
        model: u32,
        /// See `family`.
        stepping: u32,
        /// Leaf 1 `EAX` whole.
        signature: u32,
        /// The brand string, trimmed.
        model_name: Vec<u8>,
        /// The largest cache Linux names, in KiB; zero for none.
        cache_kb: u32,
        /// TLB entries, where `cpu_detect_cache_sizes` counted them.
        tlb_entries: u32,
        /// `CLFLUSH`'s line size.
        clflush: u32,
        /// Linux's `x86_cache_alignment`.
        cache_alignment: u32,
        /// Leaf `0x8000_0008`'s address widths.
        physical_bits: u32,
        /// See `physical_bits`.
        virtual_bits: u32,
        /// Linux's capability words, `NCAPINTS` of them.
        caps: [u32; X86_WORDS],
        /// Leaf `0x8000_0007` `EDX`: Linux's `x86_power`.
        power: u32,
        /// Bits of an APIC ID below its core's number.
        smt_shift: u32,
        /// Bits of an APIC ID below its package's number.
        package_shift: u32,
    }

    /// One online processor.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Processor {
        /// Its number, the one `processor` says and `sched_setaffinity` takes.
        pub number: usize,
        /// Its APIC ID.
        pub apic_id: u32,
    }

    impl Identity {
        /// Decode what `cpuid` answers, as Linux's `identify_cpu` does.
        #[must_use]
        pub fn read(cpuid: Cpuid<'_>, enabled: Enabled) -> Identity {
            let [cpuid_level, b, c, d] = cpuid(0, 0);
            let mut vendor_id = [0_u8; 12];
            for (chunk, word) in vendor_id.chunks_mut(4).zip([b, d, c]) {
                chunk.copy_from_slice(&word.to_le_bytes());
            }
            let vendor = match &vendor_id {
                b"GenuineIntel" | b"CentaurHauls" | b"  Shanghai  " => Vendor::Intel,
                b"AuthenticAMD" | b"HygonGenuine" => Vendor::Amd,
                _ => Vendor::Other,
            };
            let [signature, misc, _, edx] = if cpuid_level >= 1 {
                cpuid(1, 0)
            } else {
                [0; 4]
            };
            let (family, model, stepping) = signature_fields(signature);
            let extended = cpuid(0x8000_0000, 0)[0];
            let extended = if extended & 0xFFFF_0000 == 0x8000_0000 {
                extended
            } else {
                0
            };
            let leaf = |number: u32| -> [u32; 4] {
                let highest = if number >= 0x8000_0000 {
                    extended
                } else {
                    cpuid_level
                };
                if highest >= number {
                    cpuid(number, 0)
                } else {
                    [0; 4]
                }
            };
            let clflush = if edx & CLFLUSH != 0 {
                ((misc >> 8) & 0xFF) * 8
            } else {
                0
            };
            let sizes = leaf(0x8000_0008)[0];
            let (physical_bits, virtual_bits, clflush) = if extended >= 0x8000_0008 {
                (
                    sizes & 0xFF,
                    (sizes >> 8) & 0xFF,
                    if clflush == 0 { 32 } else { clflush },
                )
            } else {
                (36, 48, 64)
            };
            let (smt_shift, package_shift, extended_topology) =
                topology(cpuid, cpuid_level, vendor, misc, edx);
            let (cache_kb, tlb_entries) = cache_kb(cpuid, vendor, cpuid_level, extended);
            let mut identity = Identity {
                vendor_id,
                cpuid_level,
                family,
                model,
                stepping,
                signature,
                model_name: model_name(cpuid, extended, family, model),
                cache_kb,
                tlb_entries,
                clflush,
                cache_alignment: clflush,
                physical_bits,
                virtual_bits,
                caps: [0; X86_WORDS],
                power: leaf(0x8000_0007)[3],
                smt_shift,
                package_shift,
            };
            identity.fill_caps(cpuid, extended, vendor, extended_topology);
            identity.clear_disabled(enabled);
            identity
        }

        /// Whether capability `bit` is set.
        fn has(&self, bit: u16) -> bool {
            let bit = usize::from(bit);
            self.caps
                .get(bit / 32)
                .is_some_and(|word| word & (1 << (bit % 32)) != 0)
        }

        /// Set capability `bit`.
        fn set(&mut self, bit: u16) {
            let bit = usize::from(bit);
            if let Some(word) = self.caps.get_mut(bit / 32) {
                *word |= 1 << (bit % 32);
            }
        }

        /// Clear capability `bit` alone.
        fn unset(&mut self, bit: u16) {
            let bit = usize::from(bit);
            if let Some(word) = self.caps.get_mut(bit / 32) {
                *word &= !(1 << (bit % 32));
            }
        }

        /// Clear capability `bit`, and every one that needs it, as Linux's
        /// `do_clear_cpu_cap` does over `cpuid_deps`: only what follows from
        /// this bit, not every pair the processor's own bits leave unmet.
        fn clear(&mut self, bit: u16) {
            let mut cleared = Identity::empty_caps();
            let mut next = Some(bit);
            while let Some(bit) = next {
                self.unset(bit);
                let index = usize::from(bit);
                if let Some(word) = cleared.get_mut(index / 32) {
                    *word |= 1 << (index % 32);
                }
                let was_cleared = |needed: u16| {
                    let needed = usize::from(needed);
                    cleared
                        .get(needed / 32)
                        .is_some_and(|word| word & (1 << (needed % 32)) != 0)
                };
                // Each round clears a bit that was set, so this ends.
                next = X86_DEPENDENCIES
                    .iter()
                    .find(|&&(needs, needed)| self.has(needs) && was_cleared(needed))
                    .map(|&(needs, _)| needs);
            }
        }

        /// No capabilities.
        const fn empty_caps() -> [u32; X86_WORDS] {
            [0; X86_WORDS]
        }

        /// Linux's `get_cpu_cap`: the whole words leaves give, then the
        /// scattered bits, the speculation controls, and the flags Linux sets
        /// from `CPUID` alone.
        fn fill_caps(&mut self, cpuid: Cpuid<'_>, extended: u32, vendor: Vendor, topology: bool) {
            let level = self.cpuid_level;
            // (Linux's word, leaf, subleaf, register: 0 `EAX` to 3 `EDX`).
            let words: [(usize, u32, u32, usize); 13] = [
                (4, 1, 0, 2),
                (0, 1, 0, 3),
                (14, 6, 0, 0),
                (9, 7, 0, 1),
                (16, 7, 0, 2),
                (18, 7, 0, 3),
                (10, 0xD, 1, 0),
                (6, 0x8000_0001, 0, 2),
                (1, 0x8000_0001, 0, 3),
                (13, 0x8000_0008, 0, 1),
                (15, 0x8000_000A, 0, 3),
                (19, 0x8000_001F, 0, 0),
                (20, 0x8000_0021, 0, 0),
            ];
            for (word, leaf, subleaf, register) in words {
                let highest = if leaf < 0x8000_0000 { level } else { extended };
                if highest >= leaf
                    && let (Some(slot), Some(&value)) =
                        (self.caps.get_mut(word), cpuid(leaf, subleaf).get(register))
                {
                    *slot = value;
                }
            }
            if level >= 7
                && cpuid(7, 0)[0] >= 1
                && let Some(slot) = self.caps.get_mut(12)
            {
                *slot = cpuid(7, 1)[0];
            }
            for &(bit, register, shift, leaf, subleaf) in X86_SCATTERED {
                let highest = cpuid(leaf & 0xFFFF_0000, 0)[0];
                if highest < leaf || highest > (leaf | 0xFFFF) {
                    continue;
                }
                let value = cpuid(leaf, subleaf)
                    .get(usize::from(register))
                    .copied()
                    .unwrap_or(0);
                if value & (1 << shift) != 0 {
                    self.set(bit);
                }
            }
            // `ppin_init` keeps the inventory number only once it has read
            // it from a register; nothing here does.
            self.clear(feature::INTEL_PPIN);
            self.speculation_control();
            self.synthetic(cpuid, vendor, topology);
        }

        /// Linux's `init_speculation_control`: the names Linux gives the
        /// controls, from Intel's and AMD's separate bits.
        fn speculation_control(&mut self) {
            if self.has(feature::SPEC_CTRL) {
                self.set(feature::IBRS);
                self.set(feature::IBPB);
            }
            if self.has(feature::INTEL_STIBP) {
                self.set(feature::STIBP);
            }
            if self.has(feature::SPEC_CTRL_SSBD) || self.has(feature::VIRT_SSBD) {
                self.set(feature::SSBD);
            }
            if self.has(feature::AMD_IBRS) {
                self.set(feature::IBRS);
            }
            if self.has(feature::AMD_IBPB) {
                self.set(feature::IBPB);
            }
            if self.has(feature::AMD_STIBP) {
                self.set(feature::STIBP);
            }
            if self.has(feature::AMD_SSBD) {
                self.set(feature::SSBD);
                self.clear(feature::VIRT_SSBD);
            }
        }

        /// The flags in Linux's own words that `CPUID` alone decides:
        /// `cpuid` and `nopl` on every x86-64 (`detect_nopl`); `xtopology`
        /// when leaf 0x1F or 0xB describes the topology;
        /// `constant_tsc` and `nonstop_tsc` from the invariant TSC bit
        /// (`early_init_intel`, `early_init_amd`); `arch_perfmon` from leaf
        /// 0xA (`init_intel`); `tsc_known_freq` when leaf 0x15 names the
        /// crystal (`native_calibrate_tsc`); and AMD's own (`amd`).
        fn synthetic(&mut self, cpuid: Cpuid<'_>, vendor: Vendor, topology: bool) {
            self.set(feature::CPUID);
            self.set(feature::NOPL);
            if topology {
                self.set(feature::XTOPOLOGY);
            }
            if vendor != Vendor::Other && self.power & (1 << 8) != 0 {
                self.set(feature::CONSTANT_TSC);
                self.set(feature::NONSTOP_TSC);
            }
            if vendor == Vendor::Intel && self.cpuid_level > 9 {
                let perfmon = cpuid(0xA, 0)[0];
                if perfmon & 0xFF != 0 && (perfmon >> 8) & 0xFF > 1 {
                    self.set(feature::ARCH_PERFMON);
                }
            }
            if self.vendor_id == *b"GenuineIntel" && self.cpuid_level >= LEAF_TSC {
                let [denominator, numerator, crystal, _] = cpuid(LEAF_TSC, 0);
                if denominator != 0 && numerator != 0 && crystal != 0 {
                    self.set(feature::TSC_KNOWN_FREQ);
                }
            }
            if vendor == Vendor::Amd {
                self.amd();
            }
        }

        /// What `early_init_amd`, `init_amd` and `cpu_set_bug_bits` set on
        /// AMD from `CPUID` alone: the accumulated power and RAPL bits of
        /// `x86_power`, the 8-bit APIC ID after family 0x16, `vmmcall`
        /// always, `rep_good` from family 0x10, and Automatic IBRS as Intel's
        /// `ibrs_enhanced`. Memory encryption, which Linux keeps only where
        /// firmware and the kernel turned it on, is cleared: Ferrix uses none.
        fn amd(&mut self) {
            if self.power & (1 << 12) != 0 {
                self.set(feature::ACC_POWER);
            }
            if self.power & (1 << 14) != 0 {
                self.set(feature::RAPL);
            }
            if self.has(feature::APIC) && self.family > 0x16 {
                self.set(feature::EXTD_APICID);
            }
            self.set(feature::VMMCALL);
            // `init_amd`, and `init_amd_k8` for a K8 from stepping C.
            let signature = self.signature;
            if self.family >= 0x10
                || (self.family == 0xF
                    && ((0x0F48..0x0F50).contains(&signature) || signature >= 0x0F58))
            {
                self.set(feature::REP_GOOD);
            }
            if self.family > 0x11 {
                self.set(feature::ARAT);
            }
            if self.has(feature::THREEDNOW) || self.has(feature::LM) {
                self.set(feature::THREEDNOWPREFETCH);
            }
            if self.has(feature::AUTOIBRS) {
                self.set(feature::IBRS_ENHANCED);
            }
            for encryption in [
                feature::SME,
                feature::SEV,
                feature::SEV_ES,
                feature::SEV_SNP,
            ] {
                self.clear(encryption);
            }
        }

        /// Clear what the kernel did not enable, with what needs it.
        fn clear_disabled(&mut self, enabled: Enabled) {
            if enabled.xcr0 == 0 {
                self.clear(feature::XSAVE);
            }
            if enabled.xcr0 & XSTATE_AVX == 0 {
                self.clear(feature::AVX);
            }
            if enabled.xcr0 & XSTATE_AVX512 != XSTATE_AVX512 {
                self.clear(feature::AVX512F);
            }
            if enabled.xcr0 & XSTATE_AMX_TILE == 0 {
                self.clear(feature::AMX_TILE);
            }
            if !enabled.fsgsbase {
                self.clear(feature::FSGSBASE);
            }
            if !enabled.protection_keys {
                self.clear(feature::PKU);
            }
            if !enabled.five_level_paging {
                self.clear(feature::LA57);
            }
        }

        /// The package an APIC ID is in: Linux's `pkg_id`.
        fn package_of(&self, apic_id: u32) -> u32 {
            apic_id.checked_shr(self.package_shift).unwrap_or(0)
        }

        /// The core an APIC ID is, within its package: Linux's `core_id`.
        fn core_of(&self, apic_id: u32) -> u32 {
            let mask = 1_u32
                .checked_shl(self.package_shift)
                .map_or(u32::MAX, |bit| bit - 1);
            (apic_id & mask).checked_shr(self.smt_shift).unwrap_or(0)
        }

        /// The brand string, trimmed as Linux trims it: `Intel(R) N150`.
        #[must_use]
        pub fn model_name(&self) -> &[u8] {
            &self.model_name
        }

        /// The names `flags` prints, in its order.
        pub fn flags(&self) -> impl Iterator<Item = &'static str> + '_ {
            X86_FLAGS
                .iter()
                .filter(|&&(bit, _)| self.has(bit))
                .map(|&(_, name)| name)
        }

        /// Whether the processor has a TSC, which is when Linux prints
        /// `cpu MHz`.
        #[must_use]
        pub fn has_tsc(&self) -> bool {
            self.has(feature::TSC)
        }
    }

    /// Leaf 1 `EAX`'s family, model and stepping: Linux's `x86_family`,
    /// `x86_model` and `x86_stepping`.
    #[must_use]
    pub const fn signature_fields(signature: u32) -> (u32, u32, u32) {
        let mut family = (signature >> 8) & 0xF;
        if family == 0xF {
            family += (signature >> 20) & 0xFF;
        }
        let mut model = (signature >> 4) & 0xF;
        if family >= 0x6 {
            model += ((signature >> 16) & 0xF) << 4;
        }
        (family, model, signature & 0xF)
    }

    /// The brand string from leaves `0x8000_0002` to `0x8000_0004`, as Linux's
    /// `get_model_name` reads it, or, without them, the `"%02x/%02x"` of
    /// family and model it falls back to.
    fn model_name(cpuid: Cpuid<'_>, extended: u32, family: u32, model: u32) -> Vec<u8> {
        if extended < 0x8000_0004 {
            let mut name = Vec::new();
            put(&mut name, format_args!("{family:02x}/{model:02x}"));
            return name;
        }
        let mut raw = [0_u8; 48];
        for (chunk, leaf) in raw.chunks_mut(16).zip(0x8000_0002_u32..) {
            for (bytes, word) in chunk.chunks_mut(4).zip(cpuid(leaf, 0)) {
                bytes.copy_from_slice(&word.to_le_bytes());
            }
        }
        let name = trim_brand(&raw);
        if name.is_empty() {
            let mut name = Vec::new();
            put(&mut name, format_args!("{family:02x}/{model:02x}"));
            return name;
        }
        name.to_vec()
    }

    /// Linux's trim of a brand string: up to its first NUL, without the
    /// spaces Intel pads its front with, and without white space at its end.
    #[must_use]
    pub fn trim_brand(raw: &[u8]) -> &[u8] {
        let end = raw.iter().position(|&byte| byte == 0).unwrap_or(raw.len());
        let text = raw.get(..end).unwrap_or_default();
        let start = text
            .iter()
            .position(|&byte| byte != b' ')
            .unwrap_or(text.len());
        let text = text.get(start..).unwrap_or_default();
        let kept = text
            .iter()
            .rposition(|&byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r'))
            .map_or(0, |last| last + 1);
        text.get(..kept).unwrap_or_default()
    }

    /// Linux's `x86_cache_size` and `x86_tlbsize`. On Intel, the third-level
    /// cache from leaf 4, else the second, else the first two together
    /// (`intel_cacheinfo_0x4`), and with no second level what
    /// `cpu_detect_cache_sizes` finds instead. Everywhere else
    /// `cpu_detect_cache_sizes` alone. (Intel's leaf 2 descriptors, which
    /// Linux decodes on a processor without leaf 4, are not: every
    /// processor Ferrix starts has leaf 4.)
    fn cache_kb(cpuid: Cpuid<'_>, vendor: Vendor, level: u32, extended: u32) -> (u32, u32) {
        if vendor != Vendor::Intel || level < 4 {
            return legacy_cache_kb(cpuid, extended, 0);
        }
        let mut sizes = [0_u32; 4];
        for index in 0..64 {
            let [eax, ebx, ecx, _] = cpuid(4, index);
            let kind = eax & 0x1F;
            if kind == 0 {
                break;
            }
            let bytes = (u64::from(ecx) + 1)
                * u64::from((ebx & 0xFFF) + 1)
                * u64::from(((ebx >> 12) & 0x3FF) + 1)
                * u64::from((ebx >> 22) + 1);
            let slot = match ((eax >> 5) & 0x7, kind) {
                (1, 1) => 0,
                (1, 2) => 1,
                (2, _) => 2,
                (3, _) => 3,
                _ => continue,
            };
            if let Some(size) = sizes.get_mut(slot) {
                *size = u32::try_from(bytes / 1024).unwrap_or(u32::MAX);
            }
        }
        let [l1d, l1i, l2, l3] = sizes;
        let size = if l3 != 0 {
            l3
        } else if l2 != 0 {
            l2
        } else {
            l1i + l1d
        };
        if l2 != 0 {
            (size, 0)
        } else {
            legacy_cache_kb(cpuid, extended, size)
        }
    }

    /// Linux's `cpu_detect_cache_sizes` over `size`: the first-level caches
    /// from leaf `0x8000_0005`, then, on x86-64, whatever leaf `0x8000_0006`
    /// says the second level is, zero included; and the TLB entries it
    /// counts there.
    fn legacy_cache_kb(cpuid: Cpuid<'_>, extended: u32, mut size: u32) -> (u32, u32) {
        let mut tlb = 0;
        if extended >= 0x8000_0005 {
            let [_, _, ecx, edx] = cpuid(0x8000_0005, 0);
            size = (ecx >> 24) + (edx >> 24);
        }
        if extended >= 0x8000_0006 {
            let [_, ebx, ecx, _] = cpuid(0x8000_0006, 0);
            tlb = ((ebx >> 16) & 0xFFF) + (ebx & 0xFFF);
            size = ecx >> 16;
        }
        (size, tlb)
    }

    /// The APIC ID's layout: bits below the core number, bits below the
    /// package number, and whether leaf 0x1F or 0xB gave them (Linux's
    /// `parse_topology_leaf`, which sets `xtopology`). Without either, the
    /// legacy leaves: leaf 1's logical count per package, and on Intel leaf
    /// 4's cores per package.
    fn topology(
        cpuid: Cpuid<'_>,
        level: u32,
        vendor: Vendor,
        misc: u32,
        edx: u32,
    ) -> (u32, u32, bool) {
        for leaf in [0x1F, 0xB] {
            if level < leaf {
                continue;
            }
            let mut smt = 0;
            let mut package = None;
            for subleaf in 0..8 {
                let [eax, ebx, ecx, _] = cpuid(leaf, subleaf);
                let kind = (ecx >> 8) & 0xFF;
                if ebx & 0xFFFF == 0 || kind == 0 {
                    break;
                }
                if kind == 1 {
                    smt = eax & 0x1F;
                }
                package = Some(eax & 0x1F);
            }
            if let Some(package) = package {
                return (smt, package, true);
            }
        }
        if edx & HTT == 0 {
            return (0, 0, false);
        }
        let package = order((misc >> 16) & 0xFF);
        let cores = if vendor == Vendor::Intel && level >= 4 {
            (cpuid(4, 0)[0] >> 26) + 1
        } else {
            1
        };
        (package.saturating_sub(order(cores)), package, false)
    }

    /// Linux's `get_count_order`: the bits needed to count to `count`.
    const fn order(count: u32) -> u32 {
        if count <= 1 {
            0
        } else {
            32 - (count - 1).leading_zeros()
        }
    }

    /// Write the blocks: one per processor, `processors` in order. `khz`
    /// is the TSC's rate where the kernel measured it: `cpu MHz` and
    /// `bogomips`, which Linux also derives from it, are left out without.
    pub fn render(out: &mut Vec<u8>, id: &Identity, processors: &[Processor], khz: Option<u64>) {
        for processor in processors {
            block(out, id, processors, *processor, khz);
        }
    }

    /// One processor's block: `show_cpuinfo`.
    fn block(
        out: &mut Vec<u8>,
        id: &Identity,
        processors: &[Processor],
        processor: Processor,
        khz: Option<u64>,
    ) {
        let name = core::str::from_utf8(&id.model_name).unwrap_or("unknown");
        let vendor = core::str::from_utf8(&id.vendor_id).unwrap_or("unknown");
        put(
            out,
            format_args!(
                "processor\t: {}\nvendor_id\t: {vendor}\ncpu family\t: {}\nmodel\t\t: {}\n\
                 model name\t: {name}\nstepping\t: {}\n",
                processor.number, id.family, id.model, id.stepping,
            ),
        );
        if let Some(khz) = khz.filter(|_| id.has_tsc()) {
            put(
                out,
                format_args!("cpu MHz\t\t: {}.{:03}\n", khz / 1000, khz % 1000),
            );
        }
        if id.cache_kb != 0 {
            put(out, format_args!("cache size\t: {} KB\n", id.cache_kb));
        }
        let package = id.package_of(processor.apic_id);
        let core_id = id.core_of(processor.apic_id);
        let mates: Vec<u32> = processors
            .iter()
            .map(|other| other.apic_id)
            .filter(|&apic| id.package_of(apic) == package)
            .collect();
        let siblings = mates.len();
        let cores = mates
            .iter()
            .enumerate()
            .filter(|&(index, &apic)| {
                !mates
                    .iter()
                    .take(index)
                    .any(|&earlier| id.core_of(earlier) == id.core_of(apic))
            })
            .count();
        put(
            out,
            format_args!(
                "physical id\t: {package}\nsiblings\t: {siblings}\ncore id\t\t: {core_id}\n\
                 cpu cores\t: {cores}\napicid\t\t: {apic}\ninitial apicid\t: {apic}\n\
                 fpu\t\t: yes\nfpu_exception\t: yes\ncpuid level\t: {}\nwp\t\t: yes\n",
                id.cpuid_level,
                apic = processor.apic_id,
            ),
        );
        out.extend_from_slice(b"flags\t\t:");
        for flag in id.flags() {
            out.push(b' ');
            out.extend_from_slice(flag.as_bytes());
        }
        out.push(b'\n');
        if let Some(khz) = khz {
            super::bogomips(out, "bogomips\t", khz);
        }
        if id.tlb_entries > 0 {
            put(
                out,
                format_args!("TLB size\t: {} 4K pages\n", id.tlb_entries),
            );
        }
        put(
            out,
            format_args!(
                "clflush size\t: {}\ncache_alignment\t: {}\n\
                 address sizes\t: {} bits physical, {} bits virtual\npower management:",
                id.clflush, id.cache_alignment, id.physical_bits, id.virtual_bits,
            ),
        );
        power_flags(out, id.power);
        out.extend_from_slice(b"\n\n");
    }

    /// `power management:`'s names, by bit of leaf `0x8000_0007` `EDX`
    /// (`arch/x86/kernel/cpu/powerflags.c`); bit 8, the invariant TSC, has
    /// the empty name, because `constant_tsc` already says it.
    const POWER_FLAGS: [&str; 13] = [
        "ts",
        "fid",
        "vid",
        "ttp",
        "tm",
        "stc",
        "100mhzsteps",
        "hwpstate",
        "",
        "cpb",
        "eff_freq_ro",
        "proc_feedback",
        "acc_power",
    ];

    /// The rest of the `power management:` line.
    fn power_flags(out: &mut Vec<u8>, power: u32) {
        for bit in 0..32_u32 {
            if power & (1 << bit) == 0 {
                continue;
            }
            match POWER_FLAGS.get(bit as usize) {
                Some(&"") => {}
                Some(name) => put(out, format_args!(" {name}")),
                None => put(out, format_args!(" [{bit}]")),
            }
        }
    }
}

/// AArch64's and ARMv7-A's blocks.
pub mod arm {
    use alloc::vec::Vec;

    use super::names::{AARCH64_FEATURES, ARM_HWCAP, ARM_HWCAP2};
    use crate::text::put;

    /// One online processor.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Processor {
        /// Its number.
        pub number: usize,
        /// Its `MIDR`, where the kernel knows it: without it the block has no
        /// `CPU implementer`, `variant`, `part` or `revision`.
        pub midr: Option<u32>,
    }

    /// The architecture whose blocks to write, with what it needs.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Machine<'a> {
        /// AArch64: `AT_HWCAP` and `AT_HWCAP2`.
        Aarch64 {
            /// `AT_HWCAP`.
            hwcap: u64,
            /// `AT_HWCAP2`.
            hwcap2: u64,
        },
        /// ARMv7-A: `AT_HWCAP`, `AT_HWCAP2` and `AT_PLATFORM`.
        Armv7a {
            /// `AT_HWCAP`.
            hwcap: u64,
            /// `AT_HWCAP2`.
            hwcap2: u64,
            /// `AT_PLATFORM`, `v7l`.
            platform: &'a str,
        },
    }

    /// The `Features` names a machine's words give, in Linux's order.
    pub fn features<'a>(machine: Machine<'a>) -> impl Iterator<Item = &'static str> + 'a {
        let (aarch64, armv7a) = match machine {
            Machine::Aarch64 { hwcap, hwcap2 } => (Some([hwcap, hwcap2]), None),
            Machine::Armv7a { hwcap, hwcap2, .. } => (None, Some([hwcap, hwcap2])),
        };
        let wide = AARCH64_FEATURES
            .iter()
            .filter(move |&&(word, bit, _)| {
                aarch64
                    .and_then(|words| words.get(usize::from(word)).copied())
                    .unwrap_or(0)
                    & (1 << bit)
                    != 0
            })
            .map(|&(_, _, name)| name);
        let narrow =
            [ARM_HWCAP, ARM_HWCAP2]
                .into_iter()
                .enumerate()
                .flat_map(move |(word, names)| {
                    let value = armv7a
                        .and_then(|words| words.get(word).copied())
                        .unwrap_or(0);
                    names
                        .iter()
                        .enumerate()
                        .filter(move |&(bit, _)| value & (1 << bit) != 0)
                        .map(|(_, &name)| name)
                });
        wide.chain(narrow)
    }

    /// Write the blocks. `timer_hz` is the generic timer's rate, which
    /// Linux's `BogoMIPS` comes from on both architectures.
    pub fn render(
        out: &mut Vec<u8>,
        machine: Machine<'_>,
        processors: &[Processor],
        timer_hz: u64,
    ) {
        for processor in processors {
            put(out, format_args!("processor\t: {}\n", processor.number));
            if let (Machine::Armv7a { platform, .. }, Some(midr)) = (machine, processor.midr) {
                put(
                    out,
                    format_args!(
                        "model name\t: ARMv7 Processor rev {} ({platform})\n",
                        midr & 0xF
                    ),
                );
            }
            super::bogomips(out, "BogoMIPS\t", timer_hz / 1000);
            match machine {
                // `c_show` on AArch64: a space before each name.
                Machine::Aarch64 { .. } => {
                    out.extend_from_slice(b"Features\t:");
                    for name in features(machine) {
                        put(out, format_args!(" {name}"));
                    }
                }
                // On ARM: a space after each.
                Machine::Armv7a { .. } => {
                    out.extend_from_slice(b"Features\t: ");
                    for name in features(machine) {
                        put(out, format_args!("{name} "));
                    }
                }
            }
            out.push(b'\n');
            let architecture = match machine {
                Machine::Aarch64 { .. } => 8,
                Machine::Armv7a { .. } => 7,
            };
            let Some(midr) = processor.midr else {
                put(out, format_args!("CPU architecture: {architecture}\n\n"));
                continue;
            };
            put(
                out,
                format_args!(
                    "CPU implementer\t: 0x{:02x}\nCPU architecture: {architecture}\n\
                     CPU variant\t: 0x{:x}\nCPU part\t: 0x{:03x}\nCPU revision\t: {}\n\n",
                    midr >> 24,
                    (midr >> 20) & 0xF,
                    (midr >> 4) & 0xFFF,
                    midr & 0xF,
                ),
            );
        }
    }
}
