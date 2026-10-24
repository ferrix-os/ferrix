//! The descriptor, identity and link logic on their own, and the driver
//! against `model`'s chip.

mod model;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::vec::Vec;

use model::{FakeClock, Lie, Mem, Model, PERMANENT, PHY_ID, Regs, Shared, phy_reg};

use crate::chip::{self, Chip};
use crate::desc::{self, Received};
use crate::regs::{self, phy};
use crate::{
    Driver, Error, Event, Failed, Frame, Link, MAX_ENTRIES, MIN_FRAME, MacSource, Options, PAGE,
    Parts, Speed, SubmitError, Teardown, Timeout,
};

type TestDriver = Driver<Regs, FakeClock, Mem, Mem>;
type TestParts = Parts<Regs, FakeClock, Mem, Mem>;

/// Small rings, so the tests wrap them.
fn small() -> Options {
    Options {
        transmit_entries: 8,
        receive_entries: 8,
        buffer_bytes: 2048,
        gigabit: true,
        seed: [0x12, 0x34, 0x56],
    }
}

/// The PHY status a 1000 full partner gives.
const GIGABIT: u8 = regs::STATUS_1000_FULL | regs::STATUS_FULL;

struct Rig {
    model: Shared,
    clock: Rc<Cell<u64>>,
}

impl Rig {
    fn new() -> Self {
        Rig {
            model: Rc::new(RefCell::new(Model::new())),
            clock: Rc::new(Cell::new(0)),
        }
    }

    /// Parts for `options`: the ring page above 4 GiB, the receive buffers
    /// on scattered pages, the transmit region after them.
    fn parts(&self, options: &Options) -> TestParts {
        let rx_pages =
            (usize::from(options.receive_entries) * options.buffer_bytes as usize).div_ceil(PAGE);
        let scattered = (0..rx_pages as u64)
            .map(|page| 0x80_0000 + page * 3 * PAGE as u64)
            .collect();
        Parts {
            regs: Regs(Rc::clone(&self.model)),
            clock: FakeClock(Rc::clone(&self.clock)),
            rings: Mem::new(&self.model, 0x1_2340_0000, 1),
            receive: Mem::at(&self.model, scattered),
            transmit: Mem::new(&self.model, 0x3_0000_0000, 8),
        }
    }

    fn init(&self, options: Options) -> TestDriver {
        match Driver::init(self.parts(&options), options) {
            Ok(driver) => driver,
            Err(failed) => panic!("bring-up failed: {}", failed.error),
        }
    }

    fn fail(&self, options: Options) -> Failed<Regs, FakeClock, Mem, Mem> {
        match Driver::init(self.parts(&options), options) {
            Ok(_) => panic!("bring-up worked"),
            Err(failed) => failed,
        }
    }

    /// A driver with the link up at 1000 full.
    fn up(&self) -> TestDriver {
        self.model.borrow_mut().partner = Some(GIGABIT);
        let mut driver = self.init(small());
        assert_eq!(
            driver.poll_link(),
            Ok(Some(Link::Up {
                speed: Speed::Thousand,
                full_duplex: true
            })),
            "the link comes up"
        );
        driver
    }

    fn phy(&self, address: u16) -> u16 {
        self.model
            .borrow()
            .phy_ocp
            .get(&address)
            .copied()
            .unwrap_or(0)
    }
}

/// A frame of `len` bytes whose content says `seed`.
fn frame(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|index| seed.wrapping_add((index % 253) as u8))
        .collect()
}

fn drain_all(driver: &mut TestDriver) -> Vec<Event> {
    let mut out = [Event::Sent { id: 0 }; 64];
    let drained = driver.drain(&mut out).expect("drain works");
    out[..drained.events].to_vec()
}

/// Write `bytes` into transmit slot `slot` and submit them as `id`.
fn send(driver: &mut TestDriver, slot: u64, bytes: &[u8], id: u64) -> Result<(), SubmitError> {
    let offset = slot * 2048;
    driver.parts.transmit.put(offset, bytes);
    driver.submit(&Frame {
        id,
        offset,
        len: bytes.len() as u32,
    })
}

// ----- descriptors, identity and link, on their own -----

#[test]
fn a_transmit_descriptor_is_one_whole_frame_owned_by_the_chip() {
    let words = desc::transmit(0x1234_5678_9ABC, 1514, false);
    assert_eq!(words.word0, desc::OWN | desc::FIRST | desc::LAST | 1514);
    assert_eq!(words.word1, 0, "no offloads asked for");
    assert_eq!(words.address, 0x1234_5678_9ABC);
    let last = desc::transmit(0x1000, 60, true);
    assert_ne!(last.word0 & desc::RING_END, 0, "the last one ends the ring");
}

#[test]
fn a_receive_descriptor_lends_its_whole_buffer() {
    let words = desc::receive(0x8000, 2048, false);
    assert_eq!(words.word0, desc::OWN | 2048);
    assert_ne!(desc::receive(0x8000, 2048, true).word0 & desc::RING_END, 0);
}

#[test]
fn a_written_back_length_is_believed_only_for_a_whole_good_frame_in_the_buffer() {
    let whole = desc::FIRST | desc::LAST;
    assert_eq!(desc::received(whole | 64, 2048), Received::Frame(60));
    assert_eq!(desc::received(whole | 2048, 2048), Received::Frame(2044));
    assert_eq!(desc::received(whole | 2049, 2048), Received::Dropped);
    assert_eq!(desc::received(whole | 17, 2048), Received::Dropped);
    assert_eq!(desc::received(whole | 3, 2048), Received::Dropped);
    assert_eq!(
        desc::received(whole | desc::RX_ERROR | 64, 2048),
        Received::Dropped
    );
    assert_eq!(desc::received(desc::FIRST | 64, 2048), Received::Dropped);
    assert_eq!(desc::received(desc::LAST | 64, 2048), Received::Dropped);
}

#[test]
fn only_the_8168h_family_is_driven() {
    assert_eq!(chip::identify(0x541 << 20), Ok((Chip::Rtl8168h, 0x541)));
    assert_eq!(
        chip::identify((0x541 << 20) | regs::TX_CONFIG_RUNNING),
        Ok((Chip::Rtl8168h, 0x541)),
        "the configuration bits are not the identity"
    );
    assert_eq!(chip::identify(0x6C0 << 20), Ok((Chip::Rtl8168m, 0x6C0)));
    assert_eq!(chip::identify(0x4C0 << 20), Err(Error::Unsupported(0x4C0)));
    assert_eq!(chip::identify(0x641 << 20), Err(Error::Unsupported(0x641)));
    assert_eq!(chip::identify(u32::MAX), Err(Error::Absent));
}

#[test]
fn the_phy_status_register_says_the_link() {
    assert_eq!(Link::from_status(0), Link::Down);
    assert_eq!(Link::from_status(GIGABIT), Link::Down, "no link bit");
    assert_eq!(
        Link::from_status(GIGABIT | regs::STATUS_LINK),
        Link::Up {
            speed: Speed::Thousand,
            full_duplex: true
        }
    );
    assert_eq!(
        Link::from_status(regs::STATUS_100 | regs::STATUS_LINK),
        Link::Up {
            speed: Speed::Hundred,
            full_duplex: false
        }
    );
    assert_eq!(
        Link::from_status(regs::STATUS_10 | regs::STATUS_FULL | regs::STATUS_LINK),
        Link::Up {
            speed: Speed::Ten,
            full_duplex: true
        }
    );
}

#[test]
fn a_phy_page_is_an_address_and_never_a_selection() {
    assert_eq!(chip::phy_address(0, phy::BMCR), Ok(0xA400));
    assert_eq!(chip::phy_address(0, phy::CTRL1000), Ok(0xA412));
    assert_eq!(chip::phy_address(0x0A43, 0x13), Ok(0xA436));
    assert_eq!(chip::phy_address(0x0A43, 0x14), Ok(0xA438));
    assert_eq!(chip::phy_address(0x0A5D, 0x10), Ok(0xA5D0));
    assert_eq!(chip::phy_address(0x0A43, 0x0F), Err(Error::BadOptions));
    assert_eq!(chip::phy_address(0, 32), Err(Error::BadOptions));
}

#[test]
fn the_poll_backs_off_while_idle_and_snaps_back_on_traffic() {
    let mut every = crate::POLL_NANOS;
    let mut seen = Vec::new();
    for _ in 0..6 {
        every = crate::next_poll_nanos(every, false);
        seen.push(every / 1_000_000);
    }
    assert_eq!(seen, [2, 4, 8, 8, 8, 8]);
    assert_eq!(crate::next_poll_nanos(every, true), crate::POLL_NANOS);
}

// ----- bring-up -----

#[test]
fn bring_up_takes_the_chip_from_firmware_resets_it_and_programs_it() {
    let rig = Rig::new();
    let driver = rig.init(small());
    let info = *driver.info();
    assert_eq!(info.chip, Chip::Rtl8168h);
    assert_eq!(info.xid, 0x541);
    assert_eq!(info.phy_id, PHY_ID);
    assert_eq!(info.mac, PERMANENT);
    assert_eq!(info.mac_source, MacSource::Permanent);
    let model = rig.model.borrow();
    assert!(model.resets >= 1, "the chip was reset");
    assert_eq!(model.locked_writes, 0, "nothing written under the lock");
    assert_eq!(
        model.u8(regs::CFG9346),
        regs::CFG_LOCK,
        "the lock is shut after"
    );
    let [a, b, c, d, e, f] = PERMANENT;
    assert_eq!(model.u32(regs::MAC0), u32::from_le_bytes([a, b, c, d]));
    assert_eq!(model.u32(regs::MAC4), u32::from(u16::from_le_bytes([e, f])));
    assert_eq!(
        model.u8(regs::CHIP_CMD),
        regs::CMD_TX_ENABLE | regs::CMD_RX_ENABLE
    );
    assert_eq!(
        model.u32(regs::RX_CONFIG),
        regs::RX_CONFIG_BASE
            | regs::RX_ACCEPT_BROADCAST
            | regs::RX_ACCEPT_MULTICAST
            | regs::RX_ACCEPT_MINE,
        "no promiscuity, no errored frames"
    );
    assert_eq!(model.u32(regs::MAR0), u32::MAX);
    assert_eq!(model.u32(regs::MAR0 + 4), u32::MAX);
    assert_eq!(model.u16(regs::INTR_MASK), regs::INTERRUPTS);
    assert_eq!(model.u16(regs::INTR_MITIGATE), 0);
    assert_eq!(model.u32(regs::TX_DESC_LOW), 0x2340_0000);
    assert_eq!(model.u32(regs::TX_DESC_HIGH), 0x1);
    assert_eq!(model.u32(regs::RX_DESC_LOW), 0x2340_0800);
    assert_eq!(model.u32(regs::RX_DESC_HIGH), 0x1);
    assert_eq!(model.u16(regs::RX_MAX_SIZE), regs::RX_MAX);
    assert_eq!(
        model.u32(regs::MISC) & regs::MISC_RXDV_GATE,
        0,
        "the receive path is not left gated"
    );
    assert_eq!(model.u8(regs::MCU) & regs::MCU_NOW_IS_OOB, 0);
}

#[test]
fn the_chip_s_own_aspm_clkreq_and_eee_are_off() {
    let rig = Rig::new();
    {
        let mut model = rig.model.borrow_mut();
        // Firmware left all three on.
        model.write(regs::CFG9346, 1, u32::from(regs::CFG_UNLOCK));
        model.write(regs::CONFIG2, 1, u32::from(regs::CONFIG2_CLKREQ));
        model.write(regs::CONFIG5, 1, u32::from(regs::CONFIG5_ASPM));
        model.write(regs::CFG9346, 1, u32::from(regs::CFG_LOCK));
        let _ = model.mac_ocp.insert(0xE092, 0x0004);
        let _ = model.eri.insert(0x1B0, 0x0003);
        let _ = model.phy_ocp.insert(0xA5D0, 0x0006);
    }
    let _driver = rig.init(small());
    let model = rig.model.borrow();
    assert_eq!(model.u8(regs::CONFIG2) & regs::CONFIG2_CLKREQ, 0);
    assert_eq!(model.u8(regs::CONFIG5) & regs::CONFIG5_ASPM, 0);
    assert_eq!(model.mac_ocp.get(&0xE092).copied().unwrap_or(0) & 0xFF, 0);
    assert_eq!(model.eri.get(&0x1B0).copied().unwrap_or(0) & 0x3, 0);
    assert_eq!(rig.phy(0xA5D0), 0, "no EEE advertised");
}

#[test]
fn the_phy_is_set_up_reset_and_told_to_negotiate() {
    let rig = Rig::new();
    let _driver = rig.init(small());
    assert_eq!(
        rig.phy(phy_reg(phy::ADVERTISE)),
        phy::ADVERTISE_ALL,
        "10/100 both duplexes and pause"
    );
    assert_eq!(rig.phy(phy_reg(phy::CTRL1000)), phy::ADVERTISE_1000_FULL);
    let bmcr = rig.phy(phy_reg(phy::BMCR));
    assert_ne!(bmcr & phy::BMCR_ANENABLE, 0);
    assert_eq!(bmcr & phy::BMCR_POWER_DOWN, 0, "out of power-down");
    assert!(rig.model.borrow().negotiating);
    // ALDPS, PFM and the 10 Mb/s PLL off.
    assert_eq!(rig.phy(0xA430) & 0b101, 0);
    assert_eq!(rig.phy(0xA442) & (1 << 7), 0);
    assert_ne!(rig.phy(0xA442) & (1 << 11), 0, "10 Mb/s amplitude on");
    // The 1 ms counter from the sawtooth count, 16e6 / 0x400.
    let counter = rig
        .model
        .borrow()
        .mac_ocp
        .get(&0xD412)
        .copied()
        .unwrap_or(0);
    assert_eq!(counter & 0x0FFF, (16_000_000 / 0x400) as u16 & 0x0FFF);
}

#[test]
fn gigabit_is_not_advertised_when_asked_not_to() {
    let rig = Rig::new();
    let _driver = rig.init(Options {
        gigabit: false,
        ..small()
    });
    assert_eq!(rig.phy(phy_reg(phy::CTRL1000)) & 0x0300, 0);
}

#[test]
fn the_ephy_table_is_applied_over_what_was_there() {
    let rig = Rig::new();
    rig.model.borrow_mut().ephy[0x1E] = 0x0800 | 0x0100;
    let _driver = rig.init(small());
    let model = rig.model.borrow();
    assert_eq!(model.ephy[0x1E], 0x0100 | 0x0001, "0x0800 cleared, 1 set");
    assert_eq!(model.ephy[0x05], 0x2089);
    assert_eq!(model.ephy[0x01], 0x068B);
}

#[test]
fn every_receive_buffer_is_lent_at_bring_up_by_its_scattered_address() {
    let rig = Rig::new();
    let options = small();
    let parts = rig.parts(&options);
    let expected: Vec<u64> = (0..u64::from(options.receive_entries))
        .map(|index| parts.receive.address(index * 2048))
        .collect();
    let _driver = Driver::init(parts, options).expect("bring-up works");
    let mut model = rig.model.borrow_mut();
    for (index, address) in expected.iter().enumerate() {
        let word0 = model.rx_word0(index as u32);
        assert_ne!(word0 & desc::OWN, 0, "buffer {index} is the chip's");
        assert_eq!(word0 & desc::RX_LENGTH, 2048);
        let end = index + 1 == usize::from(options.receive_entries);
        assert_eq!(word0 & desc::RING_END != 0, end, "only the last ends");
        assert_eq!(model.rx_address(index as u32), *address);
    }
}

#[test]
fn a_firmware_address_is_kept_when_the_chip_has_none_and_one_is_made_otherwise() {
    let rig = Rig::new();
    {
        let mut model = rig.model.borrow_mut();
        let _ = model.eri.insert(0xE0, 0);
        let _ = model.eri.insert(0xE4, 0);
        model.write(regs::CFG9346, 1, u32::from(regs::CFG_UNLOCK));
        model.write(regs::MAC0, 4, u32::from_le_bytes([0x00, 0x11, 0x22, 0x33]));
        model.write(regs::MAC4, 4, 0x5544);
    }
    let driver = rig.init(small());
    assert_eq!(driver.info().mac, [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    assert_eq!(driver.info().mac_source, MacSource::Firmware);

    let rig = Rig::new();
    {
        let mut model = rig.model.borrow_mut();
        let _ = model.eri.insert(0xE0, u32::MAX);
        let _ = model.eri.insert(0xE4, u32::MAX);
    }
    let driver = rig.init(small());
    assert_eq!(driver.info().mac, [0x02, 0x10, 0xEC, 0x12, 0x34, 0x56]);
    assert_eq!(driver.info().mac_source, MacSource::Generated);
}

#[test]
fn another_chip_is_refused_before_anything_is_written() {
    let rig = Rig::new();
    rig.model.borrow_mut().xid = 0x4C0;
    let failed = rig.fail(small());
    assert_eq!(failed.error, Error::Unsupported(0x4C0));
    assert!(!failed.reset);
    assert_eq!(rig.model.borrow().resets, 0, "nothing was touched");
}

#[test]
fn a_chip_that_does_not_answer_is_refused() {
    let rig = Rig::new();
    rig.model.borrow_mut().vanished = true;
    assert_eq!(rig.fail(small()).error, Error::Absent);
}

#[test]
fn a_reset_that_never_finishes_fails_without_claiming_the_chip_reset() {
    let rig = Rig::new();
    rig.model.borrow_mut().stuck_in_reset = true;
    let failed = rig.fail(small());
    assert_eq!(failed.error, Error::Timeout(Timeout::Reset));
    assert!(!failed.reset, "the chip may still reach the memory");
}

#[test]
fn a_dead_bus_times_out_and_the_chip_is_reset_after() {
    let rig = Rig::new();
    rig.model.borrow_mut().phy_dead = true;
    let failed = rig.fail(small());
    assert_eq!(failed.error, Error::Timeout(Timeout::Phy));
    assert!(failed.reset);

    let rig = Rig::new();
    rig.model.borrow_mut().eri_dead = true;
    assert_eq!(rig.fail(small()).error, Error::Timeout(Timeout::Eri));
}

#[test]
fn options_and_memory_that_do_not_fit_are_refused_before_the_chip_is_touched() {
    let rig = Rig::new();
    for options in [
        Options {
            transmit_entries: 1,
            ..small()
        },
        Options {
            receive_entries: MAX_ENTRIES as u16 + 1,
            ..small()
        },
        Options {
            buffer_bytes: 1024,
            ..small()
        },
        Options {
            buffer_bytes: 3000,
            ..small()
        },
    ] {
        assert_eq!(rig.fail(options).error, Error::BadOptions, "{options:?}");
    }
    let mut parts = rig.parts(&small());
    parts.rings = Mem::at(&rig.model, std::vec![0x1000_0800]);
    match Driver::init(parts, small()) {
        Err(failed) => assert_eq!(failed.error, Error::BadOptions),
        Ok(_) => panic!("an unaligned ring page was taken"),
    }
    let mut parts = rig.parts(&small());
    parts.receive = Mem::new(&rig.model, 0x80_0000, 1);
    match Driver::init(parts, small()) {
        Err(failed) => assert_eq!(failed.error, Error::BadOptions),
        Ok(_) => panic!("too little receive memory was taken"),
    }
    assert_eq!(rig.model.borrow().resets, 0);
}

// ----- the link -----

#[test]
fn the_link_comes_up_after_negotiation_and_goes_down_with_the_cable() {
    let rig = Rig::new();
    let mut driver = rig.init(small());
    assert_eq!(driver.poll_link(), Ok(None), "no cable");
    rig.model.borrow_mut().partner = Some(regs::STATUS_100 | regs::STATUS_FULL);
    assert_eq!(
        driver.poll_link(),
        Ok(Some(Link::Up {
            speed: Speed::Hundred,
            full_duplex: true
        }))
    );
    assert_eq!(driver.poll_link(), Ok(None), "no change, no news");
    rig.model.borrow_mut().partner = None;
    assert_eq!(driver.poll_link(), Ok(Some(Link::Down)));
}

#[test]
fn a_chip_that_vanished_stops_the_driver() {
    let rig = Rig::new();
    let mut driver = rig.up();
    rig.model.borrow_mut().vanished = true;
    assert_eq!(driver.poll_link(), Err(Error::BusError));
    assert_eq!(driver.poll_link(), Err(Error::Broken));
}

// ----- sending -----

#[test]
fn frames_go_out_in_order_and_come_back_by_id() {
    let rig = Rig::new();
    let mut driver = rig.up();
    let frames: Vec<Vec<u8>> = (0..3).map(|n| frame(100 + n * 50, n as u8)).collect();
    for (slot, bytes) in frames.iter().enumerate() {
        send(&mut driver, slot as u64, bytes, 70 + slot as u64).expect("taken");
    }
    assert_eq!(rig.model.borrow().sent, frames);
    let events = drain_all(&mut driver);
    assert_eq!(
        events,
        [
            Event::Sent { id: 70 },
            Event::Sent { id: 71 },
            Event::Sent { id: 72 }
        ]
    );
    assert_eq!(driver.counters().sent, 3);
}

#[test]
fn a_short_frame_goes_out_padded_with_what_the_slot_holds() {
    let rig = Rig::new();
    let mut driver = rig.up();
    // The process zeroes the slot past the frame; here it already is.
    send(&mut driver, 0, &frame(42, 9), 1).expect("taken");
    let sent = rig.model.borrow().sent[0].clone();
    assert_eq!(sent.len(), MIN_FRAME as usize);
    assert_eq!(&sent[..42], &frame(42, 9)[..]);
    assert!(sent[42..].iter().all(|byte| *byte == 0));
}

#[test]
fn the_transmit_ring_wraps_and_fills() {
    let rig = Rig::new();
    let mut driver = rig.up();
    // Hold the transmitter off so descriptors stay out.
    rig.model
        .borrow_mut()
        .write(regs::CHIP_CMD, 1, u32::from(regs::CMD_RX_ENABLE));
    for id in 0..8 {
        send(&mut driver, id, &frame(64, id as u8), id).expect("room");
    }
    assert_eq!(
        send(&mut driver, 0, &frame(64, 0), 99),
        Err(SubmitError::QueueFull)
    );
    // The transmitter back on; the next doorbell sends all eight.
    rig.model.borrow_mut().write(
        regs::CHIP_CMD,
        1,
        u32::from(regs::CMD_RX_ENABLE | regs::CMD_TX_ENABLE),
    );
    rig.model
        .borrow_mut()
        .write(regs::TX_POLL, 1, u32::from(regs::POLL_NORMAL));
    let events = drain_all(&mut driver);
    assert_eq!(events.len(), 8);
    // Round the ring a second time: the chip follows RING_END back to 0.
    for id in 8..14 {
        send(&mut driver, id % 8, &frame(64, id as u8), id).expect("room");
    }
    assert_eq!(drain_all(&mut driver).len(), 6);
    assert_eq!(rig.model.borrow().sent.len(), 14);
    assert_eq!(rig.model.borrow().sent[13], frame(64, 13));
}

#[test]
fn a_doorbell_is_rung_again_while_frames_are_still_out() {
    let rig = Rig::new();
    let mut driver = rig.up();
    send(&mut driver, 0, &frame(64, 1), 1).expect("taken");
    // The second frame's doorbell is "lost": the transmitter was busy.
    rig.model
        .borrow_mut()
        .write(regs::CHIP_CMD, 1, u32::from(regs::CMD_RX_ENABLE));
    send(&mut driver, 1, &frame(64, 2), 2).expect("taken");
    rig.model.borrow_mut().write(
        regs::CHIP_CMD,
        1,
        u32::from(regs::CMD_RX_ENABLE | regs::CMD_TX_ENABLE),
    );
    let first = drain_all(&mut driver);
    assert_eq!(first, [Event::Sent { id: 1 }], "the first went");
    assert_eq!(driver.counters().rekicks, 1);
    assert_eq!(
        drain_all(&mut driver),
        [Event::Sent { id: 2 }],
        "the rekick sent it"
    );
}

#[test]
fn bad_frames_are_refused() {
    let rig = Rig::new();
    let mut driver = rig.up();
    let refuse =
        |driver: &mut TestDriver, offset, len| driver.submit(&Frame { id: 0, offset, len });
    assert_eq!(refuse(&mut driver, 0, 0), Err(SubmitError::BadLength));
    assert_eq!(
        refuse(&mut driver, 0, 0x1_0000),
        Err(SubmitError::BadLength)
    );
    assert_eq!(
        refuse(&mut driver, 4000, 200),
        Err(SubmitError::OutsideData)
    );
    assert_eq!(
        refuse(&mut driver, 4090, 10),
        Err(SubmitError::OutsideData),
        "padded to 60"
    );
    assert_eq!(
        refuse(&mut driver, 8 * 4096, 64),
        Err(SubmitError::OutsideData)
    );
    assert!(rig.model.borrow().sent.is_empty());
}

// ----- receiving -----

#[test]
fn frames_arrive_in_their_buffers_and_return_on_release() {
    let rig = Rig::new();
    let mut driver = rig.up();
    let bytes = frame(300, 5);
    assert!(rig.model.borrow_mut().inject(&bytes));
    assert!(rig.model.borrow().interrupt(), "the interrupt is raised");
    let events = drain_all(&mut driver);
    assert_eq!(
        events,
        [Event::Received {
            buffer: 0,
            offset: 0,
            len: 300
        }]
    );
    assert!(!rig.model.borrow().interrupt(), "the drain acknowledged it");
    assert_eq!(driver.parts.receive.get(0, 300), bytes);
    assert_eq!(rig.model.borrow_mut().rx_word0(0) & desc::OWN, 0);
    driver.release(0).expect("held");
    assert_ne!(
        rig.model.borrow_mut().rx_word0(0) & desc::OWN,
        0,
        "lent again"
    );
    assert_eq!(driver.release(0), Err(Error::BadOptions), "not held twice");
}

#[test]
fn held_buffers_run_the_ring_dry_and_release_refills_it_round_the_end() {
    let rig = Rig::new();
    let mut driver = rig.up();
    for n in 0..8 {
        assert!(rig.model.borrow_mut().inject(&frame(64, n)));
    }
    assert!(
        !rig.model.borrow_mut().inject(&frame(64, 9)),
        "no descriptor left"
    );
    assert_eq!(drain_all(&mut driver).len(), 8);
    assert_eq!(driver.counters().receive_unavailable, 1);
    for buffer in 0..8 {
        driver.release(buffer).expect("held");
    }
    // The chip went back to descriptor 0 after the ring's end.
    assert!(rig.model.borrow_mut().inject(&frame(64, 10)));
    assert_eq!(
        drain_all(&mut driver),
        [Event::Received {
            buffer: 0,
            offset: 0,
            len: 64
        }]
    );
    assert_eq!(driver.parts.receive.get(0, 64), frame(64, 10));
}

#[test]
fn a_receive_room_takes_that_many_frames_and_leaves_the_rest_in_the_ring() {
    let rig = Rig::new();
    let mut driver = rig.up();
    for n in 0..3 {
        assert!(rig.model.borrow_mut().inject(&frame(64, n)));
    }
    let mut out = [Event::Sent { id: 0 }; 8];
    let drained = driver.drain_receiving(&mut out, 1).expect("works");
    assert_eq!(drained.events, 1);
    let drained = driver.drain_receiving(&mut out, 8).expect("works");
    assert_eq!(drained.events, 2, "the other two waited in the ring");
    assert_eq!(
        out[1],
        Event::Received {
            buffer: 2,
            offset: 2 * 2048,
            len: 64
        }
    );
}

#[test]
fn write_backs_that_lie_are_dropped_and_their_buffers_lent_again() {
    let rig = Rig::new();
    let mut driver = rig.up();
    for lie in [Lie::Length(4000), Lie::Error, Lie::Piece] {
        let mut model = rig.model.borrow_mut();
        model.lie = Some(lie);
        assert!(model.inject(&frame(64, 1)));
    }
    assert!(rig.model.borrow_mut().inject(&frame(80, 2)));
    let events = drain_all(&mut driver);
    assert_eq!(
        events,
        [Event::Received {
            buffer: 3,
            offset: 3 * 2048,
            len: 80
        }]
    );
    assert_eq!(driver.counters().receive_dropped, 3);
    let mut model = rig.model.borrow_mut();
    for index in 0..3 {
        assert_ne!(model.rx_word0(index) & desc::OWN, 0, "{index} lent again");
    }
}

#[test]
fn a_system_error_stops_the_driver() {
    let rig = Rig::new();
    let mut driver = rig.up();
    rig.model.borrow_mut().raise(regs::INT_SYSTEM_ERROR);
    let mut out = [Event::Sent { id: 0 }; 4];
    assert_eq!(driver.drain(&mut out), Err(Error::BusError));
    assert_eq!(driver.drain(&mut out), Err(Error::Broken));
    assert_eq!(
        send(&mut driver, 0, &frame(64, 0), 0),
        Err(SubmitError::Broken)
    );
}

// ----- take-down -----

#[test]
fn shutdown_closes_the_receiver_and_resets_the_chip() {
    let rig = Rig::new();
    let driver = rig.up();
    let before = rig.model.borrow().resets;
    match driver.shutdown() {
        Teardown::Released(_) => {}
        Teardown::Wedged(_) => panic!("the reset finished"),
    }
    let model = rig.model.borrow();
    assert_eq!(model.resets, before + 1);
    assert_eq!(model.u16(regs::INTR_MASK), 0);
    assert_eq!(model.u32(regs::RX_CONFIG) & regs::RX_ACCEPT_MASK, 0);
}

#[test]
fn a_chip_that_will_not_reset_keeps_its_memory() {
    let rig = Rig::new();
    let driver = rig.up();
    rig.model.borrow_mut().stuck_in_reset = true;
    assert!(matches!(driver.shutdown(), Teardown::Wedged(_)));
}
