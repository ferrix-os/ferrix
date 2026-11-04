//! Play a second of a counter through `/dev/snd`, as alsa-lib's `hw` plugin
//! would, and say what the card and the stream answered.
//!
//! `xtask test-audio` runs this as init with a virtio-snd device whose far
//! end is QEMU's `wav` backend (`docs/AUDIO.md` §4). It asks the control
//! node what the card is, opens the playback node, refines the whole space
//! and then a rate the card does not have (which must be `EINVAL`, as
//! `set_rate_near` meets it), sets the parameters, and writes 48000 frames:
//! frame `n` holds `n` in the left channel and its complement in the right,
//! `S16_LE`, in blocks of 700 frames, which do not divide a period. Then it
//! drains, prints the stream's state, and stays: init does not exit.
//!
//! **The negative control** (`negative-control`) writes period 10 twice and
//! never period 11, so the file QEMU writes holds every frame but a period's
//! worth in the wrong place, and the check must fail exactly there.
//!
//! **The restart** (`restart`) first kills the card's driver twice under a
//! running stream (`docs/DEVMGR.md` §4): each round plays silence, kills the
//! `snd` process, requires the stream to answer `EBADFD` as a disconnected
//! card's does, and waits for `card0` to be published again. The second
//! is then played on the third driver's card, and found whole as ever:
//! silence is what the check skips.

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

/// Only Linux, and Ferrix through its Linux ABI, have `/dev/snd`.
#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;
    use std::io::Write as _;

    use ferrix_linux_abi::layout::{Field, Layout};
    use ferrix_linux_abi::socket::Width;
    use ferrix_linux_abi::sound::{
        ACCESS_RW_INTERLEAVED, Ctl, CtlCardInfo, FORMAT_S16_LE, HW_PARAM_FIRST_INTERVAL,
        HW_PARAM_RATE, HwParams, Interval, Mask, PCM_VERSION, Pcm, SUBFORMAT_STD, Status, SwParams,
        TSTAMP_TYPE_MONOTONIC, Xferi, protocol_version,
    };

    /// Frames in the second played.
    const FRAMES: u32 = 48_000;
    /// Frames in each write.
    const BLOCK: u32 = 700;
    /// Frames in a period, which the negative control moves one of.
    const PERIOD: u32 = 960;
    /// The playback node.
    const PCM: &str = "/dev/snd/pcmC0D0p";

    const fn width() -> Width {
        if usize::BITS == 32 {
            Width::Bits32
        } else {
            Width::Bits64
        }
    }

    fn say(text: &str) {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "tone: {text}");
        let _ = out.flush();
    }

    fn fail(text: &str) -> ! {
        say(&format!("failed: {text}"));
        rest()
    }

    /// Init does not exit.
    fn rest() -> ! {
        loop {
            // SAFETY: `pause` takes nothing and only waits.
            let _ = unsafe { libc::pause() };
        }
    }

    fn errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    fn open(path: &str, flags: libc::c_int) -> libc::c_int {
        let Ok(name) = CString::new(path) else {
            fail("a path with a NUL");
        };
        // SAFETY: a NUL-terminated path that lives across the call.
        let fd = unsafe { libc::open(name.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd < 0 {
            fail(&format!("open {path}: errno {}", errno()));
        }
        fd
    }

    /// `ioctl(fd, request, bytes)`, giving `-errno` or the result.
    fn ioctl(fd: libc::c_int, request: u32, bytes: &mut [u8]) -> i32 {
        // SAFETY: `bytes` is as long as the request's argument, which every
        // caller sizes from the request's own layout, and lives across it.
        let answer = unsafe { libc::ioctl(fd, request as _, bytes.as_mut_ptr()) };
        if answer < 0 { -errno() } else { answer }
    }

    fn ioctl_none(fd: libc::c_int, request: u32) -> i32 {
        // SAFETY: a request that takes no argument.
        let answer = unsafe { libc::ioctl(fd, request as _, 0) };
        if answer < 0 { -errno() } else { answer }
    }

    fn int(fd: libc::c_int, request: u32, value: i32) -> (i32, i32) {
        let mut bytes = value.to_le_bytes();
        let answer = ioctl(fd, request, &mut bytes);
        (answer, i32::from_le_bytes(bytes))
    }

    fn text(bytes: &[u8]) -> String {
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        String::from_utf8_lossy(bytes.get(..end).unwrap_or(&[])).into_owned()
    }

    /// What `snd_pcm_hw_params_any` sends: every mask full, every interval
    /// everything.
    fn any() -> HwParams {
        HwParams {
            flags: 0,
            masks: [Mask {
                bits: [u32::MAX; 8],
            }; 3],
            mres: [Mask { bits: [0; 8] }; 5],
            intervals: [Interval {
                min: 0,
                max: u32::MAX,
                flags: 0,
            }; 12],
            ires: [Interval {
                min: 0,
                max: 0,
                flags: 0,
            }; 9],
            rmask: u32::MAX,
            cmask: 0,
            info: u32::MAX,
            msbits: 0,
            rate_num: 0,
            rate_den: 0,
            fifo_size: 0,
            sync: [0; 16],
            reserved: [0; 48],
        }
    }

    fn hw(fd: libc::c_int, request: Pcm, params: &mut HwParams) -> i32 {
        let mut bytes = vec![0_u8; HwParams::size(width())];
        let _ = params.write(width(), &mut bytes);
        let answer = ioctl(fd, request.request(width()), &mut bytes);
        if answer == 0
            && let Some(read) = HwParams::read(width(), &bytes)
        {
            *params = read;
        }
        answer
    }

    fn card(control: libc::c_int) {
        let (answer, version) = int(control, Ctl::Pversion.request(width()), 0);
        if answer != 0 {
            fail(&format!("CTL_PVERSION: {answer}"));
        }
        let mut bytes = vec![0_u8; <CtlCardInfo as Field>::SIZE];
        let answer = ioctl(control, Ctl::CardInfo.request(width()), &mut bytes);
        let Some(info) = CtlCardInfo::read(&bytes).filter(|_| answer == 0) else {
            fail(&format!("CTL_CARD_INFO: {answer}"));
        };
        say(&format!(
            "card {} driver {} id {} name {} (control {:#x})",
            info.card,
            text(&info.driver),
            text(&info.id),
            text(&info.name),
            version
        ));
    }

    /// Refine and set the one configuration, as `snd_pcm_set_params` does.
    fn configure(pcm: libc::c_int) -> (u32, u32) {
        let (_, version) = int(pcm, Pcm::Pversion.request(width()), 0);
        if version as u32 != PCM_VERSION {
            fail(&format!("PVERSION {version:#x}"));
        }
        let _ = int(
            pcm,
            Pcm::UserPversion.request(width()),
            protocol_version(2, 0, 18) as i32,
        );
        let _ = int(
            pcm,
            Pcm::Ttstamp.request(width()),
            TSTAMP_TYPE_MONOTONIC as i32,
        );
        let mut space = any();
        let answer = hw(pcm, Pcm::HwRefine, &mut space);
        if answer != 0 {
            fail(&format!("HW_REFINE of everything: {answer}"));
        }
        // `set_rate_near(44100)` from below: nothing is at or under it.
        let mut below = space;
        if let Some(rate) = below
            .intervals
            .get_mut((HW_PARAM_RATE - HW_PARAM_FIRST_INTERVAL) as usize)
        {
            *rate = Interval {
                min: 0,
                max: 44_100,
                flags: 0,
            };
        }
        below.rmask = 1 << HW_PARAM_RATE;
        let answer = hw(pcm, Pcm::HwRefine, &mut below);
        if answer != -libc::EINVAL {
            fail(&format!(
                "a rate below 44100 was answered {answer}, not EINVAL"
            ));
        }
        let mut chosen = space;
        chosen.masks[0] = only(ACCESS_RW_INTERLEAVED);
        chosen.masks[1] = only(FORMAT_S16_LE);
        chosen.masks[2] = only(SUBFORMAT_STD);
        let answer = hw(pcm, Pcm::HwParams, &mut chosen);
        if answer != 0 {
            fail(&format!("HW_PARAMS: {answer}"));
        }
        let at = |param: u32| {
            chosen
                .intervals
                .get((param - HW_PARAM_FIRST_INTERVAL) as usize)
                .map_or(0, |interval| interval.min)
        };
        let (rate, channels) = (at(HW_PARAM_RATE), at(10));
        let (period, buffer) = (at(13), at(17));
        say(&format!(
            "configured {rate} Hz, {channels} channels, period {period}, buffer {buffer}"
        ));
        (period, buffer)
    }

    fn only(bit: u32) -> Mask {
        let mut bits = [0_u32; 8];
        if let Some(word) = bits.get_mut((bit / 32) as usize) {
            *word = 1 << (bit % 32);
        }
        Mask { bits }
    }

    fn software(pcm: libc::c_int, period: u32, buffer: u32) {
        let mut params = SwParams {
            tstamp_mode: 0,
            period_step: 1,
            sleep_min: 0,
            avail_min: u64::from(period),
            xfer_align: 1,
            start_threshold: u64::from(buffer),
            stop_threshold: u64::from(buffer),
            silence_threshold: 0,
            silence_size: 0,
            boundary: 0,
            proto: PCM_VERSION,
            tstamp_type: TSTAMP_TYPE_MONOTONIC,
            reserved: [0; 56],
        };
        let mut bytes = vec![0_u8; SwParams::size(width())];
        let _ = params.write(width(), &mut bytes);
        let answer = ioctl(pcm, Pcm::SwParams.request(width()), &mut bytes);
        if answer != 0 {
            fail(&format!("SW_PARAMS: {answer}"));
        }
        if let Some(read) = SwParams::read(width(), &bytes) {
            params = read;
        }
        say(&format!("boundary {}", params.boundary));
    }

    /// The frame at position `n` of what is played, counting from 1.
    fn frame_for(n: u32) -> u32 {
        if cfg!(feature = "negative-control") && (11 * PERIOD + 1..=12 * PERIOD).contains(&n) {
            n - PERIOD
        } else {
            n
        }
    }

    fn samples(from: u32, count: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(count as usize * 4);
        for n in from..from + count {
            let value = frame_for(n) as u16;
            bytes.extend_from_slice(&value.to_le_bytes());
            bytes.extend_from_slice(&(!value).to_le_bytes());
        }
        bytes
    }

    /// Write `data`, `count` frames, whole: `Err` with what a write that
    /// took nothing answered.
    fn write_frames(pcm: libc::c_int, data: &[u8], count: u32) -> Result<(), (i32, i64)> {
        let mut sent = 0;
        while sent < count {
            let xferi = Xferi {
                result: 0,
                buf: data.as_ptr() as u64 + u64::from(sent) * 4,
                frames: u64::from(count - sent),
            };
            let mut bytes = vec![0_u8; Xferi::size(width())];
            let _ = xferi.write(width(), &mut bytes);
            let answer = ioctl(pcm, Pcm::WriteiFrames.request(width()), &mut bytes);
            let result = Xferi::read(width(), &bytes).map_or(-1, |x| x.result);
            if answer != 0 || result <= 0 {
                return Err((answer, result));
            }
            sent += result as u32;
        }
        Ok(())
    }

    fn play(pcm: libc::c_int) {
        let mut next = 1;
        while next <= FRAMES {
            let count = BLOCK.min(FRAMES + 1 - next);
            let data = samples(next, count);
            if let Err((answer, result)) = write_frames(pcm, &data, count) {
                fail(&format!(
                    "WRITEI_FRAMES at frame {next}: {answer}, result {result}"
                ));
            }
            next += count;
        }
    }

    /// Every pid whose `comm` is `snd`.
    fn drivers() -> Vec<i32> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            fail("/proc could not be read");
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
            .filter(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .is_ok_and(|comm| comm.trim() == "snd")
            })
            .collect()
    }

    /// Up to `tries` looks, 10 ms apart, for `found` to give something.
    fn wait_for<T>(tries: u32, mut found: impl FnMut() -> Option<T>) -> Option<T> {
        for _ in 0..tries {
            if let Some(it) = found() {
                return Some(it);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }

    /// One round of the restart: silence playing, the driver killed, the
    /// stream disconnected, the card back. `killed` is the drivers earlier
    /// rounds killed, which a dead one may still be listed beside.
    fn survive_a_kill(round: u32, killed: &mut Vec<i32>) {
        let pcm = open(PCM, libc::O_RDWR);
        let (_, buffer) = configure(pcm);
        software(pcm, PERIOD, buffer);
        let answer = ioctl_none(pcm, Pcm::Prepare.request(width()));
        if answer != 0 {
            fail(&format!("round {round}: PREPARE: {answer}"));
        }
        let silence = vec![0_u8; BLOCK as usize * 4];
        // A buffer and a little more, so the stream has started.
        let mut written = 0;
        while written <= buffer + PERIOD {
            if let Err((answer, result)) = write_frames(pcm, &silence, BLOCK) {
                fail(&format!(
                    "round {round}: silence before the kill: {answer}, result {result}"
                ));
            }
            written += BLOCK;
        }
        // A driver is no Linux process under `personality=user`: a row's kill through
        // devmgr (M5-LIFT.md §11), by its slot in sysfs.
        let slot = std::fs::read_dir("/sys/bus/pci/drivers/snd")
            .ok()
            .and_then(|mut slots| {
                slots.find_map(|entry| {
                    let name = entry.ok()?.file_name().into_string().ok()?;
                    name.starts_with("0").then_some(name)
                })
            });
        let by_row = slot.as_ref().is_some_and(|slot| {
            drivers().is_empty()
                && std::fs::write("/sys/bus/pci/drivers/snd/unbind", format!("kill:{slot}")).is_ok()
        });
        if by_row {
            say(&format!("round {round}: killed snd by the row's path"));
        } else {
            let Some(pid) = wait_for(500, || {
                let fresh: Vec<i32> = drivers()
                    .into_iter()
                    .filter(|pid| !killed.contains(pid))
                    .collect();
                (fresh.len() == 1).then(|| fresh.first().copied()).flatten()
            }) else {
                fail(&format!("round {round}: no one snd driver in /proc"));
            };
            // SAFETY: `kill` takes a pid and a signal and touches no memory.
            if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
                fail(&format!("round {round}: kill {pid}: errno {}", errno()));
            }
            killed.push(pid);
            say(&format!("round {round}: killed snd {pid}"));
        }
        // Writes go on until the stream says its card went: ten seconds of
        // audio is far longer than devmgr's word of a death takes.
        let mut answered = None;
        for _ in 0..(10 * 48_000 / BLOCK) {
            if let Err((answer, _)) = write_frames(pcm, &silence, BLOCK) {
                answered = Some(answer);
                break;
            }
        }
        match answered {
            Some(answer) if answer == -libc::EBADFD => {
                say(&format!("round {round}: the stream answered EBADFD"));
            }
            Some(answer) => fail(&format!(
                "round {round}: a write on the dead card answered {answer}, not EBADFD"
            )),
            None => fail(&format!(
                "round {round}: ten seconds of writes after the kill all took"
            )),
        }
        // SAFETY: a descriptor this program opened and uses no more.
        let _ = unsafe { libc::close(pcm) };
        let Some(fd) = wait_for(3_000, || {
            let name = CString::new(PCM).ok()?;
            // SAFETY: a NUL-terminated path that lives across the call.
            let fd = unsafe { libc::open(name.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            (fd >= 0).then_some(fd)
        }) else {
            fail(&format!(
                "round {round}: {PCM} did not open again within 30 s: errno {}",
                errno()
            ));
        };
        // SAFETY: as above.
        let _ = unsafe { libc::close(fd) };
        say(&format!("round {round}: card0 is back"));
    }

    pub(crate) fn run() {
        if cfg!(feature = "negative-control") {
            say("negative control");
        }
        if cfg!(feature = "restart") {
            let mut killed = Vec::new();
            for round in 1..=2 {
                survive_a_kill(round, &mut killed);
            }
        }
        let control = open("/dev/snd/controlC0", libc::O_RDWR);
        card(control);
        let pcm = open(PCM, libc::O_RDWR);
        let (period, buffer) = configure(pcm);
        software(pcm, period, buffer);
        let answer = ioctl_none(pcm, Pcm::Prepare.request(width()));
        if answer != 0 {
            fail(&format!("PREPARE: {answer}"));
        }
        say("ready");
        play(pcm);
        let answer = ioctl_none(pcm, Pcm::Drain.request(width()));
        if answer != 0 {
            fail(&format!("DRAIN: {answer}"));
        }
        let mut bytes = vec![0_u8; Status::size(width())];
        let _ = ioctl(pcm, Pcm::StatusExt.request(width()), &mut bytes);
        if let Some(status) = Status::read(width(), &bytes) {
            say(&format!(
                "state {} appl_ptr {} hw_ptr {}",
                status.state, status.appl_ptr, status.hw_ptr
            ));
        }
        // SAFETY: descriptors this program opened and uses no more.
        let _ = unsafe { libc::close(pcm) };
        // SAFETY: as above.
        let _ = unsafe { libc::close(control) };
        say("done");
        rest()
    }
}
