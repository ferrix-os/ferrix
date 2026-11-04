//! A device call that never returns gives no error: every process using the store
//! would wait for ever. This thread gives it a deadline.
use ferrix_vtier::store::{Backend, Health};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long one device call may take. A block is 128 KiB; a loaded card takes
/// milliseconds, and nothing short of a lost device takes this.
const DEADLINE: u64 = 60;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A backend whose calls are timed.
pub(super) struct Watched<B> {
    inner: B,
    started: Arc<AtomicU64>,
}

impl<B> Watched<B> {
    fn during<T>(&mut self, call: impl FnOnce(&mut B) -> T) -> T {
        self.started.store(now(), Ordering::Relaxed);
        #[cfg(feature = "fault-inject")]
        while ferrix_vtier::inject::stall_now() {
            std::thread::sleep(Duration::from_secs(1));
        }
        let result = call(&mut self.inner);
        self.started.store(0, Ordering::Relaxed);
        result
    }
}

impl<B: Backend> Backend for Watched<B> {
    fn read(&mut self, at: u64, to: &mut [u8]) -> Result<(), i32> {
        self.during(|b| b.read(at, to))
    }
    fn write(&mut self, at: u64, from: &[u8]) -> Result<(), i32> {
        self.during(|b| b.write(at, from))
    }
    fn finish(&mut self) -> Result<(), i32> {
        self.during(Backend::finish)
    }
}

/// The kernel's number for this mount, read off the mount table: a stat of the mount
/// point would wait on this very process, which is not serving yet.
fn connection_of(mountpoint: &Path) -> Option<String> {
    let table = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let line = table.lines().find(|l| {
        l.split(' ')
            .nth(4)
            .is_some_and(|m| Path::new(m) == mountpoint)
    })?;
    line.split(' ')
        .nth(2)?
        .rsplit(':')
        .next()
        .map(str::to_owned)
}

/// Wrap `inner`, and watch it from a thread: past the deadline the status file
/// says dead, the kernel's connection is aborted, so that every waiter gets an
/// error, and this process ends, which takes the mount with it.
pub(super) fn start<B>(inner: B, mountpoint: &Path, status: &Path, instance: &str) -> Watched<B> {
    let started = Arc::new(AtomicU64::new(0));
    let watched = Arc::clone(&started);
    let connection = connection_of(mountpoint);
    let (status, instance) = (status.to_path_buf(), instance.to_owned());
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let since = watched.load(Ordering::Relaxed);
            if since != 0 && now().saturating_sub(since) >= DEADLINE {
                let why = format!("a device call has not returned in {DEADLINE} s");
                eprintln!("vtier: FATAL: {why}");
                let health = Health {
                    dead: Some(why),
                    device_errors: 1,
                    ..Health::default()
                };
                // Past every sequence number the serving thread can have written.
                let text = health.render(&instance, u64::MAX, 0, 0);
                let temporary = status.with_extension("tmp");
                if std::fs::write(&temporary, text).is_ok() {
                    let _ = std::fs::rename(&temporary, &status);
                }
                if let Some(minor) = &connection {
                    let _ = std::fs::write(format!("/sys/fs/fuse/connections/{minor}/abort"), "1");
                }
                #[expect(
                    clippy::exit,
                    reason = "AUDIT: the other thread is stuck in the device call; only ending the process takes it away"
                )]
                std::process::exit(3);
            }
        }
    });
    Watched { inner, started }
}
