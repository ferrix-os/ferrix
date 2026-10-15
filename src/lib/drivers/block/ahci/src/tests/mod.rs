//! The driver against `model`'s HBA.

mod model;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::vec;
use std::vec::Vec;

use model::{FakeClock, Mem, Model, Regs, Shared};

use crate::identify::Refused;
use crate::regs;
use crate::{Ahci, Error, Op, Segment};

type TestAhci = Ahci<Regs, FakeClock, Mem>;

/// Control pages, out of order on purpose.
const CONTROL: [u64; 2] = [0x9000, 0x3000];

fn start(model: Model) -> (Shared, Rc<Cell<u64>>, Result<TestAhci, Error>) {
    let model = Rc::new(RefCell::new(model));
    let clock = Rc::new(Cell::new(0));
    let mem = Mem {
        model: model.clone(),
        pages: CONTROL.to_vec(),
    };
    let ahci = Ahci::init(Regs(model.clone()), FakeClock(clock.clone()), mem);
    (model, clock, ahci)
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
        .collect()
}

#[test]
fn an_offline_link_is_comreset_and_the_disk_found() {
    let mut model = Model::new(1000, 512);
    // What a BIOS that put the port offline leaves: DET 4, nothing busy.
    model.ports[0].ssts = 0x4;
    model.ports[0].after_reset = Some((0x113, 0x50, regs::SIG_ATA));
    let (model, _, ahci) = start(model);
    assert_eq!(ahci.unwrap().identity().sectors, 1000);
    assert_eq!(model.borrow().ports[0].comresets, 1);
}

#[test]
fn a_device_left_busy_is_comreset_and_then_identified() {
    let mut model = Model::new(1000, 512);
    model.ports[0].tfd = 0x80;
    model.ports[0].after_reset = Some((0x113, 0x50, regs::SIG_ATA));
    let (model, _, ahci) = start(model);
    assert_eq!(ahci.unwrap().identity().sectors, 1000);
    assert_eq!(model.borrow().ports[0].comresets, 1);
}

#[test]
fn an_empty_port_is_not_reset() {
    let mut model = Model::new(1000, 512);
    model.ports[0].ssts = 0;
    let (model, _, ahci) = start(model);
    assert!(matches!(ahci, Err(Error::NoDrive)));
    assert_eq!(model.borrow().ports[0].comresets, 0);
}

#[test]
fn init_identifies_the_disk() {
    let (model, _, ahci) = start(Model::new(1000, 512));
    let ahci = ahci.unwrap();
    let id = ahci.identity();
    assert_eq!(id.sectors, 1000);
    assert_eq!(id.sector_size, 512);
    assert!(id.model.starts_with(b"FERRIX MODEL DISK  "));
    assert!(id.serial.starts_with(b"SN-0042 "));
    assert_eq!(id.port, 0);
    let m = model.borrow();
    assert_eq!(m.ports[0].clb, 0x9000);
    assert_eq!(m.ports[0].fb, 0x9000 + 1024);
    assert_eq!(
        m.ports[0].cmd & (regs::CMD_ST | regs::CMD_FRE),
        regs::CMD_ST | regs::CMD_FRE
    );
    assert_eq!(m.ghc & (regs::GHC_AE | regs::GHC_IE), regs::GHC_AE);
    assert_eq!(m.seen[0].opcode, 0xEC);
    assert_eq!(m.seen[0].prds, vec![(0x3000, 512)]);
}

#[test]
fn init_skips_empty_and_atapi_ports() {
    let mut m = Model::new(100, 512);
    m.pi = 0b1111;
    m.ports[0].ssts = 0;
    m.ports[1] = m.ports[2];
    m.ports[1].ssts = 0x113;
    m.ports[1].sig = 0xEB14_0101;
    m.ports[2] = model::Port {
        sig: regs::SIG_ATA,
        ssts: 0x113,
        tfd: 0x50,
        ..Default::default()
    };
    let (model, _, ahci) = start(m);
    assert_eq!(ahci.unwrap().identity().port, 2);
    assert_eq!(
        model.borrow().ports[1].cmd & (regs::CMD_ST | regs::CMD_FRE),
        0
    );
}

#[test]
fn no_drive() {
    let mut m = Model::new(100, 512);
    m.ports[0].ssts = 0;
    assert_eq!(start(m).2.err(), Some(Error::NoDrive));
}

#[test]
fn identify_logical_sector_size_and_lba48() {
    let (_, _, ahci) = start(Model::new(300, 4096));
    let ahci = ahci.unwrap();
    assert_eq!(ahci.identity().sector_size, 4096);
    assert_eq!(ahci.identity().sectors, 300);

    let mut m = Model::new(300, 512);
    m.identify[83] = 0;
    let refused = start(m).2.err();
    assert!(
        matches!(refused, Some(Error::Identify(Refused::NoLba48, words)) if words[7] == 0),
        "{refused:?}"
    );
}

#[test]
fn bios_handoff() {
    let mut m = Model::new(100, 512);
    m.cap2 = regs::CAP2_BOH;
    m.bohc = regs::BOHC_BOS;
    let (model, _, ahci) = start(m);
    assert!(ahci.is_ok());
    assert_eq!(model.borrow().bohc, regs::BOHC_OOS);
}

#[test]
fn write_then_read() {
    let (model, _, ahci) = start(Model::new(100, 512));
    let mut ahci = ahci.unwrap();
    let data = pattern(8 * 512, 3);
    model.borrow_mut().write_bytes(0x10_0000, &data);
    let out = [Segment {
        addr: 0x10_0000,
        len: 4096,
    }];
    ahci.transfer(Op::Write, 10, 8, &out).unwrap();
    assert_eq!(&model.borrow().disk[10 * 512..18 * 512], &data[..]);
    let back = [Segment {
        addr: 0x20_0000,
        len: 4096,
    }];
    ahci.transfer(Op::Read, 10, 8, &back).unwrap();
    assert_eq!(model.borrow_mut().read_bytes(0x20_0000, 4096), data);
    let m = model.borrow();
    let last = m.seen.last().unwrap();
    assert_eq!(
        (last.opcode, last.lba, last.count, last.write),
        (0x25, 10, 8, false)
    );
    assert_eq!(m.seen[m.seen.len() - 2].opcode, 0x35);
    assert!(m.seen[m.seen.len() - 2].write);
}

#[test]
fn multi_segment_prdt() {
    let (model, _, ahci) = start(Model::new(100, 512));
    let mut ahci = ahci.unwrap();
    let disk = pattern(100 * 512, 9);
    model.borrow_mut().disk.copy_from_slice(&disk);
    let segs = [
        Segment {
            addr: 0x40_0000,
            len: 1024,
        },
        Segment {
            addr: 0x50_0002,
            len: 510,
        },
        Segment {
            addr: 0x60_0000,
            len: 2562,
        },
    ];
    ahci.transfer(Op::Read, 5, 8, &segs).unwrap();
    let mut got = model.borrow_mut().read_bytes(0x40_0000, 1024);
    got.extend(model.borrow_mut().read_bytes(0x50_0002, 510));
    got.extend(model.borrow_mut().read_bytes(0x60_0000, 2562));
    assert_eq!(got, &disk[5 * 512..13 * 512]);
    let m = model.borrow();
    let prds: Vec<_> = segs.iter().map(|s| (s.addr, s.len)).collect();
    assert_eq!(m.seen.last().unwrap().prds, prds);
}

#[test]
fn splits_when_the_prdt_is_full() {
    let (model, _, ahci) = start(Model::new(200, 512));
    let mut ahci = ahci.unwrap();
    let segs: Vec<_> = (0..130u64)
        .map(|i| Segment {
            addr: 0x100_0000 + i * 0x2000,
            len: 512,
        })
        .collect();
    ahci.transfer(Op::Write, 7, 130, &segs).unwrap();
    let m = model.borrow();
    let n = m.seen.len();
    assert_eq!(
        (
            m.seen[n - 2].lba,
            m.seen[n - 2].count,
            m.seen[n - 2].prds.len()
        ),
        (7, 120, 120)
    );
    assert_eq!((m.seen[n - 1].lba, m.seen[n - 1].count), (127, 10));
}

#[test]
fn splits_a_segment_over_four_mib() {
    let (model, _, ahci) = start(Model::new(8200, 512));
    let mut ahci = ahci.unwrap();
    let len = (4 << 20) + 1024;
    ahci.transfer(
        Op::Read,
        0,
        8194,
        &[Segment {
            addr: 0x1000_0000,
            len,
        }],
    )
    .unwrap();
    let m = model.borrow();
    assert_eq!(
        m.seen.last().unwrap().prds,
        vec![(0x1000_0000, 4 << 20), (0x1040_0000, 1024)]
    );
}

#[test]
fn flush() {
    let (model, _, ahci) = start(Model::new(100, 512));
    let mut ahci = ahci.unwrap();
    ahci.transfer(Op::Flush, 0, 0, &[]).unwrap();
    let m = model.borrow();
    assert_eq!(m.flushes, 1);
    assert_eq!(m.seen.last().unwrap().opcode, 0xEA);
}

#[test]
fn device_error_then_recovery() {
    let (model, _, ahci) = start(Model::new(100, 512));
    let mut ahci = ahci.unwrap();
    model.borrow_mut().fail_next = Some((0x51, 0x40));
    let seg = [Segment {
        addr: 0x10_0000,
        len: 512,
    }];
    assert_eq!(
        ahci.transfer(Op::Read, 1, 1, &seg),
        Err(Error::Device {
            status: 0x51,
            error: 0x40
        })
    );
    ahci.transfer(Op::Read, 1, 1, &seg).unwrap();
}

#[test]
fn timeout_then_recovery() {
    let (model, clock, ahci) = start(Model::new(100, 512));
    let mut ahci = ahci.unwrap();
    model.borrow_mut().hang = true;
    let seg = [Segment {
        addr: 0x10_0000,
        len: 512,
    }];
    assert_eq!(
        ahci.transfer(Op::Read, 1, 1, &seg),
        Err(Error::Timeout("command"))
    );
    assert!(clock.get() >= 10_000_000_000);
    model.borrow_mut().hang = false;
    ahci.transfer(Op::Read, 1, 1, &seg).unwrap();
}

#[test]
fn refused_requests() {
    let (_, _, ahci) = start(Model::new(100, 512));
    let mut ahci = ahci.unwrap();
    let one = [Segment {
        addr: 0x10_0000,
        len: 512,
    }];
    assert_eq!(
        ahci.transfer(Op::Read, 100, 1, &one),
        Err(Error::OutOfRange)
    );
    assert_eq!(ahci.transfer(Op::Read, 0, 2, &one), Err(Error::BadBuffer));
    let odd = [Segment {
        addr: 0x10_0001,
        len: 512,
    }];
    assert_eq!(ahci.transfer(Op::Read, 0, 1, &odd), Err(Error::Unaligned));
}

#[test]
fn shutdown_flushes_and_stops() {
    let (model, _, ahci) = start(Model::new(100, 512));
    let _parts = ahci.unwrap().shutdown();
    let m = model.borrow();
    assert_eq!(m.flushes, 1);
    assert_eq!(
        m.ports[0].cmd & (regs::CMD_ST | regs::CMD_FRE | regs::CMD_CR | regs::CMD_FR),
        0
    );
}
