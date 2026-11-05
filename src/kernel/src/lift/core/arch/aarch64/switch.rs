//! A program's user state as linuxd keeps it (docs/microkernel/M5-LIFT.md §4.2):
//! the same record as the kernel's; K1's registers fill it at a restricted exit.

/// What a program owns on this processor that no trap saves: its thread
/// pointer, and its floating-point and SIMD registers.
///
/// The kernel is built soft-float and never touches either, so a trap from EL0
/// leaves them exactly as the program had them, and one program running at a
/// time needed nothing more. Two programs taking turns do: the scheduler saves
/// the outgoing program's copy and loads the incoming one's whenever it
/// switches between tasks that run user code.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct UserState {
    /// `TPIDR_EL0`, which EL0 writes for itself.
    thread_pointer: u64,
    /// `FPCR`.
    fpcr: u64,
    /// `FPSR`.
    fpsr: u64,
    /// Keeps the vectors at a sixteen-byte offset.
    reserved: u64,
    /// `q0` to `q31`, sixteen bytes each.
    vectors: [u8; 512],
}

/// A vector image with reserved bits set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadImage;

const _: () = assert!(
    core::mem::offset_of!(UserState, vectors) == 32,
    "the save sequence stores q0 at offset 32"
);

impl UserState {
    /// A copy of the user state K1 holds of this thread: what a fork child inherits.
    ///
    /// # Safety
    ///
    /// (CONTEXT) On the program's own thread.
    pub unsafe fn capture() -> UserState {
        crate::restricted::capture_state()
    }

    /// Give the program `pointer` as its thread pointer, as `CLONE_SETTLS`
    /// asks.
    pub const fn set_thread_pointer(&mut self, pointer: u64) {
        self.thread_pointer = pointer;
    }

    /// The thread pointer on record.
    pub const fn thread_pointer(&self) -> u64 {
        self.thread_pointer
    }

    /// The vector state as its image: `V0` to `V31`, then `FPCR` and `FPSR`
    /// (`docs/microkernel/K1.md` §5.2).
    pub fn vector_image(&self) -> [u8; 528] {
        let mut image = [0; 528];
        image[..512].copy_from_slice(&self.vectors);
        image[512..520].copy_from_slice(&self.fpcr.to_le_bytes());
        image[520..].copy_from_slice(&self.fpsr.to_le_bytes());
        image
    }

    /// Replace the vector state with `image`, whose `FPCR` and `FPSR` hold only
    /// defined bits.
    ///
    /// # Errors
    ///
    /// [`BadImage`], with nothing changed.
    pub fn set_vector_image(&mut self, image: &[u8; 528]) -> Result<(), BadImage> {
        /// `FPCR`'s defined bits: `AHP`, `DN`, `FZ`, `RMode`, the trap enables, `FIZ`, `AH`, `NEP`.
        const FPCR_DEFINED: u64 = 0x07C0_9F07;
        /// `FPSR`'s defined bits: `NZCV`, `QC` and the cumulative flags.
        const FPSR_DEFINED: u64 = 0xF800_009F;
        let word = |at: usize| {
            image
                .get(at..at + 8)
                .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
                .map(u64::from_le_bytes)
                .ok_or(BadImage)
        };
        let (fpcr, fpsr) = (word(512)?, word(520)?);
        if fpcr & !FPCR_DEFINED != 0 || fpsr & !FPSR_DEFINED != 0 {
            return Err(BadImage);
        }
        self.vectors.copy_from_slice(&image[..512]);
        self.fpcr = fpcr;
        self.fpsr = fpsr;
        Ok(())
    }

    /// A 32-bit x86 program's thread-local segment: there are none on this
    /// architecture, and no table here maps a call that asks for one.
    pub const fn set_thread_area(&mut self, _index: usize, _descriptor: u64) -> bool {
        false
    }

    /// A program's state before it has run: no thread pointer, rounding to
    /// nearest, no floating-point exceptions trapped, every register zero.
    pub const fn new() -> UserState {
        UserState {
            thread_pointer: 0,
            fpcr: 0,
            fpsr: 0,
            reserved: 0,
            vectors: [0; 512],
        }
    }

    /// `FPCR` and `FPSR`, as a signal frame's `fpsimd_context` records them.
    pub const fn fp_control(&self) -> (u64, u64) {
        (self.fpcr, self.fpsr)
    }

    /// `q0` to `q31`, sixteen little-endian bytes each.
    pub const fn vectors(&self) -> &[u8; 512] {
        &self.vectors
    }

    /// Replace the floating-point and SIMD registers, as `rt_sigreturn` does.
    /// The thread pointer is left as it was.
    pub const fn set_fp(&mut self, fpcr: u64, fpsr: u64, vectors: [u8; 512]) {
        self.fpcr = fpcr;
        self.fpsr = fpsr;
        self.vectors = vectors;
    }
}

impl UserState {
    /// The vector state as K1 reads and writes it.
    pub fn image(&self) -> [u8; 528] {
        self.vector_image()
    }

    /// Take the record's thread pointer from K1's context.
    pub fn read_context(&mut self, context: &ferrix_native::region::Context) {
        self.thread_pointer = context.tpidr;
    }

    /// Put the record's thread pointer into K1's context.
    pub fn write_context(&self, context: &mut ferrix_native::region::Context) {
        context.tpidr = self.thread_pointer;
    }
}

/// Load `state`: the vectors at once, the thread pointer at the program's next entry.
///
/// # Safety
///
/// (CONTEXT) On the program's own thread.
pub unsafe fn restore_user_state(state: &UserState, _entry_stack: u64) {
    crate::restricted::load_state(state);
}

/// As `execve` does.
///
/// # Safety
///
/// (CONTEXT) On the program's own thread.
pub unsafe fn reset_user_state() {
    crate::restricted::reset_state();
}
