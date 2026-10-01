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
        doc /* src/kernel/src/object/pin.rs. The quarantine's own check,
             * check_quarantine, is in object/pin/check.rs, a child module that
             * may read what pin.rs keeps to itself, and verifies the three
             * quarantine requirements. The domain side of a pin is iommu's
             * (part 16). */

        requirement <'L.object.45'> aDeviceGetsExactlyItsPages : ItemLowLevel {
            attribute :>> statement = "A pin shall map exactly the held pages of the
                VMO into the device's domain and report each page's device address,
                in page order.";
            attribute :>> criterion = "A pin of two pages reports 2 addresses, each
                of which the domain translates to the VMO's page holding what the
                VMO holds.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("object::pin::Pin::with_cap", "object::pin::Pin::new",
                "object::pin::Pin::addresses");
        }
        requirement <'L.object.46'> unpinnedBeforeFreed : ItemLowLevel {
            attribute :>> statement = "Dropping a pin of a live owner shall take its
                pages out of the domain, the invalidation complete, before the hold
                on its frames is released; on an untranslated domain, or when the
                domain refuses, the frames shall be kept.";
            attribute :>> criterion = "On a translated domain a closed pin's pages
                are no longer reachable through the domain.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = "object::pin::Pin::drop";
        }
        requirement <'L.object.47'> aDeadDriversPinIsQuarantined : ItemLowLevel {
            attribute :>> statement = "Dropping a pin on a translated domain after
                its owner terminated shall keep its pages mapped and its frames
                held, charged to nobody, in the quarantine, and a pin closed by a
                live owner shall not be quarantined.";
            attribute :>> criterion = "A pin closed by a dead process adds its 1
                page to the domain's quarantine; one closed by a live process adds
                0.";
            attribute :>> parent = "H.DMA.4";
            attribute :>> unit = ("object::pin::Pin::drop", "object::pin::quarantine",
                "object::pin::fold");
        }
        requirement <'L.object.48'> theQuarantineIsCapped : ItemLowLevel {
            attribute :>> statement = "A pin shall be refused while its device's
                quarantine holds the cap's worth of pages.";
            attribute :>> criterion = "With a cap of one page and one page
                quarantined, the next pin is refused QuarantineFull.";
            attribute :>> parent = "H.DMA.4";
            attribute :>> unit = ("object::pin::Pin::with_cap",
                "object::pin::quarantined_pages");
        }
        requirement <'L.object.49'> theQuarantineGoesBackOnHello : ItemLowLevel {
            attribute :>> statement = "quarantine_release shall give back every
                quarantined pin of the device's domain, out of the domain first and
                then to the allocator, counting the pages written after their driver
                died.";
            attribute :>> criterion = "A release gives back exactly the pages
                quarantined and leaves the domain's quarantine at 0 pages; a pin is
                taken again afterwards.";
            attribute :>> parent = "H.DMA.4";
            attribute :>> unit = ("object::pin::quarantine_release",
                "object::pin::release", "object::pin::fold");
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
                first take back clean file cache charged to the job whose limit was
                full or beneath it and retry the fault, and only when that gave
                back nothing end, with the kill's status, a process of that job or
                beneath it and none outside it, count the kill there, and retry the
                fault.";
            attribute :>> criterion = "A program writing 8 MiB in a job limited to 1
                MiB ends by SIGKILL, exactly 1 process killed, while a process with
                more resident memory in a sibling job lives; a read of a file past
                memory.max kills no process where the job's clean cache was room.";
            attribute :>> parent = ("H.QUOTA.6", "H.QUOTA.12");
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
                no limit full shall take back clean file cache anywhere and retry
                if that gave back any, and otherwise, like one with nothing
                killable in the full job, fail as it would without the OOM kill
                and kill nothing.";
            attribute :>> criterion = "A fault refused with every limit unset and
                no cache to take back kills 0 processes and ends its program with
                SIGSEGV; a system call's fault there answers EFAULT.";
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
        requirement <'L.object.106'> cpuIsChargedUp : ItemLowLevel {
            attribute :>> statement = "charge_cpu shall charge the time a task ran
                to its job and every job above it and to the machine, as user time
                when the tick that ended the slice interrupted user mode and as
                system time otherwise, and throttled_until shall report a job whose
                subtree has used its cpu.max quota this period as throttled until
                the period ends.";
            attribute :>> criterion = "A program of two counting threads in a job
                with cpu.max 20000 100000 is held to between a twelfth and two
                fifths of a processor, its cpu.stat counts at least 3 throttled
                periods and its user and system time add to its usage, and with
                cpu.max max it takes more than six tenths.";
            attribute :>> parent = "H.QUOTA.10";
            attribute :>> unit = ("object::quota::charge_cpu",
                "object::quota::throttled_until", "object::quota::refresh_period",
                "object::quota::runtime_left", "object::quota::Quota::set_bandwidth");
        }
        requirement <'L.object.107'> aFrozenProcessParks : ItemLowLevel {
            attribute :>> statement = "sync_freeze shall set a process frozen when
                its job or one above it has cgroup.freeze set, whether the freeze
                came before the process was made, before it was moved in, or after
                it was running, and a frozen process shall report is_frozen until
                its job is thawed, which SIGCONT does not do.";
            attribute :>> criterion = "A program of three threads in a frozen
                cgroup stops counting and reports frozen through cgroup.events; a
                fork made in it, a process moved into it and one made by clone3 into
                it start frozen; SIGCONT leaves it frozen; one moved out of it is
                not.";
            attribute :>> parent = "H.QUOTA.11";
            attribute :>> unit = ("object::process::Process::sync_freeze",
                "object::process::Process::is_frozen", "object::job::Job::freezing",
                "object::job::Job::set_freeze", "object::job::Job::note_frozen");
        }
        requirement <'L.object.108'> memoryHighReclaims : ItemLowLevel {
            attribute :>> statement = "throttle shall, for a program that just took
                a page while its job or one above is over its memory.high, take
                back clean cache from that job down to the mark, count the crossing
                in memory.events high in the job whose mark it was and above, and
                pause a millisecond if that gave back less than the excess.";
            attribute :>> criterion = "A cgroup reading a file twice its memory.high
                holds no more than the mark afterwards, memory.events counts the
                crossing, and a second read takes more.";
            attribute :>> parent = "H.QUOTA.12";
            attribute :>> unit = ("object::oom::throttle", "object::quota::over_high",
                "object::quota::count", "object::quota::bump", "object::quota::Counter::at",
                "object::job::Job::wake_memory_events");
        }
        requirement <'L.object.109'> reclaimIsScoped : ItemLowLevel {
            attribute :>> statement = "reclaim shall give back clean cache pages of
                disk files whose frame is charged to the job it is asked at or
                beneath it, never a sibling's and never a child's at or under its
                memory.min, and every page it gave back shall read as its source
                has it.";
            attribute :>> criterion = "Reclaim at one cgroup leaves a sibling's
                cached file untouched, spares a child using no more than its
                memory.min, and what it took reads back as the source has it.";
            attribute :>> parent = "H.QUOTA.12";
            attribute :>> unit = ("user::cache::reclaim", "user::cache::take_from",
                "user::cache::takes", "object::quota::protected",
                "user::vmo::Vmo::decommit_range", "object::quota::within",
                "object::job::Job::mark", "object::quota::Quota::set_mark",
                "object::quota::Mark::at", "user::cache::register", "user::cache::live",
                "user::vmo::Vmo::reclaimable", "user::vmo::Vmo::pick_pages",
                "user::vmo::Vmo::for_each_page");
        }
        requirement <'L.object.110'> aGoneJobStaysGone : ItemLowLevel {
            attribute :>> statement = "job_ended shall be true for a quota slot from
                the moment its job goes, though a task still holds the slot, and
                false again once the slot is claimed anew, so that state kept per
                slot is not started for a job that has gone.";
            attribute :>> criterion = "A task of a cgroup that was removed under it
                reads the disk, and the quota slots in use are back to what they
                were before the cgroup.";
            attribute :>> parent = "H.QUOTA.4";
            attribute :>> unit = ("object::quota::job_ended", "object::quota::on_job_release");
        }
        requirement <'L.object.111'> theCacheIsCounted : ItemLowLevel {
            attribute :>> statement = "resident shall count the pages of cached
                files charged to a job and the jobs beneath it, apart from those
                of memory filesystems, for memory.stat's file and shmem.";
            attribute :>> criterion = "memory.stat's file equals the cache that
                memory.current holds, and its shmem the memory filesystem's.";
            attribute :>> parent = "H.QUOTA.12";
            attribute :>> unit = "user::cache::resident";
        }
        requirement <'L.object.112'> anEvictedPageIsNoHole : ItemLowLevel {
            attribute :>> statement = "A page of a disk file that was filled
                before a private mapping's fault took its lock and was reclaimed
                since shall not be copied or read as zeros: copy_or_zero shall
                hand the frame back and ask for the fault to be made again, and
                read_present shall answer that the page is absent.";
            attribute :>> criterion = "A page reclaim took reads back as the
                source has it, never as zeros, through a read and through a
                fault.";
            attribute :>> parent = "H.QUOTA.12";
            attribute :>> unit = ("user::space::copy_or_zero", "user::vmo::Vmo::read_present");
        }
    }
}
