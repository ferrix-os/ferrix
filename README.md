<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/banner-dark.png">
    <img src="docs/brand/banner-light.png" alt="Ferrix: Linux apps without Linux" width="100%">
  </picture>
</p>

# Ferrix

**Linux apps without Linux.**

Boot Ferrix in QEMU and you can open its Wayland desktop, browse with Chrome,
or run `git`, `curl` and `rustc`. Those are ordinary Linux binaries running on
Ferrix's own Rust kernel. Disk, network, graphics and input drivers run as
separate processes that Ferrix can restart if they crash.

Ferrix is still experimental. Authentication and parts of process isolation
are unfinished, so it is not ready to be your everyday OS.

[Boot Ferrix](#boot-ferrix) · [See what works](#what-works) ·
[Read the technical guide](docs/GUIDE.md) · [Visit the website](https://ferrix-os.github.io/)

![Ferrix's Wayland desktop with Chrome, btop and a terminal](docs/brand/screenshots/desktop-hero.png)

<table>
  <tr>
    <td width="33%"><img src="docs/brand/screenshots/terminal-omz.png" alt="zinc shell showing driver processes, service status and a curl request"></td>
    <td width="33%"><img src="docs/brand/screenshots/desktop-chrome.png" alt="Chrome browsing Wikipedia on the Ferrix desktop"></td>
    <td width="33%"><img src="docs/brand/screenshots/btop.png" alt="btop showing Chrome, the compositor and driver processes"></td>
  </tr>
  <tr>
    <td>The zinc shell, driver processes and a <code>curl</code> request.</td>
    <td>Chrome browsing Wikipedia on Ferrix.</td>
    <td><code>btop</code> showing what's running.</td>
  </tr>
</table>

*Captured on x86-64 under KVM. [How the screenshots were made](docs/brand/screenshots/CAPTIONS.md).*

## What works

- **Linux programs:** Chrome, `rustc`, `git`, `curl`, Valve's `steamcmd` and
  other tested binaries run without patches, 32-bit x86 programs included.
  Valve's Steam client starts and shows its sign-in window. Ferrix implements
  the Linux system calls they need. It also has its own C library, ferrousli,
  for native userland programs.
- **A desktop:** The hyprix Wayland compositor tiles windows and starts with a
  terminal running the zinc shell. Chrome can play video with sound. X11
  programs appear as ordinary windows through yserver, an X server written in
  Rust. The bar, launcher and idle daemon are Rust rewrites of waybar, fuzzel
  and hypridle that read their usual configuration files.
- **Drivers outside the kernel:** Disk, network, graphics, input, sound and
  console drivers run as processes. On x86-64 and AArch64, an IOMMU limits their device
  access. Ferrix can restart them after a crash.
- **Persistent storage and networking:** The root filesystem is btrfs, and an
  installer can put Ferrix on a virtual machine's own disk. Inside Ferrix,
  `git` can clone a repository and `curl` can fetch over HTTPS.
- **Three architectures:** Boot tests cover x86-64, AArch64 and ARMv7-A.
  Ferrix has also booted on an STM32MP157D-DK1 board and a Pixel 7.

[The roadmap](docs/roadmap/where-it-stands.md) tracks what is done and what is
still being built. Ferrix is not ready as an everyday operating system.

## Boot Ferrix

Install Rust and QEMU, then:

```sh
git clone https://github.com/ferrix-os/ferrix.git
cd ferrix
cargo xtask run --arch x86_64
```

That opens a serial console. For the desktop, run `cargo xtask run-compositor`.
The [technical guide](docs/GUIDE.md#getting-started) covers firmware requirements,
other architectures, Windows, graphics options and the extra downloads needed
for Chrome and the Rust toolchain. Quit QEMU with `Ctrl-A`, then `x`.

## Check the claims

The boot test starts Ferrix on all three architectures:

```sh
cargo xtask test-boot --arch all
```

Other tests compile a program with an unmodified `rustc` inside Ferrix and boot
an x86-64 image built inside Ferrix. On 2026-10-03 Ferrix also built its own
AArch64 image inside Ferrix on a Pixel 7, in the phone's own virtual machine,
and that image passed the boot test. The [guide](docs/GUIDE.md#proof-you-can-run)
has the commands and serial output. These tests demonstrate specific working
paths; they are not a claim that every Linux app works.

## How the project is built

Claude sessions write most of the code in separate worktrees. A human owner
sets priorities, and changes have to pass their tests before landing. The
[working rules](docs/CONVENTIONS.md) include mistakes the project has made
and what changed afterward. Human contributions are welcome; see
[CONTRIBUTING.md](CONTRIBUTING.md).

Ferrix is MIT licensed. [Explore the code and component docs](docs/GUIDE.md#layout),
[get the logo and press kit](docs/marketing/README.md), or
[open an issue](https://github.com/ferrix-os/ferrix/issues).
