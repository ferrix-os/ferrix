//! `btrfsd`'s rows and what an image under `fs=server` carries; `test-kernel-shape`
//! joins them with M6 (`docs/microkernel/M6.md` §3).

use crate::args::Args;
use crate::check::python_with;
use crate::paths;
use crate::task::Task;
use crate::{Error, Result, cargo, fat, initramfs, native, qemu};

/// `test-first-processes`: the manager, its partition, devmgr and `linuxd`
/// (`docs/microkernel/BOOT-AUTHORITY.md` §6). Test images ship in no product.
pub(crate) const FIRST_TASKS: &[Task] = &[
    Task {
        name: "test-btrfsd",
        help_at: 961,
        help: &[
            "test-btrfsd   Boot --personality user --set fs=server: btrfs in its own process, read by a program.",
        ],
        run: |_, args| test_btrfsd(args),
    },
    Task {
        name: "test-btrfsd-restart",
        help_at: 962,
        help: &[
            "test-btrfsd-restart  As test-btrfsd, over a driver that dies, hangs and decommits: requests wait or fail by their bound, and a dead driver's memory goes.",
        ],
        run: |_, args| test_btrfsd_restart(args),
    },
];

/// The description of an `fs=server` image: `btrfsd`, the one row
/// (`docs/microkernel/BTRFSD.md` §4, BD.6).
const SERVER_ROW: &str =
    "[[partition]]\nname = \"btrfsd\"\nimage = \"sbin/btrfsd\"\nrole = \"files\"\nrestarts = 0\n";

/// The file [`SERVER_ROW`] describes, generated and judged against the images.
fn described(root: &std::path::Path, row: &str) -> Result<crate::ports::File> {
    let toml = root.join("partitions.toml");
    std::fs::write(&toml, row).map_err(|e| Error::new(e.to_string()))?;
    let out = root.join("partitions");
    let (toml, root_text, out_text) = (
        toml.display().to_string(),
        root.display().to_string(),
        out.display().to_string(),
    );
    python_with(
        "tools/common/gen/gen-partitions.py",
        &[toml.as_str(), root_text.as_str(), out_text.as_str()],
    )?;
    python_with(
        "tools/common/check/check-partitions.py",
        &[out_text.as_str(), root_text.as_str()],
    )?;
    Ok(crate::ports::File {
        path: "etc/ferrix/partitions".to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(
            std::fs::read(&out).map_err(|e| Error::new(e.to_string()))?,
        ),
    })
}

/// The first page of an ELF `image` that holds something executable and anything else,
/// or none. The builder copies such a page and protects it to the union, and K14 refuses
/// an executable copy in a measured job: found here, on the bytes the image will carry,
/// rather than as a refusal at run time (ledger 5896 BA.4). A malformed image is left to
/// the loaders' own refusals and is a finding of neither.
pub(crate) fn executable_page_shared(image: &[u8]) -> Option<u64> {
    const PAGE: u64 = 4096;
    let word = |at: usize, wide: usize| -> Option<u64> {
        let bytes = image.get(at..at.checked_add(wide)?)?;
        Some(
            bytes
                .iter()
                .rev()
                .fold(0, |value, byte| value << 8 | u64::from(*byte)),
        )
    };
    if image.get(..4) != Some(b"\x7fELF") {
        return None;
    }
    let wide = image.get(4) == Some(&2);
    let (table, size, count) = if wide {
        (word(32, 8)?, word(54, 2)?, word(56, 2)?)
    } else {
        (word(28, 4)?, word(42, 2)?, word(44, 2)?)
    };
    let mut segments = Vec::new();
    for index in 0..count {
        let at = usize::try_from(table.checked_add(index.checked_mul(size)?)?).ok()?;
        if word(at, 4)? != 1 {
            continue;
        }
        let (flags, start, memory) = if wide {
            (word(at + 4, 4)?, word(at + 16, 8)?, word(at + 40, 8)?)
        } else {
            (word(at + 24, 4)?, word(at + 8, 4)?, word(at + 20, 4)?)
        };
        if memory != 0 {
            segments.push((flags & 1 != 0, start, start.checked_add(memory)?));
        }
    }
    for (index, &(executable, start, end)) in segments.iter().enumerate() {
        if !executable {
            continue;
        }
        let (first, last) = (start / PAGE, (end - 1) / PAGE);
        for (other_index, &(_, other_start, other_end)) in segments.iter().enumerate() {
            if index == other_index {
                continue;
            }
            let (other_first, other_last) = (other_start / PAGE, (other_end - 1) / PAGE);
            if first <= other_last && other_first <= last {
                return Some(first.max(other_first) * PAGE);
            }
        }
    }
    None
}

/// Every program of a `personality=user` image goes into a measured job or may: none has
/// an executable page the builder would have to copy.
pub(crate) fn refuse_executable_copies(natives: &[native::Built]) -> Result<()> {
    if crate::flags::this_run()?.value("personality") != Some("user") {
        return Ok(());
    }
    for built in natives {
        if let Some(page) = executable_page_shared(&built.bytes) {
            return Err(Error::new(format!(
                "{}: the page at {page:#x} holds code and something else: a measured job refuses its copy (K14 §3)",
                built.name
            )));
        }
    }
    Ok(())
}

/// The description any `fs=server` image carries, its one row `btrfsd`, whose
/// digest is read from `natives` (`docs/microkernel/BTRFSD.md` §4). `None` in any
/// other build.
pub(crate) fn server_description(natives: &[native::Built]) -> Result<Option<crate::ports::File>> {
    if crate::flags::this_run()?.value("fs") != Some("server") {
        return Ok(None);
    }
    let root = paths::target_dir().join("server-description");
    let image_dir = root.join("sbin");
    std::fs::create_dir_all(&image_dir).map_err(|e| Error::new(e.to_string()))?;
    let server = natives
        .iter()
        .find(|built| built.name == "btrfsd")
        .ok_or_else(|| Error::new("btrfsd was not built"))?;
    std::fs::write(image_dir.join("btrfsd"), &server.bytes)
        .map_err(|e| Error::new(e.to_string()))?;
    described(&root, SERVER_ROW).map(Some)
}

/// `cargo xtask test-btrfsd --personality user --set fs=server --init busybox`:
/// the first tranche's row (`docs/microkernel/BTRFSD.md` §6, Q3), a read-only
/// volume from a process of its own.
fn test_btrfsd(args: &Args) -> Result<()> {
    let flags = crate::flags::this_run()?;
    if flags.value("fs") != Some("server") {
        return Err(Error::new("test-btrfsd is for --set fs=server"));
    }
    let init = args
        .init
        .as_deref()
        .ok_or_else(|| Error::new("test-btrfsd runs busybox: --init"))?;
    for arch in args.arches()? {
        let loader = cargo::build_loader(arch, args.release)?;
        let busybox = crate::program_for(init, arch)?;
        let kernel = cargo::build_kernel_with_init(
            arch,
            args.release,
            &busybox,
            crate::shell::BTRFSD_SCRIPT,
        )?;
        let natives = native::build(arch, args.release)?;
        let carried = crate::shell::btrfsd_carried(&busybox)?;
        let initramfs = initramfs::build(None, &natives, None, &carried)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        qemu::test_btrfsd(arch, &image, &kernel, args)?;
    }
    Ok(())
}

/// `test-btrfsd-restart`: L.object.332 and H.4 (`docs/microkernel/BTRFSD.md` §5),
/// with `blk-victim` under the name `blk`.
fn test_btrfsd_restart(args: &Args) -> Result<()> {
    if crate::flags::this_run()?.value("fs") != Some("server") {
        return Err(Error::new("test-btrfsd-restart is for --set fs=server"));
    }
    let init = args
        .init
        .as_deref()
        .ok_or_else(|| Error::new("test-btrfsd-restart runs busybox: --init"))?;
    for arch in args.arches()? {
        let loader = cargo::build_loader(arch, args.release)?;
        let busybox = crate::program_for(init, arch)?;
        let kernel = cargo::build_kernel_with_init(
            arch,
            args.release,
            &busybox,
            crate::shell::BTRFSD_RESTART_SCRIPT,
        )?;
        let mut natives = native::build(arch, args.release)?;
        let target = paths::target_dir();
        let victim = native::build_one(arch, args.release, &target, &native::BLK_VICTIM)?;
        natives
            .iter_mut()
            .find(|built| built.name == "blk")
            .ok_or_else(|| Error::new("the native programs carry no blk driver to replace"))?
            .bytes = victim.bytes;
        let carried = crate::shell::btrfsd_carried(&busybox)?;
        let initramfs = initramfs::build(None, &natives, None, &carried)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        qemu::test_btrfsd_restart(arch, &image, &kernel, args)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::executable_page_shared;

    /// A 64-bit ELF of these (flags, address, size) load segments.
    fn elf(segments: &[(u32, u64, u64)]) -> Vec<u8> {
        let mut image = vec![0_u8; 64];
        image[..4].copy_from_slice(b"\x7fELF");
        image[4] = 2;
        image[32] = 64;
        image[54] = 56;
        image[56] = u8::try_from(segments.len()).unwrap();
        for &(flags, address, size) in segments {
            let mut header = vec![0_u8; 56];
            header[0] = 1;
            header[4..8].copy_from_slice(&flags.to_le_bytes());
            header[16..24].copy_from_slice(&address.to_le_bytes());
            header[40..48].copy_from_slice(&size.to_le_bytes());
            image.extend(header);
        }
        image
    }

    #[test]
    fn code_alone_on_its_pages_is_no_finding() {
        assert_eq!(
            executable_page_shared(&elf(&[(4, 0, 0x800), (5, 0x1000, 0x1800), (6, 0x3000, 8)])),
            None
        );
    }

    #[test]
    fn code_and_data_on_one_page_is_found() {
        assert_eq!(
            executable_page_shared(&elf(&[(4, 0, 0x1800), (5, 0x1800, 0x100)])),
            Some(0x1000)
        );
        assert_eq!(
            executable_page_shared(&elf(&[(5, 0x1000, 0x10), (6, 0x1010, 8)])),
            Some(0x1000)
        );
    }

    #[test]
    fn something_else_is_no_finding_and_no_panic() {
        assert_eq!(executable_page_shared(b"not an image"), None);
        assert_eq!(executable_page_shared(&elf(&[(5, 0x1000, 0)])), None);
        let mut short = elf(&[(5, 0x1000, 8)]);
        short.truncate(70);
        assert_eq!(executable_page_shared(&short), None);
    }
}
