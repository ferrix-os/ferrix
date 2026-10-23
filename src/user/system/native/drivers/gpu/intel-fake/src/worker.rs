//! `intel-fake-gt`: the fake GT as the keeper's worker, for
//! `cargo xtask test-restart --boot intel` alone (`docs/INTEL-GPU.md`
//! §12.8), under the name the keeper starts, `intel-gt`. Never in a
//! product image: it drives nothing, and every batch completes at once.
//!
//! It reads the keeper's HELLO, says its facts, sends READY, and then runs
//! each batch of the keeper's calls on the fake through
//! `ferrix-intel-xelp`'s [`Worker`], answering with what finished. A bind's
//! address VMO is closed once the call is done: the fake binds nothing.
//! It holds its bootstrap channel and nothing else.

#![no_std]
#![no_main]

mod fake;

use core::fmt::{self, Write as _};

use ferrix_intel_xelp::remote::Worker;
use ferrix_intel_xelp::remote::wire::{
    BATCH_HANDLES, BATCH_RECORDS, Batch, Call, Event, RECORD_BYTES, records,
};
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::port;
use ferrix_rt::{Bootstrap, Kernel};

// The front end's chardev and START types are `intel-fake`'s, beside it.
use ferrix_blkring as _;
use ferrix_chardevctl as _;

ferrix_rt::entry!(main);

/// The port key of the bootstrap channel.
const KEY_KEEPER: u64 = 1;

/// A bind's memory, as the worker gets it: the address VMO, closed when
/// the call is done.
struct Lent {
    _addresses: OwnedHandle<Kernel>,
}

/// Where a run stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum Step {
    /// No bootstrap channel, or no HELLO first.
    Hello = 1,
    /// The port could not be made, or failed.
    Port = 3,
    /// The keeper sent a record nobody writes.
    Record = 5,
    /// The keeper's end closed: it is gone, and so is this.
    Keeper = 6,
}

/// A line on standard error.
fn say(arguments: fmt::Arguments<'_>) {
    struct Line {
        bytes: [u8; 256],
        len: usize,
    }
    impl fmt::Write for Line {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            for &byte in text.as_bytes() {
                if let Some(slot) = self.bytes.get_mut(self.len) {
                    *slot = byte;
                    self.len += 1;
                }
            }
            Ok(())
        }
    }
    let mut line = Line {
        bytes: [0; 256],
        len: 0,
    };
    let _ = line.write_fmt(arguments);
    let _ = line.write_str("\n");
    let _ = ferrix_rt::linux::write(2, line.bytes.get(..line.len).unwrap_or_default());
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(keeper) = bootstrap else {
        return Step::Hello as i32;
    };
    match run(&keeper) {
        Ok(never) => match never {},
        Err(step) => {
            if !matches!(step, Step::Keeper) {
                say(format_args!("intel-fake: the worker stopped at {step:?}"));
            }
            step as i32
        }
    }
}

fn now() -> u64 {
    ferrix_rt::linux::monotonic_nanos().unwrap_or(0)
}

/// Events on their way to the keeper.
struct Out<'a> {
    keeper: &'a Channel<Kernel>,
    batch: Batch,
}

impl Out<'_> {
    fn push(&mut self, event: &Event) {
        if !self.batch.fits(false) {
            self.flush();
        }
        let _ = self.batch.push(&event.encode(), false);
    }

    fn flush(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        for _ in 0..2 {
            match self.keeper.write(self.batch.as_bytes()) {
                Err(Error::ShouldWait) => {
                    let until = now().saturating_add(1_000_000_000);
                    let _ = self.keeper.wait_one(
                        Signals::WRITABLE | Signals::PEER_CLOSED,
                        Deadline::At(until),
                    );
                }
                _ => break,
            }
        }
        self.batch.clear();
    }
}

fn run(keeper: &Channel<Kernel>) -> Result<core::convert::Infallible, Step> {
    let _ = keeper
        .wait_one(Signals::READABLE, Deadline::Never)
        .map_err(|_| Step::Hello)?;
    let mut bytes = [0_u8; RECORD_BYTES * BATCH_RECORDS];
    let mut handles = [Handle::INVALID; BATCH_HANDLES];
    let received = keeper
        .read(&mut bytes, &mut handles)
        .map_err(|_| Step::Hello)?;
    let first = records(bytes.get(..received.bytes).unwrap_or_default()).next();
    let Some(Ok(Call::Hello {
        generation,
        location,
        device_id,
        revision,
        system_memory_bytes,
    })) = first.map(Call::decode)
    else {
        return Err(Step::Hello);
    };
    let facts = fake::facts(device_id, revision, system_memory_bytes);
    say(format_args!(
        "intel-fake: worker {generation}: {location:#010x} 8086:{device_id:04x} rev {revision}: facts {} EUs, DSS {:#x}, CS {} Hz; {}",
        facts.sseu.eu_total,
        facts.sseu.subslice_mask,
        facts.cs_timestamp_hz,
        fake::MARKER
    ));
    let mut worker = Worker::new(fake::Fake::<Lent>::new(facts, |_| Err(Errno::ENOMEM)));
    let mut out = Out {
        keeper,
        batch: Batch::default(),
    };
    out.push(&worker.ready(true, now()));
    out.flush();
    let port = port::create(Kernel).map_err(|_| Step::Port)?;
    let mut armed = false;
    loop {
        if !armed {
            keeper
                .wait_async(&port, Signals::READABLE | Signals::PEER_CLOSED, KEY_KEEPER)
                .map_err(|_| Step::Port)?;
            armed = true;
        }
        let deadline = worker.next_deadline().map_or(Deadline::Never, Deadline::At);
        match port.wait(deadline) {
            Ok(_) => {
                armed = false;
                drain(keeper, &mut worker, &mut out)?;
            }
            Err(Error::TimedOut) => {}
            Err(_) => return Err(Step::Port),
        }
        worker.service(now(), false, &mut |event| out.push(&event));
        out.flush();
    }
}

/// Every message the keeper queued, each record run in order, a bind with
/// the next handle beside it.
fn drain(
    keeper: &Channel<Kernel>,
    worker: &mut Worker<fake::Fake<Lent>>,
    out: &mut Out<'_>,
) -> Result<(), Step> {
    let mut bytes = [0_u8; RECORD_BYTES * BATCH_RECORDS];
    let mut handles = [Handle::INVALID; BATCH_HANDLES];
    loop {
        let received = match keeper.read(&mut bytes, &mut handles) {
            Ok(received) => received,
            Err(ReadError::Failed(Error::ShouldWait)) => return Ok(()),
            Err(_) => return Err(Step::Keeper),
        };
        let mut lent = handles
            .iter()
            .take(received.handles)
            .map(|handle| OwnedHandle::from_raw(Kernel, *handle));
        for record in records(bytes.get(..received.bytes).unwrap_or_default()) {
            let call = Call::decode(record).map_err(|_| Step::Record)?;
            let memory = if call.carries_handle() {
                lent.next().map(|addresses| Lent {
                    _addresses: addresses,
                })
            } else {
                None
            };
            worker.call(&call, memory.as_ref(), now(), &mut |event| out.push(&event));
        }
        // Any handle no record claimed is closed here.
        lent.for_each(drop);
    }
}
