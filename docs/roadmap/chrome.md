# Chrome — a browser on Ferrix  ·  *headless and in a window, 2026-09-24; the DK1 ≈ 45–55 points*

Placed after sysfs without a number of its own. A browser was on no stage
until the customer asked, on 2026-09-18, what one would take, and on
2026-09-23 for the work to start; `docs/CHROME.md` is the account, from the
first assessment to what each run found. On 2026-09-24 the customer chose
Google's prebuilt Chrome over building Chromium, for a first result in days
rather than a source build's 40-plus unknown points. The browser is Chrome
for Testing 154.0.8037.57 on Debian 13's glibc, from a btrfs volume
`tools/common/fetch/fetch-chrome.sh` makes from pinned downloads; ferrousli standing in
for that glibc is the other route, and since 2026-09-24 ferrousli answers
every glibc name Chrome and its libraries import.

**Exit:** Chrome renders a page on Ferrix, headless and in a window on the
compositor. Met on x86-64 (2026-09-24): `cargo xtask test-chrome` runs
`chrome-headless-shell` three ways -- `--version`, `--dump-dom` of a page
whose script only V8 could have run, and `--screenshot` -- and
`cargo xtask test-chrome-window` boots the compositor with the full browser
as a Wayland client and requires its page's colour over a tenth of the
screen. `cargo xtask run-compositor --chrome` puts it on the desktop.

**Done (2026-09-24).** In the order running it found them:

* foot, a Wayland terminal nobody here wrote, with the client libraries a
  browser links, built against ferrousli: `cargo xtask test-foot`
  (§6 of `docs/CHROME.md`). `timerfd` in the kernel, for it.
* `execve` of a program past 64 MiB, mapped from its file on demand, and
  `execve("/proc/self/exe")` from a fork, which is how Chrome starts every
  child (FX-0871).
* `madvise`, which PartitionAlloc `CHECK`s, and `signalfd` (FX-0872,
  FX-0884).
* `CLOCK_THREAD_CPUTIME_ID` and `CLOCK_PROCESS_CPUTIME_ID`, `clock_getres`,
  `creat`, and `/proc/<pid>/task`'s link count, which Chrome's sandbox helper
  counts its threads by.
* The compositor: a request's new object made at its interface's version,
  as libwayland does -- Chrome lost its connection on that -- and Hyprland's
  `env =` lines given to what it starts.

**Done (2026-09-26): Chrome on ferrousli.** `cargo xtask test-chrome
--interpreter ferrousli --library ferrousli` runs the same headless Chrome
on ferrousli's `ld.so` and `libc.so.6` in glibc's place, the volume's other
libraries unchanged, and requires the same three steps. What running it
found, in `docs/CHROME.md` §8: twelve names the 2026-09-24 count missed;
glibc's `libm.so.6` loaded beside ferrousli; a library's reference to its
own versioned symbol; `dlsym(RTLD_NEXT)`; glibc's `locale_t` layout, which
libc++ reads; the program's own `malloc`, which ferrousli's calls now
follow; GNU's `strerror_r` in `libc.so.6`; and TLS in a `dlopen`ed library,
in a static TLS surplus as glibc keeps.

**Done (2026-09-26): Chrome on ferrousli in a window.** `cargo xtask
test-chrome-window --interpreter ferrousli --library ferrousli` runs the
full browser on ferrousli's loader and `libc.so.6`, and requires its page
on the screen after a click and typing, as on glibc. The full browser loads
80 objects to the headless shell's 46; what it found, in `docs/CHROME.md`
§8: the loader's limit of 64 objects, now 256; `posix_fadvise64`; and NSS,
which could not load its soft token because the loader kept a pointer into
the environment Chrome writes its process title over, and named each
library by its `DT_NEEDED` name where glibc gives the path it was found at.

**Done (2026-09-26): the persistent btrfs root.** Chrome stopped on the
desktop's btrfs root because the kernel mounted no tmpfs on `/dev/shm`
inside it, only on the initramfs's root, and Chrome's profile is there; the
root's mounts include it now, `run-compositor --chrome` no longer forces a
tmpfs root, and `cargo xtask test-chrome-window --btrfs-root` requires the
page on the screen from a fresh btrfs root (`docs/CHROME.md` §9).

**Done (2026-09-26): what Chrome looks like and what it says it is.** Its
text was all foot's monospace, because the desktop's fontconfig had no
`conf.d`. It is now Inter and Liberation, carried from the tree's `assets/fonts/`
and drawn with slight hinting. The user agent says Ferrix, through
`--user-agent`. Chrome for Testing's bar
is gone. The compositor now splits an `exec` line as the shell would for
its quoting (`docs/CHROME.md` §9).

**Done (2026-09-26): the zygote, Chrome's speed and its sound.** The
zygote learns its children's pids from `SCM_CREDENTIALS`, which a unix
socket message now carries, so the tests run without `--no-zygote`
(`docs/CHROME.md` §8). Idle Chrome took 443% of a processor and takes 13%,
and a turning box went from 1.5 frames a second to 60, after a futex wait
woken every 5 ms, the HPET as the clock under KVM, `munmap` walking every
page, a shootdown per write after a fork, and the GPU's doorbell were
fixed, and a vDSO added on x86-64; `cargo xtask bench-chrome` measures it
(§9). It plays sound through `/dev/snd` (`cargo xtask test-chrome-audio`,
`docs/AUDIO.md`).

**Done (2026-09-26): a video with its sound.** Every futex call in the
system went through one lock, and Chrome's renderer, playing a video,
spent a fifth of a processor spinning for it. The table is now in
buckets: Chrome's processor time is a third lower and flat, and the video
drops 19% of its frames instead of 29%, at a host load of 25–39. The
sound's remaining gaps follow the host's load. `cargo xtask
bench-chrome-video` measures it (`docs/CHROME.md` §9).

**Done (2026-09-29): Chrome on the Windows desktop.** Clicks took half a
minute to land on `run-compositor --everything` on Windows. The patched
QEMU of 2026-09-27 aborts under WHPX on `-M none`, which is how `auto`
probed for it, so every Windows desktop since ran emulated under TCG; and
under WHPX the clock was the HPET, the invariant TSC being asked for only
under KVM. The probe now starts a `q35`, and WHPX gets `+invtsc`:
`bench-chrome --accel whpx` went from a machine 100% busy at 16 to 36
frames a second to 19–25% busy at 60 (`docs/CHROME.md` §8).

**Done (2026-09-30): pages that loaded slowly on the desktop.** A
download in the guest ran at 0.33 MB/s against the host's 23 MB/s: xtask's
gateway kept more segments in flight than the guest's driver takes, and
answered each loss by sending the whole window again. It now keeps eight
in flight, resends only the lost segment and retransmits after 20 ms: 4.0
to 5.2 MB/s, and a Wikipedia article in 0.21 s instead of about 0.5
(`docs/CHROME.md` §8).

**Still to do:**

* `--no-sandbox`, which is stage 13's.
* ferrousli's `ld.so` run as a command, which it cannot be yet: Chrome
  reaches it by `PT_INTERP`. (`run-compositor --chrome` and `--everything`
  run Chrome, and the compiler in the desktop's terminals, on ferrousli by
  default since 2026-09-27; `--interpreter glibc` for the volume's own.)
* Why the GPU process's fallback path stops at the sandbox's
  `proc_util.cc:115` with `ENOENT` on Ferrix (`docs/CHROME.md` §8, item 8).
* The GPU: Chrome draws in software. (`inotify` landed on 2026-09-27; the
  vDSO on x86-64 on 2026-09-26: `docs/CHROME.md` §3.)
* Where Chrome still says Linux: `navigator.platform`, the client hints
  and `chrome://version`, all constants in Chrome's build. The customer
  chose the flag alone over a patched binary; a Chromium built from
  source is what would change them.
* **The STM32MP157D-DK1**, sized on 2026-09-24 (`docs/CHROME.md` §10), about
  45 to 55 points: an armhf browser, Debian 13's Chromium 150, on the same
  kind of volume (3); a ring-3 SDMMC driver so the volume can live on the
  card rather than in 512 MiB of RAM (13; built and host-tested 2026-10-07,
  `sdmmc`, `docs/CHROME.md` §10.1, its board run owed); page-cache eviction, so the pages
  of a 200 MB program it has touched can be given back (8–13); ARM's
  `cacheflush` for V8's JIT, or `--js-flags=--jitless` (1–3); what running it
  finds (13 or more); and the board's Ethernet, if pages are to come from the
  network (8). Memory is the risk: 512 MiB may be too little whatever is
  built.

---

