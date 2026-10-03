//! The client a request is for: whose memory `os_memcpy_from_user` reads,
//! and whose identity RM asks for (`docs/NVIDIA.md` §4.4, R3).
//!
//! On Linux, RM runs in the client's own thread, so "user" memory is the
//! calling process's and `current` is the client. In nvrm every request
//! comes from another process. The thread serving it says so with
//! [`nvos_client_enter`], naming a [`Client`]: its credentials, and the
//! functions that copy its memory. N1e's request bridge will back those with
//! `request_copy_in` and `request_copy_out`, which reach the waiting client
//! only while its request is outstanding. `nvrm-link-test` is its own
//! client, and copies within itself.
//!
//! With no client entered, a copy is refused and said: RM is never allowed
//! to read nvrm's own memory as if it were a client's.

use core::ffi::{c_char, c_void};
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use crate::futex;
use crate::libc;
use crate::log::say;
use crate::status::{self, NvBool, NvStatus};

/// A client, as the thread serving its request describes it.
#[repr(C)]
#[derive(Debug)]
pub struct Client {
    /// Its process id, as RM records it.
    pub pid: u32,
    /// Its effective user id.
    pub euid: u32,
    /// Whether it is root in its user namespace (`os_is_administrator`).
    pub administrator: bool,
    /// Its name, NUL-terminated.
    pub name: [c_char; 16],
    /// Copy `length` bytes from its `from` to nvrm's `to`; 0 on success.
    pub copy_in: Option<extern "C" fn(*mut c_void, *mut c_void, u64, u32) -> i32>,
    /// Copy `length` bytes from nvrm's `from` to its `to`; 0 on success.
    pub copy_out: Option<extern "C" fn(*mut c_void, u64, *const c_void, u32) -> i32>,
    /// What the two copies are given first: the request, for the bridge.
    pub context: *mut c_void,
}

/// The most threads serving a client at once.
const SLOTS: usize = 64;

/// Which thread serves which client: a thread id and its client.
struct Slot {
    /// The thread, 0 for a free slot.
    tid: AtomicU32,
    /// Its client.
    client: AtomicPtr<Client>,
}

/// The slots.
static SLOTS_TABLE: [Slot; SLOTS] = [const {
    Slot {
        tid: AtomicU32::new(0),
        client: AtomicPtr::new(ptr::null_mut()),
    }
}; SLOTS];

/// The client the calling thread serves, if any.
fn current() -> Option<&'static Client> {
    let tid = futex::tid();
    let slot = SLOTS_TABLE
        .iter()
        .find(|slot| slot.tid.load(Ordering::Acquire) == tid)?;
    let client = slot.client.load(Ordering::Acquire);
    // SAFETY: `nvos_client_enter`'s caller keeps the client alive until it
    // calls `nvos_client_leave` on this same thread.
    (!client.is_null()).then(|| unsafe { &*client })
}

/// Say the calling thread now serves `client`, until
/// [`nvos_client_leave`]. Returns whether there was a slot for it.
///
/// # Safety
///
/// `client` lives, unchanged, until the same thread leaves.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_client_enter(client: *const Client) -> bool {
    let tid = futex::tid();
    for slot in &SLOTS_TABLE {
        if slot
            .tid
            .compare_exchange(0, tid, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            slot.client.store(client.cast_mut(), Ordering::Release);
            return true;
        }
    }
    say!("nvos_client_enter: more than {SLOTS} threads serve clients at once");
    false
}

/// Say the calling thread serves no client any more.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_client_leave() {
    let tid = futex::tid();
    for slot in &SLOTS_TABLE {
        if slot.tid.load(Ordering::Acquire) == tid {
            slot.client.store(ptr::null_mut(), Ordering::Release);
            slot.tid.store(0, Ordering::Release);
        }
    }
}

/// `os_memcpy_from_user`: from the client's `from` to nvrm's `to`.
#[unsafe(no_mangle)]
pub extern "C" fn os_memcpy_from_user(
    to: *mut c_void,
    from: *const c_void,
    length: u32,
) -> NvStatus {
    let Some(copy) = current().and_then(|client| client.copy_in.map(|copy| (client, copy))) else {
        say!("os_memcpy_from_user of {length} bytes with no client on this thread: refused");
        return status::INVALID_ADDRESS;
    };
    let (client, copy_in) = copy;
    if copy_in(client.context, to, from as usize as u64, length) == 0 {
        status::OK
    } else {
        status::INVALID_ADDRESS
    }
}

/// `os_memcpy_to_user`: from nvrm's `from` to the client's `to`.
#[unsafe(no_mangle)]
pub extern "C" fn os_memcpy_to_user(to: *mut c_void, from: *const c_void, length: u32) -> NvStatus {
    let Some(copy) = current().and_then(|client| client.copy_out.map(|copy| (client, copy))) else {
        say!("os_memcpy_to_user of {length} bytes with no client on this thread: refused");
        return status::INVALID_ADDRESS;
    };
    let (client, copy_out) = copy;
    if copy_out(client.context, to as usize as u64, from, length) == 0 {
        status::OK
    } else {
        status::INVALID_ADDRESS
    }
}

/// nvrm's own process id.
fn own_pid() -> u32 {
    // SAFETY: `getpid` takes nothing and cannot fail.
    u32::try_from(unsafe { libc::getpid() }).unwrap_or(0)
}

/// `os_get_current_process`: the client's pid, or nvrm's.
#[unsafe(no_mangle)]
pub extern "C" fn os_get_current_process() -> u32 {
    current().map_or_else(own_pid, |client| client.pid)
}

/// `os_get_current_process_name`: the client's name, or `nvrm`.
///
/// # Safety
///
/// `buffer` has room for `length` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_get_current_process_name(buffer: *mut c_char, length: u32) {
    let Some(room) = usize::try_from(length).ok().filter(|&room| room > 0) else {
        return;
    };
    // SAFETY: the caller vouches for the room.
    let out = unsafe { core::slice::from_raw_parts_mut(buffer, room) };
    let own: [c_char; 5] = [
        b'n' as c_char,
        b'v' as c_char,
        b'r' as c_char,
        b'm' as c_char,
        0,
    ];
    let name: &[c_char] = match current() {
        Some(client) => &client.name,
        None => &own,
    };
    let last = room - 1;
    for (index, slot) in out.iter_mut().enumerate() {
        let byte = if index < last {
            name.get(index).copied().unwrap_or(0)
        } else {
            0
        };
        *slot = byte;
        if byte == 0 {
            break;
        }
    }
}

/// `os_get_euid`: the client's effective user id, or nvrm's.
///
/// # Safety
///
/// `euid` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_get_euid(euid: *mut u32) -> NvStatus {
    let value = match current() {
        Some(client) => client.euid,
        // SAFETY: `geteuid` takes nothing and cannot fail.
        None => unsafe { libc::geteuid() },
    };
    // SAFETY: the caller vouches for it.
    unsafe { euid.write(value) };
    status::OK
}

/// Whether the client, or nvrm when serving none, is root.
fn administrator() -> bool {
    match current() {
        Some(client) => client.administrator,
        // SAFETY: `geteuid` takes nothing and cannot fail.
        None => (unsafe { libc::geteuid() }) == 0,
    }
}

/// `os_is_administrator`.
#[unsafe(no_mangle)]
pub extern "C" fn os_is_administrator() -> NvBool {
    status::bool(administrator())
}

/// `os_check_access`: performance monitoring and raised priority are the
/// administrator's, as on a Linux without `CAP_PERFMON` granted apart.
#[unsafe(no_mangle)]
pub extern "C" fn os_check_access(right: u16) -> NvBool {
    /// `RS_ACCESS_NICE`.
    const NICE: u16 = 1;
    /// `RS_ACCESS_PERFMON`.
    const PERFMON: u16 = 3;
    status::bool(matches!(right, NICE | PERFMON) && administrator())
}

/// `os_get_pid_info`: the client's pid, held for `os_find_ns_pid`.
#[unsafe(no_mangle)]
pub extern "C" fn os_get_pid_info() -> *mut c_void {
    // SAFETY: a fresh block for one `u32`.
    let info = unsafe { libc::malloc(size_of::<u32>()) }.cast::<u32>();
    if !info.is_null() {
        // SAFETY: a fresh, aligned block.
        unsafe { info.write(os_get_current_process()) };
    }
    info.cast()
}

/// `os_put_pid_info`.
///
/// # Safety
///
/// `info` is null or one `os_get_pid_info` returned, put once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_put_pid_info(info: *mut c_void) {
    // SAFETY: the caller vouches for it; free takes null.
    unsafe { libc::free(info) };
}

/// `os_find_ns_pid`: the pid as the caller's namespace sees it. Every
/// client shares one pid namespace with nvrm until namespaces reach the
/// request (N1e), so it is the pid itself.
///
/// # Safety
///
/// `info` is null or one `os_get_pid_info` returned; `pid` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_find_ns_pid(info: *mut c_void, pid: *mut u32) -> NvStatus {
    if info.is_null() || pid.is_null() {
        return status::INVALID_ARGUMENT;
    }
    // SAFETY: the caller vouches for `info`: one os_get_pid_info made.
    let value = unsafe { info.cast::<u32>().read() };
    // SAFETY: the caller vouches that `pid` is writable.
    unsafe { pid.write(value) };
    status::OK
}

/// `os_is_init_ns`: see [`os_find_ns_pid`].
#[unsafe(no_mangle)]
pub extern "C" fn os_is_init_ns() -> NvBool {
    status::TRUE
}
