//! `/proc/cpuinfo` against what Linux prints.
//!
//! The processors here are `CPUID` tables written out by hand: a leaf the
//! table lacks answers zeros, as a leaf past the highest does on hardware.
//! Each expectation is Linux's output, from `arch/x86/kernel/cpu/proc.c`'s
//! formats, for a processor with those leaves.

use alloc::string::String;
use alloc::vec::Vec;

use super::arm::{self, Machine};
use super::names::{X86_DEPENDENCIES, X86_FLAGS};
use super::x86::{self, Enabled, Identity, Processor, signature_fields, trim_brand};

/// A brand string's three leaves from its 48 bytes.
fn brand_leaves(brand: &[u8; 48]) -> [(u32, u32, [u32; 4]); 3] {
    let word =
        |at: usize| u32::from_le_bytes([brand[at], brand[at + 1], brand[at + 2], brand[at + 3]]);
    let leaf = |base: usize| [word(base), word(base + 4), word(base + 8), word(base + 12)];
    [
        (0x8000_0002, 0, leaf(0)),
        (0x8000_0003, 0, leaf(16)),
        (0x8000_0004, 0, leaf(32)),
    ]
}

/// A 48-byte brand string field from text, NUL-padded.
fn brand(text: &str) -> [u8; 48] {
    let mut raw = [0_u8; 48];
    raw[..text.len()].copy_from_slice(text.as_bytes());
    raw
}

/// An Intel processor: four cores, no SMT, a 6 MiB third-level cache, AVX2
/// and AVX-512F offered, `FSGSBASE` and protection keys offered, Intel's
/// right-justified brand string.
fn intel_leaves() -> Vec<(u32, u32, [u32; 4])> {
    let mut leaves = alloc::vec![
        // "GenuineIntel": EBX, EDX, ECX.
        (0, 0, [0x20, 0x756E_6547, 0x6C65_746E, 0x4965_6E69]),
        // Family 6, model 0xBE, stepping 0; CLFLUSH line 8 * 8; EDX: fpu,
        // tsc, apic, clflush, sse2, htt. ECX: sse3, xsave, osxsave, avx.
        (
            1,
            0,
            [
                0x000B_06E0,
                0x0004_0800,
                (1 << 0) | (1 << 26) | (1 << 27) | (1 << 28),
                (1 << 0) | (1 << 4) | (1 << 9) | (1 << 19) | (1 << 26) | (1 << 28)
            ]
        ),
        // Leaf 4: L1d 32 KiB, L1i 64 KiB, L2 2 MiB, L3 6 MiB.
        (4, 0, [0x21, (7 << 22) | 63, 63, 0]),
        (4, 1, [0x22, (7 << 22) | 63, 127, 0]),
        (4, 2, [0x43, (15 << 22) | 63, 2047, 0]),
        (4, 3, [0x63, (11 << 22) | 63, 8191, 0]),
        // Leaf 6: dtherm, arat.
        (6, 0, [(1 << 0) | (1 << 2), 0, 0, 0]),
        // Leaf 7: fsgsbase, avx2, avx512f; ECX pku; EDX spec-ctrl, stibp.
        (
            7,
            0,
            [
                0,
                (1 << 0) | (1 << 5) | (1 << 16),
                1 << 3,
                (1 << 26) | (1 << 27)
            ]
        ),
        // Leaf 0xB: SMT shift 0 (one thread), core level shift 3, four cores.
        (0xB, 0, [0, 1, 0x100, 0]),
        (0xB, 1, [3, 4, 0x201, 0]),
        // Leaf 0x15: TSC and crystal ratio, crystal named.
        (0x15, 0, [2, 84, 38_400_000, 0]),
        (0x8000_0000, 0, [0x8000_0008, 0, 0, 0]),
        // lahf_lm; syscall, nx, lm.
        (0x8000_0001, 0, [0, 0, 1, (1 << 11) | (1 << 20) | (1 << 29)]),
        // Invariant TSC.
        (0x8000_0007, 0, [0, 0, 0, 1 << 8]),
        // 39 bits physical, 48 virtual.
        (0x8000_0008, 0, [0x3027, 0, 0, 0]),
    ];
    leaves.extend(brand_leaves(&brand("           Intel(R) N150\0")));
    leaves
}

/// A `CPUID` that answers from `leaves`, zeros for anything else.
fn table(leaves: &[(u32, u32, [u32; 4])]) -> impl Fn(u32, u32) -> [u32; 4] + '_ {
    move |leaf, subleaf| {
        leaves
            .iter()
            .find(|&&(l, s, _)| l == leaf && s == subleaf)
            .map_or([0; 4], |&(_, _, regs)| regs)
    }
}

/// x87, SSE and AVX: what Ferrix enables.
const FERRIX: Enabled = Enabled {
    xcr0: 0b111,
    fsgsbase: false,
    protection_keys: false,
    five_level_paging: false,
};

#[test]
fn brand_strings_are_trimmed_as_linux_trims_them() {
    assert_eq!(
        trim_brand(&brand("       Intel(R) Core(TM) i7-8700 CPU @ 3.20GHz")),
        b"Intel(R) Core(TM) i7-8700 CPU @ 3.20GHz"
    );
    assert_eq!(
        trim_brand(&brand("AMD Ryzen 9 9900X 12-Core Processor            ")),
        b"AMD Ryzen 9 9900X 12-Core Processor"
    );
    // Up to the first NUL, and white space of any kind gone from the end,
    // but only spaces from the front.
    assert_eq!(
        trim_brand(&brand("Intel(R) N150 \t\0garbage")),
        b"Intel(R) N150"
    );
    assert_eq!(
        trim_brand(&brand("\tQEMU Virtual CPU version 2.5+")),
        b"\tQEMU Virtual CPU version 2.5+"
    );
    assert_eq!(trim_brand(&brand("                ")), b"");
    assert_eq!(trim_brand(&[b'x'; 48]), &[b'x'; 48][..]);
}

#[test]
fn the_signature_is_extended_as_linux_extends_it() {
    // The N150: family 6, model 190.
    assert_eq!(signature_fields(0x000B_06E0), (6, 190, 0));
    // QEMU's qemu64: family 15 + 0, model 6 << 4 + 0xB.
    assert_eq!(signature_fields(0x0006_0FB1), (15, 107, 1));
    // Zen 5: family 0xF + 0xB, model 4 << 4 + 4.
    assert_eq!(signature_fields(0x00B4_0F40), (26, 68, 0));
    // Below family 6 the extended model is not added.
    assert_eq!(signature_fields(0x0001_0543), (5, 4, 3));
}

#[test]
fn an_intel_block_is_linuxs() {
    let leaves = intel_leaves();
    let id = Identity::read(&table(&leaves), FERRIX);
    let processors = [
        Processor {
            number: 0,
            apic_id: 0,
        },
        Processor {
            number: 1,
            apic_id: 1,
        },
    ];
    let mut out = Vec::new();
    x86::render(&mut out, &id, &processors, Some(806_400));
    let text = String::from_utf8(out).unwrap();
    let first = text.split("\n\n").next().unwrap();
    assert_eq!(
        first,
        "processor\t: 0\n\
         vendor_id\t: GenuineIntel\n\
         cpu family\t: 6\n\
         model\t\t: 190\n\
         model name\t: Intel(R) N150\n\
         stepping\t: 0\n\
         cpu MHz\t\t: 806.400\n\
         cache size\t: 6144 KB\n\
         physical id\t: 0\n\
         siblings\t: 2\n\
         core id\t\t: 0\n\
         cpu cores\t: 2\n\
         apicid\t\t: 0\n\
         initial apicid\t: 0\n\
         fpu\t\t: yes\n\
         fpu_exception\t: yes\n\
         cpuid level\t: 32\n\
         wp\t\t: yes\n\
         flags\t\t: fpu tsc apic clflush sse2 ht syscall nx lm constant_tsc nopl xtopology \
         nonstop_tsc cpuid tsc_known_freq pni xsave avx lahf_lm ibrs ibpb stibp avx2 \
         dtherm arat\n\
         bogomips\t: 1612.80\n\
         clflush size\t: 64\n\
         cache_alignment\t: 64\n\
         address sizes\t: 39 bits physical, 48 bits virtual\n\
         power management:"
    );
    assert!(text.contains("processor\t: 1\n"));
    assert!(text.contains("core id\t\t: 1\n"));
    assert!(text.ends_with("power management:\n\n"));
}

#[test]
fn a_flag_the_kernel_did_not_enable_is_not_claimed() {
    let leaves = intel_leaves();
    let flags = |enabled| {
        Identity::read(&table(&leaves), enabled)
            .flags()
            .collect::<Vec<_>>()
    };
    // Everything enabled: AVX-512F, FSGSBASE and protection keys appear.
    let all = Enabled {
        xcr0: 0b1110_0111,
        fsgsbase: true,
        protection_keys: true,
        five_level_paging: true,
    };
    let everything = flags(all);
    for flag in ["avx", "avx2", "avx512f", "fsgsbase", "pku"] {
        assert!(
            everything.contains(&flag),
            "{flag} missing from {everything:?}"
        );
    }
    // Ferrix's: no AVX-512 state, no FSGSBASE, no keys.
    let ferrix = flags(FERRIX);
    for flag in ["avx512f", "fsgsbase", "pku"] {
        assert!(!ferrix.contains(&flag), "{flag} claimed in {ferrix:?}");
    }
    assert!(ferrix.contains(&"avx2"));
    // No AVX state: AVX goes, and AVX2, which needs it, with it.
    let sse = flags(Enabled {
        xcr0: 0b11,
        ..FERRIX
    });
    assert!(!sse.contains(&"avx") && !sse.contains(&"avx2"), "{sse:?}");
    assert!(sse.contains(&"xsave"));
    // No XSAVE at all: xsave goes too.
    let none = flags(Enabled { xcr0: 0, ..FERRIX });
    assert!(!none.contains(&"xsave"), "{none:?}");
}

#[test]
fn an_amd_processor_gets_linuxs_amd_flags_and_l2_size() {
    let leaves = [
        // "AuthenticAMD".
        (0, 0, [0x10, 0x6874_7541, 0x444D_4163, 0x6974_6E65]),
        (
            1,
            0,
            [
                0x00B4_0F40,
                0x0018_0800,
                0,
                (1 << 0) | (1 << 9) | (1 << 19) | (1 << 28),
            ],
        ),
        (0x8000_0000, 0, [0x8000_0021, 0, 0, 0]),
        (0x8000_0001, 0, [0, 0, 0, 1 << 29]),
        // L1: 48 KiB data, 32 KiB instructions; L2 1 MiB; TLB 2048 + 1024.
        (0x8000_0005, 0, [0, 0, 48 << 24, 32 << 24]),
        (0x8000_0006, 0, [0, (2048 << 16) | 1024, 1024 << 16, 0]),
        // Invariant TSC, RAPL.
        (0x8000_0007, 0, [0, 0, 0, (1 << 8) | (1 << 14)]),
        (0x8000_0008, 0, [0x3030, 0, 0, 0]),
        // SME offered.
        (0x8000_001F, 0, [1, 0, 0, 0]),
        // Automatic IBRS.
        (0x8000_0021, 0, [1 << 8, 0, 0, 0]),
    ];
    let id = Identity::read(&table(&leaves), FERRIX);
    let flags: Vec<_> = id.flags().collect();
    assert_eq!(
        flags,
        [
            "fpu",
            "apic",
            "clflush",
            "ht",
            "lm",
            "constant_tsc",
            "rep_good",
            "nopl",
            "nonstop_tsc",
            "cpuid",
            "extd_apicid",
            "rapl",
            "3dnowprefetch",
            "ibrs_enhanced",
            "vmmcall",
            "arat",
        ]
    );
    let mut out = Vec::new();
    x86::render(
        &mut out,
        &id,
        &[Processor {
            number: 0,
            apic_id: 0,
        }],
        None,
    );
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("cache size\t: 1024 KB\nphysical id"),
        "{text}"
    );
    assert!(
        text.contains("\nTLB size\t: 3072 4K pages\nclflush size"),
        "{text}"
    );
    // No TSC rate: no `cpu MHz`, no `bogomips`.
    assert!(
        !text.contains("cpu MHz") && !text.contains("bogomips"),
        "{text}"
    );
    // Bit 8 is `constant_tsc`'s and printed as nothing; bit 14 has no name.
    assert!(text.contains("power management: [14]\n"), "{text}");
}

#[test]
fn smt_siblings_share_a_core() {
    // Two packages of two cores of two threads: SMT shift 1, package shift 2.
    let leaves = [
        (0, 0, [0xB, 0x756E_6547, 0x6C65_746E, 0x4965_6E69]),
        (0xB, 0, [1, 2, 0x100, 0]),
        (0xB, 1, [2, 4, 0x201, 0]),
    ];
    let id = Identity::read(&table(&leaves), FERRIX);
    let processors: Vec<Processor> = (0..8)
        .map(|n| Processor {
            number: n,
            apic_id: n as u32,
        })
        .collect();
    let mut out = Vec::new();
    x86::render(&mut out, &id, &processors, None);
    let text = String::from_utf8(out).unwrap();
    let field = |block: &str, name: &str| -> String {
        block
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap()
            .into()
    };
    let blocks: Vec<&str> = text.split("\n\n").filter(|b| !b.is_empty()).collect();
    let rows: Vec<(String, String, String, String)> = blocks
        .iter()
        .map(|b| {
            (
                field(b, "physical id\t: "),
                field(b, "core id\t\t: "),
                field(b, "siblings\t: "),
                field(b, "cpu cores\t: "),
            )
        })
        .collect();
    let expect = |p: &str, c: &str| (p.into(), c.into(), "4".into(), "2".into());
    assert_eq!(
        rows,
        [
            expect("0", "0"),
            expect("0", "0"),
            expect("0", "1"),
            expect("0", "1"),
            expect("1", "0"),
            expect("1", "0"),
            expect("1", "1"),
            expect("1", "1"),
        ]
    );
}

#[test]
fn no_brand_string_falls_back_to_family_and_model() {
    let leaves = [(0, 0, [1, 0, 0, 0]), (1, 0, [0x0000_0663, 0, 0, 0])];
    let id = Identity::read(&table(&leaves), FERRIX);
    assert_eq!(id.model_name(), b"06/06");
}

#[test]
fn the_flag_names_are_linuxs() {
    // In print order, each bit once.
    assert!(X86_FLAGS.windows(2).all(|pair| pair[0].0 < pair[1].0));
    let name = |bit: u16| X86_FLAGS.iter().find(|&&(b, _)| b == bit).map(|&(_, n)| n);
    // Linux's spellings, not the manuals': SSE3 is `pni`, the debug store
    // `dts`, word 4 bit 0 and word 0 bit 21.
    assert_eq!(name(4 * 32), Some("pni"));
    assert_eq!(name(21), Some("dts"));
    assert_eq!(name(9 * 32 + 5), Some("avx2"));
    assert_eq!(name(4 * 32 + 31), Some("hypervisor"));
    // `OSXSAVE` has an empty name in `cpufeatures.h`: never printed.
    assert_eq!(name(4 * 32 + 27), None);
    // AVX2 needs AVX.
    assert!(X86_DEPENDENCIES.contains(&(9 * 32 + 5, 4 * 32 + 28)));
}

#[test]
fn an_aarch64_block_is_linuxs() {
    let machine = Machine::Aarch64 {
        hwcap: 0b1111_1011,
        hwcap2: 0,
    };
    let mut out = Vec::new();
    arm::render(
        &mut out,
        machine,
        &[
            arm::Processor {
                number: 0,
                midr: Some(0x410F_D083),
            },
            arm::Processor {
                number: 1,
                midr: None,
            },
        ],
        62_500_000,
    );
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "processor\t: 0\nBogoMIPS\t: 125.00\nFeatures\t: fp asimd aes pmull sha1 sha2 crc32\n\
         CPU implementer\t: 0x41\nCPU architecture: 8\nCPU variant\t: 0x0\nCPU part\t: 0xd08\n\
         CPU revision\t: 3\n\n\
         processor\t: 1\nBogoMIPS\t: 125.00\nFeatures\t: fp asimd aes pmull sha1 sha2 crc32\n\
         CPU architecture: 8\n\n"
    );
}

#[test]
fn an_armv7a_block_is_linuxs() {
    // half thumb fastmult vfp edsp neon vfpv3 tls vfpv4 idiva idivt vfpd32 lpae
    let hwcap = (1 << 1)
        | (1 << 2)
        | (1 << 4)
        | (1 << 6)
        | (1 << 7)
        | (1 << 12)
        | (1 << 13)
        | (1 << 15)
        | (1 << 16)
        | (1 << 17)
        | (1 << 18)
        | (1 << 19)
        | (1 << 20);
    let machine = Machine::Armv7a {
        hwcap,
        hwcap2: 0,
        platform: "v7l",
    };
    let mut out = Vec::new();
    arm::render(
        &mut out,
        machine,
        &[arm::Processor {
            number: 0,
            midr: Some(0x410F_C075),
        }],
        24_000_000,
    );
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "processor\t: 0\nmodel name\t: ARMv7 Processor rev 5 (v7l)\nBogoMIPS\t: 48.00\n\
         Features\t: half thumb fastmult vfp edsp neon vfpv3 tls vfpv4 idiva idivt vfpd32 lpae \n\
         CPU implementer\t: 0x41\nCPU architecture: 7\nCPU variant\t: 0x0\nCPU part\t: 0xc07\n\
         CPU revision\t: 5\n\n"
    );
}
