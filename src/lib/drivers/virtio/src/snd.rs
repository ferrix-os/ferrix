//! virtio-snd's device protocol: its queues, its configuration block, the
//! control requests that set a PCM stream up and run it, and the header and
//! status around every buffer of samples.
//!
//! Virtio 1.2 §5.14 defines the sound device. A driver asks each stream what
//! it can do (`PCM_INFO`), gives it one configuration (`PCM_SET_PARAMS`),
//! prepares and starts it, and then posts buffers of samples on the transmit
//! queue, each with a [`XFER_BYTES`] header naming the stream before it and
//! a [`STATUS_BYTES`] status after it for the device to write. What drives a
//! device -- how many buffers to keep posted, what a completion means for the
//! program writing samples, what to do when the device misbehaves -- is a
//! driver's, as in every other module here; `docs/AUDIO.md` §3.3 is that
//! driver.
//!
//! # Where the numbers come from
//!
//! Virtio 1.2 §5.14, checked against QEMU 9.2.4's copy of Linux's header,
//! `include/standard-headers/linux/virtio_snd.h`, and against the device QEMU
//! builds from it in `hw/audio/virtio-snd.c`.
//!
//! # What QEMU's device does
//!
//! Five things the specification lets a driver expect otherwise, each of
//! which a driver for QEMU 9.2.4 has to live with (`docs/AUDIO.md` §3.3):
//!
//! * **Two streams by default, output first.** Stream `i` plays when `i <
//!   streams / 2 + streams % 2` and records otherwise
//!   (`virtio_snd_pcm_prepare`), so the default of two is one each way.
//! * **Every stream is configured and prepared at realize**, S16, two
//!   channels, 48 kHz, so `PCM_INFO` answers before any `PCM_SET_PARAMS`,
//!   and its `channels_max` is the count *currently set*, not what the
//!   backend could take.
//! * **The event queue is not implemented.** No [`EVENT_PCM_PERIOD_ELAPSED`]
//!   or [`EVENT_PCM_XRUN`] ever arrives; a completion on the transmit queue
//!   is the only clock.
//! * **A buffer comes back only once the backend has consumed all of it**, at
//!   the audio rate, so completions pace playback.
//! * **`latency_bytes` is the buffer's own size**, not a latency.
//!
//! # Trust
//!
//! Every response is the device's word. A response shorter than its request
//! expects is refused rather than padded, and so is a status code other than
//! [`STATUS_OK`]; a configuration block declaring no streams, or more than
//! [`MAX_STREAMS`], is refused; a stream's information naming a direction
//! that does not exist, or a channel range that is empty, is refused. Format
//! and rate bits beyond the ones defined are not an error, since a later
//! device may offer more, and [`PcmInfo::formats`] and [`PcmInfo::rates`]
//! leave them out. An event this module does not know is
//! [`Event::Unknown`], for the same reason.

use crate::pci::{FEATURE_ACCESS_PLATFORM, FEATURE_VERSION_1};

/// The configuration-block reader, which every device class shares.
pub use crate::DeviceConfig;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// The device, virtio 1.2 §5.14.
// ---------------------------------------------------------------------------

/// `VIRTIO_ID_SOUND`: the virtio device id.
pub const DEVICE_ID: u16 = 25;
/// The modern PCI device id, `0x1040` plus [`DEVICE_ID`]. virtio-snd has no
/// transitional one.
pub const PCI_DEVICE_ID: u16 = 0x1059;

/// `VIRTIO_SND_F_CTLS`: the device has control elements. Not asked for in
/// version 1, which publishes none (`docs/AUDIO.md` §7), and QEMU 9.2.4 does
/// not offer it.
pub const FEATURE_CTLS: u64 = 1 << 0;

/// The features the driver accepts: the transport's [`FEATURE_VERSION_1`],
/// and [`FEATURE_ACCESS_PLATFORM`], without which a device behind the
/// machine's IOMMU refuses `FEATURES_OK`.
pub const DRIVER_FEATURES: u64 = FEATURE_VERSION_1 | FEATURE_ACCESS_PLATFORM;

/// The features without which the driver gives up.
pub const REQUIRED_FEATURES: u64 = FEATURE_VERSION_1;

// ---------------------------------------------------------------------------
// The queues, virtio 1.2 §5.14.2.
// ---------------------------------------------------------------------------

/// `VIRTIO_SND_VQ_CONTROL`: requests from the driver, answered in place.
pub const CONTROL_QUEUE: u16 = 0;
/// `VIRTIO_SND_VQ_EVENT`: device-writable buffers for [`Event`]s.
pub const EVENT_QUEUE: u16 = 1;
/// `VIRTIO_SND_VQ_TX`: samples to play.
pub const TX_QUEUE: u16 = 2;
/// `VIRTIO_SND_VQ_RX`: buffers for samples recorded.
pub const RX_QUEUE: u16 = 3;
/// `VIRTIO_SND_VQ_MAX`: how many queues the device has.
pub const QUEUE_COUNT: u16 = 4;

// ---------------------------------------------------------------------------
// Configuration space, virtio 1.2 §5.14.4.
// ---------------------------------------------------------------------------

/// Offset of `jacks`.
pub const CONFIG_JACKS: u32 = 0;
/// Offset of `streams`.
pub const CONFIG_STREAMS: u32 = 4;
/// Offset of `chmaps`.
pub const CONFIG_CHMAPS: u32 = 8;
/// Offset of `controls`, which is only there with [`FEATURE_CTLS`].
pub const CONFIG_CONTROLS: u32 = 12;
/// Bytes of the block a driver without [`FEATURE_CTLS`] reads.
pub const CONFIG_LEN: u32 = 12;

/// The most streams this crate will drive.
///
/// Virtio fixes no maximum; QEMU refuses more than 10 at realize
/// (`virtio_snd_realize`). The bound is here so that a device declaring four
/// billion streams is refused at the configuration block rather than believed
/// all the way down to a query sized from it.
pub const MAX_STREAMS: u32 = 10;

/// What the configuration block says about the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// `jacks`: physical jacks, which version 1 does not query.
    pub jacks: u32,
    /// `streams`: PCM streams, at most [`MAX_STREAMS`].
    pub streams: u32,
    /// `chmaps`: channel maps, which version 1 does not query.
    pub chmaps: u32,
}

impl Config {
    /// Read the block.
    ///
    /// # Errors
    ///
    /// [`SndError::Config`] for a block that does not reach `chmaps`, and
    /// [`SndError::Streams`] for a stream count of zero, which has nothing to
    /// play, or one above [`MAX_STREAMS`].
    pub fn read<C: DeviceConfig + ?Sized>(config: &C) -> Result<Config, SndError> {
        // A block that does not reach `chmaps` is not this device's: reading
        // past it would read whatever the transport returns for an absent
        // byte, which on QEMU is `0xff`.
        if config.config_len() < CONFIG_LEN {
            return Err(SndError::Config);
        }
        let streams = config.config_read32(CONFIG_STREAMS);
        if streams == 0 || streams > MAX_STREAMS {
            return Err(SndError::Streams(streams));
        }
        Ok(Config {
            jacks: config.config_read32(CONFIG_JACKS),
            streams,
            chmaps: config.config_read32(CONFIG_CHMAPS),
        })
    }
}

// ---------------------------------------------------------------------------
// Codes, virtio 1.2 §5.14.6.
// ---------------------------------------------------------------------------

/// `VIRTIO_SND_R_JACK_INFO`.
pub const REQUEST_JACK_INFO: u32 = 0x0001;
/// `VIRTIO_SND_R_JACK_REMAP`.
pub const REQUEST_JACK_REMAP: u32 = 0x0002;
/// `VIRTIO_SND_R_PCM_INFO`: what each stream can do.
pub const REQUEST_PCM_INFO: u32 = 0x0100;
/// `VIRTIO_SND_R_PCM_SET_PARAMS`: one stream's configuration.
pub const REQUEST_PCM_SET_PARAMS: u32 = 0x0101;
/// `VIRTIO_SND_R_PCM_PREPARE`.
pub const REQUEST_PCM_PREPARE: u32 = 0x0102;
/// `VIRTIO_SND_R_PCM_RELEASE`: give back every buffer posted.
pub const REQUEST_PCM_RELEASE: u32 = 0x0103;
/// `VIRTIO_SND_R_PCM_START`.
pub const REQUEST_PCM_START: u32 = 0x0104;
/// `VIRTIO_SND_R_PCM_STOP`.
pub const REQUEST_PCM_STOP: u32 = 0x0105;
/// `VIRTIO_SND_R_CHMAP_INFO`.
pub const REQUEST_CHMAP_INFO: u32 = 0x0200;

/// `VIRTIO_SND_EVT_JACK_CONNECTED`.
pub const EVENT_JACK_CONNECTED: u32 = 0x1000;
/// `VIRTIO_SND_EVT_JACK_DISCONNECTED`.
pub const EVENT_JACK_DISCONNECTED: u32 = 0x1001;
/// `VIRTIO_SND_EVT_PCM_PERIOD_ELAPSED`, which QEMU 9.2.4 never sends.
pub const EVENT_PCM_PERIOD_ELAPSED: u32 = 0x1100;
/// `VIRTIO_SND_EVT_PCM_XRUN`, which QEMU 9.2.4 never sends.
pub const EVENT_PCM_XRUN: u32 = 0x1101;
/// `VIRTIO_SND_EVT_CTL_NOTIFY`.
pub const EVENT_CTL_NOTIFY: u32 = 0x1200;

/// `VIRTIO_SND_S_OK`.
pub const STATUS_OK: u32 = 0x8000;
/// `VIRTIO_SND_S_BAD_MSG`: the request was malformed or out of order.
pub const STATUS_BAD_MSG: u32 = 0x8001;
/// `VIRTIO_SND_S_NOT_SUPP`: the configuration asked for is not offered.
pub const STATUS_NOT_SUPP: u32 = 0x8002;
/// `VIRTIO_SND_S_IO_ERR`.
pub const STATUS_IO_ERR: u32 = 0x8003;

/// `VIRTIO_SND_D_OUTPUT`: a stream that plays.
pub const DIRECTION_OUTPUT: u8 = 0;
/// `VIRTIO_SND_D_INPUT`: a stream that records.
pub const DIRECTION_INPUT: u8 = 1;

/// `VIRTIO_SND_PCM_F_SHMEM_HOST`.
pub const PCM_FEATURE_SHMEM_HOST: u32 = 1 << 0;
/// `VIRTIO_SND_PCM_F_SHMEM_GUEST`.
pub const PCM_FEATURE_SHMEM_GUEST: u32 = 1 << 1;
/// `VIRTIO_SND_PCM_F_MSG_POLLING`.
pub const PCM_FEATURE_MSG_POLLING: u32 = 1 << 2;
/// `VIRTIO_SND_PCM_F_EVT_SHMEM_PERIODS`.
pub const PCM_FEATURE_EVT_SHMEM_PERIODS: u32 = 1 << 3;
/// `VIRTIO_SND_PCM_F_EVT_XRUNS`.
pub const PCM_FEATURE_EVT_XRUNS: u32 = 1 << 4;

/// `VIRTIO_SND_PCM_FMT_S8`: a bit position in [`PcmInfo::formats`], and a
/// value of [`SetParams::format`].
pub const FORMAT_S8: u8 = 3;
/// `VIRTIO_SND_PCM_FMT_U8`.
pub const FORMAT_U8: u8 = 4;
/// `VIRTIO_SND_PCM_FMT_S16`: little-endian, the one version 1 uses.
pub const FORMAT_S16: u8 = 5;
/// `VIRTIO_SND_PCM_FMT_U16`.
pub const FORMAT_U16: u8 = 6;
/// `VIRTIO_SND_PCM_FMT_S32`.
pub const FORMAT_S32: u8 = 17;
/// `VIRTIO_SND_PCM_FMT_U32`.
pub const FORMAT_U32: u8 = 18;
/// `VIRTIO_SND_PCM_FMT_FLOAT`.
pub const FORMAT_FLOAT: u8 = 19;
/// `VIRTIO_SND_PCM_FMT_FLOAT64`.
pub const FORMAT_FLOAT64: u8 = 20;
/// `VIRTIO_SND_PCM_FMT_IEC958_SUBFRAME`, the last format defined.
pub const FORMAT_IEC958_SUBFRAME: u8 = 24;

/// Every format bit defined, `IMA_ADPCM` to `IEC958_SUBFRAME`.
pub const FORMATS_DEFINED: u64 = (1 << (FORMAT_IEC958_SUBFRAME as u64 + 1)) - 1;

/// The rate each `VIRTIO_SND_PCM_RATE_*` index stands for, in Hz.
pub const RATES_HZ: [u32; 14] = [
    5512, 8000, 11025, 16000, 22050, 32000, 44100, 48000, 64000, 88200, 96000, 176_400, 192_000,
    384_000,
];
/// `VIRTIO_SND_PCM_RATE_48000`: the one version 1 uses.
pub const RATE_48000: u8 = 7;

/// Every rate bit defined.
pub const RATES_DEFINED: u64 = (1 << RATES_HZ.len()) - 1;

/// The rate index for `hz`, if the device's enum has one.
#[must_use]
pub fn rate_index(hz: u32) -> Option<u8> {
    RATES_HZ
        .iter()
        .position(|rate| *rate == hz)
        .and_then(|index| u8::try_from(index).ok())
}

// ---------------------------------------------------------------------------
// Requests, virtio 1.2 §5.14.6.
// ---------------------------------------------------------------------------

/// Bytes of `struct virtio_snd_query_info`: `code`, `start_id`, `count`,
/// `size`.
pub const QUERY_BYTES: usize = 16;
/// Bytes of `struct virtio_snd_pcm_hdr`: `code`, `stream_id`.
pub const PCM_HDR_BYTES: usize = 8;
/// Bytes of `struct virtio_snd_pcm_set_params`.
pub const SET_PARAMS_BYTES: usize = 24;
/// Bytes of `struct virtio_snd_hdr`: a response's `code`.
pub const RESPONSE_BYTES: usize = 4;
/// Bytes of one `struct virtio_snd_pcm_info`.
pub const PCM_INFO_BYTES: usize = 32;
/// Bytes of `struct virtio_snd_pcm_xfer`, the header before every buffer of
/// samples.
pub const XFER_BYTES: usize = 4;
/// Bytes of `struct virtio_snd_pcm_status`, after every buffer of samples.
pub const STATUS_BYTES: usize = 8;
/// Bytes of `struct virtio_snd_event`.
pub const EVENT_BYTES: usize = 8;

/// Write `fields` as little-endian words into the start of `out`.
fn put_words(out: &mut [u8], fields: &[u32]) -> Result<usize, SndError> {
    let want = fields.len().saturating_mul(4);
    let have = out.len();
    let slot = out.get_mut(..want).ok_or(SndError::Short { want, have })?;
    for (cell, field) in slot.as_chunks_mut::<4>().0.iter_mut().zip(fields) {
        cell.copy_from_slice(&field.to_le_bytes());
    }
    Ok(want)
}

/// The little-endian word at `at`.
fn word(bytes: &[u8], at: usize) -> Result<u32, SndError> {
    let want = at.saturating_add(4);
    let field = bytes.get(at..want).ok_or(SndError::Short {
        want,
        have: bytes.len(),
    })?;
    let mut value = [0_u8; 4];
    value.copy_from_slice(field);
    Ok(u32::from_le_bytes(value))
}

/// The little-endian double word at `at`.
fn double(bytes: &[u8], at: usize) -> Result<u64, SndError> {
    Ok(u64::from(word(bytes, at)?) | (u64::from(word(bytes, at.saturating_add(4))?) << 32))
}

/// The byte at `at`.
fn byte(bytes: &[u8], at: usize) -> Result<u8, SndError> {
    bytes.get(at).copied().ok_or(SndError::Short {
        want: at.saturating_add(1),
        have: bytes.len(),
    })
}

/// Write a `PCM_INFO` query for `count` streams from `start` into `out`, and
/// say how many bytes it took, which is always [`QUERY_BYTES`]. The response
/// buffer the driver posts after it holds [`pcm_info_response_bytes`].
///
/// # Errors
///
/// [`SndError::Short`] for a buffer smaller than the query.
pub fn write_pcm_info_query(start: u32, count: u32, out: &mut [u8]) -> Result<usize, SndError> {
    // `PCM_INFO_BYTES` is 32, far inside a `u32`.
    put_words(
        out,
        &[REQUEST_PCM_INFO, start, count, PCM_INFO_BYTES as u32],
    )
}

/// Bytes of the response to a `PCM_INFO` query for `count` streams: the
/// status, then one [`PCM_INFO_BYTES`] entry each.
#[must_use]
pub const fn pcm_info_response_bytes(count: u32) -> usize {
    // `count` is at most `MAX_STREAMS` for any query this crate's driver
    // makes; for any other it saturates rather than wraps.
    RESPONSE_BYTES.saturating_add((count as usize).saturating_mul(PCM_INFO_BYTES))
}

/// The four requests that name a stream and carry nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmCommand {
    /// `PCM_PREPARE`.
    Prepare,
    /// `PCM_RELEASE`.
    Release,
    /// `PCM_START`.
    Start,
    /// `PCM_STOP`.
    Stop,
}

impl PcmCommand {
    /// Its request code.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            PcmCommand::Prepare => REQUEST_PCM_PREPARE,
            PcmCommand::Release => REQUEST_PCM_RELEASE,
            PcmCommand::Start => REQUEST_PCM_START,
            PcmCommand::Stop => REQUEST_PCM_STOP,
        }
    }

    /// Write it for `stream` into `out`, and say how many bytes it took,
    /// which is always [`PCM_HDR_BYTES`]. The response is [`RESPONSE_BYTES`].
    ///
    /// # Errors
    ///
    /// [`SndError::Short`] for a buffer smaller than that.
    pub fn write(self, stream: u32, out: &mut [u8]) -> Result<usize, SndError> {
        put_words(out, &[self.code(), stream])
    }
}

/// `struct virtio_snd_pcm_set_params`: one stream's configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetParams {
    /// Which stream.
    pub stream: u32,
    /// Its buffer, in bytes.
    pub buffer_bytes: u32,
    /// Its period, in bytes.
    pub period_bytes: u32,
    /// `PCM_FEATURE_*` chosen; none in version 1.
    pub features: u32,
    /// Channels.
    pub channels: u8,
    /// `FORMAT_*`.
    pub format: u8,
    /// A rate index, as [`rate_index`] gives.
    pub rate: u8,
}

impl SetParams {
    /// Write it into `out`, and say how many bytes it took, which is always
    /// [`SET_PARAMS_BYTES`]. The response is [`RESPONSE_BYTES`].
    ///
    /// # Errors
    ///
    /// [`SndError::Short`] for a buffer smaller than that.
    pub fn write(&self, out: &mut [u8]) -> Result<usize, SndError> {
        let have = out.len();
        let slot = out.get_mut(..SET_PARAMS_BYTES).ok_or(SndError::Short {
            want: SET_PARAMS_BYTES,
            have,
        })?;
        let tail = u32::from_le_bytes([self.channels, self.format, self.rate, 0]);
        put_words(
            slot,
            &[
                REQUEST_PCM_SET_PARAMS,
                self.stream,
                self.buffer_bytes,
                self.period_bytes,
                self.features,
                tail,
            ],
        )
    }
}

/// Write the header that goes before a buffer of samples for `stream`, and
/// say how many bytes it took, which is always [`XFER_BYTES`].
///
/// # Errors
///
/// [`SndError::Short`] for a buffer smaller than that.
pub fn write_xfer(stream: u32, out: &mut [u8]) -> Result<usize, SndError> {
    put_words(out, &[stream])
}

// ---------------------------------------------------------------------------
// Responses, virtio 1.2 §5.14.6.
// ---------------------------------------------------------------------------

/// Check the status at the start of a control response.
///
/// # Errors
///
/// [`SndError::Short`] for fewer than [`RESPONSE_BYTES`] bytes, and
/// [`SndError::Status`] for any code but [`STATUS_OK`].
pub fn check_response(bytes: &[u8]) -> Result<(), SndError> {
    match word(bytes, 0)? {
        STATUS_OK => Ok(()),
        code => Err(SndError::Status(code)),
    }
}

/// `struct virtio_snd_pcm_info`: what one stream can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmInfo {
    /// The HDA function group node, which Ferrix does not use.
    pub hda_fn_nid: u32,
    /// `PCM_FEATURE_*` offered.
    pub features: u32,
    /// Format bits the device set, defined or not.
    pub raw_formats: u64,
    /// Rate bits the device set, defined or not.
    pub raw_rates: u64,
    /// `DIRECTION_OUTPUT` or `DIRECTION_INPUT`.
    pub direction: u8,
    /// Fewest channels.
    pub channels_min: u8,
    /// Most channels. On QEMU 9.2.4 the count currently set.
    pub channels_max: u8,
}

impl PcmInfo {
    /// Read entry `index` of a `PCM_INFO` response to a query for `count`
    /// streams, checking the response's status first.
    ///
    /// # Errors
    ///
    /// [`SndError::Status`] for a response that is not [`STATUS_OK`],
    /// [`SndError::Stream`] for an `index` at or beyond `count`,
    /// [`SndError::Short`] for a response shorter than `count` entries, and
    /// [`SndError::Direction`] or [`SndError::Channels`] for an entry the
    /// device should not have written.
    pub fn read(bytes: &[u8], index: u32, count: u32) -> Result<PcmInfo, SndError> {
        check_response(bytes)?;
        if index >= count {
            return Err(SndError::Stream(index));
        }
        // The whole response is required, not just this entry: a device that
        // answered fewer entries than were asked for answered a different
        // question.
        let want = pcm_info_response_bytes(count);
        if bytes.len() < want {
            return Err(SndError::Short {
                want,
                have: bytes.len(),
            });
        }
        // `index` is below `count`, whose response fits `bytes`.
        let at = RESPONSE_BYTES.saturating_add((index as usize).saturating_mul(PCM_INFO_BYTES));
        let info = PcmInfo {
            hda_fn_nid: word(bytes, at)?,
            features: word(bytes, at.saturating_add(4))?,
            raw_formats: double(bytes, at.saturating_add(8))?,
            raw_rates: double(bytes, at.saturating_add(16))?,
            direction: byte(bytes, at.saturating_add(24))?,
            channels_min: byte(bytes, at.saturating_add(25))?,
            channels_max: byte(bytes, at.saturating_add(26))?,
        };
        if info.direction > DIRECTION_INPUT {
            return Err(SndError::Direction(info.direction));
        }
        if info.channels_min == 0 || info.channels_min > info.channels_max {
            return Err(SndError::Channels {
                min: info.channels_min,
                max: info.channels_max,
            });
        }
        Ok(info)
    }

    /// The format bits defined, `FORMAT_*`.
    #[must_use]
    pub const fn formats(&self) -> u64 {
        self.raw_formats & FORMATS_DEFINED
    }

    /// The rate bits defined, indices into [`RATES_HZ`].
    #[must_use]
    pub const fn rates(&self) -> u64 {
        self.raw_rates & RATES_DEFINED
    }

    /// Whether it offers `format`.
    #[must_use]
    pub const fn has_format(&self, format: u8) -> bool {
        format <= FORMAT_IEC958_SUBFRAME && self.formats() & (1 << format) != 0
    }

    /// Whether it offers the rate at index `rate`.
    #[must_use]
    pub const fn has_rate(&self, rate: u8) -> bool {
        (rate as usize) < RATES_HZ.len() && self.rates() & (1 << rate) != 0
    }

    /// Whether it takes `channels` channels.
    #[must_use]
    pub const fn has_channels(&self, channels: u8) -> bool {
        channels >= self.channels_min && channels <= self.channels_max
    }
}

/// `struct virtio_snd_pcm_status`: what the device wrote after a buffer of
/// samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmStatus {
    /// Bytes the device reports still to play. On QEMU 9.2.4 the buffer's
    /// own size.
    pub latency_bytes: u32,
}

impl PcmStatus {
    /// Read the status of a completed buffer, `written` being the length the
    /// used ring reported, which for a transmit buffer is the status alone.
    ///
    /// # Errors
    ///
    /// [`SndError::Written`] for a completion that wrote anything but
    /// [`STATUS_BYTES`], [`SndError::Short`] for a buffer shorter than that,
    /// and [`SndError::Status`] for any status but [`STATUS_OK`].
    pub fn read(bytes: &[u8], written: u32) -> Result<PcmStatus, SndError> {
        // `STATUS_BYTES` is 8.
        if written != STATUS_BYTES as u32 {
            return Err(SndError::Written(written));
        }
        match word(bytes, 0)? {
            STATUS_OK => Ok(PcmStatus {
                latency_bytes: word(bytes, 4)?,
            }),
            code => Err(SndError::Status(code)),
        }
    }
}

/// `struct virtio_snd_event`: what the device writes on [`EVENT_QUEUE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A jack was connected.
    JackConnected {
        /// Which jack.
        jack: u32,
    },
    /// A jack was disconnected.
    JackDisconnected {
        /// Which jack.
        jack: u32,
    },
    /// A stream's period elapsed.
    PeriodElapsed {
        /// Which stream.
        stream: u32,
    },
    /// A stream under- or overran.
    Xrun {
        /// Which stream.
        stream: u32,
    },
    /// A control element changed.
    ControlNotify {
        /// Which element.
        control: u32,
    },
    /// An event this crate does not know, which is not an error.
    Unknown {
        /// The event code.
        code: u32,
        /// Its data.
        data: u32,
    },
}

impl Event {
    /// Read an event the device wrote.
    ///
    /// # Errors
    ///
    /// [`SndError::Short`] for fewer than [`EVENT_BYTES`] bytes.
    pub fn read(bytes: &[u8]) -> Result<Event, SndError> {
        let code = word(bytes, 0)?;
        let data = word(bytes, 4)?;
        Ok(match code {
            EVENT_JACK_CONNECTED => Event::JackConnected { jack: data },
            EVENT_JACK_DISCONNECTED => Event::JackDisconnected { jack: data },
            EVENT_PCM_PERIOD_ELAPSED => Event::PeriodElapsed { stream: data },
            EVENT_PCM_XRUN => Event::Xrun { stream: data },
            EVENT_CTL_NOTIFY => Event::ControlNotify { control: data },
            code => Event::Unknown { code, data },
        })
    }
}

/// Why a sound message or block was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SndError {
    /// Fewer bytes than the message needs.
    Short {
        /// Bytes it needs.
        want: usize,
        /// Bytes there are.
        have: usize,
    },
    /// The configuration block does not reach `chmaps`.
    Config,
    /// A stream count of zero, or above [`MAX_STREAMS`].
    Streams(u32),
    /// A response whose status is not [`STATUS_OK`]: the code.
    Status(u32),
    /// A stream index at or beyond what was asked for.
    Stream(u32),
    /// A direction that is neither output nor input.
    Direction(u8),
    /// A channel range that is empty, or starts at zero.
    Channels {
        /// Its minimum.
        min: u8,
        /// Its maximum.
        max: u8,
    },
    /// A transmit completion that wrote this many bytes rather than a
    /// status's.
    Written(u32),
}
