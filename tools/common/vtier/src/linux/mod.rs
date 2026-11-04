//! The FUSE device, the mount and the status file.
mod mount;
mod ocl;
mod verify;
mod watch;

use ferrix_vtier::store::{BLOCK, Backend, Layout, Ram, Store};
use ferrix_vtier::tree::Tree;
use ferrix_vtier::wire::{MAX_WRITE, serve};
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

struct Options {
    backend: String,
    gib: u64,
    status: PathBuf,
    device: String,
    mountpoint: PathBuf,
}

fn options() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let (mut backend, mut gib, mut status, mut device, mut mountpoint) =
        (String::new(), 0, None, String::from("RTX 3090"), None);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--backend" => backend = value()?,
            "--gib" => gib = value()?.parse().map_err(|_| "--gib wants a number")?,
            "--status" => status = Some(PathBuf::from(value()?)),
            "--device" => device = value()?,
            _ if arg.starts_with('-') => return Err(format!("no option {arg}")),
            _ => mountpoint = Some(PathBuf::from(arg)),
        }
    }
    if gib == 0 {
        return Err("--gib is required".into());
    }
    Ok(Options {
        backend,
        gib,
        status: status.ok_or("--status is required")?,
        device,
        mountpoint: mountpoint.ok_or("no mountpoint")?,
    })
}

/// Counts each status written: `verify` wants one newer than it last saw.
static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Replace the status file whole, so a reader sees one state or the other.
fn write_status<B: Backend>(tree: &Tree<B>, path: &Path, instance: &str) {
    let store = &tree.store;
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let text = store.health.render(
        instance,
        seq,
        store.blocks() - store.free_blocks(),
        store.blocks(),
    );
    let temporary = path.with_extension("tmp");
    if std::fs::write(&temporary, text).is_ok() {
        let _ = std::fs::rename(&temporary, path);
    }
}

fn serve_all<B: Backend>(
    mut device: File,
    mut tree: Tree<B>,
    status: &Path,
    instance: &str,
) -> Result<(), String> {
    let mut request = vec![0; MAX_WRITE as usize + 4096 * 2];
    let mut reported = 0;
    write_status(&tree, status, instance);
    loop {
        let length = match device.read(&mut request) {
            Ok(length) => length,
            // A request the kernel took back while this was on its way here.
            Err(e)
                if matches!(e.raw_os_error(), Some(2 | 4 | 11))
                    || e.kind() == ErrorKind::Interrupted =>
            {
                continue;
            }
            // ENODEV: unmounted.
            Err(e) if e.raw_os_error() == Some(19) => return Ok(()),
            Err(e) => return Err(format!("reading the FUSE device: {e}")),
        };
        let reply = serve(&mut tree, &request[..length]);
        // Before the reply: whoever sees this failure can already read about it.
        if tree.store.changes != reported {
            reported = tree.store.changes;
            write_status(&tree, status, instance);
            if let Some(why) = &tree.store.health.dead {
                eprintln!("vtier: FATAL: {why}");
            }
        }
        if let Some(reply) = reply {
            // ENOENT: the kernel gave up on that request. The rest is the kernel's to say.
            if let Err(e) = device.write_all(&reply)
                && e.raw_os_error() != Some(2)
            {
                return Err(format!("writing the FUSE device: {e}"));
            }
        }
    }
}

fn run() -> Result<(), String> {
    if std::env::args().nth(1).as_deref() == Some("verify") {
        return verify::run();
    }
    let o = options()?;
    let total = o.gib << 30;
    // A name no two runs of this share: a store back after a reset is another store.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let instance = format!("{}-{}", now.as_nanos(), std::process::id());
    let who = mount::ids();
    match o.backend.as_str() {
        "ram" => {
            let blocks = u32::try_from(total / BLOCK as u64).map_err(|_| "too big")?;
            let device = mount::mount(&o.mountpoint)?;
            let watched = watch::start(
                Ram(vec![0; total as usize]),
                &o.mountpoint,
                &o.status,
                &instance,
            );
            serve_all(
                device,
                Tree::new(Store::new(watched, blocks), who.0, who.1),
                &o.status,
                &instance,
            )
        }
        "ocl" => {
            let card = ocl::Card::open(&o.device, total)?;
            eprintln!(
                "vtier: {} buffers on {}, largest allocation {} bytes",
                card.buffers(),
                card.name(),
                card.cap()
            );
            let blocks =
                u32::try_from(Layout::new(total, card.cap()).blocks).map_err(|_| "too big")?;
            let device = mount::mount(&o.mountpoint)?;
            let watched = watch::start(card, &o.mountpoint, &o.status, &instance);
            serve_all(
                device,
                Tree::new(Store::new(watched, blocks), who.0, who.1),
                &o.status,
                &instance,
            )
        }
        other => Err(format!("no backend {other:?}: ocl or ram")),
    }
}

pub(crate) fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("vtier: {why}");
            ExitCode::FAILURE
        }
    }
}
