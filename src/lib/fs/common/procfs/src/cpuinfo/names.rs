//! Linux's names for processor features, as `/proc/cpuinfo` prints them.
//!
//! Written by `tools/common/gen/gen-cpuinfo-names.py` from Linux v7.0's
//! `cpufeatures.h`, `cpuid-deps.c`, `scattered.c`, arm64's `hwcap.h` and
//! `cpuinfo.c`, and arm's `setup.c`. Do not edit; run the script.

/// Linux's `NCAPINTS`: 32-bit words of x86 capability bits.
pub(super) const X86_WORDS: usize = 22;

/// The x86 capability bits `/proc/cpuinfo`'s `flags` names, in the order
/// it prints them: (word * 32 + bit, name).
pub(super) const X86_FLAGS: &[(u16, &str)] = &[
    (0, "fpu"),
    (1, "vme"),
    (2, "de"),
    (3, "pse"),
    (4, "tsc"),
    (5, "msr"),
    (6, "pae"),
    (7, "mce"),
    (8, "cx8"),
    (9, "apic"),
    (11, "sep"),
    (12, "mtrr"),
    (13, "pge"),
    (14, "mca"),
    (15, "cmov"),
    (16, "pat"),
    (17, "pse36"),
    (18, "pn"),
    (19, "clflush"),
    (21, "dts"),
    (22, "acpi"),
    (23, "mmx"),
    (24, "fxsr"),
    (25, "sse"),
    (26, "sse2"),
    (27, "ss"),
    (28, "ht"),
    (29, "tm"),
    (30, "ia64"),
    (31, "pbe"),
    (43, "syscall"),
    (51, "mp"),
    (52, "nx"),
    (54, "mmxext"),
    (57, "fxsr_opt"),
    (58, "pdpe1gb"),
    (59, "rdtscp"),
    (61, "lm"),
    (62, "3dnowext"),
    (63, "3dnow"),
    (64, "recovery"),
    (65, "longrun"),
    (67, "lrti"),
    (96, "cxmmx"),
    (97, "k6_mtrr"),
    (98, "cyrix_arr"),
    (99, "centaur_mcr"),
    (104, "constant_tsc"),
    (105, "up"),
    (106, "art"),
    (107, "arch_perfmon"),
    (108, "pebs"),
    (109, "bts"),
    (112, "rep_good"),
    (113, "amd_lbr_v2"),
    (115, "acc_power"),
    (116, "nopl"),
    (118, "xtopology"),
    (119, "tsc_reliable"),
    (120, "nonstop_tsc"),
    (121, "cpuid"),
    (122, "extd_apicid"),
    (123, "amd_dcm"),
    (124, "aperfmperf"),
    (125, "rapl"),
    (126, "nonstop_tsc_s3"),
    (127, "tsc_known_freq"),
    (128, "pni"),
    (129, "pclmulqdq"),
    (130, "dtes64"),
    (131, "monitor"),
    (132, "ds_cpl"),
    (133, "vmx"),
    (134, "smx"),
    (135, "est"),
    (136, "tm2"),
    (137, "ssse3"),
    (138, "cid"),
    (139, "sdbg"),
    (140, "fma"),
    (141, "cx16"),
    (142, "xtpr"),
    (143, "pdcm"),
    (145, "pcid"),
    (146, "dca"),
    (147, "sse4_1"),
    (148, "sse4_2"),
    (149, "x2apic"),
    (150, "movbe"),
    (151, "popcnt"),
    (152, "tsc_deadline_timer"),
    (153, "aes"),
    (154, "xsave"),
    (156, "avx"),
    (157, "f16c"),
    (158, "rdrand"),
    (159, "hypervisor"),
    (162, "rng"),
    (163, "rng_en"),
    (166, "ace"),
    (167, "ace_en"),
    (168, "ace2"),
    (169, "ace2_en"),
    (170, "phe"),
    (171, "phe_en"),
    (172, "pmm"),
    (173, "pmm_en"),
    (192, "lahf_lm"),
    (193, "cmp_legacy"),
    (194, "svm"),
    (195, "extapic"),
    (196, "cr8_legacy"),
    (197, "abm"),
    (198, "sse4a"),
    (199, "misalignsse"),
    (200, "3dnowprefetch"),
    (201, "osvw"),
    (202, "ibs"),
    (203, "xop"),
    (204, "skinit"),
    (205, "wdt"),
    (207, "lwp"),
    (208, "fma4"),
    (209, "tce"),
    (211, "nodeid_msr"),
    (213, "tbm"),
    (214, "topoext"),
    (215, "perfctr_core"),
    (216, "perfctr_nb"),
    (218, "bpext"),
    (219, "ptsc"),
    (220, "perfctr_llc"),
    (221, "mwaitx"),
    (224, "ring3mwait"),
    (225, "cpuid_fault"),
    (226, "cpb"),
    (227, "epb"),
    (228, "cat_l3"),
    (229, "cat_l2"),
    (230, "cdp_l3"),
    (231, "tdx_host_platform"),
    (232, "hw_pstate"),
    (233, "proc_feedback"),
    (235, "pti"),
    (238, "intel_ppin"),
    (239, "cdp_l2"),
    (241, "ssbd"),
    (242, "mba"),
    (244, "perfmon_v2"),
    (249, "ibrs"),
    (250, "ibpb"),
    (251, "stibp"),
    (254, "ibrs_enhanced"),
    (256, "tpr_shadow"),
    (257, "flexpriority"),
    (258, "ept"),
    (259, "vpid"),
    (271, "vmmcall"),
    (273, "ept_ad"),
    (278, "tdx_guest"),
    (288, "fsgsbase"),
    (289, "tsc_adjust"),
    (290, "sgx"),
    (291, "bmi1"),
    (292, "hle"),
    (293, "avx2"),
    (295, "smep"),
    (296, "bmi2"),
    (297, "erms"),
    (298, "invpcid"),
    (299, "rtm"),
    (300, "cqm"),
    (302, "mpx"),
    (303, "rdt_a"),
    (304, "avx512f"),
    (305, "avx512dq"),
    (306, "rdseed"),
    (307, "adx"),
    (308, "smap"),
    (309, "avx512ifma"),
    (311, "clflushopt"),
    (312, "clwb"),
    (313, "intel_pt"),
    (314, "avx512pf"),
    (315, "avx512er"),
    (316, "avx512cd"),
    (317, "sha_ni"),
    (318, "avx512bw"),
    (319, "avx512vl"),
    (320, "xsaveopt"),
    (321, "xsavec"),
    (322, "xgetbv1"),
    (323, "xsaves"),
    (352, "cqm_llc"),
    (353, "cqm_occup_llc"),
    (354, "cqm_mbm_total"),
    (355, "cqm_mbm_local"),
    (358, "split_lock_detect"),
    (375, "user_shstk"),
    (388, "avx_vnni"),
    (389, "avx512_bf16"),
    (390, "lass"),
    (401, "fred"),
    (410, "lam"),
    (416, "clzero"),
    (417, "irperf"),
    (418, "xsaveerptr"),
    (420, "rdpru"),
    (425, "wbnoinvd"),
    (439, "amd_ppin"),
    (441, "virt_ssbd"),
    (443, "cppc"),
    (447, "brs"),
    (448, "dtherm"),
    (449, "ida"),
    (450, "arat"),
    (452, "pln"),
    (454, "pts"),
    (455, "hwp"),
    (456, "hwp_notify"),
    (457, "hwp_act_window"),
    (458, "hwp_epp"),
    (459, "hwp_pkg_req"),
    (467, "hfi"),
    (480, "npt"),
    (481, "lbrv"),
    (482, "svm_lock"),
    (483, "nrip_save"),
    (484, "tsc_scale"),
    (485, "vmcb_clean"),
    (486, "flushbyasid"),
    (487, "decodeassists"),
    (490, "pausefilter"),
    (492, "pfthreshold"),
    (493, "avic"),
    (495, "v_vmsave_vmload"),
    (496, "vgif"),
    (498, "x2avic"),
    (500, "v_spec_ctrl"),
    (505, "vnmi"),
    (513, "avx512vbmi"),
    (514, "umip"),
    (515, "pku"),
    (516, "ospke"),
    (517, "waitpkg"),
    (518, "avx512_vbmi2"),
    (520, "gfni"),
    (521, "vaes"),
    (522, "vpclmulqdq"),
    (523, "avx512_vnni"),
    (524, "avx512_bitalg"),
    (525, "tme"),
    (526, "avx512_vpopcntdq"),
    (528, "la57"),
    (534, "rdpid"),
    (536, "bus_lock_detect"),
    (537, "cldemote"),
    (539, "movdiri"),
    (540, "movdir64b"),
    (541, "enqcmd"),
    (542, "sgx_lc"),
    (544, "overflow_recov"),
    (545, "succor"),
    (547, "smca"),
    (578, "avx512_4vnniw"),
    (579, "avx512_4fmaps"),
    (580, "fsrm"),
    (584, "avx512_vp2intersect"),
    (586, "md_clear"),
    (590, "serialize"),
    (592, "tsxldtrk"),
    (594, "pconfig"),
    (595, "arch_lbr"),
    (596, "ibt"),
    (598, "amx_bf16"),
    (599, "avx512_fp16"),
    (600, "amx_tile"),
    (601, "amx_int8"),
    (604, "flush_l1d"),
    (605, "arch_capabilities"),
    (608, "sme"),
    (609, "sev"),
    (611, "sev_es"),
    (612, "sev_snp"),
    (622, "debug_swap"),
    (636, "svsm"),
    (672, "amd_lbr_pmc_freeze"),
];

/// `cpuid_deps`: (feature, the feature it needs). Clearing the second
/// clears the first.
pub(super) const X86_DEPENDENCIES: &[(u16, u16)] = &[
    (24, 0),    // fxsr needs fpu
    (320, 154), // xsaveopt needs xsave
    (321, 154), // xsavec needs xsave
    (323, 154), // xsaves needs xsave
    (156, 154), // avx needs xsave
    (515, 154), // pku needs xsave
    (302, 154), // mpx needs xsave
    (322, 154), // xgetbv1 needs xsave
    (681, 154), // apx needs xsave
    (15, 24),   // cmov needs fxsr
    (23, 24),   // mmx needs fxsr
    (54, 23),   // mmxext needs mmx
    (57, 24),   // fxsr_opt needs fxsr
    (154, 24),  // xsave needs fxsr
    (25, 24),   // xmm needs fxsr
    (26, 25),   // xmm2 needs xmm
    (128, 26),  // xmm3 needs xmm2
    (147, 26),  // xmm4_1 needs xmm2
    (148, 26),  // xmm4_2 needs xmm2
    (128, 26),  // xmm3 needs xmm2
    (129, 26),  // pclmulqdq needs xmm2
    (137, 26),  // ssse3 needs xmm2
    (157, 26),  // f16c needs xmm2
    (153, 26),  // aes needs xmm2
    (317, 26),  // sha_ni needs xmm2
    (520, 26),  // gfni needs xmm2
    (388, 156), // avx_vnni needs avx
    (140, 156), // fma needs avx
    (521, 156), // vaes needs avx
    (522, 156), // vpclmulqdq needs avx
    (293, 156), // avx2 needs avx
    (304, 156), // avx512f needs avx
    (309, 304), // avx512ifma needs avx512f
    (314, 304), // avx512pf needs avx512f
    (315, 304), // avx512er needs avx512f
    (316, 304), // avx512cd needs avx512f
    (305, 304), // avx512dq needs avx512f
    (318, 304), // avx512bw needs avx512f
    (319, 304), // avx512vl needs avx512f
    (513, 304), // avx512vbmi needs avx512f
    (518, 319), // avx512_vbmi2 needs avx512vl
    (523, 319), // avx512_vnni needs avx512vl
    (524, 319), // avx512_bitalg needs avx512vl
    (578, 304), // avx512_4vnniw needs avx512f
    (579, 304), // avx512_4fmaps needs avx512f
    (526, 304), // avx512_vpopcntdq needs avx512f
    (584, 319), // avx512_vp2intersect needs avx512vl
    (353, 352), // cqm_occup_llc needs cqm_llc
    (354, 352), // cqm_mbm_total needs cqm_llc
    (355, 352), // cqm_mbm_local needs cqm_llc
    (374, 354), // bmec needs cqm_mbm_total
    (374, 355), // bmec needs cqm_mbm_local
    (690, 228), // sdciae needs cat_l3
    (389, 319), // avx512_bf16 needs avx512vl
    (599, 318), // avx512_fp16 needs avx512bw
    (541, 323), // enqcmd needs xsaves
    (359, 242), // per_thread_mba needs mba
    (542, 290), // sgx_lc needs sgx
    (360, 290), // sgx1 needs sgx
    (361, 360), // sgx2 needs sgx1
    (689, 360), // sgx_eupdatesvn needs sgx1
    (370, 360), // sgx_edeccssa needs sgx1
    (324, 323), // xfd needs xsaves
    (324, 322), // xfd needs xgetbv1
    (600, 324), // amx_tile needs xfd
    (405, 600), // amx_fp16 needs amx_tile
    (598, 600), // amx_bf16 needs amx_tile
    (601, 600), // amx_int8 needs amx_tile
    (519, 323), // shstk needs xsaves
    (401, 402), // fred needs lkgs
    (607, 602), // spec_ctrl_ssbd needs spec_ctrl
    (390, 308), // lass needs smap
];

/// `cpuid_bits`, the features Linux scatters into its own words:
/// (feature, register -- 0 `EAX` to 3 `EDX` --, bit, leaf, subleaf).
pub(super) const X86_SCATTERED: &[(u16, u8, u8, u32, u32)] = &[
    (124, 2, 0, 0x6, 0),         // aperfmperf
    (227, 2, 3, 0x6, 0),         // epb
    (238, 1, 0, 0x7, 1),         // intel_ppin
    (688, 2, 5, 0x7, 1),         // msr_imm
    (681, 3, 21, 0x7, 1),        // apx
    (363, 3, 2, 0x7, 2),         // rrsba_ctrl
    (674, 3, 4, 0x7, 2),         // bhi_ctrl
    (352, 3, 1, 0xf, 0),         // cqm_llc
    (353, 3, 0, 0xf, 1),         // cqm_occup_llc
    (354, 3, 1, 0xf, 1),         // cqm_mbm_total
    (355, 3, 2, 0xf, 1),         // cqm_mbm_local
    (228, 1, 1, 0x10, 0),        // cat_l3
    (229, 1, 2, 0x10, 0),        // cat_l2
    (230, 2, 2, 0x10, 1),        // cdp_l3
    (239, 2, 2, 0x10, 2),        // cdp_l2
    (242, 1, 3, 0x10, 0),        // mba
    (359, 2, 0, 0x10, 3),        // per_thread_mba
    (360, 0, 0, 0x12, 0),        // sgx1
    (361, 0, 1, 0x12, 0),        // sgx2
    (689, 0, 10, 0x12, 0),       // sgx_eupdatesvn
    (370, 0, 11, 0x12, 0),       // sgx_edeccssa
    (544, 1, 0, 0x80000007, 0),  // overflow_recov
    (545, 1, 1, 0x80000007, 0),  // succor
    (547, 1, 3, 0x80000007, 0),  // smca
    (232, 3, 7, 0x80000007, 0),  // hw_pstate
    (226, 3, 9, 0x80000007, 0),  // cpb
    (233, 3, 11, 0x80000007, 0), // proc_feedback
    (677, 3, 15, 0x80000007, 0), // amd_fast_cppc
    (242, 1, 6, 0x80000008, 0),  // mba
    (692, 2, 6, 0x8000000a, 0),  // x2avic_ext
    (260, 1, 31, 0x8000001f, 0), // coherency_sfw_no
    (373, 1, 2, 0x80000020, 0),  // smba
    (374, 1, 3, 0x80000020, 0),  // bmec
    (687, 1, 5, 0x80000020, 0),  // abmc
    (690, 1, 6, 0x80000020, 0),  // sdciae
    (683, 2, 1, 0x80000021, 0),  // tsa_sq_no
    (684, 2, 2, 0x80000021, 0),  // tsa_l1_no
    (679, 0, 22, 0x80000021, 0), // amd_workload_class
    (244, 0, 0, 0x80000022, 0),  // perfmon_v2
    (113, 0, 1, 0x80000022, 0),  // amd_lbr_v2
    (672, 0, 2, 0x80000022, 0),  // amd_lbr_pmc_freeze
    (678, 0, 30, 0x80000026, 0), // amd_htr_cores
];

/// The x86 features the renderer names in its own logic.
pub(super) mod feature {
    /// `X86_FEATURE_ACC_POWER`, word 3 bit 19.
    pub(in super::super) const ACC_POWER: u16 = 115;
    /// `X86_FEATURE_AMD_IBPB`, word 13 bit 12.
    pub(in super::super) const AMD_IBPB: u16 = 428;
    /// `X86_FEATURE_AMD_IBRS`, word 13 bit 14.
    pub(in super::super) const AMD_IBRS: u16 = 430;
    /// `X86_FEATURE_AMD_SSBD`, word 13 bit 24.
    pub(in super::super) const AMD_SSBD: u16 = 440;
    /// `X86_FEATURE_AMD_STIBP`, word 13 bit 15.
    pub(in super::super) const AMD_STIBP: u16 = 431;
    /// `X86_FEATURE_AMX_TILE`, word 18 bit 24.
    pub(in super::super) const AMX_TILE: u16 = 600;
    /// `X86_FEATURE_APIC`, word 0 bit 9.
    pub(in super::super) const APIC: u16 = 9;
    /// `X86_FEATURE_ARAT`, word 14 bit 2.
    pub(in super::super) const ARAT: u16 = 450;
    /// `X86_FEATURE_ARCH_PERFMON`, word 3 bit 11.
    pub(in super::super) const ARCH_PERFMON: u16 = 107;
    /// `X86_FEATURE_AUTOIBRS`, word 20 bit 8.
    pub(in super::super) const AUTOIBRS: u16 = 648;
    /// `X86_FEATURE_AVX`, word 4 bit 28.
    pub(in super::super) const AVX: u16 = 156;
    /// `X86_FEATURE_AVX512F`, word 9 bit 16.
    pub(in super::super) const AVX512F: u16 = 304;
    /// `X86_FEATURE_CONSTANT_TSC`, word 3 bit 8.
    pub(in super::super) const CONSTANT_TSC: u16 = 104;
    /// `X86_FEATURE_CPUID`, word 3 bit 25.
    pub(in super::super) const CPUID: u16 = 121;
    /// `X86_FEATURE_EXTD_APICID`, word 3 bit 26.
    pub(in super::super) const EXTD_APICID: u16 = 122;
    /// `X86_FEATURE_FSGSBASE`, word 9 bit 0.
    pub(in super::super) const FSGSBASE: u16 = 288;
    /// `X86_FEATURE_IBPB`, word 7 bit 26.
    pub(in super::super) const IBPB: u16 = 250;
    /// `X86_FEATURE_IBRS`, word 7 bit 25.
    pub(in super::super) const IBRS: u16 = 249;
    /// `X86_FEATURE_IBRS_ENHANCED`, word 7 bit 30.
    pub(in super::super) const IBRS_ENHANCED: u16 = 254;
    /// `X86_FEATURE_INTEL_PPIN`, word 7 bit 14.
    pub(in super::super) const INTEL_PPIN: u16 = 238;
    /// `X86_FEATURE_INTEL_STIBP`, word 18 bit 27.
    pub(in super::super) const INTEL_STIBP: u16 = 603;
    /// `X86_FEATURE_LA57`, word 16 bit 16.
    pub(in super::super) const LA57: u16 = 528;
    /// `X86_FEATURE_LM`, word 1 bit 29.
    pub(in super::super) const LM: u16 = 61;
    /// `X86_FEATURE_NONSTOP_TSC`, word 3 bit 24.
    pub(in super::super) const NONSTOP_TSC: u16 = 120;
    /// `X86_FEATURE_NOPL`, word 3 bit 20.
    pub(in super::super) const NOPL: u16 = 116;
    /// `X86_FEATURE_PKU`, word 16 bit 3.
    pub(in super::super) const PKU: u16 = 515;
    /// `X86_FEATURE_RAPL`, word 3 bit 29.
    pub(in super::super) const RAPL: u16 = 125;
    /// `X86_FEATURE_REP_GOOD`, word 3 bit 16.
    pub(in super::super) const REP_GOOD: u16 = 112;
    /// `X86_FEATURE_SEV`, word 19 bit 1.
    pub(in super::super) const SEV: u16 = 609;
    /// `X86_FEATURE_SEV_ES`, word 19 bit 3.
    pub(in super::super) const SEV_ES: u16 = 611;
    /// `X86_FEATURE_SEV_SNP`, word 19 bit 4.
    pub(in super::super) const SEV_SNP: u16 = 612;
    /// `X86_FEATURE_SME`, word 19 bit 0.
    pub(in super::super) const SME: u16 = 608;
    /// `X86_FEATURE_SPEC_CTRL`, word 18 bit 26.
    pub(in super::super) const SPEC_CTRL: u16 = 602;
    /// `X86_FEATURE_SPEC_CTRL_SSBD`, word 18 bit 31.
    pub(in super::super) const SPEC_CTRL_SSBD: u16 = 607;
    /// `X86_FEATURE_SSBD`, word 7 bit 17.
    pub(in super::super) const SSBD: u16 = 241;
    /// `X86_FEATURE_STIBP`, word 7 bit 27.
    pub(in super::super) const STIBP: u16 = 251;
    /// `X86_FEATURE_3DNOW`, word 1 bit 31.
    pub(in super::super) const THREEDNOW: u16 = 63;
    /// `X86_FEATURE_3DNOWPREFETCH`, word 6 bit 8.
    pub(in super::super) const THREEDNOWPREFETCH: u16 = 200;
    /// `X86_FEATURE_TSC`, word 0 bit 4.
    pub(in super::super) const TSC: u16 = 4;
    /// `X86_FEATURE_TSC_KNOWN_FREQ`, word 3 bit 31.
    pub(in super::super) const TSC_KNOWN_FREQ: u16 = 127;
    /// `X86_FEATURE_VIRT_SSBD`, word 13 bit 25.
    pub(in super::super) const VIRT_SSBD: u16 = 441;
    /// `X86_FEATURE_VMMCALL`, word 8 bit 15.
    pub(in super::super) const VMMCALL: u16 = 271;
    /// `X86_FEATURE_XSAVE`, word 4 bit 26.
    pub(in super::super) const XSAVE: u16 = 154;
    /// `X86_FEATURE_XTOPOLOGY`, word 3 bit 22.
    pub(in super::super) const XTOPOLOGY: u16 = 118;
}

/// AArch64's `Features` names: (0 for `AT_HWCAP`, 1 for `AT_HWCAP2`, bit,
/// name), in the order Linux prints them.
pub(super) const AARCH64_FEATURES: &[(u8, u8, &str)] = &[
    (0, 0, "fp"),
    (0, 1, "asimd"),
    (0, 2, "evtstrm"),
    (0, 3, "aes"),
    (0, 4, "pmull"),
    (0, 5, "sha1"),
    (0, 6, "sha2"),
    (0, 7, "crc32"),
    (0, 8, "atomics"),
    (0, 9, "fphp"),
    (0, 10, "asimdhp"),
    (0, 11, "cpuid"),
    (0, 12, "asimdrdm"),
    (0, 13, "jscvt"),
    (0, 14, "fcma"),
    (0, 15, "lrcpc"),
    (0, 16, "dcpop"),
    (0, 17, "sha3"),
    (0, 18, "sm3"),
    (0, 19, "sm4"),
    (0, 20, "asimddp"),
    (0, 21, "sha512"),
    (0, 22, "sve"),
    (0, 23, "asimdfhm"),
    (0, 24, "dit"),
    (0, 25, "uscat"),
    (0, 26, "ilrcpc"),
    (0, 27, "flagm"),
    (0, 28, "ssbs"),
    (0, 29, "sb"),
    (0, 30, "paca"),
    (0, 31, "pacg"),
    (0, 32, "gcs"),
    (0, 33, "cmpbr"),
    (0, 34, "fprcvt"),
    (0, 35, "f8mm8"),
    (0, 36, "f8mm4"),
    (0, 37, "svef16mm"),
    (0, 38, "sveeltperm"),
    (0, 39, "sveaes2"),
    (0, 40, "svebfscale"),
    (0, 41, "sve2p2"),
    (0, 42, "sme2p2"),
    (0, 43, "smesbitperm"),
    (0, 44, "smeaes"),
    (0, 45, "smesfexpa"),
    (0, 46, "smestmop"),
    (0, 47, "smesmop4"),
    (1, 0, "dcpodp"),
    (1, 1, "sve2"),
    (1, 2, "sveaes"),
    (1, 3, "svepmull"),
    (1, 4, "svebitperm"),
    (1, 5, "svesha3"),
    (1, 6, "svesm4"),
    (1, 7, "flagm2"),
    (1, 8, "frint"),
    (1, 9, "svei8mm"),
    (1, 10, "svef32mm"),
    (1, 11, "svef64mm"),
    (1, 12, "svebf16"),
    (1, 13, "i8mm"),
    (1, 14, "bf16"),
    (1, 15, "dgh"),
    (1, 16, "rng"),
    (1, 17, "bti"),
    (1, 18, "mte"),
    (1, 19, "ecv"),
    (1, 20, "afp"),
    (1, 21, "rpres"),
    (1, 22, "mte3"),
    (1, 23, "sme"),
    (1, 24, "smei16i64"),
    (1, 25, "smef64f64"),
    (1, 26, "smei8i32"),
    (1, 27, "smef16f32"),
    (1, 28, "smeb16f32"),
    (1, 29, "smef32f32"),
    (1, 30, "smefa64"),
    (1, 31, "wfxt"),
    (1, 32, "ebf16"),
    (1, 33, "sveebf16"),
    (1, 34, "cssc"),
    (1, 35, "rprfm"),
    (1, 36, "sve2p1"),
    (1, 37, "sme2"),
    (1, 38, "sme2p1"),
    (1, 39, "smei16i32"),
    (1, 40, "smebi32i32"),
    (1, 41, "smeb16b16"),
    (1, 42, "smef16f16"),
    (1, 43, "mops"),
    (1, 44, "hbc"),
    (1, 45, "sveb16b16"),
    (1, 46, "lrcpc3"),
    (1, 47, "lse128"),
    (1, 48, "fpmr"),
    (1, 49, "lut"),
    (1, 50, "faminmax"),
    (1, 51, "f8cvt"),
    (1, 52, "f8fma"),
    (1, 53, "f8dp4"),
    (1, 54, "f8dp2"),
    (1, 55, "f8e4m3"),
    (1, 56, "f8e5m2"),
    (1, 57, "smelutv2"),
    (1, 58, "smef8f16"),
    (1, 59, "smef8f32"),
    (1, 60, "smesf8fma"),
    (1, 61, "smesf8dp4"),
    (1, 62, "smesf8dp2"),
    (1, 63, "poe"),
];

/// ARMv7-A's `Features` names for `AT_HWCAP`, by bit.
pub(super) const ARM_HWCAP: &[&str] = &[
    "swp",
    "half",
    "thumb",
    "26bit",
    "fastmult",
    "fpa",
    "vfp",
    "edsp",
    "java",
    "iwmmxt",
    "crunch",
    "thumbee",
    "neon",
    "vfpv3",
    "vfpv3d16",
    "tls",
    "vfpv4",
    "idiva",
    "idivt",
    "vfpd32",
    "lpae",
    "evtstrm",
    "fphp",
    "asimdhp",
    "asimddp",
    "asimdfhm",
    "asimdbf16",
    "i8mm",
];

/// ARMv7-A's `Features` names for `AT_HWCAP2`, by bit.
pub(super) const ARM_HWCAP2: &[&str] = &["aes", "pmull", "sha1", "sha2", "crc32", "sb", "ssbs"];
