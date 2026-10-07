# Stage 20 — Self-hosting

Build Ferrix — and its compositor — on Ferrix. At that point the acceptance
test writes itself: the image produced by the Ferrix-hosted compiler boots
and passes every test above. Moved from 17 on 2026-09-13, when the compositor
became the goal after `rustc`; it is not on the compositor's path, and the
compositor is cross-compiled until it is.

**Exit:** the image the Ferrix-hosted compiler produces boots and passes every
test above.

**The first step is met (2026-09-23): Ferrix builds its own x86-64 image.**
`cargo xtask test-selfhost` gives the guest one btrfs volume, mounted at
`/data`: the stage 16 toolchain -- now with Cargo and the standard libraries
for `x86_64-unknown-none` and `x86_64-unknown-uefi` --, every file git tracks
in the checkout, and the workspace's crates.io dependencies from `cargo vendor`
with a Cargo home pointing at them. zinc runs `cargo xtask build --arch
x86_64` there, the command a person runs on a Linux host: Cargo compiles xtask
for the guest, a glibc program like `rustc`, and xtask has Cargo compile the
loader, the kernel and the native programs and writes the FAT image, with no
network. The kernel commits the volume on its way to the power-off. The host
then has `btrfs check --check-data-csum` judge what Ferrix wrote, takes the
image and the kernel ELF out with `btrfs restore`, and boots that image with
`test-boot`'s judgement. On example under KVM, with four processors and 8 GiB,
the build takes about 90 seconds of the guest's time:

```
   93.49 |   image /data/src/build/x86_64/ferrix.img (63 KiB loader, 75510 KiB kernel, 10666 KiB initramfs)
   93.54 |   init     the shell exited with 20
  x86_64: btrfs check found nothing wrong
  x86_64: booting the image Ferrix built
  x86_64: boot ok
  x86_64: Ferrix built its own image, and it booted
```

The toolchain was never the problem: the same tree builds the same image in a
`bwrap` sandbox on the host with nothing else of the host's. Four things in
the kernel were, and the gate found three of them:

* **Every file written on a writable btrfs was at most a page long.** The
  writable mount answered a write with the offset it started at rather than
  the one past it, so the kernel, which copies a `write` in 4 KiB at a time,
  put every piece over the first. Stage 12 had written only through offsets
  it named. rustc read back object files of exactly 4096 bytes.
* **A btrfs file's writes could go with its inode.** The dirty pages and the
  new length lived in the inode object, and only the VFS's bounded cache of
  names kept that alive; a file closed and pushed out came back as the last
  commit had it. Found by reading, on the way to the first; the mount now
  holds every inode with dirty pages until its writeback.
* **`MAP_FIXED` was two steps.** An unmap, which lets the space's lock go for
  its shootdown, and then a map; another thread's `mmap` could take the range
  between them. jemalloc re-maps its memory that way, and rustc died with
  `SIGSEGV` within minutes, once reading memory it had just been given from a
  three-page hole between two of its regions. A per-space layout lock now
  makes each call that changes the map one step, as `mmap_lock` does on Linux.
* **jemalloc did not know memory overcommits.** With no
  `/proc/sys/vm/overcommit_memory` it assumed it must give memory back, and
  did so with a `MAP_FIXED` pair and a shootdown each time: xtask alone took
  minutes to compile and the address space broke into thousands of regions.
  The file now says 1, Linux's `OVERCOMMIT_ALWAYS`, which is what the kernel
  does.

Building the C programs on Ferrix found three more, all fixed (2026-09-23
and 24):

* **What `lld` wrote through a mapping was lost.** It writes its output
  through `MAP_SHARED`, which a writable btrfs never wrote back, so a
  proc-macro uutils needs read back as invalid metadata. See stage 12.
* **GNU grep could not drain a pipe.** Writing to `/dev/null` from a pipe,
  grep empties its input with `splice` and falls back to `read` only on
  `EINVAL`; Ferrix answered `ENOSYS`, so `echo x | grep x >/dev/null` failed
  and curl's `configure` concluded there was no grep. `splice` and
  `copy_file_range` now go through a kernel buffer as `sendfile` does, with
  the boot's pipe check calling both by number.
* **Every new file was dated 1970.** The filesystems' clock read the counter
  since boot after `CLOCK_REALTIME` had learned firmware's time, so a file
  written now was older than any a tar archive unpacked, and automake's
  "newly created file is older than distributed files" stopped curl's
  `configure`. Files are dated by `CLOCK_REALTIME` now, and the pipe check
  requires it.

**Every build, not only the image (2026-09-24).** The rest of the exit is
the test matrix booting only what Ferrix compiled, and three pieces for it
are in place:

* `FERRIX_BUILDS=record:<DIR>` makes xtask write down every build it makes
  -- Cargo's and the C programs' build scripts, each keyed by SHA-256 over
  its command, its environment and the files it reads -- as a plan, while
  the matrix runs as usual (`tools/common/xtask/src/builds.rs`).
  `FERRIX_BUILDS=replay:<DIR>/store` makes no build at all: each is answered
  from the store, and one the store lacks fails with "was not built on
  Ferrix".
* `cargo xtask test-selfhost --plan <DIR>` gives Ferrix the plan, the
  vendored crates of every workspace and the sources the C builds read, and
  zinc runs `cargo xtask builds-execute` there; the store it writes comes
  back to the host.
* `tools/common/test/selfhost-matrix.sh record|replay <DIR>` runs the matrix both
  ways, in a home of its own. A plan belongs to the tree it was recorded on.

The toolchain grew to match: every target's standard library, gcc and g++ 15,
binutils, make, cmake, ninja, meson, bison, pkg-config, Perl, Python and
wayland-scanner 1.24.0 for foot, 151 Debian packages pinned by
`tools/common/fetch/fetch-rustc-sysroot.sh`. foot builds from that tree alone in a
sandbox shaped like the guest. On 2026-09-24 the whole matrix recorded 147
builds; 26 of its 31 rows passed, and the five that failed were kernel
self-check flakes and one slow frame on a host at a load of 20 to 50. Ferrix
then made builds of the plan until two runs in a row stopped on FX-0001, a
processor that did not answer a TLB shootdown within its one second; that is
the next step, below.

**On Arm hardware (2026-10-03): Ferrix builds its own AArch64 image on the
Pixel 7.** `FERRIX_SYSROOT_ARCH=arm64 tools/common/fetch/fetch-rustc-sysroot.sh`
makes the same toolchain for an AArch64 host (the same Debian packages from the
same snapshots, and rustc and Cargo 1.97.1 for `aarch64-unknown-linux-gnu`),
and `tools/vendor/google/pixel7/selfhost.sh` puts it on a volume with the
checkout and its vendored crates, sends it to the phone, and boots the phone's
own `desktop.Image` in crosvm, unchanged, with the volume as `vdd` and
`ferrix.init=/data/selfhost/init`: no image is made for the test. On four
vCPUs (one Cortex-X1, three Cortex-A78) and 5 GiB, `cargo xtask build --arch
aarch64` at two jobs wrote the image 546 seconds after the boot (125609 KiB
kernel, 22345 KiB initramfs). Back on the host, `cargo xtask test-selfhost
--arch aarch64 --volume <img>` found the volume a clean btrfs with
`--check-data-csum`, took the image and kernel out, and booted the image to
`FERRIX-BOOT-OK stages 1-12` with every self-check run, under QEMU. FX-0001 did
not fire. The USB link to the phone drops now and then, so the script moves
the volume in checksummed pieces and runs the phone's long steps detached.

**The matrix, 2026-10-03 and -04: where it stands.**

Done, on `main`:

* d488da992 (and the two commits before it): Ferrix builds its own AArch64
  image on the Pixel 7, and `test-selfhost --arch aarch64 --volume` judges
  the volume a build left (above).
* 7f002affe: `L.init.1-4` and `H.BOOT.15` reserved for the init change
  below.
* The change below, branch `selfhost-matrix`, rebased onto `main` and
  landed with its tip 77783565a (handover
  `docs/handover/2026-10-04-stage20-selfhost.md`):

* **One kernel for every test.** A test's init program, `sh -c` script and
  command list go in the image's initramfs under `.ferrix/init/` instead of
  the kernel (`init::set_inputs`, `L.init.1-3` under `H.BOOT.15`,
  SAFETY-MANUAL AoU-24). The matrix's plan falls from 271 builds to 188, 66
  kernel builds to 6, and Ferrix no longer writes some sixty 125 MB kernel
  ELFs onto the btrfs volume it keeps in memory; a gate on nazuna saves about
  2 s. The certification consultant's OK IF, ledger lines 328 to 343.
* **The matrix itself.** `selfhost-matrix.sh` has 57 rows (the 31 of
  2026-09-24, the 19 gates added since, `build-apps` first) and a `plan`
  mode, `FERRIX_BUILDS=plan:`, that records every build without booting: 36
  minutes for the whole matrix. The script apps (curl, git, foot, sshdt,
  vkgears, btop, the ALSA apps) are recorded as builds, so a replay can no
  longer make them on the host unseen. `test-selfhost --plan` vendors the
  workspaces the plan's builds run in, read off the plan, and vendors apart
  one whose git crate collides (`pulseaudio` 0.3.1).
* **Passed:** the host tests, kernel clippy on three targets, traceability,
  the item boundary, coverage carried; test-boot, test-init, test-shell and
  test-vfs on x86-64 by hand; 9 of the 14 negative controls on 5e2eca4b2.
* cb872a732, **CI's self-hosting job's fix.** "rustc on Ferrix, and Ferrix
  built on Ferrix" had been red on `main` since the components moved to
  repositories of their own (2263225e1, 2026-10-03): the volume carried only
  what git tracks in this checkout, where the component checkouts are
  ignored now, and the guest's `cargo xtask build` tried to clone them with
  no network. The volume now carries each component checkout's tracked
  files, and xtask brings components in only in a tree that is a git
  checkout.

Still to do, in order:

* **CI's next run on `main`** shows whether the self-hosting job is green;
  the local `test-selfhost --accel kvm` passed on the fix.
* **The landing's conditions (met).** The 14 controls of
  `~/.local/share/ferrix/logs/linit-controls.md` again on the landing hash
  (5e2eca4b2 failed `cargo fmt --check`, fixed in 0eda22d00; `test-init`
  needs `sshdt` built in its gate slot) and the one-line report that the
  four boots' transcripts equal `main`'s apart from the `inputs` line and
  the sizes. Both were met before the landing: the consultant's ledger line
  349 and the `os7c-late` transcripts.
The rest of the stage was 50 points (range 40-75), sized on 2026-10-04's
evening, and is 45 since S-2 landed on 2026-10-05; S-0, CI green, is the
first bullet above and is met too. A guess is marked.

* **S-1, FX-0001 under a loaded host, 8** (3 to 13, a guess), **fixed on
  branch `po10-selfhost/fx0001` (2026-10-07)**. A shootdown wait on x86-64
  ended when the host ran the waiter and not the processor waited for.
  Reproduced without loading the host, by making one QEMU vCPU thread
  `SCHED_IDLE` beside busy loops on its core: every one of 257 waits past
  the 1 s bound was answered, after 1.0 to 3.6 s, while `main` stopped the
  same starved `test-selfhost --smp 8` on FX-0001. A wait now has a late
  bound, past which the answer is waited for and counted, and a stuck bound
  of 10 s (MEMORY-AND-TIMING.md §2.2a). Whether the whole plan now finishes
  is S-3's question.
* **S-2, plan mode made complete, 5** (firm), **done 2026-10-05** (ce133294c,
  a3b240c6b, on `main`). Plan mode stops a row at its first boot and missed
  23 of 153 distinct builds, the ones a test makes after a boot (negative
  variants, later boots' programs); those tests now build every variant
  before their first boot (approved by the product owner).
  Landed: `test-audio`, `test-badapple`, `test-compositor`,
  `test-display`, `test-input`, `test-procfs` and `test-threads` build every
  variant before their first boot, each later boot building its own again as
  cargo saying it is current. A plan run of the matrix on that tree
  (`~/.local/share/ferrix/selfhost-matrix/po5-s2-after` against
  `po5-s2-before`) records 162 distinct builds against 139, the 23 missing
  ones all among them and none lost. What a compositor boot carries only when
  the machine has it, the user's own desktop for `fuzzel-user` and
  `everything-desktop`, is still built by those boots alone.
* **S-3, the whole 188-build x86-64 plan made on Ferrix, 8** (a guess).
* **S-4, the script apps' toolchain, 8** (a guess). Mesa (vkgears) wants
  Python's mako and glslang, btop LLVM's C++ runtime.
* **S-5, the Arm toolchains in the guest, 6** (a guess). The Arm busybox and
  ports want cross gcc and the Rust targets `aarch64-unknown-linux-gnu`,
  `armv7-unknown-linux-gnueabihf` and `armv7-unknown-linux-musleabihf`.
* **S-6, a replay of all 57 rows from the Ferrix store, 8** (a guess).
* **S-7, the weekly CI workflow, 5** (a guess): plan mode, Ferrix makes the
  builds in a guest of about 8 GB, then a replay of the rows a runner can
  boot; the rows that need nazuna's volumes (Chrome, Steam, Claude Code)
  replayed there.
* **S-8, the exit record, 2** (firm).

Open for the customer, and not counted: if "three architectures" means the
compiler itself runs on ARMv7-A Ferrix, add 13 points or more.

---

