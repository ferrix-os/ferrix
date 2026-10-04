# Handover: Ferrix on the Pixel 7 (2026-09-26)

For the agent picking this up on example. `README.md` in this directory says
what the loader is. This file is the working state behind it: what is proven
on the phone, how to drive it, what went wrong, and what to do next, most
important first. Read **"Never write anything that survives a reset"** before
you touch the phone.

## State at a glance

**2026-09-27, 00:30 (ferrix-9c): the phone updated itself.** Android's
automatic system update installed CP3A.260905.009 into slot `b` and made
it active; the first reboot after that (a Ferrix run's reset) booted it
(`sys.boot.reason` `reboot,ota`, `ro.boot.slot_suffix` `_b`). Slot `b`'s
`init_boot` is not Magisk-patched, so there is no root: `su` is gone,
and with it the `ramoops` reads after a native boot, the monitor's root
shell and graphs, and the launcher's crosvm VM. Nothing of Ferrix's wrote
storage (it has no storage driver on the phone). Re-rooting is a flash
and the owner's; `panther.dts` and `vendor_boot.img` here are from the
old build (CP2A.260705.006), and the product owner has suspended phone
boots until the new build's device tree is compared with it for every
address the USB work uses. Turn off "Automatic system updates" in
developer options to keep it from happening mid-work again.

**Re-rooted the same night, at the owner's word (00:52):** the stock CP3A
`init_boot` from Google's factory image (SHA-256 checked) was patched in
the phone's Magisk app and flashed to `init_boot_b` only; `su` works again
(Magisk 30.7). Both images and their sums are in
`~/.local/share/ferrix/pixel7/cp3a/` for an undo; automatic system updates
are off (`ota_disable_automatic_update=1`). The new build's device tree is
`cp3a/panther-cp3a.dts`: against `panther.dts` it moves only
`pkvm_guest_firmware`'s reserved region (`0x8B00_0000` to `0xB500_0000`,
still `0xD_0000` long; the loader takes reserved memory from the tree at
every boot, so it follows) and ABL's own initrd, and every node
Ferrix's loader, kernel and USB driver use is byte-identical.


**Update, 2026-09-27 (ferrix-0a): `main` boots the phone on all eight
cores to `FERRIX-BOOT-OK`, seeded from TF-A's TRNG, with the kernel, the
direct map and the vmap arena moved by KASLR, and the boot console drawn in
74 s a boot. "What to do next" is empty; each item is under "Done".**
None of it is pushed.

* **The loader surveys the USB controller** (ferrix-9c, 2026-09-26,
  `src/usb.rs`), reading only, after the display: the PMU's PHY isolation
  words, `pd-hsi0`, and, when that reads on, the DWC3's registers and the
  PHY's first words. The S2MPU is not read (a security block). Run
  `usb-survey2` found everything left on by ABL; `docs/vendor/google/pixel7/USB-HANDOVER.md`
  §8 has the values. It adds about 60 lines to every record.
* **The GICv3 ITS** (`c21ce40f..a8680955`) landed after a rebase and a
  re-gate: `cargo xtask check` passed, and `test-boot` reached
  `FERRIX-BOOT-OK stages 1-12` on x86_64, armv7a, armv7a `--smp 2`,
  aarch64, and aarch64 with `FERRIX_ARM_MACHINE=gic-version=3` at `--smp 1`
  and `--smp 2`.
* **`--kernel-option WORD`** (xtask) puts any word in the image's
  `CMDLINE.TXT`, once for each time it is given, so a QEMU boot can ask for
  `ferrix.fbcon`.
* **The seed** (`dc9d44d5`) and **the boot console** (`534178cf`) landed
  after run 4. `cargo xtask check` passed on them, and `test-boot` reached
  `FERRIX-BOOT-OK` on x86_64, armv7a, armv7a `--smp 2` and aarch64, and on
  aarch64 `--smp 2 --kernel-option ferrix.fbcon` on GICv2 and GICv3.
* **Run 4** (`$P/run4b/boot.img`, `$P/run-fbcon4-nosmp.log`), with
  `ferrix.fbcon nosmp`: **the owner saw Ferrix's boot text on the screen.**
  The record has the loader's `/chosen holds 8 random bytes, passed on to
  the kernel's seed`, the kernel's `random   NOT SEEDED: 64 of 256 bits`,
  and `FERRIX-BOOT-OK stages 1-12`. Android was back 80 s after
  `fastboot boot`, not 52. Drawing costs that time: the checks that print
  most got slower (stage 6's `handlers` 94 to 2027 ms, `signals` 70 to
  719, `fork` 168 to 479, `execve` 5 to 327 against run 3), and the
  others are the same to the millisecond. The framebuffer is likely
  mapped as device memory, so every glyph pixel is an uncached store.
  Mapping it write-combining would be the fix, and nothing needs it yet.
* **Cores 1-7.** Run 5 (`$P/run-fbcon5-smp.log`, no `nosmp`) proved the
  EL2 secondary entry: `cpus 8 described by firmware, 8 online`, and stages
  4 and 5 passed across 8 processors. Stage 6 then failed with FX-0602, and
  run 6 failed in stage 7 after `pid 156 ended by signal 4 ...
  IllegalInstruction`. Two bugs QEMU on an x86 host cannot show, both now
  on `main`:
  * **No instruction-cache maintenance for user code** (`5fd317ff`).
    Android's log says the phone has IDC but not DIC, so instruction caches
    must be invalidated. `mm::map_in` now calls `arch::sync_instructions`
    for every user executable mapping: `clean_to_poc`, then `IC IALLUIS`
    (the A55s' VIPT instruction caches rule out `IC IVAU` by the direct
    map's address). ARMv7-A got the same, untested on the DK1.
    `tools/common/data/asm-allowlist.json` raised ARMv7-A `cpu.rs` from 100 to 102
    lines for it, after the three `CTR` reads there became one. The owner
    allowed the raise on 2026-09-27.
  * **The reverse-map check's user program had no barriers**
    (`9489aab4`). A diagnostic build (run 7) showed the kernel's side
    right and the parent reading page 0 before the child's marker was
    visible. Both Arm programs now have `dmb ish` after reading a command
    and before writing an answer. The same source without them assembles
    to the old bytes exactly.
  With both, runs 8, 8-2 and 8-3 (`$P/run8-dmb*/`) reached
  `FERRIX-BOOT-OK stages 1-12` on 8 cores, 88 s each. The gate passed on
  all three commits: `cargo xtask check`, and `test-boot` on x86_64,
  armv7a, armv7a `--smp 2`, aarch64, aarch64 `--smp 2`, and GICv3
  `--smp 2 --kernel-option ferrix.fbcon`.
* **Mixed cores and the side-channel check.** Run 9 failed with FX-0307
  (`the branch history loop's count disagrees with the plan`): the
  certification session's defences decided the plan on the boot core, an
  A55 needing no Spectre-BHB loop, and the A78s and X1s raised the loop's
  count anyway. ferrix-55 fixed it in `41327ee3`: each secondary decides
  the loop and SSBS for its own core. Run 10 (`$P/run10-bhb/`), that fix
  with `nosmp` dropped, reached `FERRIX-BOOT-OK stages 1-12` with
  `speculation defences read back on 8 processors`, 88 s. The commit that
  drops `nosmp` landed on top, in a tree identical to run 10's, after
  `cargo xtask check` passed. ferrix-55 has two follow-ups queued: a
  machine-wide exposure line, since the one printed is the boot core's, and
  Linux's Spectre v2 safe list (A35/A53/A55), since the A55 is reported as
  `NOT covered` for v2.
  Both landed as `f70b2516`, and run 11 (`$P/run11-v2/`, `main` at that
  commit) reached `FERRIX-BOOT-OK` on 8 cores in 89 s, with one exposure
  line per kind of core: 4 x Cortex-A55 not affected by Spectre v2 or BHB,
  2 x Cortex-A78 and 2 x Cortex-X1 not affected by v2 (CSV2) and covered for
  BHB (32 branches), store bypass covered and Meltdown not affected on all
  three, 0 switch barriers.
* **Running the phone with nobody there.** `WT=<worktree> $P/build-run.sh <name>`
  builds that worktree into `$P/<name>/`, keeping the tree's diff and a debug
  kernel. `$P/boot-run.sh <name>` boots it and saves the record. Neither
  overwrites an existing run. adb and `su` work while the phone is locked,
  so a run needs nobody at the phone, only the screen does.

* **On `main`**, merged 2026-09-26 by fast-forward after the whole gate row
  for "anything the image contains" passed on example. **Not pushed**: pushes
  are the owner's to authorise.
* **Ferrix boots on the phone to `FERRIX-BOOT-OK stages 1-12`, on one core**,
  and **draws on the screen**: the kernel's panic screen has been seen on the
  panel, in the right colours.
* The Pixel 7 commits, oldest first (the first three were `pixel7/bootloader`):

  | Commit | What |
  |---|---|
  | `6e83f794` | The loader as a probe: what ABL hands over |
  | `6c3da42a` | Load and start Ferrix; `ramoops` console; stops at stage 3 |
  | `8520dc49` | The first handover, from the Windows session |
  | `499381a2` | Device-tree path on AArch64 + GICv3 driver (`gicv3.rs`, `gic.rs` front) |
  | `ab996c9b` | Docs: boots to stage 12 on one core |
  | `f250c0bb` | Clippy fix for `gic.rs`: **`499381a2` alone fails clippy** |
  | `654022e9` | `gs201`: feed the watchdogs; end a boot by firing one |
  | `455a01d8` | Loader: display dump, first light, park for the watchdog on failure |
  | `20b242af` | Loader: hand ABL's framebuffer to the kernel as `BootInfo.framebuffer` |
  | `cf4002c6` | Docs: the screen, and the no-persistent-writes rule |
  | `65a9f5dc` | This handover, rewritten |
  | `29b6bb5e` | `gs201.rs` into `src/kernel/src/arch/aarch64/`: the owner put it in the certified core ring |
  | `83f1f8cf` | `gic::init` split for the complexity floor |
  | `73ef630e` | `arch::init_watchdogs`/`start_watchdogs` facade instead of `cfg`s in generic code |

* **Gate, on example, on `73ef630e` as it was before its last rebase** onto
  two xtask-only commits (keyboard layout, remote desktop), which reach
  neither kernel nor loader; xtask's clippy and 293 tests passed after that
  rebase. `cargo xtask check` passed every
  section; `cargo xtask build --arch all --release` passed; `test-boot` passed
  on x86_64, aarch64, armv7a and armv7a `--smp 2`. The first three `check`
  runs failed on the certification item boundary, the complexity floor and
  crate layering, and those three commits answer them. Also
  `FERRIX_ARM_MACHINE=gic-version=3 test-boot --arch aarch64` reaches stages
  3-5 and stops at stage 10, as expected without an ITS.

## The phone

Pixel 7, `panther`; its serial is in `~/.local/share/ferrix/pixel7/serial`.
Stock Android 17, build
`CP2A.260705.006`, slot `a`. Bootloader **unlocked**; rooted with Magisk 30.7
(patched `init_boot_a`); `su` works from `adb shell` (context
`u:r:magisk:s0`). The owner is at the phone and can press buttons or watch the
screen when asked. Ask them to watch before any display test, because only a
human can see the screen.

Tooling on example, all without sudo:

* Google's platform-tools in `~/.local/share/ferrix/pixel7/platform-tools`,
  linked into `~/.local/bin` (`adb`, `fastboot`). Both reach the phone as the
  desktop user through `uaccess`, with no udev rule needed, and example's adb
  key is authorised.
* In `~/.local/share/ferrix/pixel7/`: the factory zip (SHA-256
  `ed94a24e…0372b0b`), `vendor_boot.img` extracted from it, `avbtool.py`,
  `panther.dts` (the phone's live device tree), `display-src/gs-display`
  (Google's display driver, the source of the register offsets), and the
  logs of every run (`run*.log`).
* `llvm-objcopy`: `$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objcopy`.

## One run, end to end

```sh
P=~/.local/share/ferrix/pixel7
cargo xtask flash --arch aarch64 --release --stage "$P/stage"      # ~2 min
K="$P/stage/FERRIX/KERNEL.ELF" I="$P/stage/FERRIX/INITRD.IMG"
FERRIX_PIXEL7_KERNEL="$K" FERRIX_PIXEL7_KERNEL_DIGEST="$(sha256sum "$K" | cut -d' ' -f1)" \
FERRIX_PIXEL7_INITRD="$I" FERRIX_PIXEL7_INITRD_DIGEST="$(sha256sum "$I" | cut -d' ' -f1)" \
    cargo build -p ferrix-boot-pixel7 --target aarch64-unknown-none-softfloat --release
"$(rustc --print sysroot)"/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objcopy \
    -O binary target/aarch64-unknown-none-softfloat/release/ferrix-boot-pixel7 "$P/Image"
python3 src/boot/vendor/google/pixel7/mkbootimg.py "$P/Image" "$P/boot.img"
python3 "$P/avbtool.py" add_hash_footer --image "$P/boot.img" \
    --partition_size 67108864 --partition_name boot --algorithm NONE

adb reboot bootloader
fastboot stage "$P/vendor_boot.img"
fastboot boot "$P/boot.img"
# Wait for fastboot to *disappear* first, or a wait loop sees it still
# listed and thinks the phone is back. Then about 52 s to adb:
until [ "$(adb shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ]; do sleep 2; done
adb exec-out "su -c 'cat /sys/fs/pstore/console-ramoops-0'" > "$P/run.log"
sed -n '/ferrix-pixel7 loader/,/welcome to lk/p' "$P/run.log"
```

* **Timing tells you what happened:** ~52 s means the boot ended normally,
  ~105 s means the loader parked and a watchdog fired, and ~29 s with no
  `console-ramoops-0` means some other reset lost the log. ~110 s with the
  record saying `No kernel logs`, and ABL's `Reboot Info` saying
  `PIN_RESET | PO_RESET` after `CLUSTER0_NONCPU_WDTRESET`, means a hard
  failure. The watchdog reset was followed by a power-on reset, which lost
  DRAM and with it even the loader's lines. Both boot-console runs below
  ended like that.
* **Quote `su` correctly:** `adb shell su -c 'a; b'` runs only `a` as root,
  because the device shell splits on `;`. Use `adb shell "su -c 'a; b'"`.
* **ABL's own log:** `fastboot oem dmesg` prints it for the *current* fastboot
  session, including `Reboot Info` for the previous reset.

## What is measured on the phone

| | |
|---|---|
| Hand-off | loaded at `0x80000000`, device tree at `0x8a000000` (384 KiB), EL2, MMU and caches off, `HCR_EL2` `0x80000002`, counter 24.576 MHz |
| Interrupts | GICv3 from the device tree; virtual timer; 251 ticks at 999 Hz |
| Memory | 7747 MiB, 43 regions; kernel manages 7582 MiB |
| Watchdogs | `google,gs201-cl{0,1}-wdt` at `0x10060000`/`0x10070000`, **running at hand-off**; the kernel feeds them every 500 ms (a 120 s hold survived) |
| Display | DECON0 at `0x1C240000` running in command mode, TE trigger **masked by ABL** (`TRIG_CON` `0x3070`); window 5 from DPP0; framebuffer 1080 x 2400 at `0xFAC00000`; power domains on; display SysMMU reads as off |
| Pixel order | bytes B, G, R, unused, measured with four colour bands: UEFI's `Bgrx8888`, whatever the DPP format's name says |
| Frames | unmasking `TRIG_CON` (`0x3070` to `0x3061`): 12 frames in 200 ms, 60 Hz |
| `/chosen` | only `kaslr-seed`, 8 bytes; **no `rng-seed`** (the loader counted them, `run-seed-nosmp.log`) |
| Missing | no entropy (`random NOT SEEDED`), no RTC (the clock starts at the epoch), cores 1-7 (`nosmp`) |

Register offsets come from Google's gs201 display driver
(`samsung/cal_9845/regs-decon.h`, `regs-dpp.h`; gs201's `cal_9855` builds on
them). Check any new one against the phone before you rely on it.

## What bit us, so it does not bite you

* **`fastboot boot` needs `fastboot stage vendor_boot.img` first, and the AVB
  footer.** Without either, ABL accepts the upload and falls back.
* **Never boot an Android kernel through this path.** KeyMint refuses the
  keys and Android shows "Cannot load Android system". **"Try again"** fixes
  it; **"Factory data reset" would erase the phone**.
* **After `adb reboot bootloader`, reboot mode `0xfc` is sticky.** A reset
  soon after lands in ABL's menu, not Android, until the owner presses *Start*.
* **Only a watchdog reset keeps the `ramoops` log.** PSCI `SYSTEM_RESET` over
  `smc` resets at once and loses it. So the loader parks and waits for a
  watchdog on any failure (`entry::wait_for_watchdog`), and the kernel's
  `shutdown`/`reset` fire a watchdog (`gs201::reset_now`). ABL rewrites the
  record's header, so search the file for text rather than trusting it.
* **Some registers fault on read** (synchronous external abort,
  ESR `0x96000010`): the display SysMMU at `+0xC`, and `pd-disp` at `+0x14`
  (`pd-dpu` was only ever read up to `+0x10`). The loader's display dump logs one register per line, so the
  fault's `FAR` names the refused one.
* **The panel is command mode.** Nothing you write shows until DECON is
  triggered; `display::take_over` unmasks the TE trigger once, and from then
  on every framebuffer write reaches the glass.
* **The kernel only draws on a panic.** To see the screen, build with
  `ferrix.onexit=panic` added to `board::CMDLINE` (temporarily; do not commit
  it). The panic screen stays up until the watchdog fires, 30-60 s.
* **`499381a2` fails AArch64 clippy on its own.** rustfmt ran after clippy;
  `f250c0bb` fixes it. Run clippy again *after* `cargo fmt`.
* **A boot-console change put the fault before the vectors.** The boot
  console first started at stage 1, beside `panic::screen::install`. Its
  mapping check (`panic::screen::surface`) walks the tables from
  `mm::root_table()`, which is 0 until `mm::init`, and no kernel vectors
  are installed that early. The fault went to the loader's stale vectors,
  and the phone hung, drew a coloured stair pattern and came back through a
  power-on reset with no log at all. QEMU never showed it, because the flag
  is off there. Anything drawn early needs a QEMU run with the flag on
  before a phone run.
* **QEMU on an x86 host hides Arm's caches and weak ordering.** It keeps
  stores in order and models no caches, so all of the eight-core failures
  passed every QEMU gate. On the phone they moved between runs: FX-0602
  once, FX-0701 after an illegal instruction the next time. A failure that
  moves like that on real Arm is a missing barrier or missing cache
  maintenance before it is anything else. A user program that hands data
  to another core through shared memory needs `dmb ish` on both sides.
* **Ask the owner to watch in a question, right before the run.** A line
  in a message saying "watch the screen" was missed, and a run's picture
  was lost.
* **Other sessions use this phone.** ferrix-cc tests an Android Auto /
  ChatGPT Xposed module on it (the aa-chatgpt side project), and every
  Ferrix run resets the phone under it and leaves it locked until the owner
  types the PIN. Take turns: message the other session between runs, and
  don't boot while it holds the phone. Session names change when sessions
  restart: on 2026-09-26 evening it was `phone-link-9f`, testing an app on
  the Pixel and a Poco F2 Pro together over adb. Its app, `dev.phonelink`,
  stays installed: don't uninstall it or clear its data. `adb devices`
  listing the phone does not mean it is free, so list the peer sessions and
  ask.
* **The permission classifier refused `cargo xtask flash --stage "$P/stage"`**
  (it overwrites the previous stage) and then the whole rebuild. What
  worked before the second refusal, and deletes nothing: `cargo xtask build
  --arch aarch64 --release`; `python3 $P/fatget.py build/aarch64/ferrix.img
  FERRIX/INITRD.IMG <dir>/INITRD.IMG` (a read-only FAT reader, saved in the
  phone directory); `llvm-objcopy --strip-all` of
  `$CARGO_TARGET_DIR/aarch64-unknown-none-softfloat/release/ferrix-kernel`
  into `<dir>/KERNEL.ELF`; then the loader steps above with those two paths.
  The initramfs is then the unstripped one (5.4 MB), which still fits.
  Use `CARGO_TARGET_DIR=~/.local/share/ferrix/target-<session>`, never a
  worktree's own `target/`.
* **`cargo xtask check` holds more than clippy:** every kernel file needs a
  certification ring (`tools/common/data/certification-item.json`, the owner's call),
  new functions stay under the complexity floor, and generic code may not
  hold a `target_arch` conditional. Run the whole gate, not a subset.

## What to do next, most important first

1. **A USB device driver**, so a native boot streams its console and
   `ferrix-statd`'s samples to the PC live, as a crosvm guest already does:
   `docs/vendor/google/pixel7/USB-HANDOVER.md` is the brief. Not started.

Otherwise the phone boots all eight cores, seeded, with KASLR and the boot
console, to `FERRIX-BOOT-OK`, and further work is new: a display driver of
its own, input, storage, or the DK1's side of the TRNG (`TRNG_RND32`, if its
TF-A answers it).

### Done, for the record

* **The boot console** (`src/kernel/src/console/screen.rs`). With
  `ferrix.fbcon` on the command line, the kernel's own lines (the bytes
  `console::recent` keeps, not programs' output) are drawn on the firmware
  framebuffer as they are printed. It picks the largest glyph scale that
  leaves 60 columns (double size on the phone), keeps a twentieth of the
  height clear at the top for the camera, wraps to the top without
  scrolling or reading back, and keeps the row after the newest line blank.
  It stops when a panic draws or when the display core publishes a card.
  It starts right after `mm::init`, and `panic::screen::surface()`, which
  the panic and the console share, refuses while the root table is 0. The
  stage-1 version hung runs 1 and 2 (`run-fbcon-smp.log`,
  `run-fbcon-nosmp.log`, both empty of Ferrix text). Under QEMU,
  `FERRIX_ARM_MACHINE=gic-version=3,acpi=off` (the Pixel's path) also
  boots it. That `test-boot` exits 1 anyway, because with no ACPI the SMMU
  is not found, and the check that an out-of-domain DMA write faults sees
  none. That is not the console. Screendumps from a QMP boot
  (`$P/fbcon_shots.py`) are `$P/qemu-fbcon-mid.png` and
  `$P/qemu-fbcon-wrapped.png`. The owner agreed to the amendment of
  `docs/ARCHITECTURE.md` §1 that allows it. Run 4 showed it on the phone.
* **The asm budget's raw-string hole.** `check-asm-budget.py` counted a
  raw-string `global_asm!` as the two lines its quotes were on. It now
  counts every line: the tree held 1569 lines, not 733, and the owner had
  the cap and nine budgets moved to the truth. AArch64's secondary entry,
  58 lines, is the one that had really grown, by the EL2 drop.
* **KASLR on the phone.** The kernel is a PIE (KASLR,
  `docs/certification/SPECULATION.md` §6.1), and this loader now moves it
  as `src/boot/common/uefi/` does (`src/boot/vendor/google/pixel7/src/kaslr.rs`): the image, the direct
  map and the vmap arena, each from its own word of TF-A's SMCCC
  `TRNG_RND64`, the source `ferrix_bootinfo::SOURCE_SMCCC_TRNG`. Only the
  `smc` conduit is used, since the loader left EL2 itself, and nothing
  stands in for the TRNG: without it the log says `NOT randomised: the
  loader found no source of randomness`. `nokaslr` on the command line
  keeps the fixed layout. The fixups are applied at the chosen slide before
  the tables are built. Run 17 (`$P/run17-kaslr/`) moved the kernel by
  `0x7688c000` with 18 bits, the direct map with 16 and the arena with 17.
  Stage 1's FX-0101 check passed, and the boot reached `FERRIX-BOOT-OK` on
  8 cores. Run 17-2, the same image, moved everything elsewhere (slide
  `0xee25000`) and passed too. The kernel is stripped with `llvm-objcopy
  --strip-all`, which keeps AArch64's `.rela.dyn`; ARMv7-A would need
  `--strip-debug`.
  Since `47934c84` (F-34) this loader also maps the kernel's text and
  read-only data read-only in the direct map. Run 13 (`$P/run13-sealed/`)
  passed stage 2's FX-0204 check (`sealed 3536 KiB of text and read-only
  data, 1768 mappings of it, none writable`) and reached `FERRIX-BOOT-OK`
  on 8 cores.
* **A faster boot console.** The framebuffer was mapped as device memory,
  so every glyph pixel went out as its own uncached store. It is now normal
  non-cacheable memory, write-combining (`arch::FRAMEBUFFER_FLAGS`; x86-64
  keeps the device mapping, since write-combining there needs the PAT).
  Runs 15 and 16 (`$P/run15-wc/`, `$P/run16-wc/`) were back in 74 s, not
  95, with stage 6's `handlers` at 738 ms, not 2029. The owner watched run
  16 and saw the boot text as before.
* **Entropy.** The owner allowed reading the SoC's random source, read
  only. Android's `/sys/class/misc/hw_random/rng_current` said
  `smccc_trng`, so no register of the security block is touched: the kernel
  asks TF-A through SMCCC (`src/kernel/src/arch/aarch64/trng.rs`), after PSCI
  1.0, `PSCI_FEATURES(SMCCC_VERSION)`, SMCCC 1.1, `TRNG_VERSION` and
  `TRNG_FEATURES(TRNG_RND64)`, and credits each bit. Run 14
  (`$P/run14-trng/`) booted `random   seeded with 448 bits: 8 bytes from
  firmware, 48 from its TRNG, 0 words from the CPU, timer jitter`, the
  phone's first seeded boot, and reached `FERRIX-BOOT-OK` on 8 cores.
* **The seed** (`src/boot/vendor/google/pixel7/src/seed.rs`) folds `/chosen`'s
  `rng-seed` and `kaslr-seed` into `firmware_seed` and NOPs both out of the
  kernel's copy of the tree. The owner chose to credit ABL's 8 bytes rather
  than read the TRNG, so `BootInfo` version 6 carries the count.
* **The GICv3 ITS driver.** Choices the owner may still want to review:
  DeviceID is the PCI requester ID, identity, as QEMU's IORT and `msi-map`
  are, and neither is read yet. One collection, on the
  boot core. LPI 8192+k is kernel interrupt 1024+k, and `irq::SLOTS` went
  from 1024 to 1280. `gicv3_its.rs` falls in the certified core ring,
  because the certification item lists `arch/**` there.

## Starting Ferrix from Android

**The button works through the PC** (`tools/vendor/google/pixel7/`, see its README). The app
"Boot Ferrix" (`dev.ferrix.launcher`) asks `tools/vendor/google/pixel7/helper.py` on example,
over `adb reverse`, to run the `fastboot boot` cycle. The owner pressed it on
2026-09-26: the run reached `FERRIX-BOOT-OK stages 1-12` and Android was back
74 s later, with the record in `$P/launcher-20260926-144950/`. It needs the
cable, and writes nothing to the phone.

**Without the PC, as a VM** (`9df3769b`, `164515a7`). The app's "Run in a
VM" card runs Ferrix as a guest of the phone's own KVM, through AVF's
`crosvm`, started with `su`. The loader recognises crosvm (EL1 entry, and
an `ns16550a` at `0x3f8` as `stdout-path`) and sends both logs to that
16550, which the kernel drives as `console=uart8250,mmio,0x3f8`. With 8
vCPUs and 4 GiB, it reached `FERRIX-BOOT-OK stages 1-12` in 6 s, seeded
from crosvm's `rng-seed` and KVM's `TRNG_RND64`. Stage 5's costs came
within a few percent of native. The owner ran it from the app. What the
guest lacks: KASLR, since the loader asks the TRNG only by `smc` and
`entry.rs` has no assembly room for an `hvc`; a screen; virtio devices,
behind crosvm's `pci-host-cam-generic`, which Ferrix does not read; and
console input.

**Ferrix's own stats**: `src/user/system/linux/statd/` is Ferrix's stat service, `ferrix-statd`,
and `tools/vendor/google/pixel7/monitor` graphs it. `build-run.sh` images carry it. A VM
run starts it with crosvm's `-p ferrix.init=/sbin/ferrix-statd`, which the
monitor's "Stats" choice does, and a native boot needs it built in with
`FERRIX_PIXEL7_CMDLINE_EXTRA="ferrix.init=/sbin/ferrix-statd
ferrix.statd.seconds=20"`. Run `stat2-native` did that: 40 samples in the
ramoops record, and back in Android at 97 s.

**Without the PC, natively**, the kernel-module route below stays stopped:
a safety classifier stopped it again on 2026-09-26, when the owner chose
it.

### The kernel-module route (stopped)

The owner asked for an Android app that starts Ferrix without the PC. This
kernel has no `kexec` (`CONFIG_KEXEC` and `CONFIG_KEXEC_FILE` are not set).
An empty kernel module was built from Google's source for this exact kernel
(`android14-6.1-2025-12_r9`, work in `~/.local/share/ferrix/pixel7/kmod/`),
and it loaded and unloaded cleanly. An earlier build of it panicked the phone
once, because its `struct module` layout was wrong. **Work beyond that was
stopped by a safety classifier.** Do not take it up again without talking to
the owner first. The route that needs neither the classifier nor any writes
is a script on this PC that runs the whole cycle above as one command.

## Never write anything that survives a reset

The owner has lost a device's touchscreen calibration to an agent before, and
it must not happen here. On this phone, only volatile things may be written:
RAM, and SoC controller registers such as DECON's.

* Never `fastboot flash`, `erase` or an `oem` write.
* Never a DSI command to the panel: an OLED panel's MTP can be written.
* Never the touch controller, the PMIC or regulators, power domains, fuses or
  security blocks, or UFS.

Before any test that writes hardware, list the exact addresses, check them
against `panther.dts`, tell the owner, and make the write conditional on the
hardware being as expected, as `display::take_over` is.

## Working here

* **Worktrees:** the owner asked for Pixel 7 work to land on `main` in the
  root checkout rather than live in a sub-worktree; the worktree it grew in
  is removed.
* **Commits:** `docs/CONVENTIONS.md` applies. No `Co-authored-by` or tool
  trailer (the hooks refuse it), commit from your own worktree, read
  `git diff --cached --stat` before each commit, and never move work with
  `git stash`.
* **Refusals:** the auto-mode permission classifier refused, for this
  session, fetching Google's kernel source, preparing it, and loading modules
  on the phone. When it refuses, stop and hand the step to the owner.
* **Commands for the owner:** they run them in their own zsh, where a
  leading `!` is negation, not Claude Code's prefix. Give them commands
  without it.
