//! The interfaces this process drives no class of, served to class
//! drivers in processes of their own (`ferrix-usbif`, `docs/USB-SERIAL.md`
//! §6.1).
//!
//! Each exported interface gets a channel and a VMO of its own. The
//! driver's end of the channel goes to devmgr in ATTACH, on this process's
//! bootstrap channel, and devmgr starts the class driver its table names
//! with it as its bootstrap; HELLO and the VMO wait in the channel for it.
//! Requests are taken as the channel becomes readable; IN chunks and the
//! progress of what was sent are handed on each time the loop comes round,
//! so a driver's bytes move as soon as the controller's interrupt says.
//!
//! The driver's end closing is its going: what it opened is closed, and
//! the interface attached again on a new channel, which is how devmgr
//! starts a dead driver again. The device going drops the channel, and the
//! driver hears it closed.
//!
//! The controller's DMA memory stays this process's: every byte is copied
//! between it and the VMO, and no page of a class driver's is pinned.

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::{Requested, Rights};
use ferrix_native_abi::signals::Signals;
use ferrix_rt::Kernel;
use ferrix_rt::native::channel::{self, Channel, ReadError};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Object, OwnedHandle};
use ferrix_rt::native::port::Port;
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_usb_host::bus::{Bus, ExportError, MAX_EXPORTS};
use ferrix_usb_host::host::Host;
use ferrix_usbif::{
    CONTROL_AT, CONTROL_BYTES, IN_AT, IN_SLOT_BYTES, IN_SLOTS, MAX_BYTES, MAX_ENDPOINTS, Message,
    OUT_AT, OUT_SLOT_BYTES, OUT_SLOTS, Status, VMO_BYTES,
};

/// The port keys exports are watched under: this, plus the export's index.
pub(crate) const KEY_FIRST_EXPORT: u64 = 64;

/// What a class driver's VMO handle may do: be read, written and mapped by
/// it, and no more.
const VMO_RIGHTS: Rights =
    Rights(Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0 | Rights::TRANSFER.0);

/// A SEND being moved from its slot to the controller.
#[derive(Clone, Copy)]
struct Sending {
    tag: u32,
    endpoint: u8,
    slot: u8,
    len: usize,
    /// How much of it is queued.
    done: usize,
}

/// One served interface.
struct Served {
    channel: Channel<Kernel>,
    vmo: Vmo<Kernel>,
    /// The IN endpoints the driver opened.
    ins: [Option<u8>; MAX_ENDPOINTS],
    /// The OUT endpoints it opened, and whether each owes an EMPTY.
    outs: [Option<(u8, bool)>; MAX_ENDPOINTS],
    /// Each IN slot's endpoint while the driver holds it.
    held: [Option<u8>; IN_SLOTS],
    /// SENDs in order.
    sending: [Option<Sending>; OUT_SLOTS],
}

/// Every served interface.
pub(crate) struct Exports {
    served: [Option<Served>; MAX_EXPORTS],
}

impl Exports {
    pub(crate) const fn new() -> Self {
        Exports {
            served: [const { None }; MAX_EXPORTS],
        }
    }

    /// Attach exported interface `index` to devmgr: a new channel and VMO,
    /// HELLO waiting in it, the driver's end in ATTACH.
    pub(crate) fn attach<H: Host>(
        &mut self,
        bus: &Bus<H>,
        index: usize,
        boot: &Channel<Kernel>,
        port: &Port<Kernel>,
    ) {
        let Some(export) = bus.export(index).copied() else {
            return;
        };
        let made = (|| -> Result<Served, Error> {
            let (near, far) = channel::create(Kernel)?;
            let vmo = vmo::create(Kernel, VMO_BYTES)?;
            let shared = vmo.as_owned().duplicate(Requested::Exactly(VMO_RIGHTS))?;
            let hello = Message::Hello {
                key: export.key,
                identity: export.identity,
                count: export.count,
                endpoints: export.endpoints,
            };
            near.write_with(hello.encode().as_bytes(), [shared])
                .map_err(|(error, _)| error)?;
            let attach = Message::Attach {
                key: export.key,
                identity: export.identity,
            };
            boot.write_with(attach.encode().as_bytes(), [far.into_owned()])
                .map_err(|(error, _)| error)?;
            near.wait_async(
                port,
                Signals::READABLE | Signals::PEER_CLOSED,
                KEY_FIRST_EXPORT + index as u64,
            )?;
            Ok(Served {
                channel: near,
                vmo,
                ins: [None; MAX_ENDPOINTS],
                outs: [None; MAX_ENDPOINTS],
                held: [None; IN_SLOTS],
                sending: [None; OUT_SLOTS],
            })
        })();
        let identity = export.identity;
        match made {
            Ok(served) => {
                ferrix_rt::info!(
                    "usbhid: interface {} of {:04x}:{:04x} exported for its class driver (key {:#x})",
                    identity.interface,
                    identity.vendor,
                    identity.product,
                    export.key
                );
                if let Some(slot) = self.served.get_mut(index) {
                    *slot = Some(served);
                }
            }
            Err(error) => ferrix_rt::warn!(
                "usbhid: interface {} of {:04x}:{:04x} could not be exported: {error:?}",
                identity.interface,
                identity.vendor,
                identity.product
            ),
        }
    }

    /// The device went: its channel closes, which the driver hears.
    pub(crate) fn detach(&mut self, index: usize) {
        if let Some(slot) = self.served.get_mut(index) {
            *slot = None;
        }
    }

    /// Take what the driver of export `index` asked. Its end closed: what
    /// it opened is closed, and the interface attached again.
    pub(crate) fn take<H: Host>(
        &mut self,
        bus: &mut Bus<H>,
        index: usize,
        boot: &Channel<Kernel>,
        port: &Port<Kernel>,
    ) {
        let Some(Some(served)) = self.served.get_mut(index) else {
            return;
        };
        let mut gone = false;
        loop {
            let mut bytes = [0_u8; MAX_BYTES];
            let mut handles = [Handle::INVALID; 1];
            match served.channel.read(&mut bytes, &mut handles) {
                Ok(received) => {
                    for handle in handles.iter().take(received.handles) {
                        drop(OwnedHandle::from_raw(Kernel, *handle));
                    }
                    match Message::decode(bytes.get(..received.bytes).unwrap_or_default()) {
                        Ok(message) => answer(served, bus, index, message),
                        Err(_) => {
                            ferrix_rt::warn!(
                                "usbhid: export {index}'s driver sent what is no message; it is cut off"
                            );
                            gone = true;
                            break;
                        }
                    }
                }
                Err(ReadError::Failed(Error::ShouldWait)) => break,
                Err(_) => {
                    gone = true;
                    break;
                }
            }
        }
        if !gone
            && served
                .channel
                .wait_async(
                    port,
                    Signals::READABLE | Signals::PEER_CLOSED,
                    KEY_FIRST_EXPORT + index as u64,
                )
                .is_ok()
        {
            return;
        }
        // The driver went.
        if let Some(slot) = self.served.get_mut(index) {
            *slot = None;
        }
        bus.export_release(index);
        ferrix_rt::info!("usbhid: export {index}'s class driver went; attached again");
        self.attach(bus, index, boot, port);
    }

    /// Hand every driver what came for it: IN chunks into free slots, SENDs
    /// moved on, EMPTY and LOST. Whether anything moved.
    pub(crate) fn pump<H: Host>(&mut self, bus: &mut Bus<H>) -> bool {
        let mut moved = false;
        for (index, slot) in self.served.iter_mut().enumerate() {
            let Some(served) = slot else {
                continue;
            };
            moved |= pump_one(served, bus, index);
        }
        moved
    }
}

fn status(error: ExportError) -> Status {
    match error {
        ExportError::Gone => Status::Gone,
        ExportError::Refused => Status::Refused,
        ExportError::Busy => Status::Busy,
        ExportError::Transfer(error) => match error {
            ferrix_usb_host::host::Error::Stall => Status::Stall,
            ferrix_usb_host::host::Error::Timeout => Status::Timeout,
            _ => Status::Failed,
        },
    }
}

fn reply(served: &Served, message: &Message) {
    let _ = served.channel.write(message.encode().as_bytes());
}

fn answer<H: Host>(served: &mut Served, bus: &mut Bus<H>, index: usize, message: Message) {
    match message {
        Message::Control { tag, setup } => {
            let length = usize::from(u16::from_le_bytes([setup[6], setup[7]]));
            let mut data = [0_u8; CONTROL_BYTES];
            let outcome = data
                .get_mut(..length)
                .ok_or(ExportError::Refused)
                .and_then(|stage| {
                    if setup[0] & 0x80 == 0 && length > 0 {
                        served
                            .vmo
                            .read(stage, CONTROL_AT as u64)
                            .map_err(|_| ExportError::Refused)?;
                    }
                    bus.export_control(index, setup, stage)
                });
            let message = match outcome {
                Ok(moved) => {
                    if setup[0] & 0x80 != 0 && moved > 0 {
                        let _ = served
                            .vmo
                            .write(data.get(..moved).unwrap_or_default(), CONTROL_AT as u64);
                    }
                    Message::Done {
                        tag,
                        status: Status::Ok,
                        value: u32::try_from(moved).unwrap_or(0),
                    }
                }
                Err(error) => Message::Done {
                    tag,
                    status: status(error),
                    value: 0,
                },
            };
            reply(served, &message);
        }
        Message::Open { tag, endpoint } => {
            let outcome = bus.export_open(index, endpoint);
            if outcome.is_ok() {
                if endpoint & 0x80 != 0 {
                    if let Some(slot) = served.ins.iter_mut().find(|slot| slot.is_none()) {
                        *slot = Some(endpoint);
                    }
                } else if let Some(slot) = served.outs.iter_mut().find(|slot| slot.is_none()) {
                    *slot = Some((endpoint, false));
                }
            }
            reply(
                served,
                &Message::Done {
                    tag,
                    status: outcome.map_or_else(status, |()| Status::Ok),
                    value: 0,
                },
            );
        }
        Message::Send {
            tag,
            endpoint,
            slot,
            len,
        } => {
            let open = served
                .outs
                .iter()
                .flatten()
                .any(|(out, _)| *out == endpoint);
            let in_use = served
                .sending
                .iter()
                .flatten()
                .any(|sending| sending.slot == slot);
            let free = served.sending.iter_mut().find(|entry| entry.is_none());
            match free {
                Some(entry) if open && !in_use => {
                    *entry = Some(Sending {
                        tag,
                        endpoint,
                        slot,
                        len: usize::from(len),
                        done: 0,
                    });
                }
                _ => reply(
                    served,
                    &Message::Sent {
                        tag,
                        status: if open { Status::Busy } else { Status::Refused },
                    },
                ),
            }
        }
        Message::Free { slot } => {
            if let Some(held) = served.held.get_mut(usize::from(slot)) {
                *held = None;
            }
        }
        // What only the host sends.
        _ => {}
    }
}

/// Queue as much more of `sending` as its stream has room for: how much.
fn move_on<H: Host>(
    vmo: &Vmo<Kernel>,
    bus: &mut Bus<H>,
    index: usize,
    sending: &Sending,
) -> Result<usize, Status> {
    let room = bus.export_room(index, sending.endpoint);
    if room == 0 || sending.done >= sending.len {
        return Ok(0);
    }
    let take = room.min(sending.len - sending.done);
    let mut bytes = [0_u8; OUT_SLOT_BYTES];
    let offset = OUT_AT + usize::from(sending.slot) * OUT_SLOT_BYTES + sending.done;
    let piece = bytes.get_mut(..take).ok_or(Status::Invalid)?;
    vmo.read(piece, offset as u64)
        .map_err(|_| Status::Invalid)?;
    bus.export_send(index, sending.endpoint, piece)
        .map_err(status)
}

fn pump_one<H: Host>(served: &mut Served, bus: &mut Bus<H>, index: usize) -> bool {
    let mut moved = false;
    // SENDs strictly in order: the oldest is queued as far as its stream
    // has room, and the next starts only once it is whole, so two SENDs'
    // bytes never interleave.
    while let Some(Some(mut sending)) = served.sending.first().copied() {
        let mut failed = None;
        match move_on(&served.vmo, bus, index, &sending) {
            Ok(queued) => {
                sending.done += queued;
                moved |= queued > 0;
            }
            Err(why) => failed = Some(why),
        }
        let finished = failed.is_some() || sending.done >= sending.len;
        if !finished {
            if let Some(entry) = served.sending.first_mut() {
                *entry = Some(sending);
            }
            break;
        }
        reply(
            served,
            &Message::Sent {
                tag: sending.tag,
                status: failed.unwrap_or(Status::Ok),
            },
        );
        if let Some(out) = served
            .outs
            .iter_mut()
            .flatten()
            .find(|(out, _)| *out == sending.endpoint)
        {
            out.1 = true;
        }
        served.sending.rotate_left(1);
        if let Some(last) = served.sending.last_mut() {
            *last = None;
        }
        moved = true;
    }
    // EMPTY once everything sent was taken, and LOST for what a stall cost.
    for (endpoint, owed) in served.outs.iter_mut().flatten() {
        let lost = bus.export_take_lost(index, *endpoint);
        if lost > 0 {
            let _ = served.channel.write(
                Message::Lost {
                    endpoint: *endpoint,
                    bytes: u32::try_from(lost).unwrap_or(u32::MAX),
                }
                .encode()
                .as_bytes(),
            );
        }
        if *owed && bus.export_in_flight(index, *endpoint) == 0 {
            *owed = false;
            let _ = served.channel.write(
                Message::Empty {
                    endpoint: *endpoint,
                }
                .encode()
                .as_bytes(),
            );
        }
    }
    // IN chunks into every free slot.
    let ins = served.ins;
    for endpoint in ins.iter().flatten().copied() {
        while let Some(slot) = served.held.iter().position(Option::is_none) {
            let Some(chunk) = bus.export_take_chunk(index, endpoint) else {
                break;
            };
            let len = chunk.len.min(IN_SLOT_BYTES);
            let offset = IN_AT + slot * IN_SLOT_BYTES;
            if served
                .vmo
                .write(
                    chunk.as_slice().get(..len).unwrap_or_default(),
                    offset as u64,
                )
                .is_err()
            {
                break;
            }
            if let Some(held) = served.held.get_mut(slot) {
                *held = Some(endpoint);
            }
            let _ = served.channel.write(
                Message::In {
                    endpoint,
                    slot: u8::try_from(slot).unwrap_or(0),
                    len: u16::try_from(len).unwrap_or(0),
                }
                .encode()
                .as_bytes(),
            );
            moved = true;
        }
    }
    moved
}
