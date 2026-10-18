# Ferrix screenshots

## The demo video, 2026-10-18

`demo.mp4` (70 s, 1280x720, H.264, 20 fps, no sound) and `demo-loop.gif` (15 s of it,
960x540, 12 fps) come from one boot of Ferrix at main 1838944ae: x86_64 under QEMU
with KVM on a Linux host, 4 vCPUs, `run-compositor --everything` (the 3D card with
virgl, Chrome, and `rustc` and `cargo` on the data disk), served over VNC at
1920x1080. Every frame carries the label "FERRIX · x86-64 · QEMU/KVM on Linux".

Nothing was cut and nothing was sped up: the video is the recording from the moment
QEMU's screen answered to the end of the script, in real time. The recorder can cut
long waits (the boot, rustc's run, the page's load) and then says so on screen; on this
take none was long enough to cut. Two things were added to the frames: the label, and
the pointer. The compositor puts the pointer on a cursor plane, which QEMU sends to a
VNC client apart from the screen, so the recorder draws QEMU's cursor image where its
own driver put the pointer.

What it shows, by the video's clock:

- 0-3 s: the firmware (TianoCore) and Ferrix's loader printing its command line, then
  QEMU's "Display output is not active" while the kernel takes the display over.
- About 6 s: hyprix, the compositor, with the terminal the desktop starts; about 12 s:
  Chrome joins it with Ferrix's start page.
- 19-41 s: in that terminal, `uname -sm` (`Ferrix x86_64`), `rustc --version`
  (1.97.1), a three-line `hello.rs` typed into zinc as one quoted `echo`, then
  `rustc hello.rs && ./hello`, which prints "Hello from rustc on Ferrix!" about a
  second after Enter.
- 45-53 s: a click on Chrome's tab strip, Ctrl+L, and
  `https://en.wikipedia.org/wiki/Rust_(programming_language)`, fetched over the
  guest's network and drawn in software.
- 58-70 s: two scrolls down the article. A link under the pointer opens Wikipedia's
  preview card for a moment.

`--everything` also starts Valve's Steam client. The script stops it over ssh as soon
as the guest's sshd answers, before its window opens, because its first-run update
window and then its sign-in window, with a live sign-in QR code, would sit over the
middle of the screen. The terminal is the one the desktop starts at login, not one
opened on camera. Over ssh the script otherwise only asks when to go on (are the
windows up, has rustc finished, has the page's title changed) and removes the last
take's `hello.rs` and `hello` from the home disk, which `run-compositor` keeps between
boots. Everything on screen was typed and clicked through VNC.

On the host, from a worktree at that commit (`<empty>` is an empty directory, so the
`ember` background shows; the recorder's `--vnc` and `--ssh` match xtask's):

```
FERRIX_WALLPAPERS=<empty> CARGO_TARGET_DIR=<target> \
  tools/common/gen/record-demo.py record <out> --tree . --vnc 127.0.0.1:5991 --ssh 2391 -- \
    run-compositor --arch x86_64 --everything --accel kvm \
    --vnc 127.0.0.1:91 --ssh 2391 --layout us
tools/common/gen/record-demo.py edit <out> --loop 38.8 15
```

`record` writes `raw.mkv`, `marks.json` (when each step happened) and `boot.log`;
`edit` writes `demo.mp4`, `demo-loop.gif`, a `demo-loop.webp` and `cuts.json`.
This take was the fourth. The first three found what the script now does: the earlier
take's `hello` on the home disk, Steam's window, a click that landed on the start
page's `/sys` tile, and `cat > hello.rs`, whose echo is below.

Seen while making it, and not in the video: typed into `cat` in that terminal, each
Enter moves down a line without returning to the left edge, so a multi-line file
typed that way steps to the right. The script types the program into zinc's own line
editor instead.

## The screenshots, 2026-10-03

The desktop images come from one boot of Ferrix at main 0345f6adf. The guest is
x86_64 under QEMU with KVM, 4 vCPUs and 4 GiB of RAM, on a virtio-gpu 3D card (virgl,
and Venus for Vulkan). The screen is 1920x1080 and was captured over VNC. Nothing was
edited after capture. The background is `ember`, the picture xtask draws when no
wallpaper is kept or named. They replace the set of 2026-09-27, taken the same way.

The load btop shows is real: Chrome loaded and scrolled a page every ten seconds or so
(rust-lang.org, Wikipedia, the Rust book, GitHub, docs.rs, the Rust blog) for about two
minutes before each btop capture. The git prompt is a small repository made in the
guest, `/src/hello`, with one commit and a clean tree.

Two images do not come from that boot: `website.png` and `steam-yserver.png`, below.

### Captions

- **desktop-hero.png**: The hyprix Wayland compositor on Ferrix (x86_64, KVM) with three
  tiled windows: btop after two minutes of Chrome loading pages, zinc with its oh-my-zsh
  prompt in a git repository after `uname -a`, `git log` and
  `svc status hyprix.service`, and Chrome showing rust-lang.org over the guest's
  network.
- **desktop-chrome.png**: Chrome (Chrome for Testing) on Ferrix's hyprix compositor,
  showing the Wikipedia article on Rust. Chrome runs on ferrousli, Ferrix's own libc, and
  renders in software. x86_64, KVM.
- **terminal-omz.png**: zinc, Ferrix's zsh-compatible shell, running oh-my-zsh's
  agnoster prompt in hyprix's terminal, in a clean git repository. The output shows
  `uname`, the ring-3 driver processes (devmgr, input, blk, net, gpu), the services `svc`
  runs, a tmpfs root beside a btrfs volume, `git status`, and `curl` fetching
  rust-lang.org over HTTPS. x86_64, KVM.
- **btop.png**: btop, built against ferrousli, monitoring a live Ferrix system after two
  minutes of Chrome loading pages: the CPU and eth0 history, Chrome's processes, the
  compositor, the user-space drivers, memory and disks. x86_64, KVM.
- **vkgears.png**: vkgears, built against ferrousli, drawing its gears with Vulkan
  through virtio-gpu's Venus on the host's GPU. x86_64, KVM.
- **website.png**: https://ferrix-os.github.io/, Ferrix's website, as headless Chrome on
  the host draws it at 1440x900 (`google-chrome --headless=new --window-size=1440,900
  --screenshot`), 2026-10-04, after the site took the FX logo.
- **steam-yserver.png**: Steam's store on Ferrix, its X11 client drawn through yserver
  (ferrix-os/yserver) and the rootless Wayland backend. Captured 2026-10-02 during the
  Steam performance work, on a test account, not from the boot above.

### Commands

On the host (`<empty>` is an empty directory, so no kept wallpaper is found and `ember`
shows):

```
git worktree add --detach .claude/worktrees/shots-2026-10-03 main   # 0345f6adf
FERRIX_WALLPAPERS=<empty> CARGO_TARGET_DIR=<target> \
  cargo xtask run-compositor --arch x86_64 --chrome --release --tmpfs-root --accel kvm \
    --vnc 127.0.0.1:83 --ssh 2383 --layout us --venus
```

`--layout us` makes the guest read US keys, so the VNC driver's keysyms type correctly.
`--venus` gives the guest Vulkan through virtio-gpu's Venus, for vkgears.

The git repository, over ssh (`ssh -i ~/.local/share/ferrix/ssh/id_ed25519 -p 2383
root@127.0.0.1`):

```
mkdir -p /src/hello/src && cd /src/hello
# a Cargo.toml and a src/main.rs that prints "Hello from Ferrix"
git init -q -b main && git add . && git commit -q -m "Say hello"
```

Driving the desktop (the steps given to `vncdrive.py 127.0.0.1 5983`, a copy with
`super`, `Delete` and a few more keysyms added; each address is typed as
`key ctrl a ; type <url> key Delete ; key Return ;`). btop is never typed into or clicked: windows are
moved with `hyprctl` over ssh, which acts on the focused window.

- A second terminal: `key super Return ;`. In it,
  `hyprctl dispatch resizeactive 80 110` (btop needs 80x24 cells and a half-screen
  tile gives 77x21), then `clear; btop`
- Load, repeated per page: `click 1500 84 1 key ctrl a ; type <url> key Return ;
  sleep 7 wheel down 10 1470 600 sleep 2 wheel down 10 1470 600 sleep 2`
- In the other zinc: `cd /src/hello`, `uname -a`, `git --no-pager log --oneline`,
  `svc status hyprix.service`. Plain `git log` opens a pager, and a key meant for the
  shell can save a file into the repository and make the prompt dirty.
- The hero: a second round of loads, ending on `https://www.rust-lang.org`
- btop alone, over ssh: `hyprctl dispatch focuswindow pid:<btop's term>`,
  `hyprctl dispatch movetoworkspacesilent 4`, two minutes of loads in Chrome, then
  `hyprctl dispatch workspace 4`
- Chrome alone, over ssh: `hyprctl dispatch workspace 1`,
  `hyprctl dispatch focuswindow pid:<chrome>`, `hyprctl dispatch movetoworkspace 2`, then
  the address bar set to `https://en.wikipedia.org/wiki/Rust_(programming_language)`
- The terminal shot, over ssh: `hyprctl dispatch workspace 1` (zinc is alone there now),
  then `clear`, `uname -srm`, `ps -o pid,comm | head -12`, `svc list | grep running`,
  `df -h / /data`, `git status --short --branch`,
  `curl -sI https://rust-lang.org/ | head -3`
- Every capture: `move 1919 1079 shot <file>.png`
- vkgears: in the same boot, after the shots above. How it was started was not
  written down before the session that took it ended.

A `foot.png` was in this set until it turned out, the same day, to show hyprix's own
terminal: desktops linked `/bin/foot` to `/bin/term` over the foot app until main
2f067e40f, so starting foot started term. It is gone; each app's own screenshot is in
its folder in ferrix-os/apps.
