Ferrix x86-64 desktop
=====================

Ferrix is an experimental operating system with its own Rust kernel that
runs ordinary Linux programs. This archive boots its Wayland desktop in
QEMU. You need QEMU and nothing else: no Rust, no build.

    Linux, macOS:  sh run.sh
    Windows:       powershell -ExecutionPolicy Bypass -File run.ps1

A QEMU window opens on the desktop, with a terminal running zinc, Ferrix's
zsh-compatible shell. The terminal you started it from shows the serial
log; Ctrl-A x there quits QEMU. With a hypervisor (KVM, WHPX, HVF) the
desktop is up a few seconds after the firmware hands over. Without one --
an Apple Silicon Mac, a machine whose hypervisor is off, or --tcg -- QEMU
emulates the processor, and everything is slower, how much slower
depending on your machine.

Keys: SUPER+RETURN opens another terminal, SUPER+Q closes a window,
SUPER+F makes one fullscreen, SUPER+1 and SUPER+2 switch workspaces.
Inside, try btop, curl https://example.com, git, ferrofetch, or svc status.

Installing QEMU
  Debian, Ubuntu   sudo apt install qemu-system-x86 ovmf
  Fedora           sudo dnf install qemu-system-x86 edk2-ovmf
  Arch             sudo pacman -S qemu-full edk2-ovmf
  macOS            brew install qemu
  Windows          winget install SoftwareFreedomConservancy.QEMU

The scripts look for the UEFI firmware (OVMF) your QEMU or distribution
installed. OVMF_CODE and OVMF_VARS (run.sh) or -OvmfCode and -OvmfVars
(run.ps1) name it if they do not find it. `sh run.sh --help` lists the
other options: --vnc for a machine with no display, --memory, --smp,
--size, --tcg.

On Linux, KVM needs /dev/kvm to be yours (the kvm group). On Windows, turn
on the "Windows Hypervisor Platform" feature for WHPX; with released QEMU
the guest then gets one processor, because its WHPX support faults
Ferrix's ring-3 drivers with more.

What is not in this archive
  - Chrome, Steam and the Rust toolchain. Each runs on Ferrix from a volume
    of its own, gigabytes each, which a build from source fetches.
  - A disk that keeps anything: / is in memory, so nothing you do survives
    QEMU exiting.
  - The host's GPU: the screen is drawn in software, which is fine for
    terminals and slow for anything big.
  - The keyboard layout is US.

Ferrix is not ready to be anybody's everyday OS: parts of authentication
and process isolation are unfinished. Do not put anything you care about
in it.

Source, documentation and building it yourself:
https://github.com/ferrix-os/ferrix
Licences
  Ferrix is MIT licensed (LICENSE in this archive). The image also carries
  other people's programs under their own licences, among them busybox and
  git (GPL-2.0), alsa-utils (GPL-2.0+) and alsa-lib (LGPL-2.1+), and many
  permissive C libraries and Rust crates. THIRD-PARTY-LICENSES.md in this
  archive lists every part of the image with its licence and the licence's
  text. The source of every GPL and LGPL program in it, exactly as it was
  built, is the release's ferrix-<tag>-sources.tar.gz asset, beside this
  zip on the same release page. Firmware is not in the archive: the scripts
  use the OVMF your QEMU or distribution installed.
