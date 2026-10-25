//! `ferrix-install --serve`: the live session's installer daemon, which the
//! graphical installer (`ferrix-installer`, the apps repository) asks for
//! the disks and the install over a Unix socket.
//!
//! It runs as root, as `ferrix-installd.service`, and only on a live
//! session: it exits at once unless the kernel's command line says
//! `ferrix.live`, which the live medium's `CMDLINE.TXT` does and the copy
//! on an installed disk no longer does (`installed_cmdline`). The live
//! session's user is the medium's owner, as Ubuntu's live user may `sudo`
//! without a password, so a peer of uid 1000 or 0 may ask; any other uid is
//! refused. One connection carries one request:
//!
//! ```text
//! disks                 min, live, then disk/part/free lines, then ok
//! install + key=value lines + an empty line
//!                       step/progress/log lines, then done or error
//! reboot                ok, and the machine restarts
//! ```
//!
//! The exact lines are the graphical installer's contract; its client and
//! this module say the same.

use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};

use ferrix_install::{disk, layout};
use ferrix_partition::ESP;

use crate::engine::{self, Event, Plan, Space};
use crate::setup::Answers;
use crate::{SECTOR, find_live, live_disk};

/// Where it listens.
pub const SOCKET: &str = "/run/ferrix-installer.sock";

/// The uids that may ask: root, and the live session's user.
const ALLOWED: [u32; 2] = [0, 1000];

/// `--serve`.
pub fn serve() -> Result<(), String> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if !cmdline.split_whitespace().any(|word| word == "ferrix.live") {
        println!("ferrix-installd: not a live session (no ferrix.live); not serving");
        return Ok(());
    }
    let path = std::env::var("FERRIX_INSTALLER_SOCKET").unwrap_or_else(|_| SOCKET.into());
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).map_err(|e| format!("{path}: {e}"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
        .map_err(|e| format!("{path}: {e}"))?;
    println!("ferrix-installd: listening on {path}");
    if let Some((disk, fields)) = unattended() {
        println!("ferrix-installd: unattended answers on {disk}; installing without asking");
        let mut console = Console;
        let ok = run_install(&fields, &mut console, &[disk.as_str()]);
        let then = fields
            .iter()
            .rev()
            .find(|(key, _)| key == "then")
            .map_or("", |(_, value)| value.as_str());
        if ok && then == "poweroff" {
            println!("ferrix-installd: powering off, as the answers ask");
            power_off();
        }
    }
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if let Err(why) = connection(stream) {
            println!("ferrix-installd: {why}");
        }
    }
    Ok(())
}

#[repr(C)]
struct Ucred {
    pid: i32,
    uid: u32,
    gid: u32,
}

unsafe extern "C" {
    fn getsockopt(fd: i32, level: i32, name: i32, value: *mut Ucred, len: *mut u32) -> i32;
}

/// The peer's uid, from `SO_PEERCRED`.
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    const SOL_SOCKET: i32 = 1;
    const SO_PEERCRED: i32 = 17;
    let mut cred = Ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut len = size_of::<Ucred>() as u32;
    // SAFETY: `cred` and `len` are live and sized for the option.
    let got = unsafe {
        getsockopt(
            stream.as_raw_fd(),
            SOL_SOCKET,
            SO_PEERCRED,
            &raw mut cred,
            &raw mut len,
        )
    };
    (got == 0).then_some(cred.uid)
}

fn connection(stream: UnixStream) -> Result<(), String> {
    let uid = peer_uid(&stream).ok_or("a peer whose credentials could not be read")?;
    let mut out = stream.try_clone().map_err(|e| e.to_string())?;
    if !ALLOWED.contains(&uid) {
        let _ = writeln!(out, "error uid {uid} may not install");
        return Err(format!("refused a peer of uid {uid}"));
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    match line.trim() {
        "disks" => disks(&mut out),
        "install" => {
            let fields = read_fields(&mut reader).map_err(|e| e.to_string())?;
            install(&fields, &mut out);
            Ok(())
        }
        "reboot" => {
            let _ = writeln!(out, "ok");
            println!("ferrix-installd: restarting, as the installer asked");
            reboot();
            Ok(())
        }
        other => {
            let _ = writeln!(out, "error unknown request {other:?}");
            Ok(())
        }
    }
    .map_err(|e: String| e)
}

/// The `key=value` lines up to an empty one.
fn read_fields(reader: &mut impl BufRead) -> io::Result<Vec<(String, String)>> {
    let mut fields = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once('=') {
            fields.push((key.to_owned(), value.to_owned()));
        }
    }
    Ok(fields)
}

/// The `disks` answer.
fn disks(out: &mut UnixStream) -> Result<(), String> {
    let text = listing();
    out.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

/// What a disk holds, as the page shows it.
pub fn listing() -> String {
    let mut text = String::new();
    let live = live_disk();
    let esp_bytes = find_live()
        .ok()
        .and_then(|path| std::fs::File::open(path).ok())
        .and_then(|mut file| crate::fat_volume_bytes(&mut file).ok())
        .unwrap_or(1 << 30);
    text.push_str(&format!("min {}\n", engine::needed(esp_bytes)));
    if let Some(live) = &live {
        text.push_str(&format!("live {live}\n"));
    }
    for path in disk::whole_disks() {
        if live.as_deref() == Some(path.as_str()) {
            continue;
        }
        let Ok(mut file) = disk::open(&path) else {
            continue;
        };
        let Ok(bytes) = disk::size(&mut file) else {
            continue;
        };
        let model = model(&path);
        match disk::table(&mut file).ok().flatten() {
            Some(table) => {
                text.push_str(&format!("disk {path} {bytes} gpt {model}\n"));
                for (index, partition) in &table.entries {
                    let fs = file_system(&mut file, partition.first, partition.type_guid == ESP);
                    text.push_str(&format!(
                        "part {path} {} {} {} {} {fs} {}\n",
                        index + 1,
                        partition.first,
                        partition.last,
                        (partition.last - partition.first + 1) * SECTOR,
                        disk::name(partition)
                    ));
                }
                for (first, last) in engine::free_ranges(&table) {
                    text.push_str(&format!(
                        "free {path} {first} {last} {}\n",
                        (last - first + 1) * SECTOR
                    ));
                }
            }
            None => {
                let mut sector = [0_u8; 512];
                let mbr = disk::read_at(&mut file, 0, &mut sector).is_ok()
                    && sector[510..512] == [0x55, 0xAA]
                    && sector[446..510].iter().any(|&b| b != 0);
                let table = if mbr { "mbr" } else { "none" };
                text.push_str(&format!("disk {path} {bytes} {table} {model}\n"));
            }
        }
    }
    text.push_str("ok\n");
    text
}

/// What sysfs calls the disk, or a plain word for it.
fn model(path: &str) -> String {
    let name = path.trim_start_matches("/dev/");
    for file in ["device/model", "device/name"] {
        if let Ok(text) = std::fs::read_to_string(format!("/sys/class/block/{name}/{file}")) {
            let text = text.trim();
            if !text.is_empty() {
                return text.replace('\n', " ");
            }
        }
    }
    "Disk".to_owned()
}

/// The file system a partition starting at `first` holds, by its magic.
fn file_system(file: &mut std::fs::File, first: u64, esp: bool) -> &'static str {
    let mut head = vec![0_u8; layout::HEAD_BYTES];
    if disk::read_at(file, first * SECTOR, &mut head).is_err() {
        return "unknown";
    }
    if &head[82..87] == b"FAT32" || &head[54..59] == b"FAT16" || &head[54..59] == b"FAT12" {
        return "fat";
    }
    match layout::foreign(&head) {
        Some("swap") => "swap",
        Some("btrfs") => "btrfs",
        Some("NTFS") => "ntfs",
        Some("ext2/3/4") => "ext4",
        _ if esp => "fat",
        _ => "unknown",
    }
}

/// The unattended answers, as Ubuntu's autoinstall reads a disk labelled
/// `CIDATA`: a GUID partition named [`ANSWERS_NAME`] whose bytes start with
/// [`ANSWERS_MAGIC`], then the `install` request's `key=value` lines and,
/// optionally, `then=poweroff`. The disk it is on, and the fields.
fn unattended() -> Option<(String, Vec<(String, String)>)> {
    for path in disk::whole_disks() {
        let Ok(mut file) = disk::open(&path) else {
            continue;
        };
        let Ok(Some(table)) = disk::table(&mut file) else {
            continue;
        };
        for (_, partition) in &table.entries {
            if disk::name(partition) != ANSWERS_NAME {
                continue;
            }
            let mut head = vec![0_u8; 4096];
            if disk::read_at(&mut file, partition.first * SECTOR, &mut head).is_err() {
                continue;
            }
            let text = String::from_utf8_lossy(&head);
            let text = text.trim_end_matches('\0');
            let Some(body) = text.strip_prefix(ANSWERS_MAGIC) else {
                continue;
            };
            let fields = read_fields(&mut body.as_bytes()).ok()?;
            return Some((path, fields));
        }
    }
    None
}

/// The answers partition's name and its first line.
const ANSWERS_NAME: &str = "FERRIX-ANSWERS";
const ANSWERS_MAGIC: &str = "ferrix-answers 1\n";

/// Where an unattended install says what it does: the console.
struct Console;

impl Write for Console {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(bytes);
        for line in text.lines().filter(|line| !line.starts_with("progress ")) {
            println!("ferrix-installd: {line}");
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The `install` request.
fn install(fields: &[(String, String)], out: &mut UnixStream) {
    let _ = run_install(fields, out, &[]);
}

/// Run the install `fields` ask for, saying it to `out`; `busy` are disks
/// it may never take (the answers' own). Whether it succeeded.
fn run_install(fields: &[(String, String)], out: &mut dyn Write, busy: &[&str]) -> bool {
    let field = |key: &str| {
        fields
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    let say = |out: &mut dyn Write, line: String| {
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    };
    let plan = || -> Result<Plan, String> {
        let number = |key: &str| {
            field(key)
                .parse::<u64>()
                .map_err(|_| format!("{key} is not a number"))
        };
        let space = match field("mode").as_str() {
            "erase" => Space::Erase,
            "free" => Space::Free {
                first: number("first")?,
                last: number("last")?,
            },
            "partition" => Space::Partition {
                number: u32::try_from(number("number")?).map_err(|e| e.to_string())?,
            },
            other => return Err(format!("{other:?} is not a way to install")),
        };
        let answers = Answers {
            user: field("user"),
            fullname: field("fullname"),
            password: field("password"),
            hostname: field("hostname"),
            keyboard: field("keyboard"),
            variant: field("variant"),
            timezone: field("timezone"),
            language: field("language"),
        };
        answers.check()?;
        let target = field("target");
        if !disk::whole_disks().contains(&target) {
            return Err(format!("{target:?} is not a disk"));
        }
        if busy.contains(&target.as_str()) {
            return Err(format!("{target} holds the unattended answers"));
        }
        if live_disk().as_deref() == Some(target.as_str()) {
            return Err(format!("{target} is the disk the live system runs from"));
        }
        Ok(Plan {
            target,
            space,
            answers: Some(answers),
        })
    };
    let result = plan().and_then(|plan| {
        let live = find_live()?;
        println!(
            "ferrix-installd: installing onto {} from {live} for {}",
            plan.target,
            plan.answers.as_ref().map_or("", |a| a.user.as_str())
        );
        engine::install(&plan, &live, &mut |event| match event {
            Event::Step(n, of, text) => say(out, format!("step {n} {of} {text}\n")),
            Event::Progress(percent) => say(out, format!("progress {percent}\n")),
            Event::Log(text) => {
                println!("{text}");
                say(out, format!("log {text}\n"));
            }
        })
    });
    match result {
        Ok(()) => {
            say(out, "done\n".into());
            true
        }
        Err(why) => {
            println!("ferrix-installd: the install failed: {why}");
            say(out, format!("error {}\n", why.replace('\n', " ")));
            false
        }
    }
}

/// Power the machine off: init's command, else the system call.
fn power_off() {
    for program in ["/bin/poweroff", "/sbin/poweroff"] {
        if std::path::Path::new(program).exists()
            && std::process::Command::new(program)
                .status()
                .is_ok_and(|status| status.success())
        {
            return;
        }
    }
    // SAFETY: plain system calls with constant arguments.
    unsafe {
        sync();
        let _ = sys_reboot(0x4321_fedc_u32 as i32);
    }
}

unsafe extern "C" {
    fn sync();
    #[link_name = "reboot"]
    fn sys_reboot(how: i32) -> i32;
}

/// Restart the machine: through init's own command where there is one,
/// else the system call.
fn reboot() {
    for program in ["/bin/reboot", "/sbin/reboot"] {
        if std::path::Path::new(program).exists()
            && std::process::Command::new(program)
                .status()
                .is_ok_and(|status| status.success())
        {
            return;
        }
    }
    // SAFETY: plain system calls with constant arguments.
    unsafe {
        sync();
        let _ = sys_reboot(0x0123_4567);
    }
}
