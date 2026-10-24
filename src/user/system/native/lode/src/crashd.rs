//! `lode crashd`: the crash collector (`docs/LOGGING.md` §21, tombstones
//! step 2). Started by init as `lode-crashd.service`, as root, it arms the
//! kernel's crash hold by holding `/dev/ferrix-crashes` open. For each
//! process the kernel holds it reads, while the process still exists, every
//! thread's state, its `status`, its `maps` and its descriptors, sends them
//! to lode as one record of systemd's native protocol, and lets the process
//! go (`done PID`). lode makes the parts blobs and puts their references on
//! the crash's tombstone.
//!
//! The kernel lets a process go by itself after 2 s, so a collector that
//! stalls costs a crash its parts, never its end; one that dies disarms the
//! hold with its last descriptor.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::tombstone::{Held, parse_held};

use crate::cli::complain;
use crate::snapshot::{self, Fields};
use crate::sys;

/// The device the kernel's hold is armed by.
const DEVICE: &[u8] = b"/dev/ferrix-crashes";

/// `O_RDWR`.
const O_RDWR: usize = 2;

/// The most of each part a record carries: the datagram is 256 KiB.
const MAPS_MAX: usize = 128 * 1024;
const FDS_MAX: usize = 32 * 1024;
const THREADS_MAX: usize = 48 * 1024;
const STATUS_MAX: usize = 8 * 1024;

/// `bytes`, cut to `max` with a last line saying so.
fn bounded(mut bytes: Vec<u8>, max: usize) -> Vec<u8> {
    if bytes.len() > max {
        bytes.truncate(max);
        bytes.extend_from_slice(b"\n... cut short\n");
    }
    bytes
}

/// The descriptors of the process at `base`, a line each: `N -> TARGET`.
fn descriptors(base: &str) -> Vec<u8> {
    let Ok(fd) = sys::open(alloc::format!("{base}/fd").as_bytes(), sys::O_RDONLY) else {
        return Vec::new();
    };
    let mut names: Vec<u32> = sys::list_dir(fd)
        .unwrap_or_default()
        .iter()
        .filter_map(|name| core::str::from_utf8(name).ok()?.parse().ok())
        .collect();
    sys::close(fd);
    names.sort_unstable();
    let mut text = Vec::new();
    for number in names {
        let target = sys::read_link(alloc::format!("{base}/fd/{number}").as_bytes())
            .unwrap_or_else(|| Vec::from(&b"?"[..]));
        text.extend_from_slice(alloc::format!("{number} -> ").as_bytes());
        text.extend_from_slice(&target);
        text.push(b'\n');
    }
    text
}

/// The record for the held process `held`, read from `/proc`.
pub(crate) fn collect(held: &Held) -> Fields {
    let base = alloc::format!("/proc/{}", held.pid);
    let comm = sys::read_small(alloc::format!("{base}/comm").as_bytes())
        .map(|mut comm| {
            while comm.last() == Some(&b'\n') {
                let _ = comm.pop();
            }
            comm
        })
        .unwrap_or_default();
    let threads = bounded(snapshot::threads_text(&base), THREADS_MAX);
    let maps = bounded(
        sys::read_small(alloc::format!("{base}/maps").as_bytes()).unwrap_or_default(),
        MAPS_MAX,
    );
    let fds = bounded(descriptors(&base), FDS_MAX);
    let status = bounded(
        sys::read_small(alloc::format!("{base}/status").as_bytes()).unwrap_or_default(),
        STATUS_MAX,
    );
    let message = alloc::format!(
        "collected the crash of {} ({}): {} thread(s), {} unparked, {} map line(s), {} \
         descriptor(s){}",
        held.pid,
        String::from_utf8_lossy(&comm),
        held.threads,
        held.unparked,
        maps.iter().filter(|&&byte| byte == b'\n').count(),
        fds.iter().filter(|&&byte| byte == b'\n').count(),
        if held.dropped > 0 {
            alloc::format!("; {} crash(es) before it not held", held.dropped)
        } else {
            String::new()
        }
    );
    let number = |value: u32| alloc::format!("{value}").into_bytes();
    let mut fields: Fields = alloc::vec![
        (String::from("MESSAGE"), message.into_bytes()),
        (String::from("PRIORITY"), Vec::from(&b"6"[..])),
        (
            String::from("SYSLOG_IDENTIFIER"),
            Vec::from(&b"lode-crashd"[..])
        ),
        (
            String::from("FERRIX_EVENT"),
            Vec::from(&b"crash-collected"[..])
        ),
        (String::from("COLLECTED_PID"), number(held.pid)),
        (String::from("COLLECTED_TID"), number(held.tid)),
        (String::from("COLLECTED_SIGNAL"), number(held.signal)),
        (String::from("COLLECTED_UNPARKED"), number(held.unparked)),
    ];
    for (name, value) in [
        ("COLLECTED_THREADS", threads),
        ("COLLECTED_MAPS", maps),
        ("COLLECTED_FDS", fds),
        ("COLLECTED_STATUS", status),
    ] {
        if !value.is_empty() {
            fields.push((String::from(name), value));
        }
    }
    fields
}

/// `lode crashd`: arm the hold and collect until the device fails.
pub(crate) fn run() -> i32 {
    let fd = match sys::open(DEVICE, O_RDWR | sys::O_CLOEXEC) {
        Ok(fd) => fd,
        Err(error) => {
            complain(&alloc::format!(
                "lode crashd: {} did not open (errno {}); no crash is held",
                String::from_utf8_lossy(DEVICE),
                error.0
            ));
            return 1;
        }
    };
    let mut buf = [0_u8; 256];
    loop {
        let read = match sys::read(fd, &mut buf) {
            Ok(read) => read,
            Err(error) => {
                complain(&alloc::format!(
                    "lode crashd: read failed (errno {})",
                    error.0
                ));
                return 1;
            }
        };
        let Some(held) = buf.get(..read).and_then(parse_held) else {
            continue;
        };
        let record = collect(&held);
        if let Ok(log) = crate::cli::connect_datagram(b"/run/systemd/journal/socket") {
            let _ = sys::write(log, &snapshot::datagram(&record));
            sys::close(log);
        }
        let _ = sys::write(fd, alloc::format!("done {}\n", held.pid).as_bytes());
    }
}
