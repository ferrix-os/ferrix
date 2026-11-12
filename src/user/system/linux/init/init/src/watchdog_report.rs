//! What init says of a service that missed its watchdog
//! (`docs/WATCHDOG-STAGES.md` §5.4): each thread of its main process, by
//! name, and the call it waits in.
//!
//! Ferrix's `/proc/<pid>/task/<tid>/wchan` names the call a thread is
//! blocked in, with its descriptor's target for one on a descriptor
//! (`src/kernel/extensions/personality/linux/src/fs/procfs/debug.rs`, `wchan_text`), or `0` for one that
//! runs. These files are rendered from the kernel's own record of the task:
//! reading them takes no lock the hung program holds and reads no disk.

use std::fs;

/// The most threads one line names.
const MOST: usize = 16;

/// `pid 412 threads lode/412 futex, lode-io/413 read:/dev/vda`, or why not.
pub(crate) fn threads(pid: u32) -> String {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/task")) else {
        return format!("pid {pid}, its threads not readable");
    };
    let mut tids: Vec<u32> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect();
    tids.sort_unstable();
    let mut named: Vec<String> = Vec::new();
    for &tid in tids.iter().take(MOST) {
        let read = |file: &str| {
            fs::read_to_string(format!("/proc/{pid}/task/{tid}/{file}"))
                .map(|text| text.trim().to_owned())
                .unwrap_or_else(|_| String::from("?"))
        };
        let wchan = read("wchan");
        let wchan = if wchan == "0" {
            String::from("running")
        } else {
            wchan
        };
        named.push(format!("{}/{tid} {wchan}", read("comm")));
    }
    let mut line = format!("pid {pid} threads {}", named.join(", "));
    if tids.len() > MOST {
        line.push_str(&format!(", and {} more", tids.len() - MOST));
    }
    line
}
