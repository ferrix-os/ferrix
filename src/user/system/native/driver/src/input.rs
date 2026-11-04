//! The input subsystem: `inputctl`, spoken to the kernel's input core for any
//! driver that implements [`Device`] (`docs/INPUT.md` §3.2).
//!
//! The run, whatever the device:
//!
//! 1. START on the bootstrap channel; the driver binds and probes.
//! 2. HELLO, with a port the core holds, and the core's answer: READY starts
//!    the device, anything else ends the run.
//! 3. One port carries every event: the device's interrupt and the control
//!    channel becoming readable. Every interrupt hands the core whatever
//!    messages the device made.
//! 4. STOP, or the core closing its end, ends it: the device is reset, and
//!    STOPPED answers a STOP only if the reset finished.
//!
//! The exit status is the [`Step`] that failed, 0 a clean stop.

use ferrix_inputctl::message::{Events, Hello, MAX_BYTES, Message, PORT_RIGHTS};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{PACKET_INTERRUPT, PACKET_SIGNAL};
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::{Bootstrap, Kernel};

use crate::start::{self, Bind, Started};
use crate::{Probe, Step, Stopped, Stuck};

/// Port keys.
const KEY_INTERRUPT: u64 = 1;
const KEY_CONTROL: u64 = 2;

/// What the core said, as far as the run is concerned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    /// READY: the device is started, and published as input node `node`.
    Started {
        /// The node's number.
        node: u32,
    },
    /// STOP: answer STOPPED once the device is reset.
    Stop,
    /// The core refused this driver.
    Refused,
    /// Nothing the run acts on.
    Nothing,
}

/// The device broke the protocol, or the core's message made no sense to it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fault;

/// An input device, as the input core needs it.
pub trait Device: Probe {
    /// HELLO for this device at START's `location`.
    fn hello(&mut self, location: u32) -> Hello;

    /// Act on a message from the core.
    ///
    /// # Errors
    ///
    /// [`Fault`] if the device broke the protocol doing so.
    fn control(&mut self, message: &Message) -> Result<Answer, Fault>;

    /// The device interrupted: drain it. `Ok(true)` when there is more to
    /// drain once the events made so far are taken.
    ///
    /// # Errors
    ///
    /// [`Fault`] if the device broke the protocol.
    fn interrupt(&mut self) -> Result<bool, Fault>;

    /// The next batch of events for the core, if any.
    fn events(&mut self) -> Option<Events>;

    /// Reset the device, and prove it, freeing its memory; or say it would
    /// not reset, keeping its memory pinned.
    ///
    /// # Errors
    ///
    /// [`Stuck`] when the device did not reset.
    fn stop(self) -> Result<Stopped, Stuck>;
}

/// Run input driver `D` on the device START names; the exit status.
pub fn run<D: Device>(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        return Step::Start.status();
    };
    match serve_device::<D>(&boot) {
        Ok(()) => 0,
        Err(step) => step.status(),
    }
}

fn serve_device<D: Device>(boot: &Channel<Kernel>) -> Result<(), Step> {
    let Started {
        start,
        device,
        control,
    } = start::read(boot)?;
    let port = port::create(Kernel).map_err(|_| Step::Events)?;
    let bound = D::Device::bind(device, &start, &port, KEY_INTERRUPT)?;
    let mut driver = D::probe(bound)?;

    if let Err(step) = introduce(&mut driver, &port, &control, start.location) {
        // Nothing has been posted yet; the device goes back to reset, unless
        // it will not, when its memory stays.
        return match driver.stop() {
            Ok(_) => Err(step),
            Err(Stuck) => Err(Step::Wedged),
        };
    }

    let ended = serve(&mut driver, &port, &control);
    match driver.stop() {
        Ok(_) => {
            if matches!(ended, Ok(true)) {
                let _ = control.write(Message::Stopped.encode().as_bytes());
            }
            ended.map(drop)
        }
        Err(Stuck) => Err(Step::Wedged),
    }
}

/// Send HELLO and wait for the core's answer.
///
/// The port goes with HELLO: the core holds it and the driver keeps nothing
/// of it, so a core that goes away closes it and this process hears.
fn introduce<D: Device>(
    driver: &mut D,
    port: &Port<Kernel>,
    control: &Channel<Kernel>,
    location: u32,
) -> Result<u32, Step> {
    let hello = driver.hello(location);
    let port_share = port
        .as_owned()
        .duplicate(Requested::Exactly(PORT_RIGHTS))
        .map_err(|_| Step::Hello)?;
    control
        .write_with(Message::Hello(hello).encode().as_bytes(), [port_share])
        .map_err(|_| Step::Hello)?;
    let node = ready(driver, control)?;
    control
        .wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_CONTROL)
        .map_err(|_| Step::Events)?;
    Ok(node)
}

/// Take the core's answer to HELLO: READY starts the device, and anything
/// else ends the driver.
fn ready<D: Device>(driver: &mut D, control: &Channel<Kernel>) -> Result<u32, Step> {
    let _ = control
        .wait_one(Signals::READABLE, Deadline::Never)
        .map_err(|_| Step::Hello)?;
    let mut bytes = [0_u8; MAX_BYTES];
    let mut handles = [Handle::INVALID; 1];
    let received = control
        .read(&mut bytes, &mut handles)
        .map_err(|_| Step::Hello)?;
    for handle in handles.iter().take(received.handles) {
        // The core's port is for a later iteration -- the status queue the
        // LED row needs -- and is closed here.
        drop(OwnedHandle::from_raw(Kernel, *handle));
    }
    let decoded = Message::decode(bytes.get(..received.bytes).unwrap_or_default())
        .map_err(|_| Step::Control)?;
    // The driver's answer to READY is what starts the device, so it delivers
    // nothing until the core has judged it.
    match driver.control(&decoded) {
        Ok(Answer::Started { node }) => Ok(node),
        Ok(Answer::Refused | Answer::Stop | Answer::Nothing) => Err(Step::Control),
        Err(Fault) => Err(Step::Faulted),
    }
}

/// Serve the device until the core stops it or goes away.
///
/// `Ok(true)` when the core asked for a stop, which is answered with STOPPED.
fn serve<D: Device>(
    driver: &mut D,
    port: &Port<Kernel>,
    control: &Channel<Kernel>,
) -> Result<bool, Step> {
    loop {
        let packet = port.wait(Deadline::Never).map_err(|_| Step::Events)?;
        match (packet.kind, packet.key) {
            (PACKET_INTERRUPT, KEY_INTERRUPT) => forward(driver, control)?,
            (PACKET_SIGNAL, KEY_CONTROL) => {
                // A one-shot wait: asked for again after every packet.
                if let Some(stop) = take_control(driver, control)? {
                    return Ok(stop);
                }
                control
                    .wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_CONTROL)
                    .map_err(|_| Step::Events)?;
            }
            // A packet for something this loop did not ask for: waited for
            // again, not acted on.
            _ => {}
        }
    }
}

/// Drain the device and hand the core whatever messages the batch made.
fn forward<D: Device>(driver: &mut D, control: &Channel<Kernel>) -> Result<(), Step> {
    loop {
        let more = driver.interrupt().map_err(|Fault| Step::Faulted)?;
        while let Some(events) = driver.events() {
            let encoded = Message::Events(events).encode();
            control
                .write(encoded.as_bytes())
                .map_err(|_| Step::Control)?;
        }
        // More means the batch was full and the device still holds
        // completions: taking messages made room, so go again.
        if !more {
            return Ok(());
        }
    }
}

/// Read what the core said, if anything.
///
/// `Some(true)` for a STOP, `Some(false)` for the core going away or
/// refusing, and `None` when there was nothing to read.
fn take_control<D: Device>(
    driver: &mut D,
    control: &Channel<Kernel>,
) -> Result<Option<bool>, Step> {
    let mut bytes = [0_u8; MAX_BYTES];
    let mut handles = [Handle::INVALID; 1];
    let received = match control.read(&mut bytes, &mut handles) {
        Ok(received) => received,
        // The core let go of its end: the run is over, and nothing is
        // answered.
        Err(ReadError::Failed(Error::PeerClosed)) => return Ok(Some(false)),
        // Readable without a message: another read took it, or the signal
        // was for the close that has not landed yet.
        Err(ReadError::Failed(Error::ShouldWait)) => return Ok(None),
        Err(_) => return Err(Step::Control),
    };
    for handle in handles.iter().take(received.handles) {
        drop(OwnedHandle::from_raw(Kernel, *handle));
    }
    let decoded = Message::decode(bytes.get(..received.bytes).unwrap_or_default())
        .map_err(|_| Step::Control)?;
    match driver.control(&decoded) {
        Ok(Answer::Stop) => Ok(Some(true)),
        Ok(Answer::Refused) => Ok(Some(false)),
        Ok(Answer::Started { .. } | Answer::Nothing) => Ok(None),
        Err(Fault) => Err(Step::Faulted),
    }
}
