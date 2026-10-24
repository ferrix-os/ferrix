//! Stage 3's look at the interrupt descriptor table's gates: which vectors
//! ring 3 may raise by a software interrupt (`L.x86_64.81`).
//!
//! Moved here unchanged from the trap's check, which runs Linux programs and
//! so is raised above the core (`docs/KERNEL-CRATES.md`): this one runs
//! none and reads the table, which is the trap's own, and the trap's check
//! calls it after its programs.

use crate::console::println;

// CHECKS-ONLY: arch/x86_64/trap/check.rs
/// Ring 3 may raise `#BP`, `#OF` and the legacy system call by a software
/// interrupt, and no other vector: Linux's three system gates. Last, after
/// the programs, so that a gate a program needs and lost fails by what the
/// program saw, and only a vector opened beyond Linux's fails here.
///
/// Verifies: `L.x86_64.81`
#[doc(hidden)]
pub fn check_user_gates() -> Result<(), &'static str> {
    // SAFETY: (SHARED) the table is written once by `init`, before any
    // program runs, and only read from then on.
    let table = unsafe { &*super::IDT.0.get() };
    let mut user = 0;
    for (vector, gate) in table.iter().enumerate() {
        let dpl = (gate.flags >> 5) & 3;
        let wanted = match vector {
            super::BREAKPOINT | super::OVERFLOW | super::LEGACY_SYSCALL => 3,
            _ => 0,
        };
        if dpl != wanted {
            println!("  gates    vector {vector:#x}'s gate has DPL {dpl}, not {wanted}");
            return Err("a gate's DPL is not as on Linux: ring 3 raises #BP, #OF, int $0x80 alone");
        }
        user += usize::from(dpl == 3);
    }
    println!(
        "  gates    {user} of {} gates take a software interrupt from ring 3: #BP, #OF and \
         int $0x80, as Linux's system gates",
        table.len()
    );
    Ok(())
}
