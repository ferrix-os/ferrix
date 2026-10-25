//! The interface `usbhid` exported, as `ferrix-usb-serial`'s [`Usb`]: the
//! client half of `ferrix-usbif`.
//!
//! A control request is written, and its DONE waited for; whatever else
//! comes meanwhile -- IN chunks, SENT, EMPTY, LOST -- is taken as it comes,
//! so nothing waits behind the answer. OUT goes from the VMO's two slots, a
//! SEND each, each free again at its SENT; IN chunks are kept, by slot, in
//! the order they came, and each slot given back (FREE) once read.

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_rt::Kernel;
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::vmo::Vmo;
use ferrix_usb_serial::{CHUNK_BYTES, Setup, Usb};
use ferrix_usbif::{
    CONTROL_AT, CONTROL_BYTES, IN_AT, IN_SLOT_BYTES, IN_SLOTS, MAX_BYTES, Message, OUT_AT,
    OUT_SLOT_BYTES, OUT_SLOTS, Status,
};

/// How long a control request may take before the host is taken for
/// gone: past the host's own control patience, a second.
const CONTROL_PATIENCE: u64 = 2_000_000_000;
/// How long [`Usb::pause`] waits for the host.
const PAUSE: u64 = 500_000;

/// Why the interface failed a request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LinkError {
    /// The host's end closed: the device or the host went.
    Gone,
    /// The host answered with this.
    Status(Status),
    /// The host did not answer in time.
    Timeout,
}

/// The exported interface, from the class driver's side.
pub(crate) struct Link {
    pub(crate) channel: Channel<Kernel>,
    vmo: Vmo<Kernel>,
    /// The endpoints the driver streams: IN and OUT.
    inward: u8,
    outward: u8,
    tag: u32,
    /// The answer to the request being waited for.
    done: Option<(u32, Status, u32)>,
    /// IN chunks, oldest first: the slot and the length.
    chunks: [Option<(u8, u16)>; IN_SLOTS],
    chunks_len: usize,
    /// Each OUT slot's SEND, while the host holds it: its tag and length.
    sending: [Option<(u32, usize)>; OUT_SLOTS],
    /// Everything sent was taken by the device.
    empty: bool,
    /// Bytes the host said a stall lost, until taken.
    lost: usize,
    gone: bool,
}

impl Link {
    pub(crate) const fn new(
        channel: Channel<Kernel>,
        vmo: Vmo<Kernel>,
        inward: u8,
        outward: u8,
    ) -> Link {
        Link {
            channel,
            vmo,
            inward,
            outward,
            tag: 0,
            done: None,
            chunks: [None; IN_SLOTS],
            chunks_len: 0,
            sending: [None; OUT_SLOTS],
            empty: true,
            lost: 0,
            gone: false,
        }
    }

    /// Whether the host's end closed.
    pub(crate) const fn is_gone(&self) -> bool {
        self.gone
    }

    /// Bytes lost to a stall since this was last asked.
    pub(crate) fn take_lost(&mut self) -> usize {
        core::mem::take(&mut self.lost)
    }

    /// Take every message waiting, without waiting.
    pub(crate) fn take_messages(&mut self) {
        loop {
            let mut bytes = [0_u8; MAX_BYTES];
            let mut handles = [Handle::INVALID; 1];
            match self.channel.read(&mut bytes, &mut handles) {
                Ok(received) => {
                    for handle in handles.iter().take(received.handles) {
                        drop(OwnedHandle::from_raw(Kernel, *handle));
                    }
                    if let Ok(message) =
                        Message::decode(bytes.get(..received.bytes).unwrap_or_default())
                    {
                        self.take(message);
                    }
                }
                Err(ReadError::Failed(Error::ShouldWait)) => return,
                Err(_) => {
                    self.gone = true;
                    return;
                }
            }
        }
    }

    fn take(&mut self, message: Message) {
        match message {
            Message::Done { tag, status, value } => self.done = Some((tag, status, value)),
            Message::In { slot, len, .. } => {
                if let Some(entry) = self.chunks.get_mut(self.chunks_len) {
                    *entry = Some((slot, len));
                    self.chunks_len += 1;
                }
            }
            Message::Sent { tag, status } => {
                for entry in &mut self.sending {
                    if entry.is_some_and(|(sent, _)| sent == tag) {
                        *entry = None;
                    }
                }
                if status != Status::Ok {
                    ferrix_rt::warn!("usbserial: a write was dropped: {status:?}");
                }
            }
            Message::Empty { .. } => {
                if self.sending.iter().all(Option::is_none) {
                    self.empty = true;
                }
            }
            Message::Lost { bytes, .. } => {
                self.lost = self.lost.saturating_add(bytes as usize);
            }
            _ => {}
        }
    }

    fn wait(&self, nanos: u64) {
        let until = ferrix_rt::linux::monotonic_nanos()
            .unwrap_or(0)
            .saturating_add(nanos);
        let _ = self.channel.wait_one(
            Signals::READABLE | Signals::PEER_CLOSED,
            Deadline::At(until),
        );
    }

    fn request(&mut self, message: impl FnOnce(u32) -> Message) -> Result<u32, LinkError> {
        if self.gone {
            return Err(LinkError::Gone);
        }
        self.tag = self.tag.wrapping_add(1);
        let tag = self.tag;
        self.done = None;
        if self
            .channel
            .write(message(tag).encode().as_bytes())
            .is_err()
        {
            self.gone = true;
            return Err(LinkError::Gone);
        }
        let until = ferrix_rt::linux::monotonic_nanos()
            .unwrap_or(0)
            .saturating_add(CONTROL_PATIENCE);
        loop {
            self.take_messages();
            if let Some((answered, status, value)) = self.done
                && answered == tag
            {
                self.done = None;
                return if status == Status::Ok {
                    Ok(value)
                } else {
                    Err(LinkError::Status(status))
                };
            }
            if self.gone {
                return Err(LinkError::Gone);
            }
            if ferrix_rt::linux::monotonic_nanos().unwrap_or(0) >= until {
                return Err(LinkError::Timeout);
            }
            self.wait(CONTROL_PATIENCE / 20);
        }
    }

    /// Open the streams: IN read ahead, OUT never waited for.
    pub(crate) fn open_streams(&mut self) -> Result<(), LinkError> {
        let (inward, outward) = (self.inward, self.outward);
        let _ = self.request(|tag| Message::Open {
            tag,
            endpoint: inward,
        })?;
        let _ = self.request(|tag| Message::Open {
            tag,
            endpoint: outward,
        })?;
        Ok(())
    }

    /// How many bytes [`Usb::send`] takes now.
    pub(crate) fn room(&self) -> usize {
        self.sending.iter().filter(|entry| entry.is_none()).count() * OUT_SLOT_BYTES
    }
}

impl Usb for Link {
    type Error = LinkError;

    fn control(&mut self, setup: &Setup, data: &mut [u8]) -> Result<usize, LinkError> {
        let bytes = setup.bytes();
        let length = usize::from(setup.length).min(CONTROL_BYTES);
        let inward = bytes[0] & 0x80 != 0;
        if !inward && length > 0 {
            let stage = data
                .get(..length)
                .ok_or(LinkError::Status(Status::Invalid))?;
            self.vmo
                .write(stage, CONTROL_AT as u64)
                .map_err(|_| LinkError::Gone)?;
        }
        let moved = self.request(|tag| Message::Control { tag, setup: bytes })? as usize;
        if inward && moved > 0 {
            let stage = data
                .get_mut(..moved.min(length))
                .ok_or(LinkError::Status(Status::Invalid))?;
            self.vmo
                .read(stage, CONTROL_AT as u64)
                .map_err(|_| LinkError::Gone)?;
        }
        Ok(moved)
    }

    fn send(&mut self, bytes: &[u8]) -> Result<usize, LinkError> {
        if self.gone {
            return Err(LinkError::Gone);
        }
        let mut sent = 0;
        while sent < bytes.len() {
            let Some(slot) = self.sending.iter().position(Option::is_none) else {
                break;
            };
            let piece = bytes
                .get(sent..bytes.len().min(sent + OUT_SLOT_BYTES))
                .unwrap_or_default();
            let offset = OUT_AT + slot * OUT_SLOT_BYTES;
            self.vmo
                .write(piece, offset as u64)
                .map_err(|_| LinkError::Gone)?;
            self.tag = self.tag.wrapping_add(1);
            let message = Message::Send {
                tag: self.tag,
                endpoint: self.outward,
                slot: u8::try_from(slot).unwrap_or(0),
                len: u16::try_from(piece.len()).unwrap_or(0),
            };
            if self.channel.write(message.encode().as_bytes()).is_err() {
                self.gone = true;
                return Err(LinkError::Gone);
            }
            if let Some(entry) = self.sending.get_mut(slot) {
                *entry = Some((self.tag, piece.len()));
            }
            self.empty = false;
            sent += piece.len();
        }
        Ok(sent)
    }

    fn in_flight(&self) -> usize {
        self.sending.iter().flatten().map(|(_, len)| *len).sum()
    }

    fn take_chunk(&mut self, into: &mut [u8; CHUNK_BYTES]) -> Option<usize> {
        if self.chunks_len == 0 {
            self.take_messages();
        }
        let (slot, len) = self.chunks.first().copied().flatten()?;
        self.chunks.rotate_left(1);
        if let Some(last) = self.chunks.last_mut() {
            *last = None;
        }
        self.chunks_len -= 1;
        let len = usize::from(len).min(IN_SLOT_BYTES).min(CHUNK_BYTES);
        let offset = IN_AT + usize::from(slot) * IN_SLOT_BYTES;
        let read = self
            .vmo
            .read(into.get_mut(..len).unwrap_or_default(), offset as u64);
        let _ = self
            .channel
            .write(Message::Free { slot }.encode().as_bytes());
        read.ok().map(|()| len)
    }

    fn pause(&mut self) -> Result<(), LinkError> {
        self.wait(PAUSE);
        self.take_messages();
        if self.gone {
            Err(LinkError::Gone)
        } else {
            Ok(())
        }
    }

    fn now(&self) -> u64 {
        ferrix_rt::linux::monotonic_nanos().unwrap_or(0)
    }
}
