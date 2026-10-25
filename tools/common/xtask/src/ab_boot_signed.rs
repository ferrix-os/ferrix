//! `test-ab-boot-signed`: signed slots under OVMF (`docs/SIGNING.md` §8.4,
//! §10.6), on the disk `test-ab-boot` lays out.
//!
//! The loader is built three times from one commit: with the run's own
//! trust-policy block (the shipped loader), with `development-required`
//! (the gate's relinked loader), and with the run's block again. The
//! first and third must be the same bytes (the build is reproducible), and
//! the first and second may differ only inside `.fxanch` and the PE
//! checksum (§6.4). A fourth build, `development-off`, is the loader of a
//! development image with signing off. Only the gate's images carry
//! development anchors, under `build/x86_64/signing-gate/`, never
//! `build/x86_64/ferrix.img`.
//!
//! The slots are signed by the development soft signer with nazuna's
//! development slot key (anchor 32768, `signing-anchors.toml`), so the gate
//! runs on nazuna (§6.4, D5).
//!
//! | case | loader | setup (record: good B, A the fallback) | required |
//! |---|---|---|---|
//! | S1 | gate | B signed as a configuration package | B refused `WrongKind`; A boots, its own `ferrix.slot.sig=0` overridden (N4) |
//! | S2 | gate | B's kernel byte flipped after signing | B refused at its kernel's digest; A boots |
//! | S3 | gate | B signed for the N150's family | B refused `WrongFamily`; A boots |
//! | S10 | gate | B's header with generation 1, resealed | B refused `Header(Generation)`; A boots |
//! | N1 | development-off | B signed by a key that is not the anchor's | B refused `BadSignature`; A boots |
//! | N2 | gate | both slots header version 1, the ESP's unsigned files present | both refused `Unsigned`; the unsigned files are not booted: no RESCUE.SLT, the loader panics |
//! | N3 | shipped (release table, empty) | B signed | B refused `UnknownAnchor`; A, version 1, boots unsigned |
//! | R1 | gate | B wrong kind, A's kernel flipped, a signed RESCUE.SLT | the rescue image boots |
//!
//! Every failure names its case in a line beginning `test-ab-boot-signed:`.

use std::path::PathBuf;
use std::process::Command;

use ferrix_slots::Slot;
use ferrix_slots::record::Record;
use ferrix_slots::slot::{HEADER_LEN, SlotHeader, VERSION_1, VERSION_2};

use super::{Bench, Layout, RESCUE_CMDLINE, Setup, copies, swap_pattern};
use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result, cargo, fat, initramfs, installer, native, signing};

/// The development slot key's label and anchor id (`signing-anchors.toml`).
const SLOT_KEY: &str = "dev-slot";
const SLOT_ANCHOR: &str = "32768";
/// The configuration key, to sign a manifest of the wrong kind.
const CONFIG_KEY: &str = "dev-config";
/// The manifest profile byte of `desktop`, the gate's profile (level none),
/// and its arch.
const PROFILE: &str = "4";
const ARCH: &str = "1";
/// x86-64 UEFI PC, and the N150's family, which this machine is not.
const PC: &str = "257";
const N150: &str = "258";
/// A's own command line, with a forged signature word after the loader's
/// (N4): the loader's first word is the one that counts.
const A_CMDLINE: &str = "ferrix.slot.sig=0 ferrix.root=tmpfs";
/// B's.
const B_CMDLINE: &str = "ferrix.root=tmpfs";

fn fail(what: &str) -> Error {
    Error::new(format!("test-ab-boot-signed: {what}"))
}

fn require(lines: &[String], wanted: &str, problem: &str) -> Result<()> {
    if lines.iter().any(|line| line.contains(wanted)) {
        Ok(())
    } else {
        Err(fail(&format!("{problem} (no `{wanted}`)")))
    }
}

fn refuse(lines: &[String], wanted: &str, problem: &str) -> Result<()> {
    if lines.iter().any(|line| line.contains(wanted)) {
        Err(fail(&format!("{problem} (`{wanted}` was printed)")))
    } else {
        Ok(())
    }
}

/// The gate's own directory: nothing it writes goes where a shipped image
/// does.
fn gate_dir(arch: Arch) -> PathBuf {
    paths::build_dir(arch).join("signing-gate")
}

/// Build the loader with `table` (or the run's own, for `None`), and keep a
/// copy of it named `name` in the gate's directory.
fn loader_with(arch: Arch, release: bool, table: Option<&str>, name: &str) -> Result<PathBuf> {
    signing::set_gate_table(table);
    let built = cargo::build_loader(arch, release);
    signing::set_gate_table(None);
    let copy = gate_dir(arch).join(name);
    std::fs::create_dir_all(gate_dir(arch))?;
    let _ = std::fs::copy(built?, &copy)?;
    Ok(copy)
}

/// A slot image, header `version`, its manifest block zero.
fn image(
    version: u16,
    label: &str,
    kernel: &[u8],
    initrd: &[u8],
    cmdline: &str,
) -> (SlotHeader, Vec<u8>) {
    let header = SlotHeader::lay_out(version, 0, label, kernel, initrd, cmdline.as_bytes());
    let mut bytes = vec![0_u8; usize::try_from(header.needed()).unwrap_or(0)];
    if let Some(head) = bytes.get_mut(..HEADER_LEN) {
        head.copy_from_slice(&header.encode());
    }
    for (blob, data) in header
        .blobs()
        .iter()
        .zip([kernel, initrd, cmdline.as_bytes()])
    {
        let at = usize::try_from(blob.offset).unwrap_or(usize::MAX);
        if let Some(place) = bytes.get_mut(at..at + data.len()) {
            place.copy_from_slice(data);
        }
    }
    (header, bytes)
}

/// Sign `bytes`' header (a version-2 slot image) with the soft signer, and
/// put the manifest block in place: its payload is header bytes 0..4064,
/// whose SHA-256 is the seal (`docs/SIGNING.md` §1.2).
fn sign(arch: Arch, bytes: &mut [u8], key: &str, extra: &[&str]) -> Result<()> {
    let dir = gate_dir(arch);
    let payload = dir.join("payload.bin");
    let out = dir.join("manifest.bin");
    std::fs::write(&payload, bytes.get(..HEADER_LEN - 32).unwrap_or_default())?;
    // The signer takes the first of a flag given twice, so a case's own
    // flags go first and the defaults only where the case gives none.
    let mut overridden = Command::new(cargo::cargo());
    let _ = overridden
        .current_dir(paths::workspace_root())
        .args([
            "run",
            "-q",
            "--release",
            "-p",
            "ferrix-softsign",
            "--",
            "sign",
            key,
        ])
        .args(extra);
    for pair in [
        ["--kind", "slot"],
        ["--anchor-id", SLOT_ANCHOR],
        ["--profile", PROFILE],
        ["--arch", ARCH],
        ["--family", PC],
        ["--security-version", "1"],
        ["--label", "test-ab-boot-signed"],
    ] {
        if !extra.contains(&pair[0]) {
            let _ = overridden.args(pair);
        }
    }
    let _ = overridden
        .arg("--payload")
        .arg(&payload)
        .arg("--out")
        .arg(&out);
    let status = overridden
        .status()
        .map_err(|error| Error::new(format!("running the soft signer: {error}")))?;
    if !status.success() {
        return Err(fail("the soft signer refused to sign"));
    }
    let manifest = std::fs::read(&out)?;
    bytes
        .get_mut(HEADER_LEN..HEADER_LEN * 2)
        .ok_or_else(|| fail("a slot image shorter than its manifest block"))?
        .copy_from_slice(&manifest);
    Ok(())
}

/// The gate's ESP size: room for the loader, a rescue image, or the
/// unsigned files N2 puts beside it (as large as `run`'s image).
const ESP_BYTES: usize = 512 << 20;

/// An ESP of `files`, hidden where the layout puts it.
fn esp(files: &[(&str, &[u8])], first: u64) -> Result<Vec<u8>> {
    let mut fs = fat::Fat32::new(ESP_BYTES)?;
    for (path, data) in files {
        fs.add_file(path, data)?;
    }
    let mut volume = fs.finish();
    installer::hide(&mut volume, first)?;
    Ok(volume)
}

/// The four loaders, read.
struct Loaders {
    shipped: Vec<u8>,
    gate: Vec<u8>,
    off: Vec<u8>,
}

/// Build the loaders, and hold the gate's to the shipped one (§6.4; ledger
/// 4016 (5)): the same bytes on a rebuild, and only `.fxanch` and the PE
/// checksum apart from the gate's.
fn loaders(arch: Arch, release: bool) -> Result<Loaders> {
    let shipped = std::fs::read(loader_with(arch, release, None, "shipped.efi")?)?;
    let gate = std::fs::read(loader_with(
        arch,
        release,
        Some("development-required"),
        "gate.efi",
    )?)?;
    let again = std::fs::read(loader_with(arch, release, None, "shipped-again.efi")?)?;
    let off = std::fs::read(loader_with(
        arch,
        release,
        Some("development-off"),
        "development-off.efi",
    )?)?;
    signing::compare_rebuilt(&shipped, &again)
        .map_err(|error| fail(&format!("relink: {error}")))?;
    signing::compare_relinked(&shipped, &gate)
        .map_err(|error| fail(&format!("relink: {error}")))?;
    let (entries, signatures) =
        signing::derived_fields(&shipped).map_err(|error| fail(&format!("relink: {error}")))?;
    if signing::derived_fields(&gate).ok() != Some((entries, signatures)) {
        return Err(fail(
            "relink: the gate's loader skips other fields than the shipped one",
        ));
    }
    println!(
        "  {arch}: relink: skipped only .fxanch, the COFF timestamp, the checksum, {entries} debug \
         timestamp(s) and {signatures} CodeView signature(s)"
    );
    signing::set_gate_table(Some("development-required"));
    let checked = signing::check_loader_policy(arch, "the gate's loader", &gate);
    signing::set_gate_table(None);
    checked?;
    println!("  {arch}: relink: the gate's loader differs from the shipped one only in .fxanch");
    Ok(Loaders { shipped, gate, off })
}

/// The slot images the cases use.
struct Slots {
    a: Vec<u8>,
    a_flipped: Vec<u8>,
    a_v1: Vec<u8>,
    b: Vec<u8>,
    b_v1: Vec<u8>,
    b_kind: Vec<u8>,
    b_family: Vec<u8>,
    b_other_key: Vec<u8>,
    b_flipped: Vec<u8>,
    b_generation: Vec<u8>,
    rescue: Vec<u8>,
}

/// Flip one byte of a slot image's kernel, which starts at 8192.
fn flipped(image: &[u8]) -> Vec<u8> {
    let mut out = image.to_vec();
    if let Some(byte) = out.get_mut(8192 + 100) {
        *byte ^= 1;
    }
    out
}

fn slots(arch: Arch, kernel: &[u8], initrd: &[u8]) -> Result<Slots> {
    let (_, mut a) = image(VERSION_2, "ferrix signed a", kernel, initrd, A_CMDLINE);
    sign(arch, &mut a, SLOT_KEY, &[])?;
    let (_, unsigned_b) = image(VERSION_2, "ferrix signed b", kernel, initrd, B_CMDLINE);
    let signed = |key: &str, extra: &[&str]| -> Result<Vec<u8>> {
        let mut out = unsigned_b.clone();
        sign(arch, &mut out, key, extra)?;
        Ok(out)
    };
    let b = signed(SLOT_KEY, &[])?;
    let mut b_generation = b.clone();
    let mut header = SlotHeader::decode(&b, b.len() as u64)
        .map_err(|error| fail(&format!("B's own header: {error:?}")))?;
    header.generation = 1;
    if let Some(head) = b_generation.get_mut(..HEADER_LEN) {
        head.copy_from_slice(&header.encode());
    }
    let (_, mut rescue) = image(VERSION_2, "ferrix rescue", kernel, initrd, A_CMDLINE);
    sign(arch, &mut rescue, SLOT_KEY, &[])?;
    Ok(Slots {
        a_flipped: flipped(&a),
        a_v1: image(VERSION_1, "ferrix unsigned a", kernel, initrd, B_CMDLINE).1,
        b_v1: image(VERSION_1, "ferrix unsigned b", kernel, initrd, B_CMDLINE).1,
        b_kind: signed(
            CONFIG_KEY,
            &["--kind", "config", "--anchor-id", "32769", "--subject", "x"],
        )?,
        b_family: signed(SLOT_KEY, &["--family", N150])?,
        b_other_key: signed(CONFIG_KEY, &[])?,
        b_flipped: flipped(&b),
        b_generation,
        a,
        b,
        rescue,
    })
}

/// What every case shares.
struct Gate<'a> {
    arch: Arch,
    kernel: PathBuf,
    layout: Layout,
    args: Args,
    slots: &'a Slots,
}

impl Gate<'_> {
    /// A bench whose ESP holds `loader` and `extra`, on its own disk file.
    fn bench(&self, loader: &[u8], extra: &[(&str, &[u8])], name: &str) -> Result<Bench> {
        let mut files: Vec<(&str, &[u8])> = vec![("EFI/BOOT/BOOTX64.EFI", loader)];
        files.extend_from_slice(extra);
        Ok(Bench {
            arch: self.arch,
            kernel: self.kernel.clone(),
            esp: esp(&files, self.layout.esp.0)?,
            layout: self.layout,
            path: gate_dir(self.arch).join(name),
            args: self.args.clone(),
            swap_hash: ferrix_slots::sha256::digest(&swap_pattern()),
        })
    }

    /// Boot `a` and `b` under the record "good B, A the fallback".
    fn boot(&self, bench: &Bench, case: &str, a: &[u8], b: &[u8]) -> Result<(bool, Vec<String>)> {
        bench.write(&Setup {
            a,
            b,
            copies: copies(Some(Record::initial(Slot::B)), None),
        })?;
        bench.boot(case)
    }

    /// B refused with `refusal`, and A booted as the fallback, its own
    /// forged signature word after the loader's (N4).
    fn fallback(&self, bench: &Bench, case: &str, b: &[u8], refusal: &str) -> Result<()> {
        let (panicked, lines) = self.boot(bench, case, &self.slots.a, b)?;
        require(
            &lines,
            &format!("sign     refused slot b: {refusal}"),
            &format!("{case}: B was not refused"),
        )
        .or_else(|_| {
            require(
                &lines,
                &format!("slot b (good) does not verify: {refusal}"),
                &format!("{case}: B was not refused"),
            )
        })?;
        require(
            &lines,
            "boot     slot a fallback",
            &format!("{case}: A did not boot as the fallback"),
        )?;
        require(
            &lines,
            "ferrix.slot.sig=32768 ferrix.slot.sig=0 ferrix.root=tmpfs",
            &format!(
                "{case}: A's command line does not begin with the loader's signature word (N4)"
            ),
        )?;
        if panicked {
            return Err(fail(&format!("{case}: A's system panicked")));
        }
        println!("  {case}: held");
        Ok(())
    }
}

/// N2: header version 1 under `required`, the unsigned files on the ESP.
fn case_n2(gate: &Gate<'_>, loader: &[u8], kernel: &[u8], initramfs: &[u8]) -> Result<()> {
    let bench = gate.bench(
        loader,
        &[
            ("FERRIX/KERNEL.ELF", kernel),
            ("FERRIX/INITRD.IMG", initramfs),
            ("FERRIX/CMDLINE.TXT", RESCUE_CMDLINE.as_bytes()),
        ],
        "unsigned.img",
    )?;
    let slots = gate.slots;
    let (panicked, lines) = gate.boot(
        &bench,
        "N2, version-1 slots where signing is required",
        &slots.a_v1,
        &slots.b_v1,
    )?;
    require(
        &lines,
        "sign     refused slot b: Unsigned",
        "N2: B was not refused as unsigned",
    )?;
    require(
        &lines,
        "sign     refused slot a: Unsigned",
        "N2: A was not refused as unsigned",
    )?;
    refuse(
        &lines,
        "boot     esp",
        "N2: the ESP's unsigned files were booted",
    )?;
    require(
        &lines,
        "the ESP holds no RESCUE.SLT",
        "N2: the loader did not stop for want of a rescue image",
    )?;
    if !panicked {
        return Err(fail("N2: the loader did not stop"));
    }
    println!("  N2: held");
    Ok(())
}

/// N3: the shipped loader, whose release table is empty.
fn case_n3(gate: &Gate<'_>, loader: &[u8]) -> Result<()> {
    let bench = gate.bench(loader, &[], "shipped.img")?;
    let slots = gate.slots;
    let (panicked, lines) = gate.boot(
        &bench,
        "N3, a signed slot under an empty release table",
        &slots.a_v1,
        &slots.b,
    )?;
    require(
        &lines,
        "sign     refused slot b: UnknownAnchor",
        "N3: B was not refused",
    )?;
    require(
        &lines,
        "boot     slot a fallback",
        "N3: A, unsigned, did not boot under off",
    )?;
    require(
        &lines,
        "ferrix.slot.sig=none",
        "N3: A's command line does not say it is unsigned",
    )?;
    if panicked {
        return Err(fail("N3: A's system panicked"));
    }
    println!("  N3: held");
    Ok(())
}

/// R1: no slot starts, and a signed RESCUE.SLT does.
fn case_r1(gate: &Gate<'_>, loader: &[u8]) -> Result<()> {
    let slots = gate.slots;
    let bench = gate.bench(
        loader,
        &[("FERRIX/RESCUE.SLT", &slots.rescue)],
        "rescue.img",
    )?;
    let (panicked, lines) = gate.boot(
        &bench,
        "R1, both slots refused, a signed rescue image",
        &slots.a_flipped,
        &slots.b_kind,
    )?;
    require(
        &lines,
        "sign     refused slot b: WrongKind",
        "R1: B was not refused",
    )?;
    require(
        &lines,
        "slot a (fallback) does not verify",
        "R1: A was not refused",
    )?;
    require(
        &lines,
        "boot     rescue",
        "R1: the rescue image did not boot",
    )?;
    require(
        &lines,
        "ferrix.slot.sig=32768 ferrix.slot.sig=0 ferrix.root=tmpfs",
        "R1: the rescue image's command line does not begin with the loader's signature word (N4)",
    )?;
    if panicked {
        return Err(fail("R1: the rescue system panicked"));
    }
    println!("  R1: held");
    Ok(())
}

/// `cargo xtask test-ab-boot-signed`.
pub(crate) fn test_ab_boot_signed(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-ab-boot-signed runs on x86-64 only, as test-ab-boot does",
        ));
    }
    // The gate's image carries development anchors, so it is built at level
    // none (docs/SIGNING.md §6.4: `level=none anchors=development`, no
    // claim), and S3's refusal of development anchors in a claimed image
    // stays as it is: under a claimed profile the gate's loader is refused.
    let run = crate::flags::this_run()?;
    if run.level != "none" {
        return Err(fail(&format!(
            "the gate's image carries development anchors and must be built at level none; \
             this run is profile {} at level {}: run it with --kernel-profile desktop",
            run.profile, run.level
        )));
    }
    let natives = native::build(arch, args.release)?;
    let initramfs = initramfs::build(None, &natives, None, &[])?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let carried = kernel.initramfs(&initramfs)?;
    let kernel_bytes = std::fs::read(&*kernel)?;
    let loaders = loaders(arch, args.release)?;
    let slots = slots(arch, &kernel_bytes, &carried)?;
    let largest = [&slots.a, &slots.b, &slots.a_v1, &slots.b_v1]
        .iter()
        .map(|slot| slot.len())
        .max()
        .unwrap_or(0) as u64;
    let mut booted = args.clone();
    booted.boot_virtio = true;
    let gate = Gate {
        arch,
        kernel: kernel.to_path_buf(),
        layout: Layout::new(ESP_BYTES as u64, largest + (1 << 20)),
        args: booted,
        slots: &slots,
    };
    let gated = gate.bench(&loaders.gate, &[], "gate.img")?;
    gate.fallback(
        &gated,
        "S1, B signed as a configuration package",
        &slots.b_kind,
        "WrongKind",
    )?;
    gate.fallback(
        &gated,
        "S2, B's kernel flipped after signing",
        &slots.b_flipped,
        "its kernel does not match its digest",
    )?;
    gate.fallback(
        &gated,
        "S3, B signed for another family",
        &slots.b_family,
        "WrongFamily",
    )?;
    gate.fallback(
        &gated,
        "S10, B's header with a generation",
        &slots.b_generation,
        "its header: Generation",
    )?;
    let developer = gate.bench(&loaders.off, &[], "development-off.img")?;
    gate.fallback(
        &developer,
        "N1, B signed by another key, signing off",
        &slots.b_other_key,
        "BadSignature",
    )?;
    case_n2(&gate, &loaders.gate, &kernel_bytes, &initramfs)?;
    case_n3(&gate, &loaders.shipped)?;
    case_r1(&gate, &loaders.gate)?;
    println!(
        "  {arch}: test-ab-boot-signed: relink, S1-S3, S10, N1-N4 and R1 held; p1 unchanged after every boot"
    );
    Ok(())
}
