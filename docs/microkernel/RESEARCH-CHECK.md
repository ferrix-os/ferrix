# M0 part E: the research of 2026-10-13, checked against its sources

Checked 2026-10-13 against `docs/MICROKERNEL.md` §6.5, §7 and *Sources*
(unchanged between `2f1a43db2` and `a21835fbb`). Web research only: no
build, no code change, the Pixel 7 not touched. Every verdict cites the page
it was read on; a quoted phrase is the source's own words. **Confirmed**
means the source says what the plan says; **corrected** means the source
says something different or more precise, and the fix is listed at the end;
**unverified** means no public source was found, and the plan must keep it
marked as such.

**Count: 56 claims; 42 confirmed (several with a precision or a better
source), 9 corrected, 5 unverified.** The text fixes for `MICROKERNEL.md`
are in §11; the Ferrocene route in §9; the standards to buy in §10.

§7's figures are the tree's own measurements (gate runs, the consultant's
measurements on nazuna, seL4 run on nazuna) and the customer's targets; none
of them rests on a web source, so §7 has no row here. The DO-330 row comes
from the *Sources* list.

## 1. Common Criteria: ADV_SPM.1 covers the entire TSF

| Claim | Verdict | Source | Note |
|---|---|---|---|
| Under CC:2022, ADV_SPM.1 requires a formal model of the **entire TSF** (§6.5, F1) | confirmed | [JIL, *ADV_SPM.1 interpretation for [CC:2022] transition*, v1.0, May 2024](https://www.ipa.go.jp/en/security/jisec/g6ovkg00000067p3-att/5_JIL-SPM-Interpretation-v1.0.pdf), ¶1; [CC:2022 Part 3 R1](https://www.commoncriteriaportal.org/files/ccfiles/CC2022PART3R1.pdf), ADV_SPM.1.3C | JIL ¶1: "In [CC:2022], the evaluation of formal models according to ADV_SPM.1 requires the formal modelling of the entire TSF." The primary text, Part 3 ADV_SPM.1.3C: "The formal model shall cover the complete set of SFRs that define the TSF"; 1.4C: "The formal properties shall cover the complete set of security objectives for the TOE." Two limits the plan should state: (a) JIL's interim relief (modelling only sub-TSFs) applies **only** to ICs and Java Card under PP-0084, PP-0099 and PP-0101 (JIL ¶3, ¶7), so not to Ferrix; (b) CC:2022 itself allows a **multi-assurance** ST in which ADV_SPM.1 applies to named sub-TSFs only, each "entirely modelled" (JIL ¶1). The plan may cite Part 3 as the primary source and JIL as the interpretation. |
| EAL6 requires ADV_SPM.1; EAL7 adds formal functional specification and design (ADV_FSP.6, ADV_TDS.6) | corrected (wording) | [CC:2022 Part 5 R1](https://www.commoncriteriaportal.org/files/ccfiles/CC2022PART5R1.pdf), Tables 7 and 8 | EAL6 (Table 7) lists "ADV_SPM.1 Formal TOE security model policy", ADV_FSP.5, ADV_TDS.5. EAL7 (Table 8): "ADV_FSP.6 Complete semi-formal functional specification with additional formal specification" and "ADV_TDS.6 Complete semi-formal modular design with formal high-level design presentation". So EAL7's design is formal at the **high level** only, not throughout. |
| EAL6's independence-related SARs: ALC_DVS.2, ALC_CMC.5, ATE_DPT.3, AVA_VAN.5 (§6.5 table) | confirmed | [CC:2022 Part 5 R1](https://www.commoncriteriaportal.org/files/ccfiles/CC2022PART5R1.pdf), Table 7 | All four are in Table 7. |
| The CC:2022 texts are free | confirmed | [commoncriteriaportal.org/cc](https://www.commoncriteriaportal.org/cc/index.cfm) | Parts 1 to 5 and CEM:2022, all R1, free PDFs; the page says the errata/interpretation document v1.2 ([CCMB-2025-001](https://www.commoncriteriaportal.org/files/ccfiles/CCMB-2025-001-v1_2-Errata_Interpretation_CC_CEM_2022.pdf)) is the one to apply. |

## 2. Arm Cortex-A78AE

| Claim | Verdict | Source | Note |
|---|---|---|---|
| Armv8.2-A, no SMT | confirmed | [A78AE TRM 101779_0002_07, r0p2](https://documentation-service.arm.com/static/62502fb164e3265f7c90b120), §1 and MPIDR_EL1 | "Full implementation of the Arm®v8.2-A A64, A32, and T32 instruction sets"; Aff0: "The Cortex®-A78AE core is single-threaded, so this field has the value 0x00." |
| Lock-mode chosen by CEMODE at cluster (cold) reset; CEMODE 0b11 | confirmed | same TRM, §1.2 and §2.11.1 | "the ability to choose a mode based on the DynamIQ Shared Unit AE (DSU-AE) CEMODE input signal during cluster cold reset"; "the DSU-AE Lock-mode operation, that is, where CEMODE = 0b11" (Split 0b01, Hybrid 0b10). |
| MPIDR_EL1.MT reads 1 although the core is not multithreaded | confirmed | same TRM, §3.2.111 | MT [24] = 1: "Affinity0 represents threads. Cortex®-A78AE is not multithreaded, but may be in a system with other cores that are multithreaded." |
| No MPAM | confirmed (by register) | same TRM, §3.2.87 | The TRM never names MPAM; ID_AA64PFR0_EL1 bits [55:32], which hold the MPAM field [43:40], are "RES0", so MPAM is not implemented. |
| The shared L3 is partitioned only through CLUSTERPARTCR_EL1 | corrected | same TRM, register summary; [DSU-AE TRM 101322_0101_04](https://documentation-service.arm.com/static/5f73673b1b758617cd959dbc), §A5.3 | The A78AE TRM only lists CLUSTERPARTCR_EL1 ("Cluster partition control register"); the mechanism is in the DSU-AE TRM: "a partitioning scheme that alters the victim selection policy", groups of four ways assigned to "eight partition scheme IDs", "Up to four private L3 cache partitions can be created." The scheme ID of each core comes from CLUSTERTHREADSID_EL1 (with CLUSTERACPSID/CLUSTERSTASHSID for ACP and stash). Partitioning controls **allocation** only (victim selection), which matters for the timing claim. |
| A lockstep mismatch is reported to the SoC, which owns the fault reaction | confirmed | A78AE TRM §2.11.1 | Faults are aggregated by the CPU bridge and reported "with redundancy to the DSU-AE"; §2.11.1.7: "The Cortex®-A78AE processor does not include any specific features to react to a fault detected by the lock-step mechanism ... The system integrator might choose to reset the system". Also §2.11.1.6: "Errors that are detected in Lock-mode cannot be contained" and the TRM says the fault reaction time cannot be quantified. Both belong in the AoUs. |
| Arm rates Lock-mode ASIL D / SIL 3 (hardware metrics) | confirmed; source corrected | [Arm, Cortex-A78AE product page](https://www.arm.com/products/silicon-ip-cpu/cortex-a/cortex-a78ae) | "lock mode for advanced multicore fault-tolerance with ASIL D/SIL 3 hardware metrics". The plan's cited [Arm blog](https://developer.arm.com/community/arm-community-blogs/b/automotive-blog/posts/functional-safety-compute) (M. Rao, 2024-03-13) is about DSU-120AE-generation cores and does not name the A78AE or SIL 3. |
| Arm rates Hybrid mode ASIL B | confirmed, add SIL 2 | Arm product page, as above | "hybrid mode to deliver higher compute availability ... to help achieve ASIL B/SIL 2 safety requirements." |
| A78AE MIDR | confirmed (new fact) | A78AE TRM register summary | MIDR_EL1 reset value 0x410FD422 (r0p2), PartNum 0xD42. Distinct from the plain A78's 0xD41 (§6). |

## 3. seL4

| Claim | Verdict | Source | Note |
|---|---|---|---|
| seL4's AArch64 security (integrity and confidentiality) proofs completed in 2026 | confirmed | [seL4 forum, 2026-08-24](https://sel4.discourse.group/t/sel4-security-proofs-now-complete-on-aarch64/1074) | "Proofcraft has now established the proof that seL4 enforces confidentiality on AArch64", after "completing the proofs of functional correctness and integrity"; it "completes the formal proof that the seL4 implementation code on AArch64 enforces security isolation". The post does not say which configuration (hypervisor, MCS); it points to the proofs' assumptions page. |
| About 25 person-years for 8,700 lines of C | confirmed, with precision | [Trustworthy Systems, l4.verified numbers](https://trustworthy.systems/projects/OLD/l4.verified/numbers) | "8,700 lines of C code plus 600 lines of ARM assembly"; 7,500 lines of C verified; "about 25 person years" covers design, documentation, implementation and verification at the 2009 completion; 200,000 lines of Isabelle. Atmosphere's paper gives the narrower figure: seL4 "required 11 person-years (an additional 9 person-years were needed for the development of formal language frameworks, proof tools, and libraries)". |
| Proofcraft did the AArch64 security proofs | confirmed | seL4 forum post, as above | |
| Proofcraft works with Verus in DARPA INSPECTA | corrected | [INSPECTA slides, seL4 Summit 2024](https://www.sel4.systems/Summit/2024/slides/inspecta.pdf); [seL4 Summit 2025 abstracts](https://sel4.systems/Summit/2025/abstracts2025.html); [proofcraft.systems](https://proofcraft.systems/) | Proofcraft is a member of INSPECTA (led by Collins Aerospace) and does its **seL4** verification; the Verus contract generation in INSPECTA is Kansas State's HAMR. No source found showing Proofcraft doing Verus work; its own site names only seL4. |

## 4. Ferrocene

| Claim | Verdict | Source | Note |
|---|---|---|---|
| Ferrocene 26.05 = Rust 1.95 | confirmed | [Ferrocene 26.05.0 blog](https://ferrous-systems.com/blog/ferrocene-26-05-0/) | "includes changes from Rust 1.93.0, 1.93.1, 1.94.0, 1.94.1 and 1.95.0"; published 2026-07-28. **New:** 26.08 is in preparation ("upcoming release", Rust 1.96 to 1.97, [release notes source](https://github.com/ferrocene/ferrocene/blob/main/ferrocene/doc/release-notes/src/26.08.0.rst)); on release it reaches the tree's 1.97 pin. |
| Qualified ISO 26262 ASIL D | confirmed | [ferrocene.dev](https://ferrocene.dev/); 26.05 blog | "TÜV SÜD-qualified ... ISO 26262 (ASIL D)". |
| Qualified IEC 61508 SIL 3, "supports" SIL 4 | confirmed | ferrocene.dev; 26.05 blog | "IEC 61508 (SIL 3 with support for customer certification toward SIL 4)". Note: the repository's [README](https://github.com/ferrocene/ferrocene/blob/main/README.md) still says "IEC 61508 (SIL 4)"; the vendor's site and release post are the ones to cite, and F-17 should ask which is current. |
| Qualified IEC 62304 Class C | confirmed | ferrocene.dev; 26.05 blog | "IEC 62304 (Class C)". |
| No DO-178C qualification | corrected | [26.05 blog](https://ferrous-systems.com/blog/ferrocene-26-05-0/) | Still no DO-178C/DO-330 qualification, but the vendor now says it "supports customer certification efforts toward assurance levels IEC 61508 SIL 4 and DO-178C DAL C". DAL C only, so the DAL A objectives in §6.5 still need F-17. |
| Certified `core` subset | confirmed (new fact) | ferrocene.dev; 26.05 blog | "A subset of the `core` library ... is certified to IEC 61508 (SIL 2) and ISO 26262 (ASIL B)"; 26.05 adds `core::fmt`. Below the kernel's SC 4 / ASIL D, which the plan should note where it relies on `core`. |
| `aarch64-unknown-none` qualified | confirmed, with condition | [targets, release/1.95 branch](https://github.com/ferrocene/ferrocene/blob/release/1.95/ferrocene/doc/user-manual/src/targets/index.rst); [public-docs (main)](https://public-docs.ferrocene.dev/main/user-manual/targets/index.html) | "Only qualified when cross-compiled from x86-64 Linux (glibc)". 26.05 lets it take `-C target-cpu=cortex-a53` in place of the retired `aarch64-a53-none`. |
| `x86_64-unknown-none` not offered | confirmed, with a question | same | Not in 26.05's target list at any level. But the 26.05 release notes say the retired `x86_64-ferrocene-none` "can be replaced with `x86_64-unknown-none`", so the vendor ships or plans something for it; F-17 asks. The only qualified x86-64 targets are `x86_64-unknown-linux-gnu` (host) and `x86_64-pc-nto-qnx710`. |
| `armv7a-none-eabi` not offered | confirmed | same | No Armv7-A target at any level. Armv7-R (`armv7r-none-eabihf`) is "Supported" in 26.05 and qualified in 26.08. |
| The cited targets page is 26.05's | corrected (source) | public-docs (main) vs release/1.95 | `public-docs.ferrocene.dev/main` tracks the **main** branch (already listing `armv7r-none-eabihf` as qualified), not 26.05. Cite the release's own docs. |

## 5. Verus and Atmosphere

| Claim | Verdict | Source | Note |
|---|---|---|---|
| Verus pins rustc 1.98.1 | confirmed | [rust-toolchain.toml](https://raw.githubusercontent.com/verus-lang/verus/main/rust-toolchain.toml) | `channel = "1.98.1"`, set by commit "Use Rust 1.98.1 (#2909)", 2026-09-08. A moving pin: record the Verus commit with it. |
| Atmosphere: about 3.3 lines of proof per line of code | confirmed | [Atmosphere, SOSP 2025](https://mars-research.github.io/doc/2025-sosp-atmo.pdf) | "a proof-to-code ratio of 3.32:1"; 6K lines of executable code, 20.1K lines of proof (14.3K specification, 5.8K hints and proofs). |
| Atmosphere: 1.5 person-years for 6,000 lines | confirmed | same | "only one and a half person-years was spent on the development of the verified parts"; about 2.5 person-years in all. |
| Atmosphere has capabilities, IPC, address spaces, IOMMU, refinement and noninterference | confirmed | same | Functional correctness as "refinement of a high-level specification"; "The noninterference theorem guarantees that any arbitrary system call from any of the isolated containers cannot change the observable state of another." The kernel runs under one big lock, which the plan should note against Ferrix's SMP kernel. |
| Verus's trusted base includes about 700 lines of axioms | unverified | none found | No public source located; keep it marked or drop it. |

## 6. Pixel 7 (Tensor G2)

| Claim | Verdict | Source | Note |
|---|---|---|---|
| Tensor G2 has Cortex-A78 cores | confirmed | [XDA, Tensor G2](https://www.xda-developers.com/google-tensor-g2/); [Notebookcheck](https://www.notebookcheck.net/Google-Tensor-G2-Chipset-details-revealed-following-Pixel-7-and-Pixel-7-Pro-launches.660056.0.html) | "2x Cortex-X1 @ 2.85GHz", "2x Cortex-A78 @ 2.35GHz", "4x Cortex-A55 @ 1.8GHz". The A78s are the **middle** cluster. Google's own spec page names only "Google Tensor G2"; the core types come from launch reporting. |
| Which logical CPUs are the A78s | unverified | none found | By cluster order the A78s should be cpu4 and cpu5 (A55 cpu0 to 3, X1 cpu6 and 7); no public gs201 device tree was found to confirm. The on-device read settles it. |
| MIDR_EL1 of the A78: implementer 0x41, part 0xD41 | confirmed | [Linux `arch/arm64/include/asm/cputype.h`](https://github.com/torvalds/linux/blob/master/arch/arm64/include/asm/cputype.h) | `ARM_CPU_IMP_ARM 0x41`, `ARM_CPU_PART_CORTEX_A78 0xD41`. Expect `0x41?FD41?` (variant and revision unknown until read). The others: X1 0xD44, A55 0xD05. The A78AE is 0xD42, so the phone is a proxy for barrier, CSV2 and timing work only, with no Split-Lock. |

## 7. AMD Versal AI Edge Gen 2 / VEK385

amd.com's own pages timed out on every automated fetch; quotes marked
*(search summary)* were read from the search engine's copy of the page and
should be re-read in a browser before they are quoted onward. The boot-tool
findings were read in the source code (two of them re-read for this record).

| Claim | Verdict | Source | Note |
|---|---|---|---|
| 8 Cortex-A78AE in four two-core clusters | confirmed, with caveat | [AM026 TRM, APU](https://docs.amd.com/r/en-US/am026-versal-ai-edge-prime-gen2-trm/Application-Processing-Unit); [selection guide XMP505](https://docs.amd.com/api/khub/documents/N_GW1BBysxl9Szx5Xxvb7A/content) | "Up to four clusters, 2 cores each"; "1 MB L3 Cache (per 2-core cluster)". Smaller parts (2VE3304/3504/3804) have 4 APU cores. The VEK385's device is the XC2VE3858. *Consequence (inference):* in lock-step each cluster is one logical core with its own L3, so §6.5's L3-partitioning fact matters on this board only in split or hybrid mode. |
| Documented per-cluster lock-step | confirmed | [AM026, Lockstep CPU cores](https://docs.amd.com/r/en-US/am026-versal-ai-edge-prime-gen2-trm/Application-Processing-Unit); [UG1304](https://docs.amd.com/r/2025.2-English/ug1304-versal-acap-ssdg/Self-Suspending) | "The two cores within a cluster can optionally be configured in lock-step mode"; UG1304: "APUs also can run independently (split mode) or in fault tolerant mode (lockstep)". That AMD's lock-step is Arm's DSU-AE Lock-mode (CEMODE 0b11) rather than Hybrid is **inferred**, not stated: M1 must confirm it. |
| How lock-step is configured in AMD's boot tools | corrected (answered from source; undocumented) | [bootgen `bif.y`/`bif.l`](https://github.com/Xilinx/bootgen); [PLM `xloader_plat.c`, xlnx_rel_v2026.1](https://raw.githubusercontent.com/Xilinx/embeddedsw/xlnx_rel_v2026.1/lib/sw_services/xilloader/src/versal_2ve_2vm/server/xloader_plat.c); `xilpm_ng` `xpm_apucore.c`, `xpm_ioctl.c` (same tag) | **Not Vivado, not a CDO.** A per-partition BIF attribute in the boot image: `core=a78-N`, `cluster=<n>`, `lockstep`. Bootgen writes it into the partition header (lock-step at bit 4, cluster at bit 29); the PLM reads `XilPdi_GetClusterLockstep(PrtnHdr)`, picks `XPM_APU_MODE_LOCKSTEP` or `XPM_APU_MODE_SPLIT`, and calls `XPm_ApuSetOperMode(DeviceId, Mode)`, which sets the cluster's bit in `FPX_SLCR_APU_CTRL`; in lock-step "only core 0 of each cluster is valid". A runtime path exists (`IOCTL_SET_APU_OPER_MODE`). [UG1283](https://docs.amd.com/r/en-US/ug1283-bootgen-user-guide/Attributes) (2026.1) documents neither `cluster` nor `lockstep` for A78; [PG450](https://docs.amd.com/r/en-US/pg450-processing-system-wizard) shows no APU lock-step option. |
| AMD says ASIL D "certified" for the APU | confirmed that AMD wrote it *(search summary)* | [AMD ADAS solution brief](https://www.amd.com/content/dam/amd/en/documents/solutions/automotive/coo-automotive-solution-brief.pdf) | "Certified to ISO 26262 ASIL D Standards, on APU and RPU with lock-step". The Chinese industrial brief also says "获得相关 ASIL D 和 SIL 3 认证" ("has obtained the relevant ASIL D and SIL 3 certification"). |
| AMD says ASIL D "targeted" | confirmed *(search summary)* | [Gen 2 automotive brief](https://www.amd.com/content/dam/amd/en/documents/products/adaptive-socs-and-fpgas/versal/versal-ai-edge-gen2-automotive-solution-brief.pdf); [Gen 2 industrial brief](https://www.amd.com/content/dam/amd/en/documents/products/adaptive-socs-and-fpgas/versal/versal-ai-edge-gen2-industrial-solution-brief.pdf) | APU "targeted for ASIL B (split mode)/ ASIL D (lock-step)"; industrial: ASIL D / SIL 3 "certification targeted". The Gen 2 infographic cites "pre-silicon functional safety targets". **No assessor or certificate for Gen 2 found.** Use "targeted" until AMD shows a certificate. |
| Safety manual via the Functional Safety Lounge (NDA) | corrected | [AMD functional safety page](https://www.amd.com/en/products/adaptive-socs-and-fpgas/technologies/functional-safety.html) *(search summary)*; [Electropages, 2022](https://www.electropages.com/2022/11/first-adaptive-soc-fully-certified-automotive-functional-safety-applications) | The Lounge is a **paid subscription** ("For paid subscribers only"; "purchasing access to the company's online Functional Safety Lounges"; order code EM-DI-SAFETY-SITE, search summary), not an NDA site; the NDA site is the Design Security Lounge (security manual UG1508). No public source says a Versal Gen 2 safety manual exists in the Lounge yet: **unverified**; ask AMD. |
| SMMUv3 | confirmed | [AMD wiki, "Versal and Versal AI Edge Series Gen 2 SMMU Usage Guidance"](https://xilinx-wiki.atlassian.net/wiki/rest/api/content/3720478747?expand=body.storage) | Device tree `compatible = "arm,smmu-v3"` at 0xec000000; boot log "arm-smmu-v3 ec000000.iommu: ias 48-bit, oas 48-bit" (Gen 1 had MMU-500, SMMUv2). |
| GICv3 with ITS | corrected | [UG1304, APU subsystem](https://docs.amd.com/r/en-US/ug1304-versal-acap-ssdg/APU-Subsystem); [TF-A versal2 platform.mk](https://raw.githubusercontent.com/ARM-software/arm-trusted-firmware/master/plat/amd/versal2/platform.mk); [QEMU docs](https://qemu.readthedocs.io/en/master/system/arm/xlnx-versal-virt.html) | "GIC-600 interrupt controller" (GICv3; TF-A sets `GICV3_SUPPORT_GIC600 := 1`). An ITS is confirmed only by QEMU's model ("8 ACPUs (Arm Cortex-A78AE) with their GICv3 and ITS"); the silicon's ITS needs the TRM read in full. Not GIC-600AE as far as found. |
| About $16,000 | confirmed | [AMD VEK385 page](https://www.amd.com/en/products/adaptive-socs-and-fpgas/evaluation-boards/vek385.html) *(search summary)*; [CNX Software, 2026-02-23](https://www.cnx-software.com/2026/02/23/amd-vek385-versal-ai-edge-gen-2-fpga-evaluation-kit-plugs-directly-into-a-pcie-gen5-gen4-slot/); [DigiKey EK-VEK385-G](https://www.digikey.com/en/products/detail/amd/EK-VEK385-G/29285567) | AMD: "Price: $15,995.00 Part Number: EK-VEK385-G Lead Time: 16 weeks"; DigiKey: $17,658 for one. |
| 10 to 16 weeks' lead | corrected (range) | as above; Mouser *(search summary)* | AMD 16 weeks; CNX (Feb 2026) 10 weeks; DigiKey "In-Stock: 27" with a 12-week standard lead; Mouser "Factory Lead-Time: 23 Weeks". So **in stock at DigiKey today**, otherwise 12 to 23 weeks. Orderable (general access, DigiKey status "Active"). |
| Upstream TF-A | confirmed | [TF-A, AMD Versal Gen 2](https://trustedfirmware-a.readthedocs.io/en/latest/plat/amd-versal2.html) | `PLAT=versal2`, `RESET_TO_BL31=1`; "The platform only uses the runtime part of TF-A". |
| U-Boot with UEFI | confirmed for the QEMU config only | [U-Boot `amd_versal2_virt_defconfig`](https://raw.githubusercontent.com/u-boot/u-boot/master/configs/amd_versal2_virt_defconfig) | EFI options set (`CONFIG_EFI_HTTP_BOOT=y`, `CONFIG_CMD_EFIDEBUG=y`); no VEK385 board defconfig found upstream. AMD's own U-Boot fork for the board was not checked. |
| Public TRM | confirmed | [AM026](https://docs.amd.com/r/en-US/am026-versal-ai-edge-prime-gen2-trm) | Revision 1.4, 2026-05-29. |
| QEMU model | confirmed | [QEMU docs](https://qemu.readthedocs.io/en/master/system/arm/xlnx-versal-virt.html) | Upstream machine `amd-versal2-virt`; "QEMU does not yet model any other devices, including the PL and the AI Engine". |

## 8. The other sourced claims in §6.5

| Claim | Verdict | Source | Note |
|---|---|---|---|
| DO-330 Criterion 3 tools are TQL-5 at every level | confirmed | [LDRA, DO-330 briefing v2.0](https://ldra.com/wp-content/uploads/ldra/DO-330-Test-Tool-Qualification-Technical-Briefing-v2.0.pdf) | "Irrespective of the application DAL, such a tool is always assigned Tool Qualification Level 5". The criteria themselves are DO-178C §12.2; the clause check should confirm against the purchased text whether Verus used **in place of** tests under DO-333 stays Criterion 3 or becomes Criterion 2 (TQL-4 at Level A). |
| NVIDIA Orin: Lock-mode unsupported by NVIDIA's software | confirmed, with precision | [Jetson AGX Orin adaptation guide, r35.2.1](https://docs.nvidia.com/jetson/archives/r35.2.1/DeveloperGuide/_sources/text/HR/JetsonModuleAdaptationAndBringUp/JetsonAgxOrinSeries.rst.txt) | "Booting the standard/default Jetson Linux in lock/hybrid mode is not supported." Lock-mode can be set in MB1's BCT (`num_clusters_2core_pair_lockstep`), so the hardware path exists. |
| Orin's IOMMU is an SMMUv2; the SoC is rated ASIL B for random faults | unverified | none found | NVIDIA's [Jetson safety page](https://developer.nvidia.com/embedded/jetson/jetson-safety) says the Jetson Safety Extension Package is discontinued; no public rating found. |
| NXP S32G has no SMMU | unverified | [NXP community thread](https://community.nxp.com/t5/S32G/S32G274A-SMMU-Support-and-Configuration/m-p/2230972) | The cited thread refuses automated access (HTTP 403); not re-read. |
| AWS's Nitro Isolation Engine is proved in Isabelle/HOL, with noninterference | confirmed | [Amazon Science, 2026-06-10](https://www.amazon.science/blog/ec2s-formally-verified-isolation-engine-provides-mathematical-assurance-of-virtual-machine-isolation) | "the Isabelle/HOL (higher-order logic) proof assistant"; written in Rust; confidentiality and integrity by noninterference; the infrastructure was "open-sourced in 2025 as the AutoCorrode library". |
| AutoCorrode's µRust lacks traits and dynamic dispatch | unverified | [AutoCorrode](https://github.com/awslabs/AutoCorrode) | The README calls µRust "the Rust-like language µRust" and states no limitations; the trait/dispatch gap needs a source. |
| Kansas State with Collins (HAMR, DO-333) | confirmed in part | [seL4 Summit 2025 abstracts](https://sel4.systems/Summit/2025/abstracts2025.html); [INSPECTA slides](https://www.sel4.systems/Summit/2024/slides/inspecta.pdf) | Kansas State's HAMR generates Verus contracts in INSPECTA, led by Collins. That Collins "helped write DO-333" was not re-checked. |

## 9. Ferrocene: how a project gets a toolchain

**Short answer: the qualified binaries are not free; the source is.**

| Question | Answer | Source |
|---|---|---|
| Can the compiler be downloaded without paying? | **No** for binaries. `releases.ferrocene.dev` redirects to a login at `customers.ferrocene.dev` (HTTP 302 to `/auth/static`); the repository says "Prebuilt Ferrocene binaries are available for customers and partners ... after logging in with your Ferrocene account." | [ferrocene/ferrocene README](https://github.com/ferrocene/ferrocene/blob/main/README.md); fetch of `https://releases.ferrocene.dev/` on 2026-10-13 |
| Cheapest paid route | **Individual**: "€25" "per month/seat"; €240 a year per user in the 25.05 announcement. Includes "Full access to all Rust high-assurance compilers", e-mail support, "2 years of patch releases for select versions". | [ferrocene.dev/pricing](https://ferrocene.dev/pricing/); [25.05 blog](https://ferrous-systems.com/blog/ferrocene-25-05-0/) |
| What the Individual plan lacks | "Qualification signatures and assessor reports", certification assistance and target enablement are in **Enterprise** ("Custom" price). A certification needs the signed material, so the assessed build needs Enterprise; the M0 CI build does not. | [ferrocene.dev/pricing](https://ferrocene.dev/pricing/) |
| Licence | Compiler source and the qualification documents' source: "MIT OR Apache-2.0". criticalup: MIT OR Apache-2.0. | [ferrocene.dev](https://ferrocene.dev/); [open-sourcing post](https://ferrous-systems.com/blog/ferrocene-open-source/); [criticalup](https://github.com/ferrocene/criticalup) |
| Is a self-built compiler qualified? | No: the qualification is tied to the vendor's build infrastructure and the signed binaries ("the pre-built binaries and packages of our qualified compilers"). | [open-sourcing post](https://ferrous-systems.com/blog/ferrocene-open-source/) |

**Route A, the vendor's binaries** (needs a paid seat), for `aarch64-unknown-none`
on an x86-64 Linux (glibc) host, which is the only host on which that target
is qualified. From the vendor's training material
([installing Ferrocene](https://rust-training.ferrous-systems.com/latest/book/ferrocene-installing)):

1. Buy an Individual seat (€25 a month) at ferrocene.dev; log in at `customers.ferrocene.dev`.
2. Install criticalup ([criticalup.ferrocene.dev](https://criticalup.ferrocene.dev/), or a [GitHub release](https://github.com/ferrocene/criticalup/releases)).
3. Make a token at `https://customers.ferrocene.dev/users/tokens`; store it with `criticalup auth set`. The token is "only required to _download_ a toolchain".
4. Put a `criticalup.toml` in the project:
   ```toml
   manifest-version = 1
   [products.ferrocene]
   release = "stable-26.05.0"
   packages = [
     "rustc-${rustc-host}", "rust-std-${rustc-host}", "cargo-${rustc-host}",
     "rust-src", "rust-std-aarch64-unknown-none"
   ]
   ```
5. `criticalup install`; then build with the installed toolchain (criticalup's
   "Running Tools" chapter; the exact `criticalup run` form was not read and is
   **unverified**).

**Route B, no subscription**: build from the open repository. Clone
`https://github.com/ferrocene/ferrocene`, branch `release/1.95` (26.05's Rust;
the branch exists), with `llvm.download-ci-llvm = false` and
`rust.download-ci-rustc = false` in `config.toml` (the vendor's note: "If you
aren't part of the Ferrocene team, you can still build it" that way,
[setup-local-env](https://github.com/ferrocene/ferrocene/blob/main/ferrocene/doc/internal-procedures/src/setup-local-env.rst)),
then the usual rustc bootstrap with `aarch64-unknown-none` among the targets
(`./x.py build` / `./x.py dist`; the exact invocation is **unverified**).
Cost: none, but a full LLVM build and no qualification. **For M0's CI build
of `core` and `item`, whose purpose is only to stop newer language features,
route B (or even upstream rustc 1.95.0) is enough**; route A is needed for
the evidence build, and Enterprise for the signed qualification material.

## 10. The standards' texts to buy

Prices read 2026-10-13. Rates used: 1 CHF ≈ €1.07; $1 ≈ €0.86; 1 SEK ≈
€0.091. "Seen" means read on the store's page; the IEC 61508-3, EVS EN 50716
and RTCA DO-178C/DO-330 prices were each re-read for this record. EVS
(Estonia) and SIS (Sweden) sell the European adoptions, whose text is the
IEC/ISO text with a European foreword; the clause numbers are the same.
EVS prices include VAT; EUROCAE prices exclude it. ISO's own store refused
automated access, so its prices come from ISO's `committee.iso.org` mirror of
the same product pages.

**What §6.5's table cites, and must be bought (M0's clause check):**

| Document | Edition | Official store, price | Cheaper legitimate store, price | Cited in §6.5 for |
|---|---|---|---|---|
| IEC 61508-3 | 2010 (Ed. 2.0), current | [IEC webstore](https://webstore.iec.ch/en/publication/5517): CHF 405 (≈ €433), seen | [EVS-EN 61508-3:2010](https://www.evs.ee/en/evs-en-61508-3-2010): €39.68, seen | Tables A.1, A.2, A.4, A.5, B.1, B.2; Annex D |
| ISO 26262-6 | 2018 (Ed. 2), current | [ISO](https://www.iso.org/standard/68388.html): CHF 225 (≈ €241), seen on mirror | SIS SS-ISO 26262-6:2018 (English): 1,599 SEK (≈ €145), seen by the price search | Tables 2, 6, 7, 8, 9 |
| ISO 26262-8 | 2018 (Ed. 2), current | [ISO](https://www.iso.org/standard/68390.html): CHF 225, seen on mirror | SIS: 1,599 SEK | tool confidence (Verus, MC/DC tool) |
| EN 50716 | 2023 (replaces EN 50128 and EN 50657) | CENELEC does not sell; [DIN EN 50716:2024-09](https://www.dinmedia.de/de/norm/din-en-50716/378381071) (German only): €197.92 excl. VAT | [EVS-EN 50716:2023](https://www.evs.ee/en/evs-en-50716-2023): €39.68, seen | Annex A |
| RTCA DO-178C | Dec 2011 | [RTCA store](https://my.rtca.org/nc__store?search=DO-178C): $525 PDF, $625 print, seen; errata PDF free | [EUROCAE ED-12C](https://www.eurocae.net/product/ed-12c-software-considerations-in-airborne-systems-and-equipment-certification/): €250 | 6.3.4f, 6.4.4.2b, Table A-7, §12.2 (tool criteria) |
| RTCA DO-330 | 2011 | RTCA store: $258 PDF, seen | [EUROCAE ED-215](https://www.eurocae.net/product/ed-215-software-tool-qualification-considerations/): €215 | TQL-5 |
| RTCA DO-333 | 2011 | [RTCA store](https://my.rtca.org/nc__store?search=DO-333): $258 PDF | [EUROCAE ED-216](https://www.eurocae.net/product/ed-216-formal-methods-supplement-to-ed-12c-and-ed-109a/): €215 | formal methods as an alternative means |
| CC:2022 Parts 2 and 3, CEM:2022 (with Parts 1, 4, 5 and errata v1.2) | Release 1 | [commoncriteriaportal.org](https://www.commoncriteriaportal.org/cc/index.cfm): **free** | — | ADV_SPM.1, ADV_FSP.6, ADV_TDS.6, ALC_DVS.2, ALC_CMC.5, ATE_DPT.3, AVA_VAN.5; EAL packages are in **Part 5** |

**Also cited by §6.5, worth buying for the same check:**

| Document | Edition | Official store, price | Cheaper store, price | Cited for |
|---|---|---|---|---|
| IEC 61508-1 | 2010 (Ed. 2.0) | [IEC](https://webstore.iec.ch/en/publication/5515): CHF 380 (≈ €407) | [EVS](https://www.evs.ee/en/evs-en-61508-1-2010): €32.24 | §8, assessment independence |
| IEC 61508-7 | 2010 (Ed. 2.0) | [IEC](https://webstore.iec.ch/en/publication/5521): CHF 430 (≈ €460) | [EVS](https://www.evs.ee/en/evs-en-61508-7-2010): €39.68 | the techniques Part 3's tables name |
| ISO 26262-2 | 2018 (Ed. 2) | [ISO](https://www.iso.org/standard/68384.html): CHF 196 (≈ €210) | none cheaper found (EVS €261.44) | confirmation measures, I3 |
| RTCA DO-297 | 2005 | [RTCA store](https://my.rtca.org/nc__store?search=DO-297): $255 PDF | (ED-124, not priced) | IMA platform |
| FAA AC 20-148 | 2004 | [FAA](https://www.faa.gov/documentLibrary/media/Advisory_Circular/AC_20-148.pdf): **free** | — | reusable software component |
| EASA AMC 20-193 (FAA AC 20-193) | AMC-20 Amdt 23, 2022 | EASA AMC-20: **free** | — | multicore interference |
| CAST-32A | Nov 2016 | [FAA](https://www.faa.gov/aircraft/air_cert/design_approvals/air_software/cast/cast-32a.pdf): **free** | — | superseded in practice by AMC/AC 20-193 |

IEC 62304 (Class C) is named in the targets but no clause of it is cited in
§6.5's table, so it is not on the list.

**Totals** (approximate, excluding VAT where the store does):

| Set | Official stores | Cheapest legitimate route |
|---|---|---|
| Minimum (61508-3, 26262-6, 26262-8, EN 50716, DO-178C, DO-330, DO-333; CC free) | CHF 855 + $1,041 + €39.68 ≈ **€1,850 ($2,160)** | EVS ×2, SIS ×2, EUROCAE ×3 ≈ **€1,050 ($1,230)** |
| Full (adds 61508-1, 61508-7, 26262-2, DO-297) | ≈ **€3,150 ($3,680)** | ≈ **€1,600 ($1,870)** |

RTCA and EUROCAE members get their documents free. Editions in progress, none
current yet: IEC 61508 Ed. 3, ISO 26262 Ed. 3, the DO-330/ED-215 revision.
Reported by the price search and **not re-read**: ISO/IEC 15408-2/-3 and
18045 were reissued as 2026 editions in May 2026; the plan should go on citing
CC:2022 R1, which is what the CC portal and the schemes publish.

## 11. Corrections for `MICROKERNEL.md`

Each item quotes the text as it stands (line numbers from `a21835fbb`) and
gives the replacement. Items 1 to 12 change a fact; 13 to 16 add a fact the
check found; 17 updates the *Sources* list.

1. **§6.5 table, Common Criteria column (l. 655).** "EAL7 adds formal
   functional specification and design (ADV_FSP.6, ADV_TDS.6)" →
   "EAL7 adds a formal functional specification (ADV_FSP.6) and a formal
   high-level design presentation (ADV_TDS.6)".
2. **§6.5 toolchain (l. 626).** "and offers no DO-178C qualification" →
   "and offers no DO-178C (DO-330) qualification; it "supports customer
   certification efforts toward" DO-178C DAL C, below the DAL A objectives
   the plan needs".
3. **§6.5 toolchain (l. 633).** "Today Ferrocene is at Rust 1.95" → "Today
   Ferrocene is at Rust 1.95 (26.05; 26.08, with Rust 1.96 to 1.97, is in
   preparation)".
4. **§6.5 F2 gaps (l. 716).** "Verus's trusted base includes about 700 lines
   of axioms." → "Verus's trusted base includes its axioms (about 700 lines:
   *unverified*, no source found)."
5. **§6.5 Who (l. 717-718).** "they did seL4's AArch64 security proofs and
   work with Verus in DARPA INSPECTA" → "they did seL4's AArch64 security
   proofs (completed August 2026) and do the seL4 verification in DARPA
   INSPECTA, whose Verus contracts are Kansas State's".
6. **§6.5 Effort (l. 724).** "seL4's about 25 person-years for 8,700 lines
   of C" → "seL4's about 25 person-years (design, implementation and proof)
   for 8,700 lines of C, 7,500 of them verified".
7. **§6.5 T1 (l. 733).** "(Arm rates it ASIL B)" → "(Arm rates it ASIL B /
   SIL 2)".
8. **§6.5 T1 (l. 736-737).** "the shared L3 is partitioned only through
   CLUSTERPARTCR_EL1" → "the shared L3 is partitioned only by the DSU-AE's
   way partitioning (CLUSTERPARTCR_EL1 with the per-core scheme IDs of
   CLUSTERTHREADSID_EL1: groups of four ways, at most four private
   partitions), which restricts allocation (victim selection), not lookup".
9. **§6.5 board (l. 740-742).** "GICv3 with ITS" → "a GIC-600 (GICv3; an ITS
   in QEMU's model, the silicon's to confirm in the TRM)"; "about $16,000,
   10 to 16 weeks' lead" → "$15,995 list (EK-VEK385-G), in stock at DigiKey
   on 2026-10-13 at $17,658, otherwise 12 to 23 weeks' lead"; add after
   "per-cluster lock-step": "(set per boot-image partition by Bootgen's
   `cluster=` and `lockstep` attributes and applied by the PLM; AMD's
   safety briefs say ASIL D "targeted" for the APU in lock-step, ASIL B in
   split mode)".
10. **§6.5 runner-up (l. 744).** "Lock-mode is unsupported by NVIDIA's own
    software" → "booting the standard Jetson Linux in Lock-mode is
    unsupported (NVIDIA's adaptation guide)"; and mark "its IOMMU is an
    SMMUv2 with quirks and the SoC is rated ASIL B for random faults" and
    "NXP's S32G is excluded (no SMMU)" as *unverified* (no public source
    re-read).
11. **§11 Q18 (l. 1167).** "safety-manual access (under NDA if need be)" →
    "safety-manual access (AMD's Functional Safety Lounge is a paid
    subscription; whether it holds a Versal Gen 2 safety manual is not
    public)"; and "M0's research check confirms AMD's per-cluster Lock-mode
    configuration in its own boot software" → "... AMD's per-cluster
    lock-step configuration (found: Bootgen's per-partition `lockstep`
    attribute, applied by the PLM; to confirm on the board that it is Arm's
    Lock-mode, CEMODE 0b11, not Hybrid)".
12. **Sources, "Not verified" paragraph (l. 1431 on).** Replace it with: "Not
    verified and to be confirmed before relying on them: that AMD's APU
    lock-step is Arm's Lock-mode rather than Hybrid; AMD's ASIL D status for
    the Versal Gen 2 APU (one ADAS brief says "certified", the Gen 2 briefs
    say "targeted" and cite pre-silicon targets; no certificate found);
    whether a Versal Gen 2 safety manual is in the Functional Safety Lounge;
    the silicon's GIC ITS; Verus's 700 lines of axioms; µRust's lack of
    traits and dynamic dispatch; Orin's SMMUv2 and ASIL B rating; the S32G's
    lack of an SMMU; whether Cortex-A720AE and Neoverse V3AE lack SMT;
    ProvenCore's EAL7 certificate; Tensor G2's logical CPU numbering."
13. **§6.5 F1 (after "requires a formal model of the **entire TSF**").** Add:
    "(CC:2022 Part 3, ADV_SPM.1.3C: "The formal model shall cover the
    complete set of SFRs that define the TSF"; JIL's interim sub-TSF
    reading is limited to IC and Java Card protection profiles; the only
    other route is a multi-assurance ST with ADV_SPM.1 on named sub-TSFs)".
14. **§6.5 T1, after "which owns the fault reaction".** Add: "; errors
    detected in Lock-mode "cannot be contained", and Arm says the fault
    reaction time cannot be quantified (TRM §2.11.1.6-7), both AoUs for the
    integrator".
15. **§6.5 toolchain, after the qualifications.** Add: "Ferrocene's certified
    `core` subset is at IEC 61508 SIL 2 / ISO 26262 ASIL B only, and the
    signed qualification material comes with the Enterprise plan, not the
    €25-a-month Individual plan".
16. **§11 Q18, the Pixel 7.** Add: "(expected MIDR_EL1 implementer 0x41, part
    0xD41 on the two middle-cluster cores; the A78AE is 0xD42)".
17. **Sources.** (a) Ferrocene targets: cite the 26.05 release's list
    ([release/1.95](https://github.com/ferrocene/ferrocene/blob/release/1.95/ferrocene/doc/user-manual/src/targets/index.rst))
    in place of `public-docs.ferrocene.dev/main`, which tracks the next
    release. (b) Hybrid mode's rating: Arm's
    [Cortex-A78AE page](https://www.arm.com/products/silicon-ip-cpu/cortex-a/cortex-a78ae)
    in place of the blog, which names neither the A78AE nor SIL 3. (c) CC:
    add [CC:2022 Part 3](https://www.commoncriteriaportal.org/files/ccfiles/CC2022PART3R1.pdf)
    and [Part 5](https://www.commoncriteriaportal.org/files/ccfiles/CC2022PART5R1.pdf)
    as the primary texts. (d) L3 partitioning: add the
    [DSU-AE TRM](https://documentation-service.arm.com/static/5f73673b1b758617cd959dbc).
    (e) AMD lock-step: add Bootgen and the PLM's `xloader_plat.c`
    (embeddedsw `xlnx_rel_v2026.1`). (f) Ferrocene pricing:
    [ferrocene.dev/pricing](https://ferrocene.dev/pricing/).
