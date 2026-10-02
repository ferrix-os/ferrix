//! Fault windows: a range of a client's address space whose pages a server
//! puts in and takes out, and whose faults the server answers.
//!
//! `docs/NVIDIA.md` §11.4 (K1), as the certification consultant accepted it
//! on 2026-10-02. A GPU driver with shared virtual memory -- NVIDIA's UVM for
//! CUDA's managed memory, and later AMD's or Intel's -- puts pages of its own
//! into a program's address space at any page of a mapping, and takes them
//! out again when the device wants the data. Linux gives such a driver
//! `vm_insert_page` and `unmap_mapping_range` and lets its fault handler run
//! in the faulting thread. Ferrix's driver is a process, so the kernel offers
//! the same three things across the boundary: a fault on a page the window
//! lacks goes to the server as a packet and the faulting thread waits; the
//! server inserts pages of VMOs it holds; and it revokes them. Nothing here
//! names a device or a vendor.
//!
//! # The objects
//!
//! A [`Server`] is the driver's side: its port, where the packets go, and the
//! windows it serves. Its identity is its handle ([`ServerHandle`]), which
//! can be neither duplicated nor transferred, and whose close is the
//! server's death.
//!
//! A [`FaultWindow`] is one mapping's pages, numbered from the mapping's
//! start, which the map keeps one region for its whole life
//! (`ferrix_vma::Backing::Window`). It holds:
//!
//! * a table from page offset to an entry: a frame of a VMO the server
//!   holds, kept in place by a [`Held`] of its own, and whether clients may
//!   write it;
//! * the address spaces that map it, each with the id it names the window
//!   by, and how many mappings there are;
//! * the `UNMAPPED` packet's promised room on the server's port.
//!
//! # The rules, and where each is kept
//!
//! * **A client's thread waits only for a fault it took in a window it
//!   mapped**, and the wait ends on the server's answer, on the client's
//!   kill or a pending signal ([`Host::wait_interrupted`]), and on the
//!   server's death ([`FaultWindow::forward`]). A kernel copy never waits:
//!   it finds the page there or fails ([`Waits::Never`]).
//! * **No client can reach a frame after its hold is let go.** Every way an
//!   entry leaves -- a revoke, a replacement, the server's death -- takes it
//!   out of the table first, then every mapper's translations of it, then
//!   one shootdown, and only then drops the hold ([`FaultWindow::revoke`]).
//!   A fault racing that never installs a frame that has left the table,
//!   because the table is read under the space's lock, and every mapper is
//!   visited under its space's lock after the entry left.
//! * **The end of a window is its mapping count reaching zero**, decremented
//!   where the last mapping's region leaves its map, without allocating; the
//!   `UNMAPPED` packet's room was promised when the window was made, so it is
//!   queued there exactly once, before the unmap returns.
//! * **The server's death is a mark, then a revoke.** The mark is set under
//!   each window's lock as the handle closes, so faults stop waiting and
//!   inserts are refused at once; the revoke, which needs shootdowns, runs on
//!   the window death task, never in a `Drop`.
//!
//! # Lock order
//!
//! An address space's lock, then a window's lock, then a VMO's pages. No
//! window lock is held while a space's lock is taken, and no shootdown waits
//! under either. A server's list of its windows is taken before a window's
//! lock and never after it.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_frame::Frame;
use ferrix_native_abi::types::{
    PACKET_WINDOW_FAULT, PACKET_WINDOW_UNMAPPED, PortPacket, WINDOW_FAULT_WRITE,
};
use ferrix_sched::{CpuSet, NICE_0_WEIGHT};
use ferrix_sync::Once;

use crate::fallible::{self, AllocError};
use crate::object::port::{Port, Promise};
use crate::object::process::Process;
use crate::sched::{UserThread, WaitQueue};
use crate::smp::{self, TlbPages};
use crate::sync::SpinLock;
use crate::user::space::AddressSpace;
use crate::user::vmo::{Held, Vmo};

/// Pages one chunk of a window's table holds. A chunk is allocated before the
/// window's lock is taken, so that putting an entry in never allocates under
/// it.
const CHUNK_PAGES: u64 = 64;

/// The most entries a revoke takes out of a table under one hold of its
/// lock: the bound on how long the lock is held for a revoke of any length.
const REVOKE_BATCH: usize = 64;

/// The most entries one insert puts in, as the native call bounds it.
pub(crate) const INSERT_MAX: usize = 512;

/// How a fault may be resolved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Waits {
    /// A program's own fault, from the trap path: it may wait for a server.
    OnServer,
    /// A kernel access on a program's behalf -- a copy to or from user
    /// memory, a futex word, another process's memory -- which never waits
    /// for a server. A page a window lacks is refused there, and the copy
    /// fails with `EFAULT`.
    Never,
}

/// Why a window refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WindowError {
    /// The server is dead, or the fault was its own: the faulting thread gets
    /// `SIGBUS`.
    Dead,
    /// The server answered the fault with an error: `SIGBUS` too.
    Refused,
    /// A kernel access found the page absent and may not wait: `EFAULT`.
    Absent,
    /// The window no client maps any more, or a dead one: an insert is
    /// refused.
    Closed,
    /// A page offset outside the window, a page not committed, a VMO that is
    /// a file's: an insert is refused, all of it.
    BadEntry,
    /// No memory to note the fault or the entries, before anything changed.
    NoMemory,
}

/// One page in a window's table.
#[derive(Debug)]
struct Entry {
    /// The frame clients are shown.
    frame: Frame,
    /// Whether clients may write it.
    write: bool,
    /// Whether it is mapped past the caches: the memory type of the VMO it
    /// came from, read once as it was inserted, never chosen per insert.
    uncached: bool,
    /// What keeps `frame` at its index in its VMO, and the VMO alive, until
    /// the entry has left every client's tables.
    held: Held,
}

/// [`CHUNK_PAGES`] slots of a window's table.
#[derive(Debug)]
struct Chunk {
    slots: [Option<Entry>; CHUNK_PAGES as usize],
}

impl Chunk {
    /// An empty chunk, on the heap.
    fn new() -> Result<Box<Chunk>, AllocError> {
        fallible::try_box(Chunk {
            slots: [const { None }; CHUNK_PAGES as usize],
        })
    }
}

/// Where a window is in its life.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Life {
    /// Made, and mapped by at least one client or about to be.
    Open,
    /// Its mapping count reached zero: no client maps it, and none can again.
    Closed,
    /// Its server died. Faults fail, inserts are refused, and its entries are
    /// being revoked.
    Dead,
}

/// What a fault waiting on the server knows of its answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answer {
    /// None yet.
    Waiting,
    /// Retry the access.
    Retry,
    /// `SIGBUS`.
    Refused,
}

/// One address space's mapping of a window.
#[derive(Debug)]
struct Mapper {
    /// The space. Weak, as a VMO's mapper is: the space keeps the window.
    space: Weak<AddressSpace>,
    /// The id its regions name the window by.
    id: u64,
}

/// A window's state, under one lock.
#[derive(Debug)]
struct State {
    /// The table: chunk index to chunk.
    chunks: BTreeMap<u64, Box<Chunk>>,
    /// The spaces mapping it.
    mappers: Vec<Mapper>,
    /// Where it is in its life.
    life: Life,
    /// Faults waiting on the server, by token.
    faults: BTreeMap<u64, Answer>,
    /// The next token to hand a fault.
    next_token: u64,
}

impl State {
    /// The entry at page `offset`, if the table has one.
    fn entry(&self, offset: u64) -> Option<&Entry> {
        self.chunks
            .get(&(offset / CHUNK_PAGES))?
            .slots
            .get((offset % CHUNK_PAGES) as usize)?
            .as_ref()
    }

    /// The slot for page `offset`, if its chunk exists.
    fn slot(&mut self, offset: u64) -> Option<&mut Option<Entry>> {
        self.chunks
            .get_mut(&(offset / CHUNK_PAGES))?
            .slots
            .get_mut((offset % CHUNK_PAGES) as usize)
    }

    /// Take up to `max` of the entries at page offsets `first..end` out of
    /// the table into `out`, which has room for them, lowest first, with
    /// their offsets. Allocates nothing.
    fn take_into(&mut self, first: u64, end: u64, max: usize, out: &mut Vec<(u64, Entry)>) {
        let first_chunk = first / CHUNK_PAGES;
        let last_chunk = end.saturating_sub(1) / CHUNK_PAGES;
        for (&chunk, slots) in self.chunks.range_mut(first_chunk..=last_chunk) {
            for (at, slot) in slots.slots.iter_mut().enumerate() {
                let offset = chunk * CHUNK_PAGES + at as u64;
                if offset < first || offset >= end || slot.is_none() {
                    continue;
                }
                if out.len() >= max || out.len() == out.capacity() {
                    return;
                }
                if let Some(entry) = slot.take() {
                    // NOALLOC: below the capacity, checked just above.
                    out.push((offset, entry));
                }
            }
        }
    }
}

/// A fault window. See the module.
#[derive(Debug)]
pub(crate) struct FaultWindow {
    /// The key its packets carry, which the server names it by.
    key: u64,
    /// Its length in pages.
    pages: u64,
    /// Its server.
    server: Arc<Server>,
    /// Its state.
    state: SpinLock<State>,
    /// The faults waiting on the server sleep here.
    waiters: WaitQueue,
    /// The room the `UNMAPPED` packet was promised on the server's port, taken
    /// out as the packet is queued: there is one.
    unmapped: SpinLock<Option<Promise>>,
    /// That room, charged to the job of the client that made the window, for
    /// as long as the window lives.
    #[expect(dead_code, reason = "AUDIT: held for its drop, which uncharges the job")]
    unmapped_charge: crate::object::quota::Charge,
}

impl FaultWindow {
    /// A window of `pages` pages served by `server`: what a subsystem in the
    /// load ring makes when a program maps a device file whose driver serves
    /// faults. Mapped by nobody yet; [`AddressSpace::map_fault_window`] maps
    /// it.
    ///
    /// The `UNMAPPED` packet's room is promised on the server's port here,
    /// as the running task -- the mapping client -- makes the window, so a
    /// client cannot make a window whose end the server would not hear of.
    ///
    /// # Errors
    ///
    /// [`WindowError::Dead`] for a dead server, and
    /// [`WindowError::NoMemory`].
    pub(crate) fn new(server: &Arc<Server>, pages: u64) -> Result<Arc<FaultWindow>, WindowError> {
        if pages == 0 {
            return Err(WindowError::BadEntry);
        }
        let unmapped_charge = crate::object::quota::Charge::running(
            crate::object::quota::Resource::Memory,
            size_of::<PortPacket>() as u64,
        )
        .map_err(|_| WindowError::NoMemory)?;
        let promise = Promise::new(&server.port).map_err(|_| WindowError::NoMemory)?;
        let window = fallible::try_arc(FaultWindow {
            key: server.next_key.fetch_add(1, Ordering::Relaxed),
            pages,
            server: Arc::clone(server),
            state: SpinLock::new(State {
                chunks: BTreeMap::new(),
                mappers: Vec::new(),
                life: Life::Open,
                faults: BTreeMap::new(),
                next_token: 1,
            }),
            waiters: WaitQueue::new(),
            unmapped: SpinLock::new(Some(promise)),
            unmapped_charge,
        })
        .map_err(|_| WindowError::NoMemory)?;
        server.adopt(&window)?;
        Ok(window)
    }

    /// The key the server knows it by.
    pub(crate) fn key(&self) -> u64 {
        self.key
    }

    /// Its length in pages.
    pub(crate) fn pages(&self) -> u64 {
        self.pages
    }

    /// Its server.
    pub(crate) fn server(&self) -> &Arc<Server> {
        &self.server
    }

    /// The space at `space` maps it as `id`: one more mapping. Refused once
    /// it has closed.
    ///
    /// # Errors
    ///
    /// [`WindowError::Closed`], and [`WindowError::NoMemory`] when there is
    /// no room to list the mapper.
    pub(crate) fn attach(&self, space: Weak<AddressSpace>, id: u64) -> Result<(), WindowError> {
        let mut state = self.state.lock();
        // A dead window is still mapped, by the fork's parent at least, and
        // a child of it maps it too: its faults fail as the parent's do.
        if state.life == Life::Closed {
            return Err(WindowError::Closed);
        }
        // FALLIBLE: refused with nothing listed, as a VMO's mapper is.
        fallible::try_push(&mut state.mappers, Mapper { space, id })
            .map_err(|_| WindowError::NoMemory)
    }

    /// The space at `space` no longer maps it as `id`: its region left the
    /// map, or the space is being dropped.
    ///
    /// Allocates nothing and takes only this window's lock, so it may run
    /// under a space's lock and in a space's `Drop`. When it takes the last
    /// mapping away the window closes, and the `UNMAPPED` packet is queued on
    /// the room promised for it; a mapping that was never attached -- a fork
    /// that ran out of memory part-way -- changes nothing.
    ///
    /// The window's entries stay, holds and all: the space's translations
    /// are down but its shootdown has not run yet. They go when the server
    /// revokes them, when it dies, or when the window is dropped, which is
    /// after the last space that named it has let it go.
    pub(crate) fn detach(&self, space: *const AddressSpace, id: u64) {
        let closed = {
            let mut state = self.state.lock();
            let before = state.mappers.len();
            state
                .mappers
                .retain(|mapper| !(core::ptr::eq(mapper.space.as_ptr(), space) && mapper.id == id));
            let removed = state.mappers.len() != before;
            if removed && state.mappers.is_empty() && state.life == Life::Open {
                state.life = Life::Closed;
                true
            } else {
                false
            }
        };
        if closed {
            self.waiters.wake_all();
            let promise = self.unmapped.lock().take();
            if let Some(promise) = promise {
                promise.keep(PortPacket {
                    key: self.key,
                    kind: PACKET_WINDOW_UNMAPPED,
                    signals: 0,
                    data: [0; 2],
                });
            }
        }
    }

    /// How many mappings it has.
    pub(crate) fn mappings(&self) -> usize {
        self.state.lock().mappers.len()
    }

    /// Whether its server has died.
    pub(crate) fn is_dead(&self) -> bool {
        self.state.lock().life == Life::Dead
    }

    /// What a mapping at page `offset` shows, for `write`: the frame, whether
    /// it may be written, and whether it is uncached. `None` when the table
    /// lacks the page, or has it read-only and `write` asks to write.
    ///
    /// Called by the fault path under the space's lock: the order the module
    /// gives.
    pub(crate) fn shows(&self, offset: u64, write: bool) -> Option<(Frame, bool, bool)> {
        let state = self.state.lock();
        if state.life == Life::Dead {
            return None;
        }
        let entry = state.entry(offset)?;
        if write && !entry.write {
            return None;
        }
        Some((entry.frame, entry.write, entry.uncached))
    }

    /// Resolve a fault at page `offset` of this window as far as the server
    /// is concerned: `Ok` once the table has the page for `write`, or the
    /// server has answered, or the wait ended early -- each of which retries
    /// the access -- and the refusal otherwise.
    ///
    /// With [`Waits::Never`] nothing is queued and nobody waits: a page the
    /// table lacks is [`WindowError::Absent`]. With [`Waits::OnServer`] the
    /// fault is queued for the server, once, as a packet on the room promised
    /// out of the faulting job's charge, and the thread waits for the answer.
    ///
    /// Called with no lock held: the space's lock was let go after the region
    /// was found, as a file's page is filled.
    ///
    /// # Errors
    ///
    /// [`WindowError::Dead`] for a dead server or a fault from the server's
    /// own process, [`WindowError::Refused`] for the server's error answer,
    /// [`WindowError::Absent`] as above, and [`WindowError::NoMemory`] when
    /// the fault could not be noted.
    pub(crate) fn forward(&self, offset: u64, write: bool, waits: Waits) -> Result<(), WindowError> {
        if offset >= self.pages {
            return Err(WindowError::BadEntry);
        }
        {
            let state = self.state.lock();
            if state.life == Life::Dead {
                return Err(WindowError::Dead);
            }
            if state.entry(offset).is_some_and(|entry| !write || entry.write) {
                return Ok(());
            }
        }
        if waits == Waits::Never {
            return Err(WindowError::Absent);
        }
        let Some(thread) = running_thread() else {
            return Err(WindowError::Absent);
        };
        let host = thread.process();
        if core::ptr::eq(host.core(), self.server.process) {
            return Err(WindowError::Dead);
        }

        // The packet's room, promised before anything is noted: charged to
        // the faulting job, so a flood of faults is bounded by the faulting
        // job's own tasks and memory, never by the server's port.
        let charge = crate::object::quota::Charge::running(
            crate::object::quota::Resource::Memory,
            size_of::<PortPacket>() as u64,
        )
        .map_err(|_| WindowError::NoMemory)?;
        let promise = Promise::new(&self.server.port).map_err(|_| WindowError::NoMemory)?;
        let held = fallible::reserve().map_err(|_| WindowError::NoMemory)?;
        let token = {
            let mut state = self.state.lock();
            if state.life == Life::Dead {
                return Err(WindowError::Dead);
            }
            let token = state.next_token;
            state.next_token = state.next_token.wrapping_add(1).max(1);
            let _ = fallible::insert_held(&held, &mut state.faults, token, Answer::Waiting);
            token
        };
        drop(held);
        promise.keep(PortPacket {
            key: self.key,
            kind: PACKET_WINDOW_FAULT,
            signals: 0,
            data: [token, offset | if write { WINDOW_FAULT_WRITE } else { 0 }],
        });

        let _ = self.waiters.wait_until_deadline(
            || {
                let state = self.state.lock();
                state.life == Life::Dead
                    || state.faults.get(&token) != Some(&Answer::Waiting)
                    || host.wait_interrupted()
            },
            u64::MAX,
        );
        drop(charge);
        let mut state = self.state.lock();
        let answer = state.faults.remove(&token);
        if state.life == Life::Dead {
            return Err(WindowError::Dead);
        }
        match answer {
            Some(Answer::Refused) => Err(WindowError::Refused),
            // Answered, or the wait ended for a kill or a signal: back to
            // user mode, where the access faults again, or the thread ends.
            _ => Ok(()),
        }
    }

    /// The server's answer to the fault with `token`. An answer to a fault
    /// nobody waits for any more is ignored.
    pub(crate) fn answer(&self, token: u64, retry: bool) {
        let woken = {
            let mut state = self.state.lock();
            match state.faults.get_mut(&token) {
                Some(answer @ Answer::Waiting) => {
                    *answer = if retry { Answer::Retry } else { Answer::Refused };
                    true
                }
                _ => false,
            }
        };
        if woken {
            self.waiters.wake_all();
        }
    }

    /// Put `entries` into the table, all or nothing.
    ///
    /// Each names a page of a VMO the caller showed it holds -- checked
    /// above, where the handles are -- which must be committed, of anonymous
    /// memory, at a page offset inside the window. Each is held before the
    /// window's lock is taken, and the table's room is had too, so that
    /// nothing is changed until nothing can fail. A page offset that already
    /// has an entry is revoked first, through every client and a shootdown,
    /// so a translation is never rewritten in place (break before make).
    ///
    /// Must not be called holding a spin lock: holding pages takes a VMO's
    /// lock, and a replaced entry's revoke waits for a shootdown.
    ///
    /// # Errors
    ///
    /// [`WindowError::BadEntry`] for a bad entry, [`WindowError::Closed`] or
    /// [`WindowError::Dead`] for a window that is past serving, and
    /// [`WindowError::NoMemory`]. Nothing is inserted on any of them.
    pub(crate) fn insert(&self, entries: &[(u64, Arc<Vmo>, u64, bool)]) -> Result<(), WindowError> {
        if entries.is_empty() || entries.len() > INSERT_MAX {
            return Err(WindowError::BadEntry);
        }
        for (at, (offset, vmo, index, _)) in entries.iter().enumerate() {
            let twice = entries
                .get(..at)
                .is_some_and(|before| before.iter().any(|(other, ..)| other == offset));
            if *offset >= self.pages || twice || !vmo.is_anonymous() || vmo.page(*index).is_none() {
                return Err(WindowError::BadEntry);
            }
        }
        // Holds first, with no lock held.
        let mut made: Vec<(u64, Entry)> =
            fallible::try_with_capacity(entries.len()).map_err(|_| WindowError::NoMemory)?;
        for (offset, vmo, index, write) in entries {
            let held = vmo.hold(*index, 1).map_err(|_| WindowError::NoMemory)?;
            let &[frame] = held.frames() else {
                return Err(WindowError::NoMemory);
            };
            // NOALLOC: room for every entry was had above.
            made.push((
                *offset,
                Entry {
                    frame,
                    write: *write,
                    uncached: vmo.is_coherent(),
                    held,
                },
            ));
        }
        self.make_chunks(&made)?;

        // Entries already there leave first, through every client.
        let replaced = {
            let state = self.state.lock();
            match state.life {
                Life::Open => {}
                Life::Closed => return Err(WindowError::Closed),
                Life::Dead => return Err(WindowError::Dead),
            }
            made.iter()
                .any(|(offset, _)| state.entry(*offset).is_some())
        };
        if replaced {
            for (offset, _) in &made {
                self.revoke(*offset, 1);
            }
        }

        let mut state = self.state.lock();
        match state.life {
            Life::Open => {}
            Life::Closed => return Err(WindowError::Closed),
            Life::Dead => return Err(WindowError::Dead),
        }
        // A concurrent insert may have filled a slot since: refused whole,
        // its holds let go once the lock is.
        if made.iter().any(|(offset, _)| state.entry(*offset).is_some()) {
            return Err(WindowError::BadEntry);
        }
        for (offset, entry) in made {
            if let Some(slot) = state.slot(offset) {
                *slot = Some(entry);
            }
        }
        Ok(())
    }

    /// Make sure the table has a chunk for every offset in `made`: allocated
    /// with no lock held, and put in under the lock inside a reserved
    /// section, so that the insert proper allocates nothing.
    fn make_chunks(&self, made: &[(u64, Entry)]) -> Result<(), WindowError> {
        for (offset, _) in made {
            let chunk = offset / CHUNK_PAGES;
            if self.state.lock().chunks.contains_key(&chunk) {
                continue;
            }
            let fresh = Chunk::new().map_err(|_| WindowError::NoMemory)?;
            let held = fallible::reserve().map_err(|_| WindowError::NoMemory)?;
            let mut state = self.state.lock();
            if !state.chunks.contains_key(&chunk) {
                let _ = fallible::insert_held(&held, &mut state.chunks, chunk, fresh);
            }
        }
        Ok(())
    }

    /// Take the entries at page offsets `first..first + pages` out of the
    /// table and out of every client, and let their holds go once no
    /// processor can reach them. Offsets the table lacks are skipped.
    ///
    /// In batches of [`REVOKE_BATCH`], each in the order the module gives:
    /// the entries out under the window's lock; every mapper's translations
    /// of them down, each under its space's lock, through the walk every
    /// other way a page leaves a space takes (`forget_runs`); one shootdown
    /// for all of them; and only then the holds.
    ///
    /// Must not be called holding a spin lock.
    pub(crate) fn revoke(&self, first: u64, pages: u64) {
        let end = first.saturating_add(pages).min(self.pages);
        let mut from = first;
        // The batch's room, had once; with no memory for it, one entry at a
        // time from a list that always has room for one.
        let mut batch: Vec<(u64, Entry)> =
            fallible::try_with_capacity(REVOKE_BATCH).unwrap_or_default();
        if batch.capacity() == 0 && fallible::try_reserve(&mut batch, 1).is_err() {
            // Not even one: the entries stay, held, until the window is
            // dropped, which is after every space that maps it has let it
            // go. Counted, as the space's own unkept ranges are.
            let _ = UNREVOKED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let max = batch.capacity().min(REVOKE_BATCH);
        while from < end {
            batch.clear();
            self.state.lock().take_into(from, end, max, &mut batch);
            let Some(&(last, _)) = batch.last() else {
                return;
            };
            self.forget_everywhere(&batch);
            // The holds go here, after the shootdown above returned.
            from = last.saturating_add(1);
            batch.clear();
        }
    }

    /// Every mapper's translations of `entries` down, and one shootdown for
    /// all of them. Mappers attached meanwhile cannot have had the entries:
    /// they were out of the table before the first space was visited.
    fn forget_everywhere(&self, entries: &[(u64, Entry)]) {
        let mut runs = [(0, 0); REVOKE_BATCH];
        let count = runs_of(entries, &mut runs);
        let runs = runs.get(..count).unwrap_or(&[]);
        let mut cpus = CpuSet::empty();
        let mut pages = TlbPages::new();
        let mut forgotten: Vec<Arc<AddressSpace>> = Vec::new();
        let mut after = None;
        // The walk goes in order of a key that does not move, the space's
        // address and the id, so the mapper list may change between visits
        // and each mapper there all along is visited once.
        while let Some((space, id)) = self.next_mapper(after) {
            after = Some((Arc::as_ptr(&space).addr(), id));
            if let Some((theirs, _)) = space.forget_runs(id, runs, &mut pages, None) {
                smp::add_cpus(&mut cpus, &theirs);
                // With no room to remember it, its shootdown runs on its own.
                if let Err((space, _)) = try_keep(&mut forgotten, space) {
                    smp::flush_tlb_pages(&theirs, &mut pages);
                    space.flushed();
                }
            }
        }
        smp::flush_tlb_pages(&cpus, &mut pages);
        for space in forgotten {
            space.flushed();
        }
    }

    /// The live mapper with the least key past `after`.
    fn next_mapper(&self, after: Option<(usize, u64)>) -> Option<(Arc<AddressSpace>, u64)> {
        let state = self.state.lock();
        state
            .mappers
            .iter()
            .map(|mapper| ((mapper.space.as_ptr().addr(), mapper.id), mapper))
            .filter(|&(key, _)| after.is_none_or(|after| key > after))
            .filter(|(_, mapper)| mapper.space.strong_count() > 0)
            .min_by_key(|&(key, _)| key)
            .and_then(|(_, mapper)| Some((mapper.space.upgrade()?, mapper.id)))
    }

    /// Its server died: mark it, so faults stop waiting and inserts are
    /// refused, and wake the faults. Takes only its lock and the wait
    /// queue's; the revoke is the death task's.
    fn mark_dead(&self) {
        self.state.lock().life = Life::Dead;
        self.waiters.wake_all();
    }
}

/// Entries a revoke could not take out for want of memory (finding F-23):
/// they stay held until their window is dropped.
static UNREVOKED: AtomicU64 = AtomicU64::new(0);

/// Push `space` onto `list` if there is room for it, or give it back.
fn try_keep(
    list: &mut Vec<Arc<AddressSpace>>,
    space: Arc<AddressSpace>,
) -> Result<(), (Arc<AddressSpace>, ())> {
    if fallible::try_reserve(list, 1).is_err() {
        return Err((space, ()));
    }
    // NOALLOC: room for one was had above.
    list.push(space);
    Ok(())
}

/// The page offsets of `entries`, lowest first, as runs of `(first, count)`
/// in `out`: how many were written. `out` has room for every entry.
fn runs_of(entries: &[(u64, Entry)], out: &mut [(u64, u64)]) -> usize {
    let mut used = 0;
    for &(offset, _) in entries {
        let joined = used
            .checked_sub(1)
            .and_then(|at: usize| out.get_mut(at))
            .is_some_and(|(first, count)| {
                if first.saturating_add(*count) == offset {
                    *count += 1;
                    true
                } else {
                    false
                }
            });
        if !joined && let Some(slot) = out.get_mut(used) {
            *slot = (offset, 1);
            used += 1;
        }
    }
    used
}

/// The thread running, if a process's thread is.
fn running_thread() -> Option<Arc<dyn UserThread>> {
    crate::sched::current().and_then(|task| task.thread().cloned())
}

/// A server of fault windows: a driver's side. See the module.
#[derive(Debug)]
pub(crate) struct Server {
    /// Where its windows' packets go.
    port: Arc<Port>,
    /// Its process, for refusing a fault the server takes in a window it
    /// serves: compared, never followed.
    process: *const Process,
    /// The key the next window gets.
    next_key: AtomicU64,
    /// Its windows, by key. Weak: a window lives while a client maps it or
    /// the server is still to hear of it.
    windows: SpinLock<BTreeMap<u64, Weak<FaultWindow>>>,
    /// Set as its handle closes.
    dead: AtomicBool,
}

// SAFETY: (IDENTITY) `process` is an address compared for identity and never
// dereferenced; everything else is `Send + Sync` on its own.
unsafe impl Send for Server {}
// SAFETY: (IDENTITY) as for `Send`.
unsafe impl Sync for Server {}

impl Server {
    /// A server for the process `process`, whose packets go to `port`, and
    /// the handle that is its identity: what a subsystem in the load ring
    /// makes when a driver registers a device file whose faults it serves.
    ///
    /// # Errors
    ///
    /// [`AllocError`].
    pub(crate) fn new(port: Arc<Port>, process: &Process) -> Result<ServerHandle, AllocError> {
        start_death_task()?;
        reserve_death()?;
        let server = fallible::try_arc(Server {
            port,
            process,
            next_key: AtomicU64::new(1),
            windows: SpinLock::new(BTreeMap::new()),
            dead: AtomicBool::new(false),
        });
        match server {
            Ok(server) => Ok(ServerHandle(server)),
            Err(error) => {
                unreserve_death();
                Err(error)
            }
        }
    }

    /// Record `window` among this server's windows.
    fn adopt(&self, window: &Arc<FaultWindow>) -> Result<(), WindowError> {
        let held = fallible::reserve().map_err(|_| WindowError::NoMemory)?;
        let mut windows = self.windows.lock();
        if self.dead.load(Ordering::Acquire) {
            return Err(WindowError::Dead);
        }
        windows.retain(|_, window| window.strong_count() > 0);
        let _ = fallible::insert_held(&held, &mut windows, window.key, Arc::downgrade(window));
        Ok(())
    }

    /// Its window with `key`, if it still has one.
    pub(crate) fn window(&self, key: u64) -> Option<Arc<FaultWindow>> {
        self.windows.lock().get(&key)?.upgrade()
    }

    /// Whether its handle has closed.
    pub(crate) fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// The handle closed: every window is marked dead, under its lock, now;
    /// their revoke waits for the death task. Allocates nothing, takes only
    /// spin locks and waits for nothing, so it runs wherever a handle can be
    /// dropped.
    fn die(self: &Arc<Server>) {
        if self.dead.swap(true, Ordering::AcqRel) {
            return;
        }
        let windows = self.windows.lock();
        for window in windows.values().filter_map(Weak::upgrade) {
            window.mark_dead();
        }
        drop(windows);
        queue_death(Arc::clone(self));
    }

    /// Revoke every entry of every window, from the death task.
    fn revoke_all(&self) {
        let mut after = 0;
        loop {
            // One window at a time, the lock let go before each revoke.
            let next = {
                let windows = self.windows.lock();
                windows
                    .range(after..)
                    .find_map(|(&key, window)| Some((key, window.upgrade()?)))
            };
            let Some((key, window)) = next else {
                return;
            };
            window.revoke(0, window.pages);
            after = key.saturating_add(1);
        }
    }
}

/// The handle a server's driver holds, and whose close is the server's
/// death: see the module. Its rights carry neither duplicate nor transfer
/// (`Rights::WINDOW_SERVER`), so there is exactly one.
#[derive(Debug)]
pub(crate) struct ServerHandle(Arc<Server>);

impl ServerHandle {
    /// The server.
    pub(crate) fn server(&self) -> &Arc<Server> {
        &self.0
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.0.die();
    }
}

// ---------------------------------------------------------------------------
// The window death task: where a dead server's windows are revoked, in task
// context, with shootdowns.
// ---------------------------------------------------------------------------

/// Dead servers waiting for their revoke. Its room is reserved as each server
/// is made, so queueing a death never allocates.
static DEATHS: SpinLock<Vec<Arc<Server>>> = SpinLock::new(Vec::new());

/// Servers made and not yet revoked after their death: what [`DEATHS`] keeps
/// room for.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// Woken when a death is queued.
static DEATH_WAITERS: WaitQueue = WaitQueue::new();

/// Whether the death task started, once it was tried.
static DEATH_TASK: Once<bool> = Once::new();

/// Start the death task, once: no server is made without it.
fn start_death_task() -> Result<(), AllocError> {
    let started = *DEATH_TASK
        .call_once(|| crate::sched::spawn("window-death", death_task, 0, NICE_0_WEIGHT).is_ok());
    if started { Ok(()) } else { Err(AllocError) }
}

/// Room in [`DEATHS`] for one more server's death.
fn reserve_death() -> Result<(), AllocError> {
    let mut deaths = DEATHS.lock();
    let live = LIVE.load(Ordering::Acquire).saturating_add(1);
    fallible::try_reserve(&mut deaths, live.saturating_sub(deaths.len()))?;
    LIVE.store(live, Ordering::Release);
    Ok(())
}

/// Give back the room [`reserve_death`] had.
fn unreserve_death() {
    let _ = LIVE.fetch_sub(1, Ordering::AcqRel);
}

/// Queue `server`'s revoke for the death task, on the room reserved for it.
fn queue_death(server: Arc<Server>) {
    let refused = {
        let mut deaths = DEATHS.lock();
        fallible::push_within(&mut deaths, server).err()
    };
    if refused.is_some() {
        // The accounting says this cannot happen. The windows stay dead and
        // held until they are dropped.
        let _ = UNREVOKED.fetch_add(1, Ordering::Relaxed);
    }
    DEATH_WAITERS.wake_all();
}

/// The death task's body: revoke every dead server's windows, for ever.
fn death_task(_: usize) {
    loop {
        let _ = DEATH_WAITERS.wait_until_deadline(|| !DEATHS.lock().is_empty(), u64::MAX);
        let next = DEATHS.lock().pop();
        if let Some(server) = next {
            server.revoke_all();
            unreserve_death();
        }
    }
}
