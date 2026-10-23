//! Host tests for the opaque crash mode's protocol: the records, the
//! journal, and a keeper over workers on the engine model, killed with
//! requests in flight and replaced, against a run in which nothing died.

extern crate std;

use std::collections::{BTreeMap, VecDeque};
use std::format;
use std::string::String;
use std::vec::Vec;

use ferrix_linux_abi::errno::Errno;

use super::keeper::{GiveUp, Journal, KeeperLink, MAX_RESTARTS, PING_NS, RemoteBackend, TDR_NS};
use super::wire::{self, Call, Event, Malformed, UAPI_ENGINES, kind};
use super::worker::Worker;
use crate::backend::{Identity, XelpBackend};
use crate::commands::{Cs, emit_store_dword, gpu};
use crate::engine::ENGINES;
use crate::frontend::{Backend, Completion, ContextParams, Engine, Facts, VmId};
use crate::tests_backend::Model;

const RENDER: Engine = Engine {
    class: 0,
    instance: 0,
};

fn model_backend(revision: u8) -> XelpBackend<Model> {
    XelpBackend::new(
        Model::new(),
        0b11,
        &ENGINES[..4],
        1,
        Identity {
            device_id: 0x46d4,
            revision,
            ggtt_bytes: 4 << 30,
            system_memory_bytes: 16 << 30,
        },
    )
    .expect("the GT comes up")
}

/// The keeper's program, in one process: the worker a field, its death a
/// flag, every record encoded and decoded on its way.
#[derive(Default)]
struct Link {
    worker: Option<Worker<XelpBackend<Model>>>,
    /// The keeper's memory: each object's pages, by token.
    memories: Vec<Vec<u64>>,
    next_page: u64,
    /// Client pages while no worker's model holds them: memory is the
    /// keeper's and outlives every worker.
    parked: BTreeMap<u64, Vec<u32>>,
    outbox: Vec<([u8; wire::RECORD_BYTES], Option<usize>)>,
    inbox: VecDeque<[u8; wire::RECORD_BYTES]>,
    dead: bool,
    /// A worker that takes calls and answers nothing.
    deaf: bool,
    /// The next worker's revision, and whether it says its inherited
    /// engines were reset.
    revision: u8,
    reset_done: bool,
    started: Vec<u32>,
    gave_up: Option<GiveUp>,
    now: u64,
    lines: Vec<String>,
}

impl Link {
    fn new() -> Self {
        Link {
            next_page: 0x8000_0000,
            reset_done: true,
            ..Link::default()
        }
    }

    fn model(&mut self) -> &mut Model {
        self.worker.as_mut().expect("a worker").backend().platform()
    }

    /// A client page's words, wherever it is now.
    fn page(&mut self, page: u64) -> &mut Vec<u32> {
        if self.worker.is_some() {
            self.model().gpu.pages.get_mut(&page).expect("a page")
        } else {
            self.parked.get_mut(&page).expect("a page")
        }
    }

    /// The worker dies, as `kill -9`: its model's client pages stay, as
    /// the keeper's memory does.
    fn kill(&mut self) {
        if let Some(mut worker) = self.worker.take() {
            let pages = core::mem::take(&mut worker.backend().platform().gpu.pages);
            self.parked.extend(pages);
        }
        self.dead = true;
    }

    /// Let the GT run what it was given.
    fn run(&mut self) {
        if self.worker.is_some() {
            let _ = self.model().gpu.run();
        }
    }
}

impl KeeperLink for Link {
    type Memory = usize;
    type Token = usize;

    fn create_memory(&mut self, bytes: u64) -> Result<usize, Errno> {
        let pages: Vec<u64> = (0..bytes / 4096)
            .map(|n| self.next_page + n * 4096)
            .collect();
        self.next_page += bytes;
        for &page in &pages {
            if self.worker.is_some() {
                self.model().gpu.add_page(page);
            } else {
                let _ = self.parked.insert(page, std::vec![0; 1024]);
            }
        }
        self.memories.push(pages);
        Ok(self.memories.len() - 1)
    }

    fn token(&mut self, memory: &usize) -> Result<(usize, u64), Errno> {
        let pages = self.memories.get(*memory).ok_or(Errno::EINVAL)?.len();
        Ok((*memory, pages as u64))
    }

    fn send(&mut self, call: &Call, token: Option<usize>) -> bool {
        assert_eq!(call.carries_handle(), token.is_some(), "{call:?}");
        if self.dead {
            return false;
        }
        self.outbox.push((call.encode(), token));
        true
    }

    fn flush(&mut self) -> bool {
        if self.dead {
            self.outbox.clear();
            return false;
        }
        let Link {
            worker,
            memories,
            outbox,
            inbox,
            deaf,
            now,
            ..
        } = self;
        let Some(worker) = worker.as_mut() else {
            return false;
        };
        for (record, token) in outbox.drain(..) {
            let call = Call::decode(&record).expect("a call the keeper wrote");
            let memory = token.map(|token| memories[token].clone());
            if *deaf {
                continue;
            }
            worker.call(&call, memory.as_ref(), *now, &mut |event| {
                inbox.push_back(event.encode());
            });
        }
        true
    }

    fn receive(&mut self, sink: &mut dyn FnMut(Event)) -> bool {
        while let Some(record) = self.inbox.pop_front() {
            sink(Event::decode(&record).expect("an event the worker wrote"));
        }
        !self.dead
    }

    fn start_worker(&mut self, generation: u32) -> bool {
        self.kill();
        let mut backend = model_backend(self.revision);
        backend.platform().gpu.pages = core::mem::take(&mut self.parked);
        let mut worker = Worker::new(backend);
        let ready = worker.ready(self.reset_done, self.now);
        self.inbox.clear();
        self.inbox.push_back(ready.encode());
        self.worker = Some(worker);
        self.dead = false;
        self.started.push(generation);
        true
    }

    fn wait(&mut self, until_ns: u64) {
        self.now = self.now.max(until_ns);
    }

    fn now_ns(&mut self) -> u64 {
        self.now
    }

    fn say(&mut self, line: core::fmt::Arguments<'_>) {
        self.lines.push(format!("{line}"));
    }

    fn gave_up(&mut self, why: GiveUp) {
        self.gave_up = Some(why);
    }
}

/// Run `body` on a thread with room for the keeper's and the worker's
/// tables, as the driver's process has.
fn on_big_stack(body: fn()) {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(body)
        .expect("a thread")
        .join()
        .expect("the test passed");
}

/// A journal of the test's own, as a program's `static` is.
fn journal() -> &'static mut Journal<usize> {
    std::boxed::Box::leak(std::boxed::Box::new(Journal::new()))
}

fn keeper(link: Link) -> RemoteBackend<'static, Link> {
    RemoteBackend::start(link, journal())
        .map_err(|(_, why)| why)
        .expect("up")
}

/// A batch at `at` storing `value` at `target`, both bound in `vm`.
fn store(
    keeper: &mut RemoteBackend<'static, Link>,
    vm: VmId,
    at: u64,
    target: u64,
    value: u32,
) -> usize {
    let batch = keeper.create_memory(4096).expect("memory");
    let out = keeper.create_memory(4096).expect("memory");
    let mut words = [0_u32; 6];
    let mut cs = Cs::new(&mut words);
    emit_store_dword(&mut cs, target, value);
    cs.push(gpu::MI_BATCH_BUFFER_END);
    let page = keeper.link().memories[batch][0];
    keeper.link().page(page)[..6].copy_from_slice(&words);
    keeper.bind(vm, at, &batch, 4096, 0).expect("bound");
    keeper
        .bind(vm, target & !0xfff, &out, 4096, 0)
        .expect("bound");
    out
}

/// Run the GT and service the keeper until nothing moves; every
/// completion handed to the front end, in order.
fn settle(keeper: &mut RemoteBackend<'static, Link>, done: &mut Vec<(u64, Completion)>) {
    for _ in 0..32 {
        keeper.link().run();
        keeper.link().now += 1_000;
        let now = keeper.link().now;
        keeper.service(now, true);
        keeper.completed(&mut |id, how| done.push((id, how)));
    }
}

/// Four requests on two contexts, a worker killed (or not) after the
/// first ran, and what the GPU did.
fn scenario(
    kill: bool,
) -> (
    Vec<(u64, Completion)>,
    Vec<u32>,
    RemoteBackend<'static, Link>,
) {
    let mut keeper = keeper(Link::new());
    let vm = keeper.vm_create().expect("vm");
    let mut outs = Vec::new();
    for n in 0..4_u64 {
        let at = 0x1_0000_0000 + n * 0x10_0000;
        outs.push(store(&mut keeper, vm, at, at + 0x8_0040, 0xbe00 + n as u32));
    }
    let a = keeper
        .context_create(vm, RENDER, ContextParams::DEFAULT)
        .expect("a");
    let b = keeper
        .context_create(vm, RENDER, ContextParams::DEFAULT)
        .expect("b");
    keeper.submit(a, 0x1_0000_0000, 6 * 4, 1).expect("1");
    let mut done = Vec::new();
    settle(&mut keeper, &mut done);
    assert_eq!(done, [(1, Completion::Ok)]);
    for (n, context) in [(1, a), (2, b), (3, a)] {
        let batch = 0x1_0000_0000 + n * 0x10_0000;
        keeper.submit(context, batch, 6 * 4, n + 1).expect("taken");
    }
    keeper.flush();
    if kill {
        keeper.link().kill();
    }
    settle(&mut keeper, &mut done);
    let stored = outs
        .iter()
        .map(|&out| {
            let page = keeper.link().memories[out][0];
            keeper.link().page(page)[0x10]
        })
        .collect();
    (done, stored, keeper)
}

#[test]
fn every_record_round_trips() {
    on_big_stack(every_record_round_trips_body);
}

fn every_record_round_trips_body() {
    let facts = *model_backend(0).facts();
    let calls = [
        Call::Hello {
            generation: 3,
            location: 0x10,
            device_id: 0x46d4,
            revision: 1,
            system_memory_bytes: 16 << 30,
        },
        Call::VmCreate { vm: 7 },
        Call::VmDestroy { vm: 7 },
        Call::Bind {
            vm: 1,
            address: 0x1_2345_6000,
            bytes: 1 << 21,
            pages: 512,
            pat_index: 3,
        },
        Call::Unbind {
            vm: 1,
            address: 0x1_2345_6000,
            bytes: 1 << 21,
        },
        Call::ContextCreate {
            context: 9,
            vm: 1,
            engine: Engine {
                class: 1,
                instance: 0,
            },
            ring_bytes: 16 << 10,
        },
        Call::ContextDestroy { context: 9 },
        Call::Submit {
            context: 9,
            batch: 0xdead_b000,
            length: 24,
            request: u64::MAX - 1,
        },
        Call::Interrupt,
        Call::Ping { seq: 42 },
        Call::Held(true),
        Call::Watched(false),
        Call::Boost { request: 5 },
    ];
    for call in calls {
        assert_eq!(Call::decode(&call.encode()), Ok(call));
    }
    let events = [
        Event::Ready {
            facts,
            reset_done: true,
            timestamp: 123,
            now_ns: 456,
        },
        Event::Completed {
            request: 77,
            how: Completion::Cancelled,
        },
        Event::Pong {
            seq: 1,
            timestamp: 2,
            now_ns: 3,
        },
        Event::Refused {
            kind: kind::BIND,
            id: 0x1000,
            errno: 12,
        },
    ];
    for event in events {
        assert_eq!(Event::decode(&event.encode()), Ok(event));
    }
    // A call is not an event, nor the other way round; nothing short reads.
    assert_eq!(
        Event::decode(&Call::Interrupt.encode()),
        Err(Malformed::Kind)
    );
    assert_eq!(Call::decode(&events[1].encode()), Err(Malformed::Kind));
    assert_eq!(Call::decode(&[kind::SUBMIT, 1, 2]), Err(Malformed::Kind));
    // An engine list that is not a prefix of the uAPI's order is refused.
    let mut odd = events[0].encode();
    let at = odd.len() - wire::RECORD_BYTES + 57;
    odd[at] = 1;
    assert_eq!(Event::decode(&odd), Err(Malformed::Field));
    assert_eq!(facts.engines, &UAPI_ENGINES[..1]);
}

#[test]
fn a_batch_holds_records_and_at_most_sixty_four_handles() {
    on_big_stack(a_batch_holds_records_and_at_most_sixty_four_handles_body);
}

fn a_batch_holds_records_and_at_most_sixty_four_handles_body() {
    let mut batch = wire::Batch::default();
    let bind = Call::Bind {
        vm: 1,
        address: 0,
        bytes: 4096,
        pages: 1,
        pat_index: 0,
    }
    .encode();
    for _ in 0..wire::BATCH_HANDLES {
        assert!(batch.push(&bind, true));
    }
    assert!(!batch.push(&bind, true), "a 65th handle");
    assert!(batch.push(&Call::Interrupt.encode(), false));
    assert_eq!(batch.len(), wire::BATCH_HANDLES + 1);
    let records: Vec<_> = wire::records(batch.as_bytes()).collect();
    assert_eq!(Call::decode(records[0]), Call::decode(&bind));
    assert_eq!(Call::decode(records[64]), Ok(Call::Interrupt));
    batch.clear();
    assert!(batch.is_empty());
}

#[test]
fn a_worker_killed_with_requests_in_flight_changes_nothing_a_program_sees() {
    on_big_stack(a_worker_killed_with_requests_in_flight_changes_nothing_a_program_sees_body);
}

fn a_worker_killed_with_requests_in_flight_changes_nothing_a_program_sees_body() {
    let (clean, stored_clean, clean_keeper) = scenario(false);
    let (killed, stored_killed, mut keeper) = scenario(true);
    assert_eq!(clean_keeper.restarts(), 0);
    assert_eq!(keeper.restarts(), 1);
    assert_eq!(keeper.link().started, [1, 2]);
    // Every request completed once, Ok, and each context's in order.
    for done in [&clean, &killed] {
        let mut ids: Vec<u64> = done.iter().map(|(id, _)| *id).collect();
        assert!(
            done.iter().all(|(_, how)| *how == Completion::Ok),
            "{done:?}"
        );
        let pos = |id| ids.iter().position(|&x| x == id).expect("done");
        assert!(pos(2) < pos(4), "{done:?}");
        ids.sort_unstable();
        assert_eq!(ids, [1, 2, 3, 4]);
    }
    // Every store landed where it did in the run nothing interrupted.
    assert_eq!(stored_killed, stored_clean);
    assert_eq!(stored_clean, [0xbe00, 0xbe01, 0xbe02, 0xbe03]);
    let said = keeper.link().lines.join("\n");
    assert!(said.contains("worker 1 is replaced (Died)"), "{said}");
    assert!(
        said.contains("worker 2 is up")
            && said.contains("replayed 1 VMs, 8 bindings, 2 contexts, 3 requests"),
        "{said}"
    );
}

#[test]
fn the_new_workers_page_tables_map_what_the_dead_ones_did() {
    on_big_stack(the_new_workers_page_tables_map_what_the_dead_ones_did_body);
}

fn the_new_workers_page_tables_map_what_the_dead_ones_did_body() {
    let (_, _, mut keeper) = scenario(true);
    let memories = keeper.link().memories.clone();
    let worker = keeper.link().worker.as_mut().expect("a worker");
    let vm = worker.vm(1).expect("keeper VM 1 is in the new worker");
    for n in 0..4_u64 {
        let at = 0x1_0000_0000 + n * 0x10_0000;
        let batch = worker.backend().translate(vm.0 as usize, at + 0x10);
        assert_eq!(batch, Some(memories[2 * n as usize][0] + 0x10));
        let out = worker.backend().translate(vm.0 as usize, at + 0x8_0040);
        assert_eq!(out, Some(memories[2 * n as usize + 1][0] + 0x40));
    }
}

#[test]
fn a_completion_is_handed_over_once_whichever_worker_says_it() {
    on_big_stack(a_completion_is_handed_over_once_whichever_worker_says_it_body);
}

fn a_completion_is_handed_over_once_whichever_worker_says_it_body() {
    let mut keeper = keeper(Link::new());
    let vm = keeper.vm_create().expect("vm");
    let _ = store(&mut keeper, vm, 0x1_0000_0000, 0x1_0008_0040, 1);
    let c = keeper
        .context_create(vm, RENDER, ContextParams::DEFAULT)
        .expect("context");
    keeper.submit(c, 0x1_0000_0000, 24, 5).expect("taken");
    let mut done = Vec::new();
    settle(&mut keeper, &mut done);
    assert_eq!(done, [(5, Completion::Ok)]);
    // A dead worker's late word, and a replayed request said again.
    for _ in 0..2 {
        let late = Event::Completed {
            request: 5,
            how: Completion::Ok,
        };
        keeper.link().inbox.push_back(late.encode());
        settle(&mut keeper, &mut done);
    }
    assert_eq!(done, [(5, Completion::Ok)]);
}

#[test]
fn calls_made_while_a_worker_starts_are_journalled_and_reach_the_next() {
    on_big_stack(calls_made_while_a_worker_starts_are_journalled_and_reach_the_next_body);
}

fn calls_made_while_a_worker_starts_are_journalled_and_reach_the_next_body() {
    let mut keeper = keeper(Link::new());
    let vm = keeper.vm_create().expect("vm");
    keeper.link().kill();
    keeper.link().now += 1;
    let now = keeper.link().now;
    keeper.service(now, false);
    assert_eq!(keeper.link().started, [1, 2]);
    // The new worker's READY is waiting in the link; these come first.
    let _ = store(&mut keeper, vm, 0x1_0000_0000, 0x1_0008_0040, 0x5a);
    let c = keeper
        .context_create(vm, RENDER, ContextParams::DEFAULT)
        .expect("context");
    keeper.submit(c, 0x1_0000_0000, 24, 9).expect("taken");
    let mut done = Vec::new();
    settle(&mut keeper, &mut done);
    assert_eq!(done, [(9, Completion::Ok)]);
}

#[test]
fn the_ninth_death_gives_up_and_fails_what_is_in_flight() {
    on_big_stack(the_ninth_death_gives_up_and_fails_what_is_in_flight_body);
}

fn the_ninth_death_gives_up_and_fails_what_is_in_flight_body() {
    let mut keeper = keeper(Link::new());
    let vm = keeper.vm_create().expect("vm");
    let c = keeper
        .context_create(vm, RENDER, ContextParams::DEFAULT)
        .expect("context");
    keeper.submit(c, 0x1_0000_0000, 24, 3).expect("taken");
    let mut done = Vec::new();
    for death in 1..=MAX_RESTARTS + 1 {
        keeper.link().kill();
        keeper.link().now += 1;
        let now = keeper.link().now;
        keeper.service(now, false);
        keeper.completed(&mut |id, how| done.push((id, how)));
        assert_eq!(keeper.restarts(), death);
    }
    assert!(keeper.gave_up());
    assert_eq!(keeper.link().gave_up, Some(GiveUp::Budget));
    assert_eq!(keeper.link().started.len(), MAX_RESTARTS as usize + 1);
    assert_eq!(done, [(3, Completion::Hung)]);
}

#[test]
fn a_worker_with_other_facts_gives_up() {
    on_big_stack(a_worker_with_other_facts_gives_up_body);
}

fn a_worker_with_other_facts_gives_up_body() {
    let mut keeper = keeper(Link::new());
    keeper.link().revision = 1;
    keeper.link().kill();
    keeper.service(1, false);
    assert_eq!(keeper.link().gave_up, Some(GiveUp::Facts));
}

#[test]
fn an_engine_left_running_and_not_reset_gives_up() {
    on_big_stack(an_engine_left_running_and_not_reset_gives_up_body);
}

fn an_engine_left_running_and_not_reset_gives_up_body() {
    // The consultant's condition 2 (ledger 3156): nothing of the dead
    // worker's is handed back, and the keeper exits, so the kernel keeps
    // every pin quarantined.
    let mut keeper = keeper(Link::new());
    keeper.link().reset_done = false;
    keeper.link().kill();
    keeper.service(1, false);
    assert_eq!(keeper.link().gave_up, Some(GiveUp::NotReset));
    assert!(!keeper.live());
    let mut link = Link::new();
    link.reset_done = false;
    let refused = RemoteBackend::start(link, journal())
        .map(|_| ())
        .map_err(|(_, why)| why);
    assert_eq!(refused, Err(GiveUp::NotReset));
}

#[test]
fn a_ping_unanswered_for_two_seconds_replaces_the_worker() {
    on_big_stack(a_ping_unanswered_for_two_seconds_replaces_the_worker_body);
}

fn a_ping_unanswered_for_two_seconds_replaces_the_worker_body() {
    let mut keeper = keeper(Link::new());
    keeper.link().deaf = true;
    let start = keeper.link().now;
    keeper.service(start + PING_NS, false);
    assert_eq!(keeper.next_deadline(), Some(start + PING_NS + TDR_NS));
    keeper.service(start + PING_NS + TDR_NS - 1, false);
    assert_eq!(keeper.restarts(), 0);
    keeper.link().deaf = false;
    keeper.service(start + PING_NS + TDR_NS, false);
    assert_eq!(keeper.restarts(), 1);
    let said = keeper.link().lines.join("\n");
    assert!(said.contains("(Unanswered)"), "{said}");
    // An answered ping keeps it.
    let mut fine = self::keeper(Link::new());
    fine.service(PING_NS, false);
    fine.service(PING_NS + 1, false);
    fine.service(PING_NS + TDR_NS + 1, false);
    assert_eq!(fine.restarts(), 0);
}

#[test]
fn a_refused_call_replaces_the_worker_and_the_replay_holds_it() {
    on_big_stack(a_refused_call_replaces_the_worker_and_the_replay_holds_it_body);
}

fn a_refused_call_replaces_the_worker_and_the_replay_holds_it_body() {
    let mut keeper = keeper(Link::new());
    let vm = keeper.vm_create().expect("vm");
    let memory = keeper.create_memory(4096).expect("memory");
    // The keeper checks what it can itself.
    assert_eq!(
        keeper.bind(vm, 0x1000, &memory, 4096, 4),
        Err(Errno::EINVAL)
    );
    let video = Engine {
        class: 2,
        instance: 0,
    };
    assert!(
        keeper
            .context_create(vm, video, ContextParams::DEFAULT)
            .is_err()
    );
    // A refusal the keeper could not foresee: no page for a table.
    keeper.flush();
    let live = keeper.link().model().gpu.tables.pages.len();
    keeper.link().model().gpu.tables.limit = Some(live);
    keeper
        .bind(vm, 0x40_0000_0000, &memory, 4096, 0)
        .expect("journalled");
    keeper.service(1, false);
    assert_eq!(keeper.restarts(), 1);
    let said = keeper.link().lines.join("\n");
    assert!(
        said.contains("refused call 4") && said.contains("(Refused)"),
        "{said}"
    );
    // The next worker has room, takes the bind from the journal, and is up.
    keeper.service(2, false);
    assert!(keeper.live());
    let said = keeper.link().lines.join("\n");
    assert!(said.contains("replayed 1 VMs, 1 bindings"), "{said}");
}

#[test]
fn a_worker_without_room_queues_and_keeps_each_contexts_order() {
    on_big_stack(a_worker_without_room_queues_and_keeps_each_contexts_order_body);
}

fn a_worker_without_room_queues_and_keeps_each_contexts_order_body() {
    let mut keeper = keeper(Link::new());
    let vm = keeper.vm_create().expect("vm");
    let _ = store(&mut keeper, vm, 0x1_0000_0000, 0x1_0008_0040, 1);
    let c = keeper
        .context_create(
            vm,
            RENDER,
            ContextParams {
                ring_bytes: 4 << 10,
            },
        )
        .expect("context");
    for id in 1..=40 {
        keeper
            .submit(c, 0x1_0000_0000, 24, id)
            .expect("the keeper takes it");
    }
    let mut done = Vec::new();
    for _ in 0..8 {
        settle(&mut keeper, &mut done);
    }
    let ids: Vec<u64> = done.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, (1..=40).collect::<Vec<u64>>());
}

#[test]
fn the_timestamp_never_waits_and_never_goes_back() {
    on_big_stack(the_timestamp_never_waits_and_never_goes_back_body);
}

fn the_timestamp_never_waits_and_never_goes_back_body() {
    let mut keeper = keeper(Link::new());
    let hz = u64::from(keeper.facts().cs_timestamp_hz);
    let first = keeper.timestamp();
    keeper.link().now += 1_000_000_000;
    let second = keeper.timestamp();
    assert_eq!(second - first, hz);
    // A new worker's clock may restart lower: answers still climb.
    keeper.link().kill();
    let now = keeper.link().now;
    keeper.service(now, false);
    assert!(keeper.timestamp() >= second);
}

#[test]
fn the_bindings_table_finds_every_entry_through_removals() {
    on_big_stack(the_bindings_table_finds_every_entry_through_removals_body);
}

fn the_bindings_table_finds_every_entry_through_removals_body() {
    // Many bindings, removed in an order that splits probe runs; a lookup
    // after each removal must still find every live one.
    let mut keeper = keeper(Link::new());
    let vm = keeper.vm_create().expect("vm");
    let memory = keeper.create_memory(4096).expect("memory");
    let mut live: BTreeMap<u64, ()> = BTreeMap::new();
    for n in 0..3000_u64 {
        let address = n * 0x1000 * 37;
        keeper.bind(vm, address, &memory, 4096, 0).expect("bound");
        let _ = live.insert(address, ());
    }
    for n in (0..3000_u64).filter(|n| n % 3 != 1) {
        let address = n * 0x1000 * 37;
        keeper.unbind(vm, address, 4096);
        let _ = live.remove(&address);
    }
    for (&address, ()) in &live {
        assert_eq!(
            keeper.bind(vm, address, &memory, 4096, 0),
            Err(Errno::EINVAL),
            "{address:#x} is still bound"
        );
    }
    keeper.flush();
    // And a new worker gets exactly the live ones.
    keeper.link().kill();
    keeper.service(1, false);
    let said = keeper.link().lines.join("\n");
    assert!(said.contains(&format!("{} bindings", live.len())), "{said}");
}

#[test]
fn facts_name_engines_by_a_static_prefix() {
    on_big_stack(facts_name_engines_by_a_static_prefix_body);
}

fn facts_name_engines_by_a_static_prefix_body() {
    let facts: Facts = *model_backend(0).facts();
    assert!(core::ptr::eq(
        facts.engines.as_ptr(),
        facts.engines.as_ptr()
    ));
    let ready = Event::Ready {
        facts,
        reset_done: true,
        timestamp: 0,
        now_ns: 0,
    };
    let Ok(Event::Ready { facts: read, .. }) = Event::decode(&ready.encode()) else {
        panic!("a READY");
    };
    assert_eq!(read, facts);
    assert!(core::ptr::eq(read.engines.as_ptr(), UAPI_ENGINES.as_ptr()));
}

#[test]
fn a_destroyed_vm_takes_its_bindings_out_of_the_journal() {
    on_big_stack(a_destroyed_vm_takes_its_bindings_out_of_the_journal_body);
}

fn a_destroyed_vm_takes_its_bindings_out_of_the_journal_body() {
    // A program's file closed: its VM goes with every binding in it, and
    // the next program's VM, which may get the same id, binds the same
    // addresses (Mesa's are fixed).
    let mut keeper = keeper(Link::new());
    let other = keeper.vm_create().expect("vm");
    let memory = keeper.create_memory(4096).expect("memory");
    keeper.bind(other, 0x9000, &memory, 4096, 0).expect("bound");
    let vm = keeper.vm_create().expect("vm");
    for n in 0..64_u64 {
        keeper
            .bind(vm, 0x1_0000_0000 + n * 0x1000, &memory, 4096, 0)
            .expect("bound");
    }
    keeper.vm_destroy(vm);
    let again = keeper.vm_create().expect("vm");
    assert_eq!(again, vm, "the slot is used again");
    for n in 0..64_u64 {
        keeper
            .bind(again, 0x1_0000_0000 + n * 0x1000, &memory, 4096, 0)
            .expect("bound again");
    }
    assert_eq!(
        keeper.bind(other, 0x9000, &memory, 4096, 0),
        Err(Errno::EINVAL)
    );
    keeper.flush();
    keeper.link().kill();
    keeper.service(1, false);
    let said = keeper.link().lines.join("\n");
    assert!(said.contains("replayed 2 VMs, 65 bindings"), "{said}");
}
