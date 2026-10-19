//! File descriptor tables.
//!
//! A descriptor is a small integer naming an open file description, plus one
//! bit of its own: close-on-exec. Everything else — the offset, the access
//! mode, `O_APPEND` — belongs to the description and is shared by every
//! descriptor that names it.
//!
//! Generic over what a slot holds, so the numbering rules are tested here on
//! integers and the kernel stores `Arc<OpenFile>`. The rules are small and
//! every one of them is load-bearing for a shell:
//!
//! * a new descriptor is the **lowest free number** — `close(1); open(...)`
//!   is how a program redirects its own output, and `2>&1` depends on it;
//! * `dup2` onto a live descriptor replaces it, and hands the old description
//!   back so the caller can release it outside whatever lock holds the table;
//! * `F_DUPFD` finds the lowest free number *at or above* a minimum;
//! * a table has a limit, `RLIMIT_NOFILE`, and a full table is `EMFILE`.
//!
//! # Reserving a number before the file exists
//!
//! `open` has to know there is a free descriptor before it touches the path,
//! because what it does to the path is not undone: `open("new", O_CREAT |
//! O_EXCL)` refused for `EMFILE` after creating the file leaves the file
//! behind, and the program's retry is `EEXIST`. Nor can the table's lock be
//! held across the open, which walks filesystems and may wait. So the number
//! is taken first, as Linux's `get_unused_fd_flags` takes it, and filled once
//! there is something to put in it — or released if there is not.
//!
//! A reserved number is taken but names nothing. Linux's rules for one are
//! followed: it is not handed out again, `close` of it is `EBADF` and `dup2`
//! onto it `EBUSY`, and a `fork` in between does not copy it, since nothing
//! would ever fill the child's.

use alloc::vec::Vec;

use ferrix_kmem::{Charge, NOBODY, buffer_footprint};
use ferrix_linux_abi::errno::Errno;
use ferrix_sync::nospec;

use crate::Result;

/// `RLIMIT_NOFILE`'s soft default on Linux.
pub const DEFAULT_LIMIT: u32 = 1024;

/// The most `RLIMIT_NOFILE` may be raised to: Linux's default `nr_open`.
pub const MAX_LIMIT: u32 = 1 << 20;

/// One descriptor number in use.
#[derive(Debug, Clone)]
struct Slot<T> {
    /// What it names, or `None` while it is reserved.
    item: Option<T>,
    cloexec: bool,
}

/// A descriptor number [`FdTable::reserve`] took, to be spent on
/// [`FdTable::fill`] or [`FdTable::release`] of the same table.
///
/// Not `Clone`, so a number is filled or released once.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a reserved descriptor stays taken until it is filled or released"]
pub struct Reserved {
    index: usize,
    fd: i32,
}

impl Reserved {
    /// The number reserved.
    #[must_use]
    pub fn fd(&self) -> i32 {
        self.fd
    }
}

/// A process's descriptors.
///
/// [`FdTable::try_clone`] is `fork` without `CLONE_FILES`: the child gets
/// its own table naming the same descriptions, and none of the parent's
/// reservations.
#[derive(Debug)]
pub struct FdTable<T> {
    slots: Vec<Option<Slot<T>>>,
    limit: u32,
    /// Numbers in use, reserved ones included.
    open: usize,
    /// The room `slots` holds, charged to the job of whoever first grew the
    /// table -- the process itself, or its maker -- as it grows (F-37). A
    /// table is as large as its highest descriptor, which `dup2` may put
    /// anywhere below the limit.
    room: Charge,
}

impl<T> Default for FdTable<T> {
    fn default() -> Self {
        FdTable::new()
    }
}

impl<T: Clone> FdTable<T> {
    /// A copy naming the same items, and none of the reservations, charged
    /// to the running task's job: `fork`'s.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit, or with no memory.
    pub fn try_clone(&self) -> Result<Self> {
        let mut room = crate::charge(0)?;
        let mut slots = Vec::new();
        ferrix_kmem::reserve(&mut slots, self.slots.len(), &mut room).map_err(|_| Errno::ENOMEM)?;
        // Room was had just above: this does not grow the vector.
        slots.extend(
            self.slots
                .iter()
                .map(|slot| slot.as_ref().filter(|slot| slot.item.is_some()).cloned()),
        );
        let mut table = FdTable {
            slots,
            limit: self.limit,
            open: 0,
            room,
        };
        table.open = table.slots.iter().flatten().count();
        table.trim();
        Ok(table)
    }
}

impl<T> FdTable<T> {
    /// An empty table with the default limit.
    #[must_use]
    pub const fn new() -> FdTable<T> {
        FdTable {
            slots: Vec::new(),
            limit: DEFAULT_LIMIT,
            open: 0,
            room: Charge::none(),
        }
    }

    /// Make the table `len` slots long, charged.
    ///
    /// A table charged to nobody -- made empty, or given a new process's
    /// first descriptors by the kernel -- is charged, room and all, to the
    /// job of whoever first grows it, which is the one that fills it.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit, or with no memory.
    fn grow_to(&mut self, len: usize) -> Result<()> {
        let now = self.slots.len();
        if len <= now {
            return Ok(());
        }
        if self.room.owner() == NOBODY {
            self.room = crate::charge(buffer_footprint::<Option<Slot<T>>>(self.slots.capacity()))?;
        }
        ferrix_kmem::reserve(&mut self.slots, len - now, &mut self.room)
            .map_err(|_| Errno::ENOMEM)?;
        self.slots.resize_with(len, || None);
        Ok(())
    }

    /// The heap its slots hold and are charged for, in bytes.
    #[must_use]
    pub fn charged(&self) -> u64 {
        self.room.charged()
    }

    /// The limit on descriptor numbers: every descriptor is below it.
    #[must_use]
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Change the limit. Descriptors already at or above it stay open, as
    /// they do on Linux; only new ones are constrained.
    ///
    /// # Errors
    ///
    /// `EPERM` above [`MAX_LIMIT`].
    pub fn set_limit(&mut self, limit: u32) -> Result<()> {
        if limit > MAX_LIMIT {
            return Err(Errno::EPERM);
        }
        self.limit = limit;
        Ok(())
    }

    /// How many descriptor numbers are taken, reserved ones included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.open
    }

    /// Whether none are.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.open == 0
    }

    /// The live slot `fd` names; a reserved one names nothing yet.
    fn slot_mut(&mut self, fd: i32) -> Result<&mut Slot<T>> {
        let index = usize::try_from(fd).map_err(|_| Errno::EBADF)?;
        let index = nospec::bounded(index, self.slots.len()).ok_or(Errno::EBADF)?;
        self.slots
            .get_mut(index)
            .and_then(Option::as_mut)
            .filter(|slot| slot.item.is_some())
            .ok_or(Errno::EBADF)
    }

    fn slot(&self, fd: i32) -> Result<&Slot<T>> {
        let index = usize::try_from(fd).map_err(|_| Errno::EBADF)?;
        let index = nospec::bounded(index, self.slots.len()).ok_or(Errno::EBADF)?;
        self.slots
            .get(index)
            .and_then(Option::as_ref)
            .filter(|slot| slot.item.is_some())
            .ok_or(Errno::EBADF)
    }

    /// What `fd` names.
    ///
    /// # Errors
    ///
    /// `EBADF` for a negative, closed or reserved descriptor.
    pub fn get(&self, fd: i32) -> Result<&T> {
        self.slot(fd)
            .and_then(|slot| slot.item.as_ref().ok_or(Errno::EBADF))
    }

    /// Whether `fd` is close-on-exec.
    ///
    /// # Errors
    ///
    /// `EBADF` for a negative, closed or reserved descriptor.
    pub fn cloexec(&self, fd: i32) -> Result<bool> {
        self.slot(fd).map(|slot| slot.cloexec)
    }

    /// `fcntl(F_SETFD)`.
    ///
    /// # Errors
    ///
    /// `EBADF` for a negative, closed or reserved descriptor.
    pub fn set_cloexec(&mut self, fd: i32, cloexec: bool) -> Result<()> {
        self.slot_mut(fd)?.cloexec = cloexec;
        Ok(())
    }

    /// Install `item` at the lowest free descriptor.
    ///
    /// # Errors
    ///
    /// `EMFILE` if every number below the limit is taken.
    pub fn insert(&mut self, item: T, cloexec: bool) -> Result<i32> {
        self.insert_from(0, item, cloexec)
    }

    /// Install `item` at the lowest free descriptor at or above `min`:
    /// `fcntl(F_DUPFD)`.
    ///
    /// On an error `item` is dropped here, so a caller holding the table
    /// under a lock must hold another reference to it.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a negative `min` or one at or above the limit, `EMFILE`
    /// if nothing is free from there.
    pub fn insert_from(&mut self, min: i32, item: T, cloexec: bool) -> Result<i32> {
        let (index, fd) = self.lowest_free(min)?;
        self.take(index, Some(item), cloexec)?;
        Ok(fd)
    }

    /// Take the lowest free descriptor for an item not made yet, as `open`
    /// must; see the module documentation.
    ///
    /// # Errors
    ///
    /// `EMFILE` if every number below the limit is taken, `ENOMEM` if the
    /// table cannot grow to it.
    pub fn reserve(&mut self, cloexec: bool) -> Result<Reserved> {
        let (index, fd) = self.lowest_free(0)?;
        self.take(index, None, cloexec)?;
        Ok(Reserved { index, fd })
    }

    /// Put `item` in the number `reserved` holds, returning the number.
    ///
    /// # Errors
    ///
    /// `item` back if the number is not reserved in this table, which only a
    /// reservation made on another table can cause. It is handed back rather
    /// than dropped, for the caller to release outside the table's lock.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the reservation is spent: taking it by value is what stops one \
                  number being filled or released twice"
    )]
    pub fn fill(&mut self, reserved: Reserved, item: T) -> core::result::Result<i32, T> {
        match self.slots.get_mut(reserved.index) {
            Some(Some(slot)) if slot.item.is_none() => {
                slot.item = Some(item);
                Ok(reserved.fd)
            }
            _ => Err(item),
        }
    }

    /// Give back a number `reserve` took, for an item that never came.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the reservation is spent, as in `fill`"
    )]
    pub fn release(&mut self, reserved: Reserved) {
        if let Some(slot) = self.slots.get_mut(reserved.index)
            && slot.as_ref().is_some_and(|slot| slot.item.is_none())
        {
            *slot = None;
            self.open = self.open.saturating_sub(1);
            self.trim();
        }
    }

    /// The lowest number at or above `min` that is neither live nor reserved.
    fn lowest_free(&self, min: i32) -> Result<(usize, i32)> {
        let min = usize::try_from(min).map_err(|_| Errno::EINVAL)?;
        let limit = usize::try_from(self.limit).map_err(|_| Errno::EINVAL)?;
        if min >= limit {
            return Err(Errno::EINVAL);
        }
        let free = (min..limit)
            .find(|&index| self.slots.get(index).is_none_or(Option::is_none))
            .ok_or(Errno::EMFILE)?;
        Ok((free, i32::try_from(free).map_err(|_| Errno::EMFILE)?))
    }

    /// Mark the free number `index` taken, holding `item`.
    ///
    /// # Errors
    ///
    /// `ENOMEM` if the table cannot grow to it.
    fn take(&mut self, index: usize, item: Option<T>, cloexec: bool) -> Result<()> {
        self.grow_to(index.saturating_add(1))?;
        if let Some(slot) = self.slots.get_mut(index) {
            *slot = Some(Slot { item, cloexec });
            self.open = self.open.saturating_add(1);
        }
        Ok(())
    }

    /// Install `item` at exactly `fd`, replacing what was there: `dup2` and
    /// `dup3`. The replaced item is returned for the caller to release.
    ///
    /// # Errors
    ///
    /// `EBADF` for a negative `fd` or one at or above the limit, `EBUSY` for
    /// one an `open` has reserved and not yet filled, as on Linux.
    pub fn install(&mut self, fd: i32, item: T, cloexec: bool) -> Result<Option<T>> {
        let index = usize::try_from(fd).map_err(|_| Errno::EBADF)?;
        if index >= usize::try_from(self.limit).map_err(|_| Errno::EBADF)? {
            return Err(Errno::EBADF);
        }
        self.grow_to(index.saturating_add(1))?;
        let index = nospec::bounded(index, self.slots.len()).ok_or(Errno::EBADF)?;
        let slot = self.slots.get_mut(index).ok_or(Errno::EBADF)?;
        match slot {
            Some(held) if held.item.is_none() => Err(Errno::EBUSY),
            Some(held) => {
                held.cloexec = cloexec;
                Ok(held.item.replace(item))
            }
            None => {
                *slot = Some(Slot {
                    item: Some(item),
                    cloexec,
                });
                self.open = self.open.saturating_add(1);
                Ok(None)
            }
        }
    }

    /// `close`: remove `fd` and return what it named.
    ///
    /// # Errors
    ///
    /// `EBADF` for a negative, closed or reserved descriptor.
    pub fn remove(&mut self, fd: i32) -> Result<T> {
        let item = self.slot_mut(fd)?.item.take().ok_or(Errno::EBADF)?;
        if let Some(slot) = usize::try_from(fd)
            .ok()
            .and_then(|index| nospec::bounded(index, self.slots.len()))
            .and_then(|index| self.slots.get_mut(index))
        {
            *slot = None;
        }
        self.open = self.open.saturating_sub(1);
        self.trim();
        Ok(item)
    }

    /// Remove every close-on-exec descriptor, for `execve`, returning what
    /// they named.
    pub fn take_cloexec(&mut self) -> Vec<T> {
        let mut taken = Vec::new();
        for slot in &mut self.slots {
            if slot.as_ref().is_some_and(|s| s.cloexec && s.item.is_some())
                && let Some(item) = slot.take().and_then(|s| s.item)
            {
                taken.push(item);
            }
        }
        self.open = self.open.saturating_sub(taken.len());
        self.trim();
        taken
    }

    /// Drop the free slots at the end, so the table shrinks as it empties.
    fn trim(&mut self) {
        while self.slots.last().is_some_and(Option::is_none) {
            let _ = self.slots.pop();
        }
    }

    /// Every open descriptor, in ascending order; reserved ones name nothing
    /// and are not listed.
    pub fn iter(&self) -> impl Iterator<Item = (i32, &T)> {
        self.slots.iter().enumerate().filter_map(|(index, slot)| {
            let fd = i32::try_from(index).ok()?;
            slot.as_ref()
                .and_then(|slot| slot.item.as_ref())
                .map(|item| (fd, item))
        })
    }
}
