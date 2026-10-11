//! `test-ab-boot`: A/B system slots under OVMF (`docs/AB-BOOT.md` §9).
//!
//! One virtual disk with the layout of the customer's SN720, scaled down:
//!
//! ```text
//!  p1  a "swap" partition, filled with a pattern     never written: hashed after every boot
//!  p2  the ESP: the loader, and the rescue files     the image `run` boots
//!  p3  the control record (type c29aa8c5-...)        1 MiB
//!  p4  slot A (type 5475b681-...)
//!  p5  slot B
//!  p6  a root partition, empty                       the slots boot with / in memory
//! ```
//!
//! Each boot is a fresh QEMU process on the same disk file: a power cycle,
//! which is what "comes back at the next reset" means (§6.3). The record is
//! written from the host with `ferrix-slots`, as `bootctl` will write it
//! from inside Ferrix; `bootctl good` is the host's [`mark_good`] until
//! ab-sys's `bootctl` lands.
//!
//! | case | setup | required |
//! |---|---|---|
//! | 1 | good A, trial B (a system that panics at boot), 1 try | boot 1 starts B after taking the try, and panics; boot 2 starts A as good |
//! | 2 | good A, trial B (a good system), 1 try | boot 1 starts B as a trial; after `good`, boot 2 starts B as good |
//! | 3 | as 2, the newer record copy corrupted, the older armed | the older copy is used, and the try is written over the corrupt one |
//! | 4 | as 2, a byte of B's initramfs flipped | B does not verify, A boots, nothing is written |
//! | 5 | the record zeroed | A boots, nothing is written |
//! | 6 | A's kernel corrupted, B a failed trial (0 tries) | the ESP's rescue files boot; B never starts |
//!
//! Each boot also requires p1 to hash as it did before the first.
//! Every failure names its case in a line beginning `test-ab-boot:`, which
//! is what `fleet/gate.sh control --expect-line` matches.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use ferrix_partition::{ESP, Guid, LINUX_FILESYSTEM, Partition, write_table};
use ferrix_slots::Slot;
use ferrix_slots::gpt::{CONTROL_TYPE, Part, SLOT_TYPE};
use ferrix_slots::record::{self, ControlDisk, Current, DeviceError, Record};
use ferrix_slots::slot::{HEADER_LEN, SlotHeader};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result, cargo, fat, initramfs, installer, native, qemu};

/// Bytes in a sector of the test disk.
const SECTOR: u64 = 512;
/// One MiB in sectors: every partition starts on one.
const MIB: u64 = 2048;
/// The "swap" partition's size, in MiB.
const SWAP_MIB: u64 = 8;
/// The control partition's size, in MiB, as on the SN720.
const CONTROL_MIB: u64 = 1;
/// The root partition's size, in MiB.
const ROOT_MIB: u64 = 16;
/// Linux swap's partition type.
const SWAP_TYPE: Guid = Guid::from_fields(0x0657_FD6D, 0xA4AB, 0x43C4, 0x84E5, 0x0933_C84B_4F4F);
/// The test disk's GUID, which the loader names as `ferrix.slot.disk`.
const DISK_GUID: Guid = Guid::random([7; 16]);
/// The slots' own command line: `/` in memory, so no boot touches p6.
const SLOT_CMDLINE: &str = "ferrix.root=tmpfs";
/// The ESP's rescue command line.
const RESCUE_CMDLINE: &str = "ferrix.root=tmpfs";
/// What a boot that came up prints.
const BOOTED: &str = "FERRIX-BOOT-OK";

/// The disk's partitions, in sectors, as laid out for one size of slot.
#[derive(Clone, Copy, Debug)]
struct Layout {
    swap: (u64, u64),
    esp: (u64, u64),
    control: (u64, u64),
    a: (u64, u64),
    b: (u64, u64),
    root: (u64, u64),
    sectors: u64,
}

impl Layout {
    /// Partitions one after the other from 1 MiB, each a whole number of MiB.
    fn new(esp_bytes: u64, slot_bytes: u64) -> Layout {
        let mib = |bytes: u64| bytes.div_ceil(SECTOR * MIB) * MIB;
        let mut at = MIB;
        let mut next = |sectors: u64| {
            let range = (at, at + sectors - 1);
            at += sectors;
            range
        };
        let swap = next(SWAP_MIB * MIB);
        let esp = next(mib(esp_bytes));
        let control = next(CONTROL_MIB * MIB);
        let a = next(mib(slot_bytes));
        let b = next(mib(slot_bytes));
        let root = next(ROOT_MIB * MIB);
        Layout {
            swap,
            esp,
            control,
            a,
            b,
            root,
            sectors: at + MIB,
        }
    }

    fn part(range: (u64, u64)) -> Part {
        Part {
            index: 0,
            unique: [0; 16],
            first_lba: range.0,
            last_lba: range.1,
        }
    }
}

/// The disk file as the control partition's device, for writing the record
/// from the host with the same function the loader uses.
struct FileDisk<'a>(&'a File);

impl ControlDisk for FileDisk<'_> {
    fn block_size(&self) -> u32 {
        SECTOR as u32
    }
    fn write_block(&mut self, lba: u64, bytes: &[u8]) -> std::result::Result<(), DeviceError> {
        self.0
            .write_all_at(bytes, lba * SECTOR)
            .map_err(|_| DeviceError)
    }
    fn flush(&mut self) -> std::result::Result<(), DeviceError> {
        self.0.sync_data().map_err(|_| DeviceError)
    }
    fn read_block(&mut self, lba: u64, bytes: &mut [u8]) -> std::result::Result<(), DeviceError> {
        self.0
            .read_exact_at(bytes, lba * SECTOR)
            .map_err(|_| DeviceError)
    }
}

/// A slot's image: the header, then the three blobs where it says.
fn slot_image(label: &str, kernel: &[u8], initrd: &[u8]) -> Vec<u8> {
    let header = SlotHeader::lay_out(1, label, kernel, initrd, SLOT_CMDLINE.as_bytes());
    let mut image = vec![0_u8; usize::try_from(header.needed()).unwrap_or(0)];
    if let Some(head) = image.get_mut(..HEADER_LEN) {
        head.copy_from_slice(&header.encode());
    }
    for (blob, bytes) in header
        .blobs()
        .iter()
        .zip([kernel, initrd, SLOT_CMDLINE.as_bytes()])
    {
        let at = usize::try_from(blob.offset).unwrap_or(usize::MAX);
        if let Some(place) = image.get_mut(at..at + bytes.len()) {
            place.copy_from_slice(bytes);
        }
    }
    image
}

/// What one case starts from.
struct Setup<'a> {
    a: &'a [u8],
    b: &'a [u8],
    /// The record's two copies' bytes, or zeroes.
    copies: [Option<[u8; record::RECORD_LEN]>; 2],
}

/// Everything the cases share.
struct Bench {
    arch: Arch,
    kernel: PathBuf,
    esp: Vec<u8>,
    layout: Layout,
    path: PathBuf,
    args: Args,
    swap_hash: [u8; 32],
}

impl Bench {
    /// Write the disk for one case.
    fn write(&self, setup: &Setup<'_>) -> Result<()> {
        let on = |error: std::io::Error| Error::new(format!("{}: {error}", self.path.display()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.path)
            .map_err(on)?;
        file.set_len(self.layout.sectors * SECTOR).map_err(on)?;
        let l = self.layout;
        let partition = |kind: Guid, range: (u64, u64), seed: u8, name: &str| {
            Partition::new(kind, Guid::random([seed; 16]), range.0, range.1, name)
        };
        let partitions = [
            partition(SWAP_TYPE, l.swap, 1, "nazuna-swap"),
            partition(ESP, l.esp, 2, "ferrix-esp"),
            partition(Guid(CONTROL_TYPE), l.control, 3, "ferrix-bootctl"),
            partition(Guid(SLOT_TYPE), l.a, 4, "ferrix-slot-a"),
            partition(Guid(SLOT_TYPE), l.b, 5, "ferrix-slot-b"),
            partition(LINUX_FILESYSTEM, l.root, 6, "ferrix-root"),
        ];
        let table = write_table(l.sectors, SECTOR as u32, DISK_GUID, &partitions)
            .map_err(|error| Error::new(format!("test-ab-boot's partition table: {error}")))?;
        for (lba, bytes) in table {
            file.write_all_at(&bytes, lba * SECTOR).map_err(on)?;
        }
        file.write_all_at(&swap_pattern(), l.swap.0 * SECTOR)
            .map_err(on)?;
        file.write_all_at(&self.esp, l.esp.0 * SECTOR).map_err(on)?;
        file.write_all_at(setup.a, l.a.0 * SECTOR).map_err(on)?;
        file.write_all_at(setup.b, l.b.0 * SECTOR).map_err(on)?;
        for (copy, bytes) in setup.copies.iter().enumerate() {
            if let Some(bytes) = bytes {
                let offset = record::COPY_OFFSETS.get(copy).copied().unwrap_or(0);
                let at = l.control.0 * SECTOR + offset;
                file.write_all_at(bytes, at).map_err(on)?;
            }
        }
        file.sync_all().map_err(on)
    }

    /// The current record on the disk.
    fn current(&self) -> Result<Current> {
        let file = File::open(&self.path)?;
        record::read_current(&mut FileDisk(&file), &Layout::part(self.layout.control))
            .map_err(|error| Error::new(format!("reading the record back: {error:?}")))
    }

    /// `bootctl good`, from the host: the trial becomes the good slot.
    fn mark_good(&self) -> Result<()> {
        let current = self.current()?;
        let Current::Valid { record, .. } = current else {
            return Err(fail("case 2: no record to mark good"));
        };
        let next = record
            .marked_good()
            .ok_or_else(|| fail("case 2: the record has no trial to mark good"))?;
        let file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        record::write_record(
            &mut FileDisk(&file),
            &Layout::part(self.layout.control),
            &current,
            &next,
        )
        .map_err(|error| fail(&format!("case 2: marking good: {error:?}")))
    }

    /// Boot once; the lines, and whether it panicked.
    fn boot(&self, case: &str) -> Result<(bool, Vec<String>)> {
        println!("  {}: {case}", self.arch);
        let (panicked, lines, log) =
            qemu::boot_until(self.arch, &self.path, &self.kernel, &self.args, BOOTED)?;
        self.swap_unchanged(case, &log)?;
        Ok((panicked, lines))
    }

    /// p1 hashes as it did before the first boot.
    fn swap_unchanged(&self, case: &str, log: &Path) -> Result<()> {
        let mut file = File::open(&self.path)?;
        let _ = file.seek(SeekFrom::Start(self.layout.swap.0 * SECTOR))?;
        let mut swap = vec![0_u8; usize::try_from(SWAP_MIB << 20).unwrap_or(0)];
        file.read_exact(&mut swap)?;
        if ferrix_slots::sha256::digest(&swap) == self.swap_hash {
            Ok(())
        } else {
            Err(fail(&format!(
                "{case}: p1, the swap partition, changed (P4); serial log {}",
                log.display()
            )))
        }
    }
}

/// The swap partition's pattern: every 8 bytes their own offset, so a write
/// anywhere in it changes its hash.
fn swap_pattern() -> Vec<u8> {
    (0..(SWAP_MIB << 20) / 8)
        .flat_map(|at: u64| (at ^ 0x5357_4150_5357_4150).to_le_bytes())
        .collect()
}

/// A gate failure, in the form `fleet/gate.sh control --expect-line` reads.
fn fail(what: &str) -> Error {
    Error::new(format!("test-ab-boot: {what}"))
}

/// Require `wanted` in `lines`, or fail with `problem`.
fn require(lines: &[String], wanted: &str, problem: &str) -> Result<()> {
    if lines.iter().any(|line| line.contains(wanted)) {
        Ok(())
    } else {
        Err(fail(&format!("{problem} (no `{wanted}`)")))
    }
}

/// Require the record on the disk to be `wanted`.
fn require_record(bench: &Bench, wanted: Current, problem: &str) -> Result<()> {
    let current = bench.current()?;
    if current == wanted {
        Ok(())
    } else {
        Err(fail(&format!(
            "{problem}: the record is {current:?}, not {wanted:?}"
        )))
    }
}

/// Two record copies' bytes.
fn copies(first: Option<Record>, second: Option<Record>) -> [Option<[u8; record::RECORD_LEN]>; 2] {
    [first.map(|r| r.encode()), second.map(|r| r.encode())]
}

/// `cargo xtask test-ab-boot`.
pub(crate) fn test_ab_boot(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-ab-boot runs on x86-64 only for now: the disk boots as a virtio-blk-pci \
             disk through OVMF",
        ));
    }
    let natives = native::build(arch, args.release)?;
    let loader = cargo::build_loader(arch, args.release)?;
    let initramfs = initramfs::build(None, &natives, None, &[])?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let carried = kernel.initramfs(&initramfs)?;
    let kernel_bytes = std::fs::read(&*kernel)?;

    // The ESP: the image `run` boots, which holds the loader and the
    // rescue files, its hidden sectors set for where it sits.
    let volume = fat::write_image_with(arch, &loader, &kernel, &initramfs, Some(RESCUE_CMDLINE))?;
    let mut esp = std::fs::read(&volume)?;

    let good = slot_image("ferrix good", &kernel_bytes, &carried);
    // A system that panics at boot: the same kernel, with an initramfs that
    // is not an archive, so stage 8 cannot build the root.
    let panics = slot_image("ferrix panics", &kernel_bytes, &[0xA5; 4096]);
    let slot_bytes = good.len().max(panics.len()) as u64 + (1 << 20);
    let layout = Layout::new(esp.len() as u64, slot_bytes);
    installer::hide(&mut esp, layout.esp.0)?;

    let mut booted = args.clone();
    booted.boot_virtio = true;
    let bench = Bench {
        arch,
        kernel: kernel.to_path_buf(),
        esp,
        layout,
        path: paths::build_dir(arch).join("ab-boot.img"),
        args: booted,
        swap_hash: ferrix_slots::sha256::digest(&swap_pattern()),
    };

    case_1(&bench, &good, &panics)?;
    case_2(&bench, &good)?;
    case_3(&bench, &good)?;
    case_4(&bench, &good)?;
    case_5(&bench, &good)?;
    case_6(&bench, &good, &panics)?;
    println!("  {arch}: test-ab-boot: all six cases held; p1 unchanged after every boot");
    Ok(())
}

/// The quiet record, and the armed one after it.
fn armed(trial: Slot, tries: u8) -> (Record, Record) {
    let quiet = Record::initial(trial.other());
    let armed = quiet.armed(trial, tries).unwrap_or(quiet);
    (quiet, armed)
}

fn case_1(bench: &Bench, good: &[u8], panics: &[u8]) -> Result<()> {
    let (quiet, armed) = armed(Slot::B, 1);
    bench.write(&Setup {
        a: good,
        b: panics,
        copies: copies(Some(quiet), Some(armed)),
    })?;
    let (panicked, lines) = bench.boot("case 1, boot 1: a trial of a system that panics")?;
    require(
        &lines,
        "took a try off slot b: 0 left",
        "case 1, boot 1: the try was not taken",
    )?;
    require(
        &lines,
        "boot     slot b trial seq 3",
        "case 1, boot 1: B was not started as the trial",
    )?;
    if !panicked {
        return Err(fail("case 1, boot 1: B's system did not panic"));
    }
    // Read now, judged after boot 2: a missing decrement then shows as the
    // loop it causes -- B started again -- which is what its control expects.
    let after_boot_1 = bench.current()?;
    let (panicked, lines) = bench.boot("case 1, boot 2: the reset after the panic")?;
    if lines.iter().any(|line| line.contains("boot     slot b")) {
        return Err(fail(
            "case 1, boot 2: started slot b again after its last try (the decrement did not hold)",
        ));
    }
    require(
        &lines,
        "boot     slot a good seq 3",
        "case 1, boot 2: A was not started as good",
    )?;
    require(
        &lines,
        &format!(
            "cmdline  ferrix.slot=a ferrix.slot.boot=good ferrix.slot.seq=3 ferrix.slot.disk={} \
             ferrix.root=tmpfs",
            ferrix_slots::gpt::GuidText(DISK_GUID.0)
        ),
        "case 1, boot 2: the command line does not begin with the loader's words",
    )?;
    let tried = armed.after_try().unwrap_or(armed);
    let wanted = Current::Valid {
        copy: 0,
        record: tried,
    };
    if after_boot_1 != wanted {
        return Err(fail(&format!(
            "case 1, boot 1: the try was not recorded: the record was {after_boot_1:?}, not \
             {wanted:?}"
        )));
    }
    if panicked {
        return Err(fail("case 1, boot 2: A's system panicked"));
    }
    println!("  case 1: the trial that panicked came back on A at the next reset");
    Ok(())
}

fn case_2(bench: &Bench, good: &[u8]) -> Result<()> {
    let (quiet, armed) = armed(Slot::B, 1);
    bench.write(&Setup {
        a: good,
        b: good,
        copies: copies(Some(quiet), Some(armed)),
    })?;
    let (panicked, lines) = bench.boot("case 2, boot 1: a trial of a good system")?;
    require(
        &lines,
        "boot     slot b trial seq 3",
        "case 2, boot 1: B was not started as the trial",
    )?;
    if panicked {
        return Err(fail("case 2, boot 1: B's system panicked"));
    }
    bench.mark_good()?;
    let (panicked, lines) = bench.boot("case 2, boot 2: after good")?;
    require(
        &lines,
        "boot     slot b good seq 4",
        "case 2, boot 2: B was not kept as good",
    )?;
    if panicked {
        return Err(fail("case 2, boot 2: B's system panicked"));
    }
    println!("  case 2: the good trial, marked good, stayed on B");
    Ok(())
}

fn case_3(bench: &Bench, good: &[u8]) -> Result<()> {
    let (_, armed) = armed(Slot::B, 1);
    let mut torn = armed.armed(Slot::B, 2).unwrap_or(armed).encode();
    torn[300] ^= 0x40;
    bench.write(&Setup {
        a: good,
        b: good,
        copies: [Some(armed.encode()), Some(torn)],
    })?;
    let (_, lines) = bench.boot("case 3: the newer record copy torn")?;
    require(
        &lines,
        "control record seq 2 (copy 0)",
        "case 3: the older valid copy was not used",
    )?;
    require(
        &lines,
        "boot     slot b trial seq 3",
        "case 3: B was not started as the trial",
    )?;
    let tried = armed.after_try().unwrap_or(armed);
    require_record(
        bench,
        Current::Valid {
            copy: 1,
            record: tried,
        },
        "case 3: the try was not written over the torn copy",
    )?;
    println!("  case 3: the torn copy was passed over and then rewritten");
    Ok(())
}

fn case_4(bench: &Bench, good: &[u8]) -> Result<()> {
    let (quiet, armed) = armed(Slot::B, 1);
    let mut flipped = good.to_vec();
    let header = SlotHeader::decode(good, flipped.len() as u64)
        .map_err(|error| fail(&format!("case 4: {error:?}")))?;
    let at = usize::try_from(header.initrd.offset + header.initrd.length / 2).unwrap_or(0);
    if let Some(byte) = flipped.get_mut(at) {
        *byte ^= 1;
    }
    bench.write(&Setup {
        a: good,
        b: &flipped,
        copies: copies(Some(quiet), Some(armed)),
    })?;
    let (panicked, lines) = bench.boot("case 4: the trial slot's initramfs corrupted")?;
    require(
        &lines,
        "slot b (trial) does not verify: its initramfs does not match its digest",
        "case 4: B's corrupted initramfs was not refused (P3)",
    )?;
    require(
        &lines,
        "boot     slot a good seq 2",
        "case 4: A was not started as good",
    )?;
    if panicked {
        return Err(fail("case 4: the boot panicked"));
    }
    require_record(
        bench,
        Current::Valid {
            copy: 1,
            record: armed,
        },
        "case 4: the record was written for a trial that did not verify",
    )?;
    println!("  case 4: the corrupted trial was refused, A booted, nothing written");
    Ok(())
}

fn case_5(bench: &Bench, good: &[u8]) -> Result<()> {
    bench.write(&Setup {
        a: good,
        b: good,
        copies: [None, None],
    })?;
    let (panicked, lines) = bench.boot("case 5: the record zeroed")?;
    require(
        &lines,
        "control record uninitialised",
        "case 5: the zeroed record was not seen",
    )?;
    require(
        &lines,
        "boot     slot a good seq 0",
        "case 5: A was not started",
    )?;
    if panicked {
        return Err(fail("case 5: the boot panicked"));
    }
    require_record(
        bench,
        Current::Uninitialised,
        "case 5: the loader wrote a record",
    )?;
    println!("  case 5: an uninitialised record booted A and wrote nothing");
    Ok(())
}

fn case_6(bench: &Bench, good: &[u8], panics: &[u8]) -> Result<()> {
    let (quiet, armed) = armed(Slot::B, 1);
    let failed = armed.after_try().unwrap_or(armed);
    let mut broken = good.to_vec();
    if let Some(byte) = broken.get_mut(HEADER_LEN + 100) {
        *byte ^= 1;
    }
    bench.write(&Setup {
        a: &broken,
        b: panics,
        copies: copies(Some(quiet), Some(failed)),
    })?;
    let (panicked, lines) = bench.boot("case 6: good slot broken, the other a failed trial")?;
    if lines.iter().any(|line| line.contains("boot     slot b")) {
        return Err(fail(
            "case 6: started slot b, a failed trial, before the ESP's rescue files (P2)",
        ));
    }
    require(
        &lines,
        "slot a (good) does not verify: its kernel",
        "case 6: A's broken kernel was not refused",
    )?;
    require(
        &lines,
        "boot     esp",
        "case 6: the ESP's rescue files were not booted",
    )?;
    if panicked {
        return Err(fail("case 6: the rescue boot panicked"));
    }
    println!("  case 6: a broken good slot and a failed trial: the ESP's rescue files booted");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Layout, swap_pattern};

    #[test]
    fn the_layout_is_whole_mebibytes_in_order() {
        let layout = Layout::new(64 << 20, 40 << 20);
        let ranges = [
            layout.swap,
            layout.esp,
            layout.control,
            layout.a,
            layout.b,
            layout.root,
        ];
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].1 + 1, pair[1].0);
        }
        for (first, last) in ranges {
            assert_eq!(first % 2048, 0);
            assert_eq!((last + 1) % 2048, 0);
        }
        assert_eq!(swap_pattern().len(), 8 << 20);
    }
}
