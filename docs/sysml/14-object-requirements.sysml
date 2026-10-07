/*
 * Ferrix — SysML v2 model, part 14: the low-level requirements of object/.
 *
 * What each unit of src/kernel/src/object/ does, as `ItemLowLevel` requirements
 * (part 13 defines the format): the core ring's object layer -- what a handle
 * names and how it is dropped, channels, ports, interrupts, I/O mappings,
 * pins, the job quotas, jobs, the core half of a process, and the scoped OOM
 * kill. The handle table itself and the rights arithmetic are
 * `src/lib/kernel/objects` and `src/lib/proto/native-abi`, whose host tests verify
 * H.OBJ.1 to H.OBJ.4 directly; nothing here restates them.
 *
 * This is W-8's pilot (docs/certification/IMPLEMENTATION.md, step 3), the
 * subsystem the format was corrected on before the others copy it. What it
 * settled:
 *
 *  - one requirement per behaviour a check can pass or fail, and a
 *    requirement names every function that carries the behaviour as its
 *    `unit`, not one each: a write that is all or nothing is one sentence
 *    whatever number of functions make it so;
 *  - a requirement is no broader than one check can prove: a `Verifies:`
 *    tag credits the whole requirement, so every check that names one proves
 *    its whole criterion alone. Where the checks that exist each prove a
 *    part, the requirement is split, one part per check, and a part no check
 *    proves stays in the baseline as check-writing work -- never a tag
 *    crediting what its check does not test. The statement says no more than
 *    the criterion tests;
 *  - an accessor -- one statement or one expression, no branch and no
 *    `unsafe` -- needs no requirement of its own (check-traceability.py's
 *    rule); one is still named where it is the behaviour's entry point;
 *  - check code that lives in a product file is listed in
 *    tools/common/data/traceability-units.json, not given a requirement, and the
 *    requirement it would verify waits in the baseline until it moves into a
 *    check file where a `Verifies:` tag may go on it.
 *
 * Ids are `L.object.<n>`, numbered through the subsystem; the package gives
 * the file.
 */
package FerrixObjectRequirements {
    private import FerrixItemRequirements::*;

    package ObjectKinds {
        doc /* src/kernel/src/object/mod.rs: what a handle names, its signals, and
             * how objects are dropped without recursing. */

        requirement <'L.object.1'> aProcessHandleSaysTerminated : ItemLowLevel {
            attribute :>> statement = "Object::signals shall report a handle to a
                process TERMINATED once the process has ended and closed its
                handles.";
            attribute :>> criterion = "A handle to a process that has ended and
                closed its handles reports TERMINATED when waited on.";
            attribute :>> parent = "H.OBJ.11";
            attribute :>> unit = "object::Object::signals";
        }
        requirement <'L.object.2'> theOtherKindsSignals : ItemLowLevel {
            attribute :>> statement = "Object::signals shall report a port READABLE
                while it holds a packet, and no signal for a VMO, a device, an I/O
                mapping, a pin or the starter token.";
            attribute :>> criterion = "A wait for READABLE on a port holding a
                packet returns at once and on an empty port times out; a wait on
                each of a VMO, a device, an I/O mapping and a pin times out with 0
                signals observed.";
            attribute :>> parent = "H.OBJ.11";
            attribute :>> unit = "object::Object::signals";
        }
        requirement <'L.object.3'> waitersAreWokenAtTheChange : ItemLowLevel {
            attribute :>> statement = "Object::waiters shall name a channel end's
                own queue, which a message written to the end wakes, so that a wait
                on the end ends at the message and not at its deadline.";
            attribute :>> criterion = "A wait on a channel end with a two-minute
                deadline returns after a message written 20 ms later and before the
                deadline.";
            attribute :>> parent = "H.OBJ.11";
            attribute :>> unit = "object::Object::waiters";
        }
        requirement <'L.object.4'> disposeFreesWhatIsNested : ItemLowLevel {
            attribute :>> statement = "dispose shall drop every object it is given
                and every object those hold, a channel end's unread messages and the
                handles they carry included, so that nothing an object held outlives
                it.";
            attribute :>> criterion = "Closing both ends of a channel whose queue
                holds a VMO with a committed page gives that page back: the native
                checks' free-frame count is the same before and after, 0 frames
                kept.";
            attribute :>> parent = ("H.OBJ.16", "H.MEM.9");
            attribute :>> unit = ("object::dispose", "object::Object::drops_at_once");
        }
        requirement <'L.object.5'> disposeNeverRecurses : ItemLowLevel {
            attribute :>> statement = "dispose shall hold at most one queued
                object's drop on the stack at a time, however deeply objects are
                nested in one another's queues.";
            attribute :>> criterion = "A chain of N channel ends, each queued unread
                in the next's inbox, is freed from its outermost end with N in the
                thousands and no guard-page fault, every frame given back.";
            attribute :>> parent = ("H.OBJ.8", "H.FAIL.2");
            attribute :>> unit = ("object::dispose", "object::Object::drops_at_once");
        }
        requirement <'L.object.6'> aChannelEndClosesAtOnce : ItemLowLevel {
            attribute :>> statement = "dispose shall close a channel end whose last
                reference it holds before it returns, even while another context is
                draining disposed objects, so that the peer sees PEER_CLOSED from
                the moment the close returns (FX-1004).";
            attribute :>> criterion = "A close made while another context is marked
                as draining is followed at once by a write and a read on the peer,
                each refused PEER_CLOSED, 2 of 2.";
            attribute :>> parent = "H.OBJ.16";
            attribute :>> unit = "object::dispose";
        }
        requirement <'L.object.7'> nothingGivenUpWhileMemoryServes : ItemLowLevel {
            attribute :>> statement = "defer shall queue an object for the drainer
                and, when the queue cannot grow, drop it where it is while fewer
                than four such drops are under way, so that no object is given up
                while a drop in place is still allowed.";
            attribute :>> criterion = "Across the allocation-failure rounds, every
                nth fallible allocation failing for several n, 0 objects are given
                up (abandoned() reads 0) and every frame comes back.";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("object::defer", "object::abandoned");
        }
        requirement <'L.object.8'> givenUpOnlyPastTheDepth : ItemLowLevel {
            attribute :>> statement = "defer shall give an object up, and count it,
                only when the queue cannot grow and four drops in place are already
                under way, and shall neither recurse further nor stop the kernel:
                the one exception to H.OBJ.8, an object that outlives its last
                handle, bounded by the depth and counted by abandoned().";
            attribute :>> criterion = "With the orphan queue refused its growth and
                four in-place drops nested, a fifth deferred object is counted as
                given up (abandoned() rises by 1) and the kernel runs on.";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = "object::defer";
        }
        requirement <'L.object.105'> droppedInPlaceWithoutQueueRoom : ItemLowLevel {
            attribute :>> statement = "defer shall drop an object where it is, and
                give nothing up, when the orphan queue cannot grow and fewer than
                four drops in place are under way.";
            attribute :>> criterion = "A close of a job handle with the orphan
                queue's one growth refused answers success, the job is gone when it
                returns, and abandoned() stays 0.";
            attribute :>> parent = ("H.MEM.11", "H.OBJ.8");
            attribute :>> unit = "object::defer";
        }
    }

    package Channels {
        doc /* src/kernel/src/object/channel.rs. */

        requirement <'L.object.9'> aPairIsTwoObjects : ItemLowLevel {
            attribute :>> statement = "Endpoint::pair shall make two connected ends
                charged together as two objects to the running task's job, and shall
                make neither when the job's object limit refuses the charge.";
            attribute :>> criterion = "In a job limited to 5 objects and holding 5,
                a pair is refused; holding 3, a pair is made and the job holds 5;
                with everything dropped it holds 0.";
            attribute :>> parent = "H.QUOTA.3";
            attribute :>> unit = "object::channel::Endpoint::pair";
        }
        requirement <'L.object.10'> messagesReachThePeer : ItemLowLevel {
            attribute :>> statement = "A message written on one end of a channel
                shall be read, its bytes and the objects its handles name with the
                rights they carried, only on the other end, in the order written.";
            attribute :>> criterion = "Bytes and a VMO handle written by one process
                are read by the other as the same bytes and a handle through which
                the same VMO's contents are read.";
            attribute :>> parent = "H.OBJ.5";
            attribute :>> unit = ("object::channel::Endpoint::write",
                "object::channel::Endpoint::read", "object::channel::Side::other",
                "object::channel::Channel::half");
        }
        requirement <'L.object.11'> aRefusedTakeMovesNothing : ItemLowLevel {
            attribute :>> statement = "Endpoint::write shall answer a refusal of its
                take -- a handle the sender does not hold, or one named twice -- as
                the write's refusal, queueing nothing, so that every handle the
                write named stays with the sender.";
            attribute :>> criterion = "A send naming a good handle and one that does
                not exist is refused BAD_HANDLE and the good handle still works in
                the sender; a send of a channel end through itself is refused
                INVALID_ARGS.";
            attribute :>> parent = "H.OBJ.5";
            attribute :>> unit = "object::channel::Endpoint::write";
        }
        requirement <'L.object.12'> aWriteIsAllOrNothing : ItemLowLevel {
            attribute :>> statement = "Endpoint::write shall take the sender's
                handles out of its table only after the peer's queue has agreed to
                take the message, so that a write refused because the peer closed,
                the message is too big or the queue is full moves no handle.";
            attribute :>> criterion = "A write carrying a handle refused
                PEER_CLOSED, one refused TOO_BIG for its bytes and one refused
                SHOULD_WAIT for a full queue each leave the handle in the sender
                under the same value, usable: 0 handles taken by a refused write.";
            attribute :>> parent = "H.OBJ.5";
            attribute :>> unit = "object::channel::Endpoint::write";
        }
        requirement <'L.object.13'> aQueueIsBounded : ItemLowLevel {
            attribute :>> statement = "A channel end shall hold a bounded number of
                unread messages (channel::LIMITS, 256); a write to a full one shall
                be refused as full, and the writer shall not be told there is room
                until a read makes it.";
            attribute :>> criterion = "Writing to a reader that does not read is
                refused SHOULD_WAIT before 4097 messages are queued; the writer of
                the full channel is told neither that its peer has room nor
                WRITABLE; after draining it is told there is room, and its next
                write arrives. (That the bound is 256 exactly is the host tests' of
                the message queue, with the limit as a parameter.)";
            attribute :>> parent = "H.OBJ.10";
            attribute :>> unit = ("object::channel::Endpoint::write",
                "object::channel::Endpoint::peer_has_room",
                "object::channel::Endpoint::signals");
        }
        requirement <'L.object.14'> aReadThatDoesNotFitTakesNothing : ItemLowLevel {
            attribute :>> statement = "Endpoint::read shall leave a message queued,
                and report its size in bytes and handles, when the caller has room
                for fewer of either.";
            attribute :>> criterion = "A read with too little room for the bytes,
                then for the handle, is refused with the message's sizes both times,
                and a read with room then returns the message whole.";
            attribute :>> parent = "H.OBJ.7";
            attribute :>> unit = "object::channel::Endpoint::read";
        }
        requirement <'L.object.15'> anUndeliveredMessageGoesBack : ItemLowLevel {
            attribute :>> statement = "Endpoint::unread shall put a message the
                caller took and could not deliver back at the head of the queue and
                wake the end's waiters, so that the next read returns it.";
            attribute :>> criterion = "A message carrying an endpoint, read into a
                buffer that faults, is read whole by the next read with a good
                buffer, and the endpoint delivered is the one sent.";
            attribute :>> parent = "H.OBJ.7";
            attribute :>> unit = "object::channel::Endpoint::unread";
        }
        requirement <'L.object.16'> endpointsOnlyUnderTheTopologyLock : ItemLowLevel {
            attribute :>> statement = "Endpoint::read shall refuse, leaving it
                queued, a message that carries a channel end unless the caller holds
                the topology lock, so that no end is put back into a queue while a
                send's cycle walk runs.";
            attribute :>> criterion = "A message carrying an endpoint, read into
                buffers a fork still shares copy-on-write, is delivered whole after
                the delivery put it back and read again, and the fork keeps what it
                had.";
            attribute :>> parent = "H.OBJ.5";
            attribute :>> unit = ("object::channel::Endpoint::read",
                "object::channel::carries_endpoints");
        }
        requirement <'L.object.17'> anEmptyEndWithAClosedPeer : ItemLowLevel {
            attribute :>> statement = "Once its peer has closed, an end with nothing
                queued shall answer a read PEER_CLOSED, and a write to the peer
                shall be refused PEER_CLOSED.";
            attribute :>> criterion = "After the peer closes, while another context
                is marked as draining, a read and a write on the survivor are
                refused PEER_CLOSED, 2 of 2.";
            attribute :>> parent = "H.OBJ.16";
            attribute :>> unit = ("object::channel::Endpoint::read",
                "object::channel::Endpoint::peer_closed");
        }
        requirement <'L.object.18'> anEmptyEndWithAnOpenPeerSaysWait : ItemLowLevel {
            attribute :>> statement = "With nothing queued and its peer open,
                Endpoint::read shall report the queue empty, which the call answers
                SHOULD_WAIT.";
            attribute :>> criterion = "A read of an emptied channel end whose peer
                is open is refused SHOULD_WAIT.";
            attribute :>> parent = "H.OBJ.11";
            attribute :>> unit = "object::channel::Endpoint::read";
        }
        requirement <'L.object.19'> messagesBeforeTheCloseAreReadFirst : ItemLowLevel {
            attribute :>> statement = "Endpoint::read shall return every message the
                peer wrote before it closed before it answers PEER_CLOSED.";
            attribute :>> criterion = "A peer that writes 3 messages and closes
                leaves 3 to be read, in order, and only the fourth read answers
                PEER_CLOSED. A write and close between a read's look at the queue
                and its answer is argued in F-54 (the peer's close is looked at, with
                Acquire, before the queue), not shown by this check.";
            attribute :>> parent = "H.OBJ.16";
            attribute :>> unit = "object::channel::Endpoint::read";
        }
        requirement <'L.object.20'> closingAnEndFreesItsQueue : ItemLowLevel {
            attribute :>> statement = "Closing a channel end shall mark it closed
                under its queue's lock and free every message queued for it unread,
                with the objects they carry.";
            attribute :>> criterion = "A VMO queued unread in the closed end is
                freed: the native checks' free- frame count, taken around the check,
                is unchanged.";
            attribute :>> parent = ("H.OBJ.16", "H.MEM.9");
            attribute :>> unit = ("object::channel::Endpoint::drop",
                "object::channel::Endpoint::take_unread");
        }
        requirement <'L.object.21'> closingAnEndTellsItsPeer : ItemLowLevel {
            attribute :>> statement = "Closing a channel end shall fire the peer's
                registrations waiting for PEER_CLOSED.";
            attribute :>> criterion = "A registration for PEER_CLOSED on one end
                fires when the other end is closed, its packet carrying its key and
                PEER_CLOSED.";
            attribute :>> parent = "H.OBJ.17";
            attribute :>> unit = "object::channel::Endpoint::drop";
        }
        requirement <'L.object.22'> anEndsSignals : ItemLowLevel {
            attribute :>> statement = "Endpoint::signals shall report READABLE while
                a message is queued for the end and PEER_CLOSED once its peer has
                closed.";
            attribute :>> criterion = "A wait woken by a message reports the end
                READABLE, and a wait for PEER_CLOSED on an end whose peer has closed
                returns at once.";
            attribute :>> parent = "H.OBJ.11";
            attribute :>> unit = ("object::channel::Endpoint::signals",
                "object::Object::signals");
        }
        requirement <'L.object.23'> anEndsRegistrations : ItemLowLevel {
            attribute :>> statement = "Endpoint::observe shall fire a registration
                for READABLE or PEER_CLOSED at once when either already holds, and
                otherwise list it under the queue lock that the change it waits for
                is made under, so it is neither missed nor fired early.";
            attribute :>> criterion = "A registration on a readable end fires at
                once with its key; one on a quiet end fires on the next message, and
                one for PEER_CLOSED when the peer closes.";
            attribute :>> parent = "H.OBJ.17";
            attribute :>> unit = "object::channel::Endpoint::observe";
        }
        requirement <'L.object.24'> noCycleOfEnds : ItemLowLevel {
            attribute :>> statement = "check_carry shall find that a send carrying
                channel ends closes a cycle when the inbox it lands in is reachable
                from what it carries, following the ends queued in each inbox.";
            attribute :>> criterion = "Queueing one channel's end in another and
                then the second's in the first is refused, the refused send keeps
                its handle, and 0 frames of a VMO riding in the queue are kept.";
            attribute :>> parent = ("H.OBJ.8", "H.MEM.9");
            attribute :>> unit = ("object::channel::check_carry",
                "object::channel::queued_endpoints", "object::channel::identity",
                "object::channel::Endpoint::identity");
        }
        requirement <'L.object.25'> anEndQueuedTwiceIsNoCycle : ItemLowLevel {
            attribute :>> statement = "A send carrying an end whose inbox holds
                another end twice, by two handles to it, shall not be refused as a
                cycle or as a walk too long.";
            attribute :>> criterion = "With one end carried twice in an inbox, a
                send carrying that inbox's own end is taken.";
            attribute :>> parent = "H.OBJ.8";
            attribute :>> unit = "object::channel::queued_endpoints";
        }
        requirement <'L.object.26'> theWalkIsBounded : ItemLowLevel {
            attribute :>> statement = "A cycle walk shall visit at most 1024 queued
                ends, and a send whose walk would visit more shall be refused as too
                big, keeping what it would have carried with the sender.";
            attribute :>> criterion = "A send whose walk meets more queued ends than
                the bound is refused TOO_BIG and every handle it named is still the
                sender's.";
            attribute :>> parent = ("H.OBJ.10", "H.OBJ.5");
            attribute :>> unit = ("object::channel::check_carry",
                "object::channel::queued_endpoints");
        }
    }

    package Ports {
        doc /* src/kernel/src/object/port.rs. */

        requirement <'L.object.27'> aPortIsOneObject : ItemLowLevel {
            attribute :>> statement = "Port::new shall make an empty port charged as
                one object to the running task's job, and none when the job's object
                limit refuses the charge.";
            attribute :>> criterion = "In a job limited to 5 objects, exactly 5
                ports are made and the sixth refused; with them gone the job holds
                0.";
            attribute :>> parent = "H.QUOTA.3";
            attribute :>> unit = "object::port::Port::new";
        }
        requirement <'L.object.28'> userPacketsAreBounded : ItemLowLevel {
            attribute :>> statement = "Port::queue_user shall refuse a program's
                packet as full when 1024 of its packets are queued, whatever room
                signal packets have been promised.";
            attribute :>> criterion = "Filling a port nobody takes from is refused
                SHOULD_WAIT at exactly 1024 packets, and not before.";
            attribute :>> parent = "H.OBJ.10";
            attribute :>> unit = ("object::port::Port::queue_user",
                "object::port::Queue::push_fitting");
        }
        requirement <'L.object.29'> aPacketComesBackAsQueued : ItemLowLevel {
            attribute :>> statement = "Port::take shall return a user packet exactly
                as it was queued, and give back its share of the bound it was
                counted in.";
            attribute :>> criterion = "A queued user packet is taken with its key,
                its kind and both data words as queued.";
            attribute :>> parent = "H.OBJ.11";
            attribute :>> unit = ("object::port::Port::take",
                "object::port::Port::queue_user");
        }
        requirement <'L.object.30'> roomIsPromisedWhenRegistering : ItemLowLevel {
            attribute :>> statement = "A registration and an interrupt binding shall
                reserve room for their packet on the port when they are made, and be
                refused for memory then if it cannot be reserved, so that queueing a
                signal or interrupt packet never allocates and never finds the port
                without room.";
            attribute :>> criterion = "Across rounds of native calls with every nth
                allocation failing, every port reports 0 packets lost for want of
                room.";
            attribute :>> parent = ("H.MEM.11", "H.OBJ.17");
            attribute :>> unit = ("object::port::Port::promise",
                "object::port::Port::release_promise", "object::port::Promise::new",
                "object::port::Promise::drop", "object::port::Observer::new",
                "object::port::Port::queue_signal", "object::port::Queue::push_fitting");
        }
        requirement <'L.object.31'> anUndeliveredPacketGoesBack : ItemLowLevel {
            attribute :>> statement = "Port::put_back shall queue a packet the
                caller took and could not deliver ahead of every other, with its
                share of the bound, so that the next take returns it.";
            attribute :>> criterion = "A packet taken for a buffer that faults -- a
                user packet, a signal packet fired on a change and one fired as it
                was registered -- is returned by the next wait with a good buffer.";
            attribute :>> parent = "H.OBJ.7";
            attribute :>> unit = "object::port::Port::put_back";
        }
        requirement <'L.object.32'> aRegistrationFiresOnce : ItemLowLevel {
            attribute :>> statement = "A registration shall not fire before a signal
                it asked for is asserted, and shall fire once when one is, reporting
                it with its key, and not again.";
            attribute :>> criterion = "A registration does not fire before its
                signal, fires once on the message that asserts it with its key and
                READABLE, and a second write fires nothing.";
            attribute :>> parent = "H.OBJ.17";
            attribute :>> unit = ("object::port::trigger", "object::port::deliver",
                "object::port::Observer::fire", "object::port::register");
        }
        requirement <'L.object.33'> aChangeFiresOnlyWhatWantsIt : ItemLowLevel {
            attribute :>> statement = "trigger shall fire only the registrations
                whose signals the change asserts, leaving every other listed.";
            attribute :>> criterion = "A message fires the registration waiting for
                READABLE and not the one waiting for PEER_CLOSED listed ahead of
                it.";
            attribute :>> parent = "H.OBJ.17";
            attribute :>> unit = "object::port::trigger";
        }
        requirement <'L.object.34'> sixtyFourRegistrations : ItemLowLevel {
            attribute :>> statement = "register shall refuse a registration as full
                when the object already holds 64, counted after dropping those whose
                port has gone.";
            attribute :>> criterion = "The 65th registration on one object is
                refused SHOULD_WAIT; after its ports are closed the object takes 64
                again.";
            attribute :>> parent = "H.OBJ.10";
            attribute :>> unit = "object::port::register";
        }
    }

    package Interrupts {
        doc /* src/kernel/src/object/interrupt.rs. */

        requirement <'L.object.35'> aLineIsClaimedOnce : ItemLowLevel {
            attribute :>> statement = "Interrupt::new shall claim a line for at most
                one Interrupt at a time, and a line whose claim failed for memory
                shall be left free for the next claim.";
            attribute :>> criterion = "A second claim of a held line is refused
                ALREADY_BOUND; interrupt_create run once per allocation it makes,
                that one failing, claims the line or answers NO_MEMORY each time,
                and the next run claims it.";
            attribute :>> parent = ("H.IRQ.1", "H.IRQ.2");
            attribute :>> unit = ("object::interrupt::Interrupt::new",
                "object::interrupt::register_handler");
        }
        requirement <'L.object.36'> theLineIsFreedWithItsHolder : ItemLowLevel {
            attribute :>> statement = "Dropping the Interrupt that claimed a line
                shall free the line before the close returns, even while another
                context drains disposed objects, and a delivery in flight for the
                old holder shall not reach a new one.";
            attribute :>> criterion = "With a delivery taken and not yet fired, the
                holder's close lets the line be claimed again at once and the
                delivery then leaves the new holder quiet; a close made while
                another context drains lets it be claimed a third time.";
            attribute :>> parent = "H.IRQ.2";
            attribute :>> unit = ("object::interrupt::Interrupt::drop",
                "object::interrupt::Delivery::fire");
        }
        requirement <'L.object.37'> pendingUntilAcknowledged : ItemLowLevel {
            attribute :>> statement = "A delivery shall mark the line pending, and
                Object::signals report the interrupt READABLE, until
                Interrupt::acknowledge clears it.";
            attribute :>> criterion = "Before a delivery a wait on the interrupt
                times out; after one it sees READABLE; after interrupt_ack it times
                out again.";
            attribute :>> parent = "H.IRQ.2";
            attribute :>> unit = ("object::interrupt::take_delivery",
                "object::interrupt::on_interrupt", "object::interrupt::Line::fire",
                "object::interrupt::Interrupt::acknowledge",
                "object::interrupt::Delivery::fire",
                "object::Object::signals");
        }
        requirement <'L.object.38'> aDeliveryWakes : ItemLowLevel {
            attribute :>> statement = "Line::fire shall wake the tasks waiting on
                the interrupt and on the port it is bound to, after letting go of
                the interrupt-safe locks it took.";
            attribute :>> criterion = "In at least 5 of 8 rounds each way, a wait on
                the interrupt and a wait on its port are ended by the delivery's
                wake, not by the recheck timer.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = ("object::interrupt::Line::fire",
                "object::Object::waiters");
        }
        requirement <'L.object.39'> aBoundInterruptQueuesOnePacket : ItemLowLevel {
            attribute :>> statement = "A bound interrupt shall queue one packet on
                its port, carrying its key, the interrupt kind and when it fired,
                each time it goes from quiet to pending, and Interrupt::bind shall
                refuse a second binding while the first port is held.";
            attribute :>> criterion = "A second bind is refused; two deliveries
                before the acknowledgement queue 1 packet with its key, kind and
                time, and a delivery after it queues another.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = ("object::interrupt::Interrupt::bind",
                "object::interrupt::Line::fire", "object::port::Port::queue_interrupt",
                "object::port::Port::queue_from_interrupt");
        }
        requirement <'L.object.40'> aPendingInterruptIsQueuedWhenBound : ItemLowLevel {
            attribute :>> statement = "Interrupt::bind shall queue a packet at once
                for an interrupt that is already pending.";
            attribute :>> criterion = "An interrupt that fired while its port was
                gone queues its packet, with its new key and the interrupt kind, on
                a new port as it is bound.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = ("object::interrupt::Interrupt::bind",
                "object::port::Port::queue_interrupt");
        }
        requirement <'L.object.41'> maskedFromDeliveryToAcknowledgement : ItemLowLevel {
            doc /* Rationale: an edge MSI-X vector's extra deliveries fold into the
                packet its holder has not answered, so masking it per delivery
                only cost two exits to the hypervisor per interrupt. The bound
                keeps interrupt time bounded: at most STORM_BOUND (64) handler
                runs per scheduling of the holder, where it was 1, each charged
                to the task the interrupt cut, so a storm costs other partitions
                up to that bound, accounted in their share
                (docs/certification/MEMORY-AND-TIMING.md §2.2; AoU-4, ASR-8). */
            attribute :>> statement = "A delivery shall mask a line the controller
                holds before it marks it pending, and acknowledge shall unmask it; a
                delivery shall leave an edge-triggered MSI-X vector unmasked unless
                more than STORM_BOUND (64) deliveries have reached it since the last
                acknowledgement, when it shall mask the vector's entry until the
                next acknowledgement; acknowledge shall clear pending before it
                unmasks, so a delivery after it queues a packet of its own; and a
                refused claim shall leave the holder's line unmasked.";
            attribute :>> criterion = "On an edge-triggered MSI-X vector: read at its
                entry the claimed line is unmasked and stays unmasked after a
                delivery; a delivery after the acknowledgement and before the drain
                queues a packet; after 64 unacknowledged deliveries the entry is
                still unmasked, after the 65th it is masked with the line's storm
                count one higher and one packet queued, and after the
                acknowledgement it is unmasked (the `irq` line).";
            attribute :>> parent = "H.IRQ.2";
            attribute :>> unit = ("object::interrupt::take_delivery",
                "object::interrupt::Line::hold_back",
                "object::interrupt::Interrupt::acknowledge",
                "object::interrupt::Interrupt::new",
                "device::Vector::coalesces", "device::Vector::reads_masked");
        }
        requirement <'L.object.42'> anUnheldLineIsMasked : ItemLowLevel {
            attribute :>> statement = "take_delivery shall mask at the controller a
                line no Interrupt holds and deliver nothing for it.";
            attribute :>> criterion = "A delivery on a line whose holder has let go
                wakes nobody, queues no packet, and leaves the line masked.";
            attribute :>> parent = "H.IRQ.3";
            attribute :>> unit = "object::interrupt::take_delivery";
        }
    }

    package IoMappings {
        doc /* src/kernel/src/object/io_mapping.rs. */

        requirement <'L.object.43'> wholePagesOrRefused : ItemLowLevel {
            attribute :>> statement = "IoMapping::new shall refuse an aperture that
                does not start on a page boundary or is not a whole number of
                pages.";
            attribute :>> criterion = "Every sub-page aperture the machine has (the
                virtio-mmio transports on ARMv7-A) is refused; 0 are mapped.";
            attribute :>> parent = "H.DMA.5";
            attribute :>> unit = ("object::io_mapping::IoMapping::new",
                "device::Aperture::whole_pages");
        }
        requirement <'L.object.44'> exactlyItsAperture : ItemLowLevel {
            attribute :>> statement = "IoMapping::map_into shall map exactly the
                aperture's pages, readable and writable, onto the device's own
                physical addresses.";
            attribute :>> criterion = "Each mapped page translates to the aperture's
                physical page at the same offset, and a forked child reaches the
                same device pages rather than copies.";
            attribute :>> parent = "H.DMA.5";
            attribute :>> unit = "object::io_mapping::IoMapping::map_into";
        }
    }

    package Pins {
        doc /* src/kernel/src/object/pin.rs. The budget's and the quarantine's
             * checks, P1 to P5 of docs/NVIDIA.md section 12.2, are in
             * object/pin/check.rs, a child module that may read what pin.rs
             * keeps to itself. Each device has a pin budget B, and live,
             * quarantined and kept counts against it, kept on its domain under
             * the quarantine's lock (NVIDIA N0f). The domain side of a pin is
             * iommu's (part 16). */

        requirement <'L.object.45'> aDeviceGetsExactlyItsPages : ItemLowLevel {
            attribute :>> statement = "A pin shall map exactly the held pages of the
                VMO into the device's domain and report each page's device address,
                in page order.";
            attribute :>> criterion = "A pin of two pages reports 2 addresses, each
                of which the domain translates to the VMO's page holding what the
                VMO holds.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("object::pin::Pin::with_budget", "object::pin::Pin::map",
                "object::pin::Pin::new", "object::pin::Pin::addresses");
        }
        requirement <'L.object.46'> unpinnedBeforeFreed : ItemLowLevel {
            attribute :>> statement = "Dropping a pin of a live owner shall take its
                pages out of the domain, the invalidation complete, before the hold
                on its frames is released; when the domain refuses, the frames
                shall be kept.";
            attribute :>> criterion = "On a translated domain a closed pin's pages
                are no longer reachable through the domain.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("object::pin::Pin::drop", "object::pin::keep");
        }
        requirement <'L.object.47'> aDeadDriversPinIsQuarantined : ItemLowLevel {
            attribute :>> statement = "Dropping a pin on a translated domain after
                its owner terminated shall keep its pages mapped and its frames
                held, charged to nobody, in the quarantine (on an untranslated
                domain too, with nothing to unmap: L.object.127), moving them from its
                device's live count to its quarantined count; a pin closed by a
                live owner shall not be quarantined, and its pages shall leave the
                live count.";
            attribute :>> criterion = "A 2-page pin closed by a dead process leaves
                the device 2 pages quarantined and 0 live; a pin closed by a live
                process leaves 0 live, 0 quarantined and 0 kept.";
            attribute :>> parent = "H.DMA.4";
            attribute :>> unit = ("object::pin::Pin::drop", "object::pin::quarantine",
                "object::pin::fold", "object::pin::unreserve");
        }
        requirement <'L.object.48'> twiceTheBudgetBoundsTheQuarantine : ItemLowLevel {
            attribute :>> statement = "A pin shall be refused QuarantineFull, with
                nothing counted, when its device's quarantined and live pages and
                its own would pass twice the device's budget.";
            attribute :>> criterion = "With a budget of 2 pages and nothing kept: a
                2-page pin is taken with 2 pages quarantined; a 1-page pin is
                refused with 4 quarantined; a 2-page pin is refused with 3
                quarantined.";
            attribute :>> parent = "H.DMA.4";
            attribute :>> unit = ("object::pin::reserve", "object::pin::Pin::with_budget");
        }
        requirement <'L.object.49'> theQuarantineGoesBackOnHello : ItemLowLevel {
            attribute :>> statement = "quarantine_release shall give back every
                quarantined pin of the device's domain, out of the domain first and
                then to the allocator, counting the pages written after their driver
                died, and take off the quarantined count exactly the pages it gave
                back; a pin whose unpin is refused shall move to the kept count,
                which counts against twice the budget for good.";
            attribute :>> criterion = "Of two 1-page quarantined pins, one refused
                its unpin: the release gives back 1 page and leaves 0 quarantined
                and 1 kept; a 2-page pin is then taken at a budget of 2; with 2
                more pages quarantined, a 2-page pin is refused QuarantineFull (1
                kept + 2 + 2 > 4).";
            attribute :>> parent = "H.DMA.4";
            attribute :>> unit = ("object::pin::quarantine_release",
                "object::pin::release", "object::pin::fold",
                "object::pin::account_release");
        }
        requirement <'L.object.118'> theBudgetIsTheDelegators : ItemLowLevel {
            attribute :>> statement = "device_set_limit shall set a device's pin
                budget only through a handle carrying SET_LIMIT, never while the
                device has live pins, and never so that twice every budget raised
                above the default would pass the ceiling, a quarter of RAM; a
                refusal shall change nothing. Every answer on a device handle
                shall be audited with the device, the limit, and the old and new
                values: a set as DEVICE_LIMIT_SET, a refusal as DEVICE_LIMIT with
                its status. device_get_limit shall answer the budget, the ceiling
                and the room.";
            attribute :>> criterion = "Without SET_LIMIT the set is ACCESS_DENIED;
                with a live pin, BAD_STATE; a budget whose double passes the room,
                NO_MEMORY, with the budget and the room read back unchanged; a set
                to the budget it has is taken; the ceiling read back is the
                kernel's, nonzero, and the room no more than it; and the audit
                record holds, for the checking process and the device, the three
                refusals with their statuses and the set, each naming the pin
                budget and its old and new values.";
            attribute :>> parent = ("H.DMA.4", "H.OBJ.2", "H.AUD.8");
            attribute :>> unit = ("object::pin::set_budget", "object::pin::raise_of",
                "object::pin::room", "object::pin::ceiling_room",
                "object::pin::count_ceiling", "object::pin::counts",
                "object::pin::budget_of", "syscall::native::device_set_limit",
                "syscall::native::set_limit_on", "syscall::native::device_and_rights",
                "syscall::native::device_get_limit", "audit::device_limit");
        }
        requirement <'L.object.119'> aPinPastTheBudgetIsRefused : ItemLowLevel {
            attribute :>> statement = "A pin shall be refused LimitReached, with
                nothing pinned and nothing counted, when its device's live pages and
                its own would pass the device's budget.";
            attribute :>> criterion = "With a budget of 2 pages, a 2-page pin is
                taken and counted 2 live, and a 1-page pin after it is refused
                LimitReached with the live count still 2.";
            attribute :>> parent = "H.DMA.4";
            attribute :>> unit = ("object::pin::reserve", "object::pin::Pin::new",
                "object::pin::announce");
        }
        requirement <'L.object.120'> liveIsReservedBeforeThePin : ItemLowLevel {
            attribute :>> statement = "A pin's pages shall be counted live under the
                quarantine's lock, in the step that tests the budget, before the
                domain maps them, and given back if the domain refuses.";
            attribute :>> criterion = "A page pinned twice into one domain is
                refused by the domain, and the live count is as before the refused
                pin.";
            attribute :>> parent = "H.DMA.4";
            attribute :>> unit = ("object::pin::Pin::with_budget", "object::pin::Pin::map",
                "object::pin::reserve", "object::pin::unreserve",
                "object::pin::PinBudget::write");
        }
        requirement <'L.object.126'> anUntranslatedLiveCloseGivesBack : ItemLowLevel {
            attribute :>> statement = "On an untranslated domain, dropping a pin of
                a live owner shall give its frames back at once and take its pages
                off the device's live count, keeping nothing: the device of such a
                domain reaches all of memory anyway (VULNERABILITY-ANALYSIS V-03,
                finding F-59).";
            attribute :>> criterion = "On an untranslated domain, six 2-page pins
                opened and closed in turn by a live process at a budget of 2 pages
                are each taken, leave the counts at 0 live, 0 quarantined and 0
                kept, and leave each frame with no reference but the check's.";
            attribute :>> parent = ("H.DMA.3", "H.DMA.4");
            attribute :>> unit = ("object::pin::Pin::drop", "object::pin::Pin::map",
                "object::pin::unreserve");
        }
        requirement <'L.object.127'> anUntranslatedDeadPinIsQuarantined : ItemLowLevel {
            attribute :>> statement = "On an untranslated domain, a pin dropped after
                its owner terminated shall go to the quarantine with its frames held
                and its pages counted quarantined, not kept, and the next HELLO its
                device's core accepts shall give it back.";
            attribute :>> criterion = "On an untranslated domain a 2-page pin closed
                by a dead process leaves 0 live, 2 quarantined and 0 kept with its
                frames still referenced; DeviceNode::hello_accepted then leaves the
                counts at 0 and each frame with no reference but the check's.";
            attribute :>> parent = "H.DMA.4";
            attribute :>> unit = ("object::pin::quarantine", "object::pin::quarantine_release",
                "object::pin::release", "device::DeviceNode::hello_accepted");
        }
    }

    package QuotaSlots {
        doc /* src/kernel/src/object/quota.rs: the counters behind FRU_RSA.1. */

        requirement <'L.object.50'> aChargeIsHierarchical : ItemLowLevel {
            attribute :>> statement = "quota::charge shall add to the slot and to
                every slot above it only if every level stays within its limit, and
                a charge refused at any level shall leave no level charged and count
                one refusal at the level whose limit refused it.";
            attribute :>> criterion = "For each of the four resources, with a child
                limited to 5 under a parent limited to 3, the fourth unit is
                refused, child, parent and root each read 3, and one refusal is
                counted at the parent and none at the child; with the parent
                unlimited the child takes 5 and refuses the sixth.";
            attribute :>> parent = ("H.QUOTA.5", "H.QUOTA.3");
            attribute :>> unit = ("object::quota::charge",
                "object::quota::uncharge_below", "object::quota::Resource::at",
                "object::quota::slot", "object::quota::record_refused");
        }
        requirement <'L.object.51'> anUnchargeComesBackExactly : ItemLowLevel {
            attribute :>> statement = "quota::uncharge shall take back exactly the
                amount from the slot and every slot above it, never below zero.";
            attribute :>> criterion = "A job charged and uncharged the same amount
                reads 0, and so does every level above it.";
            attribute :>> parent = "H.QUOTA.4";
            attribute :>> unit = ("object::quota::uncharge",
                "object::quota::uncharge_below");
        }
        requirement <'L.object.52'> aLimitReadsBackAndRefuses : ItemLowLevel {
            attribute :>> statement = "Quota::set_limit shall set the limit the next
                charge is judged against, and Quota::usage shall report the use, the
                limit and the refusals.";
            attribute :>> criterion = "pids.max and memory.max written through
                cgroupfs reach the job's quota and read back as written; an empty
                job reads 0 used and 0 refused.";
            attribute :>> parent = "H.QUOTA.5";
            attribute :>> unit = ("object::quota::Quota::set_limit",
                "object::quota::Quota::usage", "object::job::Job::set_limit",
                "object::job::Job::usage");
        }
        requirement <'L.object.53'> aSlotLivesWhileHeld : ItemLowLevel {
            attribute :>> statement = "A job's slot shall be held by its job, its
                child slots, each charge and each task that names it, and freed --
                letting go of its parent -- when the last hold goes.";
            attribute :>> criterion = "After the quota checks' jobs are gone the
                number of live slots is what it was before them: every slot given
                back.";
            attribute :>> parent = "H.QUOTA.4";
            attribute :>> unit = ("object::quota::claim", "object::quota::find_free",
                "object::quota::grow", "object::quota::hold", "object::quota::release",
                "object::quota::Quota::new", "object::quota::Quota::drop",
                "object::quota::hold_group", "object::quota::release_group");
        }
        requirement <'L.object.54'> aSlotIsClaimedClean : ItemLowLevel {
            attribute :>> statement = "claim shall hand out a slot, reused or new,
                with every use and refusal count at zero, every limit unlimited and
                the default weight.";
            attribute :>> criterion = "A job made on a slot another job freed after
                charging 5 of each resource under limits of 3 reads 0 used, 0
                refused and no limit for each, and weight 100.";
            attribute :>> parent = "H.QUOTA.4";
            attribute :>> unit = ("object::quota::claim", "object::quota::find_free");
        }
        requirement <'L.object.55'> aFrameIsChargedAPage : ItemLowLevel {
            attribute :>> statement = "charge_frame shall charge one page of memory
                to the slot and hold the slot, refusing when a limit would be
                passed, and uncharge_frame shall give back both.";
            attribute :>> criterion = "Faults in a job limited to 48 pages' worth
                are refused when the next page would not fit, the refusal counted,
                while a sibling faults 48 pages in; with the spaces gone every
                level's memory reads 0.";
            attribute :>> parent = ("H.QUOTA.2", "H.QUOTA.4");
            attribute :>> unit = ("object::quota::charge_frame",
                "object::quota::uncharge_frame");
        }
        requirement <'L.object.56'> kernelHeapIsMemory : ItemLowLevel {
            attribute :>> statement = "charge_kernel shall charge kernel heap
                against the memory limit and count it again as kernel memory, both
                or neither, and uncharge_kernel shall give back both.";
            attribute :>> criterion = "At a 32 KiB limit each kind of personality
                object is refused one past its count while a sibling makes one, and
                every byte charged comes back when they go.";
            attribute :>> parent = "H.QUOTA.7";
            attribute :>> unit = ("object::quota::charge_kernel",
                "object::quota::uncharge_kernel", "object::quota::KernelHeap::charge",
                "object::quota::KernelHeap::uncharge");
        }
        requirement <'L.object.57'> aMoveBringsItsTasks : ItemLowLevel {
            attribute :>> statement = "charge_regardless shall add the amount to the
                slot and every slot above it whatever their limits say.";
            attribute :>> criterion = "A process moved into a job at its task limit
                of 8 is moved: the job reads 9 tasks and the job it left 1 fewer.";
            attribute :>> parent = "H.QUOTA.1";
            attribute :>> unit = "object::quota::charge_regardless";
        }
        requirement <'L.object.58'> chargingNobodyRestores : ItemLowLevel {
            attribute :>> statement = "charging_nobody shall run its work with no
                job charged for what it allocates, and restore the running task's
                job afterwards.";
            attribute :>> criterion = "Work run under charging_nobody in a job at
                its memory limit completes, the job's use is unchanged, and an
                allocation after it is charged to the job again.";
            attribute :>> parent = "H.QUOTA.2";
            attribute :>> unit = "object::quota::charging_nobody";
        }
        requirement <'L.object.59'> anObjectsChargeLivesWithIt : ItemLowLevel {
            attribute :>> statement = "A Charge shall be taken from the job when the
                object it pays for is made, refusing the object when a limit would
                be passed, and given back when the object is dropped.";
            attribute :>> criterion = "Objects in a job limited to 5 are refused at
                exactly 5, a channel's two ends together; with them dropped the job
                holds 0.";
            attribute :>> parent = ("H.QUOTA.3", "H.QUOTA.4");
            attribute :>> unit = ("object::quota::Charge::to",
                "object::quota::Charge::drop", "object::quota::Charge::running");
        }
        requirement <'L.object.60'> weightIsShared : ItemLowLevel {
            attribute :>> statement = "effective shall scale a task's weight, at
                each level from its job up, by the job's weight over the job's load,
                and adjust shall keep each job's load the weight of its runnable
                tasks and busy children.";
            attribute :>> criterion = "One spinning task alone in a job keeps about
                half a processor against eight in another job; with the tasks gone
                every job's load reads 0.";
            attribute :>> parent = "H.SCHED.3";
            attribute :>> unit = ("object::quota::effective", "object::quota::adjust");
        }
        requirement <'L.object.61'> theWeightIsSet : ItemLowLevel {
            attribute :>> statement = "Job::set_cpu_weight shall make the job's
                weight the value written.";
            attribute :>> criterion = "cpu.weight written 300 through cgroupfs reads
                back 300 from the job.";
            attribute :>> parent = "H.SCHED.3";
            attribute :>> unit = ("object::quota::Quota::set_weight",
                "object::job::Job::set_cpu_weight", "object::job::Job::cpu_weight");
        }
        requirement <'L.object.62'> theWeightIsClampedAndPassedUp : ItemLowLevel {
            attribute :>> statement = "Quota::set_weight shall clamp the weight to 1
                to 10000, and change a busy job's contribution to its parent's load
                at once.";
            attribute :>> criterion = "A weight of 0 is stored as 1 and one of 20000
                as 10000; setting a busy job's weight changes its parent's load by
                the difference of the scaled weights.";
            attribute :>> parent = "H.SCHED.3";
            attribute :>> unit = "object::quota::Quota::set_weight";
        }
        requirement <'L.object.160'> theCarriedWeightIsTheWideFormula : ItemLowLevel {
            attribute :>> statement = "quota::effective shall carry a task's weight up
                its jobs in 64-bit arithmetic (ferrix_sched::carried_weight), giving,
                before it clamps the answer, the value the same formula gives in
                128-bit arithmetic, bit for bit, for every base and entity weight
                below 2^32 and every load.";
            attribute :>> criterion = "ferrix-sched's host test compares the two over
                50000 random and boundary bases, weights and loads at each job depth
                from 1 to 8, and at every weight 2^32 - 1, and finds no difference;
                with one division made in 32 bits, it finds one.";
            attribute :>> parent = "H.SCHED.3";
            attribute :>> unit = "object::quota::effective";
        }
        requirement <'L.object.161'> everyEntityWeightIsBelowTwoToThe32 : ItemLowLevel {
            attribute :>> statement = "Every entity weight a job can be given, its
                cpu.weight clamped to 1 to 10000 and scaled to task units, shall be
                below 2^32, as every task's weight, a u32, is.";
            attribute :>> criterion = "The kernel's build asserts that 10000 scaled
                to task units fits a u32; the quota check sets cpu.weight to 20000,
                reads 10000 back, and finds it scaled below 2^32.";
            attribute :>> parent = "H.SCHED.3";
            attribute :>> unit = ("object::quota::Quota::set_weight",
                "object::quota::Slot::entity_weight");
        }
    }

    package Jobs {
        doc /* src/kernel/src/object/job.rs. */

        requirement <'L.object.63'> everyProcessInOneJob : ItemLowLevel {
            attribute :>> statement = "root shall answer the one job at the top of
                the tree, in which every process starts unless it is forked from a
                process elsewhere or moved.";
            attribute :>> criterion = "A new process is in the root job, and a
                fork's child in its parent's job.";
            attribute :>> parent = "H.OBJ.12";
            attribute :>> unit = ("object::job::root", "object::job::Job::new_tree_root",
                "object::job::Job::bare");
        }
        requirement <'L.object.64'> aChildsQuotaIsInsideItsParents : ItemLowLevel {
            doc /* The job's audit budget (docs/certification/AUDIT.md §3) is
                 * fixed here too, when the job is made: its own id, or its
                 * parent's where its maker says the job was its members' own
                 * to make (job::Budget), and written into its quota slot for
                 * the record of a refused charge. audit::check proves it. */
            attribute :>> statement = "A child job shall be made inside its parent,
                with a quota slot inside the parent's.";
            attribute :>> criterion = "A charge to a grandchild is counted in its
                parent and in the root of the check's tree too, and a limit on the
                parent refuses it.";
            attribute :>> parent = ("H.OBJ.12", "H.QUOTA.5");
            attribute :>> unit = ("object::job::Job::new_child",
                "object::job::Job::bare_child", "object::job::Job::new_root",
                "object::quota::Quota::identify");
        }
        requirement <'L.object.65'> aChildIsAnObjectAndRefusedWhenKilled : ItemLowLevel {
            attribute :>> statement = "A child job shall be charged as one object to
                the running task's job, and refused once its parent has been
                killed.";
            attribute :>> criterion = "In a job limited to 5 objects and holding 5,
                job_create is refused and nothing is charged; job_create in a killed
                job is refused.";
            attribute :>> parent = ("H.QUOTA.9", "H.OBJ.9");
            attribute :>> unit = ("object::job::Job::new_child",
                "object::job::Job::bare_child");
        }
        requirement <'L.object.66'> aNamedChildIsUnique : ItemLowLevel {
            attribute :>> statement = "Job::new_named_child shall refuse a name a
                named child of the job already has.";
            attribute :>> criterion = "A second child named a.slice beside the first
                is refused.";
            attribute :>> parent = "H.OBJ.12";
            attribute :>> unit = "object::job::Job::new_named_child";
        }
        requirement <'L.object.67'> theTreesLimits : ItemLowLevel {
            attribute :>> statement = "A new named child shall be refused when a job
                at or above its parent already holds as many descendants as its
                cgroup.max.descendants allows, or is more levels above it than its
                cgroup.max.depth allows.";
            attribute :>> criterion = "Under a job with cgroup.max.depth 1, a child
                is made and a grandchild refused Limited; under one with
                cgroup.max.descendants 2, a third descendant is refused.";
            attribute :>> parent = ("H.OBJ.12", "H.QUOTA.3");
            attribute :>> unit = ("object::job::Job::room_for_a_child",
                "object::job::Job::descendants", "object::job::Job::limits",
                "object::job::Job::walk");
        }
        requirement <'L.object.68'> rmdirKeepsALiveJob : ItemLowLevel {
            attribute :>> statement = "Job::remove_named_child shall refuse to
                remove a named child that holds a live anonymous job.";
            attribute :>> criterion = "rmdir of a cgroup holding a job native
                job_create made is refused EBUSY.";
            attribute :>> parent = "H.OBJ.12";
            attribute :>> unit = "object::job::Job::remove_named_child";
        }
        requirement <'L.object.69'> aRemovedJobTakesNothing : ItemLowLevel {
            attribute :>> statement = "A job remove_named_child has removed shall
                take no process and no child.";
            attribute :>> criterion = "Through a directory looked up before the
                rmdir, a move into the removed cgroup and a mkdir in it are each
                refused ENODEV, and the refused process stays where it was.";
            attribute :>> parent = "H.OBJ.12";
            attribute :>> unit = ("object::job::Job::remove_named_child",
                "object::job::Job::new_named_child", "object::job::Members::admits");
        }
        requirement <'L.object.70'> rmdirRefusedWhilePopulated : ItemLowLevel {
            attribute :>> statement = "Job::remove_named_child shall refuse to
                remove a named child that has a member not yet released or a named
                child of its own, and remove nothing.";
            attribute :>> criterion = "rmdir of a cgroup with one member is refused
                EBUSY and so is one with a child cgroup; after the member leaves and
                the child is removed, the rmdir succeeds.";
            attribute :>> parent = "H.OBJ.12";
            attribute :>> unit = "object::job::Job::remove_named_child";
        }
        requirement <'L.object.71'> theNoInternalProcessRule : ItemLowLevel {
            attribute :>> statement = "change_subtree_control shall enable only
                controllers offered and disable none a child still enables, and a
                job enabling a domain controller for its children shall admit no
                process.";
            attribute :>> criterion = "A controller not offered is refused; one a
                child enables cannot be disabled above it; a move into a job
                enabling a domain controller is refused, for a process counted in
                its job and for one that has left the count.";
            attribute :>> parent = "H.OBJ.12";
            attribute :>> unit = ("object::job::Job::change_subtree_control",
                "object::job::Members::admits", "object::job::Job::admits",
                "object::job::Job::subtree_control",
                "object::process::Process::move_charged");
        }
        requirement <'L.object.72'> whatCgroupfsSetsIsKept : ItemLowLevel {
            attribute :>> statement = "A job shall keep the owner, group and mode
                set for each of its cgroupfs nodes, the last set winning, and shall
                name its path from the root's child down, an anonymous job as
                job-<id>.";
            attribute :>> criterion = "A node's owner set twice reads as the second;
                a path names an anonymous job by number above its named child.";
            attribute :>> parent = "H.OBJ.12";
            attribute :>> unit = ("object::job::Job::set_node",
                "object::job::Job::path_names", "object::job::Job::display_name",
                "object::job::Job::children");
        }
        requirement <'L.object.73'> populatedIsCounted : ItemLowLevel {
            attribute :>> statement = "A job shall count its own members not yet
                released and its populated children, so that it is populated exactly
                while it or a job beneath it has a member not yet released, and each
                job whose state flips shall wake its waiters and fire its EMPTY
                registrations once no lock is held.";
            attribute :>> criterion = "A job with a member, and the job above it,
                are populated; each empties at its last member's release, not
                before, and wakes its event queue then and not while it stays
                populated.";
            attribute :>> parent = ("H.OBJ.12", "H.OBJ.11");
            attribute :>> unit = ("object::job::Job::count", "object::job::Job::count_in",
                "object::job::Job::count_out", "object::job::Job::count_in_checked",
                "object::job::Flipped::add", "object::job::notify",
                "object::job::Members::populated");
        }
        requirement <'L.object.74'> aJobsSignals : ItemLowLevel {
            attribute :>> statement = "A job shall assert EMPTY exactly while it is
                not populated, and Job::observe shall fire a registration for EMPTY
                at once if the job is empty and otherwise when it empties.";
            attribute :>> criterion = "EMPTY is asserted while the job is empty and
                not while it has a member; a registration for EMPTY fires at the
                last release and not the first, and one made while the job is empty
                fires at once.";
            attribute :>> parent = "H.OBJ.11";
            attribute :>> unit = ("object::job::Members::signals",
                "object::job::Job::observe", "object::job::Job::signals",
                "object::Object::signals");
        }
        requirement <'L.object.75'> aKilledJobSaysTerminated : ItemLowLevel {
            attribute :>> statement = "Job::kill shall fire the job's registrations
                for TERMINATED, and the job shall report TERMINATED from then on.";
            attribute :>> criterion = "A registration for TERMINATED on a job fires
                when the job is killed, its packet saying what fired it, and a wait
                then sees the job TERMINATED.";
            attribute :>> parent = "H.OBJ.17";
            attribute :>> unit = ("object::job::Job::kill", "object::job::Members::signals",
                "object::job::Job::observe");
        }
        requirement <'L.object.76'> aKillEndsTheTree : ItemLowLevel {
            attribute :>> statement = "Job::kill shall mark the job and every job
                beneath it killed, fire their TERMINATED registrations, and end
                every process in them and none elsewhere; a job killed, or beneath
                one killed, shall refuse a new process or child from then on.";
            attribute :>> criterion = "Of 4 programs in a tree of three jobs and
                none, killing the middle job ends the middle and leaf programs with
                the kill's status and not the root's; killing the root ends it; the
                bystander finishes with its own status; a job beneath a killed one
                takes no new process.";
            attribute :>> parent = "H.OBJ.9";
            attribute :>> unit = ("object::job::Job::kill", "object::job::Job::walk",
                "object::job::Job::end_members", "object::job::Job::refuses",
                "object::job::Job::contains");
        }
        requirement <'L.object.77'> cgroupKillLeavesTheJob : ItemLowLevel {
            attribute :>> statement = "Job::kill_members shall end every process in
                the job and beneath it and leave every job as usable as it was.";
            attribute :>> criterion = "cgroup.kill ends the job's 1 member and
                leaves the job neither populated, killed nor dying, and a new member
                is taken.";
            attribute :>> parent = ("H.OBJ.12", "H.OBJ.9");
            attribute :>> unit = ("object::job::Job::kill_members",
                "object::job::Job::is_dying");
        }
        requirement <'L.object.78'> aForkIntoADyingJobEnds : ItemLowLevel {
            attribute :>> statement = "A process joining a job while a kill of
                either kind runs over it, or over a job above it, shall be ended, so
                that a loop of forks cannot outrun the kill.";
            attribute :>> criterion = "A program forking in a loop inside a job is
                ended with every child by one job_kill and by one cgroup.kill: 0
                processes of the job left running after either.";
            attribute :>> parent = "H.OBJ.9";
            attribute :>> unit = "object::job::Job::is_dying";
        }
        requirement <'L.object.79'> aChainOfJobsIsFreedIteratively : ItemLowLevel {
            attribute :>> statement = "Dropping the last reference to the deepest
                job of a chain shall free every job of it with one stack frame, not
                one per job.";
            attribute :>> criterion = "A chain of 10,000 jobs each holding the one
                above, built one reference at a time as a job_create and
                handle_close loop builds it, is freed from the deepest end with no
                guard-page fault.";
            attribute :>> parent = ("H.OBJ.8", "H.FAIL.2");
            attribute :>> unit = "object::job::Job::drop";
        }
        requirement <'L.object.80'> oomsAreCountedUpTheTree : ItemLowLevel {
            attribute :>> statement = "Job::count_oom shall count an OOM or an OOM
                kill in the job and in every job above it, and wake whatever polls
                their memory.events.";
            attribute :>> criterion = "After a scoped OOM kill, memory.events counts
                one OOM kill in the job and in its parent and none in a sibling, and
                an epoll on it is woken by the kill with EPOLLPRI.";
            attribute :>> parent = "H.QUOTA.6";
            attribute :>> unit = ("object::job::Job::count_oom",
                "object::job::Job::oom_counts");
        }
    }

    package Processes {
        doc /* src/kernel/src/object/process.rs: the core half of a process. */

        requirement <'L.object.81'> aProcessStartsCountedAndCharged : ItemLowLevel {
            attribute :>> statement = "Process::new shall make a process counted in
                its job and charged one task there; when the job's task limit
                refuses the charge it shall be marked over quota and charge
                nothing.";
            attribute :>> criterion = "A fork loop in a job limited to 8 tasks is
                refused at exactly 8, counting the forking process, the refusal
                counted as pids.events counts it, while a sibling job forks on.";
            attribute :>> parent = "H.QUOTA.1";
            attribute :>> unit = ("object::process::Process::new",
                "object::process::Process::over_quota");
        }
        requirement <'L.object.82'> aTableOf4096 : ItemLowLevel {
            attribute :>> statement = "Process::new shall give the process a handle
                table that holds at most HANDLE_LIMIT, 4096, handles.";
            attribute :>> criterion = "Duplicating into one process's table until it
                is refused ends in NO_HANDLES with at most 4096 held.";
            attribute :>> parent = "H.OBJ.7";
            attribute :>> unit = "object::process::Process::new";
        }
        requirement <'L.object.83'> tasksAreGivenBack : ItemLowLevel {
            attribute :>> statement = "A process shall give every task it charged
                back to its job once, when it is reaped or dropped.";
            attribute :>> criterion = "With the fork loop's processes and the
                process moved in gone, both jobs' task counts, and their parent's,
                read 0.";
            attribute :>> parent = ("H.QUOTA.1", "H.QUOTA.4");
            attribute :>> unit = ("object::process::Process::uncharge_tasks",
                "object::process::Process::drop");
        }
        requirement <'L.object.84'> aThreadIsATask : ItemLowLevel {
            attribute :>> statement = "Process::charge_thread shall charge the job
                one task for each thread beside the first, refused at the task
                limit, and Process::uncharge_thread shall give it back once as the
                thread goes.";
            attribute :>> criterion = "In a job limited to 3 tasks, a process makes
                2 threads and a third is refused EAGAIN; as the threads end the
                job's task count falls back to 1.";
            attribute :>> parent = ("H.QUOTA.1", "H.QUOTA.4");
            attribute :>> unit = ("object::process::Process::charge_thread",
                "object::process::Process::uncharge_thread");
        }
        requirement <'L.object.85'> aMoveCarriesItsTasks : ItemLowLevel {
            attribute :>> statement = "A move shall count the process in the new job
                and out of the old one, and carry its task charge with it.";
            attribute :>> criterion = "A process of a job with 2 tasks moved into a
                job at its limit of 8 leaves 9 tasks there and 1 behind.";
            attribute :>> parent = ("H.OBJ.12", "H.QUOTA.1");
            attribute :>> unit = ("object::process::Process::move_charged",
                "object::process::Process::move_to",
                "object::process::Process::move_new_to", "object::job::Job::adopt");
        }
        requirement <'L.object.86'> aRefusedMoveLeavesItWhereItWas : ItemLowLevel {
            attribute :>> statement = "A move refused because the destination, or a
                job above it, has been killed shall leave the process in its job.";
            attribute :>> criterion = "A move into a job beneath a killed one is
                refused and the process is still in the root job.";
            attribute :>> parent = "H.OBJ.12";
            attribute :>> unit = ("object::process::Process::move_charged", "object::job::Job::refuses",
                "object::job::Job::adopt");
        }
        requirement <'L.object.87'> aNewProcessMeetsTheTaskLimit : ItemLowLevel {
            attribute :>> statement = "Process::move_new_to shall refuse a process
                being made when the destination's task limit, or one above it, would
                be passed, leaving it where it was with nothing charged there.";
            attribute :>> criterion = "process_create into a job at its task limit
                is refused SHOULD_WAIT, the job's task count unchanged, and the
                refused process is not started.";
            attribute :>> parent = "H.QUOTA.8";
            attribute :>> unit = ("object::process::Process::move_new_to",
                "object::process::Process::move_charged");
        }
        requirement <'L.object.88'> leavingTheJobOnce : ItemLowLevel {
            attribute :>> statement = "Process::leave_job shall count the process
                out of its job exactly once, however often it is called.";
            attribute :>> criterion = "A released member is counted out once: the
                job's live count falls by exactly 1 per member released.";
            attribute :>> parent = "H.OBJ.12";
            attribute :>> unit = "object::process::Process::leave_job";
        }
        requirement <'L.object.89'> oneBootstrapOnce : ItemLowLevel {
            attribute :>> statement = "A process's bootstrap slot shall take one
                object and hand it over once, refusing a second give before the take
                and after it.";
            attribute :>> criterion = "A second give to one child is refused
                ALREADY_BOUND; the child's first process_bootstrap answers the end
                given with its rights and its second answers zero; a give after the
                take is refused ALREADY_BOUND.";
            attribute :>> parent = "H.OBJ.6";
            attribute :>> unit = ("object::process::Bootstrap::refusal",
                "object::process::Bootstrap::give", "object::process::Bootstrap::take");
        }
        requirement <'L.object.90'> anEndClosesTheSlot : ItemLowLevel {
            attribute :>> statement = "A bootstrap never taken shall be closed as
                its process ends, and the slot of an ended process shall refuse a
                give.";
            attribute :>> criterion = "A bootstrap never taken is closed as its
                holder ends, and its peer hears it; a give to an ended child is
                refused.";
            attribute :>> parent = "H.OBJ.6";
            attribute :>> unit = "object::process::Process::close_bootstrap";
        }
        requirement <'L.object.91'> anExecveSealsTheSlot : ItemLowLevel {
            attribute :>> statement = "Completing an execve shall seal a bootstrap
                slot nothing was given, and keep one given before it for the new
                program to take.";
            attribute :>> criterion = "After an execve with nothing given, a give is
                refused; one given before an execve is taken after it.";
            attribute :>> parent = "H.OBJ.6";
            attribute :>> unit = "object::process::Process::seal_bootstrap";
        }
        requirement <'L.object.92'> anUndeliveredBootstrapGoesBack : ItemLowLevel {
            attribute :>> statement = "Bootstrap::put_back shall hold again a
                bootstrap that was taken and could not be placed in the table.";
            attribute :>> criterion = "process_bootstrap into a full table is
                refused NO_HANDLES and the next process_bootstrap, with room,
                answers the handle given.";
            attribute :>> parent = ("H.OBJ.6", "H.OBJ.7");
            attribute :>> unit = "object::process::Bootstrap::put_back";
        }
        requirement <'L.object.104'> anUndeliveredBootstrapOfAnEndedProcessIsClosed : ItemLowLevel {
            attribute :>> statement = "Bootstrap::put_back shall hand a bootstrap
                that could not be placed back to be closed when the process has
                ended since it was taken.";
            attribute :>> criterion = "A bootstrap taken and not placed while its
                process ends is closed, and its peer hears it.";
            attribute :>> parent = ("H.OBJ.6", "H.OBJ.7");
            attribute :>> unit = "object::process::Bootstrap::put_back";
        }
        requirement <'L.object.93'> anEndFiresItsWatches : ItemLowLevel {
            attribute :>> statement = "Exit::close shall hand the registrations
                waiting for a process's end over to be fired, once, and a
                registration made afterwards shall fire at once.";
            attribute :>> criterion = "A watch registered while the process runs
                fires when it ends, and one registered afterwards fires at once,
                with its key.";
            attribute :>> parent = "H.OBJ.17";
            attribute :>> unit = ("object::process::Exit::close", "object::process::Exit::observe");
        }
        requirement <'L.object.94'> howItEndedIsKept : ItemLowLevel {
            attribute :>> statement = "A process's Exit shall report its status and
                the signal that ended it once it has terminated, and nothing
                before.";
            attribute :>> criterion = "process_status says running before the end,
                killed by SIGTERM for a process a signal ended, and exited with its
                code for one that exited.";
            attribute :>> parent = "H.OBJ.11";
            attribute :>> unit = ("object::process::Exit::record", "object::process::Exit::status",
                "object::process::Exit::signal");
        }
        requirement <'L.object.95'> anUnstartedProcessEndsWithItsHandles : ItemLowLevel {
            attribute :>> statement = "A process made through the native ABI and
                never started shall be ended, with the kill's status, when the last
                handle to it goes.";
            attribute :>> criterion = "Closing an unstarted process's only handle
                ends it and it is freed; one whose handle rode unread in a channel
                ends when the channel is closed.";
            attribute :>> parent = "H.OBJ.8";
            attribute :>> unit = ("object::process::ProcessRef::created",
                "object::process::Control::drop", "object::process::Control::process",
                "object::process::downcast");
        }
        requirement <'L.object.96'> aHandleKeepsOnlyTheEnd : ItemLowLevel {
            attribute :>> statement = "A handle ProcessRef::new makes shall hold
                only how the process ended, and not the process.";
            attribute :>> criterion = "With nothing but a handle to it left, an
                ended process is freed.";
            attribute :>> parent = "H.OBJ.8";
            attribute :>> unit = "object::process::ProcessRef::new";
        }
        requirement <'L.object.97'> aStartedProcessIsHeldByItsTask : ItemLowLevel {
            attribute :>> statement = "Control::started shall let go of the
                reference that held a process nobody had started, so that once
                started the process is held by its task and not by the handles
                process_create made.";
            attribute :>> criterion = "A process made by process_create and started,
                which then exits, is freed while its creator still holds its handle,
                and the handle reports its status.";
            attribute :>> parent = "H.OBJ.8";
            attribute :>> unit = ("object::process::Control::started", "object::process::ProcessRef::created");
        }
        requirement <'L.object.98'> pidsAreCyclic : ItemLowLevel {
            attribute :>> statement = "allocate shall hand out the number after the
                last one handed out, skipping every number in use and wrapping past
                32767 to 300, and allocate_init shall hand out 1 only while no
                process holds it.";
            attribute :>> criterion = "Allocating through the range wraps to 300 and
                passes over a number still held; 0 numbers handed out twice.";
            attribute :>> parent = "H.OBJ.13";
            attribute :>> unit = ("object::process::allocate",
                "object::process::allocate_init", "object::process::name",
                "object::process::release_naming");
        }
        requirement <'L.object.99'> theLiveList : ItemLowLevel {
            attribute :>> statement = "live shall list every live process once,
                under its pid and not its threads' numbers, and drop its references
                with the table unlocked.";
            attribute :>> criterion = "A job kill, which finds its processes through
                live, reports exactly 1 process ended for a job with 1 member.";
            attribute :>> parent = ("H.OBJ.13", "H.OBJ.9");
            attribute :>> unit = "object::process::live";
        }
    }

    package OomKill {
        doc /* src/kernel/src/object/oom.rs, and the quota reading it starts from. */

        requirement <'L.object.100'> theKillIsScoped : ItemLowLevel {
            attribute :>> statement = "A user fault refused by a memory limit shall
                end, with the kill's status, a process of the job whose limit was
                full or beneath it and none outside it, count the kill there, and
                retry the fault.";
            attribute :>> criterion = "A program writing 8 MiB in a job limited to 1
                MiB ends by SIGKILL, exactly 1 process killed, while a process with
                more resident memory in a sibling job lives.";
            attribute :>> parent = "H.QUOTA.6";
            attribute :>> unit = ("object::oom::user_fault", "object::oom::out_of_memory",
                "object::oom::kill_within", "object::oom::at_or_above",
                "object::quota::at_limit");
        }
        requirement <'L.object.101'> theLargestIsChosen : ItemLowLevel {
            attribute :>> statement = "Among the live processes of the full job and
                the jobs beneath it, kill_within shall choose the one with the most
                resident pages, and never pid 1.";
            attribute :>> criterion = "With three processes of 1, 4 and 2 MiB
                resident in the full job, the one of 4 MiB is killed; with pid 1 the
                largest, the next largest is.";
            attribute :>> parent = "H.QUOTA.6";
            attribute :>> unit = ("object::oom::kill_within", "object::oom::at_or_above");
        }
        requirement <'L.object.102'> noLimitFullNoKill : ItemLowLevel {
            attribute :>> statement = "A fault that failed for want of memory with
                no limit full, or with nothing killable in the full job, shall fail
                as it would without the OOM kill and kill nothing.";
            attribute :>> criterion = "A fault refused with every limit unset kills
                0 processes and ends its program with SIGSEGV; a system call's fault
                there answers EFAULT.";
            attribute :>> parent = "H.QUOTA.6";
            attribute :>> unit = ("object::oom::out_of_memory", "object::oom::fault",
                "object::oom::kill_within");
        }
        requirement <'L.object.103'> anEndedVictimIsEmptied : ItemLowLevel {
            attribute :>> statement = "A victim that has ended and closed
                everything, whose address space no live process shares, shall have
                that space emptied at once rather than at its parent's wait, and a
                fault that finds a victim still ending shall wait and retry, killing
                no second process.";
            attribute :>> criterion = "Two faults at one full limit while the first
                victim is ending kill 1 process, and the job's memory use falls by
                the victim's resident pages before its parent waits.";
            attribute :>> parent = "H.QUOTA.6";
            attribute :>> unit = ("object::oom::kill_within",
                "object::oom::shared_with_the_living");
        }
    }

    package SpeculationDomains {
        doc /* docs/OPAQUE-KERNEL.md §9.2-§9.3a: a job marked at its creation,
             * whose born members' switches skip the predictor invalidation.
             * The customer's decision of 2026-10-01 (docs/BACKLOG.md). */

        requirement <'L.object.113'> barrierSkippedOnlyInsideOneDomain : ItemLowLevel {
            attribute :>> statement = "entered_space shall skip the predictor
                invalidation at a switch of address space only when the outgoing
                space's domain, read as it leaves, and the incoming space's are the
                same and not zero, and shall otherwise decide the barrier as it did
                before; forget_root shall clear a processor's last domain with its
                last root.";
            attribute :>> criterion = "Two members of one marked job switched both
                ways on one processor decide 0 barriers; a member and a process of
                no domain, and members of two marked jobs, decide 2; a switch inside
                the domain issues no invalidation; and a member installed after a
                forgotten member's root decides 1 (the `domain` line, cases 1, 2 and
                10).";
            attribute :>> parent = "H.TRAP.17";
            attribute :>> unit = ("arch::speculation::entered_space",
                "arch::speculation::same_domain", "arch::speculation::left_space",
                "arch::speculation::entering_space", "arch::speculation::forget_root",
                "user::space::AddressSpace::install",
                "user::space::AddressSpace::uninstall");
        }
        requirement <'L.object.114'> markedOnlyAtCreationUnderManage : ItemLowLevel {
            attribute :>> statement = "job_create shall make a job a speculation
                domain only with JOB_SPECULATION_DOMAIN, only under MANAGE on the
                parent, and only as it makes the job; it shall refuse any other
                option bit with INVALID_ARGS, write a DOMAIN audit record for each
                job it marks and none for a refusal, and make every other job,
                a child of a marked one included, in no domain.";
            attribute :>> criterion = "job_create with the option and a parent
                handle without MANAGE answers ACCESS_DENIED, with an unknown bit
                INVALID_ARGS, and neither adds a DOMAIN record; with MANAGE it adds
                one, naming the new job, its parent and its domain (the `domain`
                line, case 4).";
            attribute :>> parent = ("H.TRAP.17", "H.OBJ.8");
            attribute :>> unit = ("syscall::native::job_create",
                "object::job::Job::new_child_domain", "object::job::Job::new_child_in",
                "object::job::Job::domain");
        }
        requirement <'L.object.115'> membershipByBirth : ItemLowLevel {
            attribute :>> statement = "A process shall be in a speculation domain
                only if it was made in the marked job, by process_create or by a
                fork of a member in the same job; its address space shall carry the
                domain, and a space a process of another domain comes to share
                shall be in no domain from then on.";
            attribute :>> criterion = "A process made in a marked job reports the
                job's domain, and a member whose space a process of no domain shares
                decides the barrier both ways with another member (the `domain`
                line, case 6); a marked job inside a marked job is a domain of its
                own (case 7); a child job of a marked job is in no domain, a member's
                fork is a member, and a process moved into a marked job, its fork, and
                a process that left before its birth are not (cases 9 and 10).";
            attribute :>> parent = "H.TRAP.17";
            attribute :>> unit = ("object::process::Process::new",
                "object::process::Process::move_charged",
                "user::space::AddressSpace::claim_domain",
                "user::space::AddressSpace::set_birth_domain",
                "object::process::Process::speculation_domain",
                "user::space::AddressSpace::domain");
        }
        requirement <'L.object.116'> leavingIsForGood : ItemLowLevel {
            attribute :>> statement = "A member shall leave its speculation domain
                for good, and its address space with it, when it moves between jobs,
                and when it stops being dumpable: a set-id execve, a change of
                credentials, or PR_SET_DUMPABLE; and every processor whose last space
                was in the domain shall issue the barrier before the leave returns.
                In hardened builds the leave shall find them two ways, and wait for
                a grace period: a scan that asks each processor whose last domain is
                the leaver's, and a local check by which each processor, answering
                the leave's grace period with interrupts masked, compares its own
                last domain with the domains being left and issues the barrier
                itself on a match. A build with the mitigations off has no barrier,
                and its leave waits for no grace period. Each domain being left
                shall be published in a slot of its own in a set, sequentially
                consistent and before the scan and before the grace period's
                generation is advanced, and the answering processor shall read the generation
                before the set, so that one that records the domain after the scan
                has passed it is still found (F-60).";
            attribute :>> criterion = "A member moved out of its job, one whose
                credentials changed and one made not dumpable each decide the
                barrier both ways with a member left in the domain (the `domain`
                line, cases 3 and 5); a member that leaves makes this processor and one
                running a thread in its space each decide the barrier before the
                leave returns (case 8); and a processor made, by stage 9's hook
                between the scan and the grace period, to record the domain after
                the scan has passed it decides the barrier before the leave returns
                (case 11), which the local check removed fails.";
            attribute :>> parent = "H.TRAP.17";
            attribute :>> unit = ("object::process::Process::leave_speculation_domain",
                "object::process::Process::leave_speculation_domain_unstarted",
                "arch::speculation::leaving_domain", "arch::speculation::serve_wanted_barrier",
                "arch::speculation::answer_leaving", "smp::answer_grace_periods",
                "user::space::AddressSpace::leave_domain");
        }
    }

    package DeviceConfiguration {
        doc /* A published PCI node's configuration space, written by the kernel
             * and, through device_config_write, by its driver: one lock per
             * node, as a type (docs/NVIDIA.md section 12.1). */

        requirement <'L.object.117'> oneConfigurationLockPerNode : ItemLowLevel {
            attribute :>> statement = "Every write to a published node's PCI
                configuration space, by the kernel or by a driver, shall be made
                holding the node's one configuration lock, through the one type
                that holds it; enumeration's writers run at stage 10, before any
                node is published.";
            attribute :>> criterion = "A ConfigWrites is made only by taking the
                lock, MappedConfig has no write, and bus mastering, an MSI mint and
                mask, MSI-X's enable and a driver's write go through ConfigWrites.
                Bus mastering switched on and off on one processor while another
                makes a driver's configuration writes to the same function, 1,000
                rounds, reads back as dma_on says every time (the second config
                line).";
            attribute :>> parent = ("H.DMA.5", "H.IRQ.2");
            attribute :>> unit = ("device::DeviceNode::config_writes",
                "device::DeviceNode::config_writes_mapped", "device::DeviceNode::set_bus_master",
                "device::DeviceNode::config_mapping", "device::ConfigWrites::write",
                "device::ConfigWrites::inside", "device::MsiFunction::mint",
                "device::MsiFunction::set_masked", "device::MsixTable::open");
        }
    }

    package ChannelWriteRead {
        doc /* docs/OPAQUE-KERNEL.md §9.4, items 3 and 4: channel_write_read
             * (0x1013), the inbox's slot it writes into, its answer in
             * registers, and the wait it trusts. Checked through each
             * architecture's own entry by object::write_read_check, the
             * `wrread` line. */

        requirement <'L.object.128'> aSmallMessageIsHeldInTheSlot : ItemLowLevel {
            attribute :>> statement = "Endpoint::write_small shall refuse a message
                as Endpoint::write does -- the peer closed, more bytes than three
                words, the queue full -- and shall otherwise hold a message of at
                most three words without handles in the peer's slot only while
                nothing waits there and the inbox's spare has room for it or can be
                given it, queueing it behind what does, so that the slot always
                holds the oldest message and a buffer for it.";
            attribute :>> criterion = "A small message written behind a queued one
                is read after it, and a send to a closed peer, one past three words
                and one on a handle without WRITE send nothing (the `wrread` line).";
            attribute :>> parent = "H.OBJ.10";
            attribute :>> unit = ("object::channel::Endpoint::write_small",
                "object::channel::Inbox::put_small", "object::channel::Inbox::is_empty",
                "object::channel::Small::of", "object::channel::Inbox::drain");
        }
        requirement <'L.object.129'> theSlotIsEveryReadersHead : ItemLowLevel {
            attribute :>> statement = "Endpoint::read shall take the message held
                in the slot before any queued, as a message without handles in the
                inbox's spare, never failing for memory, leave it with its size
                reported when the caller has too little room, and Endpoint::unread
                shall put a message back ahead of one the slot has taken since, so
                that every reader sees the messages in the order written.";
            attribute :>> criterion = "A message held in the slot is read before
                one queued after it; a read with room for 2 bytes of a 5-byte slot
                message is refused with its size and the next read returns it whole;
                a message taken from the slot and put back after the slot took
                another is read first, then the other (the `wrread` line).";
            attribute :>> parent = "H.OBJ.7";
            attribute :>> unit = ("object::channel::Inbox::pop_fitting",
                "object::channel::Inbox::unpop", "object::channel::Inbox::head_needs_topology",
                "object::channel::Inbox::take_spare");
        }
        requirement <'L.object.130'> aSmallReadIsBytesThenZeros : ItemLowLevel {
            attribute :>> statement = "Endpoint::read_small shall take the oldest
                message only if it carries no handles and at most three words, and
                give its bytes followed by zeros to the width of three words; it
                shall leave any other message queued and report its size.";
            attribute :>> criterion = "A 5-byte message held in the slot and a
                7-byte message queued each come back from channel_write_read as
                their bytes then zeros, and a 40-byte message is refused by a small
                read and then read whole (the `wrread` line).";
            attribute :>> parent = "H.OBJ.7";
            attribute :>> unit = ("object::channel::Endpoint::read_small",
                "object::channel::Inbox::pop_small", "object::channel::Small::of");
        }
        requirement <'L.object.131'> theAnswerInRegisters : ItemLowLevel {
            attribute :>> statement = "channel_write_read shall send its count bytes
                from its third to fifth argument registers, then answer the next
                message on the same end with its size in the return register and its
                words in the second to fourth argument registers, zero past its
                bytes; a call that fails shall answer its status in the return
                register alone and leave the second to fourth argument registers as
                the program set them, on each architecture's entry.";
            attribute :>> criterion = "Through the entry, a call that sends 3 bytes
                and finds a 4-byte answer waiting answers 4 and its words, and the
                peer reads the 3 bytes then zeros; a closed handle, a count past
                three words, a send without WRITE, a message too big for registers,
                and a send and a receive on a closed peer each answer their status
                with registers 2 to 4 as sent, 6 of 6 (the `wrread` line).";
            attribute :>> parent = ("H.TRAP.10", "H.OBJ.2");
            attribute :>> unit = ("syscall::native::channel_write_read",
                "syscall::native::dispatch_write_read", "syscall::native::send_words",
                "syscall::native::receive_words", "syscall::native::read_refusal",
                "syscall::native_call", "arch::x86_64::syscall::ferrix_syscall_entry",
                "arch::aarch64::trap::system_call", "arch::armv7a::trap::system_call",
                "arch::armv7a::trap::store_words");
        }
        requirement <'L.object.132'> aTrustingWaitIsWokenByWhatEndsIt : ItemLowLevel {
            attribute :>> statement = "channel_write_read's wait shall file no
                recheck while it is listed on its end's queue, and shall end on each
                of what it waits for: a message arriving on the end, the peer's
                close, and its process's end or another thread's execve, each of
                which wakes it; it shall then answer the message, PEER_CLOSED, or
                EINTR. The message and the close shall be read from the end's state
                word and the end or execve from the task's END bit, each ordered
                against the task's blocked state by a SeqCst fence on each side
                (L.object.141, L.sched.40).";
            attribute :>> criterion = "A thread blocked in channel_write_read and
                listed on its end's queue is woken within 10 s by a message, which it
                answers, by its peer's close, which it answers PEER_CLOSED, and by
                its process's kill, after which its thread ends (the `wrread`
                line).";
            attribute :>> parent = "H.OBJ.11";
            attribute :>> unit = ("object::channel::Endpoint::readable_or_closed",
                "sched::wait::WaitQueue::wait_trusting", "sched::wait::WaitQueue::wait_sliced",
                "object::channel::Endpoint::drop");
        }
        requirement <'L.object.140'> anEndsWordIsItsInbox : ItemLowLevel {
            attribute :>> statement = "Under a channel end's inbox lock, its state
                word's NONEMPTY shall be set exactly when the inbox holds a message,
                after every operation that changes that -- a write, a small write, a
                read, a small read, a put-back and the close's drain -- and its
                PEER_CLOSED shall be set once the peer has closed, after the peer's
                closed and before the end's waiters are woken.";
            attribute :>> criterion = "4096 writes, small writes, reads, small reads
                and put-backs in a pseudo-random order on one pair leave each end's
                word equal to its inbox after every one, each operation having
                crossed the empty boundary once first, and a close is marked in the
                survivor's (the `chword` line); with the put-back's update removed
                the check fails after a put-back into an empty inbox.";
            attribute :>> parent = "H.OBJ.11";
            attribute :>> unit = ("object::channel::Half::note",
                "object::channel::Half::note_peer_closed", "object::channel::Half::word",
                "object::channel::Endpoint::read", "object::channel::Endpoint::read_small",
                "object::channel::Endpoint::write", "object::channel::Endpoint::write_small",
                "object::channel::Endpoint::unread", "object::channel::Endpoint::take_unread");
        }
        requirement <'L.object.141'> aWakerFencesTheWordBeforeItsWake : ItemLowLevel {
            attribute :>> statement = "channel_write_read's wait shall read readiness
                with one load of its end's state word and one of the caller's END
                bit, without the inbox lock or the personality; and a writer, a
                put-back and a closer shall make a SeqCst fence after the word's
                store and before their wake reads a waiter's state, paired with the
                wait's fence after it stores BLOCKED, so that a waiter listed and
                not yet blocked either is found blocked or reads the word.";
            attribute :>> criterion = "The `wrread` line's message and close cases;
                with the close's mark removed, the close case's waiter stays blocked
                and the check fails by name. The loom model of a
                waiter listed and not yet blocked against a writer, a closer, both,
                and an end finds no lost wake, and its two controls, the writer
                without its fence and the closer marking after its wake, each find
                one (`cargo xtask loom`, src/tests/loom).";
            attribute :>> parent = "H.OBJ.11";
            attribute :>> unit = ("object::channel::Endpoint::readable_or_closed",
                "syscall::native::receive_words", "sched::work::own_end");
        }
    }

    package FastPath {
        doc /* src/kernel/src/object/channel.rs and src/kernel/src/syscall/native.rs:
             * channel_write_read's fast path, its park and its continuation
             * (docs/OPAQUE-KERNEL.md §9.7, parts 2 and 3). x86-64 only. */

        requirement <'L.object.164'> aParkIsSetBesideAnEmptyInbox : ItemLowLevel {
            attribute :>> statement = "A park record shall be set only while its
                side's inbox is empty, nothing else is parked there and the peer has
                not closed, with its task set blocked in the same hold of the inbox
                lock; the receive half then makes a SeqCst fence and looks at END
                before it blocks.";
            attribute :>> criterion = "Case 4's two readers are both answered (the
                `fastcase` line); the loom model park.rs and its controls.";
            attribute :>> parent = "H.OBJ.18";
            attribute :>> unit = ("object::channel::Endpoint::park",
                "syscall::native::park_for_reply", "sched::direct::block_parked",
                "sched::direct::set_blocked");
        }
        requirement <'L.object.165'> aGeneralWriterWakesThePark : ItemLowLevel {
            attribute :>> statement = "A write, write_small, unread or close that
                changes an inbox shall take its park record under the inbox lock and
                wake the parked task after letting the lock go, counting the wait as
                ended by a wake.";
            attribute :>> criterion = "write_read_check's waits and the continuation's
                message and close cases end within the bound with the fast path on;
                with the take left out the loom control strands a reader.";
            attribute :>> parent = "H.OBJ.18";
            attribute :>> unit = ("object::channel::Inbox::take_parked",
                "object::channel::wake_parked", "sched::wait::WaitQueue::note_ended_by_a_wake");
        }
        requirement <'L.object.166'> theSendHalfDeclinesOrCommits : ItemLowLevel {
            attribute :>> statement = "send_direct shall take the two halves' inbox
                locks only if free, the first side's before the second's, held under
                the entry's interrupt mask and counted as locks on the preemption word
                that A3 checks at the switch, decline
                unless the caller's inbox is empty (T6), nothing is parked on its side
                (T7), its peer is open (T8), a reader is parked on the peer's side
                (T9) and the peer's side has no observers and no waiters (T10), stop
                the machine with FX-0531 for a record beside a message or on a
                closed end (A2), and otherwise fill the reader's reply cell, park
                the caller and hand the processor over.";
            attribute :>> criterion = "ipc-equiv's cases 3, 5, 8 and 12, and stage 9's
                case 4, reach T6 to T10 with the fast path on and get the general
                path's results; each test's control fails its case.";
            attribute :>> parent = "H.OBJ.18";
            attribute :>> unit = ("object::channel::Endpoint::send_direct",
                "object::channel::sendable");
        }
        requirement <'L.object.167'> theReplyIsZeroPastItsCount : ItemLowLevel {
            attribute :>> statement = "The words a commit hands over shall be the
                count's bytes of the sent words, the bytes past the count zero.";
            attribute :>> criterion = "ipc-equiv's case 1 with the fast path on, and
                stage 9's reply-words case, which holds the commit's words to the
                general write then read for every count (the `fastcase` line); with
                the mask past the count left out its control fails it.";
            attribute :>> parent = "H.OBJ.18";
            attribute :>> unit = ("syscall::native::reply_words", "syscall::native::words_of",
                "syscall::native::sent_bytes");
        }
        requirement <'L.object.168'> theContinuationIsTheGeneralPath : ItemLowLevel {
            attribute :>> statement = "A caller the fast path parked and woken without
                a reply shall open interrupts, stop with FX-0533 if it may not block,
                leave its record, answer EINTR if END is posted or carry on as
                receive_words does, write the call's audit record as
                dispatch_write_read does, and run regroup_current and call_left.";
            attribute :>> criterion = "The continuation's cases by a message, its
                peer's close and its kill answer the message, PEER_CLOSED and EINTR
                (the `fastcase` line).";
            attribute :>> parent = "H.OBJ.18";
            attribute :>> unit = ("syscall::native::continue_general",
                "object::channel::Endpoint::unpark");
        }
        requirement <'L.object.169'> theFastPathLooksAsTheGeneralPathDoes : ItemLowLevel {
            attribute :>> statement = "fast_write_read shall decline, having changed
                nothing but its counts, unless the filter would let the
                call through unlooked at (T2), it sends at most 24 bytes (T3), and the
                handle table's lock is free and channel_in finds an endpoint with
                READ and WRITE (T4, T5).";
            attribute :>> criterion = "Stage 9's case 11 is refused with the filter's
                errno with the fast path on, and comes back with IN_CALL down whether
                refused or allowed; ipc-equiv's case 12 answers every
                refusal as the general path does, with its audit records; every
                stage 9 echo's task names, as its cached core process, the one its
                thread's process holds.";
            attribute :>> parent = "H.OBJ.18";
            attribute :>> unit = ("syscall::native::fast_write_read",
                "object::process::Process::try_with_handles_masked",
                "sched::task::Task::core_process", "trap::filter_quiet",
                "trap::set_quiet_syscall_filter");
        }
        requirement <'L.object.170'> theWayOutsAreShared : ItemLowLevel {
            attribute :>> statement = "channel_write_read's answer shall go into the
                program's registers through write_read_outcome on the general path,
                the frame tail and the continuation alike.";
            attribute :>> criterion = "ipc-equiv's transcripts are the same on both
                boots.";
            attribute :>> parent = "H.OBJ.18";
            attribute :>> unit = "syscall::write_read_outcome";
        }
    }
}
