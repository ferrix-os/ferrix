//! Wait queues in the shared region, on K5's `ADDRESS_WAIT`
//! (docs/microkernel/M5-LIFT.md §3): a queue is a list of parked tasks under a
//! lock, each parked on its own word, so a wait costs no kernel object and a
//! wake that finds nobody costs no system call.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use super::task::Task;
use crate::fallible;
use crate::sync::SpinLock;

const RECHECK_NANOS: u64 = 5_000_000;
const IDLE: u8 = 0;
const LISTED: u8 = 1;
const WOKEN: u8 = 2;

#[derive(Debug)]
struct Inner {
    waiters: Vec<Arc<Task>>,
    marks: Vec<Arc<Mark>>,
}

#[derive(Debug)]
pub struct WaitQueue {
    inner: SpinLock<Inner>,
    any: AtomicBool,
    wakes: AtomicU64,
    ended: AtomicU32,
}

#[derive(Debug)]
struct Listed {
    serials: Vec<u64>,
    woken: bool,
}

#[derive(Debug)]
pub struct ReadyList {
    serials: SpinLock<Listed>,
    queue: Arc<WaitQueue>,
    overflowed: AtomicBool,
    tells: AtomicU64,
}

impl ReadyList {
    pub fn new() -> Result<Arc<ReadyList>, fallible::AllocError> {
        let queue = fallible::try_arc(WaitQueue::new())?;
        fallible::try_arc(ReadyList {
            serials: SpinLock::new(Listed {
                serials: Vec::new(),
                woken: false,
            }),
            queue,
            overflowed: AtomicBool::new(false),
            tells: AtomicU64::new(0),
        })
    }
    pub fn queue(&self) -> &Arc<WaitQueue> {
        &self.queue
    }
    pub fn reserve(&self, room: usize) -> Result<usize, fallible::AllocError> {
        let mut listed = self.serials.lock();
        let more = room.saturating_sub(listed.serials.len());
        fallible::try_reserve(&mut listed.serials, more)?;
        Ok(listed.serials.capacity())
    }
    pub fn take(&self, into: &mut Vec<u64>) -> bool {
        let mut overflowed = false;
        {
            let mut listed = self.serials.lock();
            for &serial in &listed.serials {
                overflowed |= fallible::push_within(into, serial).is_err();
            }
            listed.serials.clear();
            listed.woken = false;
        }
        self.overflowed.swap(false, Ordering::AcqRel) || overflowed
    }
    pub fn tells(&self) -> u64 {
        self.tells.load(Ordering::Relaxed)
    }
    pub fn crowd(&self) {
        let mut listed = self.serials.lock();
        while fallible::push_within(&mut listed.serials, u64::MAX).is_ok() {}
    }
}

#[derive(Debug)]
pub struct Mark {
    serial: u64,
    state: AtomicU8,
    list: Arc<ReadyList>,
}

impl Mark {
    pub fn new(list: &Arc<ReadyList>, serial: u64) -> Result<Arc<Mark>, fallible::AllocError> {
        fallible::try_arc(Mark {
            serial,
            state: AtomicU8::new(IDLE),
            list: Arc::clone(list),
        })
    }
    pub fn clear(&self) {
        let _ = self.state.swap(IDLE, Ordering::AcqRel);
    }
    /// A wake reached this mark's queue: list its serial once, wake its list.
    fn tell(&self) {
        let _ = self.list.tells.fetch_add(1, Ordering::Relaxed);
        if self.state.swap(WOKEN, Ordering::AcqRel) != IDLE {
            self.list.queue.wake_all();
            return;
        }
        {
            let mut listed = self.list.serials.lock();
            if fallible::push_within(&mut listed.serials, self.serial).is_err() {
                self.list.overflowed.store(true, Ordering::Release);
            }
        }
        self.list.queue.wake_all();
    }
}

impl WaitQueue {
    pub const fn new() -> WaitQueue {
        WaitQueue {
            inner: SpinLock::new(Inner {
                waiters: Vec::new(),
                marks: Vec::new(),
            }),
            any: AtomicBool::new(false),
            wakes: AtomicU64::new(0),
            ended: AtomicU32::new(0),
        }
    }
    pub fn hook(&self, mark: &Arc<Mark>) -> Result<(), fallible::AllocError> {
        let mut inner = self.inner.lock();
        fallible::try_push(&mut inner.marks, Arc::clone(mark))?;
        self.any.store(true, Ordering::SeqCst);
        Ok(())
    }
    pub fn unhook(&self, mark: &Arc<Mark>) {
        let mut inner = self.inner.lock();
        inner.marks.retain(|listed| !Arc::ptr_eq(listed, mark));
        self.any.store(
            !inner.marks.is_empty() || !inner.waiters.is_empty(),
            Ordering::SeqCst,
        );
    }
    fn list(&self, task: &Arc<Task>) {
        let mut inner = self.inner.lock();
        if !inner.waiters.iter().any(|listed| Arc::ptr_eq(listed, task)) {
            inner.waiters.push(Arc::clone(task));
        }
        self.any.store(true, Ordering::SeqCst);
    }
    fn unlist(&self, task: &Arc<Task>) {
        let mut inner = self.inner.lock();
        inner.waiters.retain(|listed| !Arc::ptr_eq(listed, task));
        self.any.store(
            !inner.marks.is_empty() || !inner.waiters.is_empty(),
            Ordering::SeqCst,
        );
    }
    pub fn wait_until_deadline(&self, ready: impl FnMut() -> bool, deadline: u64) -> bool {
        Self::wait_on_any(&[self], ready, deadline, RECHECK_NANOS)
    }
    pub fn wait_trusting(&self, ready: impl FnMut() -> bool) -> bool {
        Self::wait_on_any(&[self], ready, u64::MAX, u64::MAX)
    }
    pub fn wait_trusting_until(&self, ready: impl FnMut() -> bool, deadline: u64) -> bool {
        Self::wait_on_any(&[self], ready, deadline, u64::MAX)
    }
    pub fn wait_on_any(
        queues: &[&WaitQueue],
        mut ready: impl FnMut() -> bool,
        deadline: u64,
        recheck: u64,
    ) -> bool {
        if ready() {
            return true;
        }
        let Some(me) = super::current() else {
            // The first thread, before it has a task: there is nobody to wake it.
            while !ready() {
                if crate::timer::now_nanos() >= deadline {
                    return false;
                }
                super::yield_now();
            }
            return true;
        };
        for queue in queues {
            queue.list(&me);
        }
        let answer = loop {
            me.park.store(0, Ordering::SeqCst);
            me.blocked.store(true, Ordering::SeqCst);
            core::sync::atomic::fence(Ordering::SeqCst);
            if ready() {
                break true;
            }
            let now = crate::timer::now_nanos();
            if now >= deadline {
                break false;
            }
            let until = if recheck == u64::MAX {
                deadline
            } else {
                deadline.min(now.saturating_add(recheck))
            };
            let _ = me.voluntary.fetch_add(1, Ordering::Relaxed);
            me.note_parking();
            crate::sys::park(&me.park, until);
        };
        me.blocked.store(false, Ordering::SeqCst);
        for queue in queues {
            queue.unlist(&me);
        }
        answer
    }
    pub fn hooked_now(&self) -> u32 {
        self.inner.lock().marks.len() as u32
    }
    pub fn note_ended_by_a_wake(&self) {
        let _ = self.ended.fetch_add(1, Ordering::Relaxed);
    }
    pub fn wake_all_with(&self, how: super::Wake) {
        self.wake_all();
    }
    pub fn waits_ended_by_a_wake(&self) -> u32 {
        self.ended.load(Ordering::Relaxed)
    }
    pub fn listed(&self) -> usize {
        self.inner.lock().waiters.len()
    }
    pub fn listed_now(&self) -> usize {
        self.listed()
    }
    pub fn wakes(&self) -> u64 {
        self.wakes.load(Ordering::Relaxed)
    }
    pub fn wake_all(&self) {
        core::sync::atomic::fence(Ordering::SeqCst);
        if !self.any.load(Ordering::SeqCst) {
            return;
        }
        let _ = self.wakes.fetch_add(1, Ordering::Relaxed);
        let (waiters, marks) = {
            let inner = self.inner.lock();
            (inner.waiters.clone(), inner.marks.clone())
        };
        for task in &waiters {
            super::wake(task);
        }
        for mark in &marks {
            mark.tell();
        }
    }
}

impl ferrix_sync::Parking for WaitQueue {
    fn park_until(&self, ready: &mut dyn FnMut() -> bool) {
        let _ = self.wait_until_deadline(ready, u64::MAX);
    }
    fn unpark_all(&self) {
        self.wake_all();
    }
}
