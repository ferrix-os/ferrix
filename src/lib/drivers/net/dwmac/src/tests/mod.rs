//! The descriptor and link logic on their own, and the driver against
//! `model`'s core.

mod model;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::vec;
use std::vec::Vec;

use model::{FakeClock, Lie, Mem, Model, PHY_ID, Regs, Shared};

use crate::desc::{self, Received};
use crate::mdio::{self, Link, Speed};
use crate::regs::{self, phy};
use crate::{
    Driver, Error, Event, Failed, Frame, MAX_ENTRIES, Options, PAGE, Pages, Parts, SubmitError,
    Teardown,
};

type TestDriver = Driver<Regs, FakeClock, Mem, Mem>;
type TestParts = Parts<Regs, FakeClock, Mem, Mem>;

/// The YT6801's options, as `ferrix-yt6801` passes them.
fn yt6801() -> Options {
    Options {
        mac: [0x00, 0x0A, 0x35, 0x01, 0x02, 0x03],
        transmit_entries: 16,
        receive_entries: 16,
        buffer_bytes: 2048,
        bus: mdio::Bus {
            clock_range: 2,
            preamble_suppression: true,
            phy: 0,
        },
        gigabit: true,
        interrupt_mode: 1,
        fifo_cap: 81920,
        rx_fifo_share: 4,
    }
}

/// A link partner advertising everything up to 1000 full.
const PARTNER_ALL: (u16, u16) = (
    phy::ADVERTISE_CSMA
        | phy::ADVERTISE_10HALF
        | phy::ADVERTISE_10FULL
        | phy::ADVERTISE_100HALF
        | phy::ADVERTISE_100FULL,
    phy::LPA_1000FULL | phy::LPA_1000HALF,
);

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

    /// Parts for `options`: the ring page, then receive and transmit
    /// regions, all at scattered addresses.
    fn parts(&self, options: &Options) -> TestParts {
        let rx_pages =
            (usize::from(options.receive_entries) * options.buffer_bytes as usize).div_ceil(PAGE);
        Parts {
            regs: Regs(Rc::clone(&self.model)),
            clock: FakeClock(Rc::clone(&self.clock)),
            rings: Mem::new(&self.model, 0x1_2340_0000, 1),
            receive: Mem::new(&self.model, 0x80_0000, rx_pages),
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
        self.model
            .borrow_mut()
            .phy
            .plug(PARTNER_ALL.0, PARTNER_ALL.1);
        let mut driver = self.init(yt6801());
        let link = driver.poll_link().expect("the bus works");
        assert_eq!(
            link,
            Some(Link::Up {
                speed: Speed::Thousand,
                full_duplex: true
            })
        );
        driver
    }

    fn reg(&self, offset: u32) -> u32 {
        self.model.borrow().reg(offset)
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

// ----- descriptors and link resolution, on their own -----

#[test]
fn a_transmit_descriptor_is_one_whole_frame_handed_over_last() {
    let words = desc::transmit(0x12_3456_7000, 60);
    assert_eq!(words[0], 0x3456_7000);
    assert_eq!(words[1], 0x12);
    assert_eq!(words[2], desc::TX_IOC | 60);
    assert_eq!(words[3], desc::OWN | desc::TX_FD | desc::TX_LD | 60);
}

#[test]
fn a_receive_descriptor_lends_its_buffer() {
    let words = desc::receive(0xAB_0000_0800);
    assert_eq!(
        words,
        [0x800, 0xAB, 0, desc::OWN | desc::RX_IOC | desc::RX_BUF1V]
    );
}

#[test]
fn a_written_back_length_is_believed_only_inside_the_buffer() {
    let whole = desc::RX_FD | desc::RX_LD;
    assert_eq!(desc::received(whole | 60, 2048), Received::Frame(60));
    assert_eq!(desc::received(whole | 2048, 2048), Received::Frame(2048));
    assert_eq!(desc::received(whole | 2049, 2048), Received::Dropped);
    assert_eq!(desc::received(whole | 0x7FFF, 2048), Received::Dropped);
    assert_eq!(desc::received(whole | 13, 2048), Received::Dropped);
    assert_eq!(
        desc::received(whole | desc::RX_ES | 60, 2048),
        Received::Dropped
    );
    assert_eq!(
        desc::received(whole | desc::RX_CTXT | 60, 2048),
        Received::Dropped
    );
    assert_eq!(desc::received(desc::RX_FD | 60, 2048), Received::Dropped);
    assert_eq!(desc::received(desc::RX_LD | 60, 2048), Received::Dropped);
}

#[test]
fn the_best_common_mode_wins() {
    let all100 = phy::ADVERTISE_100FULL | phy::ADVERTISE_100HALF | phy::ADVERTISE_10FULL;
    let up = |speed, full_duplex| Link::Up { speed, full_duplex };
    assert_eq!(
        mdio::resolve(phy::ADVERTISE_1000FULL, phy::LPA_1000FULL, all100, all100),
        up(Speed::Thousand, true)
    );
    assert_eq!(
        mdio::resolve(phy::ADVERTISE_1000HALF, phy::LPA_1000HALF, 0, 0),
        up(Speed::Thousand, false)
    );
    assert_eq!(
        mdio::resolve(0, phy::LPA_1000FULL, all100, all100),
        up(Speed::Hundred, true)
    );
    assert_eq!(
        mdio::resolve(0, 0, all100, phy::ADVERTISE_100HALF),
        up(Speed::Hundred, false)
    );
    assert_eq!(
        mdio::resolve(0, 0, all100, phy::ADVERTISE_10FULL),
        up(Speed::Ten, true)
    );
    assert_eq!(
        mdio::resolve(0, 0, phy::ADVERTISE_10HALF, phy::ADVERTISE_10HALF),
        up(Speed::Ten, false)
    );
    assert_eq!(mdio::resolve(0, 0, all100, 0), Link::Down);
}

#[test]
fn fifo_sizes_and_address_width_come_from_feature_register_1() {
    assert_eq!(regs::rx_fifo_bytes(7), 16384);
    assert_eq!(regs::tx_fifo_bytes(7 << 6), 16384);
    assert_eq!(regs::address_bits(0), 32);
    assert_eq!(regs::address_bits(1 << 14), 40);
    assert_eq!(regs::address_bits(2 << 14), 48);
    assert_eq!(regs::address_bits(3 << 14), 32);
    // A hostile field: no panic, a size that wraps to nothing.
    assert_eq!(regs::rx_fifo_bytes(31), 0);
}

// ----- bring-up -----

#[test]
fn bring_up_resets_the_core_and_programs_it() {
    let rig = Rig::new();
    let driver = rig.init(yt6801());
    let info = *driver.info();
    assert_eq!(info.phy_id, PHY_ID);
    assert_eq!(info.address_bits, 40);
    assert_eq!(info.version & 0xFF, 0x51);
    assert_eq!(rig.model.borrow().resets, 1);

    // The rings, by the device address of the ring page.
    assert_eq!(rig.reg(regs::CH_TX_LIST_HIGH), 0x1);
    assert_eq!(rig.reg(regs::CH_TX_LIST_LOW), 0x2340_0000);
    assert_eq!(rig.reg(regs::CH_RX_LIST_LOW), 0x2340_0800);
    assert_eq!(rig.reg(regs::CH_TX_RING_LENGTH), 15);
    assert_eq!(rig.reg(regs::CH_RX_RING_LENGTH), 15);
    assert_eq!(
        (rig.reg(regs::CH_RX_CONTROL) >> regs::CH_RBSZ_SHIFT) & 0x3FFF,
        2048
    );
    // A 40-bit DMA needs the enhanced address mode.
    assert_ne!(rig.reg(regs::DMA_SYSBUS_MODE) & regs::SYSBUS_EAME, 0);
    assert_eq!((rig.reg(regs::DMA_MODE) >> regs::DMA_INTM_SHIFT) & 0b11, 1);
    assert_eq!(rig.reg(regs::CH_INTERRUPT_ENABLE), regs::CH_INTERRUPTS);
    // Both directions started, the MAC still off until the link is up.
    assert_ne!(rig.reg(regs::CH_TX_CONTROL) & regs::CH_TX_ST, 0);
    assert_ne!(rig.reg(regs::CH_RX_CONTROL) & regs::CH_RX_SR, 0);
    assert_eq!(
        rig.reg(regs::MAC_CONFIGURATION) & (regs::MAC_TE | regs::MAC_RE),
        0
    );
    assert_ne!(rig.reg(regs::MAC_CONFIGURATION) & regs::MAC_CST, 0);
    // The address: bytes 0-3 low, 4-5 high, enabled.
    assert_eq!(rig.reg(regs::MAC_ADDRESS0_LOW), 0x0135_0A00);
    assert_eq!(
        rig.reg(regs::MAC_ADDRESS0_HIGH),
        regs::ADDRESS_ENABLE | 0x0302
    );
    // The FIFO: 16 KiB each way reported, under the 80 KiB cap; receive
    // shared four ways: 4 KiB, 16 blocks less one.
    assert_eq!(
        (rig.reg(regs::MTL_TXQ0_OPERATION_MODE) >> regs::TXQ_TQS_SHIFT) & 0x1FF,
        63
    );
    assert_eq!(
        (rig.reg(regs::MTL_RXQ0_OPERATION_MODE) >> regs::RXQ_RQS_SHIFT) & 0x3FF,
        15
    );
    // The tails are one past each ring.
    assert_eq!(rig.reg(regs::CH_RX_TAIL), 0x2340_0800 + 16 * 16);
}

#[test]
fn every_receive_buffer_is_lent_at_bring_up_by_its_scattered_address() {
    let rig = Rig::new();
    let options = yt6801();
    let parts = rig.parts(&options);
    let pages = parts.receive.device_pages().to_vec();
    let ring_page = parts.rings.device_pages()[0];
    let unordered = Rc::clone(&parts.rings.unordered);
    let _driver = Driver::init(parts, options).expect("bring-up works");
    let mut model = rig.model.borrow_mut();
    for index in 0..16_u64 {
        let at = ring_page + 2048 + index * 16;
        let word = |model: &mut Model, offset: u64| {
            u32::from_le_bytes(core::array::from_fn(|byte| {
                model.read_byte(at + offset + byte as u64)
            }))
        };
        let address = u64::from(word(&mut model, 0)) | (u64::from(word(&mut model, 4)) << 32);
        let expected = pages[(index / 2) as usize] + (index % 2) * 2048;
        assert_eq!(address, expected, "buffer {index}");
        assert_eq!(
            word(&mut model, 12),
            desc::OWN | desc::RX_IOC | desc::RX_BUF1V
        );
    }
    assert_eq!(
        unordered.get(),
        0,
        "no descriptor handed over before its words"
    );
}

#[test]
fn a_reset_that_never_finishes_fails_without_claiming_the_core_reset() {
    let rig = Rig::new();
    rig.model.borrow_mut().stuck_reset = true;
    let failed = rig.fail(yt6801());
    assert_eq!(failed.error, Error::ResetTimeout);
    assert!(!failed.reset);
    // It waited about the second it promises, not forever.
    assert!(rig.clock.get() >= 1_000_000_000);
    assert!(rig.clock.get() < 2_000_000_000);
}

#[test]
fn no_phy_is_refused_and_the_core_is_reset_again() {
    let rig = Rig::new();
    rig.model.borrow_mut().phy_present = false;
    let failed = rig.fail(yt6801());
    assert_eq!(failed.error, Error::NoPhy(u32::MAX));
    assert!(failed.reset);
    assert_eq!(rig.model.borrow().resets, 2);
}

#[test]
fn a_bus_that_stays_busy_times_out() {
    let rig = Rig::new();
    rig.model.borrow_mut().mdio_stuck = true;
    let failed = rig.fail(yt6801());
    assert_eq!(failed.error, Error::MdioTimeout);
    assert!(failed.reset);
}

#[test]
fn a_phy_stuck_in_reset_times_out() {
    let rig = Rig::new();
    rig.model.borrow_mut().phy.stuck_in_reset = true;
    let failed = rig.fail(yt6801());
    assert_eq!(failed.error, Error::PhyResetTimeout);
}

#[test]
fn mdio_commands_carry_the_clock_range_and_preamble_suppression() {
    let rig = Rig::new();
    let _driver = rig.init(yt6801());
    let model = rig.model.borrow();
    // The first is the read of PHYSID1 at PHY 0: 0x0800020D | 2 << 16.
    assert_eq!(model.mdio_commands[0], 0x0802_020D);
    let mut options = yt6801();
    options.bus.preamble_suppression = false;
    options.bus.phy = 3;
    options.bus.clock_range = 5;
    let rig = Rig::new();
    let failed = rig.fail(options);
    assert_eq!(failed.error, Error::NoPhy(u32::MAX));
    assert_eq!(
        rig.model.borrow().mdio_commands[0],
        regs::MDIO_GB | regs::MDIO_GOC_READ | (5 << 8) | (2 << 16) | (3 << 21)
    );
}

#[test]
fn options_and_memory_that_do_not_fit_are_refused_before_the_core_is_touched() {
    let cases: [fn(&mut Options); 6] = [
        |o| o.transmit_entries = 0,
        |o| o.receive_entries = 0,
        |o| o.transmit_entries = MAX_ENTRIES as u16 + 1,
        |o| o.buffer_bytes = 3000,
        |o| o.buffer_bytes = 1024,
        |o| o.buffer_bytes = 8192,
    ];
    for change in cases {
        let rig = Rig::new();
        let mut options = yt6801();
        change(&mut options);
        let failed = rig.fail(options);
        assert_eq!(failed.error, Error::BadOptions);
        assert!(!failed.reset);
        assert_eq!(rig.model.borrow().resets, 0);
    }
    // A receive region too small for its buffers.
    let rig = Rig::new();
    let options = yt6801();
    let mut parts = rig.parts(&options);
    parts.receive = Mem::new(&rig.model, 0x80_0000, 7);
    assert_eq!(
        Driver::init(parts, options).map(|_| ()).unwrap_err().error,
        Error::BadOptions
    );
    // A ring page that is not page-aligned.
    let rig = Rig::new();
    let mut parts = rig.parts(&options);
    parts.rings = Mem::at(&rig.model, vec![0x1000_0800]);
    assert_eq!(
        Driver::init(parts, options).map(|_| ()).unwrap_err().error,
        Error::BadOptions
    );
}

#[test]
fn memory_past_the_dma_s_reach_is_refused() {
    let rig = Rig::new();
    rig.model.borrow_mut().feature1 = 7 | (7 << 6);
    let failed = rig.fail(yt6801());
    assert_eq!(failed.error, Error::AddressTooWide);
    assert!(failed.reset);

    // Below 4 GiB a 32-bit DMA is fine, and gets no enhanced addressing.
    let rig = Rig::new();
    rig.model.borrow_mut().feature1 = 7 | (7 << 6);
    let options = yt6801();
    let mut parts = rig.parts(&options);
    parts.rings = Mem::new(&rig.model, 0x4000_0000, 1);
    parts.transmit = Mem::new(&rig.model, 0x5000_0000, 8);
    let _driver = Driver::init(parts, options).expect("bring-up works");
    assert_eq!(rig.reg(regs::DMA_SYSBUS_MODE) & regs::SYSBUS_EAME, 0);
}

// ----- the link -----

#[test]
fn the_link_comes_up_and_the_mac_follows_it() {
    let rig = Rig::new();
    let mut driver = rig.init(yt6801());
    assert_eq!(driver.poll_link(), Ok(None), "no cable: still down");
    assert_eq!(driver.link(), Link::Down);

    rig.model.borrow_mut().phy.plug(
        phy::ADVERTISE_CSMA | phy::ADVERTISE_100HALF | phy::ADVERTISE_10FULL,
        0,
    );
    assert_eq!(
        driver.poll_link(),
        Ok(Some(Link::Up {
            speed: Speed::Hundred,
            full_duplex: false
        }))
    );
    let config = rig.reg(regs::MAC_CONFIGURATION);
    assert_ne!(config & regs::MAC_PS, 0);
    assert_ne!(config & regs::MAC_FES, 0);
    assert_eq!(config & regs::MAC_DM, 0);
    assert_ne!(config & regs::MAC_TE, 0);
    assert_ne!(config & regs::MAC_RE, 0);
    assert_eq!(driver.poll_link(), Ok(None), "unchanged");

    rig.model.borrow_mut().phy.unplug();
    assert_eq!(driver.poll_link(), Ok(Some(Link::Down)));
    assert_eq!(
        rig.reg(regs::MAC_CONFIGURATION) & (regs::MAC_TE | regs::MAC_RE),
        0
    );

    rig.model
        .borrow_mut()
        .phy
        .plug(PARTNER_ALL.0, PARTNER_ALL.1);
    assert_eq!(
        driver.poll_link(),
        Ok(Some(Link::Up {
            speed: Speed::Thousand,
            full_duplex: true
        }))
    );
    let config = rig.reg(regs::MAC_CONFIGURATION);
    assert_eq!(config & (regs::MAC_PS | regs::MAC_FES), 0);
    assert_ne!(config & regs::MAC_DM, 0);
}

#[test]
fn gigabit_is_not_advertised_when_asked_not_to() {
    let rig = Rig::new();
    rig.model
        .borrow_mut()
        .phy
        .plug(PARTNER_ALL.0, PARTNER_ALL.1);
    let mut options = yt6801();
    options.gigabit = false;
    let mut driver = rig.init(options);
    assert_eq!(
        driver.poll_link(),
        Ok(Some(Link::Up {
            speed: Speed::Hundred,
            full_duplex: true
        }))
    );
}

// ----- transmit -----

#[test]
fn frames_go_out_in_order_and_come_back_by_id() {
    let rig = Rig::new();
    let options = yt6801();
    let parts = rig.parts(&options);
    let transmit_pages = parts.transmit.device_pages().to_vec();
    let model = Rc::clone(&rig.model);
    let tx = Mem::at(&model, transmit_pages);
    rig.model
        .borrow_mut()
        .phy
        .plug(PARTNER_ALL.0, PARTNER_ALL.1);
    let mut driver = Driver::init(parts, options).expect("bring-up works");
    let _ = driver.poll_link().expect("link");
    let one = frame(60, 1);
    let two = frame(1514, 2);
    tx.put(0, &one);
    tx.put(PAGE as u64, &two);
    driver
        .submit(&Frame {
            id: 10,
            offset: 0,
            len: 60,
        })
        .expect("taken");
    driver
        .submit(&Frame {
            id: 11,
            offset: PAGE as u64,
            len: 1514,
        })
        .expect("taken");
    assert_eq!(rig.model.borrow().sent, vec![one, two]);
    assert!(rig.model.borrow().interrupt());
    assert_eq!(
        drain_all(&mut driver),
        vec![Event::Sent { id: 10 }, Event::Sent { id: 11 }]
    );
    assert!(!rig.model.borrow().interrupt(), "the status was cleared");
    assert_eq!(driver.counters().sent, 2);
    // Every tail write was one past the ring.
    let tx_end = 0x2340_0000 + 16 * 16;
    assert!(
        rig.model
            .borrow()
            .tails
            .iter()
            .filter(|(register, _)| *register == regs::CH_TX_TAIL)
            .all(|(_, value)| *value == tx_end)
    );
}

#[test]
fn the_transmit_ring_wraps_and_fills() {
    let rig = Rig::new();
    let mut driver = rig.up();
    let mut id = 0;
    for _round in 0..5 {
        for _ in 0..7 {
            driver
                .submit(&Frame {
                    id,
                    offset: 64 * (id % 32),
                    len: 60,
                })
                .expect("taken");
            id += 1;
        }
        let events = drain_all(&mut driver);
        assert_eq!(events.len(), 7);
    }
    assert_eq!(rig.model.borrow().sent.len(), 35);

    // With the transmitter off the frames stay out, until all are.
    let config = rig.reg(regs::MAC_CONFIGURATION);
    rig.model
        .borrow_mut()
        .register_write(regs::MAC_CONFIGURATION, config & !regs::MAC_TE);
    for index in 0..16 {
        driver
            .submit(&Frame {
                id: 100 + index,
                offset: 0,
                len: 60,
            })
            .expect("taken");
    }
    assert_eq!(
        driver.submit(&Frame {
            id: 200,
            offset: 0,
            len: 60
        }),
        Err(SubmitError::QueueFull)
    );
    assert!(drain_all(&mut driver).is_empty());
    rig.model
        .borrow_mut()
        .register_write(regs::MAC_CONFIGURATION, config);
    assert_eq!(drain_all(&mut driver).len(), 16);
    driver
        .submit(&Frame {
            id: 200,
            offset: 0,
            len: 60,
        })
        .expect("room again");
}

#[test]
fn bad_frames_are_refused() {
    let rig = Rig::new();
    let mut driver = rig.up();
    assert_eq!(
        driver.submit(&Frame {
            id: 1,
            offset: 0,
            len: 0
        }),
        Err(SubmitError::BadLength)
    );
    assert_eq!(
        driver.submit(&Frame {
            id: 1,
            offset: 0,
            len: desc::TX_LENGTH + 1
        }),
        Err(SubmitError::BadLength)
    );
    assert_eq!(
        driver.submit(&Frame {
            id: 1,
            offset: PAGE as u64 - 10,
            len: 60
        }),
        Err(SubmitError::OutsideData),
        "across a page"
    );
    assert_eq!(
        driver.submit(&Frame {
            id: 1,
            offset: 8 * PAGE as u64,
            len: 60
        }),
        Err(SubmitError::OutsideData),
        "past the region"
    );
    assert!(rig.model.borrow().sent.is_empty());
}

#[test]
fn a_failed_send_is_counted_and_still_handed_back() {
    let rig = Rig::new();
    let mut driver = rig.up();
    rig.model.borrow_mut().fail_sends = 1;
    driver
        .submit(&Frame {
            id: 1,
            offset: 0,
            len: 60,
        })
        .expect("taken");
    driver
        .submit(&Frame {
            id: 2,
            offset: 0,
            len: 60,
        })
        .expect("taken");
    assert_eq!(
        drain_all(&mut driver),
        vec![Event::Sent { id: 1 }, Event::Sent { id: 2 }]
    );
    assert_eq!(driver.counters().send_errors, 1);
    assert_eq!(driver.counters().sent, 1);
}

// ----- receive -----

#[test]
fn frames_arrive_in_their_buffers_and_return_on_release() {
    let rig = Rig::new();
    let options = yt6801();
    let parts = rig.parts(&options);
    let rx = Mem::at(&rig.model, parts.receive.device_pages().to_vec());
    rig.model
        .borrow_mut()
        .phy
        .plug(PARTNER_ALL.0, PARTNER_ALL.1);
    let mut driver = Driver::init(parts, options).expect("bring-up works");
    let _ = driver.poll_link().expect("link");

    let mut expected = Vec::new();
    for index in 0..40_u32 {
        let bytes = frame(64 + index as usize * 30, index as u8);
        assert!(rig.model.borrow_mut().inject(&bytes), "frame {index} taken");
        let events = drain_all(&mut driver);
        assert_eq!(events.len(), 1);
        let Event::Received {
            buffer,
            offset,
            len,
        } = events[0]
        else {
            panic!("not a receive: {events:?}");
        };
        assert_eq!(u32::from(buffer), index % 16);
        assert_eq!(offset, u64::from(buffer) * 2048);
        assert_eq!(len as usize, bytes.len());
        assert_eq!(rx.get(offset, bytes.len()), bytes);
        driver.release(buffer).expect("held");
        expected.push(bytes);
    }
    assert_eq!(driver.counters().received, 40);
    assert_eq!(driver.counters().receive_dropped, 0);
}

#[test]
fn held_buffers_run_the_ring_dry_and_release_refills_it() {
    let rig = Rig::new();
    let mut driver = rig.up();
    for index in 0..16 {
        assert!(rig.model.borrow_mut().inject(&frame(100, index)));
    }
    assert!(
        !rig.model.borrow_mut().inject(&frame(100, 99)),
        "no descriptor left"
    );
    assert_eq!(rig.model.borrow().missed, 1);
    let events = drain_all(&mut driver);
    assert_eq!(events.len(), 16);
    assert_eq!(driver.counters().receive_unavailable, 1);
    // Nothing more while every buffer is held.
    assert!(drain_all(&mut driver).is_empty());
    assert!(!rig.model.borrow_mut().inject(&frame(100, 98)));

    driver.release(0).expect("held");
    assert_eq!(driver.release(0), Err(Error::BadOptions), "not held twice");
    assert_eq!(driver.release(MAX_ENTRIES as u16), Err(Error::BadOptions));
    assert!(rig.model.borrow_mut().inject(&frame(100, 97)));
    let events = drain_all(&mut driver);
    assert_eq!(
        events,
        vec![Event::Received {
            buffer: 0,
            offset: 0,
            len: 100
        }]
    );
}

#[test]
fn a_short_slice_takes_what_it_holds_and_leaves_the_rest() {
    let rig = Rig::new();
    let mut driver = rig.up();
    for index in 0..5 {
        assert!(rig.model.borrow_mut().inject(&frame(80, index)));
    }
    let mut out = [Event::Sent { id: 0 }; 2];
    assert_eq!(driver.drain(&mut out).expect("works").events, 2);
    assert_eq!(driver.drain(&mut out).expect("works").events, 2);
    assert_eq!(driver.drain(&mut out).expect("works").events, 1);
    assert_eq!(driver.drain(&mut out).expect("works").events, 0);
    assert_eq!(driver.counters().received, 5);
}

#[test]
fn write_backs_that_lie_are_dropped_and_their_buffers_lent_again() {
    for lie in [
        Lie::Length(2049),
        Lie::Length(0x7FFF),
        Lie::Length(5),
        Lie::Error,
        Lie::Context,
        Lie::FirstOnly,
    ] {
        let rig = Rig::new();
        let mut driver = rig.up();
        rig.model.borrow_mut().lies.push(lie);
        assert!(rig.model.borrow_mut().inject(&frame(100, 1)));
        assert!(rig.model.borrow_mut().inject(&frame(100, 2)));
        let events = drain_all(&mut driver);
        assert_eq!(
            events,
            vec![Event::Received {
                buffer: 1,
                offset: 2048,
                len: 100
            }],
            "{lie:?}"
        );
        assert_eq!(driver.counters().receive_dropped, 1, "{lie:?}");
        // Descriptor 0 is the core's again: the ring goes all the way
        // round without running dry.
        driver.release(1).expect("held");
        for index in 0..16 {
            assert!(rig.model.borrow_mut().inject(&frame(64, index)), "{lie:?}");
            let events = drain_all(&mut driver);
            let Event::Received { buffer, .. } = events[0] else {
                panic!("{events:?}");
            };
            driver.release(buffer).expect("held");
        }
    }
}

#[test]
fn a_fatal_bus_error_stops_the_driver() {
    let rig = Rig::new();
    let mut driver = rig.up();
    rig.model.borrow_mut().fail_bus();
    let mut out = [Event::Sent { id: 0 }; 4];
    assert_eq!(driver.drain(&mut out), Err(Error::BusError));
    assert_eq!(driver.drain(&mut out), Err(Error::Broken));
    assert_eq!(
        driver.submit(&Frame {
            id: 1,
            offset: 0,
            len: 60
        }),
        Err(SubmitError::Broken)
    );
    assert_eq!(driver.release(0), Err(Error::Broken));
    assert_eq!(driver.poll_link(), Err(Error::Broken));
    // Shutdown still resets it, so the memory may go.
    assert!(matches!(driver.shutdown(), Teardown::Released(_)));
}

// ----- shutdown -----

#[test]
fn shutdown_stops_both_directions_and_gives_memory_back_only_after_a_reset() {
    let rig = Rig::new();
    let driver = rig.up();
    let teardown = driver.shutdown();
    assert!(matches!(teardown, Teardown::Released(_)));
    assert_eq!(rig.model.borrow().resets, 2);

    let rig = Rig::new();
    let driver = rig.up();
    rig.model.borrow_mut().stuck_reset = true;
    let Teardown::Wedged(_) = driver.shutdown() else {
        panic!("a core that never resets is not given up");
    };
    assert_eq!(rig.reg(regs::CH_TX_CONTROL) & regs::CH_TX_ST, 0);
    assert_eq!(rig.reg(regs::CH_RX_CONTROL) & regs::CH_RX_SR, 0);
    assert_eq!(
        rig.reg(regs::MAC_CONFIGURATION) & (regs::MAC_TE | regs::MAC_RE),
        0
    );
}
