//! A driver whose own page a gate poisons: `cargo xtask test-mce-driver`'s,
//! carried under the name `intel` on the QEMU stand-in for ADL-N's GPU, so
//! that devmgr starts it -- and starts it again after it dies -- as it does
//! the Intel driver (`docs/MACHINE-CHECK-DRIVER.md` §3).
//!
//! It reads START as an engine's -- one handle, the device -- pins one page
//! of memory to the device and maps it, so the page is the driver's own and
//! its device's, as a real driver's DMA buffer is. It fills the page with
//! words `MAGIC << 32 | (pid & 0xffff) << 16 | n`, so the gate can tell
//! from the frame which start's page it is -- no count is kept anywhere --
//! says `mce-victim: pid <p> va <hex>`, sends the chardev HELLO the Intel
//! driver sends -- whose acceptance gives a dead start's pins back
//! (`object::pin::quarantine_release`, the line the gate reads) -- and then,
//! round after round, reads
//! the page in ring 3 and writes it through a channel of its own and reads
//! it back: the gate injects at a ring-3 load of it, or at the kernel's copy
//! of it in `channel_write` (`docs/MACHINE-CHECK-COPY.md`).
//!
//! It never ends on its own: the machine check ends it, and devmgr starts it
//! again. Only that gate's image carries it: xtask puts it under `intel` for
//! the one boot and nowhere else (`tools/common/xtask/src/mce_driver.rs`).

#![no_std]
#![no_main]

use ferrix_blkring::control::{Message as StartMessage, START_BYTES};
use ferrix_chardevctl::message::{self, Hello, MAX_NODES, Message, VERSION};
use ferrix_chardevctl::node::RENDER_MINOR;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_rt::native::device::Device;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::PinAccess;
use ferrix_rt::native::{channel, vmo};
use ferrix_rt::{Bootstrap, Kernel};

ferrix_rt::entry!(main);

/// A page.
const PAGE: usize = 4096;

/// The high half of every word of the page: `MCEV`.
const MAGIC: u64 = 0x4d43_4556;

/// The status a start that could not set itself up ends with:
/// `EX_SOFTWARE`, a driver's own failure.
const FAILED: i32 = 70;

/// What every line starts with.
const SAID: &str = "mce-victim:";

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        ferrix_rt::err!("{SAID} no bootstrap channel");
        return FAILED;
    };
    let _ = boot.wait_one(Signals::READABLE, Deadline::Never);
    let mut bytes = [0_u8; START_BYTES];
    let mut handles = [Handle::INVALID; 1];
    let Ok(received) = boot.read(&mut bytes, &mut handles) else {
        ferrix_rt::err!("{SAID} no START");
        return FAILED;
    };
    if !matches!(
        StartMessage::decode(bytes.get(..received.bytes).unwrap_or_default()),
        Ok(StartMessage::Start(_))
    ) || received.handles != 1
    {
        ferrix_rt::err!("{SAID} not a START with one handle");
        return FAILED;
    }
    let device = Device::from_owned(OwnedHandle::from_raw(Kernel, handles[0]));
    let location = match StartMessage::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Ok(StartMessage::Start(start)) => start.location,
        _ => 0,
    };
    let Ok(memory) = vmo::create(Kernel, PAGE) else {
        ferrix_rt::err!("{SAID} no page");
        return FAILED;
    };
    let Ok(pin) = device.pin(&memory, 0, PAGE, PinAccess::ReadWrite) else {
        ferrix_rt::err!("{SAID} the page's pin was refused");
        return FAILED;
    };
    let Ok(at) = memory.map(None, PAGE, Protection::ReadWrite, 0) else {
        ferrix_rt::err!("{SAID} the page would not map");
        return FAILED;
    };
    let Ok((ours, theirs)) = channel::create(Kernel) else {
        ferrix_rt::err!("{SAID} no channel");
        return FAILED;
    };
    // SAFETY: no pointer argument; `getpid` reads nothing of ours.
    let pid = unsafe { ferrix_rt::linux::call(ferrix_rt::linux::numbers::GETPID, [0; 6]) }
        .unwrap_or(0) as u64;
    let words = at as *mut u64;
    for n in 0..PAGE / 8 {
        // SAFETY: `at` is a fresh read-write mapping of `PAGE` bytes this
        // program alone writes, and `n` is within it.
        unsafe {
            words
                .wrapping_add(n)
                .write_volatile(MAGIC << 32 | (pid & 0xffff) << 16 | n as u64);
        }
    }
    ferrix_rt::err!("{SAID} pid {pid} va {at:#x}");
    // Held for the life of the start: its close would end the publication.
    let Some(_control) = hello(&device, location) else {
        return FAILED;
    };
    // SAFETY: as above; read only from here on, and through the channel.
    let page = unsafe { core::slice::from_raw_parts(at as *const u8, PAGE) };
    let mut back = [0_u8; PAGE];
    let mut sum = 0_u64;
    loop {
        for n in 0..PAGE / 8 {
            // SAFETY: as above: a load in ring 3, which the gate may poison.
            sum = sum.wrapping_add(unsafe { words.wrapping_add(n).read_volatile() });
        }
        // The kernel's copy of the page, which the gate may poison instead.
        if ours.write(page).is_ok() {
            let _ = theirs.read(&mut back, &mut []);
        }
        sum = core::hint::black_box(sum);
        let _ = &pin;
    }
}

/// The chardev HELLO the Intel driver sends, one render node: its
/// acceptance gives back the pins a dead start left in the quarantine.
/// The control channel, or `None` with the reason said.
fn hello(device: &Device<Kernel>, location: u32) -> Option<channel::Channel<Kernel>> {
    let Ok(control) = device.chardev_control() else {
        ferrix_rt::err!("{SAID} no chardev control");
        return None;
    };
    let mut minors = [0; MAX_NODES];
    if let Some(first) = minors.first_mut() {
        *first = RENDER_MINOR;
    }
    let hello = Hello {
        version: VERSION,
        location,
        count: 1,
        minors,
    };
    if control
        .write(Message::Hello(hello).encode().as_bytes())
        .is_err()
    {
        ferrix_rt::err!("{SAID} HELLO not sent");
        return None;
    }
    let _ = control.wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never);
    let mut bytes = [0_u8; message::MAX_BYTES];
    let mut handles = [Handle::INVALID; 1];
    let read = control.read(&mut bytes, &mut handles).ok()?;
    match Message::decode(bytes.get(..read.bytes).unwrap_or(&[])) {
        Ok(Message::Ready(_)) => Some(control),
        _ => {
            ferrix_rt::err!("{SAID} HELLO refused");
            None
        }
    }
}
