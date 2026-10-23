//! The worker's side (`docs/INTEL-GPU.md` §12.3): the keeper's calls run on
//! a [`Backend`] that drives the GT -- `XelpBackend` in `intel-gt`, the fake
//! in the stand-in's worker -- and what it finishes goes back as events.
//!
//! The keeper's ids are kept apart from the back end's: each worker maps
//! them to its own, so a new worker that is given the journal holds the
//! same objects under the same keeper ids. A submission the back end has
//! no room for waits here, in order, and is offered again after each
//! service, so the keeper never hears `Full`.

use super::wire::{Call, Event, errno_field, kind};
use crate::frontend::limits;
use crate::frontend::{Backend, Completion, ContextParams, HwContext, SubmitError, VmId};

/// A submission waiting for room.
#[derive(Clone, Copy, Debug)]
struct Queued {
    context: HwContext,
    batch: u64,
    length: u32,
    request: u64,
}

/// The worker: a back end and the keeper's ids mapped to it.
pub struct Worker<B: Backend> {
    backend: B,
    vms: [Option<VmId>; limits::VMS],
    contexts: [Option<HwContext>; limits::CONTEXTS],
    queued: [Option<Queued>; limits::REQUESTS],
    head: usize,
    tail: usize,
}

impl<B: Backend> core::fmt::Debug for Worker<B> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Worker")
            .field("queued", &(self.tail - self.head))
            .finish_non_exhaustive()
    }
}

/// The slot of keeper id `id` (ids are from 1).
const fn slot(id: u32) -> usize {
    (id as usize).wrapping_sub(1)
}

impl<B: Backend> Worker<B> {
    /// A worker over `backend`, holding nothing yet.
    pub fn new(backend: B) -> Self {
        Worker {
            backend,
            vms: [None; limits::VMS],
            contexts: [None; limits::CONTEXTS],
            queued: [None; limits::REQUESTS],
            head: 0,
            tail: 0,
        }
    }

    /// The back end.
    pub fn backend(&mut self) -> &mut B {
        &mut self.backend
    }

    /// The back end's VM for keeper VM `vm`.
    #[must_use]
    pub fn vm(&self, vm: u32) -> Option<VmId> {
        self.vms.get(slot(vm)).copied().flatten()
    }

    /// READY: the back end's facts, whether every inherited engine was
    /// reset, and a timestamp sample.
    pub fn ready(&mut self, reset_done: bool, now_ns: u64) -> Event {
        Event::Ready {
            facts: *self.backend.facts(),
            reset_done,
            timestamp: self.backend.timestamp(),
            now_ns,
        }
    }

    /// One call from the keeper; a bind's memory, from its handle, beside
    /// it. What it produced goes to `out`.
    pub fn call(
        &mut self,
        call: &Call,
        memory: Option<&B::Memory>,
        now_ns: u64,
        out: &mut dyn FnMut(Event),
    ) {
        match *call {
            Call::Hello { .. } => {}
            Call::VmCreate { vm } => self.vm_create(vm, out),
            Call::VmDestroy { vm } => {
                if let Some(id) = self.vms.get_mut(slot(vm)).and_then(Option::take) {
                    self.backend.vm_destroy(id);
                }
            }
            Call::Bind {
                vm,
                address,
                bytes,
                pat_index,
                ..
            } => {
                let mapped = self.vms.get(slot(vm)).copied().flatten();
                let result = match (mapped, memory) {
                    (Some(id), Some(memory)) => {
                        self.backend.bind(id, address, memory, bytes, pat_index)
                    }
                    _ => Err(ferrix_linux_abi::errno::Errno::EINVAL),
                };
                if let Err(errno) = result {
                    out(Event::Refused {
                        kind: kind::BIND,
                        id: address,
                        errno: errno_field(errno),
                    });
                }
            }
            Call::Unbind { vm, address, bytes } => {
                if let Some(id) = self.vms.get(slot(vm)).copied().flatten() {
                    self.backend.unbind(id, address, bytes);
                }
            }
            Call::ContextCreate {
                context,
                vm,
                engine,
                ring_bytes,
            } => self.context_create(context, vm, engine, ring_bytes, out),
            Call::ContextDestroy { context } => {
                if let Some(hw) = self.contexts.get_mut(slot(context)).and_then(Option::take) {
                    self.backend.context_destroy(hw);
                }
            }
            Call::Submit {
                context,
                batch,
                length,
                request,
            } => self.submit(context, batch, length, request, out),
            Call::Interrupt => self.service(now_ns, true, out),
            Call::Ping { seq } => out(Event::Pong {
                seq,
                timestamp: self.backend.timestamp(),
                now_ns,
            }),
            Call::Held(on) => self.backend.set_held(on),
            Call::Watched(on) => self.backend.set_watched(on),
            Call::Boost { request } => self.backend.boost(request),
        }
    }

    fn vm_create(&mut self, vm: u32, out: &mut dyn FnMut(Event)) {
        let refused = |errno| Event::Refused {
            kind: kind::VM_CREATE,
            id: u64::from(vm),
            errno: errno_field(errno),
        };
        let Some(cell) = self.vms.get_mut(slot(vm)) else {
            out(refused(ferrix_linux_abi::errno::Errno::EINVAL));
            return;
        };
        if let Some(old) = cell.take() {
            self.backend.vm_destroy(old);
        }
        match self.backend.vm_create() {
            Ok(id) => {
                if let Some(cell) = self.vms.get_mut(slot(vm)) {
                    *cell = Some(id);
                }
            }
            Err(errno) => out(refused(errno)),
        }
    }

    fn context_create(
        &mut self,
        context: u32,
        vm: u32,
        engine: crate::frontend::Engine,
        ring_bytes: u32,
        out: &mut dyn FnMut(Event),
    ) {
        let mapped = self.vms.get(slot(vm)).copied().flatten();
        let result = match mapped {
            Some(id) if slot(context) < limits::CONTEXTS => {
                self.backend
                    .context_create(id, engine, ContextParams { ring_bytes })
            }
            _ => Err(ferrix_linux_abi::errno::Errno::EINVAL),
        };
        match result {
            Ok(hw) => {
                if let Some(cell) = self.contexts.get_mut(slot(context)) {
                    *cell = Some(hw);
                }
            }
            Err(errno) => out(Event::Refused {
                kind: kind::CONTEXT_CREATE,
                id: u64::from(context),
                errno: errno_field(errno),
            }),
        }
    }

    fn submit(
        &mut self,
        context: u32,
        batch: u64,
        length: u32,
        request: u64,
        out: &mut dyn FnMut(Event),
    ) {
        let Some(hw) = self.contexts.get(slot(context)).copied().flatten() else {
            out(Event::Refused {
                kind: kind::SUBMIT,
                id: request,
                errno: errno_field(ferrix_linux_abi::errno::Errno::ENOENT),
            });
            return;
        };
        let queued = Queued {
            context: hw,
            batch,
            length,
            request,
        };
        if self.head == self.tail && self.offer(queued, out) {
            return;
        }
        // Behind what already waits, so each context's order holds.
        if self.tail - self.head < limits::REQUESTS {
            if let Some(cell) = self.queued.get_mut(self.tail % limits::REQUESTS) {
                *cell = Some(queued);
            }
            self.tail += 1;
        } else {
            out(Event::Refused {
                kind: kind::SUBMIT,
                id: request,
                errno: errno_field(ferrix_linux_abi::errno::Errno::ENOMEM),
            });
        }
    }

    /// Offer `queued` to the back end: `false` when it has no room.
    fn offer(&mut self, queued: Queued, out: &mut dyn FnMut(Event)) -> bool {
        match self
            .backend
            .submit(queued.context, queued.batch, queued.length, queued.request)
        {
            Ok(()) => true,
            Err(SubmitError::Full) => false,
            Err(SubmitError::Failed(_)) => {
                // Refused for good, as the front end would complete it with
                // the error: EIO, as for a request its reset made guilty.
                out(Event::Completed {
                    request: queued.request,
                    how: Completion::Hung,
                });
                true
            }
        }
    }

    /// Let the GT run, offer what waits again, and say what finished.
    pub fn service(&mut self, now_ns: u64, interrupt: bool, out: &mut dyn FnMut(Event)) {
        self.backend.service(now_ns, interrupt);
        self.backend.completed(&mut |request, how| {
            out(Event::Completed { request, how });
        });
        while self.head < self.tail {
            let Some(queued) = self
                .queued
                .get(self.head % limits::REQUESTS)
                .copied()
                .flatten()
            else {
                self.head += 1;
                continue;
            };
            if !self.offer(queued, out) {
                break;
            }
            if let Some(cell) = self.queued.get_mut(self.head % limits::REQUESTS) {
                *cell = None;
            }
            self.head += 1;
        }
    }

    /// When [`Worker::service`] must next run: the back end's deadline.
    #[must_use]
    pub fn next_deadline(&self) -> Option<u64> {
        self.backend.next_deadline()
    }
}
