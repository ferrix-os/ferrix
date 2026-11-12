# Audio: `/dev/snd` over a ring-3 virtio-snd driver

Version 1, a draft written on 2026-09-26 at the customer's request. The
customer is the product owner (`docs/BACKLOG.md`, since 2026-09-15), moved
audio forward the same day (§6, decision 1), and decides the rest of §6. It
has the shape of `docs/INPUT.md` on purpose: a core in the kernel, a ring-3
driver started by devmgr, the Linux ABI (ALSA) with its subset taken from a
UAPI probe, and QEMU's far end as the test's witness.

## 1. What this is, and what it is not

Ferrix has no audio at all. `docs/ROADMAP.md` stage 22 names three parts for
Steam: a `virtio-snd` driver in ring 3, an audio core with a `/dev/snd`
shaped enough for a client library, and a server speaking the PulseAudio or
PipeWire protocol, 30 points as a first guess. `docs/CHROME.md` §3 notes the
same gap for the browser. This document specifies the first two, which are
the part the kernel owns, and says what the third needs from them:

* **what clients call**, read from alsa-lib, PipeWire, SDL3 and Chromium
  sources rather than assumed (§2);
* **the audio core** in the kernel, which owns each playback stream, its
  buffer and its pointers (§3.1);
* **the control protocol** between the core and the ring-3 driver, in a
  host-tested crate `src/lib/proto/sndctl` (§3.2);
* **the driver** and what QEMU's virtio-snd does that the spec does not say
  (§3.3);
* **the ALSA subset** `/dev/snd/controlC0` and `/dev/snd/pcmC0D0p` answer
  (§3.4);
* **the test**: QEMU's `wav` backend records what the guest played, and
  xtask compares it with what the program wrote (§4).

It is not capture, a mixer or volume control, more than one sample format,
mapped (`MMAP`) access, or a sound server; §7 says when each comes. It is not
hardware other than virtio-snd: the DK1's audio interface and codec and the
Pixel 7's are §7 too.

**Exit of the audio iteration:** on x86-64 and AArch64, with a virtio-snd
device whose far end is QEMU's `wav` backend, a user program finds
`/dev/snd/pcmC0D0p`, configures it through ALSA's `HW_PARAMS`, writes a
second of a known sample sequence with `WRITEI_FRAMES`, drains it, and
closes it. `cargo xtask test-audio` requires every frame the program wrote
to be in QEMU's WAV file, in order, with none missing, repeated or changed.
`cargo xtask run --audio` gives a person the same device with a backend they
can hear.

## 2. Prerequisites: what clients call

The sources read, on 2026-09-26: alsa-lib 1.2.16.1 (`src/pcm/pcm_hw.c`,
`src/control/control_hw.c`, `src/pcm/pcm_mmap.c`), alsa-utils `aplay.c`,
PipeWire 1.6.9 (`spa/plugins/alsa/alsa-pcm.c`, `alsa-udev.c`), SDL3's ALSA
backend, Chromium's `media/audio/alsa`, and Linux's `sound/core/pcm_native.c`.
Every number below came from compiling `/usr/include/sound/asound.h`
(linux-libc-dev 7.0.0-29.29) at both widths; L1 (§5) commits that probe.

### 2.1 What alsa-lib's `hw` plugin does

For `aplay -D hw:0,0`, alsa-lib:

1. Loads `/usr/share/alsa/alsa.conf`. Even `hw:0,0` is defined there, so
   without the file nothing opens.
2. Opens `/dev/snd/controlC0` and asks `CTL_CARD_INFO`, then reopens it and
   asks `CTL_PVERSION` (major and minor must be 2.0) and
   `CTL_PCM_PREFER_SUBDEVICE` (−1). Any failure here fails the open.
3. Opens `/dev/snd/pcmC0D0p` with `O_RDWR | O_NONBLOCK | O_CLOEXEC`, asks
   `PCM_INFO` and `PCM_PVERSION`, and sends `PCM_USER_PVERSION` and
   `PCM_TTSTAMP` (monotonic). All four are fatal on failure.
4. **Tries to map the status and control pages, and falls back.** When
   either `mmap` fails, alsa-lib keeps that half in its own memory and asks
   `PCM_SYNC_PTR` after every operation instead. Linux itself allows the
   mapping only on x86, PowerPC and Alpha (`pcm_native.c`), so on every Arm
   Linux machine alsa-lib already runs the `SYNC_PTR` path. Ferrix refuses
   the mapping on all three architectures and implements `SYNC_PTR`.
5. Clears `O_NONBLOCK`, then configures: twenty or more `PCM_HW_REFINE`s as
   `snd_pcm_hw_params_any` and each `set_*` narrow the space, then
   `PCM_HW_PARAMS`, a default `PCM_SW_PARAMS`, and `PCM_PREPARE`.
6. Plays with `PCM_WRITEI_FRAMES`. On `EPIPE` (an underrun) it asks
   `PCM_STATUS_EXT` and prepares again.
7. At the end, `PCM_DRAIN`, which must *start* a prepared stream that holds
   frames: a short file never reaches the start threshold. Then `PCM_DROP`,
   `PCM_HW_FREE` and `close`.

For the `hw` plugin alsa-lib solves no constraints of its own: every `set_*`
is a `HW_REFINE` round trip, and `set_*_near` tries a minimum, then a
maximum, and restores the saved space on `EINVAL`. So **one fixed
configuration is enough** if the core intersects exactly. It must treat
`openmin`, `openmax` and `integer` as Linux does, fill `cmask` with the
parameters it changed, and answer `EINVAL` for an empty intersection.

### 2.2 What real clients add

* **The `default` device** is `plug` over `hw` unless
  `/usr/share/alsa/cards/<driver>.conf` exists for the card's driver
  string. The core names its driver `Ferrix`, which has no such file, so
  `default` converts any rate, format and channel count to the card's one
  configuration, and emulates mapped access over a read-write card
  (`mmap_emul`, built by default). dmix, which needs mapped access and
  System V IPC, is never chosen.
* **PipeWire** asks for mapped access first and falls back to read-write
  when `set_access` fails (`alsa-pcm.c`, about lines 2316–2333;
  `api.alsa.disable-mmap` forces it). It uses `snd_pcm_status`, `start`,
  `delay`, `rewind` and `forward`. It uses pause only when `INFO_PAUSE` is
  set. It finds cards through libudev, which Ferrix does not have, so it is
  given the card as a static node (`api.alsa.path = "hw:0,0"`).
* **SDL3** and **Chromium** open `default` with read-write interleaved access
  only, and use `writei`, `avail`, `delay` and `start`. SDL also calls
  `reset`, and enumerates with `CTL_PCM_NEXT_DEVICE` and `CTL_PCM_INFO`.
* **A mixer user** (amixer, PipeWire's profile code) lists elements with
  `CTL_ELEM_LIST` and subscribes with `CTL_SUBSCRIBE_EVENTS`. A failed
  subscription fails the whole load. An empty element list is fine.

### 2.3 What a sound server needs from the kernel

This is for iteration 2, not this one, and most of it is already there:
`memfd_create` with seals, `MAP_SHARED` of a memfd, `eventfd`, `timerfd`,
`signalfd`, `epoll`, futexes, `clock_nanosleep`, `SCM_RIGHTS`,
`SCM_CREDENTIALS`, `SO_PEERCRED` and `SO_PASSCRED` all exist. What does not:
`sched_setscheduler` accepts only `SCHED_OTHER` (`src/kernel/extensions/personality/linux/src/syscall/limits.rs`,
the policy check), and `RLIMIT_RTPRIO` is stored but enforces nothing. A
server asks for `SCHED_FIFO` and runs without it when refused, so the gap is
latency under load rather than a failure. Stage 14 (real-time domains) is
where it closes.

## 3. The design

### 3.1 Who owns the stream: the core, with a buffer it allocates

**The audio core** (`src/kernel/src/interfaces/audio`) has one task per card, as the input
core has one per device. It holds what the driver said the device offers,
and for each playback stream it publishes: the state, the buffer, and the
pointers.

* **One configuration.** Version 1 publishes S16_LE, two channels, 48 kHz,
  a period of 960 frames (20 ms, a whole number of microseconds, so that
  every interval in the refine is closed) and sixteen periods: a buffer
  of 15360 frames, 61440 bytes, 320 ms. It was four periods, 80 ms, until
  a loaded guest missed them (§8). It publishes this only if the device's
  `PCM_INFO` offers it. QEMU's does (§3.3).
* **The buffer is the core's.** At `READY` the core allocates one VMO per
  stream, fifteen pages holding the 61440-byte buffer, and hands it to the
  driver, which pins it **read-only** into its device's IOMMU domain. This is
  the display's model (`docs/DISPLAY.md`, the card VMO), not the block or net
  ring's, whose data areas are the driver's. The device reads samples
  straight from the core's pages, and the driver never touches a sample and
  cannot change one.
* **One opener.** A second open of `pcmC0D0p` answers `EBUSY`, as Linux's
  answer for a card with one substream. Mixing streams from several programs
  is the sound server's job, as it is on Linux without dmix.
* **Pointers are ALSA's.** `appl_ptr` counts frames the program wrote,
  `hw_ptr` frames the device finished, both modulo `boundary`. `boundary`
  starts at the buffer size and doubles while `boundary × 2 ≤ LONG_MAX −
  buffer_size`, where `LONG_MAX` is 2³¹−1 on ARMv7-A: the same arithmetic
  alsa-lib does on its side, so `SW_PARAMS` returns it. `avail` for playback
  is `hw_ptr + buffer_size − appl_ptr`.
* **A write copies, and not under a lock.** `WRITEI_FRAMES` copies from the
  program into the VMO between `appl_ptr` and `hw_ptr + buffer_size`, a
  region nothing else reads, and only then advances `appl_ptr` under the
  stream's lock. The input core learned that a copy from a program's memory
  may fault, and a fault may not be taken with preemption disabled
  (`docs/INPUT.md` §5, the fourth finding).
* **Submission.** The core tells the driver about a range of the buffer once
  it holds samples. It submits each period as it fills. When nothing is in
  flight it submits whatever is queued at once, part of a period or not, so
  the device is never idle with frames waiting; that covers `START`. At a
  `DRAIN` everything left goes. The next submission continues from where the
  last ended, and none crosses a period boundary. A completion advances
  `hw_ptr` by exactly the frames it carried. So `hw_ptr` moves in periods
  while playing, and the card says so with `INFO_BATCH`.
* **Start.** A write that brings the queued frames to `start_threshold`
  starts the stream, as does `PCM_START` or `PCM_DRAIN` from `PREPARED`.
  `HW_PARAMS` sets the threshold to 1, as Linux's does, so by default the
  first write starts it.
* **Underrun.** When a completion leaves `avail ≥ stop_threshold` while
  `RUNNING`, the stream enters `XRUN`. The core halts it, and every
  following write answers `EPIPE` until `PREPARE`. The device is never left
  holding stale periods: the core only ever submits frames the program has
  written since the last prepare.
* **Drain.** `INFO_PERFECT_DRAIN` is set, so alsa-lib asks for no silence
  fill. The core submits what is left and waits, or answers `EAGAIN` to a
  non-blocking descriptor, until every submission has completed, then
  returns the stream to `SETUP`. The silence fields of `SW_PARAMS` are
  stored and not acted on, a written deviation (§6).
* **Timestamps.** Each completion is stamped from the monotonic clock, which
  `STATUS_EXT` and `SYNC_PTR` report as `tstamp`. `trigger_tstamp` is the
  moment of the start or stop. `audio_tstamp` is zero, as for a Linux driver
  without a link clock.

The state machine, the refine, the pointer and boundary arithmetic, the
submission rule and the underrun rule are pure logic. They go in
`src/lib/proto/sndctl` beside the protocol (`sndctl::pcm`), host-tested and fuzzed, so
the kernel's part is glue.

### 3.2 The driver protocol: a control channel, `src/lib/proto/sndctl`

Messages are small and come at most once a period per stream, fifty a second
at version 1's configuration. As for input, there is no data ring: one
control `Channel` per card, with `src/lib/proto/inputctl`'s message shape. Each
message is fixed-size and little-endian, starts with a type and a length,
has reserved bytes that must be zero, and is validated in the order its
fields are read. Handles travel alongside.

**Bring-up follows input.** devmgr's table gets `(0x1AF4, [0x1059],
b"snd", Kind::Sound)`; 0x1059 is virtio's modern PCI id for device 25,
`VIRTIO_ID_SOUND`. `start_sound` asks the kernel for the control channel
with a new native call, `SOUND_CONTROL_CREATE` (0x1050, the next free number
after `DEVICE_CLOCK`), sends `START` with blk's layout to `/lib/drivers/snd`,
and waits for `PUBLISHED`. The core is ready for devmgr to start it again
when it dies, as it does a display driver (`docs/DEVMGR.md` §4). It claims
the device through `crate::claim`, so a quiesce waits for the dead
driver's task to let go. It numbers its cards lowest-free, so the card
comes back as `C0`. A program holding the dead card gets `EBADFD` and opens
the new one. A dead driver's pins are kept until the next driver's HELLO
(§3.3's sixth quirk).

| Type | Direction | Body | Handles |
|---|---|---|---|
| `HELLO` | driver → core | version; location; the device's stream count; for each stream (at most 10, QEMU's limit), its direction, channel range, rates as bits over the crate's table and formats as ALSA's `FORMAT_*` bits, translated by the driver from `PCM_INFO` | driver port (`WRITE \| TRANSFER`) |
| `READY` | core → driver | the card's index; for each stream the core publishes (at most 2), its id and version 1's configuration: rate in Hz, ALSA format, channels, period and buffer bytes | core port (`WRITE`); one buffer VMO per published stream (`READ \| MAP`) |
| `REFUSED` | core → driver | reason | — |
| `SUBMIT` | core → driver | stream; sequence number; offset and length in bytes within the buffer VMO | — |
| `ELAPSED` | driver → core | stream; sequence number; whether the device played it (a refused buffer is an underrun); `latency_bytes` | — |
| `HALT` | core → driver | stream | — |
| `HALTED` | driver → core | stream; how many submissions came back unplayed | — |
| `STOP` / `STOPPED` | as blk | | |

`READY` carries the configuration so that the driver does not repeat the
refine: it sends `PCM_SET_PARAMS` and `PCM_PREPARE` for each published
stream, pins each VMO, and only then sets `DRIVER_OK`. `HALT` means stop the
stream and hand back everything posted. The driver answers once the device
has returned every buffer, and the stream is prepared again for the next
`START`. `STOP` and `STOPPED` keep blk's meaning, the whole session, which is
why the stream's pair is named differently.

**What the core never trusts:** a `HELLO` stream count over 10 or a bitmap
bit beyond its enum; an `ELAPSED` for a stream not published, out of
sequence order, or for a submission not outstanding; a `HALTED` count that
does not match what was outstanding. Any of these gets `REFUSED`, then
quiesce, the same as an input driver that lies. The device's word is checked
by the driver first (§3.3).

**A driver that goes away** takes the card with it. Each open stream enters
`DISCONNECTED`. Every ioctl then answers `EBADFD`, as
`snd_pcm_common_ioctl` answers for a disconnected card, and `poll` reports
`POLLERR | POLLHUP`, as a Linux client sees an unplugged USB card. The nodes
leave `/dev/snd`, and the card's index is not given out again this boot.

### 3.3 The driver, and what QEMU's device does

**The driver** (`src/user/system/native/drivers/sound/virtio-snd`, logic in `src/lib/drivers/sound/virtio-snd` over
`src/lib/drivers/block/virtio-blk`'s traits, as `src/lib/drivers/input/virtio-input` does) negotiates
`VIRTIO_F_VERSION_1`, reads the configuration (jacks, streams, channel maps),
asks `PCM_INFO` for every stream, and sends `HELLO`. On `READY` it prepares,
pins and sets `DRIVER_OK` (§3.2). For each `SUBMIT` it posts one chain on the
transmit queue: the 4-byte `virtio_snd_pcm_xfer` from its own memory, the
submitted range as one or two device-readable descriptors (a 3840-byte
period crosses at most one page boundary of the VMO), and the 8-byte
`virtio_snd_pcm_status` for the device to write. Each used chain becomes an
`ELAPSED`. The control and event queues get one interrupt vector and the
transmit queue another. The receive queue is not set up in version 1.

The protocol is `src/lib/drivers/virtio::snd`, beside `::input` and `::console`: the
configuration, the request and response layouts, and a trust section, with
hostile-device tests. The driver checks that a used chain's length is the
status's 8 bytes, that the status is `VIRTIO_SND_S_OK`, and that a control
response has the length its request expects.

**QEMU 9.2.4's device** (`hw/audio/virtio-snd.c`, read 2026-09-26) differs
from what the spec lets a driver expect in six ways, each of which the
driver has to live with:

* **Two streams by default, one each way** (`VIRTIO_SOUND_STREAM_DEFAULT`,
  line 30). Stream `i` is output when `i < streams/2 + streams%2` (the
  `prepare` function), so stream 0 plays and stream 1 records. The core
  publishes only the output. The boot line says the input was left out, as
  the input core's line names what it did not publish.
* **Every stream is already configured and prepared at realize**, with
  S16, two channels, 48 kHz, an 8192-byte buffer and 2048-byte periods
  (lines 1075–1080). `PCM_INFO` before any `SET_PARAMS` therefore answers.
  Its `channels_max` is the channel count *currently set*, not what the
  backend could take (the `stream->info.channels_max = as.nchannels`
  assignment in `prepare`). A driver that believed it would offer one
  channel after a mono configuration.
* **The event queue is not implemented** (`virtio_snd_handle_event`, line
  802: "event queue is unimplemented"). No `PCM_PERIOD_ELAPSED` and no
  `PCM_XRUN` event ever arrives. Completions on the transmit queue are the
  only clock, which is why §3.1 builds everything on them.
* **A buffer comes back only when the backend has consumed all of it**
  (`virtio_snd_pcm_out_cb`, lines 1144–1190), and the backend consumes at
  the audio rate against `QEMU_CLOCK_VIRTUAL` (`audio_rate_peek_bytes`,
  `audio/audio.c`). So completions do pace playback. With nothing posted,
  the backend is given nothing.
* **`latency_bytes` is the buffer's own size** (`return_tx_buffer`, line
  1121), not a latency. The core does not add it to `DELAY`.
* **A reset leaves the streams as they were** (`virtio_snd_reset`, line
  1344, clears only the queued control commands). A driver killed while
  playing leaves its stream started and its buffers held, and QEMU returns
  them as it plays them, or all at once at a `RELEASE`, into whatever
  transmit queue there is by then: the next driver's, as completions of
  chains it never posted, which it fails the device for. Found when devmgr
  first restarted `snd` (2026-09-26): every restarted driver ended as
  `Faulted` within a few milliseconds. So bring-up is two passes
  (`Driver::init`). The first enables only the control queue, sets
  `DRIVER_OK`, and sends `STOP` and `RELEASE` for every stream, whose
  buffers go to a queue with no rings, where QEMU drops them
  (`virtqueue_split_fill`). The second pass starts from a reset. QEMU also
  writes each returned buffer's status through the mapping it took when
  it popped the buffer, so on x86-64 those eight bytes land in the dead
  driver's pages after the IOMMU has let them go. A driver started at once
  was given those frames: on AArch64 a restarted `snd` died of `SIGILL`
  30 ms in, and on x86-64 xtask's DMA-fault check caught the writes the
  unit faulted. Real hardware is stopped at the IOMMU. So the kernel keeps a
  dead driver's pins mapped and its frames held until the core accepts the
  next driver's HELLO, which comes after this drain (`object/pin.rs`, the
  quarantine; finding F-38). The release counts the pages written after
  the death: one to three in each of test-audio's restarts.

Formats offered are S8, U8, S16, U16, S32, U32 and FLOAT, and rates from
5512 to 384000 Hz (lines 40–61). Version 1 uses one of each.

### 3.4 The ALSA subset

`/dev/snd/controlC0` and `/dev/snd/pcmC0D0p` are character devices of major
116, Linux's static ALSA major (`CONFIG_SND_MAJOR`, which is not in the UAPI
headers), with minors from Linux's static layout: control 0 and the first
playback device 16. alsa-lib never checks either number. This machine's
Ubuntu kernel uses dynamic minors (its `controlC0` is `116:18`), which shows
nothing depends on them. Both nodes are `0660` and root's, as `card0` is.

`sys_ioctl` gets one more branch beside the console's, `card<N>`'s,
`renderD<N>`'s and `eventN`'s, by the per-open object as theirs are. The numbers below are x86-64's and AArch64's, which are the same
for every request. ARMv7-A's differ where a structure holds a `long`,
`snd_pcm_uframes_t` or a pointer, and L1 pins both widths.

**The control node** answers `CTL_PVERSION` (2.0.9, `SNDRV_CTL_VERSION`),
`CTL_CARD_INFO` (driver `Ferrix`, name and long name from the device),
`CTL_PCM_PREFER_SUBDEVICE` (stored), `CTL_PCM_NEXT_DEVICE`, `CTL_PCM_INFO`
(`ENOENT` for the capture direction), `CTL_ELEM_LIST` with no elements, and
`CTL_SUBSCRIBE_EVENTS`, after which `poll` never fires because there are no
events. `CTL_ELEM_INFO`, `ELEM_READ`, `ELEM_WRITE` and `TLV_READ` answer
`ENOENT`, which is what alsa-lib's channel-map queries and PipeWire's pitch
probe expect for an element that does not exist. Hardware-dependent, rawmidi
and UMP device enumeration answer "none" (−1).

**The playback node:**

| Request | Number (64-bit) | Answer |
|---|---|---|
| `PVERSION` | `0x80044100` | 2.0.18 (`SNDRV_PCM_VERSION`) |
| `INFO` | `0x81204101` | card 0, device 0, one subdevice, playback |
| `TSTAMP`, `TTSTAMP` | `0x40044102`, `0x40044103` | stored; `TTSTAMP` chooses the clock of `tstamp` |
| `USER_PVERSION` | `0x40044104` | stored |
| `HW_REFINE`, `HW_PARAMS` | `0xc2604110`, `0xc2604111` | §2.1's intersection with the one configuration; `info` = `INTERLEAVED \| BLOCK_TRANSFER \| BATCH \| PERFECT_DRAIN`, `msbits` 16, `rate_num`/`rate_den` 48000/1, `fifo_size` 0 |
| `HW_FREE` | `0x00004112` | back to `OPEN` |
| `SW_PARAMS` | `0xc0884113` | thresholds and `avail_min` stored; `boundary` computed (§3.1) |
| `STATUS_EXT` | `0xc0984124` | state, pointers, `avail`, `delay`, timestamps |
| `DELAY` | `0x80084121` | `appl_ptr − hw_ptr` |
| `SYNC_PTR` | `0xc0884123` | the status half out; `appl_ptr` and `avail_min` in unless the flags say otherwise; the same number at both widths |
| `PREPARE`, `RESET`, `START`, `DROP`, `DRAIN` | `0x00004140`–`0x00004144` | §3.1 |
| `XRUN` | `0x00004148` | forces `XRUN` |
| `REWIND`, `FORWARD` | `0x40084146`, `0x40084149` | 0 frames moved |
| `WRITEI_FRAMES` | `0x40184150` | §3.1; `EAGAIN` when non-blocking and full, `EPIPE` in `XRUN`, `EBADFD` before `PREPARE` |

**Refused:** `mmap` at every offset (`ENXIO`), including the status and
control pages; `PAUSE` and `RESUME` (`ENOSYS`, Linux's answer for a card
without `INFO_PAUSE` and `INFO_RESUME`); `LINK` and `UNLINK`, since there is
one stream, which PipeWire tolerates because it links only follower devices;
`CHANNEL_INFO`, which only mapped access uses. The legacy `STATUS` and the
time32 forms of `SYNC_PTR`, `STATUS` and `STATUS_EXT` are not implemented.
L6 takes each refusal's errno from `pcm_native.c`, not from this list.

**Never `ENOTTY` for `STATUS`, `STATUS_EXT` or `SYNC_PTR`.** musl's `ioctl`
retries exactly these three (with the timer's and rawmidi's status) under
their time32 numbers when the kernel answers `ENOTTY`
(`src/misc/ioctl.c`, `compat_map`). So an `ENOTTY` would come back as a
second request with the time32 layout. The legacy `STATUS` answers `EINVAL`
instead. Every other request the node does not know answers `ENOTTY`, as
Linux's does.

**`poll`** reports `POLLOUT | POLLWRNORM` when the stream is `PREPARED` or
`RUNNING` and `avail ≥ avail_min`, and `POLLOUT | POLLERR` in `XRUN`,
`SETUP`, `OPEN` or `DISCONNECTED`. A draining stream reports nothing until
the drain ends, as Linux does.

### 3.5 Discovery and configuration without udev

alsa-lib finds cards only by opening `controlC0` to `controlC31` and asking
`CARD_INFO`. It reads nothing from `/proc/asound` or `/sys`. So the nodes
existing is discovery. Two files must be on the image for any client:
`/usr/share/alsa/alsa.conf` and the `pcm/` and `ctl/` files it includes,
from the same alsa-lib version as the library that reads them. Iteration 2
adds them with the library.

`/sys/class/sound/card0`, `controlC0` and `pcmC0D0p` are published with
`dev`, `uevent` and `pcm_class` (`generic`), following `docs/SYSFS.md`'s
classes, because PipeWire's udev path reads `pcm_class` and a future udev
replacement will look there. No client in this iteration needs them.

## 4. QEMU and xtask

* **The device.** `-device virtio-sound-pci,audiodev=snd0,disable-legacy=on,iommu_platform=on`
  on x86-64 and AArch64, only under `test-audio` and `run --audio`, so no
  existing gate changes. On ARMv7-A the same without `iommu_platform=on`, for
  the U-Boot reason `docs/INPUT.md` §4 records.
* **The backend for the test** is `-audiodev
  wav,id=snd0,path=<log dir>/audio.wav,out.mixing-engine=off`. With the
  mixing engine off, QEMU neither resamples nor applies volume, and opens the
  backend with the stream's own settings (`audio_template.h`,
  `audio_pcm_hw_add_`). So the file should hold exactly the frames the device
  consumed. That is read from source, not yet seen, and L7's first run
  confirms it. If it does not hold, the check falls back to a tolerance,
  which §6 asks the customer to accept or refuse. QEMU writes the WAV header's
  lengths when the voice closes (`wav_fini_out`), so xtask ends QEMU with
  QMP `quit` and, if the header still says zero, reads the data from byte 44
  to the end.
* **The program.** `tone`, a Linux program built as init the way
  `src/user/system/linux/compositor/evecho` is, over a new `ferrix-linux-abi::sound`. It opens
  `controlC0`, prints `CARD_INFO`'s driver and name, opens `pcmC0D0p`, and
  refines to version 1's configuration exactly as alsa-lib would, including
  one `set_*_near` round of `EINVAL`s. It prints `tone: ready`, then writes
  48000 frames of a **counter**: frame `n` holds `n + 1` in the left channel
  and its complement in the right, as S16_LE, so no frame is silence and a
  lost, repeated or swapped period has a position. It writes in blocks of
  700 frames, which is not a divisor of the period, so partial periods are
  exercised. It then drains, prints `STATUS_EXT`'s state and pointers,
  closes, and prints `tone: done`.
* **`test-audio`.** It boots the program with the device and waits for the
  card line, `ready` and `done`, then ends QEMU. It requires the WAV's data,
  with leading and trailing zero frames trimmed, to be exactly frames 1 to
  48000 of the counter. On a mismatch it reports the first frame where the
  two part, as expected against actual. If the program prints `tone:
  failed`, the test stops at once and reports it.
* **The negative control.** `tone` built with `negative-control` prints
  `tone: negative control` first, and writes period 10 of the sequence
  twice, skipping period 11. The check must see that marker line, then fail
  at exactly frame 10 × 960 + 1, and on nothing else.
* **`run --audio`** adds the device with a backend a person can hear:
  `pipewire` or `pa` on a Linux host, `dsound` on Windows. The QEMU on this
  machine's `PATH`, `/usr/local/bin`'s 9.2.4, lists only `none`, `dbus`,
  `oss`, `spice` and `wav`, although the build tree it came from
  (`~/Documents/qemu/qemu/build`) is configured with `pa` and `pipewire`.
  `/usr/bin`'s 10.2.1 has both, for x86-64 only. §6 asks the customer which
  to use.

## 5. Landings and points

Each is a small landing on main, gated on example. The first four touch no
kernel code. **L1 comes first:** every later landing takes its numbers from
it.

| # | Landing | Kernel? | Points |
|---|---|---|---|
| L1 | `src/lib/proto/linux-abi::sound`: §3.4's requests, the protocol versions, the parameter, access, format and state enums, the `INFO_*` bits, the mmap offsets, and the layouts of `snd_interval`, `snd_mask`, `hw_params`, `sw_params`, `status`, `mmap_status`, `mmap_control`, `sync_ptr`, `xferi`, `ctl_card_info`, `pcm_info` and `ctl_elem_list`, at both widths with time64 on ARMv7-A. From a committed probe (`probe/sound.c`, `sound.sh`, `sound-64.txt`, `sound-32.txt`) compiled against `/usr/include/sound/asound.h`, as `input.sh` does, pinned by `src/tests/sound.rs` | no | 2 |
| L2 | `src/lib/drivers/virtio::snd`: configuration, control requests and responses, the transfer header and status, hostile-device tests, checked against QEMU 9.2.4, fuzzed | no | 2 |
| L3 | `src/lib/proto/sndctl`: §3.2's messages and validation, and `pcm`: the state machine, the refine with open ends and the integer flag, pointers and `boundary` at both widths, submission, start, underrun and drain, `SYNC_PTR`'s flags, `poll`'s answer. Host-tested against a table of alsa-lib's `set_*_near` sequences, fuzzed | no | 5 |
| L4 | `src/lib/drivers/sound/virtio-snd`: driver logic over `src/lib/drivers/block/virtio-blk`'s traits (bring-up, `PCM_INFO` into `HELLO`, prepare, one chain per `SUBMIT`, completions into `ELAPSED`, `HALT`), tested against a simulated device with QEMU's five behaviours (§3.3) | no | 3 |
| L5 | `src/user/system/native/drivers/sound/virtio-snd`, devmgr's table entry and `start_sound`, `SOUND_CONTROL_CREATE`, the core's per-card task and buffer VMOs; exit: the boot line names the card, its published stream and what it left out | yes | 4 |
| L6 | devfs `/dev/snd/controlC0` and `pcmC0D0p`: per-open objects, the ioctl branch and §3.4's subset, `WRITEI_FRAMES`, `poll`, `EBUSY` for a second opener, `DISCONNECTED`; `/sys/class/sound` | yes | 5 |
| L7 | `tone` and `ferrix-linux-abi::sound`'s use in it; `xtask test-audio` with the `wav` backend and its negative control; `run --audio` | no | 3 |
|  | **The audio iteration** |  | **24** |

**Iteration 2, the user space, is not in these 24**:

| # | Landing | Points |
|---|---|---|
| U1 | alsa-lib 1.2.16.1 built on ferrousli (static first, shared when a client wants it), `alsa.conf` and its includes on the image, alsa-utils' `aplay` and `speaker-test`; `test-audio` gains a boot that plays through `default`, which is `plug:hw`. **Done 2026-09-27** (§8) | 3 |
| U2 | A Rust server speaking the PulseAudio native protocol (§6, decision 5), in the four slices below | 18 |
| U3 | SDL, Chromium and a game through it, and whatever they find missing | found by running |

The roadmap prices all three parts at 30. This breakdown puts the first two
at 27 and leaves 3 for the server, which is not credible: PipeWire alone is
a daemon, a session manager and a protocol server. The customer
re-baselines once U2's first attempt has sized it.

**U2, sized 2026-09-27**, before any of its code, at the customer's order
(U1, then U2). It lives in `src/user/system/linux/media/`, beside the `pcm` and
`resample` crates Bad Apple!! plays through, which it uses rather than
repeats. Each slice lands on its own:

| # | Slice | Exit | Points |
|---|---|---|---|
| U2a | **Done 2026-09-27.** `pulse-server`, the protocol state machine, on `pulseaudio` at the pinned revision: `AUTH` at protocol 35 with shared memory and memfd both refused, so samples come inline; the client's name; the server's, sink's and source's information and lists (one sink, the card's 48 kHz S16_LE stereo); `CREATE_PLAYBACK_STREAM` and `DELETE_PLAYBACK_STREAM` with the buffer attributes, and requests accounted as `pa_memblockq` does (`tlength`, `minreq`, `prebuf`); `CORK`, `FLUSH`, `DRAIN`, `GET_PLAYBACK_LATENCY`. One stream, no mixing | Host tests replay command sequences `patrace` recorded between the host's `paplay` or Chrome and its server, and each answer matches in kind and field | 4 |
| U2b | **Done 2026-09-27.** `pulsed`, the daemon: `$XDG_RUNTIME_DIR/pulse/native`, one stream written through `media/pcm` to `/dev/snd` and clocked by the card; `pa-tone`, a Rust client that sends tone's counter over the protocol | `test-audio`'s fifth boot, `pulsed` and `pa-tone`, frame for frame, with a negative control | 4 |
| U2c | **Done 2026-09-27.** Mixing: any number of streams summed with saturation into the card's one format, each resampled from its own rate by `media/resample` and its channels mapped (mono to stereo), per-stream volume and mute, an underrun filled with silence | Host tests of the mix against a model; a boot of two `pa-tone`s at 44.1 and 48 kHz whose sum the file holds within the resampler's bound | 5 |
| U2d | **Done 2026-09-27.** The desktop: `pulsed` as an init unit of the session, Debian's `libpulse0` and what it links on Chrome's volume, Chrome through Pulse rather than ALSA (it prefers Pulse once `libpulse.so.0` loads) | `test-chrome-audio` through `pulsed`, on ferrousli as the desktop runs Chrome; `run-compositor --everything` plays a video's sound through it | 5 |

U3's SDL and games then find what is missing by running.

## 6. Decisions and open questions

**For the customer, as product owner:**

1. **Priority. Decided by the customer, 2026-09-26: audio is current
   work, not stage 22's.** It was written into stage 22 (Steam), which has
   not started, and none of this iteration depends on the rest of that
   stage. The browser (`docs/CHROME.md` §3) and the desktop want it sooner.
   Where it sits in the roadmap's numbering, and how it is ordered against
   the other open streams, is still to say.
2. **A kernel core with `/dev/snd`**, rather than a driver with no kernel
   subsystem that binds a Unix socket itself (devmgr's `Kind::Port`, as
   vport does for the clipboard). The socket design is less kernel code, but
   every client then needs the server's protocol from day one, and ALSA
   clients reach it only through alsa-lib's PipeWire plugin. The core gives
   first sound with no port at all (`tone`), a deterministic gate, and the
   same place a Linux sound server sits on. Proposed: the core.
3. **One configuration in version 1** (§3.1), with `plug` converting for
   every client that asks for something else. More rates and period sizes
   need the refine to carry ranges and Linux's rules between them. That
   work is a later row, not a blocker.
4. **Written deviations:** one opener per stream (`EBUSY`), no status and
   control page mapping on x86-64 either, although Linux maps them there,
   `REWIND` and `FORWARD` moving nothing, `SYNC_PTR` refusing to move
   `appl_ptr` (`EPERM`, which is how mapped access commits frames), and
   `SW_PARAMS`' silence fields stored but not acted on. Each is a behaviour
   alsa-lib handles on some Linux machine today.
5. **The server (U2):** PipeWire ported, or a Rust server. PipeWire is what
   Steam's runtime and every current distribution expect, and it is a large
   C port with a session manager (WirePlumber) and optional D-Bus. A Rust
   server needs to speak only the PulseAudio native protocol, which SDL,
   Chromium and Steam all speak, and none of the rest. `src/user/system/linux/compositor/README.md`'s
   no-C rule is the compositor's, not the clients' (`docs/CHROME.md` §3), so
   either is allowed. Not needed until U2.

   **What exists in Rust (surveyed 2026-09-26).** No complete PulseAudio
   server, but the parts of one:
   * `pulseaudio` (github.com/colinmarc/pulseaudio-rs, MIT, 0.3.1 of
     2025-11-30, about 12k lines, no C, no tokio): the whole native
     protocol's messages, typed in both directions, up to protocol version
     35. It is not a server. It has no memfd or shared-memory transport,
     which a server does not need, because a server that answers `AUTH` with
     both off gets its samples inline on the socket. A fix to
     `CREATE_PLAYBACK_STREAM`'s flags (3c0325f, 2026-07-01) is on its
     branch and not in 0.3.1, so the pin is a git revision. Its `patrace`
     sits between a real client and a real server and prints every command,
     which is how U2's tests get their sequences.
   * moonshine's `pulse_server` (github.com/hgaiser/moonshine, BSD-2, about
     1.5k lines in `moonshine-core/src/session/stream/audio/`): a
     playback-only server on that crate. It has stream states, request
     accounting after `pa_memblockq`, introspection, and mixing and
     resampling into one output. It has been run against Wine and Proton,
     native games and Waydroid. Its `buffer.rs` appears to derive from
     magic-mirror's, which is BUSL-1.1, so that file is not a source.
   * magic-mirror (BUSL-1.1) is moonshine's ancestor. It is read, not
     copied.

   The proposal is a Rust server on `pulseaudio` at the pinned revision,
   laid out after moonshine's with credit. Its mixer and resampler are
   written here or taken from `dasp` or `rubato`, not from either
   project's `buffer.rs`. That puts U2 at 1.5k to 2k lines. PipeWire stays
   the alternative if Steam's runtime turns out to want PipeWire's own
   protocol.
6. **The test's strictness.** Exact frames if §4's reading of QEMU holds.
   If the first run shows QEMU changing samples with the mixing engine off,
   accept a tolerance of ±1 per sample, or treat it as a finding against
   QEMU and keep the exact check?
7. **Owner.** ferrix-90, the session that wrote this, from 2026-09-26 at
   the customer's word, with a row in `docs/BACKLOG.md`'s owners table.
8. **Which QEMU `run --audio` uses** (§4). Either reinstall the local
   9.2.4 with its `pa` and `pipewire` backends, which its build is already
   configured for, or use `/usr/bin`'s 10.2.1, which has them but is x86-64
   only. The test itself needs neither: `wav` is in both.

**For others:**

* **ferrousli (32-bit):** musl's `alltypes.h` defines `__USE_TIME_BITS64`,
  so alsa-lib built against it uses the time64 layouts and mmap offsets
  (`0x82000000` and `0x83000000` for status and control). L1's 32-bit probe
  records that view. The core refuses those mappings as it refuses the
  64-bit ones.
* **The kernel reader of L6:** a `hw_params` is 608 bytes (604 on ARMv7-A)
  and a `status` 152. The input core overflowed its task's stack with a
  1680-byte message (`docs/INPUT.md` §5), so the ioctl branch decodes these
  onto the heap.

## 7. Not in version 1

* **Capture** (`pcmC0D0c`): the receive queue, the reverse copy, and
  `READI_FRAMES`. QEMU's second stream is already an input. About 3 points
  on top of this iteration, since the state machine is shared.
* **Controls and volume.** QEMU 9.2.4 offers no control elements: it never
  offers `VIRTIO_SND_F_CTLS` and leaves the configuration's `controls` at
  zero. So there is nothing to publish. Volume is
  the server's, as with PipeWire's software volume.
* **Mapped access** (`MMAP_INTERLEAVED`): the buffer VMO mapped into the
  program, and the status and control pages. PipeWire prefers it and works
  without it.
* **Pause, several streams, several cards, hotplug.** Each is a backlog row
  once a client asks.
* **The DK1** (STM32MP157D): its SAI audio interface and the codec on its
  I²C bus, a DMA engine driver, and the device-tree binding. A driver that
  speaks `src/lib/proto/sndctl` to the same core would be all that is new above the
  hardware. P3, unsized.
* **The Pixel 7:** out of scope. Its audio path is a DSP behind firmware,
  and the phone is tested with no writes to its devices
  (`src/boot/vendor/google/pixel7/HANDOVER.md`).

## 8. Where it stands

L1 is done (2026-09-26): `src/lib/proto/linux-abi::sound` holds every PCM and
control request the header defines, the constants §3.4 uses and the layouts
of the structures it answers. It is pinned line by line to a committed probe
(`probe/sound.c`, `sound.sh`, `sound-64.txt`, `sound-32.txt`, from
linux-libc-dev 7.0.0-29.29, the 32-bit view built with a 64-bit `time_t`).
The probe agreed with §3.4's numbers and with §6's note on the time64 mmap
offsets. It added one thing this document had not said: on ARMv7-A
`SYNC_PTR`'s control half puts `avail_min` at 76, directly after
`appl_ptr`, where 64-bit has it at 80. The crate gained `wide_layout!` for
structures with a layout per width.

L2 is done (2026-09-26): `src/lib/drivers/virtio::snd` is the device protocol. It
covers the queues, the configuration block bounded to QEMU's ten streams,
the `PCM_INFO`, `SET_PARAMS` and four stream commands, the transfer header
and status, and the events. §3.3's five quirks are written into its module
comment. Its tests build QEMU 9.2.4's own answers by hand from
`virtio_snd.h` and then hostile ones: a short or not-OK response, fewer
entries than asked, a direction that does not exist, an empty channel
range, and a transmit completion that wrote anything but its status. The
`virtio_snd` fuzz target ran 223,434,735 inputs in five minutes on example
without a failure.

L3 is done (2026-09-26): `src/lib/proto/sndctl` is the protocol and the stream.
`message` is §3.2's messages, decoded strictly. `session` judges HELLO and
routes the driver's reports. `pcm` is the stream as Linux keeps it,
function by function from `sound/core/pcm_native.c` and `pcm_lib.c`: the
states, pointers, `boundary` at both widths, the thresholds, when a stream
starts, underruns and drains, `SYNC_PTR`, `STATUS`, `DELAY` and `poll`.
`refine` is `HW_REFINE` and `HW_PARAMS`, with Linux's interval and mask rules
against the one configuration. Reading those sources settled four things
this document had guessed at or had wrong:

* A disconnected stream answers `EBADFD`, not `ENODEV` (§3.2, corrected).
* `HW_PARAMS` defaults `start_threshold` to 1 (§3.1).
* Linux refines only the parameters in `rmask` and lets its rules carry
  the change. With one configuration, refining every parameter gives the
  same answer (`refine`'s module comment).
* A drain that hears nothing for `max(100 ms, buffer × 1100 / rate)` ends
  in `SETUP` with `EIO`.

Its 30 tests drive the stream with a model program and device: a second of
a counter written in 700-frame blocks plays whole and in order and drains;
underrun, drop, reset, close and a driver that lies are each checked; the
refine answers alsa-lib's `any`, `set_*` and `set_*_near` calls. The
`sndctl` fuzz target ran 5,047,081 scripts in five minutes on example,
mixing requests, completions and lies, without breaking a property.
`ferrix-linux-abi` gained `EBADFD` and `ESTRPIPE`.

L4 is done (2026-09-26): `src/lib/drivers/sound/virtio-snd` is the driver's logic over a
transport, its own scratch memory and the core's buffer. §3.3 decided most
of it. Writing it settled three things the design had not:

* **`DRIVER_OK` is set before HELLO**, unlike input's driver. A sound
  device describes its streams only through its control queue, which
  virtio 1.2 §3.1.1 opens only after `DRIVER_OK`.
* **Control requests are made one at a time and waited for** by polling,
  with a hook the process may yield in. QEMU answers them as the doorbell
  rings.
* **A halt is `STOP` if started, then `RELEASE`.** QEMU answers `RELEASE`
  only once it has completed every buffer, with status OK
  (`virtio_snd_handle_pcm_release`, as virtio 1.2 §5.14.6.6.5.1 requires).
  So every buffer in flight comes back as an ordinary ELAPSED, which the
  core takes as void. HALTED then counts nothing unplayed, and the stream
  is prepared again.

Its 13 tests run it against a device doing what QEMU 9.2.4's does, and
compare the samples the device read with the ones submitted. They cover a
submission across two pages that are apart in device memory, completions
out of order held back until they are in order again, and a halt's
requests in order. They also cover what a hostile device and a hostile
core are refused. It is not fuzzed yet: virtio-input's driver fuzz target
builds its own device, and this crate's device lives in its tests.

L5, L6 and L7 are done (2026-09-26), in one landing since none is
testable without the others. They met the audio iteration's exit on x86-64
and AArch64:

* **L5.** `src/user/system/native/drivers/sound/virtio-snd` is the driver process. devmgr starts it for PCI
  0x1059 (`Kind::Sound`), and `sound_control_create` (native call 0x1050)
  gives it its channel. `src/kernel/src/interfaces/audio` is the core: a task per card
  that judges HELLO, allocates the stream's buffer as a VMO (four pages
  then, fifteen now),
  publishes the card and answers READY with it. The boot line is
  `audio    card0 virtio-snd: playback 48000 Hz, 2 channels, S16_LE, 1
  stream left out`.
* **L6.** devfs has `/dev/snd/controlC0` (116:0) and `pcmC0D0p` (116:16)
  while a card is published. `src/kernel/extensions/personality/linux/src/interfaces/audio/pcm.rs` answers §3.4's
  subset over `ferrix-sndctl`'s stream. `WRITEI_FRAMES` copies from the
  program with no lock held and then submits, and a non-blocking descriptor
  gets a short count or `EAGAIN`. Every message the stream asks the driver
  for is written with the stream's lock held, so SUBMITs reach the driver
  in sequence order whether a program's write or a completion made them.
  `/sys/class/sound` is not published: no client of this iteration reads
  it.
* **L7.** `src/user/system/linux/compositor/tone` and `cargo xtask test-audio`. A second of the
  counter, written through the ALSA ioctls in 700-frame blocks, was found
  in QEMU's WAV file frame for frame, on x86-64 and on AArch64. It took
  1.1 s of guest time, the device's own pace. The negative control failed
  at frame 10561, the first frame of the period it moved. §4 said
  `10 × 960 + 1`; the moved period is the eleventh, so it is `11 × 960 + 1`.
  `--audio BACKEND` puts the card on any boot: `pipewire` or `pa` to hear
  it, `wav:PATH` to record it.

Chrome plays through it (`docs/CHROME.md` §3). Its audio service falls back
to ALSA by itself, since there is no libpulse on the volume. It asks
`snd_pcm_set_params` for S16, stereo, 48 kHz, which is exactly the one
configuration, through alsa-lib's `default`, which is `plug` over the card.
The volume gained `libasound2-data`, linked at `/usr/share/alsa`, and Chrome
gained three flags (`tools/common/xtask/src/chrome.rs`, `window_command`). `cargo xtask
test-chrome-audio` boots the desktop with Chrome on a page that plays
440 Hz and requires the tone in the card's WAV file. Its first run under
KVM found 6.6 s of it at 439.9 Hz, amplitude 6553 (the page's gain of 0.2),
with no gap anywhere. `cargo xtask run-compositor --chrome --audio pipewire`
puts the same card behind the host's PipeWire, and the welcome page's
button plays the tone aloud.

**Under load (2026-09-26).** The customer played a video in Chrome under
`run-compositor --everything` on a busy host. The sound glitched, and then
the `snd` process was gone and Chrome said "Audio renderer error". The
likeliest reading is the driver's control-request timeout. It polled by
spinning a fixed count, a few milliseconds, and QEMU answers `PCM_START`,
`STOP` and `PREPARE` only once its host backend has. A PipeWire stream
opened or closed on a loaded host takes longer than that, and a request
that timed out ended the driver. Four changes followed:

* the driver naps 100 µs between looks, on a port of its own, and gives a
  request five seconds;
* the buffer is sixteen periods, 320 ms (§3.1);
* xtask asks QEMU's `pipewire` and `pa` backends for 100 ms of latency, not
  their 46 ms default, and every backend for 48 kHz, so the host does not
  resample;
* the kernel prints `audio    card<N>: underrun <count>` the first three
  times a stream underruns and every hundredth time after, so a glitch has
  a line in the log.

Three things are still open:

* **§3.3's reading of QEMU was of 9.2.4.** The QEMU these runs used is
  10.2.1: `~/.local/bin` on example links it ahead of `/usr/local/bin`'s
  9.2.4, and it is the one that has `pipewire` and `pa`. §6's decision 8 is
  answered by that link for x86-64. AArch64 runs 9.2.4's, and passed.
* **ARMv7-A has run since 2026-09-26**, in every audio landing's gate at
  two processors, the restart boot included. The card is attached without
  `iommu_platform=on` there, as input's devices are.
* **The server is not written.** U2 remains, and §6 decision 5's proposal
  is unchanged; the customer chose U1 first, then U2 (2026-09-27).

U1 is done (2026-09-27): alsa-lib 1.2.16.1 and alsa-utils 1.2.16's `aplay`
and `speaker-test` are ferrousli ports, static, on x86-64: the `alsa-lib`
and `alsa-utils` apps since 2026-10-01 (`cargo xtask build-apps`).
Both linked against ferrousli with nothing undefined on the first try.
alsa-lib is static, so every PCM and control plugin is built in and nothing
is `dlopen`ed. Its configuration is at `/usr/share/ferrousli/alsa`, not
`/usr/share/alsa`: that path is Chrome's, where Debian's alsa-lib finds
Debian's configuration on the volume. `test-audio` gains a fourth boot on
x86-64: ferrousli's busybox runs `aplay -D default` on a WAV file of tone's
counter, which goes through alsa-lib's configuration and `plug` to
`/dev/snd`, and QEMU's file must hold the counter frame for frame. It did,
on its first run. With one frame of the WAV file changed, the check failed
at exactly that frame. Shared libraries wait for a client that wants them.

U2a is done (2026-09-27): `src/user/system/linux/media/pulse-server` is the protocol's
server side as a state machine with no I/O, on `pulseaudio` at 3c0325f.
Its 12 host tests drive it as libpulse does, with the crate's own encoders,
through sequences `patrace` recorded between mpv or ffmpeg and PipeWire's
server on the host. Recording them settled two things §5 had not said. mpv
names its format only in the format list, with the sample specification
left invalid and the values JSON-quoted. And a client polls
`GET_PLAYBACK_LATENCY` far more than anything else: 72 of mpv's commands in
one second of silence. The latency reply's `write_offset` and `read_offset`
are what it keeps time by. The buffer attributes a client leaves to the
server are PulseAudio's rules with a `tlength` of 250 ms rather than 2 s,
since the card has 320 ms of its own.

U2b is done (2026-09-27): `src/user/system/linux/media/pulsed` is the daemon, and
`pa-tone` a client that sends tone's counter over the protocol as
libpulse's simple API would. `pulsed` waits for its sockets at most a
quarter of a period, then writes the card a period at a time for as long
as it has room, from the stream that is playing, through `media/pcm`. It
never makes up what a stream has not sent: an underrunning stream gives
the card less, and QEMU records nothing for a starved card, so nothing is
heard that no client wrote. `test-audio`'s fifth boot runs both under zinc
and holds QEMU's file to the counter, frame for frame. It passed on all
three architectures on its first gate. Writing it found a flaw in the
`aplay` boot from U1: a script that ends lets init exit, and the machine
powers off while QEMU's audio backend still holds the last 100 ms or so,
which never reach the file. Under KVM that cut 6084 frames. The scripts
now stay up, and the harness waits a second after a program's last word.

U2c is done (2026-09-27): `pulse-server`'s `Server::mix` takes every
playing stream as the card takes it and sums them. Each is decoded from
U8, S16, S32 or float, its channels mapped onto the card's (left to left,
right to right, mono, centre and LFE to both at -3 dB), converted from its
own rate by `media/resample`, and scaled by its volume and the sink's,
cubed as `PulseAudio` does. The sum is clipped once, and encoded as the
inverse of how S16 is decoded, so a stream in the card's own format at full
volume reaches the card bit for bit, which the counter boots hold it to.
`SET_SINK_INPUT_VOLUME`, `SET_SINK_INPUT_MUTE`, `SET_SINK_VOLUME` and
`SET_SINK_MUTE` set them. A stream with less than the others is silent for
the rest of a period; with nothing from any, nothing is made. `test-audio`'s
sixth boot plays two `pa-tone` sines at once, 440 Hz at 48 kHz and 1000 Hz
at 44.1 kHz. In the middle half second of QEMU's file, a Goertzel filter
must find each at a quarter of full scale within 5%, and nothing at 700 Hz.
On x86-64 it found 8192 and 8192 of 8192, and 0. With the mix taking only
the first stream, the check failed with 440 Hz at 7.

**A drain's timeout and a stopped guest (2026-09-27).** `test-audio`'s tone
boots failed twice under TCG at host load 25-29 with the drain's `EIO`:
352 ms, Linux's `max(100 ms, buffer × 1100 / rate)`, with no completion.
The wait is restarted by every completion, as Linux's is, so it was no
completion at all. The cause was the host stopping every one of the guest's
processors for longer than that. The guest's clock runs on meanwhile, and
when the processors run again the drain's timer is taken before the driver
is, with the device's completions already waiting for it. Stopping QEMU for
0.6 s with `SIGSTOP` in a drain made it fail every time: "a drain heard no
completion for 352 ms and gave up with 11520 frames queued". So a drain
whose timeout passes now waits one period more for a completion
(`audio/pcm.rs`, `graced`), and the same stop passes. A device that has
really stopped still fails, one period later, which is the one way this
departs from Linux. The kernel says when a drain gives up, and with how
much queued.

U2d is done (2026-09-27): the desktop runs `pulsed` as a unit of
`graphical.target`, before the compositor, listening on
`/run/pulse/native`, and `hyprix.service` tells every client it starts
`PULSE_SERVER=unix:/run/pulse/native`, since there is no session to give
them an `XDG_RUNTIME_DIR`. Chrome's volume carries Debian's `libpulse0` and
what it links (`libsndfile1` and its codecs, `libasyncns0`,
`libx11-xcb1`). Chrome takes Pulse over ALSA once `libpulse.so.0` loads,
and plays 48 kHz stereo float, which the mixer takes. `test-chrome-audio`
requires `pulsed`'s `a stream: Chromium` line as well as the tone in
QEMU's file: 2.61 s of 439.9 Hz on its first pass. Getting there found two
things. Chrome's own verbose log (`--vmodule=pulse_util=1`) said only
"Failed on loading the Pulse library and symbols"; `dlopen` from inside
the guest said why, `libX11-xcb.so.1` missing. `fetch-chrome.sh`'s check
of every library's needs had passed, because it walked only the top of
`usr/lib/x86_64-linux-gnu`, not libpulse's private `pulseaudio/` beside
it. The check now walks that too, and reads each file's `RUNPATH`. A
volume fetched before 2026-09-27 has no libpulse: xtask then leaves
`pulsed` out of the desktop and says so, since Chrome's ALSA would find the
card held, and the gate refuses such a volume. U2, 18 points, is done.

**On ferrousli (2026-09-27).** That gate ran Chrome on the volume's glibc
and ignored `--interpreter`, while `run-compositor --everything` runs it on
ferrousli, the customer's choice of 2026-09-26. There Chrome fell back to
ALSA at start, found the card held by `pulsed`, and played nothing.
`dlopen` inside the guest said `backtrace_symbols: undefined symbol`:
`libpulsecommon` imports it for the stack its log may print, and ferrousli
had `backtrace` and `backtrace_symbols_fd` and not it. It was the only
symbol the whole of libpulse's closure needed that ferrousli lacked.
ferrousli has it now (`src/user/system/linux/ferrousli/src/execinfo.rs`). With it
libpulse loaded, authenticated, asked for the server's information, and
waited for ever: its threaded main loop's lock is a recursive mutex that
inherits priority, and Ferrix's futex has no `FUTEX_LOCK_PI`, so the first
contended lock waited as a deadlock would. glibc refuses such a mutex in
`pthread_mutex_init` with `ENOTSUP` where the kernel has no PI futexes,
and libpulse then asks for a plain one; ferrousli had accepted it. It
refuses it as glibc does now, having asked the kernel once
(`src/user/system/linux/ferrousli/src/mutex.rs`). A client of libpulse's, a probe
doing Chrome's sequence step by step, found where it stopped. And
`test-chrome-audio` runs Chrome on ferrousli unless `--interpreter glibc`
asks for the volume's own: 1.52 s of 439.9 Hz on its first pass there.

