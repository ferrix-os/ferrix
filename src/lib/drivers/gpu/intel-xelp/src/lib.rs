//! Intel's Xe-LP GT (Gen12.0), as on the N150's Alder Lake-N, as logic over
//! registers and memory, translated from Linux's i915 (`docs/INTEL-GPU.md`
//! §3.6).
//!
//! `src/user/system/native/drivers/gpu/intel` runs this in a ring-3 process.
//! The process holds the device's register window, its interrupt and the
//! pinned memory. None of those exist in a unit test, so everything that
//! decides a register value, a table entry or a command word is here, written
//! against [`io::Mmio`] and [`io::Clock`], and tested against models of them.
//!
//! # Following i915
//!
//! Each module is named after the i915 file it translates, and keeps that
//! file's structure, names and order, so a later Linux release can be read
//! against it. Each translated file starts with its i915 file's copyright
//! lines, and the MIT notice they are under is `i915/NOTICE` in this crate.
//! Each function and table names the i915 function or array it follows.
//! Tables are not retyped: `tools/common/gen/gen-intel-xelp.py` generates
//! [`generated`] from i915's own files, vendored unchanged under `i915/`.
//!
//! What is left out, and why, is `docs/INTEL-GPU.md` §3.6: everything for a
//! platform other than ADL-P and ADL-N, the display, `GuC`, and the Linux machinery the
//! process replaces.
//!
//! * [`reg`]: `i915_reg_defs.h`'s helpers: bits, masks, masked writes;
//! * [`io`]: what the process supplies: register access and time;
//! * [`device_info`]: `i915_pci.c`'s `adl_p_info` and the ADL-N ids;
//! * [`uncore`]: `intel_uncore.c`'s forcewake, for Gen12;
//! * [`mocs`]: `intel_mocs.c`'s Gen12 table and how it is written;
//! * [`lrc`]: `intel_lrc.c`'s register-offset byte code;
//! * [`gt`]: the order i915 brings the GT and each engine up in;
//! * [`gtt`]: the GGTT and PPGTT entry formats and the PAT;
//! * [`engine`]: the engines and their `RING_*` registers;
//! * [`commands`] and [`engine_cs`]: command streams, and what i915 writes
//!   around each request;
//! * [`execlists`]: descriptors, the submit queue and the status buffer;
//! * [`irq`]: the Gen11 interrupt banks and identities;
//! * [`plane`]: the firmware's lit plane, read so its GGTT entries are kept;
//! * [`rps`] and [`rc6`]: frequency requests and idle power, by the host;
//! * [`clock`]: the command streamer's timestamp frequency;
//! * [`sseu`]: the fuses, and the topology Mesa is told;
//! * [`uc`]: whether `GuC` locked the `WOPCM` since the last reset;
//! * [`uapi`]: the i915 ioctl numbers and argument layouts;
//! * [`workarounds`]: the GT, engine, context and whitelist lists.

#![no_std]
#![forbid(unsafe_code)]

pub mod clock;
pub mod commands;
pub mod device_info;
pub mod engine;
pub mod engine_cs;
pub mod execlists;
pub mod generated;
pub mod gt;
pub mod gtt;
pub mod io;
pub mod irq;
pub mod lrc;
pub mod mocs;
pub mod plane;
pub mod rc6;
pub mod reg;
pub mod rps;
pub mod sseu;
pub mod uapi;
pub mod uc;
pub mod uncore;
pub mod workarounds;

#[cfg(test)]
mod tests;
