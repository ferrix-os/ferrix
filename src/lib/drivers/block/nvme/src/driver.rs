//! The controller's life: reset and enable, Identify, the queues, requests,
//! timeouts, and the clean shutdown.
//!
//! # Bring-up (NVMe Base Specification 2.0, section 3.5.1)
//!
//! [`Driver::enable`] takes whatever state firmware or a dead driver left:
//! it clears `CC.EN` and waits for `CSTS.RDY` to fall, programs the admin
//! queue, sets `CC.EN` with 4 KiB pages and 64- and 16-byte entries, waits
//! for `CSTS.RDY`, reads Identify Controller and asks for I/O queues.
//! The process then lends host memory if the controller asked for some
//! ([`Driver::host_memory_wish`]), and [`Driver::bring_up`] finishes: host
//! memory, the volatile write cache, power management, the namespace, and
//! the I/O queues.
//!
//! # Requests
//!
//! Each I/O queue has a slot per entry. [`Driver::submit`] takes a free
//! slot and [`Driver::drain`] gives it back; a command's id is its slot
//! and the slot's generation, so a completion for a command that was
//! aborted or sent again is told from the one waited for. A slot whose
//! command is not in the submission queue yet -- the queue was full, or
//! the command is to be sent again -- waits, and is sent as room appears.
//!
//! # When a command does not come back
//!
//! [`Driver::check`] is called while requests are in flight and nothing has
//! been heard for a while:
//!
//! 1. A controller whose registers read all ones has left the bus: fatal.
//! 2. `CSTS.CFS`: the controller is reset, set up again, and every request
//!    in flight is sent again.
//! 3. A completion that was already waiting at the look before, with no
//!    interrupt in between to have it taken: the interrupt was lost (or,
//!    under a hypervisor, the processor it is routed to has not run), and
//!    the caller is told to drain ([`Driver::wants_drain`]). One seen for
//!    the first time is left: its interrupt may be on the way.
//! 4. A command older than the I/O timeout is aborted (Abort, as far as the
//!    controller's abort limit allows). One that the abort did not bring
//!    back within the grace time means the controller is reset, as for 2.
//!
//! A request is sent again at most [`MAX_RETRIES`] times, after a reset or
//! after a completion the controller did not mark do-not-retry; then it is
//! completed as failed. [`MAX_RESETS`] resets in a row without one command
//! succeeding is fatal: the process exits, devmgr starts another, and the
//! kernel's ring keeps the requests (`docs/BLOCK-RING.md`, parking).
//!
//! # Shutdown (section 3.6.1)
//!
//! [`Driver::shutdown`] takes host memory back, writes `CC.SHN` = 01b and
//! waits for `CSTS.SHST` = 10b, so the drive has written out what it holds
//! before power may go, then clears `CC.EN` and waits for `CSTS.RDY` to
//! fall: only a controller seen disabled can no longer reach its memory.

use core::fmt;

use crate::command::{self, Command, Status};
use crate::hmb::{self, Chunk, MAX_CHUNKS, Wish};
use crate::identify::{self, Controller, Namespace, NamespaceRefused};
use crate::prp::{self, LIST_ENTRIES};
use crate::queue::Indices;
use crate::quirks::{self, Quirks, READY_DELAY_NANOS};
use crate::regs::{self, Capabilities, PAGE};
use crate::{Clock, Memory, Registers};

/// The most I/O queue pairs.
pub const MAX_IO_QUEUES: usize = 4;
/// The most entries in a queue: one page of submission entries.
pub const MAX_ENTRIES: usize = PAGE / command::SQE_BYTES;
/// Entries in the admin queue.
const ADMIN_ENTRIES: u16 = 32;
/// The scratch page among the lists.
const SCRATCH_PAGE: usize = 0;
/// The host memory descriptor list's page among the lists.
const HMB_LIST_PAGE: usize = 1;
/// The first PRP list page among the lists.
const FIRST_LIST_PAGE: usize = 2;
/// How often a request is sent again before it is failed.
pub const MAX_RETRIES: u8 = 3;
/// Resets in a row, with no command succeeding between, before the
/// controller is given up.
pub const MAX_RESETS: u8 = 3;
/// The wait between two looks at the admin completion queue.
const ADMIN_POLL_NANOS: u64 = 100_000;
/// The wait between two looks at `CSTS`.
const STATUS_POLL_NANOS: u64 = 1_000_000;
/// The least time a shutdown is given.
const SHUTDOWN_MIN_NANOS: u64 = 5_000_000_000;
/// The most time a shutdown is given, whatever `RTD3E` says.
const SHUTDOWN_MAX_NANOS: u64 = 30_000_000_000;
/// Command ids of the admin commands waited for.
const ADMIN_SYNC: u16 = 0x100;
/// Command ids of the Abort commands, whose completions are only counted.
const ADMIN_ABORT: u16 = 0x200;

/// How the driver is run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Options {
    /// I/O queue pairs wanted, 1 to [`MAX_IO_QUEUES`]; the controller may
    /// grant fewer.
    pub io_queues: u16,
    /// Entries per I/O queue, 2 to [`MAX_ENTRIES`]; bounded by `CAP.MQES`.
    pub entries: u16,
    /// Interrupt vectors the process bound, at least 1. The admin queue
    /// uses vector 0; I/O queue `n` uses vector `n` modulo this.
    pub vectors: u16,
    /// PRP list pages each command slot has.
    pub list_pages: usize,
    /// A command older than this is aborted.
    pub io_timeout_nanos: u64,
    /// An aborted command still missing after this resets the controller.
    pub abort_grace_nanos: u64,
    /// An admin command not answered within this fails.
    pub admin_timeout_nanos: u64,
    /// What this controller needs, by its PCI identity.
    pub quirks: Quirks,
    /// The most host memory lent, in pages.
    pub host_memory_limit_pages: u32,
    /// The most bytes one request moves, whatever the controller allows.
    pub max_transfer_bytes: u32,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            io_queues: 1,
            entries: MAX_ENTRIES as u16,
            vectors: 1,
            list_pages: 1,
            io_timeout_nanos: 30_000_000_000,
            abort_grace_nanos: 30_000_000_000,
            admin_timeout_nanos: 60_000_000_000,
            quirks: Quirks::NONE,
            host_memory_limit_pages: hmb::LIMIT_PAGES,
            max_transfer_bytes: 128 * 1024,
        }
    }
}

/// Pages of list memory a driver with `io_queues` queues needs when each
/// command slot has `list_pages` PRP list pages.
#[must_use]
pub const fn list_pages_needed(io_queues: usize, list_pages: usize) -> usize {
    FIRST_LIST_PAGE + io_queues * MAX_ENTRIES * list_pages
}

/// Pages of queue memory a driver with `io_queues` I/O queues needs.
#[must_use]
pub const fn queue_pages_needed(io_queues: usize) -> usize {
    2 * (1 + io_queues)
}

/// What the driver is made of.
#[derive(Debug)]
pub struct Parts<R, C, Q, L, D> {
    /// The controller's registers.
    pub regs: R,
    /// Time.
    pub clock: C,
    /// The queues' pages: [`queue_pages_needed`] of them.
    pub queues: Q,
    /// The scratch, descriptor and PRP list pages: [`list_pages_needed`].
    pub lists: L,
    /// The block ring's data region.
    pub data: D,
}

/// Why the controller did not come up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InitError {
    /// Its registers read all ones: nothing answers at this address.
    Gone,
    /// It does not offer the NVM command set.
    NoNvmCommandSet,
    /// It cannot run with 4 KiB pages.
    PageSize,
    /// Its identity is one this driver does not drive.
    Unsupported,
    /// `CSTS.RDY` did not fall after `CC.EN` was cleared.
    DisableTimeout,
    /// `CSTS.RDY` did not rise after `CC.EN` was set.
    EnableTimeout,
    /// `CSTS.CFS` came up while enabling.
    Fatal,
    /// The memory given is smaller than the queues or lists need.
    Memory,
    /// An admin command was not answered in time.
    AdminTimeout(u8),
    /// An admin command failed.
    Admin(u8, Status),
    /// 64-byte submission or 16-byte completion entries are outside what
    /// the controller allows.
    EntrySize,
    /// The controller granted no I/O queue.
    NoQueues,
    /// No namespace to serve.
    NoNamespace,
    /// The namespace is not one this driver serves.
    Namespace(NamespaceRefused),
}

/// The controller did not come up: the parts back, and whether it was seen
/// disabled, which alone lets its memory be freed.
#[derive(Debug)]
pub struct Failed<P> {
    /// What the driver was to be made of.
    pub parts: P,
    /// Why.
    pub error: InitError,
    /// Whether `CSTS.RDY` was seen clear after `CC.EN` was cleared, with
    /// nothing enabling it since.
    pub disabled: bool,
}

/// Why the driver stopped for good.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fatal {
    /// It had stopped already.
    Dead,
    /// The controller left the bus.
    Gone,
    /// Too many resets in a row.
    ResetLimit,
    /// A reset did not bring the controller back.
    Reset(InitError),
}

/// What a request does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// Read blocks into the data region.
    Read,
    /// Write blocks from the data region.
    Write,
    /// Make every completed write durable.
    Flush,
}

/// One request, in the namespace's logical blocks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request {
    /// The caller's name for it, handed back on completion.
    pub id: u64,
    /// What to do.
    pub op: Op,
    /// The first logical block. Ignored by a flush.
    pub lba: u64,
    /// How many logical blocks. Ignored by a flush.
    pub blocks: u32,
    /// Where the data starts in the data region. Ignored by a flush.
    pub data_offset: u64,
    /// Force unit access: a write that is durable when it completes.
    pub fua: bool,
}

/// Why a request was not taken.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubmitError {
    /// The driver has stopped.
    Dead,
    /// No slot is free; drain and retry.
    Busy,
    /// A read or write of no blocks.
    Empty,
    /// More blocks than one command may carry.
    TooLarge,
    /// Blocks past the end of the namespace.
    OutOfRange,
    /// Data past the end of the data region.
    OutsideData,
    /// Data that does not start on a four-byte boundary.
    Unaligned,
    /// A write to a write-protected namespace.
    ReadOnly,
}

/// How a request ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Done.
    Ok,
    /// The controller failed it, or it was lost too often.
    IoError,
    /// The controller does not know the command.
    Unsupported,
}

/// A finished request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Completion {
    /// The request's id.
    pub id: u64,
    /// How it went.
    pub outcome: Outcome,
    /// Bytes moved: all of them on success, none otherwise.
    pub bytes: u64,
}

/// What [`Driver::submit`] did with a request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Submitted {
    /// Held; its completion will come from [`Driver::drain`].
    Queued,
    /// Answered without the controller: a flush with no volatile write
    /// cache to flush.
    Completed(Completion),
}

/// What [`Driver::drain`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Drained {
    /// Completions written to the start of the caller's slice.
    pub completions: usize,
    /// More are waiting than the slice had room for.
    pub more: bool,
}

/// Why the controller was reset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cause {
    /// `CSTS.CFS`.
    ControllerFatal,
    /// A command that an abort did not bring back.
    Timeout,
}

/// Something [`Driver::check`] did, for the process to say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// Completions were waiting that no interrupt had announced.
    MissedInterrupt,
    /// This many commands timed out and were aborted.
    Aborted(u32),
    /// The controller was reset.
    Reset {
        /// Why.
        cause: Cause,
        /// Requests sent again.
        requeued: u32,
        /// Requests failed, having been sent too often.
        failed: u32,
    },
}

/// Counts, for the process to say when it ends.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    /// Controller resets after bring-up.
    pub resets: u32,
    /// Abort commands sent.
    pub aborts: u32,
    /// Commands sent again after a completion that allowed it.
    pub retries: u32,
    /// Requests completed as failed.
    pub errors: u32,
    /// Completions naming no command waited for.
    pub spurious: u32,
    /// Times completions were found without an interrupt.
    pub missed_interrupts: u32,
    /// The status of the last command that failed.
    pub last_error: Status,
}

/// Where a slot's request is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// No request.
    Free,
    /// Held, not in the submission queue.
    Waiting,
    /// In the controller.
    Queued,
    /// In the controller, and an Abort was sent for it.
    Aborting,
    /// Finished without a completion entry; [`Driver::drain`] reports it.
    Done(Outcome),
}

/// One command slot of an I/O queue.
#[derive(Clone, Copy, Debug)]
struct Slot {
    state: State,
    request: Request,
    /// Bumped each time the request is sent, so an old completion is told
    /// from the one waited for.
    generation: u8,
    retries: u8,
    /// When [`Driver::check`] first saw it in the controller, or when its
    /// Abort was sent; zero until then.
    since: u64,
}

impl Slot {
    const EMPTY: Slot = Slot {
        state: State::Free,
        request: Request {
            id: 0,
            op: Op::Flush,
            lba: 0,
            blocks: 0,
            data_offset: 0,
            fua: false,
        },
        generation: 0,
        retries: 0,
        since: 0,
    };
}

/// One I/O queue pair.
#[derive(Clone, Copy, Debug)]
struct IoQueue {
    indices: Indices,
    slots: [Slot; MAX_ENTRIES],
}

impl IoQueue {
    const EMPTY: IoQueue = IoQueue {
        indices: Indices::new(2),
        slots: [Slot::EMPTY; MAX_ENTRIES],
    };
}

/// What a look at a slot's age found.
enum Aged {
    /// Nothing to do.
    Young,
    /// Past the I/O timeout: an Abort was sent for it.
    Aborted,
    /// Aborted, and past the grace.
    Expired,
}

/// What became of a waiting slot.
enum Sent {
    /// In the submission queue, whose tail is now this.
    Tail(u16),
    /// Finished as failed without being sent.
    Failed,
    /// The submission queue has no room.
    Full,
}

/// How a driver ended.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "made once, as a driver ends, and there is no heap to box into"
)]
pub enum Shutdown<P> {
    /// The controller was seen disabled: its memory may be freed.
    Released(Released<P>),
    /// It would not disable: its memory must stay pinned.
    Wedged(P),
}

/// A driver taken down, the controller disabled.
#[derive(Debug)]
pub struct Released<P> {
    /// What the driver was made of.
    pub parts: P,
    /// Whether the controller said its shutdown processing was complete.
    pub clean: bool,
    abandoned: [u64; MAX_IO_QUEUES * MAX_ENTRIES],
    count: usize,
}

impl<P> Released<P> {
    /// The requests that were held when the controller was disabled.
    pub fn abandoned(&self) -> impl Iterator<Item = u64> + '_ {
        self.abandoned.iter().copied().take(self.count)
    }
}

/// What [`Driver::enable`] answers: the driver, or the parts back.
pub type Enabled<R, C, Q, L, D> = Result<Driver<R, C, Q, L, D>, Failed<Parts<R, C, Q, L, D>>>;

/// An NVMe controller, enabled.
pub struct Driver<R, C, Q, L, D> {
    parts: Parts<R, C, Q, L, D>,
    options: Options,
    capabilities: Capabilities,
    stride: u32,
    entries: u16,
    admin: Indices,
    admin_sequence: u16,
    io: [IoQueue; MAX_IO_QUEUES],
    io_count: usize,
    next_queue: usize,
    controller: Controller,
    namespace: Option<Namespace>,
    chunks: [Chunk; MAX_CHUNKS],
    chunk_count: usize,
    host_memory_on: bool,
    host_memory_given: bool,
    write_cache: bool,
    aborts_outstanding: u16,
    resets_in_a_row: u8,
    disabled: bool,
    dead: bool,
    wants_drain: bool,
    /// Completions were waiting at the last look, and nothing has taken
    /// them since.
    stale: bool,
    max_blocks: u32,
    stats: Stats,
}

impl<R, C, Q, L, D> fmt::Debug for Driver<R, C, Q, L, D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Driver")
            .field("io_queues", &self.io_count)
            .field("entries", &self.entries)
            .field("namespace", &self.namespace)
            .field("dead", &self.dead)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

/// The admin command a failure names, for the exit line: its opcode.
fn admin_failure(command: &Command, error: AdminError) -> InitError {
    match error {
        AdminError::Timeout => InitError::AdminTimeout(command.opcode()),
        AdminError::Status(status) => InitError::Admin(command.opcode(), status),
        AdminError::Gone => InitError::Gone,
        AdminError::Fatal => InitError::Fatal,
    }
}

/// Why a waited-for admin command has no completion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AdminError {
    Timeout,
    Status(Status),
    Gone,
    Fatal,
}

impl<R: Registers, C: Clock, Q: Memory, L: Memory, D: Memory> Driver<R, C, Q, L, D> {
    /// Reset the controller, enable it, read Identify Controller and ask
    /// for I/O queues.
    ///
    /// # Errors
    ///
    /// [`Failed`], with the parts and whether the controller is disabled.
    pub fn enable(parts: Parts<R, C, Q, L, D>, options: Options) -> Enabled<R, C, Q, L, D> {
        let mut driver = Driver {
            parts,
            options,
            capabilities: Capabilities(0),
            stride: 4,
            entries: 2,
            admin: Indices::new(ADMIN_ENTRIES),
            admin_sequence: 0,
            io: [IoQueue::EMPTY; MAX_IO_QUEUES],
            io_count: 0,
            next_queue: 0,
            controller: Controller::parse(&Blank, 0),
            namespace: None,
            chunks: [Chunk::default(); MAX_CHUNKS],
            chunk_count: 0,
            host_memory_on: false,
            host_memory_given: false,
            write_cache: false,
            aborts_outstanding: 0,
            resets_in_a_row: 0,
            disabled: false,
            dead: false,
            wants_drain: false,
            stale: false,
            max_blocks: 0,
            stats: Stats::default(),
        };
        match driver.first_start() {
            Ok(()) => Ok(driver),
            Err(error) => {
                // Leave it disabled if it will go: its memory can then be
                // freed.
                if !driver.disabled && error != InitError::Gone {
                    let _ = driver.disable();
                }
                Err(Failed {
                    disabled: driver.disabled,
                    parts: driver.parts,
                    error,
                })
            }
        }
    }

    /// [`Driver::enable`]'s steps.
    fn first_start(&mut self) -> Result<(), InitError> {
        if self.options.quirks.contains(Quirks::UNSUPPORTED) {
            return Err(InitError::Unsupported);
        }
        let io_queues = usize::from(self.options.io_queues).clamp(1, MAX_IO_QUEUES);
        if self.parts.queues.pages().len() < queue_pages_needed(io_queues)
            || self.parts.lists.pages().len()
                < list_pages_needed(io_queues, self.options.list_pages.max(1))
        {
            return Err(InitError::Memory);
        }
        self.start()?;
        let page = self.scratch();
        let _ = self.admin_ok(Command::identify(command::CNS_CONTROLLER, 0, page))?;
        self.controller = Controller::parse(&self.parts.lists, SCRATCH_PAGE * PAGE);
        self.options.quirks = self.options.quirks.union(quirks::for_model(
            self.controller.vendor,
            &self.controller.model,
        ));
        if !Controller::entry_size_fits(self.controller.sq_entry_sizes, 6)
            || !Controller::entry_size_fits(self.controller.cq_entry_sizes, 4)
        {
            return Err(InitError::EntrySize);
        }
        self.negotiate_queues(io_queues)
    }

    /// Disable the controller, program the admin queue, and enable it.
    fn start(&mut self) -> Result<(), InitError> {
        let low = self.parts.regs.read(regs::CAP);
        let high = self.parts.regs.read(regs::CAP + 4);
        if low == regs::GONE && high == regs::GONE {
            return Err(InitError::Gone);
        }
        let capabilities = Capabilities(u64::from(low) | (u64::from(high) << 32));
        if !capabilities.nvm_command_set() {
            return Err(InitError::NoNvmCommandSet);
        }
        if capabilities.min_page_shift() > regs::PAGE_SHIFT {
            return Err(InitError::PageSize);
        }
        self.capabilities = capabilities;
        self.stride = capabilities.doorbell_stride();
        let most = capabilities.max_queue_entries().min(MAX_ENTRIES as u32) as u16;
        self.entries = if self.options.quirks.contains(Quirks::QUEUE_DEPTH_ONE) {
            2
        } else {
            self.options.entries.clamp(2, most.max(2))
        };
        self.disable()?;
        self.admin = Indices::new(ADMIN_ENTRIES.min(most.max(2)));
        self.aborts_outstanding = 0;
        let size = u32::from(self.admin.entries() - 1);
        let sq = self.queue_page(0, false)?;
        let cq = self.queue_page(0, true)?;
        self.clear_queue_pages(0);
        let regs = &mut self.parts.regs;
        regs.write(regs::AQA, (size << 16) | size);
        regs.write(regs::ASQ, sq as u32);
        regs.write(regs::ASQ + 4, (sq >> 32) as u32);
        regs.write(regs::ACQ, cq as u32);
        regs.write(regs::ACQ + 4, (cq >> 32) as u32);
        // NVM command set, 4 KiB pages, round robin, no shutdown, 64- and
        // 16-byte entries.
        let config = regs::CC_IOSQES | regs::CC_IOCQES;
        regs.write(regs::CC, config);
        regs.write(regs::CC, config | regs::CC_ENABLE);
        self.disabled = false;
        self.wait_ready(true).map_err(|error| match error {
            InitError::DisableTimeout => InitError::EnableTimeout,
            other => other,
        })
    }

    /// Clear `CC.EN` and wait for `CSTS.RDY` to fall.
    fn disable(&mut self) -> Result<(), InitError> {
        let config = self.parts.regs.read(regs::CC);
        if config == regs::GONE {
            return Err(InitError::Gone);
        }
        if config & regs::CC_ENABLE != 0 {
            self.parts
                .regs
                .write(regs::CC, config & !(regs::CC_ENABLE | regs::CC_SHN_MASK));
        }
        self.wait_ready(false)?;
        self.disabled = true;
        Ok(())
    }

    /// Wait for `CSTS.RDY` to equal `ready`, for as long as `CAP.TO` says.
    fn wait_ready(&mut self, ready: bool) -> Result<(), InitError> {
        if self.options.quirks.contains(Quirks::DELAY_BEFORE_READY) {
            self.parts.clock.pause(READY_DELAY_NANOS);
        }
        let deadline = self
            .parts
            .clock
            .now_nanos()
            .saturating_add(self.capabilities.ready_timeout_nanos());
        loop {
            let status = self.parts.regs.read(regs::CSTS);
            if status == regs::GONE {
                return Err(InitError::Gone);
            }
            if (status & regs::CSTS_READY != 0) == ready {
                return Ok(());
            }
            // A controller that failed while coming up will never be
            // ready; one being disabled may show the old failure.
            if ready && status & regs::CSTS_FATAL != 0 {
                return Err(InitError::Fatal);
            }
            if self.parts.clock.now_nanos() >= deadline {
                return Err(InitError::DisableTimeout);
            }
            self.parts.clock.pause(STATUS_POLL_NANOS);
        }
    }

    /// Ask for `wanted` I/O queue pairs and keep what is granted.
    fn negotiate_queues(&mut self, wanted: usize) -> Result<(), InitError> {
        let ask = (wanted as u32).saturating_sub(1);
        let answer = self.admin_ok(Command::set_feature(
            command::FEATURE_QUEUES,
            (ask << 16) | ask,
        ))?;
        // Both counts are zero based.
        let submission = (answer.result & 0xFFFF) as usize + 1;
        let completion = (answer.result >> 16) as usize + 1;
        let granted = wanted.min(submission).min(completion);
        if granted == 0 {
            return Err(InitError::NoQueues);
        }
        // After a reset the queues must be the ones the slots are in.
        if self.io_count != 0 && granted < self.io_count {
            return Err(InitError::NoQueues);
        }
        if self.io_count == 0 {
            self.io_count = granted;
        }
        Ok(())
    }

    /// What Identify Controller said.
    #[must_use]
    pub const fn controller(&self) -> &Controller {
        &self.controller
    }

    /// The controller's capabilities register.
    #[must_use]
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// The quirks in force: the PCI identity's and the model's.
    #[must_use]
    pub const fn quirks(&self) -> Quirks {
        self.options.quirks
    }

    /// The host memory buffer the controller asks for, if it uses one and
    /// this host lends that much.
    #[must_use]
    pub fn host_memory_wish(&self) -> Option<Wish> {
        Wish::of(
            self.controller.host_memory_preferred,
            self.controller.host_memory_minimum,
            self.controller.host_memory_min_chunk,
            self.controller.host_memory_max_chunks,
            self.options.host_memory_limit_pages,
        )
    }

    /// Lend the controller `chunks` as its host memory buffer, before
    /// [`Driver::bring_up`]. `false`, and nothing lent, when they are not a
    /// buffer the controller can use.
    pub fn lend_host_memory(&mut self, chunks: &[Chunk]) -> bool {
        let Some(wish) = self.host_memory_wish() else {
            return false;
        };
        if !wish.accepts(chunks) {
            return false;
        }
        for (slot, chunk) in self.chunks.iter_mut().zip(chunks.iter()) {
            *slot = *chunk;
        }
        self.chunk_count = chunks.len().min(MAX_CHUNKS);
        true
    }

    /// Finish bringing the controller up: host memory, the write cache,
    /// power management, the namespace, and the I/O queues.
    ///
    /// # Errors
    ///
    /// [`InitError`]; the caller shuts the driver down.
    pub fn bring_up(&mut self) -> Result<(), InitError> {
        self.apply_features();
        let namespace = self.find_namespace()?;
        self.namespace = Some(namespace);
        self.max_blocks = self.transfer_limit(&namespace);
        self.create_queues()
    }

    /// The features set after every enable. None of them is needed for the
    /// controller to work, so a refusal is recorded and not an error.
    fn apply_features(&mut self) {
        self.host_memory_on = false;
        if self.chunk_count > 0 {
            let chunks = self.chunks;
            let chunks = chunks.get(..self.chunk_count).unwrap_or_default();
            let pages = hmb::write_descriptors(&mut self.parts.lists, HMB_LIST_PAGE * PAGE, chunks);
            let list = self.list_page(HMB_LIST_PAGE);
            let mut flags = command::HOST_MEMORY_ENABLE;
            if self.host_memory_given {
                flags |= command::HOST_MEMORY_RETURN;
            }
            let set = Command::host_memory(flags, pages, list, self.chunk_count as u32);
            self.host_memory_on = self.admin_ok(set).is_ok();
            self.host_memory_given |= self.host_memory_on;
        }
        self.write_cache = false;
        if self.controller.write_cache {
            // On, so that writes are fast; a flush or a forced write is
            // what makes them durable.
            let _ = self.admin_ok(Command::set_feature(command::FEATURE_WRITE_CACHE, 1));
            self.write_cache = true;
        }
        if self.controller.apst {
            // Off: the controller stays in the power state it is in. The
            // command carries a transition table whatever it says.
            self.clear_scratch();
            let table = self.scratch();
            let off = Command::set_feature(command::FEATURE_APST, 0).prp(table, 0);
            let _ = self.admin_ok(off);
        }
    }

    /// The namespace to serve: the first active one.
    fn find_namespace(&mut self) -> Result<Namespace, InitError> {
        let page = self.scratch();
        let (major, minor) = self.controller.version_pair();
        let lists = (major, minor) >= (1, 1) && !self.options.quirks.contains(Quirks::IDENTIFY_CNS);
        let mut id = 1;
        if lists {
            self.clear_scratch();
            let list = Command::identify(command::CNS_ACTIVE_NAMESPACES, 0, page);
            if self.admin_ok(list).is_ok() {
                id = identify::first_active(&self.parts.lists, SCRATCH_PAGE * PAGE)
                    .ok_or(InitError::NoNamespace)?;
            }
        }
        self.clear_scratch();
        let _ = self.admin_ok(Command::identify(command::CNS_NAMESPACE, id, page))?;
        Namespace::parse(&self.parts.lists, SCRATCH_PAGE * PAGE, id).map_err(
            |refused| match refused {
                NamespaceRefused::Inactive => InitError::NoNamespace,
                other => InitError::Namespace(other),
            },
        )
    }

    /// The most blocks one command carries on `namespace`.
    fn transfer_limit(&self, namespace: &Namespace) -> u32 {
        let mut bytes = u64::from(self.options.max_transfer_bytes);
        if self.controller.max_transfer_shift != 0 {
            let shift = u32::from(self.controller.max_transfer_shift)
                .saturating_add(self.capabilities.min_page_shift())
                .min(40);
            bytes = bytes.min(1_u64 << shift);
        }
        // What the list pages of one slot can name, less the page a
        // transfer that starts inside a page spills into.
        let listed = (self.options.list_pages.max(1) * (LIST_ENTRIES - 1)) as u64;
        bytes = bytes.min(listed * PAGE as u64);
        bytes = bytes.min((self.parts.data.pages().len() * PAGE) as u64);
        ((bytes >> namespace.block_shift).min(0x1_0000) as u32).max(1)
    }

    /// Make the I/O queues, each completion queue before its submission
    /// queue.
    fn create_queues(&mut self) -> Result<(), InitError> {
        for index in 0..self.io_count {
            let queue = (index + 1) as u16;
            let cq = self.queue_page(queue, true)?;
            let sq = self.queue_page(queue, false)?;
            self.clear_queue_pages(queue);
            let vector = queue % self.options.vectors.max(1);
            let _ = self.admin_ok(Command::create_cq(queue, self.entries, cq, vector))?;
            let _ = self.admin_ok(Command::create_sq(queue, self.entries, sq))?;
            if let Some(io) = self.io.get_mut(index) {
                io.indices = Indices::new(self.entries);
            }
        }
        Ok(())
    }

    /// The namespace served, once [`Driver::bring_up`] found it.
    #[must_use]
    pub const fn namespace(&self) -> Option<&Namespace> {
        self.namespace.as_ref()
    }

    /// The most blocks one request may carry.
    #[must_use]
    pub const fn max_transfer_blocks(&self) -> u32 {
        self.max_blocks
    }

    /// Whether the controller has a volatile write cache, so that a flush
    /// is a command and not a no-op.
    #[must_use]
    pub const fn write_cache(&self) -> bool {
        self.write_cache
    }

    /// Pages of host memory the controller took.
    #[must_use]
    pub fn host_memory_pages(&self) -> u32 {
        if !self.host_memory_on {
            return 0;
        }
        self.chunks
            .iter()
            .take(self.chunk_count)
            .map(|chunk| chunk.pages)
            .sum()
    }

    /// I/O queue pairs in use.
    #[must_use]
    pub const fn io_queues(&self) -> usize {
        self.io_count
    }

    /// Entries in each I/O queue.
    #[must_use]
    pub const fn entries(&self) -> u16 {
        self.entries
    }

    /// The counts so far.
    #[must_use]
    pub const fn stats(&self) -> Stats {
        self.stats
    }

    /// Requests held: in the controller, waiting to go, or finished and
    /// not yet drained.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.io
            .iter()
            .take(self.io_count)
            .flat_map(|queue| queue.slots.iter())
            .filter(|slot| slot.state != State::Free)
            .count()
    }

    /// Whether [`Driver::drain`] has something to report that no interrupt
    /// will announce.
    #[must_use]
    pub const fn wants_drain(&self) -> bool {
        self.wants_drain
    }

    /// Take a request.
    ///
    /// # Errors
    ///
    /// [`SubmitError`], with [`SubmitError::Busy`] the one to retry after a
    /// drain.
    pub fn submit(&mut self, request: &Request) -> Result<Submitted, SubmitError> {
        if self.dead {
            return Err(SubmitError::Dead);
        }
        let namespace = self.namespace.ok_or(SubmitError::Dead)?;
        if request.op == Op::Flush {
            if !self.write_cache {
                return Ok(Submitted::Completed(Completion {
                    id: request.id,
                    outcome: Outcome::Ok,
                    bytes: 0,
                }));
            }
        } else {
            self.validate(request, &namespace)?;
        }
        let usable = usize::from(self.entries) - 1;
        let count = self.io_count.max(1);
        for step in 0..count {
            let index = (self.next_queue + step) % count;
            let Some(queue) = self.io.get_mut(index) else {
                continue;
            };
            let Some(slot) = queue
                .slots
                .iter_mut()
                .take(usable)
                .find(|slot| slot.state == State::Free)
            else {
                continue;
            };
            slot.state = State::Waiting;
            slot.request = *request;
            slot.generation = slot.generation.wrapping_add(1);
            slot.retries = 0;
            slot.since = 0;
            self.next_queue = (index + 1) % count;
            self.pump();
            return Ok(Submitted::Queued);
        }
        Err(SubmitError::Busy)
    }

    /// Whether a read or write is one the namespace and the data region
    /// can serve.
    fn validate(&self, request: &Request, namespace: &Namespace) -> Result<(), SubmitError> {
        if request.blocks == 0 {
            return Err(SubmitError::Empty);
        }
        if request.blocks > self.max_blocks {
            return Err(SubmitError::TooLarge);
        }
        let end = request.lba.checked_add(u64::from(request.blocks));
        if end.is_none_or(|end| end > namespace.blocks) {
            return Err(SubmitError::OutOfRange);
        }
        if request.op == Op::Write && namespace.read_only {
            return Err(SubmitError::ReadOnly);
        }
        if !request.data_offset.is_multiple_of(4) {
            return Err(SubmitError::Unaligned);
        }
        let bytes = u64::from(request.blocks) << namespace.block_shift;
        let data = (self.parts.data.pages().len() * PAGE) as u64;
        if request
            .data_offset
            .checked_add(bytes)
            .is_none_or(|end| end > data)
        {
            return Err(SubmitError::OutsideData);
        }
        Ok(())
    }

    /// Send every waiting request its submission queue has room for.
    fn pump(&mut self) {
        for index in 0..self.io_count {
            let mut tail = None;
            for slot in 0..usize::from(self.entries) {
                let waiting = self
                    .io
                    .get(index)
                    .and_then(|queue| queue.slots.get(slot))
                    .is_some_and(|slot| slot.state == State::Waiting);
                if !waiting {
                    continue;
                }
                match self.send(index, slot) {
                    Sent::Tail(new_tail) => tail = Some(new_tail),
                    Sent::Failed => {}
                    Sent::Full => break,
                }
            }
            if let Some(tail) = tail {
                self.parts.queues.barrier();
                self.parts.regs.write(
                    regs::sq_doorbell((index + 1) as u16, self.stride),
                    u32::from(tail),
                );
            }
        }
    }

    /// Put waiting slot `slot` of I/O queue `index` into its submission
    /// queue.
    fn send(&mut self, index: usize, slot: usize) -> Sent {
        let (Some(namespace), Some(queue)) = (self.namespace, self.io.get(index)) else {
            return Sent::Full;
        };
        if queue.indices.sq_full() {
            return Sent::Full;
        }
        let Some(held) = queue.slots.get(slot).copied() else {
            return Sent::Full;
        };
        let request = held.request;
        let id = (slot as u16) | (u16::from(held.generation) << 8);
        let command = match request.op {
            Op::Flush => Some(Command::flush(namespace.id)),
            Op::Read | Op::Write => {
                let lists = self.options.list_pages.max(1);
                let first = FIRST_LIST_PAGE + (index * MAX_ENTRIES + slot) * lists;
                let bytes = u64::from(request.blocks) << namespace.block_shift;
                let opcode = if request.op == Op::Read {
                    command::NVM_READ
                } else {
                    command::NVM_WRITE
                };
                prp::build(
                    &self.parts.data,
                    &mut self.parts.lists,
                    first,
                    lists,
                    request.data_offset,
                    bytes,
                )
                .ok()
                .map(|prp| {
                    Command::transfer(
                        opcode,
                        namespace.id,
                        request.lba,
                        request.blocks,
                        request.fua,
                    )
                    .prp(prp.first, prp.second)
                })
            }
        };
        let Some(queue) = self.io.get_mut(index) else {
            return Sent::Full;
        };
        let Some(held) = queue.slots.get_mut(slot) else {
            return Sent::Full;
        };
        let Some(command) = command else {
            // Checked when it was taken, so only a changed data region
            // gets here: fail it rather than send a wrong address.
            held.state = State::Done(Outcome::IoError);
            self.wants_drain = true;
            return Sent::Failed;
        };
        let Some((at, tail)) = queue.indices.push() else {
            return Sent::Full;
        };
        held.state = State::Queued;
        held.since = 0;
        self.write_command((index + 1) as u16, at, &command.id(id));
        Sent::Tail(tail)
    }

    /// Take what the controller completed, and what finished without it.
    ///
    /// # Errors
    ///
    /// [`Fatal`] once the driver has stopped.
    pub fn drain(&mut self, out: &mut [Completion]) -> Result<Drained, Fatal> {
        if self.dead {
            return Err(Fatal::Dead);
        }
        self.wants_drain = false;
        self.stale = false;
        let mut count = 0;
        let mut more = self.report_done(out, &mut count);
        for index in 0..self.io_count {
            more |= self.drain_queue(index, out, &mut count);
        }
        self.drain_admin();
        self.pump();
        if more {
            self.wants_drain = true;
        }
        Ok(Drained {
            completions: count,
            more,
        })
    }

    /// Report the slots that finished without a completion entry; whether
    /// some did not fit.
    fn report_done(&mut self, out: &mut [Completion], count: &mut usize) -> bool {
        let io_count = self.io_count;
        for slot in self
            .io
            .iter_mut()
            .take(io_count)
            .flat_map(|queue| queue.slots.iter_mut())
        {
            let State::Done(outcome) = slot.state else {
                continue;
            };
            let Some(place) = out.get_mut(*count) else {
                return true;
            };
            *place = Completion {
                id: slot.request.id,
                outcome,
                bytes: 0,
            };
            *count += 1;
            slot.state = State::Free;
        }
        false
    }

    /// Take I/O queue `index`'s completions; whether some did not fit.
    fn drain_queue(&mut self, index: usize, out: &mut [Completion], count: &mut usize) -> bool {
        let queue = (index + 1) as u16;
        let mut head = None;
        let mut more = false;
        while let Some(indices) = self.io.get(index).map(|queue| queue.indices) {
            let Some(entry) = self.peek(queue, &indices) else {
                break;
            };
            if *count >= out.len() {
                more = true;
                break;
            }
            if let Some(io) = self.io.get_mut(index) {
                head = Some(io.indices.pop(entry.sq_head));
            }
            if let Some(completion) = self.finish(index, &entry)
                && let Some(place) = out.get_mut(*count)
            {
                *place = completion;
                *count += 1;
            }
        }
        if let Some(head) = head {
            self.parts
                .regs
                .write(regs::cq_doorbell(queue, self.stride), u32::from(head));
        }
        more
    }

    /// What completion `entry` of I/O queue `index` means for its slot.
    fn finish(&mut self, index: usize, entry: &command::Completion) -> Option<Completion> {
        let block_shift = self.namespace.map_or(9, |namespace| namespace.block_shift);
        let slot = self
            .io
            .get_mut(index)
            .and_then(|queue| queue.slots.get_mut(usize::from(entry.id & 0xFF)));
        let Some(slot) = slot.filter(|slot| {
            matches!(slot.state, State::Queued | State::Aborting)
                && slot.generation == (entry.id >> 8) as u8
        }) else {
            self.stats.spurious = self.stats.spurious.saturating_add(1);
            return None;
        };
        let request = slot.request;
        if entry.status.is_success() {
            slot.state = State::Free;
            self.resets_in_a_row = 0;
            let bytes = match request.op {
                Op::Flush => 0,
                Op::Read | Op::Write => u64::from(request.blocks) << block_shift,
            };
            return Some(Completion {
                id: request.id,
                outcome: Outcome::Ok,
                bytes,
            });
        }
        if !entry.status.do_not_retry() && slot.retries < MAX_RETRIES {
            slot.retries += 1;
            slot.generation = slot.generation.wrapping_add(1);
            slot.state = State::Waiting;
            self.stats.retries = self.stats.retries.saturating_add(1);
            return None;
        }
        slot.state = State::Free;
        self.stats.errors = self.stats.errors.saturating_add(1);
        self.stats.last_error = entry.status;
        let outcome = if entry.status.code() == Status::INVALID_OPCODE.0 {
            Outcome::Unsupported
        } else {
            Outcome::IoError
        };
        Some(Completion {
            id: request.id,
            outcome,
            bytes: 0,
        })
    }

    /// Take the admin queue's completions while nothing waits on one: they
    /// are the Abort commands'.
    fn drain_admin(&mut self) {
        let mut head = None;
        while let Some(entry) = self.peek(0, &self.admin) {
            head = Some(self.admin.pop(entry.sq_head));
            self.note_admin(&entry);
        }
        if let Some(head) = head {
            self.parts
                .regs
                .write(regs::cq_doorbell(0, self.stride), u32::from(head));
        }
    }

    /// An admin completion nobody waits for.
    fn note_admin(&mut self, entry: &command::Completion) {
        if entry.id & ADMIN_ABORT != 0 {
            self.aborts_outstanding = self.aborts_outstanding.saturating_sub(1);
        }
    }

    /// Look at what happened to the controller while nothing was heard
    /// from it; see the module's documentation.
    ///
    /// # Errors
    ///
    /// [`Fatal`] when the driver stops for good.
    pub fn check(&mut self) -> Result<Option<Event>, Fatal> {
        if self.dead {
            return Err(Fatal::Dead);
        }
        let status = self.parts.regs.read(regs::CSTS);
        if status == regs::GONE {
            self.dead = true;
            return Err(Fatal::Gone);
        }
        if status & regs::CSTS_FATAL != 0 {
            return self.recover(Cause::ControllerFatal);
        }
        let now = self.parts.clock.now_nanos().max(1);
        if !self.completions_waiting() {
            self.stale = false;
        } else if !self.stale {
            // Seen for the first time: its interrupt may be on the way.
            self.stale = true;
        } else {
            // Still there a whole look later, and no interrupt came to have
            // it taken: the interrupt was lost, or under a hypervisor the
            // processor it goes to has not run since.
            self.stale = false;
            self.wants_drain = true;
            self.stats.missed_interrupts = self.stats.missed_interrupts.saturating_add(1);
            return Ok(Some(Event::MissedInterrupt));
        }
        let (aborted, expired) = self.age(now);
        if expired {
            return self.recover(Cause::Timeout);
        }
        Ok((aborted > 0).then_some(Event::Aborted(aborted)))
    }

    /// Whether any I/O completion queue has an entry not taken.
    fn completions_waiting(&self) -> bool {
        (0..self.io_count).any(|index| {
            self.io
                .get(index)
                .is_some_and(|queue| self.peek((index + 1) as u16, &queue.indices).is_some())
        })
    }

    /// Stamp the commands first seen now, abort the ones past the I/O
    /// timeout, and say how many were aborted and whether an aborted one
    /// is past its grace.
    fn age(&mut self, now: u64) -> (u32, bool) {
        let mut aborted = 0;
        let mut expired = false;
        for index in 0..self.io_count {
            for at in 0..usize::from(self.entries) {
                match self.age_slot(index, at, now) {
                    Aged::Aborted => aborted += 1,
                    Aged::Expired => expired = true,
                    Aged::Young => {}
                }
            }
        }
        (aborted, expired)
    }

    /// [`Driver::age`] for slot `at` of I/O queue `index`.
    fn age_slot(&mut self, index: usize, at: usize, now: u64) -> Aged {
        let timeout = self.options.io_timeout_nanos;
        let grace = self.options.abort_grace_nanos;
        let Some(slot) = self.io.get_mut(index).and_then(|io| io.slots.get_mut(at)) else {
            return Aged::Young;
        };
        let age = now.saturating_sub(slot.since);
        match slot.state {
            State::Queued if slot.since == 0 => slot.since = now,
            State::Aborting if age >= grace => return Aged::Expired,
            State::Queued if age >= timeout => {
                let id = (at as u16) | (u16::from(slot.generation) << 8);
                if !self.send_abort((index + 1) as u16, id) {
                    return Aged::Young;
                }
                if let Some(slot) = self.io.get_mut(index).and_then(|io| io.slots.get_mut(at)) {
                    slot.state = State::Aborting;
                    slot.since = now;
                }
                return Aged::Aborted;
            }
            _ => {}
        }
        Aged::Young
    }

    /// Send Abort for command `id` of submission queue `queue`, if the
    /// controller's abort limit and the admin queue have room.
    fn send_abort(&mut self, queue: u16, id: u16) -> bool {
        if self.aborts_outstanding > u16::from(self.controller.abort_limit) {
            return false;
        }
        let Some((at, tail)) = self.admin.push() else {
            return false;
        };
        self.admin_sequence = self.admin_sequence.wrapping_add(1);
        let abort = Command::abort(queue, id).id(ADMIN_ABORT | (self.admin_sequence & 0xFF));
        self.write_command(0, at, &abort);
        self.parts.queues.barrier();
        self.parts
            .regs
            .write(regs::sq_doorbell(0, self.stride), u32::from(tail));
        self.aborts_outstanding += 1;
        self.stats.aborts = self.stats.aborts.saturating_add(1);
        true
    }

    /// Reset the controller, set it up as it was, and send again what was
    /// in flight.
    fn recover(&mut self, cause: Cause) -> Result<Option<Event>, Fatal> {
        self.stats.resets = self.stats.resets.saturating_add(1);
        if self.resets_in_a_row >= MAX_RESETS {
            self.dead = true;
            return Err(Fatal::ResetLimit);
        }
        self.resets_in_a_row += 1;
        if let Err(error) = self.restart() {
            self.dead = true;
            return Err(Fatal::Reset(error));
        }
        let (requeued, failed) = self.requeue();
        self.pump();
        self.wants_drain = true;
        Ok(Some(Event::Reset {
            cause,
            requeued,
            failed,
        }))
    }

    /// The bring-up again, for a controller that was up before.
    fn restart(&mut self) -> Result<(), InitError> {
        self.start()?;
        self.negotiate_queues(self.io_count)?;
        self.apply_features();
        self.create_queues()
    }

    /// Mark every request that was in the controller to be sent again, or
    /// failed if it has been sent too often; how many of each.
    fn requeue(&mut self) -> (u32, u32) {
        let mut requeued = 0;
        let mut failed = 0;
        let io_count = self.io_count;
        for slot in self
            .io
            .iter_mut()
            .take(io_count)
            .flat_map(|queue| queue.slots.iter_mut())
        {
            if !matches!(slot.state, State::Queued | State::Aborting) {
                continue;
            }
            slot.generation = slot.generation.wrapping_add(1);
            slot.since = 0;
            if slot.retries >= MAX_RETRIES {
                slot.state = State::Done(Outcome::IoError);
                failed += 1;
            } else {
                slot.retries += 1;
                slot.state = State::Waiting;
                requeued += 1;
            }
        }
        self.stats.errors = self.stats.errors.saturating_add(failed);
        (requeued, failed)
    }

    /// Take the controller down: host memory back, the shutdown
    /// notification, then disabled.
    pub fn shutdown(mut self) -> Shutdown<Parts<R, C, Q, L, D>> {
        let mut clean = false;
        let alive = self.parts.regs.read(regs::CSTS);
        if alive != regs::GONE && !self.disabled {
            if self.host_memory_on && alive & regs::CSTS_FATAL == 0 && !self.dead {
                let _ = self.admin_ok(Command::host_memory(0, 0, 0, 0));
                self.host_memory_on = false;
            }
            clean = self.notify_shutdown();
        }
        if self.disable().is_err() {
            return Shutdown::Wedged(self.parts);
        }
        let mut abandoned = [0_u64; MAX_IO_QUEUES * MAX_ENTRIES];
        let mut count = 0;
        let held = self
            .io
            .iter()
            .take(self.io_count)
            .flat_map(|queue| queue.slots.iter())
            .filter(|slot| slot.state != State::Free);
        for (place, slot) in abandoned.iter_mut().zip(held) {
            *place = slot.request.id;
            count += 1;
        }
        Shutdown::Released(Released {
            parts: self.parts,
            clean,
            abandoned,
            count,
        })
    }

    /// Write `CC.SHN` = 01b and wait for `CSTS.SHST` = 10b; whether it
    /// came.
    fn notify_shutdown(&mut self) -> bool {
        let config = self.parts.regs.read(regs::CC);
        if config == regs::GONE || config & regs::CC_ENABLE == 0 {
            return false;
        }
        self.parts.regs.write(
            regs::CC,
            (config & !regs::CC_SHN_MASK) | regs::CC_SHN_NORMAL,
        );
        let allowed = (u64::from(self.controller.shutdown_micros) * 1000)
            .clamp(SHUTDOWN_MIN_NANOS, SHUTDOWN_MAX_NANOS);
        let deadline = self.parts.clock.now_nanos().saturating_add(allowed);
        loop {
            let status = self.parts.regs.read(regs::CSTS);
            if status == regs::GONE {
                return false;
            }
            if status & regs::CSTS_SHST_MASK == regs::CSTS_SHST_COMPLETE {
                return true;
            }
            if self.parts.clock.now_nanos() >= deadline {
                return false;
            }
            self.parts.clock.pause(STATUS_POLL_NANOS);
        }
    }

    /// Send an admin command and wait for its completion, which must be a
    /// success.
    fn admin_ok(&mut self, command: Command) -> Result<command::Completion, InitError> {
        self.admin_wait(command)
            .map_err(|error| admin_failure(&command, error))
    }

    /// Send an admin command and wait for its completion.
    fn admin_wait(&mut self, command: Command) -> Result<command::Completion, AdminError> {
        let (at, tail) = self.admin.push().ok_or(AdminError::Timeout)?;
        self.admin_sequence = self.admin_sequence.wrapping_add(1);
        let id = ADMIN_SYNC | (self.admin_sequence & 0xFF);
        self.write_command(0, at, &command.id(id));
        self.parts.queues.barrier();
        self.parts
            .regs
            .write(regs::sq_doorbell(0, self.stride), u32::from(tail));
        let deadline = self
            .parts
            .clock
            .now_nanos()
            .saturating_add(self.options.admin_timeout_nanos);
        loop {
            while let Some(entry) = self.peek(0, &self.admin) {
                let head = self.admin.pop(entry.sq_head);
                self.parts
                    .regs
                    .write(regs::cq_doorbell(0, self.stride), u32::from(head));
                if entry.id != id {
                    self.note_admin(&entry);
                    continue;
                }
                return if entry.status.is_success() {
                    Ok(entry)
                } else {
                    Err(AdminError::Status(entry.status))
                };
            }
            let status = self.parts.regs.read(regs::CSTS);
            if status == regs::GONE {
                return Err(AdminError::Gone);
            }
            if status & regs::CSTS_FATAL != 0 {
                return Err(AdminError::Fatal);
            }
            if self.parts.clock.now_nanos() >= deadline {
                return Err(AdminError::Timeout);
            }
            self.parts.clock.pause(ADMIN_POLL_NANOS);
        }
    }

    /// The completion at the head of queue `queue`'s completion queue, if
    /// a new one is there.
    fn peek(&self, queue: u16, indices: &Indices) -> Option<command::Completion> {
        let at = (2 * usize::from(queue) + 1) * PAGE
            + usize::from(indices.cq_head()) * command::CQE_BYTES;
        let memory = &self.parts.queues;
        // The status word first, in one access: its phase bit is what says
        // the rest of the entry has been written.
        let status = memory.read_u16(at + 14);
        if !indices.is_new(status & 1 != 0) {
            return None;
        }
        let result = u32::from(memory.read_u16(at)) | (u32::from(memory.read_u16(at + 2)) << 16);
        Some(command::Completion {
            result,
            sq_head: memory.read_u16(at + 8),
            id: memory.read_u16(at + 12),
            status: Status(status >> 1),
        })
    }

    /// Write `command` into entry `at` of queue `queue`'s submission queue.
    fn write_command(&mut self, queue: u16, at: u16, command: &Command) {
        let base = 2 * usize::from(queue) * PAGE + usize::from(at) * command::SQE_BYTES;
        for (index, byte) in command.0.iter().enumerate() {
            self.parts.queues.write_u8(base + index, *byte);
        }
    }

    /// The device address of queue `queue`'s submission or completion
    /// page.
    fn queue_page(&self, queue: u16, completion: bool) -> Result<u64, InitError> {
        self.parts
            .queues
            .pages()
            .get(2 * usize::from(queue) + usize::from(completion))
            .copied()
            .ok_or(InitError::Memory)
    }

    /// Zero both pages of queue `queue`, so no entry of an earlier life
    /// has the phase a new one would.
    fn clear_queue_pages(&mut self, queue: u16) {
        let base = 2 * usize::from(queue) * PAGE;
        for offset in base..base + 2 * PAGE {
            self.parts.queues.write_u8(offset, 0);
        }
    }

    /// The device address of list page `page`.
    fn list_page(&self, page: usize) -> u64 {
        self.parts.lists.pages().get(page).copied().unwrap_or(0)
    }

    /// The scratch page's device address.
    fn scratch(&self) -> u64 {
        self.list_page(SCRATCH_PAGE)
    }

    /// Zero the scratch page.
    fn clear_scratch(&mut self) {
        for offset in SCRATCH_PAGE * PAGE..(SCRATCH_PAGE + 1) * PAGE {
            self.parts.lists.write_u8(offset, 0);
        }
    }
}

/// Memory that reads as zero: what Identify Controller is before it is
/// read.
struct Blank;

impl Memory for Blank {
    fn pages(&self) -> &[u64] {
        &[]
    }

    fn read_u8(&self, _offset: usize) -> u8 {
        0
    }

    fn write_u8(&mut self, _offset: usize, _value: u8) {}

    fn read_u16(&self, _offset: usize) -> u16 {
        0
    }
}
