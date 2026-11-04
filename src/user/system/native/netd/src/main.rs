//! netd, the network switch (`docs/MICROKERNEL.md` §2.3, `docs/microkernel/M8.md` §2):
//! the ring's kernel side toward the wire's driver, and the ring's driver side toward
//! each Linux partition's core, which sees a NIC and changes nothing. Frames go by
//! `ferrix_netswitch`.
//!
//! One thread. All of it is bounded per turn: [`BUDGET`] frames from each side, so a
//! partition flooding the switch costs another its share and no more. It holds no
//! device handle and speaks to no driver but on the ring. Both rings' memory can change
//! under it at any moment: a frame is copied here before anything reads it.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

use ferrix_blkclient::{Mapped, Shared, held_mapping};
use ferrix_devmgr_proto::{CORE_BYTES, CoreOpen, CoreOpened, CorePublished};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::signals::Signals;
use ferrix_netring::control::{
    CONTROL_RIGHTS, HELLO_RIGHTS, Hello, Interface, MAX_MESSAGE, Message, PORT_RIGHTS, Refusal,
    VMO_RIGHTS,
};
use ferrix_netring::driver::DriverSide;
use ferrix_netring::kernel::{Completed, KernelSide};
use ferrix_netring::layout::{Op, Status, Submission};
use ferrix_netring::{RingMemory, Wait};
use ferrix_netswitch::{Route, Switch};
use ferrix_rt::native::channel::{self, Channel};
use ferrix_rt::native::handle::{Deadline, Object as _, OwnedHandle};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::space::Space;
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::{Bootstrap, Kernel};

ferrix_rt::entry!(main);

/// Frames from each side in one turn.
const BUDGET: usize = 32;
/// How long a partition has to answer a `CORE_OPEN` or a HELLO.
const PARTITION_PATIENCE_NANOS: u64 = 10_000_000_000;
/// How long a driver has between its `CORE_OPEN` and its HELLO.
const HELLO_PATIENCE_NANOS: u64 = 10_000_000_000;
/// Keys on the one port, past the rings' two bells.
/// The internal link's interface in each partition.
const INTERNAL_NAME: &[u8] = b"int0";
const KEY_CORES: u64 = 16;
const KEY_PENDING: u64 = 17;
const KEY_WIRE_CONTROL: u64 = 18;
const KEY_PARTITION_CONTROL: u64 = 32;

/// A bump over mapped VMOs: this program allocates at its start and at a driver's, a
/// fixed amount each, and frees nothing it would use again but through its reuse below.
struct Bump(UnsafeCell<(usize, usize)>);

// SAFETY: one thread.
unsafe impl Sync for Bump {}

#[global_allocator]
static HEAP: Bump = Bump(UnsafeCell::new((0, 0)));

// SAFETY: a block is live and unshared for ever: dealloc does nothing, and the bytes
// come from a mapping this keeps.
unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: one thread; no other reference lives now.
        let state = unsafe { &mut *self.0.get() };
        let size = layout.size().max(1);
        let mut start = state.0.next_multiple_of(layout.align().max(1));
        if state.0 == 0 || start + size > state.1 {
            let bytes = (size.next_multiple_of(4096)).max(256 * 1024);
            let Ok(made) = vmo::create(Kernel, bytes) else {
                return core::ptr::null_mut();
            };
            let Ok(at) = made.map(None, bytes, Protection::ReadWrite, 0) else {
                return core::ptr::null_mut();
            };
            state.1 = at + bytes;
            start = at;
        }
        state.0 = start + size;
        start as *mut u8
    }

    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {}
}

/// A ring's memory, in the net ring's trait.
struct Memory(Shared);

impl RingMemory for Memory {
    fn read_u8(&self, offset: usize) -> u8 {
        ferrix_blkclient::RingMemory::read_u8(&self.0, offset)
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        ferrix_blkclient::RingMemory::write_u8(&mut self.0, offset, value);
    }

    fn barrier(&self) {
        ferrix_blkclient::RingMemory::barrier(&self.0);
    }

    fn read_u16(&self, offset: usize) -> u16 {
        ferrix_blkclient::RingMemory::read_u16(&self.0, offset)
    }

    fn read_u32(&self, offset: usize) -> u32 {
        ferrix_blkclient::RingMemory::read_u32(&self.0, offset)
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        ferrix_blkclient::RingMemory::write_u16(&mut self.0, offset, value);
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        ferrix_blkclient::RingMemory::write_u32(&mut self.0, offset, value);
    }
}

fn now() -> u64 {
    ferrix_rt::linux::monotonic_nanos().unwrap_or(0)
}

/// The wire's driver, this side of its ring.
struct Wire {
    control: Channel<Kernel>,
    side: KernelSide,
    ring: Memory,
    data: Shared,
    bell: Port<Kernel>,
    slot_bytes: usize,
    entries: u32,
    mapped: [Mapped<Kernel>; 2],
    location: u32,
}

/// A partition, this side of its ring.
struct Partition {
    control: Channel<Kernel>,
    side: DriverSide,
    ring: Memory,
    data: Shared,
    bell: Port<Kernel>,
    /// Receive slots its core has given, waiting for a frame.
    empty: Vec<u32>,
    slot_bytes: usize,
    /// Kept: the mappings are this ring's for the partition's life.
    _vmos: [Vmo<Kernel>; 2],
}

struct Netd {
    port: Port<Kernel>,
    /// `devmgr` asks here for the wire's driver's control channel.
    cores: Channel<Kernel>,
    /// The partitions' cores, to ask for their rings.
    partition_cores: Vec<Channel<Kernel>>,
    space: Space<Kernel>,
    pending: Option<(Channel<Kernel>, u32, u64)>,
    wire: Option<Wire>,
    partitions: Vec<Partition>,
    switch: Option<Switch>,
    /// One frame, on its way, in this side's memory.
    frame: Vec<u8>,
    /// The wire's first offer: what each partition is offered.
    interface: Option<Hello>,
}

impl Netd {
    fn serve_core(&mut self) {
        let mut bytes = [0_u8; CORE_BYTES];
        let mut handles = [Handle::INVALID; 1];
        let Ok(got) = self.cores.read(&mut bytes, &mut handles) else {
            return;
        };
        let opened = CoreOpen::decode(bytes.get(..got.bytes).unwrap_or(&[]))
            .filter(|open| got.handles == 0 && open.call as usize == nr::NET_RING_CREATE)
            .filter(|_| self.wire.is_none() && self.pending.is_none())
            .and_then(|open| {
                let (near, far) = channel::create(Kernel).ok()?;
                let far = far
                    .into_owned()
                    .replace(Requested::Exactly(CONTROL_RIGHTS))
                    .ok()?;
                Some((near, far, open.location))
            });
        let sent = match opened {
            Some((near, far, location)) => {
                let wake = Signals::READABLE | Signals::PEER_CLOSED;
                let _ = near.wait_async(&self.port, wake, KEY_PENDING);
                self.pending = Some((near, location, now().saturating_add(HELLO_PATIENCE_NANOS)));
                self.cores
                    .write_with(&CoreOpened { status: 0 }.encode(), [far])
                    .is_ok()
            }
            None => self
                .cores
                .write(&CoreOpened { status: 16 }.encode())
                .is_ok(),
        };
        if !sent {
            ferrix_rt::notice!("netd     devmgr's channel would not take an answer");
        }
    }

    /// A driver's HELLO, checked as the kernel's core checks it.
    fn take_up(&mut self) {
        let Some((control, location, _)) = self.pending.take() else {
            return;
        };
        match self.wire_from(control, location) {
            Ok(mac_of_wire) => {
                ferrix_rt::notice!("netd     the wire is up at {location:#x}");
                let _ = self.cores.write(&CorePublished { location }.encode());
                if self.switch.is_none() {
                    self.switch = Some(Switch::new(
                        mac_of_wire,
                        port_count(self.partition_cores.len()),
                    ));
                    self.open_partitions();
                }
                self.tell_link(true);
            }
            Err(why) => ferrix_rt::notice!("netd     {location:#x}: refused: {why:?}"),
        }
    }

    fn wire_from(&mut self, control: Channel<Kernel>, location: u32) -> Result<[u8; 6], Refusal> {
        let mut bytes = [0_u8; MAX_MESSAGE];
        let mut handles = [Handle::INVALID; 3];
        let refuse = |why: Refusal, control: &Channel<Kernel>| {
            let mut out = [0_u8; MAX_MESSAGE];
            if let Ok(length) = Message::Refused(why).encode(&mut out) {
                let _ = control.write(out.get(..length).unwrap_or(&[]));
            }
            why
        };
        let Ok(got) = control.read(&mut bytes, &mut handles) else {
            return Err(Refusal::Malformed);
        };
        let [ring, data, bell] = handles.map(|handle| OwnedHandle::from_raw(Kernel, handle));
        let hello: Hello = match Message::decode(bytes.get(..got.bytes).unwrap_or(&[])) {
            Ok(Message::Hello(hello)) if got.handles == HELLO_RIGHTS.len() => hello,
            _ => return Err(refuse(Refusal::Malformed, &control)),
        };
        if let Err(why) = hello.validate() {
            return Err(refuse(why, &control));
        }
        let exactly = |handle: OwnedHandle<Kernel>, rights| {
            handle
                .replace(Requested::Exactly(rights))
                .map_err(|_| Refusal::Malformed)
        };
        let ring = exactly(ring, VMO_RIGHTS).map_err(|why| refuse(why, &control))?;
        let data = exactly(data, VMO_RIGHTS).map_err(|why| refuse(why, &control))?;
        let bell = exactly(bell, PORT_RIGHTS).map_err(|why| refuse(why, &control))?;
        let (ring_map, ring) = held_mapping(Vmo::from_owned(ring))
            .ok_or_else(|| refuse(Refusal::RingTooSmall, &control))?;
        let (data_map, data) = held_mapping(Vmo::from_owned(data))
            .ok_or_else(|| refuse(Refusal::DataTooSmall, &control))?;
        let memory = Memory(ring);
        let side = KernelSide::attach(&memory, ring_map.bytes(), data_map.bytes())
            .map_err(|_| refuse(Refusal::RingTooSmall, &control))?;
        let given = self
            .port
            .as_owned()
            .duplicate(Requested::Exactly(PORT_RIGHTS))
            .map_err(|_| refuse(Refusal::Malformed, &control))?;
        let mut out = [0_u8; MAX_MESSAGE];
        let length = Message::Ready
            .encode(&mut out)
            .map_err(|_| Refusal::Malformed)?;
        control
            .write_with(out.get(..length).unwrap_or(&[]), [given])
            .map_err(|_| Refusal::Malformed)?;
        let _ = control.wait_async(
            &self.port,
            Signals::READABLE | Signals::PEER_CLOSED,
            KEY_WIRE_CONTROL,
        );
        self.frame = vec![0; hello.slot_bytes as usize];
        self.wire = Some(Wire {
            control,
            side,
            ring: memory,
            data,
            bell: Port::from_owned(bell),
            slot_bytes: hello.slot_bytes as usize,
            entries: hello.entries,
            mapped: [ring_map, data_map],
            location,
        });
        self.interface = Some(hello);
        Ok(hello.interface.mac)
    }
}

impl Netd {
    /// Each partition's ring, made as a driver makes one, opened as `devmgr` opens one.
    fn open_partitions(&mut self) {
        let Some(hello) = self.interface else {
            return;
        };
        let Some(switch) = self.switch else {
            return;
        };
        for port in 0..port_count(self.partition_cores.len()) {
            match self.open_partition(port, &hello, &switch) {
                Some(partition) => {
                    self.partitions.push(partition);
                    ferrix_rt::notice!("netd     port {port} is up");
                }
                None => ferrix_rt::notice!("netd     port {port} took no interface"),
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one ring's making, each step refusing in its place"
    )]
    fn open_partition(&self, index: usize, hello: &Hello, switch: &Switch) -> Option<Partition> {
        // Port 0 is the first partition's uplink, ports 1 up the internal link's, its
        // own first (`ferrix_netswitch`): a partition's cores by its index on that link.
        let cores = self.partition_cores.get(index.saturating_sub(1))?;
        let location = self.wire.as_ref()?.location | (index as u32) << 24;
        let open = CoreOpen {
            call: u32::try_from(nr::NET_RING_CREATE).ok()?,
            location,
        };
        cores.write(&open.encode()).ok()?;
        let until = Deadline::At(now().saturating_add(PARTITION_PATIENCE_NANOS));
        let _ = cores
            .wait_one(Signals::READABLE | Signals::PEER_CLOSED, until)
            .ok()?;
        let mut bytes = [0_u8; CORE_BYTES];
        let mut handles = [Handle::INVALID; 1];
        let got = cores.read(&mut bytes, &mut handles).ok()?;
        let opened = CoreOpened::decode(bytes.get(..got.bytes)?)?;
        if opened.status != 0 || got.handles != 1 {
            return None;
        }
        let control: Channel<Kernel> =
            Channel::from_owned(OwnedHandle::from_raw(Kernel, handles[0]));

        // This partition's memory: made here, charged here, bounded by the wire's own
        // ring: as many entries, as many bytes a slot.
        let slot_bytes = hello.slot_bytes as usize;
        let data_bytes = (slot_bytes * hello.entries as usize).next_multiple_of(4096);
        let ring_bytes = 64 * 1024_usize;
        let ring_vmo: Vmo<Kernel> = vmo::create(Kernel, ring_bytes).ok()?;
        let data_vmo: Vmo<Kernel> = vmo::create(Kernel, data_bytes).ok()?;
        let ring_at = ring_vmo
            .map(None, ring_bytes, Protection::ReadWrite, 0)
            .ok()?;
        let data_at = data_vmo
            .map(None, data_bytes, Protection::ReadWrite, 0)
            .ok()?;
        // SAFETY: mapped above for the partition's life, and this the only name here.
        let ring = unsafe { Shared::from_raw(ring_at, ring_bytes) };
        // SAFETY: as `ring`.
        let data = unsafe { Shared::from_raw(data_at, data_bytes) };
        let mut ring = Memory(ring);
        let side =
            DriverSide::create(&mut ring, ring_bytes, hello.entries, hello.slot_bytes).ok()?;
        let mac = switch.mac(index)?;
        let interface = Interface::new(
            if index == 0 {
                hello.interface.name()
            } else {
                INTERNAL_NAME
            },
            mac,
            hello.interface.mtu,
            hello.interface.flags,
        );
        let offer = Message::Hello(Hello {
            version: hello.version,
            entries: hello.entries,
            slot_bytes: hello.slot_bytes,
            interface,
        });
        let mut out = [0_u8; MAX_MESSAGE];
        let length = offer.encode(&mut out).ok()?;
        let share = |vmo: &Vmo<Kernel>| {
            vmo.as_owned()
                .duplicate(Requested::Exactly(VMO_RIGHTS))
                .ok()
        };
        let given = self
            .port
            .as_owned()
            .duplicate(Requested::Exactly(PORT_RIGHTS))
            .ok()?;
        control
            .write_with(
                out.get(..length)?,
                [share(&ring_vmo)?, share(&data_vmo)?, given],
            )
            .ok()?;
        let until = Deadline::At(now().saturating_add(PARTITION_PATIENCE_NANOS));
        let _ = control
            .wait_one(Signals::READABLE | Signals::PEER_CLOSED, until)
            .ok()?;
        let mut answer = [0_u8; MAX_MESSAGE];
        let mut ready = [Handle::INVALID; 1];
        let got = control.read(&mut answer, &mut ready).ok()?;
        if !matches!(
            Message::decode(answer.get(..got.bytes)?),
            Ok(Message::Ready)
        ) || got.handles != 1
        {
            return None;
        }
        let bell = Port::from_owned(OwnedHandle::from_raw(Kernel, ready[0]));
        let _ = control.wait_async(
            &self.port,
            Signals::READABLE | Signals::PEER_CLOSED,
            KEY_PARTITION_CONTROL + index as u64,
        );
        Some(Partition {
            control,
            side,
            ring,
            data,
            bell,
            empty: Vec::with_capacity(hello.entries as usize),
            slot_bytes,
            _vmos: [ring_vmo, data_vmo],
        })
    }

    /// Tell every partition the wire's state, as its driver would.
    fn tell_link(&self, up: bool) {
        let mut out = [0_u8; MAX_MESSAGE];
        let Ok(length) = Message::Link(up).encode(&mut out) else {
            return;
        };
        for partition in &self.partitions {
            let _ = partition.control.write(out.get(..length).unwrap_or(&[]));
        }
    }

    /// The wire's driver ended, or broke its ring: its memory goes and the link is down.
    /// Each stack sees a cable pulled; the next driver is a new HELLO.
    fn wire_ended(&mut self) {
        if let Some(wire) = self.wire.take() {
            for one in &wire.mapped {
                let _ = self.space.unmap(one.address(), one.bytes());
            }
            self.tell_link(false);
            ferrix_rt::notice!("netd     the wire's driver ended");
        }
    }

    /// `frame`, `length` bytes of it, to each partition `route` names that has room. Full
    /// is dropped: a wire may drop.
    fn deliver(&mut self, route: Route, length: usize) {
        let frame = self.frame.get(..length).unwrap_or(&[]);
        for (index, partition) in self.partitions.iter_mut().enumerate() {
            if route.ports & (1 << index) == 0 || length > partition.slot_bytes {
                continue;
            }
            let Some(slot) = partition.empty.pop() else {
                continue;
            };
            let at = slot as usize * partition.slot_bytes;
            if partition.data.copy_in(at, frame) {
                let _ =
                    partition
                        .side
                        .complete(&mut partition.ring, slot, length as u32, Status::Ok);
            }
        }
    }

    /// One turn of the wire: receives posted, completions drained, up to the budget.
    /// False when the ring broke.
    fn turn_wire(&mut self) -> bool {
        let mut done = [Completed {
            slot: 0,
            op: Op::Receive,
            length: 0,
            status: Status::Ok,
        }; BUDGET];
        let Some(wire) = self.wire.as_mut() else {
            return true;
        };
        let half = wire.entries / 2;
        while wire.side.outstanding() < half {
            let Some(slot) = wire.side.free_slot() else {
                break;
            };
            if wire
                .side
                .submit(&mut wire.ring, slot, Op::Receive, 0)
                .is_err()
            {
                break;
            }
        }
        let Ok(drained) = wire.side.drain(&mut wire.ring, &mut done) else {
            return false;
        };
        let slot_bytes = wire.slot_bytes;
        for one in done.iter().take(drained.taken) {
            if one.op != Op::Receive || one.status != Status::Ok {
                continue;
            }
            let length = (one.length as usize).min(slot_bytes);
            let Some(wire) = self.wire.as_ref() else {
                return true;
            };
            let Some(room) = self.frame.get_mut(..length) else {
                continue;
            };
            if !wire.data.copy_out(one.slot as usize * slot_bytes, room) {
                continue;
            }
            let Some(route) = self.switch.as_ref().map(|switch| switch.from_wire(room)) else {
                continue;
            };
            self.deliver(route, length);
        }
        true
    }

    /// One partition's turn, up to the budget.
    fn turn_partition(&mut self, index: usize) -> bool {
        let mut taken = [Submission {
            slot: 0,
            length: 0,
            op: Op::Receive,
        }; BUDGET];
        let Some(partition) = self.partitions.get_mut(index) else {
            return true;
        };
        let Ok(consumed) = partition.side.take(&mut partition.ring, &mut taken) else {
            return false;
        };
        for one in taken.iter().take(consumed.taken) {
            let Some(partition) = self.partitions.get_mut(index) else {
                return true;
            };
            if one.op == Op::Receive {
                partition.empty.push(one.slot);
                continue;
            }
            let slot_bytes = partition.slot_bytes;
            let length = (one.length as usize).min(slot_bytes);
            let Some(room) = self.frame.get_mut(..length) else {
                continue;
            };
            let copied = partition
                .data
                .copy_out(one.slot as usize * slot_bytes, room);
            // Sent, as far as the partition is told: a wire drops without saying.
            let _ =
                partition
                    .side
                    .complete(&mut partition.ring, one.slot, length as u32, Status::Ok);
            if !copied {
                continue;
            }
            let Some(route) = self
                .switch
                .as_ref()
                .map(|switch| switch.from_partition(index, room))
            else {
                continue;
            };
            if route.wire
                && let Some(wire) = self.wire.as_mut()
                && let Some(slot) = wire.side.free_slot()
                && wire.data.copy_in(
                    slot as usize * wire.slot_bytes,
                    self.frame.get(..length).unwrap_or(&[]),
                )
            {
                let _ = wire
                    .side
                    .submit(&mut wire.ring, slot, Op::Transmit, length as u32);
            }
            self.deliver(route, length);
        }
        true
    }

    fn publish_all(&mut self) {
        if let Some(wire) = self.wire.as_mut()
            && let Some(bell) = wire.side.publish(&mut wire.ring)
        {
            let _ = wire.bell.queue(bell.key(), bell.packet().data);
        }
        for partition in &mut self.partitions {
            if let Some(bell) = partition.side.publish(&mut partition.ring) {
                let _ = partition.bell.queue(bell.key(), bell.packet().data);
            }
        }
    }

    /// Whether anything is left to do before sleeping.
    fn must_go_on(&mut self) -> bool {
        let mut pending = false;
        if let Some(wire) = self.wire.as_mut()
            && matches!(
                wire.side.prepare_to_sleep(&mut wire.ring),
                Ok(Wait::Pending(_))
            )
        {
            pending = true;
        }
        for partition in &mut self.partitions {
            if matches!(
                partition.side.prepare_to_sleep(&mut partition.ring),
                Ok(Wait::Pending(_))
            ) {
                pending = true;
            }
        }
        pending
    }

    fn woke(&mut self) {
        if let Some(wire) = self.wire.as_mut() {
            wire.side.woke(&mut wire.ring);
        }
        for partition in &mut self.partitions {
            partition.side.woke(&mut partition.ring);
        }
    }

    fn serve_wire_control(&mut self) {
        let Some(wire) = self.wire.as_ref() else {
            return;
        };
        let mut bytes = [0_u8; MAX_MESSAGE];
        match wire.control.read(&mut bytes, &mut []) {
            Ok(got) => {
                if let Ok(Message::Link(up)) =
                    Message::decode(bytes.get(..got.bytes).unwrap_or(&[]))
                {
                    self.tell_link(up);
                }
                if let Some(wire) = self.wire.as_ref() {
                    let wake = Signals::READABLE | Signals::PEER_CLOSED;
                    let _ = wire.control.wait_async(&self.port, wake, KEY_WIRE_CONTROL);
                }
            }
            Err(_) => self.wire_ended(),
        }
    }

    fn run(&mut self) -> i32 {
        // Only `test-netd`'s build: the row's way of killing it, since nothing can
        // from outside.
        let dies_after: Option<u32> =
            option_env!("FERRIX_TEST_NETD_DIES_AFTER").and_then(|text| text.parse().ok());
        let mut turns = 0_u32;
        let wake = Signals::READABLE | Signals::PEER_CLOSED;
        if self.cores.wait_async(&self.port, wake, KEY_CORES).is_err() {
            return 5;
        }
        loop {
            turns = turns.saturating_add(1);
            if dies_after.is_some_and(|after| turns > after) {
                return 99;
            }
            let mut live = true;
            if self.wire.is_some() && !self.turn_wire() {
                self.wire_ended();
                live = false;
            }
            for index in 0..self.partitions.len() {
                if !self.turn_partition(index) {
                    ferrix_rt::notice!("netd     partition {index}'s ring broke");
                }
            }
            self.publish_all();
            if live && self.must_go_on() {
                continue;
            }
            let until = self
                .pending
                .as_ref()
                .map_or(Deadline::Never, |(_, _, until)| Deadline::At(*until));
            match self.port.wait(until) {
                Ok(packet) => match packet.key {
                    KEY_CORES => {
                        self.serve_core();
                        let _ = self.cores.wait_async(&self.port, wake, KEY_CORES);
                    }
                    KEY_PENDING => self.take_up(),
                    KEY_WIRE_CONTROL => self.serve_wire_control(),
                    _ => {}
                },
                Err(_) => {
                    if self
                        .pending
                        .as_ref()
                        .is_some_and(|(_, _, until)| now() > *until)
                    {
                        self.pending = None;
                        ferrix_rt::notice!("netd     a driver said no HELLO in time");
                    }
                }
            }
            self.woke();
        }
    }
}

/// One port alone with one partition, which sees exactly today's interface; with more, the
/// uplink and one internal port each.
const fn port_count(partitions: usize) -> usize {
    if partitions > 1 { partitions + 1 } else { 1 }
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(start): Option<Channel<Kernel>> = bootstrap else {
        return 1;
    };
    if start
        .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
        .is_err()
    {
        return 2;
    }
    // START: devmgr's channel, this process's space, its debug log, a partition's cores each.
    let mut bytes = [0_u8; 8];
    let mut handles = [Handle::INVALID; 4];
    let Ok(got) = start.read(&mut bytes, &mut handles) else {
        return 3;
    };
    if got.handles != 4 {
        return 4;
    }
    ferrix_rt::log::use_debug_log(handles[2]);
    let Ok(port) = port::create(Kernel) else {
        return 5;
    };
    // The first partition's cores, and from devmgr, who keeps them for every next `netd`,
    // the further ones' (ledger 6226 N.5).
    let first: Channel<Kernel> = Channel::from_owned(OwnedHandle::from_raw(Kernel, handles[3]));
    let mut partition_cores = Vec::with_capacity(ferrix_netswitch::MAX_PORTS);
    partition_cores.push(first);
    // All written before this process ran, so that an empty channel is the end of them.
    while partition_cores.len() < ferrix_netswitch::MAX_PORTS - 1 {
        let mut said = [0_u8; 8];
        let mut end = [Handle::INVALID; 1];
        match start.read(&mut said, &mut end) {
            Ok(got) if got.handles == 1 => {
                partition_cores.push(Channel::from_owned(OwnedHandle::from_raw(Kernel, end[0])));
            }
            _ => break,
        }
    }
    let mut netd = Netd {
        port,
        cores: Channel::from_owned(OwnedHandle::from_raw(Kernel, handles[0])),
        partition_cores,
        space: Space::from_owned(OwnedHandle::from_raw(Kernel, handles[1])),
        pending: None,
        wire: None,
        partitions: Vec::new(),
        switch: None,
        frame: Vec::new(),
        interface: None,
    };
    ferrix_rt::notice!("netd     up: {} partitions", netd.partition_cores.len());
    netd.run()
}
