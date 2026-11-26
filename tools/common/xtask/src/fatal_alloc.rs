//! `cargo xtask check-fatal-alloc --personality user --set kernel_memory=retyped`: every
//! FATAL-ALLOC site shown boot-only from the image (ledger 5942 K16.5, 10421 H1-H8,
//! 10563 R6). It builds the release kernel of this run's flags for AArch64 and x86-64 and
//! runs `tools/common/check/check-fatal-alloc-boot.py` on each. Not in `check` until its
//! review (10421 H8).

use crate::args::Args;
use crate::check::python_with;
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, cargo};

/// The task.
pub(crate) const TASKS: &[Task] = &[Task {
    name: "check-fatal-alloc",
    help_at: 968,
    help: &[
        "check-fatal-alloc  Build release kernels under kernel_memory=retyped and fail on a FATAL-ALLOC site something",
        "              able to run after the seal reaches, from the image's call graph (K16.5)",
    ],
    run: |_, args| check_fatal_alloc(args),
}];

fn check_fatal_alloc(args: &Args) -> Result<()> {
    let flags = crate::flags::this_run()?;
    if flags.value("kernel_memory") != Some("retyped") {
        return Err(Error::new(
            "check-fatal-alloc judges a kernel_memory=retyped image: --personality user --set kernel_memory=retyped",
        ));
    }
    let arches = if args.arch.is_some() {
        args.arches()?
    } else {
        vec![Arch::AArch64, Arch::X86_64]
    };
    for arch in arches {
        let kernel = cargo::build_kernel(arch, true)?;
        let elf = kernel.elf.display().to_string();
        let name = arch.to_string();
        println!("  {arch}: check-fatal-alloc on {elf}");
        python_with(
            "tools/common/check/check-fatal-alloc-boot.py",
            &[elf.as_str(), name.as_str(), "--paths"],
        )?;
    }
    Ok(())
}
