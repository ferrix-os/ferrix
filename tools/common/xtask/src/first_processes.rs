//! `cargo xtask test-first-processes --personality user`: the kernel's first processes
//! (`docs/microkernel/BOOT-AUTHORITY.md` §6).

use crate::args::Args;
use crate::check::python_with;
use crate::paths;
use crate::task::Task;
use crate::{Error, Result, cargo, fat, initramfs, native, qemu};

/// The task.
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-first-processes",
    help_at: 1222,
    help: &[
        "test-first-processes  Boot --personality user: the manager, devmgr and linuxd, each with its row, and each one's death",
    ],
    run: |_, args| test_first_processes(args),
}];

/// The file `SERVER_ROW` describes, generated and judged against the images.
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

fn test_first_processes(args: &Args) -> Result<()> {
    if crate::flags::this_run()?.value("personality") != Some("user") {
        return Err(Error::new("test-first-processes is for --personality user"));
    }
    for arch in args.arches()? {
        let loader = cargo::build_loader(arch, args.release)?;
        let kernel = cargo::build_kernel(arch, args.release)?;
        let target = paths::target_dir();
        let mut natives = native::build(arch, args.release)?;
        natives.push(native::build_one(
            arch,
            args.release,
            &target,
            &native::PARTITION_HELLO,
        )?);
        let root = target.join("first-processes");
        let image_dir = root.join("sbin");
        std::fs::create_dir_all(&image_dir).map_err(|e| Error::new(e.to_string()))?;
        let hello = natives
            .iter()
            .find(|built| built.name == "partition-hello")
            .ok_or_else(|| Error::new("partition-hello was not built"))?;
        std::fs::write(image_dir.join("partition-hello"), &hello.bytes)
            .map_err(|e| Error::new(e.to_string()))?;
        let plain =
            "[[partition]]\nname = \"hello\"\nimage = \"sbin/partition-hello\"\nrestarts = 1\n";
        let carried = [described(&root, plain)?];
        let initramfs = initramfs::build(None, &natives, None, &carried)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        qemu::test_first_processes(arch, &image, &kernel, args)?;
        // A description's refusals and its filter (BA.2, BA.3, L.object.322, 323), each
        // on x86-64's machine, where edu is at 02:00.0.
        if arch == paths::Arch::X86_64 {
            let device = |bdf: u32| {
                format!(
                    "{plain}[[partition.devices]]\nkind = \"pci\"\nbdf = {bdf}\nvendor = 4660\ndevice = 4584\n"
                )
            };
            let held = [described(&root, &device(0x0200))?];
            let initramfs = initramfs::build(None, &natives, None, &held)?;
            let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
            qemu::test_first_processes_held(arch, &image, &kernel, args)?;
            // A device that asks for a certified driver, past the checker: this kernel has no
            // grant, so its parser's yes is the boot's stop (ledger 6564 Y.1).
            let mut needs = described(&root, &device(0x0200))?;
            if let crate::ports::Content::Bytes(bytes) = &mut needs.content
                && let Some(flag) = bytes.get_mut(16 + 168 + 4)
            {
                *flag = 1;
            }
            let initramfs = initramfs::build(None, &natives, None, &[needs])?;
            let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
            qemu::test_description_stops(arch, &image, &kernel, args, "this kernel has no grant")?;
            let no_such = [described(&root, &device(0x7700))?];
            let initramfs = initramfs::build(None, &natives, None, &no_such)?;
            let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
            qemu::test_description_stops(arch, &image, &kernel, args, "a device matches no node")?;
            // q35's host bridge: on the root bus with every other function there (BA.3).
            let on_a_shared_bus = format!(
                "{plain}[[partition.devices]]\nkind = \"pci\"\nbdf = 0\nvendor = 32902\ndevice = 10688\n"
            );
            let shared = [described(&root, &on_a_shared_bus)?];
            let initramfs = initramfs::build(None, &natives, None, &shared)?;
            let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
            qemu::test_description_stops(
                arch,
                &image,
                &kernel,
                args,
                "shares its bus with another",
            )?;
            let mut short = described(&root, plain)?;
            if let crate::ports::Content::Bytes(bytes) = &mut short.content {
                bytes.truncate(bytes.len() - 1);
            }
            let initramfs = initramfs::build(None, &natives, None, &[short])?;
            let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
            qemu::test_description_stops(
                arch,
                &image,
                &kernel,
                args,
                "the description is malformed",
            )?;
        }
        // Each first process dying at its start (BA.8, L.object.326, 328): an image
        // that ends at once, under that process's name.
        let dies = native::build_one(arch, args.release, &target, &native::CRASH_AT_START)?;
        for victim in ["devmgr", "partmgr"] {
            let mut dying = natives.clone();
            dying
                .iter_mut()
                .find(|built| built.name == victim)
                .ok_or_else(|| Error::new(format!("no {victim} to replace")))?
                .bytes
                .clone_from(&dies.bytes);
            let initramfs = initramfs::build(None, &dying, None, &carried)?;
            let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
            qemu::test_first_process_dies(arch, &image, &kernel, args, victim)?;
        }
    }
    Ok(())
}
