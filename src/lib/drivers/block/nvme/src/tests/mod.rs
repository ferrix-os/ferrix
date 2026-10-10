//! The queue and PRP logic on their own, and the driver against `model`'s
//! controller.

mod model;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::vec;
use std::vec::Vec;

use model::{FakeClock, Mem, Model, Regs, Shared};

use crate::command::{self, Command};
use crate::driver::{
    Cause, Completion, Driver, Event, Fatal, InitError, MAX_ENTRIES, MAX_RETRIES, Op, Options,
    Outcome, Parts, Request, Shutdown, SubmitError, Submitted, list_pages_needed,
    queue_pages_needed,
};
use crate::hmb::{Chunk, Wish};
use crate::identify::NamespaceRefused;
use crate::prp::{self, LIST_ENTRIES, PrpError};
use crate::queue::Indices;
use crate::quirks::{self, Quirks};
use crate::regs::{self, Capabilities, PAGE};
use crate::{Memory, Registers};

type TestDriver = Driver<Regs, FakeClock, Mem, Mem, Mem>;

/// Pages of data region in a test.
const DATA_PAGES: usize = 128;

struct Rig {
    model: Shared,
    clock: Rc<Cell<u64>>,
}

impl Rig {
    fn new(blocks: usize, shift: u8) -> Self {
        Rig {
            model: Rc::new(RefCell::new(Model::new(blocks, shift))),
            clock: Rc::new(Cell::new(1)),
        }
    }

    fn parts(&self, options: &Options) -> Parts<Regs, FakeClock, Mem, Mem, Mem> {
        let queues = usize::from(options.io_queues);
        Parts {
            regs: Regs(Rc::clone(&self.model)),
            clock: FakeClock(Rc::clone(&self.clock)),
            queues: Mem::new(&self.model, 0x1000_0000, queue_pages_needed(queues)),
            lists: Mem::new(
                &self.model,
                0x2000_0000,
                list_pages_needed(queues, options.list_pages),
            ),
            data: Mem::new(&self.model, 0x4000_0000, DATA_PAGES),
        }
    }

    fn up_with(&self, options: Options) -> TestDriver {
        let mut driver = Driver::enable(self.parts(&options), options).expect("enabled");
        driver.bring_up().expect("brought up");
        driver
    }

    fn up(&self) -> TestDriver {
        self.up_with(Options::default())
    }

    fn advance(&self, nanos: u64) {
        self.clock.set(self.clock.get() + nanos);
    }
}

fn read(id: u64, lba: u64, blocks: u32, data_offset: u64) -> Request {
    Request {
        id,
        op: Op::Read,
        lba,
        blocks,
        data_offset,
        fua: false,
    }
}

fn write(id: u64, lba: u64, blocks: u32, data_offset: u64) -> Request {
    Request {
        op: Op::Write,
        ..read(id, lba, blocks, data_offset)
    }
}

fn flush(id: u64) -> Request {
    Request {
        op: Op::Flush,
        ..read(id, 0, 0, 0)
    }
}

fn drain(driver: &mut TestDriver) -> Vec<Completion> {
    let mut out = [Completion {
        id: 0,
        outcome: Outcome::Ok,
        bytes: 0,
    }; 8];
    let mut all = Vec::new();
    // More rounds than a request has retries: a command sent again by one
    // drain is completed by the model at once, with no interrupt in a test
    // to say so.
    let mut rounds = 0;
    while rounds < 8 {
        let drained = driver.drain(&mut out).expect("drained");
        all.extend_from_slice(&out[..drained.completions]);
        if !drained.more {
            rounds += 1;
        }
    }
    all
}

fn ok(id: u64, bytes: u64) -> Completion {
    Completion {
        id,
        outcome: Outcome::Ok,
        bytes,
    }
}

// -- The registers' fields --------------------------------------------------

#[test]
fn capabilities_are_taken_apart_by_field() {
    // MQES 0x3FF, TO 0x0A, DSTRD 2, CSS bit 0, MPSMIN 1, MPSMAX 5.
    let cap = Capabilities(0x3FF | (0x0A << 24) | (2 << 32) | (1 << 37) | (1 << 48) | (5 << 52));
    assert_eq!(cap.max_queue_entries(), 0x400);
    assert_eq!(cap.ready_timeout_nanos(), 5_000_000_000);
    assert_eq!(cap.doorbell_stride(), 16);
    assert!(cap.nvm_command_set());
    assert_eq!(cap.min_page_shift(), 13);
    assert_eq!(cap.max_page_shift(), 17);
    assert_eq!(regs::sq_doorbell(0, 4), 0x1000);
    assert_eq!(regs::cq_doorbell(0, 4), 0x1004);
    assert_eq!(regs::sq_doorbell(3, 4), 0x1018);
    assert_eq!(regs::cq_doorbell(3, 16), 0x1000 + 7 * 16);
}

#[test]
fn commands_put_each_field_where_the_specification_does() {
    let rw = Command::transfer(command::NVM_WRITE, 7, 0x1122_3344_5566_7788, 0x100, true)
        .prp(0xAAAA_0000_1000, 0xBBBB_0000_2000)
        .id(0x1234);
    assert_eq!(rw.0[0], 0x01);
    assert_eq!(rw.0[1], 0, "PRPs, not SGLs, and no fused operation");
    assert_eq!(&rw.0[2..4], &[0x34, 0x12]);
    assert_eq!(&rw.0[4..8], &7_u32.to_le_bytes());
    assert_eq!(&rw.0[24..32], &0xAAAA_0000_1000_u64.to_le_bytes());
    assert_eq!(&rw.0[32..40], &0xBBBB_0000_2000_u64.to_le_bytes());
    assert_eq!(&rw.0[40..48], &0x1122_3344_5566_7788_u64.to_le_bytes());
    // The count is zero based; force unit access is bit 30 of CDW12.
    assert_eq!(&rw.0[48..52], &(0xFF_u32 | (1 << 30)).to_le_bytes());

    let cq = Command::create_cq(3, 64, 0x9000, 2);
    assert_eq!(cq.0[0], 0x05);
    assert_eq!(&cq.0[40..44], &((63_u32 << 16) | 3).to_le_bytes());
    assert_eq!(&cq.0[44..48], &((2_u32 << 16) | 0b11).to_le_bytes());
    let sq = Command::create_sq(3, 64, 0xA000);
    assert_eq!(sq.0[0], 0x01);
    assert_eq!(&sq.0[44..48], &((3_u32 << 16) | 1).to_le_bytes());

    let abort = Command::abort(2, 0x0513);
    assert_eq!(abort.0[0], 0x08);
    assert_eq!(&abort.0[40..44], &((0x0513_u32 << 16) | 2).to_le_bytes());

    let hmb = Command::host_memory(3, 0x4000, 0x1_2345_6000, 16);
    assert_eq!(hmb.0[0], 0x09);
    assert_eq!(&hmb.0[40..44], &0x0D_u32.to_le_bytes());
    assert_eq!(&hmb.0[44..48], &3_u32.to_le_bytes());
    assert_eq!(&hmb.0[48..52], &0x4000_u32.to_le_bytes());
    assert_eq!(&hmb.0[52..56], &0x2345_6000_u32.to_le_bytes());
    assert_eq!(&hmb.0[56..60], &1_u32.to_le_bytes());
    assert_eq!(&hmb.0[60..64], &16_u32.to_le_bytes());
}

// -- A queue pair's indices -------------------------------------------------

#[test]
fn a_submission_queue_holds_one_less_than_its_entries() {
    let mut indices = Indices::new(4);
    assert_eq!(indices.push(), Some((0, 1)));
    assert_eq!(indices.push(), Some((1, 2)));
    assert_eq!(indices.push(), Some((2, 3)));
    assert!(indices.sq_full());
    assert_eq!(indices.push(), None);
    // The controller read two: two more fit, and the tail wraps.
    let _ = indices.pop(2);
    assert_eq!(indices.push(), Some((3, 0)));
    assert_eq!(indices.push(), Some((0, 1)));
    assert_eq!(indices.push(), None);
}

#[test]
fn the_expected_phase_flips_when_the_completion_head_wraps() {
    let mut indices = Indices::new(3);
    assert!(indices.is_new(true));
    assert!(!indices.is_new(false));
    assert_eq!(indices.pop(0), 1);
    assert_eq!(indices.pop(0), 2);
    assert!(indices.is_new(true));
    assert_eq!(indices.pop(0), 0);
    assert!(indices.is_new(false), "the second pass has the other phase");
    assert!(!indices.is_new(true), "an entry of the first pass is old");
    // A head the controller could not have reported is not believed.
    let _ = indices.pop(9);
    assert!(!indices.sq_full());
}

// -- PRPs -------------------------------------------------------------------

fn prp_rig(data_pages: usize, list_pages: usize) -> (Mem, Mem) {
    let model = Rc::new(RefCell::new(Model::new(8, 9)));
    (
        Mem::new(&model, 0x4000_0000, data_pages),
        Mem::new(&model, 0x2000_0000, list_pages),
    )
}

fn list_entry(lists: &Mem, page: usize, slot: usize) -> u64 {
    let bytes = lists.bytes(page * PAGE + slot * 8, 8);
    u64::from_le_bytes(bytes.try_into().unwrap())
}

#[test]
fn data_within_one_page_needs_only_the_first_entry() {
    let (data, mut lists) = prp_rig(8, 2);
    let prp = prp::build(&data, &mut lists, 0, 1, 2 * PAGE as u64 + 512, 512).unwrap();
    assert_eq!(prp.first, data.pages()[2] + 512);
    assert_eq!(prp.second, 0);
    // To the last byte of the page is still one page.
    let prp = prp::build(&data, &mut lists, 0, 1, 512, PAGE as u64 - 512).unwrap();
    assert_eq!(prp.second, 0);
}

#[test]
fn data_ending_in_the_second_page_names_it_in_the_second_entry() {
    let (data, mut lists) = prp_rig(8, 2);
    let prp = prp::build(&data, &mut lists, 0, 1, PAGE as u64, 2 * PAGE as u64).unwrap();
    assert_eq!(prp.first, data.pages()[1]);
    assert_eq!(prp.second, data.pages()[2]);
    // One byte into the second page is enough.
    let prp = prp::build(&data, &mut lists, 0, 1, 4, PAGE as u64).unwrap();
    assert_eq!(prp.first, data.pages()[0] + 4);
    assert_eq!(prp.second, data.pages()[1]);
}

#[test]
fn data_beyond_two_pages_goes_through_a_list() {
    let (data, mut lists) = prp_rig(8, 3);
    // From the middle of page 1, five pages' worth: it ends in page 6.
    let prp = prp::build(&data, &mut lists, 1, 1, PAGE as u64 + 2048, 5 * PAGE as u64).unwrap();
    assert_eq!(prp.first, data.pages()[1] + 2048);
    assert_eq!(prp.second, lists.pages()[1], "the list page given");
    for (slot, page) in (2..=6).enumerate() {
        assert_eq!(list_entry(&lists, 1, slot), data.pages()[page]);
    }
    assert_eq!(list_entry(&lists, 1, 5), 0, "nothing past the last page");
    assert_eq!(list_entry(&lists, 0, 0), 0, "nor in another slot's list");
}

#[test]
fn a_list_chains_through_its_last_entry_only_when_it_must() {
    // 512 pages after the first fill one list exactly: no chain.
    let (data, mut lists) = prp_rig(LIST_ENTRIES + 2, 3);
    let bytes = (LIST_ENTRIES as u64 + 1) * PAGE as u64;
    let prp = prp::build(&data, &mut lists, 0, 2, 0, bytes).unwrap();
    assert_eq!(prp.second, lists.pages()[0]);
    assert_eq!(list_entry(&lists, 0, 511), data.pages()[512]);
    assert_eq!(list_entry(&lists, 1, 0), 0);
    assert_eq!(prp::list_pages_for(LIST_ENTRIES), 1);

    // One more: the last entry becomes the next list's address, and the
    // two pages it displaced go there.
    let (data, mut lists) = prp_rig(LIST_ENTRIES + 2, 3);
    let bytes = (LIST_ENTRIES as u64 + 2) * PAGE as u64;
    let prp = prp::build(&data, &mut lists, 0, 2, 0, bytes).unwrap();
    assert_eq!(prp.second, lists.pages()[0]);
    assert_eq!(list_entry(&lists, 0, 510), data.pages()[511]);
    assert_eq!(list_entry(&lists, 0, 511), lists.pages()[1]);
    assert_eq!(list_entry(&lists, 1, 0), data.pages()[512]);
    assert_eq!(list_entry(&lists, 1, 1), data.pages()[513]);
    assert_eq!(list_entry(&lists, 1, 2), 0);
    assert_eq!(prp::list_pages_for(LIST_ENTRIES + 1), 2);

    // With one list page it does not fit, and nothing is half built.
    let (data, mut lists) = prp_rig(LIST_ENTRIES + 2, 3);
    assert_eq!(
        prp::build(&data, &mut lists, 0, 1, 0, bytes),
        Err(PrpError::TooLong)
    );
    assert_eq!(list_entry(&lists, 0, 0), 0);
}

#[test]
fn a_transfer_the_data_region_cannot_hold_has_no_prps() {
    let (data, mut lists) = prp_rig(4, 1);
    let end = 4 * PAGE as u64;
    assert_eq!(
        prp::build(&data, &mut lists, 0, 1, 0, 0),
        Err(PrpError::Empty)
    );
    assert_eq!(
        prp::build(&data, &mut lists, 0, 1, 2, 512),
        Err(PrpError::Unaligned)
    );
    assert_eq!(
        prp::build(&data, &mut lists, 0, 1, end - 512, 513),
        Err(PrpError::OutsideData)
    );
    assert_eq!(
        prp::build(&data, &mut lists, 0, 1, u64::MAX - 3, 8),
        Err(PrpError::OutsideData)
    );
    assert!(prp::build(&data, &mut lists, 0, 1, end - 512, 512).is_ok());
}

// -- Bring-up ---------------------------------------------------------------

#[test]
fn bring_up_programs_the_admin_queue_before_enabling() {
    let rig = Rig::new(1024, 9);
    let driver = rig.up();
    let model = rig.model.borrow();
    // AQA, ASQ and ACQ are written while the controller is disabled, and
    // CC.EN is set only after them, with 64- and 16-byte entries.
    let enable = model
        .register_writes
        .iter()
        .position(|write| write.0 == regs::CC && write.1 & 1 != 0)
        .expect("enabled");
    for register in [
        regs::AQA,
        regs::ASQ,
        regs::ASQ + 4,
        regs::ACQ,
        regs::ACQ + 4,
    ] {
        let at = model
            .register_writes
            .iter()
            .position(|write| write.0 == register)
            .expect("written");
        assert!(at < enable, "register {register:#x} before CC.EN");
    }
    assert_eq!(model.cc, (6 << 16) | (4 << 20) | 1);
    assert_eq!(model.aqa, (31 << 16) | 31);
    assert_ne!(model.asq, model.acq);
    // The I/O queue pair: completion queue first, both of 64 entries,
    // interrupting through vector 0 of the one vector bound.
    let created: Vec<u8> = model
        .seen
        .iter()
        .filter(|seen| matches!(seen.opcode, 0x01 | 0x05))
        .map(|seen| seen.opcode)
        .collect();
    assert_eq!(created, [0x05, 0x01]);
    let cq = model.cq(1).expect("made");
    assert_eq!((cq.entries, cq.vector, cq.interrupts), (64, 0, true));
    assert_eq!(model.sq(1).expect("made").entries, 64);
    assert_eq!(driver.io_queues(), 1);
    assert_eq!(driver.entries(), 64);
    let controller = driver.controller();
    assert_eq!(&controller.serial, b"MODEL-SERIAL-0001   ");
    assert_eq!(controller.version_pair(), (1, 4));
    assert_eq!(controller.abort_limit, 3);
}

#[test]
fn a_controller_firmware_left_enabled_is_disabled_first() {
    let rig = Rig::new(1024, 9);
    {
        let mut model = rig.model.borrow_mut();
        model.cc = 1 | (6 << 16) | (4 << 20);
        model.csts = 1;
    }
    let _driver = rig.up();
    let model = rig.model.borrow();
    assert_eq!(model.resets, 1);
    let first = model
        .register_writes
        .iter()
        .find(|write| write.0 == regs::CC)
        .expect("written");
    assert_eq!(first.1 & 1, 0, "the first write to CC clears EN");
}

#[test]
fn both_logical_block_sizes_are_read_from_the_format_in_use() {
    for (shift, bytes) in [(9, 512), (12, 4096)] {
        let rig = Rig::new(256, shift);
        let driver = rig.up();
        let namespace = driver.namespace().expect("found");
        assert_eq!(namespace.id, 1);
        assert_eq!(namespace.blocks, 256);
        assert_eq!(namespace.block_bytes(), bytes);
        // 128 KiB a request, the default bound, MDTS 7 allowing 512 KiB.
        assert_eq!(driver.max_transfer_blocks(), (128 * 1024) >> shift);
    }
}

#[test]
fn the_transfer_limit_follows_mdts() {
    let rig = Rig::new(256, 9);
    rig.model.borrow_mut().mdts = 3;
    let driver = rig.up();
    // 2^3 pages of 4 KiB.
    assert_eq!(driver.max_transfer_blocks(), 64);
}

#[test]
fn a_namespace_this_driver_cannot_serve_is_refused() {
    let rig = Rig::new(256, 9);
    rig.model.borrow_mut().metadata = 8;
    let options = Options::default();
    let mut driver = Driver::enable(rig.parts(&options), options).expect("enabled");
    assert_eq!(
        driver.bring_up(),
        Err(InitError::Namespace(NamespaceRefused::Metadata(8)))
    );
    let rig = Rig::new(256, 9);
    rig.model.borrow_mut().protection = 1;
    let mut driver = Driver::enable(rig.parts(&options), options).expect("enabled");
    assert_eq!(
        driver.bring_up(),
        Err(InitError::Namespace(NamespaceRefused::Protected))
    );
    let rig = Rig::new(4, 13);
    let mut driver = Driver::enable(rig.parts(&options), options).expect("enabled");
    assert_eq!(
        driver.bring_up(),
        Err(InitError::Namespace(NamespaceRefused::BlockSize(13)))
    );
}

#[test]
fn a_controller_that_never_becomes_ready_fails_within_its_timeout() {
    let rig = Rig::new(64, 9);
    rig.model.borrow_mut().stuck = true;
    let options = Options::default();
    let failed = Driver::enable(rig.parts(&options), options).expect_err("stuck");
    assert_eq!(failed.error, InitError::EnableTimeout);
    // It never said ready, so with CC.EN cleared again it reads disabled.
    assert!(failed.disabled);
    // CAP.TO is 4: two seconds, and not much more.
    let waited = rig.clock.get();
    assert!((2_000_000_000..2_100_000_000).contains(&waited), "{waited}");
}

#[test]
fn a_controller_without_the_nvm_command_set_or_4k_pages_is_refused() {
    let options = Options::default();
    let rig = Rig::new(64, 9);
    rig.model.borrow_mut().cap &= !(1 << 37);
    let failed = Driver::enable(rig.parts(&options), options).expect_err("no NVM");
    assert_eq!(failed.error, InitError::NoNvmCommandSet);
    let rig = Rig::new(64, 9);
    rig.model.borrow_mut().cap |= 1 << 48;
    let failed = Driver::enable(rig.parts(&options), options).expect_err("8 KiB pages");
    assert_eq!(failed.error, InitError::PageSize);
    assert!(rig.model.borrow().register_writes.is_empty());
}

#[test]
fn the_queues_asked_for_are_bounded_by_what_is_granted() {
    let rig = Rig::new(1024, 9);
    rig.model.borrow_mut().granted_queues = 2;
    let options = Options {
        io_queues: 3,
        vectors: 2,
        ..Options::default()
    };
    let mut driver = rig.up_with(options);
    assert_eq!(driver.io_queues(), 2);
    {
        let model = rig.model.borrow();
        // Asked for three of each, zero based.
        assert!(model.features.contains(&(0x07, (2 << 16) | 2)));
        assert_eq!(model.cq(1).expect("made").vector, 1);
        assert_eq!(model.cq(2).expect("made").vector, 0);
        assert!(model.cq(3).is_none());
    }
    // Requests go to the queues in turn, and both complete.
    for id in 0..6 {
        assert_eq!(
            driver.submit(&read(id, id, 1, id * 512)),
            Ok(Submitted::Queued)
        );
    }
    let queues: Vec<u16> = rig
        .model
        .borrow()
        .io_seen(0x02)
        .iter()
        .map(|seen| seen.queue)
        .collect();
    assert_eq!(queues, [1, 2, 1, 2, 1, 2]);
    let mut done: Vec<u64> = drain(&mut driver).iter().map(|done| done.id).collect();
    done.sort_unstable();
    assert_eq!(done, [0, 1, 2, 3, 4, 5]);
}

// -- Requests ---------------------------------------------------------------

#[test]
fn reads_and_writes_move_the_bytes_whatever_the_size_and_offset() {
    for shift in [9_u8, 12] {
        let rig = Rig::new(4096 >> (shift - 9), shift);
        let mut driver = rig.up();
        let block = 1_usize << shift;
        // One block, two pages' worth, and enough for a list, each from an
        // offset that puts the first PRP inside a page.
        let most = driver.max_transfer_blocks();
        for (id, blocks, offset) in [(1_u64, 1_u32, 0_u64), (2, 2, 512), (3, most, 4096 + 1024)] {
            let bytes = blocks as usize * block;
            let lba = id * 3;
            // Write a pattern, read it back somewhere else in the region.
            let mut parts_view = Mem::new(&rig.model, 0x4000_0000, DATA_PAGES);
            parts_view.fill(offset as usize, bytes, id as u8);
            assert_eq!(
                driver.submit(&write(id, lba, blocks, offset)),
                Ok(Submitted::Queued)
            );
            assert_eq!(drain(&mut driver), [ok(id, bytes as u64)]);
            let start = (lba as usize) << shift;
            let written = parts_view.bytes(offset as usize, bytes);
            assert_eq!(rig.model.borrow().disk[start..start + bytes], written[..]);
            let back = 64 * PAGE as u64 - offset;
            assert_eq!(
                driver.submit(&read(id + 100, lba, blocks, back)),
                Ok(Submitted::Queued)
            );
            assert_eq!(drain(&mut driver), [ok(id + 100, bytes as u64)]);
            assert_eq!(
                parts_view.bytes(back as usize, bytes),
                parts_view.bytes(offset as usize, bytes)
            );
        }
        // The largest went through a PRP list.
        let lists = list_pages_needed(1, 1);
        let list_memory = Mem::new(&rig.model, 0x2000_0000, lists);
        let large = rig.model.borrow().io_seen(0x01)[2];
        assert!(list_memory.pages().contains(&large.prp2));
    }
}

#[test]
fn a_flush_is_a_command_only_when_there_is_a_cache_to_flush() {
    let rig = Rig::new(64, 9);
    let mut driver = rig.up();
    assert!(driver.write_cache());
    assert!(rig.model.borrow().features.contains(&(0x06, 1)));
    assert_eq!(driver.submit(&flush(9)), Ok(Submitted::Queued));
    assert_eq!(drain(&mut driver), [ok(9, 0)]);
    let seen = rig.model.borrow().io_seen(0x00);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].nsid, 1);

    let rig = Rig::new(64, 9);
    rig.model.borrow_mut().write_cache = false;
    let mut driver = rig.up();
    assert!(!driver.write_cache());
    assert_eq!(driver.submit(&flush(9)), Ok(Submitted::Completed(ok(9, 0))));
    assert!(rig.model.borrow().io_seen(0x00).is_empty());
    assert!(!rig.model.borrow().features.iter().any(|set| set.0 == 0x06));
}

#[test]
fn force_unit_access_is_said_in_the_write_and_nowhere_else() {
    let rig = Rig::new(64, 9);
    let mut driver = rig.up();
    let forced = Request {
        fua: true,
        ..write(1, 4, 2, 0)
    };
    assert_eq!(driver.submit(&forced), Ok(Submitted::Queued));
    assert_eq!(driver.submit(&write(2, 8, 2, 4096)), Ok(Submitted::Queued));
    let writes = rig.model.borrow().io_seen(0x01);
    assert_eq!(writes[0].dwords[2], 1 | (1 << 30));
    assert_eq!(writes[1].dwords[2], 1);
    assert_eq!(drain(&mut driver).len(), 2);
}

#[test]
fn requests_the_namespace_or_region_cannot_serve_are_refused_unsent() {
    let rig = Rig::new(64, 9);
    let mut driver = rig.up();
    let region = (DATA_PAGES * PAGE) as u64;
    assert_eq!(driver.submit(&read(1, 0, 0, 0)), Err(SubmitError::Empty));
    assert_eq!(
        driver.submit(&read(1, 0, 257, 0)),
        Err(SubmitError::TooLarge)
    );
    assert_eq!(
        driver.submit(&read(1, 63, 2, 0)),
        Err(SubmitError::OutOfRange)
    );
    assert_eq!(
        driver.submit(&read(1, u64::MAX, 2, 0)),
        Err(SubmitError::OutOfRange)
    );
    assert_eq!(
        driver.submit(&read(1, 0, 1, 2)),
        Err(SubmitError::Unaligned)
    );
    assert_eq!(
        driver.submit(&read(1, 0, 2, region - 512)),
        Err(SubmitError::OutsideData)
    );
    assert!(rig.model.borrow().io_seen(0x02).is_empty());
    assert_eq!(driver.in_flight(), 0);
    assert_eq!(
        driver.submit(&read(1, 62, 2, region - 1024)),
        Ok(Submitted::Queued)
    );
}

#[test]
fn a_full_queue_says_busy_and_takes_more_once_drained() {
    let rig = Rig::new(1024, 9);
    let mut driver = rig.up();
    // The model completes at once but the driver has not drained: slots
    // stay taken. 63 fit in 64 entries.
    for id in 0..63 {
        assert_eq!(
            driver.submit(&read(id, id, 1, id * 512)),
            Ok(Submitted::Queued),
            "{id}"
        );
    }
    assert_eq!(driver.submit(&read(99, 0, 1, 0)), Err(SubmitError::Busy));
    assert_eq!(driver.in_flight(), 63);
    assert_eq!(drain(&mut driver).len(), 63);
    assert_eq!(driver.in_flight(), 0);
    // Several times round both queues, so head, tail and phase all wrap.
    for id in 100..400 {
        assert_eq!(
            driver.submit(&read(id, id % 1000, 1, 0)),
            Ok(Submitted::Queued)
        );
        assert_eq!(drain(&mut driver), [ok(id, 512)]);
    }
    assert_eq!(driver.stats().spurious, 0);
}

#[test]
fn a_failed_command_is_retried_unless_the_controller_says_not_to() {
    let rig = Rig::new(64, 9);
    let mut driver = rig.up();
    // Namespace Not Ready without do-not-retry, then success.
    rig.model.borrow_mut().fail_io = vec![0x82];
    assert_eq!(driver.submit(&read(1, 0, 1, 0)), Ok(Submitted::Queued));
    assert_eq!(drain(&mut driver), [ok(1, 512)]);
    assert_eq!(driver.stats().retries, 1);
    assert_eq!(rig.model.borrow().io_seen(0x02).len(), 2);
    let ids: Vec<u16> = rig
        .model
        .borrow()
        .io_seen(0x02)
        .iter()
        .map(|seen| seen.id)
        .collect();
    assert_ne!(ids[0], ids[1], "sent again under a new id");

    // Unrecovered Read Error with do-not-retry: failed at once.
    rig.model.borrow_mut().fail_io = vec![0x4281];
    assert_eq!(driver.submit(&read(2, 0, 1, 0)), Ok(Submitted::Queued));
    let failed = Completion {
        id: 2,
        outcome: Outcome::IoError,
        bytes: 0,
    };
    assert_eq!(drain(&mut driver), [failed]);
    assert_eq!(driver.stats().last_error.code(), 0x281);

    // Retry allowed every time: given up after the limit.
    rig.model.borrow_mut().fail_io = vec![0x82; usize::from(MAX_RETRIES) + 1];
    assert_eq!(driver.submit(&read(3, 0, 1, 0)), Ok(Submitted::Queued));
    assert_eq!(drain(&mut driver)[0].outcome, Outcome::IoError);
    assert!(rig.model.borrow().fail_io.is_empty());

    // An opcode the controller does not know is unsupported, not an error.
    rig.model.borrow_mut().fail_io = vec![0x4001];
    assert_eq!(driver.submit(&flush(4)), Ok(Submitted::Queued));
    assert_eq!(drain(&mut driver)[0].outcome, Outcome::Unsupported);
}

// -- When a command does not come back ----------------------------------------

const SECOND: u64 = 1_000_000_000;

#[test]
fn a_lost_command_is_aborted_and_then_the_controller_is_reset() {
    let rig = Rig::new(64, 9);
    let mut driver = rig.up();
    rig.model.borrow_mut().drop_io = 1;
    let mut view = Mem::new(&rig.model, 0x4000_0000, DATA_PAGES);
    view.fill(0, 512, 0x5A);
    assert_eq!(driver.submit(&write(7, 5, 1, 0)), Ok(Submitted::Queued));
    // First look: the command is seen, nothing else.
    assert_eq!(driver.check(), Ok(None));
    rig.advance(29 * SECOND);
    assert_eq!(driver.check(), Ok(None));
    assert!(rig.model.borrow().admin_seen(0x08).is_empty());
    // Past the timeout: aborted, by queue and command id.
    rig.advance(2 * SECOND);
    assert_eq!(driver.check(), Ok(Some(Event::Aborted(1))));
    let aborts = rig.model.borrow().admin_seen(0x08);
    let lost = rig.model.borrow().io_seen(0x01)[0];
    assert_eq!(aborts.len(), 1);
    assert_eq!(aborts[0].dwords[0], (u32::from(lost.id) << 16) | 1);
    // The model's Abort does nothing. Within the grace nothing more
    // happens, and no second Abort is sent.
    rig.advance(29 * SECOND);
    assert_eq!(driver.check(), Ok(None));
    assert_eq!(rig.model.borrow().admin_seen(0x08).len(), 1);
    assert_eq!(rig.model.borrow().resets, 0);
    // Past it: the controller is reset, set up again, and the write sent
    // again, which now completes.
    rig.advance(2 * SECOND);
    assert_eq!(
        driver.check(),
        Ok(Some(Event::Reset {
            cause: Cause::Timeout,
            requeued: 1,
            failed: 0
        }))
    );
    assert!(driver.wants_drain());
    assert_eq!(rig.model.borrow().resets, 1);
    assert_eq!(rig.model.borrow().cc & 1, 1);
    assert_eq!(drain(&mut driver), [ok(7, 512)]);
    let written = view.bytes(0, 512);
    assert_eq!(rig.model.borrow().disk[5 * 512..6 * 512], written[..]);
    assert_eq!(driver.stats().resets, 1);
    assert_eq!(driver.stats().aborts, 1);
    assert_eq!(driver.in_flight(), 0);
}

#[test]
fn a_command_the_abort_brings_back_is_sent_again_without_a_reset() {
    let rig = Rig::new(64, 9);
    let mut driver = rig.up();
    {
        let mut model = rig.model.borrow_mut();
        model.drop_io = 1;
        model.abort_works = true;
    }
    assert_eq!(driver.submit(&read(7, 5, 1, 0)), Ok(Submitted::Queued));
    assert_eq!(driver.check(), Ok(None));
    rig.advance(31 * SECOND);
    assert_eq!(driver.check(), Ok(Some(Event::Aborted(1))));
    // The aborted command's completion is there with no interrupt: the
    // second look after it says to drain, and the drain sends the read
    // again.
    assert_eq!(driver.check(), Ok(None));
    assert_eq!(driver.check(), Ok(Some(Event::MissedInterrupt)));
    assert!(driver.wants_drain());
    assert_eq!(drain(&mut driver), [ok(7, 512)]);
    assert_eq!(rig.model.borrow().resets, 0);
    assert_eq!(driver.stats().retries, 1);
}

#[test]
fn a_fatal_controller_is_reset_and_its_requests_sent_again() {
    let rig = Rig::new(64, 9);
    let mut driver = rig.up();
    rig.model.borrow_mut().fatal_on_io = true;
    assert_eq!(driver.submit(&read(1, 0, 1, 0)), Ok(Submitted::Queued));
    assert_eq!(driver.submit(&read(2, 1, 1, 512)), Ok(Submitted::Queued));
    assert_eq!(
        driver.check(),
        Ok(Some(Event::Reset {
            cause: Cause::ControllerFatal,
            requeued: 2,
            failed: 0
        }))
    );
    let mut done = drain(&mut driver);
    done.sort_by_key(|done| done.id);
    assert_eq!(done, [ok(1, 512), ok(2, 512)]);
    // The completion of the first life's second read was written before
    // the reset; the queues were cleared, so it is not taken twice.
    assert_eq!(driver.stats().spurious, 0);
}

#[test]
fn a_request_lost_too_often_is_failed_and_a_controller_reset_too_often_given_up() {
    let rig = Rig::new(64, 9);
    let options = Options {
        io_timeout_nanos: SECOND,
        abort_grace_nanos: SECOND,
        ..Options::default()
    };
    let mut driver = rig.up_with(options);
    rig.model.borrow_mut().drop_io = 100;
    assert_eq!(driver.submit(&read(1, 0, 1, 0)), Ok(Submitted::Queued));
    let mut resets = 0;
    let fatal = loop {
        rig.advance(SECOND);
        match driver.check() {
            Ok(Some(Event::Reset { .. })) => resets += 1,
            Ok(_) => {}
            Err(fatal) => break fatal,
        }
        assert!(resets < 10, "it must give up");
    };
    // Three resets in a row with nothing succeeding, and the fourth is not
    // tried.
    assert_eq!(resets, 3);
    assert_eq!(fatal, Fatal::ResetLimit);
    assert_eq!(driver.check(), Err(Fatal::Dead));
    assert_eq!(driver.submit(&read(2, 0, 1, 0)), Err(SubmitError::Dead));
    // The process takes it down: the request it held is handed back.
    let Shutdown::Released(released) = driver.shutdown() else {
        panic!("the model disables");
    };
    assert_eq!(released.abandoned().collect::<Vec<_>>(), [1]);
}

#[test]
fn a_success_between_resets_starts_the_count_again() {
    let rig = Rig::new(64, 9);
    let mut driver = rig.up();
    for round in 0..5 {
        rig.model.borrow_mut().fatal_on_io = true;
        assert_eq!(driver.submit(&read(round, 0, 1, 0)), Ok(Submitted::Queued));
        assert!(matches!(driver.check(), Ok(Some(Event::Reset { .. }))));
        assert_eq!(drain(&mut driver), [ok(round, 512)]);
    }
    assert_eq!(driver.stats().resets, 5);
}

#[test]
fn a_controller_that_left_the_bus_is_fatal_at_once() {
    let rig = Rig::new(64, 9);
    let mut driver = rig.up();
    assert_eq!(driver.submit(&read(1, 0, 1, 0)), Ok(Submitted::Queued));
    rig.model.borrow_mut().gone = true;
    assert_eq!(driver.check(), Err(Fatal::Gone));
    let writes = rig.model.borrow().register_writes.len();
    // Nothing is written to a device that is not there, and its memory is
    // kept: it was never seen disabled.
    assert!(matches!(driver.shutdown(), Shutdown::Wedged(_)));
    assert_eq!(rig.model.borrow().register_writes.len(), writes);
}

#[test]
fn completions_no_interrupt_announced_are_found_by_the_watch() {
    let rig = Rig::new(64, 9);
    let mut driver = rig.up();
    assert_eq!(driver.submit(&read(1, 0, 1, 0)), Ok(Submitted::Queued));
    // The completion is there. At the first look its interrupt may be on
    // its way, so nothing is said and nothing taken.
    assert_eq!(driver.check(), Ok(None));
    assert!(!driver.wants_drain());
    // Still there at the next look, with no interrupt between: lost.
    assert_eq!(driver.check(), Ok(Some(Event::MissedInterrupt)));
    assert!(driver.wants_drain());
    assert_eq!(drain(&mut driver), [ok(1, 512)]);
    assert!(!driver.wants_drain());
    assert_eq!(driver.stats().missed_interrupts, 1);

    // One an interrupt did announce, between two looks, is not a miss.
    assert_eq!(driver.submit(&read(2, 0, 1, 0)), Ok(Submitted::Queued));
    assert_eq!(driver.check(), Ok(None));
    assert_eq!(drain(&mut driver), [ok(2, 512)]);
    assert_eq!(driver.submit(&read(3, 0, 1, 0)), Ok(Submitted::Queued));
    assert_eq!(driver.check(), Ok(None));
    assert_eq!(driver.stats().missed_interrupts, 1);
}

// -- The host memory buffer ---------------------------------------------------

#[test]
fn the_host_memory_wish_is_bounded_by_the_limit_and_the_controller() {
    // Nothing asked for: nothing lent.
    assert_eq!(Wish::of(0, 0, 0, 0, 1000), None);
    // 200 MiB preferred, 16 MiB at least: the limit decides.
    let wish = Wish::of(51200, 4096, 0, 0, 32768).expect("wished");
    assert_eq!(wish.preferred_pages, 32768);
    assert_eq!(wish.minimum_pages, 4096);
    assert_eq!(wish.min_chunk_pages, 1);
    assert_eq!(wish.max_chunks, 256);
    // A minimum above the limit cannot be met: no buffer.
    assert_eq!(Wish::of(51200, 40000, 0, 0, 32768), None);
    // Its bounds on the pieces are kept.
    let wish = Wish::of(8192, 2048, 512, 8, 32768).expect("wished");
    assert_eq!((wish.min_chunk_pages, wish.max_chunks), (512, 8));
    let piece = |pages| Chunk {
        address: 0x10_0000,
        pages,
    };
    assert!(wish.accepts(&[piece(1024), piece(1024)]));
    assert!(!wish.accepts(&[piece(1024)]), "below the minimum");
    assert!(
        !wish.accepts(&[piece(2048), piece(256)]),
        "a piece too small"
    );
    assert!(!wish.accepts(&[piece(512); 9]), "too many pieces");
    assert!(!wish.accepts(&[]));
    let odd = Chunk {
        address: 0x10_0800,
        pages: 4096,
    };
    assert!(!wish.accepts(&[odd]), "not page aligned");
}

fn host_memory_rig() -> (Rig, TestDriver, [Chunk; 2]) {
    let rig = Rig::new(64, 9);
    {
        let mut model = rig.model.borrow_mut();
        model.hmpre = 2048;
        model.hmmin = 1024;
    }
    let options = Options::default();
    let mut driver = Driver::enable(rig.parts(&options), options).expect("enabled");
    let wish = driver.host_memory_wish().expect("wished");
    assert_eq!((wish.preferred_pages, wish.minimum_pages), (2048, 1024));
    let chunks = [
        Chunk {
            address: 0x8000_0000,
            pages: 1024,
        },
        Chunk {
            address: 0x9000_0000,
            pages: 512,
        },
    ];
    assert!(driver.lend_host_memory(&chunks));
    driver.bring_up().expect("brought up");
    (rig, driver, chunks)
}

#[test]
fn host_memory_is_described_to_the_controller_and_taken_back_before_shutdown() {
    let (rig, driver, chunks) = host_memory_rig();
    assert_eq!(driver.host_memory_pages(), 1536);
    {
        let model = rig.model.borrow();
        let lent = model.host_memory.clone().expect("set");
        assert_eq!(
            lent.flags, 1,
            "enable, and not memory return the first time"
        );
        assert_eq!(lent.pages, 1536);
        assert_eq!(
            lent.chunks,
            [(chunks[0].address, 1024), (chunks[1].address, 512)]
        );
        // Before the I/O queues exist.
        let set = model
            .seen
            .iter()
            .position(|seen| seen.opcode == 0x09 && seen.dwords[0] == 0x0D)
            .expect("set");
        let queue = model
            .seen
            .iter()
            .position(|seen| seen.opcode == 0x05)
            .expect("made");
        assert!(set < queue);
    }
    let Shutdown::Released(released) = driver.shutdown() else {
        panic!("the model disables");
    };
    assert!(released.clean);
    let model = rig.model.borrow();
    // Host memory disabled, then the shutdown notification, then EN clear.
    let disable = model
        .seen
        .iter()
        .rposition(|seen| seen.opcode == 0x09 && seen.dwords[..2] == [0x0D, 0])
        .expect("taken back");
    assert_eq!(disable, model.seen.len() - 1, "the last command of all");
    assert_eq!(model.shutdowns, 1);
}

#[test]
fn host_memory_is_given_back_after_a_reset_as_the_same_memory() {
    let (rig, mut driver, _) = host_memory_rig();
    rig.model.borrow_mut().fatal_on_io = true;
    assert_eq!(driver.submit(&read(1, 0, 1, 0)), Ok(Submitted::Queued));
    assert!(matches!(driver.check(), Ok(Some(Event::Reset { .. }))));
    let lent = rig.model.borrow().host_memory.clone().expect("set again");
    assert_eq!(lent.flags, 3, "enable and memory return");
    assert_eq!(lent.pages, 1536);
    assert_eq!(drain(&mut driver), [ok(1, 512)]);
}

#[test]
fn a_controller_works_without_the_host_memory_it_asked_for() {
    // Too little to lend: refused here, and nothing is sent.
    let rig = Rig::new(64, 9);
    rig.model.borrow_mut().hmpre = 2048;
    rig.model.borrow_mut().hmmin = 1024;
    let options = Options::default();
    let mut driver = Driver::enable(rig.parts(&options), options).expect("enabled");
    let small = Chunk {
        address: 0x8000_0000,
        pages: 512,
    };
    assert!(!driver.lend_host_memory(&[small]));
    driver.bring_up().expect("brought up");
    assert_eq!(driver.host_memory_pages(), 0);
    assert!(!rig.model.borrow().features.iter().any(|set| set.0 == 0x0D));
    assert_eq!(driver.submit(&read(1, 0, 1, 0)), Ok(Submitted::Queued));
    assert_eq!(drain(&mut driver), [ok(1, 512)]);

    // Lent, and the controller refuses the feature: still up, none in use.
    let rig = Rig::new(64, 9);
    {
        let mut model = rig.model.borrow_mut();
        model.hmpre = 2048;
        model.hmmin = 1024;
        model.refuse_host_memory = true;
    }
    let mut driver = Driver::enable(rig.parts(&options), options).expect("enabled");
    let enough = Chunk {
        address: 0x8000_0000,
        pages: 2048,
    };
    assert!(driver.lend_host_memory(&[enough]));
    driver.bring_up().expect("brought up");
    assert_eq!(driver.host_memory_pages(), 0);
    assert_eq!(driver.submit(&read(1, 0, 1, 0)), Ok(Submitted::Queued));
    assert_eq!(drain(&mut driver), [ok(1, 512)]);
}

// -- Shutdown ---------------------------------------------------------------

#[test]
fn shutdown_notifies_the_controller_before_disabling_it() {
    let rig = Rig::new(64, 9);
    let mut driver = rig.up();
    rig.model.borrow_mut().drop_io = 2;
    assert_eq!(driver.submit(&read(11, 0, 1, 0)), Ok(Submitted::Queued));
    assert_eq!(driver.submit(&read(12, 1, 1, 512)), Ok(Submitted::Queued));
    let Shutdown::Released(released) = driver.shutdown() else {
        panic!("the model disables");
    };
    assert!(released.clean);
    let mut abandoned: Vec<u64> = released.abandoned().collect();
    abandoned.sort_unstable();
    assert_eq!(abandoned, [11, 12]);
    let model = rig.model.borrow();
    let config: Vec<u32> = model
        .register_writes
        .iter()
        .filter(|write| write.0 == regs::CC)
        .map(|write| write.1)
        .collect();
    let [.., notify, disable] = config[..] else {
        panic!("two writes at least");
    };
    assert_eq!(notify & regs::CC_SHN_MASK, regs::CC_SHN_NORMAL);
    assert_eq!(notify & 1, 1, "notified while still enabled");
    assert_eq!(disable & 1, 0);
    assert_eq!(disable & regs::CC_SHN_MASK, 0);
    assert_eq!(model.csts & 1, 0);
}

#[test]
fn a_controller_that_will_not_disable_keeps_its_memory() {
    let rig = Rig::new(64, 9);
    let driver = rig.up();
    rig.model.borrow_mut().stuck = true;
    assert!(matches!(driver.shutdown(), Shutdown::Wedged(_)));
}

// -- Quirks -----------------------------------------------------------------

#[test]
fn quirks_are_found_by_pci_identity_and_by_model() {
    assert_eq!(quirks::for_device(0x15B7, 0x5009), Quirks::BROKEN_MSI);
    assert_eq!(quirks::for_device(0x15B7, 0x5003), Quirks::NONE);
    assert_eq!(
        quirks::for_device(0x144D, 0xA822),
        Quirks::DELAY_BEFORE_READY
    );
    assert_eq!(quirks::for_device(0x106B, 0x2005), Quirks::UNSUPPORTED);
    assert_eq!(quirks::for_device(0x1B36, 0x0010), Quirks::NONE);
    assert_eq!(
        quirks::for_model(0x144D, b"Samsung Portable SSD X5                 "),
        Quirks::DELAY_BEFORE_READY
    );
    assert_eq!(
        quirks::for_model(0x144D, b"Samsung SSD 970 EVO Plus 1TB            "),
        Quirks::NONE
    );
}

#[test]
fn each_quirk_changes_what_the_driver_does() {
    // The delay: 2.3 s before each look at CSTS.RDY, disable and enable.
    let rig = Rig::new(64, 9);
    let _driver = rig.up_with(Options {
        quirks: Quirks::DELAY_BEFORE_READY,
        ..Options::default()
    });
    assert!(rig.clock.get() >= 2 * 2_300_000_000);

    // Without it, bring-up waits for nothing on a controller that answers.
    let rig = Rig::new(64, 9);
    let _driver = rig.up();
    assert!(rig.clock.get() < 1_000_000);

    // Identify answers CNS 0 and 1 only: the list is never asked for.
    let rig = Rig::new(64, 9);
    let driver = rig.up_with(Options {
        quirks: Quirks::IDENTIFY_CNS,
        ..Options::default()
    });
    assert_eq!(driver.namespace().expect("found").id, 1);
    let asked: Vec<u32> = rig
        .model
        .borrow()
        .admin_seen(0x06)
        .iter()
        .map(|seen| seen.dwords[0])
        .collect();
    assert_eq!(asked, [1, 0]);
    let rig = Rig::new(64, 9);
    let _driver = rig.up();
    let asked: Vec<u32> = rig
        .model
        .borrow()
        .admin_seen(0x06)
        .iter()
        .map(|seen| seen.dwords[0])
        .collect();
    assert_eq!(asked, [1, 2, 0]);

    // One command at a time.
    let rig = Rig::new(64, 9);
    let mut driver = rig.up_with(Options {
        quirks: Quirks::QUEUE_DEPTH_ONE,
        ..Options::default()
    });
    assert_eq!(driver.entries(), 2);
    assert_eq!(driver.submit(&read(1, 0, 1, 0)), Ok(Submitted::Queued));
    assert_eq!(driver.submit(&read(2, 0, 1, 0)), Err(SubmitError::Busy));
    assert_eq!(drain(&mut driver), [ok(1, 512)]);
    assert_eq!(driver.submit(&read(2, 0, 1, 0)), Ok(Submitted::Queued));

    // Not NVMe as specified: not touched.
    let rig = Rig::new(64, 9);
    let options = Options {
        quirks: Quirks::UNSUPPORTED,
        ..Options::default()
    };
    let failed = Driver::enable(rig.parts(&options), options).expect_err("refused");
    assert_eq!(failed.error, InitError::Unsupported);
    assert!(rig.model.borrow().register_writes.is_empty());

    // The model's quirk is found from Identify, after the first enable.
    let rig = Rig::new(64, 9);
    rig.model.borrow_mut().model_name = b"Samsung Portable SSD X5";
    rig.model.borrow_mut().vendor = 0x144D;
    let options = Options::default();
    let driver = Driver::enable(rig.parts(&options), options).expect("enabled");
    assert!(driver.quirks().contains(Quirks::DELAY_BEFORE_READY));
}

#[test]
fn power_management_is_switched_off_with_a_table_to_read() {
    let rig = Rig::new(64, 9);
    let _driver = rig.up();
    let model = rig.model.borrow();
    let apst: Vec<_> = model
        .seen
        .iter()
        .filter(|seen| seen.opcode == 0x09 && seen.dwords[0] == 0x0C)
        .collect();
    assert_eq!(apst.len(), 1);
    assert_eq!(apst[0].dwords[1], 0, "APSTE clear");
    assert_ne!(apst[0].prp1, 0);
    // The power state itself is never set.
    assert!(!model.features.iter().any(|set| set.0 == 0x02));
    drop(model);

    let rig = Rig::new(64, 9);
    rig.model.borrow_mut().apst = false;
    let _driver = rig.up();
    assert!(!rig.model.borrow().features.iter().any(|set| set.0 == 0x0C));
}

#[test]
fn the_memory_given_must_hold_the_queues_and_lists() {
    let rig = Rig::new(64, 9);
    let options = Options::default();
    let mut parts = rig.parts(&options);
    parts.lists = Mem::new(&rig.model, 0x2000_0000, MAX_ENTRIES);
    let failed = Driver::enable(parts, options).expect_err("too small");
    assert_eq!(failed.error, InitError::Memory);
    assert!(rig.model.borrow().register_writes.is_empty());
    let _ = Regs(Rc::clone(&rig.model)).read(0);
}
