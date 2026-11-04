//! `vtier verify`: what a verdict stands on. Everything written through the page
//! cache is flushed to the daemon, a sentinel goes to the card and back past the
//! page cache, and a status newer than the one the run began with must say this
//! same store has had no fault. Anything short of that is a failure.
//!
//! ```text
//! vtier verify --status FILE [--since SEQ --instance ID] [--allow-inject] MOUNTPOINT
//! ```
use ferrix_vtier::store::BLOCK;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

unsafe extern "C" {
    fn syncfs(fd: i32) -> i32;
}
const O_DIRECT: i32 = 0o40_000;

/// A block's worth of memory aligned as `O_DIRECT` wants.
struct Aligned(Vec<u8>, usize);
impl Aligned {
    fn new() -> Self {
        let buffer = vec![0u8; BLOCK + 4096];
        let skip = buffer.as_ptr().align_offset(4096);
        Self(buffer, skip)
    }
    fn get(&mut self) -> &mut [u8] {
        &mut self.0[self.1..self.1 + BLOCK]
    }
}

/// The status file's fields; every one must be there and the counters nought.
fn judge(
    text: &str,
    since: Option<u64>,
    instance: Option<&str>,
    allow_inject: bool,
) -> Result<(String, u64), String> {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix('=')))
            .ok_or(format!("status has no {name}"))
    };
    let number = |name: &str| {
        field(name)?
            .parse::<u64>()
            .map_err(|_| format!("status: {name} is no number"))
    };
    let id = field("instance")?;
    if id.is_empty() {
        return Err("status names no instance".into());
    }
    if let Some(want) = instance
        && want != id
    {
        return Err(format!("another store: {id}, the run began on {want}"));
    }
    let seq = number("seq")?;
    if let Some(since) = since
        && seq <= since
    {
        return Err(format!(
            "status did not advance: seq {seq}, began at {since}"
        ));
    }
    match field("inject")? {
        "no" => {}
        "yes" if allow_inject => {}
        other => return Err(format!("store built with fault injection: {other}")),
    }
    if field("state")? != "ok" {
        return Err(format!("store dead: {}", field("why").unwrap_or("")));
    }
    for name in ["crc_errors", "device_errors", "no_space"] {
        let count = number(name)?;
        if count != 0 {
            return Err(format!("{name}={count}"));
        }
    }
    Ok((id.to_owned(), seq))
}

fn sentinel(mountpoint: &Path) -> Result<(), String> {
    let root = std::fs::File::open(mountpoint).map_err(|e| format!("opening the mount: {e}"))?;
    // SAFETY: a descriptor open for the length of the call.
    if unsafe { syncfs(root.as_raw_fd()) } != 0 {
        return Err(format!("syncfs: {}", std::io::Error::last_os_error()));
    }
    let path = mountpoint.join(".vtier-sentinel");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(O_DIRECT)
        .open(&path)
        .map_err(|e| format!("sentinel: {e}"))?;
    let mut buffer = Aligned::new();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pattern = stamp.to_le_bytes();
    for (i, byte) in buffer.get().iter_mut().enumerate() {
        *byte = pattern[i % pattern.len()] ^ (i >> 12) as u8;
    }
    let want = buffer.get().to_vec();
    file.write_all(buffer.get())
        .map_err(|e| format!("sentinel write: {e}"))?;
    buffer.get().fill(0);
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("sentinel seek: {e}"))?;
    file.read_exact(buffer.get())
        .map_err(|e| format!("sentinel read: {e}"))?;
    if buffer.get() != want.as_slice() {
        return Err("sentinel came back wrong".into());
    }
    // The barrier: the card is done, and a new status is written.
    file.sync_all().map_err(|e| format!("sentinel fsync: {e}"))
}

/// The subcommand: 0 and `<instance> <seq>` when all is well.
pub(super) fn run() -> Result<(), String> {
    let (mut status, mut since, mut instance, mut allow, mut mountpoint) =
        (None, None, None, false, None::<PathBuf>);
    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--status" => status = Some(PathBuf::from(value()?)),
            "--since" => {
                since = Some(
                    value()?
                        .parse::<u64>()
                        .map_err(|_| "--since wants a number")?,
                );
            }
            "--instance" => instance = Some(value()?),
            "--allow-inject" => allow = true,
            _ => mountpoint = Some(PathBuf::from(arg)),
        }
    }
    let status = status.ok_or("--status is required")?;
    sentinel(&mountpoint.ok_or("no mountpoint")?)?;
    let text = std::fs::read_to_string(&status).map_err(|e| format!("status: {e}"))?;
    let (id, seq) = judge(&text, since, instance.as_deref(), allow)?;
    println!("{id} {seq}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::judge;

    const GOOD: &str = "instance=a\nseq=5\ninject=no\nstate=ok\ncrc_errors=0\ndevice_errors=0\nno_space=0\nused_blocks=1\nblocks=2\nwhy=\n";

    #[test]
    fn a_clean_newer_status_of_the_same_store_passes() {
        assert_eq!(judge(GOOD, Some(4), Some("a"), false), Ok(("a".into(), 5)));
    }
    #[test]
    fn anything_else_fails_closed() {
        for (from, to, says) in [
            ("seq=5", "seq=4", "did not advance"),
            ("instance=a", "instance=b", "another store"),
            ("state=ok", "state=dead", "dead"),
            ("crc_errors=0", "crc_errors=1", "crc_errors"),
            ("device_errors=0", "device_errors=2", "device_errors"),
            ("no_space=0", "no_space=9", "no_space"),
            ("inject=no", "inject=yes", "injection"),
            ("inject=no", "inject=maybe", "injection"),
            ("inject=no\n", "", "no inject"),
            ("seq=5\n", "", "no seq"),
            ("state=ok\n", "", "no state"),
            ("crc_errors=0", "crc_errors=x", "no number"),
        ] {
            let text = GOOD.replace(from, to);
            let said = judge(&text, Some(4), Some("a"), false).unwrap_err();
            assert!(said.contains(says), "{to:?}: {said}");
        }
        assert!(judge("", None, None, false).is_err());
    }
    #[test]
    fn inject_passes_only_when_a_test_asks() {
        let text = GOOD.replace("inject=no", "inject=yes");
        assert!(judge(&text, None, None, true).is_ok());
    }
}
