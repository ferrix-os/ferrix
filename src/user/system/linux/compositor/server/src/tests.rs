//! What a client's requests do, and what breaking the rules gets.
//!
//! The bytes here are built with `compositor_wire`'s writer, which
//! `src/user/system/linux/compositor/wire`'s own probe has already shown to be libwayland's bytes,
//! so a test that writes a request writes the one a real client would.

use std::time::Duration;

use compositor_protocol::{core, xdg_shell};
use compositor_wire::{Arg, ArgType, Fd, Fixed, Header, ObjectId, Reader, Writer};

use crate::{Client, Event, Fatal, Globals, Role, Surface};

/// The globals a test connection offers: enough that binding, versions and
/// mistaken interfaces can all be tried.
fn globals() -> Globals {
    let mut globals = Globals::new();
    assert_eq!(
        globals.add(&core::WL_COMPOSITOR, 6, Role::Compositor),
        Some(1)
    );
    assert_eq!(globals.add(&core::WL_SHM, 1, Role::Shm), Some(2));
    assert_eq!(globals.add(&core::WL_SEAT, 7, Role::Seat), Some(3));
    assert_eq!(
        globals.add(&xdg_shell::XDG_WM_BASE, 6, Role::XdgWmBase),
        Some(4)
    );
    globals
}

fn client() -> Client {
    Client::new(globals())
}

/// One message's bytes, as a client would send them.
fn request(sender: u32, opcode: u16, signature: &'static [ArgType], args: &[Arg<'_>]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer
        .write(ObjectId(sender), opcode, signature, args)
        .expect("a message a client could send");
    writer.bytes().to_vec()
}

/// `wl_display.get_registry(id)`.
fn get_registry(id: u32) -> Vec<u8> {
    request(
        1,
        core::wl_display::request::GET_REGISTRY,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(id))],
    )
}

/// `wl_display.sync(id)`.
fn sync(id: u32) -> Vec<u8> {
    request(
        1,
        core::wl_display::request::SYNC,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(id))],
    )
}

/// `wl_registry.bind(name, interface, version, id)`.
fn bind(registry: u32, name: u32, interface: &str, version: u32, id: u32) -> Vec<u8> {
    request(
        registry,
        core::wl_registry::request::BIND,
        &[ArgType::Uint, ArgType::AnyNewId],
        &[
            Arg::Uint(name),
            Arg::AnyNewId {
                interface,
                version,
                id: ObjectId(id),
            },
        ],
    )
}

/// One event the server queued: who sent it, which opcode, and the arguments
/// read back with the signature that interface gives.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Sent {
    sender: ObjectId,
    opcode: u16,
    args: Vec<String>,
}

/// Everything the server has queued, decoded.
///
/// Each event is read with the signature of its sender's interface, which is
/// what a real client does, so an event written with the wrong signature
/// shows up here as a message that will not decode.
fn sent(client: &mut Client) -> Vec<Sent> {
    sent_knowing(client, &[])
}

/// The same, told what a few objects speak.
///
/// An object the *server* destroyed as it sent the event -- a
/// `wp_presentation_feedback`, which the protocol gives no destroy request
/// and which is gone the moment it is answered -- is no longer in the map,
/// so a test that wants to read one says which interface it was. A real
/// client has the same knowledge in its own proxy.
fn sent_knowing(
    client: &mut Client,
    known: &[(ObjectId, &'static compositor_protocol::Interface)],
) -> Vec<Sent> {
    let outgoing = client.take_outgoing();
    let mut reader = Reader::new(&outgoing.bytes, &outgoing.descriptors);
    let mut out = Vec::new();
    while !reader.is_done() {
        let header = reader.peek().expect("a header");
        let interface = known
            .iter()
            .find(|(id, _)| *id == header.sender)
            .map_or_else(|| interface_of(client, header), |(_, known)| *known);
        let method = interface
            .event(header.opcode)
            .unwrap_or_else(|| panic!("{} has no event {}", interface.name, header.opcode));
        let (_, args) = reader
            .read(method.signature)
            .unwrap_or_else(|error| panic!("{}.{}: {error:?}", interface.name, method.name));
        out.push(Sent {
            sender: header.sender,
            opcode: header.opcode,
            args: args.iter().map(|arg| format!("{arg:?}")).collect(),
        });
    }
    out
}

/// Which interface an event's sender speaks.
///
/// Objects the server has already destroyed -- a `wl_callback` that fired,
/// anything a destructor took -- are not in the map any more, so the two the
/// server sends to unconditionally are named here.
fn interface_of(client: &Client, header: Header) -> &'static compositor_protocol::Interface {
    if let Some(entry) = client.objects().get(header.sender) {
        return entry.interface;
    }
    if header.sender == ObjectId::DISPLAY {
        return &core::WL_DISPLAY;
    }
    &core::WL_CALLBACK
}

#[test]
fn a_fresh_connection_has_the_display_and_nothing_else() {
    let client = client();
    assert_eq!(client.objects().len(), 1);
    let display = client.objects().get(ObjectId::DISPLAY).expect("object 1");
    assert_eq!(display.interface.name, "wl_display");
    assert_eq!(display.data, Role::Display);
    assert!(!client.is_finished());
    assert_eq!(client.fatal(), None);
}

#[test]
fn get_registry_announces_every_global_in_order() {
    let mut client = client();
    let bytes = get_registry(2);
    assert_eq!(client.read(&bytes, &[]), bytes.len());

    let events = sent(&mut client);
    assert_eq!(events.len(), 4, "one global event each");
    for (event, (name, interface, version)) in events.iter().zip([
        (1u32, "wl_compositor", 6u32),
        (2, "wl_shm", 1),
        (3, "wl_seat", 7),
        (4, "xdg_wm_base", 6),
    ]) {
        assert_eq!(event.sender, ObjectId(2), "the registry sends them");
        assert_eq!(event.opcode, core::wl_registry::event::GLOBAL);
        assert_eq!(
            event.args,
            [
                format!("Uint({name})"),
                format!("Str(Some({interface:?}))"),
                format!("Uint({version})")
            ]
        );
    }
    assert!(client.objects().contains(ObjectId(2)));
}

#[test]
fn sync_fires_its_callback_and_takes_the_id_back() {
    let mut client = client();
    let bytes = sync(2);
    assert_eq!(client.read(&bytes, &[]), bytes.len());

    let events = sent(&mut client);
    assert_eq!(events.len(), 2, "done, then delete_id");
    assert_eq!(events[0].sender, ObjectId(2));
    assert_eq!(events[0].opcode, core::wl_callback::event::DONE);
    assert_eq!(events[1].sender, ObjectId::DISPLAY);
    assert_eq!(events[1].opcode, core::wl_display::event::DELETE_ID);
    assert_eq!(events[1].args, ["Uint(2)"]);

    // The id is gone, so the client may use it again -- and does.
    assert!(!client.objects().contains(ObjectId(2)));
    assert_eq!(
        client.take_events(),
        [Event::Destroyed {
            object: ObjectId(2),
            role: Role::Callback
        }]
    );
    let again = get_registry(2);
    assert_eq!(client.read(&again, &[]), again.len());
    assert!(client.objects().contains(ObjectId(2)));
    assert!(!client.is_finished());
}

#[test]
fn a_request_to_a_callback_ends_the_connection() {
    // wl_callback has no requests at all, and the server destroys it the
    // moment it fires, so a client sending to one has lost track of an id.
    let mut client = client();
    let bytes = sync(2);
    let _ = client.read(&bytes, &[]);
    let _ = client.take_outgoing();

    let stray = request(2, 0, &[], &[]);
    let _ = client.read(&stray, &[]);
    assert_eq!(client.fatal(), Some(&Fatal::NoSuchObject(ObjectId(2))));
}

#[test]
fn binding_a_global_makes_an_object_and_tells_the_compositor() {
    let mut client = client();
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_compositor", 6, 3));
    bytes.extend(bind(2, 2, "wl_shm", 1, 4));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(!client.is_finished());

    let entry = client.objects().get(ObjectId(3)).expect("bound");
    assert_eq!(entry.interface.name, "wl_compositor");
    assert_eq!(entry.version, 6);
    assert_eq!(entry.data, Role::Compositor);

    assert_eq!(
        client.take_events(),
        [
            Event::Bound {
                object: ObjectId(3),
                role: Role::Compositor,
                version: 6
            },
            Event::Bound {
                object: ObjectId(4),
                role: Role::Shm,
                version: 1
            },
        ]
    );
}

#[test]
fn a_global_may_be_bound_below_the_version_it_was_advertised_at() {
    let mut client = client();
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_compositor", 1, 3));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(!client.is_finished());
    assert_eq!(
        client.objects().get(ObjectId(3)).map(|entry| entry.version),
        Some(1)
    );
}

#[test]
fn binding_above_the_advertised_version_ends_the_connection() {
    let mut client = client();
    let mut bytes = get_registry(2);
    // wl_shm is advertised at 1 here, though the interface itself is 2.
    bytes.extend(bind(2, 2, "wl_shm", 2, 3));
    let _ = client.read(&bytes, &[]);
    assert_eq!(
        client.fatal(),
        Some(&Fatal::BadBind {
            name: 2,
            version: 2
        })
    );
    assert!(!client.objects().contains(ObjectId(3)));
}

#[test]
fn binding_a_name_with_the_wrong_interface_ends_the_connection() {
    // Name 2 is wl_shm. A client naming it while saying wl_seat would
    // otherwise get a wl_shm that answers seat requests.
    let mut client = client();
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 2, "wl_seat", 1, 3));
    let _ = client.read(&bytes, &[]);
    assert_eq!(
        client.fatal(),
        Some(&Fatal::BadBind {
            name: 2,
            version: 1
        })
    );
}

#[test]
fn binding_a_name_that_was_never_advertised_ends_the_connection() {
    let mut client = client();
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 99, "wl_compositor", 1, 3));
    let _ = client.read(&bytes, &[]);
    assert_eq!(
        client.fatal(),
        Some(&Fatal::BadBind {
            name: 99,
            version: 1
        })
    );
    // And name 0, which no global ever has.
    let mut fresh = client::new_for(globals());
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 0, "wl_compositor", 1, 3));
    let _ = fresh.read(&bytes, &[]);
    assert!(fresh.is_finished());
}

/// A second constructor for a test that needs two connections in one body.
mod client {
    use crate::{Client, Globals};

    pub(super) fn new_for(globals: Globals) -> Client {
        Client::new(globals)
    }
}

#[test]
fn an_id_already_in_use_ends_the_connection() {
    let mut client = client();
    let mut bytes = get_registry(2);
    bytes.extend(get_registry(2));
    let _ = client.read(&bytes, &[]);
    assert_eq!(client.fatal(), Some(&Fatal::BadNewId(ObjectId(2))));
}

#[test]
fn an_id_in_the_servers_half_ends_the_connection() {
    let mut client = client();
    let bytes = get_registry(ObjectId::SERVER_BASE);
    let _ = client.read(&bytes, &[]);
    assert_eq!(
        client.fatal(),
        Some(&Fatal::BadNewId(ObjectId(ObjectId::SERVER_BASE)))
    );
}

#[test]
fn a_request_to_an_object_that_is_not_live_ends_the_connection() {
    let mut client = client();
    let bytes = request(7, core::wl_surface::request::COMMIT, &[], &[]);
    let _ = client.read(&bytes, &[]);
    assert_eq!(client.fatal(), Some(&Fatal::NoSuchObject(ObjectId(7))));
}

#[test]
fn an_opcode_the_interface_does_not_have_ends_the_connection() {
    let mut client = client();
    let bytes = request(1, 9, &[], &[]);
    let _ = client.read(&bytes, &[]);
    assert_eq!(
        client.fatal(),
        Some(&Fatal::NoSuchMethod {
            object: ObjectId::DISPLAY,
            opcode: 9
        })
    );
}

#[test]
fn a_request_newer_than_the_version_bound_ends_the_connection() {
    // wl_surface.offset is since 5. A client that bound wl_compositor at 1
    // and made a surface may not send it -- but this is checked on the object
    // the request is for, so bind wl_seat at 1 and send wl_seat.release,
    // which is since 5.
    let mut client = client();
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 3, "wl_seat", 1, 3));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(!client.is_finished());
    let _ = client.take_outgoing();

    let release = request(3, core::wl_seat::request::RELEASE, &[], &[]);
    let _ = client.read(&release, &[]);
    assert_eq!(
        client.fatal(),
        Some(&Fatal::NoSuchMethod {
            object: ObjectId(3),
            opcode: core::wl_seat::request::RELEASE
        })
    );

    // At version 5 it is allowed, and is a destructor.
    let mut ok = client::new_for(globals());
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 3, "wl_seat", 5, 3));
    let _ = ok.read(&bytes, &[]);
    let _ = ok.take_outgoing();
    let _ = ok.take_events();
    let _ = ok.read(&release, &[]);
    assert!(!ok.is_finished(), "{:?}", ok.fatal());
    assert!(!ok.objects().contains(ObjectId(3)), "release destroys it");
    assert_eq!(
        ok.take_events(),
        [Event::Destroyed {
            object: ObjectId(3),
            role: Role::Seat
        }]
    );
}

#[test]
fn a_protocol_error_is_told_once_and_nothing_after_it_is_answered() {
    let mut client = client();
    let mut bytes = get_registry(2);
    // A bad bind, then a sync that would otherwise be answered.
    bytes.extend(bind(2, 99, "wl_compositor", 1, 3));
    bytes.extend(sync(4));
    let consumed = client.read(&bytes, &[]);
    assert!(consumed < bytes.len(), "the rest is not read");

    let events = sent(&mut client);
    let errors: Vec<&Sent> = events
        .iter()
        .filter(|event| {
            event.sender == ObjectId::DISPLAY && event.opcode == core::wl_display::event::ERROR
        })
        .collect();
    assert_eq!(errors.len(), 1, "one error, not one per request");
    assert_eq!(errors[0].args[0], format!("Object(ObjectId({}))", 1));
    assert_eq!(
        errors[0].args[1],
        format!("Uint({})", core::wl_display::error::INVALID_OBJECT)
    );
    assert!(
        !client.objects().contains(ObjectId(4)),
        "the sync never ran"
    );

    // And a further read does nothing at all.
    let more = sync(5);
    assert_eq!(client.read(&more, &[]), 0);
    assert!(client.take_outgoing().bytes.is_empty());
}

#[test]
fn a_message_that_has_not_all_arrived_is_left_for_the_next_read() {
    let mut client = client();
    let whole = get_registry(2);
    for cut in 0..whole.len() {
        let mut half = client::new_for(globals());
        assert_eq!(half.read(&whole[..cut], &[]), 0, "{cut} bytes");
        assert!(!half.is_finished());
    }
    // Arriving in two pieces is the same as arriving in one.
    assert_eq!(client.read(&whole[..7], &[]), 0);
    assert_eq!(client.read(&whole, &[]), whole.len());
    assert!(client.objects().contains(ObjectId(2)));
}

#[test]
fn bytes_that_are_not_a_message_end_the_connection() {
    let mut client = client();
    // wl_display.get_registry with a null new_id, which the protocol forbids.
    let mut bytes = get_registry(2);
    bytes[8..12].copy_from_slice(&0u32.to_le_bytes());
    let _ = client.read(&bytes, &[]);
    assert!(
        matches!(client.fatal(), Some(Fatal::Unreadable(_))),
        "{:?}",
        client.fatal()
    );
}

#[test]
fn every_fatal_names_an_object_a_code_and_a_sentence() {
    let cases = [
        Fatal::NoSuchObject(ObjectId(7)),
        Fatal::NoSuchMethod {
            object: ObjectId(1),
            opcode: 9,
        },
        Fatal::BadNewId(ObjectId(0)),
        Fatal::BadBind {
            name: 1,
            version: 9,
        },
        Fatal::Unreadable(compositor_wire::Error::Size(3)),
    ];
    for reason in cases {
        assert!(!reason.message().is_empty(), "{reason:?}");
        assert!(
            matches!(
                reason.code(),
                core::wl_display::error::INVALID_OBJECT | core::wl_display::error::INVALID_METHOD
            ),
            "{reason:?} has code {}",
            reason.code()
        );
        // The object named must be one wl_display.error can carry: its
        // `object_id` argument is not nullable, so an error naming zero
        // could not be encoded and the client would see a silent close.
        assert!(!reason.object().is_null(), "{reason:?}");
    }
}

#[test]
fn a_new_id_of_zero_is_refused_with_an_error_the_client_can_read() {
    // The refusal names wl_display rather than the zero the client sent,
    // because wl_display.error's object is not nullable.
    let mut client = client();
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_compositor", 6, 3));
    let _ = client.read(&bytes, &[]);
    let _ = client.take_outgoing();

    // wl_compositor.create_surface with a null new_id. The wire reader
    // refuses it before the server sees it, so this is Unreadable -- but the
    // error still has to reach the client.
    let mut surface = request(
        3,
        core::wl_compositor::request::CREATE_SURFACE,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(4))],
    );
    surface[8..12].copy_from_slice(&0u32.to_le_bytes());
    let _ = client.read(&surface, &[]);
    assert!(client.is_finished());

    let events = sent(&mut client);
    assert_eq!(events.len(), 1, "one error event");
    assert_eq!(events[0].sender, ObjectId::DISPLAY);
    assert_eq!(events[0].opcode, core::wl_display::event::ERROR);
    assert_eq!(events[0].args[0], "Object(ObjectId(1))");
}

#[test]
fn a_global_cannot_be_advertised_above_its_interface() {
    let mut globals = Globals::new();
    assert_eq!(globals.add(&core::WL_SHM, 99, Role::Shm), None);
    assert_eq!(globals.add(&core::WL_SHM, 0, Role::Shm), None);
    assert!(globals.is_empty());
    assert_eq!(
        globals.add(&core::WL_SHM, core::WL_SHM.version, Role::Shm),
        Some(1)
    );
    assert_eq!(
        globals.get(1).map(|global| global.version),
        Some(core::WL_SHM.version)
    );
    assert_eq!(globals.get(0), None, "names start at 1");
    assert_eq!(globals.get(2), None);
}

#[test]
fn a_descriptor_a_request_carried_is_taken_from_the_ones_that_arrived() {
    // No role answers an `fd` request yet, but the reader must still take the
    // descriptor so the next message gets the right one. wl_shm.create_pool
    // is the shape.
    let mut client = client();
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 2, "wl_shm", 1, 3));
    bytes.extend(request(
        3,
        core::wl_shm::request::CREATE_POOL,
        &[ArgType::NewId, ArgType::Fd, ArgType::Int],
        &[Arg::NewId(ObjectId(4)), Arg::Fd(Fd(0)), Arg::Int(4096)],
    ));
    assert_eq!(client.read(&bytes, &[Fd(11)]), bytes.len());
    assert!(!client.is_finished(), "{:?}", client.fatal());
}

// ---------------------------------------------------------------------------
// Against a real client
//
// Everything above builds the bytes with `compositor_wire` and reads them back
// with `compositor_wire`, which shows the crate agrees with itself.
// `probe/roundtrip.sh` is the other half: it runs `examples/transcript.rs` to
// get the server's answer to a client's opening conversation, replays it to a
// real libwayland client through `probe/roundtrip.c`, and records what the
// client made of it. libwayland is what every real client is built on, so a
// server it cannot follow is a server no application will run on.
// ---------------------------------------------------------------------------

/// The probe's output: the transcript, and libwayland's report of it.
const ROUNDTRIP: &str = include_str!("../probe/roundtrip.txt");

/// The globals `examples/transcript.rs` offers, which are the ones libwayland
/// should have reported.
const OFFERED: [(u32, &str, u32); 6] = [
    (1, "wl_compositor", 6),
    (2, "wl_subcompositor", 1),
    (3, "wl_shm", 1),
    (4, "wl_seat", 7),
    (5, "wl_output", 4),
    (6, "xdg_wm_base", 6),
];

/// The line of the probe's output beginning with `prefix`.
fn probe_line(prefix: &str) -> &'static str {
    ROUNDTRIP
        .lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("{prefix:?} is not in probe/roundtrip.txt"))
}

#[test]
fn a_real_libwayland_client_reads_every_global_the_server_announced() {
    let reported: Vec<&str> = ROUNDTRIP
        .lines()
        .filter(|line| line.starts_with("global "))
        .collect();
    let expected: Vec<String> = OFFERED
        .iter()
        .map(|(name, interface, version)| format!("global {name} {interface} {version}"))
        .collect();
    assert_eq!(reported, expected, "libwayland saw other globals");

    // `wl_display_roundtrip` returns the number of events it dispatched, and
    // a negative number on a protocol error; `wl_display_get_error` is the
    // errno it would report. Six globals and the callback's `done` is seven,
    // and libwayland counts the `delete_id` too.
    let result = probe_line("roundtrip ");
    let fields: Vec<&str> = result.split_whitespace().collect();
    assert_eq!(fields.get(2), Some(&"globals"));
    assert_eq!(fields.get(3).map(|count| count.parse()), Some(Ok(6usize)));
    assert_eq!(fields.get(5), Some(&"0"), "libwayland reported an error");
    let dispatched: i32 = fields
        .get(1)
        .and_then(|value| value.parse().ok())
        .expect("a count");
    assert!(dispatched > 0, "the roundtrip failed: {result}");
}

#[test]
fn the_transcript_libwayland_accepted_is_still_the_one_the_server_writes() {
    // The probe replays a recording. If the server's answer changes and the
    // recording does not, the test above is checking a conversation that no
    // longer happens, so the recording is rebuilt here and compared.
    let recorded = probe_line("transcript ")
        .strip_prefix("transcript ")
        .expect("the prefix is there");

    let mut globals = Globals::new();
    for (interface, version, role) in [
        (&core::WL_COMPOSITOR, 6, Role::Compositor),
        (&core::WL_SUBCOMPOSITOR, 1, Role::Subcompositor),
        (&core::WL_SHM, 1, Role::Shm),
        (&core::WL_SEAT, 7, Role::Seat),
        (&core::WL_OUTPUT, 4, Role::Output),
        (&xdg_shell::XDG_WM_BASE, 6, Role::XdgWmBase),
    ] {
        assert!(globals.add(interface, version, role).is_some());
    }
    let mut client = Client::new(globals);
    let mut bytes = get_registry(2);
    bytes.extend(sync(3));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(!client.is_finished(), "{:?}", client.fatal());

    let outgoing = client.take_outgoing();
    let hex: String = outgoing
        .bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        hex, recorded,
        "the server's answer changed; rerun probe/roundtrip.sh"
    );

    // And the bytes libwayland accepted carry the globals this test expects,
    // read back through the wire rather than taken on trust: six
    // `wl_registry.global`s, the callback's `done`, and the `delete_id` that
    // takes the callback's number back.
    let decoded = decode_transcript(&outgoing.bytes);
    let mut names = Vec::new();
    for (index, (sender, opcode, args)) in decoded.iter().enumerate() {
        match index {
            0..=5 => {
                assert_eq!(*sender, ObjectId(2), "the registry sends a global");
                assert_eq!(*opcode, core::wl_registry::event::GLOBAL);
                names.push(args.clone());
            }
            6 => {
                assert_eq!(*sender, ObjectId(3), "the callback fires");
                assert_eq!(*opcode, core::wl_callback::event::DONE);
            }
            _ => {
                assert_eq!(*sender, ObjectId::DISPLAY);
                assert_eq!(*opcode, core::wl_display::event::DELETE_ID);
                assert_eq!(args, &["Uint(3)".to_owned()]);
            }
        }
    }
    assert_eq!(decoded.len(), 8, "six globals, a done and a delete_id");
    let expected: Vec<Vec<String>> = OFFERED
        .iter()
        .map(|(name, interface, version)| {
            vec![
                format!("Uint({name})"),
                format!("Str(Some({interface:?}))"),
                format!("Uint({version})"),
            ]
        })
        .collect();
    assert_eq!(names, expected);
}

/// The transcript's messages: sender, opcode and arguments.
///
/// Read with the signatures of the three events the conversation holds, which
/// is all a client would need to read it either.
fn decode_transcript(bytes: &[u8]) -> Vec<(ObjectId, u16, Vec<String>)> {
    let global = &[
        ArgType::Uint,
        ArgType::Str { nullable: false },
        ArgType::Uint,
    ][..];
    let one_uint = &[ArgType::Uint][..];
    let mut reader = Reader::new(bytes, &[]);
    let mut out = Vec::new();
    while !reader.is_done() {
        let header = reader.peek().expect("a header");
        let signature = if header.sender == ObjectId(2) {
            global
        } else {
            one_uint
        };
        let (_, args) = reader.read(signature).expect("a message");
        out.push((
            header.sender,
            header.opcode,
            args.iter().map(|arg| format!("{arg:?}")).collect(),
        ));
    }
    out
}

#[test]
fn the_probe_names_the_libwayland_it_came_from() {
    let first = ROUNDTRIP.lines().next().expect("a first line");
    assert!(
        first.starts_with("# libwayland "),
        "probe/roundtrip.txt should say which libwayland wrote it: {first}"
    );
}

#[test]
fn a_real_client_s_window_setup_is_understood_request_for_request() {
    // `probe/roundtrip.c` drives a real libwayland client through everything
    // it does to put a window on screen -- bind, give the surface a role,
    // name it, commit, make a pool and a buffer, attach, damage, ask for a
    // frame callback, set the scale and commit -- and records the bytes it
    // wrote. This replays them into the server. Nothing in this test writes a
    // request: every byte is libwayland's.
    //
    // The recording is a one-way one: nothing answered the client, so it
    // never took a configure and never acked. The server therefore reads
    // every request up to the commit that carries a buffer and refuses that
    // one with `unconfigured_buffer`, which is the rule that stops a client
    // painting at a size the compositor never agreed to. The two-way
    // conversation, where the client is configured and acks, is
    // `probe/live.c`; the tests for it are at the end of this file.
    let hex = probe_line("client-requests ")
        .strip_prefix("client-requests ")
        .expect("the prefix is there");
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .map(|index| {
            u8::from_str_radix(hex.get(index * 2..index * 2 + 2).expect("in range"), 16)
                .expect("hex")
        })
        .collect();

    // The ids libwayland chose, and the buffer it asked for.
    let objects = probe_line("client-objects ");
    let ids: Vec<u32> = objects
        .split_whitespace()
        .skip(1)
        .skip(1)
        .step_by(2)
        .filter_map(|value| value.parse().ok())
        .collect();
    let [surface, region, pool, buffer, _frame] = ids.as_slice() else {
        panic!("client-objects should name five: {objects}");
    };
    let xdg: Vec<u32> = probe_line("client-xdg ")
        .split_whitespace()
        .skip(1)
        .skip(1)
        .step_by(2)
        .take(2)
        .filter_map(|value| value.parse().ok())
        .collect();
    let [xdg_surface, toplevel] = xdg.as_slice() else {
        panic!("client-xdg should name the xdg_surface and the toplevel");
    };
    let geometry: Vec<i32> = probe_line("client-geometry ")
        .split_whitespace()
        .skip(1)
        .filter_map(|value| value.parse().ok())
        .collect();
    let [width, height, stride, pool_bytes] = geometry.as_slice() else {
        panic!("client-geometry should name four");
    };

    let mut globals = Globals::new();
    for (interface, version, role) in [
        (&core::WL_COMPOSITOR, 6, Role::Compositor),
        (&core::WL_SUBCOMPOSITOR, 1, Role::Subcompositor),
        (&core::WL_SHM, 1, Role::Shm),
        (&core::WL_SEAT, 7, Role::Seat),
        (&core::WL_OUTPUT, 4, Role::Output),
        (&xdg_shell::XDG_WM_BASE, 6, Role::XdgWmBase),
    ] {
        assert!(globals.add(interface, version, role).is_some());
    }
    let mut client = Client::new(globals);

    // One descriptor arrives with the stream: the pool's.
    let consumed = client.read(&bytes, &[Fd(17)]);
    // A message is taken out of the buffer before it is dispatched, so the
    // commit that is refused is counted among the bytes read. It is the last
    // message in the recording, so every byte was read.
    assert_eq!(consumed, bytes.len());
    assert!(
        matches!(
            client.fatal(),
            Some(Fatal::Interface { code, object, .. })
                if *code == xdg_shell::xdg_surface::error::UNCONFIGURED_BUFFER
                    && object.0 == *xdg_surface
        ),
        "the refusal should be unconfigured_buffer on the xdg_surface: {:?}",
        client.fatal()
    );

    // Everything before it was understood. The pool is the memfd the client
    // sent, at the size it asked for.
    let made = client.pool(ObjectId(*pool)).expect("a pool");
    assert_eq!(made.fd, Fd(17), "the descriptor that arrived");
    assert_eq!(made.size, *pool_bytes);

    // The buffer is the rectangle the client cut.
    let cut = client.buffer(ObjectId(*buffer)).expect("a buffer");
    assert_eq!(
        cut.pool, made.key,
        "the buffer names the pool it was cut from"
    );
    assert_eq!(
        (cut.width, cut.height, cut.stride),
        (*width, *height, *stride)
    );
    assert_eq!(cut.format, crate::Format::Xrgb8888);
    assert_eq!(cut.offset, 0);
    assert_eq!(
        cut.range(),
        Some((0, usize::try_from(stride * height).expect("fits")))
    );

    // The window is there, with the title and app id the client set and the
    // window geometry it asked for.
    let top = client.toplevel(ObjectId(*toplevel)).expect("a window");
    assert_eq!(top.surface, ObjectId(*surface));
    assert_eq!(top.xdg_surface, ObjectId(*xdg_surface));
    assert_eq!(top.title, "probe window");
    assert_eq!(top.app_id, "rocks.magical.probe");
    assert_eq!(top.min_size, (1, 1));
    let shell = client
        .xdg_surface(ObjectId(*xdg_surface))
        .expect("an xdg_surface");
    assert_eq!(shell.geometry, Some((0, 0, *width, *height)));
    assert!(!shell.configured, "nothing answered, so nothing was acked");

    // The first commit -- the one with no buffer, which asks to be
    // configured -- went through, and the opaque region with it.
    let shown = client.surface(ObjectId(*surface)).expect("a surface");
    assert_eq!(shown.commits, 1);
    assert!(!shown.is_mapped(), "the buffer commit was the refused one");
    assert!(
        shown.pending.opaque.as_ref().is_some_and(|opaque| {
            opaque.contains(0, 0)
                && opaque.contains(width - 1, height - 1)
                && !opaque.contains(*width, 0)
        }),
        "the opaque region the client set"
    );
    assert!(client.region(ObjectId(*region)).is_some());

    // The compositor above was told what it has to act on.
    let events = client.take_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::PoolCreated { pool: key, .. } if *key == client.pool(ObjectId(*pool)).expect("a pool").key
        )),
        "the compositor is told to map the pool"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::ToplevelCreated { toplevel: made, .. } if made.0 == *toplevel
        )),
        "the compositor is told to place the window"
    );
}

// ---------------------------------------------------------------------------
// Surfaces, regions and shared memory: the rules the protocol states
// ---------------------------------------------------------------------------

/// A client with `wl_compositor` (4) and `wl_shm` (5) bound, as the probe's
/// does, and the outgoing bytes and events cleared.
fn drawing_client() -> Client {
    let mut client = client();
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_compositor", 6, 4));
    bytes.extend(bind(2, 2, "wl_shm", 1, 5));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(!client.is_finished(), "{:?}", client.fatal());
    let _ = client.take_outgoing();
    let _ = client.take_events();
    client
}

/// `wl_compositor.create_surface(id)`.
fn create_surface(id: u32) -> Vec<u8> {
    request(
        4,
        core::wl_compositor::request::CREATE_SURFACE,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(id))],
    )
}

/// `wl_shm.create_pool(id, fd, size)`.
fn create_pool(id: u32, size: i32) -> Vec<u8> {
    request(
        5,
        core::wl_shm::request::CREATE_POOL,
        &[ArgType::NewId, ArgType::Fd, ArgType::Int],
        &[Arg::NewId(ObjectId(id)), Arg::Fd(Fd(0)), Arg::Int(size)],
    )
}

/// `wl_shm_pool.create_buffer(id, offset, width, height, stride, format)`.
fn create_buffer(
    pool: u32,
    id: u32,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: u32,
) -> Vec<u8> {
    request(
        pool,
        core::wl_shm_pool::request::CREATE_BUFFER,
        &[
            ArgType::NewId,
            ArgType::Int,
            ArgType::Int,
            ArgType::Int,
            ArgType::Int,
            ArgType::Uint,
        ],
        &[
            Arg::NewId(ObjectId(id)),
            Arg::Int(offset),
            Arg::Int(width),
            Arg::Int(height),
            Arg::Int(stride),
            Arg::Uint(format),
        ],
    )
}

/// `wl_surface.commit()`.
fn commit(surface: u32) -> Vec<u8> {
    request(surface, core::wl_surface::request::COMMIT, &[], &[])
}

/// `wl_surface.attach(buffer, x, y)`.
fn attach(surface: u32, buffer: u32, x: i32, y: i32) -> Vec<u8> {
    request(
        surface,
        core::wl_surface::request::ATTACH,
        &[
            ArgType::Object { nullable: true },
            ArgType::Int,
            ArgType::Int,
        ],
        &[Arg::Object(ObjectId(buffer)), Arg::Int(x), Arg::Int(y)],
    )
}

#[test]
fn binding_wl_shm_announces_the_formats_the_compositor_draws() {
    let mut client = client();
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 2, "wl_shm", 1, 3));
    assert_eq!(client.read(&bytes, &[]), bytes.len());

    let formats: Vec<&Sent> = {
        let events = sent(&mut client);
        // The globals are announced first; the formats follow the bind.
        let kept: Vec<Sent> = events
            .into_iter()
            .filter(|event| event.sender == ObjectId(3))
            .collect();
        assert_eq!(kept.len(), 2, "argb8888 and xrgb8888, and nothing else");
        assert_eq!(kept[0].args, ["Uint(0)"], "argb8888 is 0");
        assert_eq!(kept[1].args, ["Uint(1)"], "xrgb8888 is 1");
        Vec::new()
    };
    assert!(formats.is_empty());
}

#[test]
fn a_surface_inherits_the_version_its_compositor_was_bound_at() {
    // The protocol's rule for every object made by another. A client bound
    // at 4 that was handed a version-6 surface could be sent
    // `preferred_buffer_scale`, which arrived in 6 and which it cannot read.
    let mut client = client();
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_compositor", 4, 4));
    bytes.extend(create_surface(3));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(
        client.objects().get(ObjectId(3)).map(|entry| entry.version),
        Some(4)
    );
    // And `wl_surface.offset`, which is since 5, is then not a request this
    // surface has.
    let offset = request(
        3,
        core::wl_surface::request::OFFSET,
        &[ArgType::Int, ArgType::Int],
        &[Arg::Int(1), Arg::Int(2)],
    );
    let _ = client.read(&offset, &[]);
    assert_eq!(
        client.fatal(),
        Some(&Fatal::NoSuchMethod {
            object: ObjectId(3),
            opcode: core::wl_surface::request::OFFSET
        })
    );
}

#[test]
fn nothing_a_client_says_takes_effect_until_it_commits() {
    let mut client = drawing_client();
    let mut bytes = create_surface(3);
    bytes.extend(create_pool(6, 4096));
    bytes.extend(create_buffer(6, 7, 0, 16, 16, 64, 1));
    bytes.extend(attach(3, 7, 0, 0));
    bytes.extend(request(
        3,
        core::wl_surface::request::SET_BUFFER_SCALE,
        &[ArgType::Int],
        &[Arg::Int(2)],
    ));
    assert_eq!(client.read(&bytes, &[Fd(3)]), bytes.len());
    assert_eq!(client.fatal(), None);

    let surface = client.surface(ObjectId(3)).expect("a surface");
    assert!(!surface.is_mapped(), "nothing is shown before a commit");
    assert_eq!(surface.current.scale, 1, "the scale is still the default");
    assert_eq!(surface.pending.buffer, Some(ObjectId(7)));
    assert_eq!(surface.pending.scale, 2);
    assert_eq!(surface.commits, 0);

    let commit_bytes = commit(3);
    assert_eq!(client.read(&commit_bytes, &[]), commit_bytes.len());
    let surface = client.surface(ObjectId(3)).expect("a surface");
    assert!(surface.is_mapped());
    assert_eq!(surface.current.scale, 2);
    assert_eq!(surface.commits, 1);
}

#[test]
fn a_commit_takes_the_damage_and_leaves_the_rest() {
    let mut client = drawing_client();
    let mut bytes = create_surface(3);
    bytes.extend(create_pool(6, 4096));
    bytes.extend(create_buffer(6, 7, 0, 16, 16, 64, 1));
    bytes.extend(attach(3, 7, 0, 0));
    bytes.extend(request(
        3,
        core::wl_surface::request::DAMAGE,
        &[ArgType::Int, ArgType::Int, ArgType::Int, ArgType::Int],
        &[Arg::Int(1), Arg::Int(2), Arg::Int(3), Arg::Int(4)],
    ));
    bytes.extend(request(
        3,
        core::wl_surface::request::SET_BUFFER_SCALE,
        &[ArgType::Int],
        &[Arg::Int(2)],
    ));
    bytes.extend(commit(3));
    bytes.extend(commit(3));
    assert_eq!(client.read(&bytes, &[Fd(3)]), bytes.len());
    assert_eq!(client.fatal(), None);

    let surface = client.surface(ObjectId(3)).expect("a surface");
    assert_eq!(surface.commits, 2);
    // The second commit took no damage, because the first consumed it.
    assert!(surface.current.damage.is_empty());
    // But the buffer and the scale carried over: a commit that did not
    // mention them leaves them alone.
    assert_eq!(surface.current.buffer, Some(ObjectId(7)));
    assert_eq!(surface.current.scale, 2);
}

#[test]
fn attaching_the_null_buffer_unmaps_the_surface() {
    let mut client = drawing_client();
    let mut bytes = create_surface(3);
    bytes.extend(create_pool(6, 4096));
    bytes.extend(create_buffer(6, 7, 0, 16, 16, 64, 1));
    bytes.extend(attach(3, 7, 0, 0));
    bytes.extend(commit(3));
    assert_eq!(client.read(&bytes, &[Fd(3)]), bytes.len());
    let _ = client.take_events();
    assert!(client.surface(ObjectId(3)).is_some_and(Surface::is_mapped));

    let mut down = attach(3, 0, 0, 0);
    down.extend(commit(3));
    assert_eq!(client.read(&down, &[]), down.len());
    let surface = client.surface(ObjectId(3)).expect("a surface");
    assert!(!surface.is_mapped());

    let change = client
        .take_events()
        .into_iter()
        .find_map(|event| match event {
            Event::SurfaceCommitted { change, .. } => Some(change),
            _ => None,
        })
        .expect("a commit");
    assert!(change.unmapped);
    assert_eq!(
        change.released,
        Some(ObjectId(7)),
        "the buffer is the client's again"
    );
}

#[test]
fn the_same_buffer_committed_twice_is_not_released() {
    // A client that commits the same buffer again never got it back, so
    // releasing it would tell the client it may draw into memory the
    // compositor is still showing.
    let mut client = drawing_client();
    let mut bytes = create_surface(3);
    bytes.extend(create_pool(6, 4096));
    bytes.extend(create_buffer(6, 7, 0, 16, 16, 64, 1));
    bytes.extend(attach(3, 7, 0, 0));
    bytes.extend(commit(3));
    bytes.extend(commit(3));
    assert_eq!(client.read(&bytes, &[Fd(3)]), bytes.len());

    let released: Vec<Option<ObjectId>> = client
        .take_events()
        .into_iter()
        .filter_map(|event| match event {
            Event::SurfaceCommitted { change, .. } => Some(change.released),
            _ => None,
        })
        .collect();
    assert_eq!(released, [None, None]);
}

#[test]
fn a_new_buffer_releases_the_one_it_replaced() {
    let mut client = drawing_client();
    let mut bytes = create_surface(3);
    bytes.extend(create_pool(6, 8192));
    bytes.extend(create_buffer(6, 7, 0, 16, 16, 64, 1));
    bytes.extend(create_buffer(6, 8, 1024, 16, 16, 64, 1));
    bytes.extend(attach(3, 7, 0, 0));
    bytes.extend(commit(3));
    bytes.extend(attach(3, 8, 0, 0));
    bytes.extend(commit(3));
    assert_eq!(client.read(&bytes, &[Fd(3)]), bytes.len());
    assert_eq!(client.fatal(), None);

    let released: Vec<Option<ObjectId>> = client
        .take_events()
        .into_iter()
        .filter_map(|event| match event {
            Event::SurfaceCommitted { change, .. } => Some(change.released),
            _ => None,
        })
        .collect();
    assert_eq!(released, [None, Some(ObjectId(7))]);

    // And telling the client so is one wl_buffer.release on that object.
    let _ = client.take_outgoing();
    client.release_buffer(ObjectId(7));
    let events = sent(&mut client);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sender, ObjectId(7));
    assert_eq!(events[0].opcode, core::wl_buffer::event::RELEASE);
}

#[test]
fn a_frame_callback_fires_once_and_takes_its_id_back() {
    let mut client = drawing_client();
    let mut bytes = create_surface(3);
    bytes.extend(request(
        3,
        core::wl_surface::request::FRAME,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(9))],
    ));
    bytes.extend(commit(3));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    let _ = client.take_outgoing();

    client.fire_frame_callbacks(ObjectId(3), 1234);
    let events = sent(&mut client);
    assert_eq!(events.len(), 2, "done, then delete_id");
    assert_eq!(events[0].sender, ObjectId(9));
    assert_eq!(events[0].args, ["Uint(1234)"]);
    assert_eq!(events[1].opcode, core::wl_display::event::DELETE_ID);
    assert!(!client.objects().contains(ObjectId(9)));

    // Firing again sends nothing: a frame callback fires once.
    client.fire_frame_callbacks(ObjectId(3), 2345);
    assert!(client.take_outgoing().bytes.is_empty());
}

#[test]
fn a_buffer_must_lie_inside_its_pool() {
    // libwayland's own check, and one place this is stricter than it: its
    // `stride < width` compares bytes with pixels, so a four-byte format
    // with `stride == width` passes there and gives a compositor reading
    // `width * 4` bytes a row an out-of-bounds read on the last one.
    let cases: [(i32, i32, i32, i32, &str); 7] = [
        (0, 16, 16, 63, "a stride below width * 4"),
        (0, 16, 16, 16, "libwayland's stride == width"),
        (-1, 16, 16, 64, "a negative offset"),
        (0, 0, 16, 64, "no width"),
        (0, 16, 0, 64, "no height"),
        (0, 16, 65, 64, "past the end of the pool"),
        (4000, 16, 16, 64, "an offset that pushes it past the end"),
    ];
    for (offset, width, height, stride, why) in cases {
        let mut client = drawing_client();
        let mut bytes = create_pool(6, 4096);
        bytes.extend(create_buffer(6, 7, offset, width, height, stride, 1));
        let _ = client.read(&bytes, &[Fd(3)]);
        assert!(
            matches!(
                client.fatal(),
                Some(Fatal::Interface { code, .. })
                    if *code == core::wl_shm::error::INVALID_STRIDE
            ),
            "{why} was accepted: {:?}",
            client.fatal()
        );
        assert!(client.buffer(ObjectId(7)).is_none(), "{why}");
    }

    // And the one that fits exactly is accepted.
    let mut client = drawing_client();
    let mut bytes = create_pool(6, 4096);
    bytes.extend(create_buffer(6, 7, 0, 16, 64, 64, 1));
    assert_eq!(client.read(&bytes, &[Fd(3)]), bytes.len());
    assert_eq!(client.fatal(), None, "64 rows of 64 bytes is exactly 4096");
}

#[test]
fn a_format_the_compositor_does_not_draw_is_refused() {
    // Announcing a format and then not drawing it is a client rendering a
    // frame nobody can show, so only the two mandatory ones are offered and
    // only those are accepted. 0x36314752 is rgb565.
    for format in [2u32, 0x3631_4752, u32::MAX] {
        let mut client = drawing_client();
        let mut bytes = create_pool(6, 4096);
        bytes.extend(create_buffer(6, 7, 0, 16, 16, 64, format));
        let _ = client.read(&bytes, &[Fd(3)]);
        assert!(
            matches!(
                client.fatal(),
                Some(Fatal::Interface { code, .. })
                    if *code == core::wl_shm::error::INVALID_FORMAT
            ),
            "format {format:#x} was accepted"
        );
    }
}

#[test]
fn a_pool_may_grow_and_may_not_shrink() {
    let mut client = drawing_client();
    let bytes = create_pool(6, 4096);
    assert_eq!(client.read(&bytes, &[Fd(3)]), bytes.len());
    let _ = client.take_events();

    let resize = |size: i32| {
        request(
            6,
            core::wl_shm_pool::request::RESIZE,
            &[ArgType::Int],
            &[Arg::Int(size)],
        )
    };
    let grow = resize(8192);
    assert_eq!(client.read(&grow, &[]), grow.len());
    assert_eq!(client.pool(ObjectId(6)).map(|pool| pool.size), Some(8192));
    assert_eq!(
        client.take_events(),
        [Event::PoolResized {
            pool: client.pool(ObjectId(6)).expect("a pool").key,
            size: 8192
        }]
    );

    // A shrink is ignored rather than refused: the protocol says the request
    // can only make a pool bigger, but gives no error code for it, and
    // libwayland keeps the connection.
    let shrink = resize(1024);
    assert_eq!(client.read(&shrink, &[]), shrink.len());
    assert!(!client.is_finished());
    assert_eq!(client.pool(ObjectId(6)).map(|pool| pool.size), Some(8192));
    assert!(client.take_events().is_empty());

    // A buffer past the old size fits after the grow.
    let bigger = create_buffer(6, 7, 4096, 16, 16, 64, 1);
    assert_eq!(client.read(&bigger, &[]), bigger.len());
    assert_eq!(client.fatal(), None);
}

#[test]
fn a_pool_of_no_bytes_is_refused() {
    for size in [0, -1, i32::MIN] {
        let mut client = drawing_client();
        let bytes = create_pool(6, size);
        let _ = client.read(&bytes, &[Fd(3)]);
        assert!(
            matches!(
                client.fatal(),
                Some(Fatal::Interface { code, .. })
                    if *code == core::wl_shm::error::INVALID_FD
            ),
            "a pool of {size} bytes was accepted"
        );
    }
}

#[test]
fn attaching_something_that_is_not_a_buffer_ends_the_connection() {
    let mut client = drawing_client();
    let mut bytes = create_surface(3);
    // Object 4 is the wl_compositor, not a buffer.
    bytes.extend(attach(3, 4, 0, 0));
    let _ = client.read(&bytes, &[]);
    assert_eq!(
        client.fatal(),
        Some(&Fatal::WrongInterface {
            object: ObjectId(4),
            wanted: "wl_buffer"
        })
    );
}

/// swaybg's way, the wlroots world's: the pool destroyed as soon as the
/// buffer is cut from it, the buffer as soon as it is committed. The surface
/// goes on showing it -- its description and its memory are kept -- and is
/// never told the destroyed id is released, which may be another object's
/// by then. libwayland gives the id to the next buffer; attaching that one
/// shows it, and the first pool's memory is let go.
#[test]
fn a_buffer_destroyed_after_its_commit_is_shown_until_the_next() {
    let mut client = drawing_client();
    let mut bytes = create_surface(3);
    bytes.extend(create_pool(6, 4096));
    bytes.extend(create_buffer(6, 7, 0, 16, 16, 64, 1));
    bytes.extend(request(6, core::wl_shm_pool::request::DESTROY, &[], &[]));
    bytes.extend(attach(3, 7, 0, 0));
    bytes.extend(commit(3));
    bytes.extend(request(7, core::wl_buffer::request::DESTROY, &[], &[]));
    assert_eq!(client.read(&bytes, &[Fd(3)]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(client.buffer(ObjectId(7)).is_none(), "the object is gone");
    let shown = client
        .shown_buffer(ObjectId(3))
        .copied()
        .expect("still shown");
    assert_eq!((shown.width, shown.height), (16, 16));
    assert!(client.surface(ObjectId(3)).is_some_and(Surface::is_mapped));
    assert!(client.pool_in_use(shown.pool), "its memory is kept");
    let _ = client.take_events();

    // A commit with nothing attached changes nothing.
    let again = commit(3);
    assert_eq!(client.read(&again, &[]), again.len());
    assert!(client.shown_buffer(ObjectId(3)).is_some());
    let _ = client.take_events();

    // The next buffer under the same id, attached and committed.
    let mut next = create_pool(8, 8192);
    next.extend(create_buffer(8, 7, 0, 32, 32, 128, 1));
    next.extend(attach(3, 7, 0, 0));
    next.extend(commit(3));
    let _ = client.take_outgoing();
    assert_eq!(client.read(&next, &[Fd(4)]), next.len());
    assert_eq!(client.fatal(), None);
    let events = client.take_events();
    let released: Vec<Option<ObjectId>> = events
        .iter()
        .filter_map(|event| match event {
            Event::SurfaceCommitted { change, .. } => Some(change.released),
            _ => None,
        })
        .collect();
    assert_eq!(released, [None], "nothing is released to the reused id");
    assert!(events.contains(&Event::PoolRetired { pool: shown.pool }));
    assert!(!client.pool_in_use(shown.pool), "the first pool is let go");
    let now = client
        .shown_buffer(ObjectId(3))
        .copied()
        .expect("the new one");
    assert_eq!((now.width, now.height), (32, 32));
}

/// The control: a buffer destroyed after its attach and before the commit
/// leaves the commit nothing to show.
#[test]
fn a_buffer_destroyed_before_its_commit_shows_nothing() {
    let mut client = drawing_client();
    let mut bytes = create_surface(3);
    bytes.extend(create_pool(6, 4096));
    bytes.extend(create_buffer(6, 7, 0, 16, 16, 64, 1));
    bytes.extend(attach(3, 7, 0, 0));
    bytes.extend(request(7, core::wl_buffer::request::DESTROY, &[], &[]));
    bytes.extend(commit(3));
    assert_eq!(client.read(&bytes, &[Fd(3)]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(client.shown_buffer(ObjectId(3)).is_none());
    assert!(!client.surface(ObjectId(3)).is_some_and(Surface::is_mapped));
}

#[test]
fn a_region_is_the_rectangles_added_less_the_ones_taken_away() {
    let mut client = drawing_client();
    let mut bytes = request(
        4,
        core::wl_compositor::request::CREATE_REGION,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(3))],
    );
    let rect = |opcode: u16, x, y, w, h| {
        request(
            3,
            opcode,
            &[ArgType::Int, ArgType::Int, ArgType::Int, ArgType::Int],
            &[Arg::Int(x), Arg::Int(y), Arg::Int(w), Arg::Int(h)],
        )
    };
    bytes.extend(rect(core::wl_region::request::ADD, 0, 0, 10, 10));
    bytes.extend(rect(core::wl_region::request::SUBTRACT, 2, 2, 3, 3));
    // An empty rectangle is dropped rather than refused: the protocol gives
    // no error for one and libwayland passes it through.
    bytes.extend(rect(core::wl_region::request::ADD, 0, 0, 0, 5));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    let region = client.region(ObjectId(3)).expect("a region");
    assert!(region.contains(0, 0));
    assert!(region.contains(9, 9));
    assert!(!region.contains(2, 2), "subtracted");
    assert!(!region.contains(4, 4), "subtracted");
    assert!(region.contains(5, 5), "just past the subtraction");
    assert!(!region.contains(10, 0), "outside");
    assert_eq!(region.operations().len(), 2, "the empty one was dropped");
}

#[test]
fn a_surface_scale_below_one_and_a_transform_that_is_not_one_are_refused() {
    for scale in [0, -1, i32::MIN] {
        let mut client = drawing_client();
        let mut bytes = create_surface(3);
        bytes.extend(request(
            3,
            core::wl_surface::request::SET_BUFFER_SCALE,
            &[ArgType::Int],
            &[Arg::Int(scale)],
        ));
        let _ = client.read(&bytes, &[]);
        assert!(
            matches!(
                client.fatal(),
                Some(Fatal::Interface { code, .. })
                    if *code == core::wl_surface::error::INVALID_SCALE
            ),
            "a scale of {scale} was accepted"
        );
    }
    for transform in [8, 9, i32::MAX, -1] {
        let mut client = drawing_client();
        let mut bytes = create_surface(3);
        bytes.extend(request(
            3,
            core::wl_surface::request::SET_BUFFER_TRANSFORM,
            &[ArgType::Int],
            &[Arg::Int(transform)],
        ));
        let _ = client.read(&bytes, &[]);
        assert!(
            matches!(
                client.fatal(),
                Some(Fatal::Interface { code, .. })
                    if *code == core::wl_surface::error::INVALID_TRANSFORM
            ),
            "a transform of {transform} was accepted"
        );
    }
    // Every value wl_output.transform does have is taken.
    for transform in 0..=7 {
        let mut client = drawing_client();
        let mut bytes = create_surface(3);
        bytes.extend(request(
            3,
            core::wl_surface::request::SET_BUFFER_TRANSFORM,
            &[ArgType::Int],
            &[Arg::Int(transform)],
        ));
        assert_eq!(client.read(&bytes, &[]), bytes.len());
        assert_eq!(client.fatal(), None, "transform {transform}");
    }
}

#[test]
fn destroying_a_surface_takes_its_state_with_it() {
    let mut client = drawing_client();
    let mut bytes = create_surface(3);
    bytes.extend(commit(3));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(client.surface(ObjectId(3)).is_some());

    let destroy = request(3, core::wl_surface::request::DESTROY, &[], &[]);
    assert_eq!(client.read(&destroy, &[]), destroy.len());
    assert!(client.surface(ObjectId(3)).is_none());
    assert!(!client.objects().contains(ObjectId(3)));
    assert!(client.take_events().iter().any(|event| matches!(
        event,
        Event::Destroyed {
            object,
            role: Role::Surface
        } if *object == ObjectId(3)
    )));
}

// ---------------------------------------------------------------------------
// The whole handshake, over a real socket
//
// `probe/roundtrip.sh` starts `examples/serve.rs` on a socket and runs
// `probe/live.c`, a real libwayland client, against it. That conversation is
// the one every application has when it starts, and it cannot be replayed
// from a recording: an `xdg_surface.configure` answers a request whose ids
// the client chose. What the client was told and what the server saw are both
// recorded, and the tests below require each step of it.
// ---------------------------------------------------------------------------

/// A line of the probe's live client output, without its prefix.
fn client_said(text: &str) -> bool {
    ROUNDTRIP
        .lines()
        .any(|line| line.strip_prefix("client ") == Some(text))
}

/// A line of the probe's live server output, without its prefix.
fn server_said(text: &str) -> bool {
    ROUNDTRIP
        .lines()
        .any(|line| line.strip_prefix("server ") == Some(text))
}

/// The live client's `result` line, as fields.
fn live_result() -> Vec<String> {
    ROUNDTRIP
        .lines()
        .find_map(|line| line.strip_prefix("client result "))
        .unwrap_or_else(|| panic!("the live client printed no result"))
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_real_client_completes_the_whole_window_handshake_over_a_socket() {
    // It connected and bound what it needed.
    assert!(client_said("connected"), "the client could not connect");
    assert!(client_said("bound"), "a global was missing");
    assert!(server_said("accepted"));

    // wl_shm told it both formats the compositor draws, and no others.
    assert!(client_said("format 0"), "argb8888 was not announced");
    assert!(client_said("format 1"), "xrgb8888 was not announced");

    // It made a window and the server saw it, with the title and app id.
    assert!(server_said("toplevel 8 on surface 3"));
    assert!(server_said("renamed 8"), "set_title and set_app_id");

    // The first commit carried no buffer, which is what asks to be
    // configured, and the server configured it.
    assert!(server_said(
        "commit 3 buffer None mapped false unmapped false"
    ));
    assert!(server_said("configured 8 640 480"));

    // The client was told that size and those states, and acked.
    assert!(
        client_said("toplevel-configure 640 480 states 2"),
        "the configure did not reach the client, or carried other numbers"
    );
    assert!(
        client_said("surface-configure serial 1"),
        "the xdg_surface.configure did not arrive with its serial"
    );

    // Only then did it attach a buffer, and the server mapped the surface.
    assert!(server_said("pool 9 size 4096"), "the memfd did not arrive");
    assert!(server_said(
        "commit 3 buffer Some(10) mapped true unmapped false"
    ));

    // Nothing went wrong on either side.
    assert!(
        !ROUNDTRIP
            .lines()
            .any(|line| line.contains("protocol error"))
    );
    assert!(server_said("client gone"), "the server saw a clean close");
}

#[test]
fn the_configure_the_client_took_is_the_one_the_server_sent() {
    let fields = live_result();
    let value = |name: &str| -> Option<&str> {
        let index = fields.iter().position(|field| field == name)?;
        fields.get(index + 1).map(String::as_str)
    };
    assert_eq!(value("configures"), Some("1"), "one configure, acked once");
    assert_eq!(value("size"), Some("640x480"), "examples/serve.rs's size");
    // `activated` and `tiled_left` are what a tiling compositor sends, and
    // are the two states `examples/serve.rs` configures with.
    assert_eq!(value("activated"), Some("1"));
    assert_eq!(value("tiled"), Some("1"));
    assert_eq!(value("formats"), Some("2"));
    assert_eq!(
        value("error"),
        Some("0"),
        "libwayland reported a protocol error"
    );
}

#[test]
fn a_buffer_before_the_first_ack_is_refused() {
    // The rule the handshake above exists for. A client that attached before
    // acking would be painting at a size the compositor never agreed to.
    let mut client = drawing_client();
    let mut bytes = bind(2, 4, "xdg_wm_base", 6, 6);
    // The globals in `client()` are compositor 1, shm 2, seat 3, xdg 4.
    bytes.extend(create_surface(3));
    bytes.extend(request(
        6,
        xdg_shell::xdg_wm_base::request::GET_XDG_SURFACE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(7)), Arg::Object(ObjectId(3))],
    ));
    bytes.extend(request(
        7,
        xdg_shell::xdg_surface::request::GET_TOPLEVEL,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(8))],
    ));
    bytes.extend(create_pool(9, 4096));
    bytes.extend(create_buffer(9, 10, 0, 16, 16, 64, 1));
    bytes.extend(attach(3, 10, 0, 0));
    bytes.extend(commit(3));
    let _ = client.read(&bytes, &[Fd(3)]);
    assert!(
        matches!(
            client.fatal(),
            Some(Fatal::Interface { code, object, .. })
                if *code == xdg_shell::xdg_surface::error::UNCONFIGURED_BUFFER
                    && *object == ObjectId(7)
        ),
        "a buffer before the first ack was accepted: {:?}",
        client.fatal()
    );
}

#[test]
fn a_serial_that_was_never_sent_is_refused_and_an_old_one_is_not() {
    let mut client = drawing_client();
    let mut bytes = bind(2, 4, "xdg_wm_base", 6, 6);
    bytes.extend(create_surface(3));
    bytes.extend(request(
        6,
        xdg_shell::xdg_wm_base::request::GET_XDG_SURFACE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(7)), Arg::Object(ObjectId(3))],
    ));
    bytes.extend(request(
        7,
        xdg_shell::xdg_surface::request::GET_TOPLEVEL,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(8))],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    // Three configures, none acked yet.
    client.configure_toplevel(ObjectId(8), 100, 200, &[]);
    client.configure_toplevel(ObjectId(8), 110, 210, &[]);
    client.configure_toplevel(ObjectId(8), 120, 220, &[]);
    let waiting = client.xdg_surface(ObjectId(7)).expect("an xdg_surface");
    assert_eq!(waiting.unacked.len(), 3);
    let middle = waiting.unacked[1];

    // A client several configures behind acks the one it acted on, and that
    // drops the older ones with it: `ack_configure`'s own description.
    let ack = |serial: u32| {
        request(
            7,
            xdg_shell::xdg_surface::request::ACK_CONFIGURE,
            &[ArgType::Uint],
            &[Arg::Uint(serial)],
        )
    };
    let bytes = ack(middle);
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    let after = client.xdg_surface(ObjectId(7)).expect("an xdg_surface");
    assert_eq!(after.unacked.len(), 1, "the older ones went with it");
    assert!(after.configured, "a buffer is allowed now");

    // Acking it again, now that it is gone, is invalid_serial.
    let bytes = ack(middle);
    let _ = client.read(&bytes, &[]);
    assert!(
        matches!(
            client.fatal(),
            Some(Fatal::Interface { code, .. })
                if *code == xdg_shell::xdg_surface::error::INVALID_SERIAL
        ),
        "{:?}",
        client.fatal()
    );
}

#[test]
fn a_surface_may_be_given_one_role_and_no_second() {
    let mut client = drawing_client();
    let mut bytes = bind(2, 4, "xdg_wm_base", 6, 6);
    bytes.extend(create_surface(3));
    let get_xdg = |id: u32, surface: u32| {
        request(
            6,
            xdg_shell::xdg_wm_base::request::GET_XDG_SURFACE,
            &[ArgType::NewId, ArgType::Object { nullable: false }],
            &[Arg::NewId(ObjectId(id)), Arg::Object(ObjectId(surface))],
        )
    };
    let get_toplevel = |xdg: u32, id: u32| {
        request(
            xdg,
            xdg_shell::xdg_surface::request::GET_TOPLEVEL,
            &[ArgType::NewId],
            &[Arg::NewId(ObjectId(id))],
        )
    };
    bytes.extend(get_xdg(7, 3));
    bytes.extend(get_toplevel(7, 8));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    // A second toplevel on the same xdg_surface.
    let again = get_toplevel(7, 9);
    let _ = client.read(&again, &[]);
    assert!(
        matches!(
            client.fatal(),
            Some(Fatal::Interface { code, .. })
                if *code == xdg_shell::xdg_surface::error::ALREADY_CONSTRUCTED
        ),
        "{:?}",
        client.fatal()
    );

    // And a second xdg_surface on the same wl_surface.
    let mut other = drawing_client();
    let mut bytes = bind(2, 4, "xdg_wm_base", 6, 6);
    bytes.extend(create_surface(3));
    bytes.extend(get_xdg(7, 3));
    bytes.extend(get_xdg(9, 3));
    let _ = other.read(&bytes, &[]);
    assert!(
        matches!(
            other.fatal(),
            Some(Fatal::Interface { code, .. })
                if *code == xdg_shell::xdg_wm_base::error::ROLE
        ),
        "{:?}",
        other.fatal()
    );
}

#[test]
fn a_window_keeps_the_title_and_app_id_hyprctl_prints() {
    let mut client = drawing_client();
    let mut bytes = bind(2, 4, "xdg_wm_base", 6, 6);
    bytes.extend(create_surface(3));
    bytes.extend(request(
        6,
        xdg_shell::xdg_wm_base::request::GET_XDG_SURFACE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(7)), Arg::Object(ObjectId(3))],
    ));
    bytes.extend(request(
        7,
        xdg_shell::xdg_surface::request::GET_TOPLEVEL,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(8))],
    ));
    for (opcode, text) in [
        (xdg_shell::xdg_toplevel::request::SET_TITLE, "a window"),
        (
            xdg_shell::xdg_toplevel::request::SET_APP_ID,
            "rocks.magical.test",
        ),
    ] {
        bytes.extend(request(
            8,
            opcode,
            &[ArgType::Str { nullable: false }],
            &[Arg::Str(Some(text))],
        ));
    }
    bytes.extend(request(
        8,
        xdg_shell::xdg_toplevel::request::SET_MAXIMIZED,
        &[],
        &[],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    let top = client.toplevel(ObjectId(8)).expect("a window");
    assert_eq!(top.title, "a window");
    assert_eq!(top.app_id, "rocks.magical.test");
    assert_eq!(top.surface, ObjectId(3));
    assert_eq!(top.xdg_surface, ObjectId(7));
    assert!(top.maximized);
    assert!(!top.fullscreen);
    assert_eq!(client.toplevels().count(), 1);

    // And `killactive` asks it to close, which is a request and not an
    // order: the client may decline.
    let _ = client.take_outgoing();
    client.close_toplevel(ObjectId(8));
    let events = sent(&mut client);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sender, ObjectId(8));
    assert_eq!(events[0].opcode, xdg_shell::xdg_toplevel::event::CLOSE);
}

// ---------------------------------------------------------------------------
// The interfaces a real toolkit asks for
//
// Every one of these was written because `src/user/system/linux/compositor/hyprix/probe/
// real-client.sh` ran a third-party terminal against the compositor and it
// stopped: first because `wl_data_device_manager` was not offered at all,
// then because `wl_subcompositor.get_subsurface` was offered and not
// answered, then because `wl_output` described nothing. A client written
// against this tree's own crates would not have found any of them.
// ---------------------------------------------------------------------------

/// A client with `wl_subcompositor` (6), `wl_seat` (7), `wl_output` (8) and
/// `wl_data_device_manager` (9) bound beside the two `drawing_client` has.
fn full_client(capabilities: u32) -> Client {
    let mut globals = Globals::new();
    for (interface, version, role) in [
        (&core::WL_COMPOSITOR, 6, Role::Compositor),
        (&core::WL_SHM, 1, Role::Shm),
        (&core::WL_SEAT, 7, Role::Seat),
        (&xdg_shell::XDG_WM_BASE, 6, Role::XdgWmBase),
        (&core::WL_SUBCOMPOSITOR, 1, Role::Subcompositor),
        (&core::WL_OUTPUT, 4, Role::Output),
        (&core::WL_DATA_DEVICE_MANAGER, 3, Role::DataDeviceManager),
    ] {
        assert!(globals.add(interface, version, role).is_some());
    }
    let mut client = Client::new(globals);
    client.set_seat_capabilities(capabilities);
    client.set_output(crate::Output {
        width: 1024,
        height: 768,
        ..crate::Output::default()
    });
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_compositor", 6, 4));
    bytes.extend(bind(2, 2, "wl_shm", 1, 5));
    bytes.extend(bind(2, 5, "wl_subcompositor", 1, 6));
    bytes.extend(bind(2, 3, "wl_seat", 7, 7));
    bytes.extend(bind(2, 6, "wl_output", 4, 8));
    bytes.extend(bind(2, 7, "wl_data_device_manager", 3, 9));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    client
}

#[test]
fn a_fresh_output_is_told_what_the_screen_is() {
    let mut client = full_client(0);
    let events: Vec<Sent> = sent(&mut client)
        .into_iter()
        .filter(|event| event.sender == ObjectId(8))
        .collect();
    let opcodes: Vec<u16> = events.iter().map(|event| event.opcode).collect();
    assert_eq!(
        opcodes,
        [
            core::wl_output::event::GEOMETRY,
            core::wl_output::event::MODE,
            core::wl_output::event::SCALE,
            core::wl_output::event::NAME,
            core::wl_output::event::DESCRIPTION,
            core::wl_output::event::DONE,
        ],
        "a client waits for `done` and reads everything before it"
    );
    // The mode is the screen: a client with none has no size to scale
    // against, and a real toolkit printed `(null): 0x0+0x0@0Hz` for one.
    let mode = &events[1].args;
    assert_eq!(mode[1], "Int(1024)");
    assert_eq!(mode[2], "Int(768)");
    assert_eq!(
        mode[3], "Int(60000)",
        "millihertz, as the protocol counts it"
    );
    assert_eq!(
        events[0].args[7],
        format!("Int({})", core::wl_output::transform::NORMAL),
    );
    assert_eq!(
        events[2].args,
        ["Int(1)"],
        "one buffer pixel per logical one"
    );
    assert_eq!(events[3].args, ["Str(Some(\"HEADLESS-1\"))"]);
}

#[test]
fn an_output_bound_at_version_one_is_not_sent_what_it_cannot_read() {
    // `scale` arrived in 2, `name` and `description` in 4, and `done` in 2.
    // A client bound at 1 that was sent them would read them with the wrong
    // signatures.
    let mut globals = Globals::new();
    assert!(globals.add(&core::WL_OUTPUT, 4, Role::Output).is_some());
    let mut client = Client::new(globals);
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_output", 1, 3));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    let opcodes: Vec<u16> = sent(&mut client)
        .into_iter()
        .filter(|event| event.sender == ObjectId(3))
        .map(|event| event.opcode)
        .collect();
    assert_eq!(
        opcodes,
        [
            core::wl_output::event::GEOMETRY,
            core::wl_output::event::MODE
        ]
    );
}

#[test]
fn a_fresh_seat_says_what_it_has_and_refuses_what_it_does_not() {
    // With nothing, a client is told nothing and may ask for nothing.
    let mut empty = full_client(0);
    let capabilities: Vec<Sent> = sent(&mut empty)
        .into_iter()
        .filter(|event| event.sender == ObjectId(7))
        .collect();
    assert_eq!(capabilities.len(), 2, "capabilities and a name");
    assert_eq!(capabilities[0].args, ["Uint(0)"]);
    assert_eq!(capabilities[1].args, ["Str(Some(\"seat0\"))"]);

    let ask =
        |opcode: u16, id: u32| request(7, opcode, &[ArgType::NewId], &[Arg::NewId(ObjectId(id))]);
    let keyboard = ask(core::wl_seat::request::GET_KEYBOARD, 10);
    let _ = empty.read(&keyboard, &[]);
    assert!(
        matches!(
            empty.fatal(),
            Some(Fatal::Interface { code, .. })
                if *code == core::wl_seat::error::MISSING_CAPABILITY
        ),
        "a seat with no keyboard handed one out: {:?}",
        empty.fatal()
    );

    // With a keyboard and a pointer, both are made, and the keyboard is given
    // its keymap before anything else.
    let both = core::wl_seat::capability::KEYBOARD | core::wl_seat::capability::POINTER;
    let mut seated = full_client(both);
    let _ = sent(&mut seated);
    let mut bytes = ask(core::wl_seat::request::GET_KEYBOARD, 10);
    bytes.extend(ask(core::wl_seat::request::GET_POINTER, 11));
    assert_eq!(seated.read(&bytes, &[]), bytes.len());
    assert_eq!(seated.fatal(), None);
    assert_eq!(
        seated.objects().get(ObjectId(10)).map(|entry| entry.data),
        Some(Role::Keyboard)
    );
    assert_eq!(
        seated.objects().get(ObjectId(11)).map(|entry| entry.data),
        Some(Role::Pointer)
    );

    // The keymap and the repeat settings, and nothing else yet: no `enter`,
    // no key, because nothing has focus and nobody has typed.
    let events = sent(&mut seated);
    assert_eq!(events.len(), 2, "the keymap and repeat_info: {events:?}");
    assert_eq!(events[0].sender, ObjectId(10));
    assert_eq!(events[0].opcode, core::wl_keyboard::event::KEYMAP);
    // With no keymap the compositor says so rather than sending a descriptor
    // that is not one.
    assert_eq!(
        events[0].args[0],
        format!("Uint({})", core::wl_keyboard::keymap_format::NO_KEYMAP)
    );
    assert_eq!(events[0].args[2], "Uint(0)");
    assert_eq!(events[1].sender, ObjectId(10));
    assert_eq!(events[1].opcode, core::wl_keyboard::event::REPEAT_INFO);
    assert_eq!(events[1].args, ["Int(25)", "Int(600)"]);

    // Touch is still refused, since the seat did not announce it.
    let touch = ask(core::wl_seat::request::GET_TOUCH, 12);
    let _ = seated.read(&touch, &[]);
    assert!(seated.is_finished());
}

/// `hyprctl keyword input:repeat_rate` and a new layout reach a keyboard
/// a client already holds: `repeat_info` and `keymap` again, on that
/// keyboard, with the new numbers.
#[test]
fn a_held_keyboard_hears_new_repeat_and_keymap() {
    let mut seated = full_client(core::wl_seat::capability::KEYBOARD);
    let _ = sent(&mut seated);
    let bytes = request(
        7,
        core::wl_seat::request::GET_KEYBOARD,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(10))],
    );
    assert_eq!(seated.read(&bytes, &[]), bytes.len());
    assert_eq!(sent(&mut seated).len(), 2, "the keymap and repeat_info");

    seated.change_repeat_info(40, 250);
    let events = sent(&mut seated);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].sender, ObjectId(10));
    assert_eq!(events[0].opcode, core::wl_keyboard::event::REPEAT_INFO);
    assert_eq!(events[0].args, ["Int(40)", "Int(250)"]);

    seated.change_keymap(None);
    let events = sent(&mut seated);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].sender, ObjectId(10));
    assert_eq!(events[0].opcode, core::wl_keyboard::event::KEYMAP);

    // A keyboard made after the change is given the new numbers.
    let bytes = request(
        7,
        core::wl_seat::request::GET_KEYBOARD,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(11))],
    );
    assert_eq!(seated.read(&bytes, &[]), bytes.len());
    let events = sent(&mut seated);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1].args, ["Int(40)", "Int(250)"]);
}

#[test]
fn a_subsurface_is_a_surface_placed_against_another() {
    let mut client = full_client(0);
    let _ = sent(&mut client);
    let mut bytes = create_surface(3);
    bytes.extend(create_surface(10));
    let get_sub = |id: u32, surface: u32, parent: u32| {
        request(
            6,
            core::wl_subcompositor::request::GET_SUBSURFACE,
            &[
                ArgType::NewId,
                ArgType::Object { nullable: false },
                ArgType::Object { nullable: false },
            ],
            &[
                Arg::NewId(ObjectId(id)),
                Arg::Object(ObjectId(surface)),
                Arg::Object(ObjectId(parent)),
            ],
        )
    };
    bytes.extend(get_sub(11, 10, 3));
    bytes.extend(request(
        11,
        core::wl_subsurface::request::SET_POSITION,
        &[ArgType::Int, ArgType::Int],
        &[Arg::Int(4), Arg::Int(-9)],
    ));
    bytes.extend(request(
        11,
        core::wl_subsurface::request::SET_DESYNC,
        &[],
        &[],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    let sub = client.subsurface(ObjectId(11)).expect("a subsurface");
    assert_eq!(sub.surface, ObjectId(10));
    assert_eq!(sub.parent, ObjectId(3));
    assert_eq!(sub.position, (4, -9));
    assert!(!sub.synchronised, "set_desync");

    // A surface may not be its own parent, nor take a second role.
    let mut same = full_client(0);
    let mut bytes = create_surface(3);
    bytes.extend(get_sub(11, 3, 3));
    let _ = same.read(&bytes, &[]);
    assert!(
        matches!(
            same.fatal(),
            Some(Fatal::Interface { code, .. })
                if *code == core::wl_subcompositor::error::BAD_SURFACE
        ),
        "{:?}",
        same.fatal()
    );

    let mut twice = full_client(0);
    let mut bytes = create_surface(3);
    bytes.extend(create_surface(10));
    bytes.extend(get_sub(11, 10, 3));
    bytes.extend(get_sub(12, 10, 3));
    let _ = twice.read(&bytes, &[]);
    assert!(twice.is_finished(), "a second role on one surface");
}

#[test]
fn the_clipboards_objects_are_made_even_though_nothing_is_ever_offered() {
    // A toolkit that does not find `wl_data_device_manager` refuses to start,
    // which is how this came to be written. The objects exist and no
    // selection is ever sent, which is what a client sees when nobody has
    // copied anything.
    let mut client = full_client(0);
    let mut bytes = request(
        9,
        core::wl_data_device_manager::request::CREATE_DATA_SOURCE,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(10))],
    );
    bytes.extend(request(
        9,
        core::wl_data_device_manager::request::GET_DATA_DEVICE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(11)), Arg::Object(ObjectId(7))],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert_eq!(
        client.objects().get(ObjectId(11)).map(|entry| entry.data),
        Some(Role::DataDevice)
    );

    // A data device asked for on something that is not a seat is refused.
    let mut wrong = full_client(0);
    let bytes = request(
        9,
        core::wl_data_device_manager::request::GET_DATA_DEVICE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(11)), Arg::Object(ObjectId(4))],
    );
    let _ = wrong.read(&bytes, &[]);
    assert_eq!(
        wrong.fatal(),
        Some(&Fatal::WrongInterface {
            object: ObjectId(4),
            wanted: "wl_seat"
        })
    );
}

/// A client with a keyboard and a pointer already made, and the events its
/// creation sent taken away.
fn seated_client() -> Client {
    let both = core::wl_seat::capability::KEYBOARD | core::wl_seat::capability::POINTER;
    let mut client = full_client(both);
    let ask =
        |opcode: u16, id: u32| request(7, opcode, &[ArgType::NewId], &[Arg::NewId(ObjectId(id))]);
    let mut bytes = ask(core::wl_seat::request::GET_KEYBOARD, 10);
    bytes.extend(ask(core::wl_seat::request::GET_POINTER, 11));
    // The surface the focus goes to: an event may name only a surface the
    // client still has.
    bytes.extend(create_surface(3));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    let _ = sent(&mut client);
    client
}

/// Focus, a key, and focus taken away again: the whole of what a window is
/// typed into through.
#[test]
fn a_focused_client_is_told_the_keys_and_the_modifiers() {
    let mut client = seated_client();
    let surface = ObjectId(3);
    let modifiers = compositor_xkb::Modifiers {
        depressed: compositor_xkb::generated::SHIFT,
        locked: compositor_xkb::generated::LOCK,
        ..compositor_xkb::Modifiers::default()
    };

    // A key already held when focus arrives is in `enter`, in the order it
    // was pressed, so the client does not think it is up.
    let enter = client
        .keyboard_enter(surface, &[42, 30], modifiers)
        .expect("the client has a keyboard");
    let events = sent(&mut client);
    assert_eq!(events.len(), 2, "enter and modifiers: {events:?}");
    assert_eq!(events[0].opcode, core::wl_keyboard::event::ENTER);
    assert_eq!(events[0].sender, ObjectId(10));
    assert_eq!(
        events[0].args,
        [
            format!("Uint({enter})"),
            "Object(ObjectId(3))".to_owned(),
            // Each key is a 32-bit word, little-endian: 42 then 30.
            "Array([42, 0, 0, 0, 30, 0, 0, 0])".to_owned(),
        ]
    );
    assert_eq!(events[1].opcode, core::wl_keyboard::event::MODIFIERS);
    assert_eq!(
        events[1].args[1..],
        [
            format!("Uint({})", compositor_xkb::generated::SHIFT),
            "Uint(0)".to_owned(),
            format!("Uint({})", compositor_xkb::generated::LOCK),
            "Uint(0)".to_owned(),
        ]
    );

    // A press and a release, with the evdev code and the protocol's state.
    let press = client
        .keyboard_key(1234, 16, true)
        .expect("a keyboard to send it to");
    let release = client
        .keyboard_key(1240, 16, false)
        .expect("a keyboard to send it to");
    assert_ne!(press, release, "each event has a serial of its own");
    let events = sent(&mut client);
    assert_eq!(
        events[0].args,
        [
            format!("Uint({press})"),
            "Uint(1234)".to_owned(),
            "Uint(16)".to_owned(),
            format!("Uint({})", core::wl_keyboard::key_state::PRESSED),
        ]
    );
    assert_eq!(
        events[1].args[3],
        format!("Uint({})", core::wl_keyboard::key_state::RELEASED)
    );

    let leave = client.keyboard_leave(surface).expect("a keyboard");
    let events = sent(&mut client);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].opcode, core::wl_keyboard::event::LEAVE);
    assert_eq!(
        events[0].args,
        [format!("Uint({leave})"), "Object(ObjectId(3))".to_owned()]
    );
}

#[test]
fn a_pointer_is_told_where_it_is_and_what_was_clicked() {
    let mut client = seated_client();
    let surface = ObjectId(3);
    let _ = client.keyboard_enter(surface, &[], compositor_xkb::Modifiers::default());
    let _ = sent(&mut client);

    let serial = client
        .pointer_enter(surface, Fixed::from_int(10), Fixed::from_int(20))
        .expect("the client has a pointer");
    client.pointer_motion(7, Fixed::from_int(11), Fixed::from_int(21));
    // `BTN_LEFT`, which the protocol asks for by its evdev number.
    let clicked = client
        .pointer_button(8, 0x110, true)
        .expect("a pointer to send it to");
    // One wheel click up, which the device layer reports as a whole
    // `WHEEL_STEP` of surface distance.
    client.pointer_axis(
        9,
        core::wl_pointer::axis::VERTICAL_SCROLL,
        Fixed::from_int(-15),
    );
    client.pointer_frame();
    let events = sent(&mut client);

    // This client bound `wl_seat` at 7, so it hears the scroll the way a
    // client of that version does: what kind of scroll it was, how many
    // clicks, and then the distance.
    let opcodes: Vec<u16> = events.iter().map(|event| event.opcode).collect();
    assert_eq!(
        opcodes,
        [
            core::wl_pointer::event::ENTER,
            core::wl_pointer::event::MOTION,
            core::wl_pointer::event::BUTTON,
            core::wl_pointer::event::AXIS_SOURCE,
            core::wl_pointer::event::AXIS_DISCRETE,
            core::wl_pointer::event::AXIS,
            core::wl_pointer::event::FRAME,
        ]
    );
    assert_eq!(
        events[3].args,
        [format!("Uint({})", core::wl_pointer::axis_source::WHEEL)]
    );
    assert_eq!(
        events[4].args,
        [
            format!("Uint({})", core::wl_pointer::axis::VERTICAL_SCROLL),
            "Int(-1)".to_owned(),
        ],
        "one click, in the wheel's own units"
    );
    assert!(events.iter().all(|event| event.sender == ObjectId(11)));
    assert_eq!(
        events[0].args,
        [
            format!("Uint({serial})"),
            "Object(ObjectId(3))".to_owned(),
            "Fixed(10)".to_owned(),
            "Fixed(20)".to_owned(),
        ]
    );
    assert_eq!(
        events[2].args,
        [
            format!("Uint({clicked})"),
            "Uint(8)".to_owned(),
            "Uint(272)".to_owned(),
            format!("Uint({})", core::wl_pointer::button_state::PRESSED),
        ]
    );

    let left = client.pointer_leave(surface).expect("a pointer");
    let events = sent(&mut client);
    assert_eq!(events[0].opcode, core::wl_pointer::event::LEAVE);
    assert_eq!(events[0].args[0], format!("Uint({left})"));
}

/// A client on a modern `wl_seat` hears a scroll the modern way.
///
/// `axis_discrete` was deprecated at version 8 and `axis_value120` put in
/// its place, and version 9 added `axis_relative_direction`. A client that
/// bound 9 and heard `axis_discrete` would scroll by nothing, because it
/// has stopped listening for that event; one that heard neither would
/// scroll smoothly where a wheel should click. So exactly which of the two
/// goes out is the whole of whether a wheel works, and it is decided per
/// bound pointer rather than per compositor.
#[test]
fn a_modern_pointer_hears_the_scroll_in_value120() {
    let mut globals = Globals::new();
    for (interface, version, role) in [
        (&core::WL_COMPOSITOR, 6, Role::Compositor),
        (&core::WL_SEAT, 9, Role::Seat),
    ] {
        assert!(globals.add(interface, version, role).is_some());
    }
    let mut client = Client::new(globals);
    client.set_seat_capabilities(core::wl_seat::capability::POINTER);
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_compositor", 6, 4));
    bytes.extend(bind(2, 2, "wl_seat", 9, 7));
    bytes.extend(request(
        7,
        core::wl_seat::request::GET_POINTER,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(11))],
    ));
    bytes.extend(create_surface(3));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    let _ = sent(&mut client);

    let _ = client.pointer_enter(ObjectId(3), Fixed::from_int(1), Fixed::from_int(1));
    let _ = sent(&mut client);
    // Two clicks down, which is two whole `WHEEL_STEP`s of distance.
    client.pointer_axis(
        9,
        core::wl_pointer::axis::VERTICAL_SCROLL,
        Fixed::from_int(30),
    );
    let events = sent(&mut client);
    let opcodes: Vec<u16> = events.iter().map(|event| event.opcode).collect();
    assert_eq!(
        opcodes,
        [
            core::wl_pointer::event::AXIS_SOURCE,
            core::wl_pointer::event::AXIS_RELATIVE_DIRECTION,
            core::wl_pointer::event::AXIS_VALUE120,
            core::wl_pointer::event::AXIS,
        ],
        "no `axis_discrete`, which a client on 9 has stopped listening for"
    );
    assert_eq!(
        events[1].args,
        [
            format!("Uint({})", core::wl_pointer::axis::VERTICAL_SCROLL),
            format!(
                "Uint({})",
                core::wl_pointer::axis_relative_direction::IDENTICAL
            ),
        ]
    );
    assert_eq!(
        events[2].args,
        [
            format!("Uint({})", core::wl_pointer::axis::VERTICAL_SCROLL),
            "Int(240)".to_owned(),
        ],
        "two clicks, in the protocol\'s hundred-and-twentieths"
    );
    assert_eq!(
        events[3].args,
        [
            "Uint(9)".to_owned(),
            format!("Uint({})", core::wl_pointer::axis::VERTICAL_SCROLL),
            "Fixed(30)".to_owned(),
        ]
    );
}

/// A surface the client has destroyed is never named in `enter` or `leave`.
///
/// The client has been sent `delete_id` for it, and libwayland treats an
/// event naming an id it has let go of as an unknown object and ends the
/// connection. Chrome closed a context menu under the pointer, the pointer
/// moved off where the menu had been, and the `wl_pointer.leave` for the
/// menu took the whole browser down.
#[test]
fn a_destroyed_surface_is_not_told_the_focus_left() {
    let mut client = seated_client();
    let surface = ObjectId(3);
    let _ = client
        .pointer_enter(surface, Fixed::from_int(1), Fixed::from_int(1))
        .expect("a pointer");
    let _ = client
        .keyboard_enter(surface, &[], compositor_xkb::Modifiers::default())
        .expect("a keyboard");
    let destroy = request(3, core::wl_surface::request::DESTROY, &[], &[]);
    assert_eq!(client.read(&destroy, &[]), destroy.len());
    assert_eq!(client.fatal(), None);
    let _ = sent(&mut client);

    assert_eq!(client.pointer_leave(surface), None);
    assert_eq!(client.keyboard_leave(surface), None);
    assert_eq!(
        client.pointer_enter(surface, Fixed::ZERO, Fixed::ZERO),
        None
    );
    assert_eq!(
        client.keyboard_enter(surface, &[], compositor_xkb::Modifiers::default()),
        None
    );
    assert!(sent(&mut client).is_empty());
}

/// A client that never asked for a keyboard is sent no key, and is told so.
///
/// The compositor sends to the focused window without asking whether it
/// wanted one, so this is what keeps a `wl_surface` from being handed an
/// opcode it has no event for. Saying so matters as much as not sending: a
/// window is usually mapped in the same burst that asks the seat for its
/// keyboard, and a compositor that took the silent `enter` for a delivered
/// one would leave that window unable to be typed into.
#[test]
fn a_client_with_no_keyboard_is_sent_nothing_and_told_so() {
    let mut client = full_client(core::wl_seat::capability::KEYBOARD);
    let _ = sent(&mut client);
    assert_eq!(
        client.keyboard_enter(ObjectId(3), &[30], compositor_xkb::Modifiers::default()),
        None
    );
    assert_eq!(client.keyboard_key(1, 30, true), None);
    assert_eq!(client.keyboard_leave(ObjectId(3)), None);
    assert_eq!(
        client.pointer_enter(ObjectId(3), Fixed::ZERO, Fixed::ZERO),
        None
    );
    assert_eq!(client.pointer_button(1, 272, true), None);
    client.pointer_motion(1, Fixed::from_int(1), Fixed::from_int(1));
    client.pointer_frame();
    assert!(sent(&mut client).is_empty());
}

// ---------------------------------------------------------------------------
// zwlr_layer_shell_v1
//
// A bar, a wallpaper and a launcher are not windows. Without this protocol a
// Hyprland user's setup -- waybar, hyprpaper, mako, wofi -- does not start,
// so what matters is that the conversation is the one those programs have.
// ---------------------------------------------------------------------------

/// A client with `zwlr_layer_shell_v1` bound at 12 and a surface at 3.
fn layer_client() -> Client {
    let mut globals = Globals::new();
    for (interface, version, role) in [
        (&core::WL_COMPOSITOR, 6, Role::Compositor),
        (&core::WL_SHM, 1, Role::Shm),
        (&xdg_shell::XDG_WM_BASE, 6, Role::XdgWmBase),
        (
            &compositor_protocol::layer_shell::ZWLR_LAYER_SHELL_V1,
            5,
            Role::LayerShell,
        ),
    ] {
        assert!(globals.add(interface, version, role).is_some());
    }
    let mut client = Client::new(globals);
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_compositor", 6, 4));
    bytes.extend(bind(2, 2, "wl_shm", 1, 5));
    bytes.extend(bind(2, 3, "xdg_wm_base", 6, 6));
    bytes.extend(bind(2, 4, "zwlr_layer_shell_v1", 5, 12));
    bytes.extend(create_surface(3));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    let _ = sent(&mut client);
    client
}

/// `zwlr_layer_shell_v1.get_layer_surface(id, surface, output, layer, ns)`.
fn get_layer_surface(id: u32, surface: u32, layer: u32, namespace: &str) -> Vec<u8> {
    request(
        12,
        compositor_protocol::layer_shell::zwlr_layer_shell_v1::request::GET_LAYER_SURFACE,
        &[
            ArgType::NewId,
            ArgType::Object { nullable: false },
            ArgType::Object { nullable: true },
            ArgType::Uint,
            ArgType::Str { nullable: false },
        ],
        &[
            Arg::NewId(ObjectId(id)),
            Arg::Object(ObjectId(surface)),
            Arg::Object(ObjectId(0)),
            Arg::Uint(layer),
            Arg::Str(Some(namespace)),
        ],
    )
}

/// A bar's tooltip is an `xdg_popup` made with no parent and handed to the
/// bar's layer surface with `zwlr_layer_surface_v1.get_popup`, which is how
/// GTK's layer-shell, and so waybar, makes every tooltip and menu. Refusing
/// the null parent was a protocol error the moment a pointer rested on a
/// module.
#[test]
fn a_bars_tooltip_is_a_popup_its_layer_surface_takes() {
    use compositor_protocol::layer_shell::zwlr_layer_surface_v1 as layer;

    let mut client = layer_client();
    let mut bytes = get_layer_surface(13, 3, 2, "waybar");
    bytes.extend(create_surface(20));
    bytes.extend(request(
        6,
        xdg_shell::xdg_wm_base::request::GET_XDG_SURFACE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(21)), Arg::Object(ObjectId(20))],
    ));
    bytes.extend(request(
        6,
        xdg_shell::xdg_wm_base::request::CREATE_POSITIONER,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(22))],
    ));
    bytes.extend(request(
        22,
        xdg_shell::xdg_positioner::request::SET_SIZE,
        &[ArgType::Int, ArgType::Int],
        &[Arg::Int(100), Arg::Int(30)],
    ));
    bytes.extend(request(
        22,
        xdg_shell::xdg_positioner::request::SET_ANCHOR_RECT,
        &[ArgType::Int, ArgType::Int, ArgType::Int, ArgType::Int],
        &[Arg::Int(100), Arg::Int(0), Arg::Int(50), Arg::Int(40)],
    ));
    bytes.extend(request(
        21,
        xdg_shell::xdg_surface::request::GET_POPUP,
        &[
            ArgType::NewId,
            ArgType::Object { nullable: true },
            ArgType::Object { nullable: false },
        ],
        &[
            Arg::NewId(ObjectId(23)),
            Arg::Object(ObjectId(0)),
            Arg::Object(ObjectId(22)),
        ],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None, "a null parent is not an error");
    let _ = client.take_events();
    assert_eq!(
        client.popup(ObjectId(23)).map(|popup| popup.layer_parent),
        Some(None),
        "kept, and not placed before a layer surface takes it"
    );

    let take = request(
        13,
        layer::request::GET_POPUP,
        &[ArgType::Object { nullable: false }],
        &[Arg::Object(ObjectId(23))],
    );
    assert_eq!(client.read(&take, &[]), take.len());
    assert_eq!(client.fatal(), None);
    assert_eq!(
        client
            .popup(ObjectId(23))
            .and_then(|popup| popup.layer_parent),
        Some(ObjectId(13))
    );
    assert!(
        client.take_events().iter().any(
            |event| matches!(event, Event::PopupCreated { popup, .. } if *popup == ObjectId(23))
        ),
        "the compositor is told to place it now"
    );

    // A second parent is refused, as wlroots refuses it.
    assert_eq!(client.read(&take, &[]), take.len());
    assert!(client.fatal().is_some());
}

#[test]
fn a_bar_says_what_it_is_and_is_told_what_size_to_be() {
    use compositor_protocol::layer_shell::zwlr_layer_surface_v1 as layer;

    let mut client = layer_client();
    let mut bytes = get_layer_surface(13, 3, 2, "waybar");
    // Anchored across the top, thirty pixels tall, reserving all thirty.
    bytes.extend(request(
        13,
        layer::request::SET_ANCHOR,
        &[ArgType::Uint],
        &[Arg::Uint(
            layer::anchor::TOP | layer::anchor::LEFT | layer::anchor::RIGHT,
        )],
    ));
    bytes.extend(request(
        13,
        layer::request::SET_SIZE,
        &[ArgType::Uint, ArgType::Uint],
        &[Arg::Uint(0), Arg::Uint(30)],
    ));
    bytes.extend(request(
        13,
        layer::request::SET_EXCLUSIVE_ZONE,
        &[ArgType::Int],
        &[Arg::Int(30)],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    let surface = client
        .layer_surface(ObjectId(13))
        .expect("the layer surface");
    assert_eq!(surface.namespace, "waybar");
    assert_eq!(surface.layer, crate::Layer::Top);
    assert_eq!(surface.size, (0, 30));
    assert_eq!(surface.exclusive_zone, 30);
    assert_eq!(surface.output, None, "a null output is `you choose`");

    // The compositor was told it has something to place.
    let events = client.take_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::LayerSurfaceCreated {
                layer_surface,
                surface
            } if *layer_surface == ObjectId(13) && *surface == ObjectId(3)
        )),
        "{events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::LayerSurfaceChanged { .. }))
            .count(),
        3,
        "one for each thing it said"
    );

    // And configuring it sends the size with a serial the client acks.
    client.configure_layer(ObjectId(13), 1024, 30);
    let events = sent(&mut client);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].opcode, layer::event::CONFIGURE);
    let serial: u32 = events[0].args[0]
        .trim_start_matches("Uint(")
        .trim_end_matches(')')
        .parse()
        .expect("a serial");
    assert_eq!(events[0].args[1..], ["Uint(1024)", "Uint(30)"]);

    let ack = request(
        13,
        layer::request::ACK_CONFIGURE,
        &[ArgType::Uint],
        &[Arg::Uint(serial)],
    );
    assert_eq!(client.read(&ack, &[]), ack.len());
    assert_eq!(client.fatal(), None);
    assert!(
        client
            .layer_surface(ObjectId(13))
            .is_some_and(|surface| surface.committed)
    );
}

/// The protocol's own rule, and the one that catches a bar that forgot
/// `set_size`: an axis the surface is not anchored to both edges of must
/// have a size, because the compositor has nothing else to go on.
#[test]
fn a_surface_with_no_size_on_a_free_axis_is_refused() {
    use compositor_protocol::layer_shell::zwlr_layer_surface_v1 as layer;

    let mut client = layer_client();
    let mut bytes = get_layer_surface(13, 3, 2, "forgetful");
    bytes.extend(request(
        13,
        layer::request::SET_ANCHOR,
        &[ArgType::Uint],
        &[Arg::Uint(layer::anchor::TOP | layer::anchor::LEFT)],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    client.configure_layer(ObjectId(13), 1024, 0);
    assert!(
        matches!(
            client.fatal(),
            Some(Fatal::Interface { code, .. }) if *code == layer::error::INVALID_SIZE
        ),
        "{:?}",
        client.fatal()
    );
}

#[test]
fn a_layer_that_is_not_one_of_the_four_is_refused() {
    let mut client = layer_client();
    let bytes = get_layer_surface(13, 3, 4, "nowhere");
    let _ = client.read(&bytes, &[]);
    assert!(
        matches!(
            client.fatal(),
            Some(Fatal::Interface { code, .. })
                if *code == compositor_protocol::layer_shell::zwlr_layer_shell_v1::error::INVALID_LAYER
        ),
        "{:?}",
        client.fatal()
    );
}

/// A surface may be given one role and no other, whichever protocol gives
/// it.
#[test]
fn a_surface_that_is_already_a_window_cannot_also_be_a_bar() {
    let mut client = layer_client();
    let mut bytes = request(
        6,
        xdg_shell::xdg_wm_base::request::GET_XDG_SURFACE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(8)), Arg::Object(ObjectId(3))],
    );
    bytes.extend(get_layer_surface(13, 3, 2, "greedy"));
    let _ = client.read(&bytes, &[]);
    assert!(
        matches!(
            client.fatal(),
            Some(Fatal::Interface { code, .. })
                if *code == compositor_protocol::layer_shell::zwlr_layer_shell_v1::error::ROLE
        ),
        "{:?}",
        client.fatal()
    );
}

/// An anchor with a bit the protocol does not define is `invalid_anchor`,
/// rather than a surface placed by a bit nobody agreed on.
#[test]
fn an_anchor_with_an_undefined_bit_is_refused() {
    use compositor_protocol::layer_shell::zwlr_layer_surface_v1 as layer;

    let mut client = layer_client();
    let mut bytes = get_layer_surface(13, 3, 2, "bits");
    bytes.extend(request(
        13,
        layer::request::SET_ANCHOR,
        &[ArgType::Uint],
        &[Arg::Uint(0x10)],
    ));
    let _ = client.read(&bytes, &[]);
    assert!(
        matches!(
            client.fatal(),
            Some(Fatal::Interface { code, .. }) if *code == layer::error::INVALID_ANCHOR
        ),
        "{:?}",
        client.fatal()
    );
}

/// `closed` is final: the surface is gone from the compositor's side, and a
/// second close says nothing.
#[test]
fn closing_a_layer_surface_says_so_once() {
    use compositor_protocol::layer_shell::zwlr_layer_surface_v1 as layer;

    let mut client = layer_client();
    let bytes = get_layer_surface(13, 3, 0, "wallpaper");
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    let _ = sent(&mut client);

    client.close_layer(ObjectId(13));
    let events = sent(&mut client);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].opcode, layer::event::CLOSED);
    assert!(client.layer_surface(ObjectId(13)).is_none());

    client.close_layer(ObjectId(13));
    assert!(sent(&mut client).is_empty());
}

/// A toolkit that asks who draws the title bar is told the compositor does.
///
/// `zxdg_decoration_manager_v1` has to be *offered* for that to happen at
/// all: a toolkit that does not find it assumes the job is its own and
/// draws a title bar, a shadow and a resize border inside the rectangle the
/// tiling gave it. GTK and Qt both do.
#[test]
fn a_window_that_asks_about_its_decorations_is_told_the_compositor_draws_them() {
    use compositor_protocol::xdg_decoration::{
        zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
    };

    let mut globals = Globals::new();
    for (interface, version, role) in [
        (&core::WL_COMPOSITOR, 6, Role::Compositor),
        (&xdg_shell::XDG_WM_BASE, 6, Role::XdgWmBase),
        (
            &compositor_protocol::xdg_decoration::ZXDG_DECORATION_MANAGER_V1,
            1,
            Role::DecorationManager,
        ),
    ] {
        assert!(globals.add(interface, version, role).is_some());
    }
    let mut client = Client::new(globals);
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_compositor", 6, 4));
    bytes.extend(bind(2, 2, "xdg_wm_base", 6, 5));
    bytes.extend(bind(2, 3, "zxdg_decoration_manager_v1", 1, 6));
    bytes.extend(create_surface(3));
    bytes.extend(request(
        5,
        xdg_shell::xdg_wm_base::request::GET_XDG_SURFACE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(7)), Arg::Object(ObjectId(3))],
    ));
    bytes.extend(request(
        7,
        xdg_shell::xdg_surface::request::GET_TOPLEVEL,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(8))],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    let _ = sent(&mut client);

    // The decoration is configured the moment it is made, which is what the
    // protocol allows and what saves a round trip before the first frame.
    let bytes = request(
        6,
        zxdg_decoration_manager_v1::request::GET_TOPLEVEL_DECORATION,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(9)), Arg::Object(ObjectId(8))],
    );
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    let events = sent(&mut client);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].sender, ObjectId(9));
    assert_eq!(
        events[0].opcode,
        zxdg_toplevel_decoration_v1::event::CONFIGURE
    );
    assert_eq!(
        events[0].args.first().map(String::as_str),
        Some(format!("Uint({})", zxdg_toplevel_decoration_v1::mode::SERVER_SIDE).as_str())
    );

    // And a client that asks for client-side decorations is told the same:
    // the answer does not depend on what was asked, and a client that
    // believed otherwise would draw a title bar.
    let bytes = request(
        9,
        zxdg_toplevel_decoration_v1::request::SET_MODE,
        &[ArgType::Uint],
        &[Arg::Uint(zxdg_toplevel_decoration_v1::mode::CLIENT_SIDE)],
    );
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    let events = sent(&mut client);
    assert_eq!(
        events
            .first()
            .and_then(|event| event.args.first())
            .map(String::as_str),
        Some(format!("Uint({})", zxdg_toplevel_decoration_v1::mode::SERVER_SIDE).as_str()),
        "{events:?}"
    );

    assert!(client.wants_server_frame(ObjectId(8)));

    // Where the compositor honours a client's own frame (a floating desktop
    // that draws title bars), the client that asks to draw it is told yes,
    // and gets no title bar from the compositor; asking for the
    // compositor's again gives it back.
    client.set_honour_client_frames(true);
    let mode_is = |client: &mut Client, mode: u32| {
        let bytes = request(
            9,
            zxdg_toplevel_decoration_v1::request::SET_MODE,
            &[ArgType::Uint],
            &[Arg::Uint(mode)],
        );
        assert_eq!(client.read(&bytes, &[]), bytes.len());
        sent(client)
            .first()
            .and_then(|event| event.args.first().cloned())
    };
    assert_eq!(
        mode_is(&mut client, zxdg_toplevel_decoration_v1::mode::CLIENT_SIDE),
        Some(format!(
            "Uint({})",
            zxdg_toplevel_decoration_v1::mode::CLIENT_SIDE
        ))
    );
    assert!(
        !client.wants_server_frame(ObjectId(8)),
        "no second title bar"
    );
    assert_eq!(
        mode_is(&mut client, zxdg_toplevel_decoration_v1::mode::SERVER_SIDE),
        Some(format!(
            "Uint({})",
            zxdg_toplevel_decoration_v1::mode::SERVER_SIDE
        ))
    );
    assert!(client.wants_server_frame(ObjectId(8)));

    // A window rule (`title_bar`) decides over what the client asked, and
    // the decoration is told at once: the compositor's frame for a client
    // that asked for its own, its own for one that asked for the
    // compositor's, and the client's wish again once the rule is gone.
    let _ = mode_is(&mut client, zxdg_toplevel_decoration_v1::mode::CLIENT_SIDE);
    client.force_frame(ObjectId(8), Some(false));
    let told = |client: &mut Client| {
        sent(client)
            .first()
            .and_then(|event| event.args.first().cloned())
    };
    assert_eq!(
        told(&mut client),
        Some(format!(
            "Uint({})",
            zxdg_toplevel_decoration_v1::mode::SERVER_SIDE
        ))
    );
    assert_eq!(client.frame_wish(ObjectId(8)), crate::FrameWish::Server);
    assert_eq!(
        mode_is(&mut client, zxdg_toplevel_decoration_v1::mode::CLIENT_SIDE),
        Some(format!(
            "Uint({})",
            zxdg_toplevel_decoration_v1::mode::SERVER_SIDE
        )),
        "the rule holds through a later set_mode"
    );
    client.force_frame(ObjectId(8), Some(true));
    assert_eq!(
        told(&mut client),
        Some(format!(
            "Uint({})",
            zxdg_toplevel_decoration_v1::mode::CLIENT_SIDE
        ))
    );
    assert_eq!(client.frame_wish(ObjectId(8)), crate::FrameWish::Client);
    client.force_frame(ObjectId(8), None);
    assert_eq!(
        told(&mut client),
        Some(format!(
            "Uint({})",
            zxdg_toplevel_decoration_v1::mode::CLIENT_SIDE
        )),
        "the client asked for its own"
    );
    let _ = mode_is(&mut client, zxdg_toplevel_decoration_v1::mode::SERVER_SIDE);

    // A mode that is not one of the two is the interface's own error.
    let bytes = request(
        9,
        zxdg_toplevel_decoration_v1::request::SET_MODE,
        &[ArgType::Uint],
        &[Arg::Uint(77)],
    );
    let _ = client.read(&bytes, &[]);
    assert!(
        matches!(client.fatal(), Some(Fatal::Interface { code, .. })
            if *code == zxdg_toplevel_decoration_v1::error::INVALID_MODE),
        "{:?}",
        client.fatal()
    );
}

// -- The protocols a desktop session asks for ---------------------------------

/// A connection with every one of those globals bound, so that binding each
/// is a test of the routing as well as of the handler.
///
/// `wl_compositor` is object 4 and `wl_output` object 5, as in
/// `drawing_client`; the managers are 6 upwards in the order below, and a
/// test's own objects start at 20.
fn desktop_client() -> Client {
    let mut globals = globals();
    for (interface, version, role) in [
        (&core::WL_OUTPUT, 4, Role::Output),
        (
            &compositor_protocol::xdg_output::ZXDG_OUTPUT_MANAGER_V1,
            3,
            Role::XdgOutputManager,
        ),
        (
            &compositor_protocol::presentation::WP_PRESENTATION,
            2,
            Role::Presentation,
        ),
        (
            &compositor_protocol::idle_notify::EXT_IDLE_NOTIFIER_V1,
            2,
            Role::IdleNotifier,
        ),
        (
            &compositor_protocol::idle_inhibit::ZWP_IDLE_INHIBIT_MANAGER_V1,
            1,
            Role::IdleInhibitManager,
        ),
        (
            &compositor_protocol::single_pixel::WP_SINGLE_PIXEL_BUFFER_MANAGER_V1,
            1,
            Role::SinglePixelManager,
        ),
        (
            &compositor_protocol::alpha_modifier::WP_ALPHA_MODIFIER_V1,
            1,
            Role::AlphaModifier,
        ),
        (
            &compositor_protocol::kde_decoration::ORG_KDE_KWIN_SERVER_DECORATION_MANAGER,
            1,
            Role::KdeDecorationManager,
        ),
    ] {
        assert!(globals.add(interface, version, role).is_some());
    }
    let mut client = Client::new(globals);
    client.set_outputs(vec![crate::Output {
        x: 100,
        y: 0,
        width: 2560,
        height: 1440,
        refresh: 60_000,
        scale: 2,
        transform: 0,
        name: "DP-3".to_owned(),
        description: "Dell Inc. DELL P2418D MY3ND91J09CT (DP-3)".to_owned(),
        modes: Vec::new(),
    }]);
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_compositor", 6, 4));
    bytes.extend(bind(2, 5, "wl_output", 4, 5));
    for (name, interface, version, id) in [
        (6u32, "zxdg_output_manager_v1", 3u32, 6u32),
        (7, "wp_presentation", 2, 7),
        (8, "ext_idle_notifier_v1", 2, 8),
        (9, "zwp_idle_inhibit_manager_v1", 1, 9),
        (10, "wp_single_pixel_buffer_manager_v1", 1, 10),
        (11, "wp_alpha_modifier_v1", 1, 11),
        (12, "org_kde_kwin_server_decoration_manager", 1, 12),
    ] {
        bytes.extend(bind(2, name, interface, version, id));
    }
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(!client.is_finished(), "{:?}", client.fatal());
    let _ = client.take_outgoing();
    let _ = client.take_events();
    client
}

/// `zxdg_output_manager_v1` tells a bar the screen's *logical* size, which
/// on a scaled monitor is not the mode.
#[test]
fn xdg_output_says_the_logical_size_and_the_name() {
    let mut client = desktop_client();
    let bytes = request(
        6,
        compositor_protocol::xdg_output::zxdg_output_manager_v1::request::GET_XDG_OUTPUT,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(20)), Arg::Object(ObjectId(5))],
    );
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    let events = sent(&mut client);
    let args: Vec<Vec<String>> = events
        .iter()
        .filter(|event| event.sender == ObjectId(20))
        .map(|event| event.args.clone())
        .collect();
    assert_eq!(
        args,
        [
            vec!["Int(100)".to_owned(), "Int(0)".to_owned()],
            // 2560x1440 at scale 2 is 1280x720 of the logical pixels every
            // window's rectangle is in.
            vec!["Int(1280)".to_owned(), "Int(720)".to_owned()],
            vec!["Str(Some(\"DP-3\"))".to_owned()],
            vec!["Str(Some(\"Dell Inc. DELL P2418D MY3ND91J09CT (DP-3)\"))".to_owned()],
            vec![],
        ],
        "position, size, name, description, done"
    );
}

/// A monitor stood on its edge, `monitor = ..., transform, 1`: the mode is
/// still the connector's wide one, `wl_output.geometry` says how it is
/// turned, and the logical size a bar lays itself out in is the tall one.
///
/// That is what Hyprland tells a client: `wl_output.mode` is `m_pixelSize`,
/// the geometry's transform is `m_transform`, and `zxdg_output_v1` gives
/// `m_size`, which is the transformed size divided by the scale.
#[test]
fn a_turned_output_says_so_and_is_laid_out_tall() {
    let mut client = desktop_client();
    client.set_outputs(vec![crate::Output {
        x: 100,
        y: 0,
        width: 2560,
        height: 1440,
        refresh: 60_000,
        scale: 2,
        transform: core::wl_output::transform::N90.cast_signed(),
        name: "DP-3".to_owned(),
        description: "Dell Inc. DELL P2418D MY3ND91J09CT (DP-3)".to_owned(),
        modes: Vec::new(),
    }]);
    // A second `wl_output`, bound now, so that what it is told is the
    // turned monitor's.
    let mut bytes = bind(2, 5, "wl_output", 4, 21);
    bytes.extend(request(
        6,
        compositor_protocol::xdg_output::zxdg_output_manager_v1::request::GET_XDG_OUTPUT,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(20)), Arg::Object(ObjectId(21))],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    let events = sent(&mut client);
    let output: Vec<&Sent> = events
        .iter()
        .filter(|event| event.sender == ObjectId(21))
        .collect();
    assert_eq!(output[0].opcode, core::wl_output::event::GEOMETRY);
    assert_eq!(output[0].args[7], "Int(1)", "the transform, 90");
    assert_eq!(output[1].opcode, core::wl_output::event::MODE);
    assert_eq!(
        (output[1].args[1].as_str(), output[1].args[2].as_str()),
        ("Int(2560)", "Int(1440)"),
        "the mode is the connector's, not turned"
    );
    let size = events
        .iter()
        .find(|event| {
            event.sender == ObjectId(20)
                && event.opcode
                    == compositor_protocol::xdg_output::zxdg_output_v1::event::LOGICAL_SIZE
        })
        .expect("a logical size");
    // 2560x1440 turned is 1440x2560, and at scale 2 that is 720x1280.
    assert_eq!(size.args, ["Int(720)", "Int(1280)"]);
}

/// `wp_presentation` answers a feedback when the frame is presented, and
/// the object is gone afterwards: the protocol gives it no destroy request.
#[test]
fn presentation_feedback_is_answered_once_and_then_gone() {
    let mut client = desktop_client();
    let mut bytes = create_surface(20);
    bytes.extend(request(
        7,
        compositor_protocol::presentation::wp_presentation::request::FEEDBACK,
        &[ArgType::Object { nullable: false }, ArgType::NewId],
        &[Arg::Object(ObjectId(20)), Arg::NewId(ObjectId(21))],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    let _ = sent(&mut client);

    client.presented(ObjectId(20), (7, 500), 16_666_666, 42);
    let events = sent_knowing(
        &mut client,
        &[(
            ObjectId(21),
            &compositor_protocol::presentation::WP_PRESENTATION_FEEDBACK,
        )],
    );
    let answer = events
        .iter()
        .find(|event| event.sender == ObjectId(21))
        .expect("the feedback was answered");
    assert_eq!(
        answer.args,
        [
            "Uint(0)",
            "Uint(7)",
            "Uint(500)",
            "Uint(16666666)",
            "Uint(0)",
            "Uint(42)",
            "Uint(1)"
        ],
        "the seconds split in two, the nanoseconds, the refresh, the count, and vsync"
    );
    assert!(
        client.objects().get(ObjectId(21)).is_none(),
        "and it is gone"
    );

    // A second frame owes nothing: the feedback was for one frame.
    client.presented(ObjectId(20), (8, 0), 16_666_666, 43);
    assert_eq!(sent(&mut client), []);
}

/// `ext-idle-notify` says `idled` once the timeout has passed and
/// `resumed` when it has not; an inhibitor on a surface showing nothing
/// holds nothing off.
#[test]
fn idle_notifications_fire_once_and_an_inhibitor_needs_a_mapped_surface() {
    let mut client = desktop_client();
    let mut bytes = create_surface(20);
    bytes.extend(request(
        8,
        compositor_protocol::idle_notify::ext_idle_notifier_v1::request::GET_IDLE_NOTIFICATION,
        &[
            ArgType::NewId,
            ArgType::Uint,
            ArgType::Object { nullable: false },
        ],
        &[
            Arg::NewId(ObjectId(21)),
            Arg::Uint(1_000),
            Arg::Object(ObjectId(3)),
        ],
    ));
    bytes.extend(request(
        9,
        compositor_protocol::idle_inhibit::zwp_idle_inhibit_manager_v1::request::CREATE_INHIBITOR,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(22)), Arg::Object(ObjectId(20))],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    let _ = sent(&mut client);

    let opcodes = |client: &mut Client| {
        sent(client)
            .iter()
            .map(|event| event.opcode)
            .collect::<Vec<u16>>()
    };
    assert_eq!(
        client.idle_wait(999, false),
        Some(Duration::from_millis(1)),
        "the event loop wakes at the notification's deadline"
    );
    assert_eq!(client.idle_wait(1_000, false), Some(Duration::ZERO));
    assert!(!client.idle_tick(999, false), "not yet");
    assert!(client.idle_tick(1_000, false), "now");
    assert_eq!(
        opcodes(&mut client),
        [compositor_protocol::idle_notify::ext_idle_notification_v1::event::IDLED]
    );
    assert!(!client.idle_tick(2_000, false), "and not said twice");
    assert!(client.idle_tick(0, false), "input brings it back");
    assert_eq!(
        opcodes(&mut client),
        [compositor_protocol::idle_notify::ext_idle_notification_v1::event::RESUMED]
    );

    // The inhibitor's surface is showing nothing, so it holds nothing off.
    assert!(!client.inhibits_idle());
    assert!(
        client.idle_tick(5_000, client.inhibits_idle()),
        "idle again"
    );
    let _ = sent(&mut client);
    // And with it held off, the notification goes back to not-idle.
    assert!(client.idle_tick(5_000, true));
    assert_eq!(
        client.idle_wait(5_000, true),
        None,
        "the inhibitor has no timer"
    );
    assert_eq!(
        opcodes(&mut client),
        [compositor_protocol::idle_notify::ext_idle_notification_v1::event::RESUMED]
    );
}

/// `wp_single_pixel_buffer_manager_v1` makes a `wl_buffer` that is one
/// colour and has no pool.
#[test]
fn a_single_pixel_buffer_is_one_colour_and_no_pool() {
    let mut client = desktop_client();
    let bytes = request(
        10,
        compositor_protocol::single_pixel::wp_single_pixel_buffer_manager_v1::request::CREATE_U32_RGBA_BUFFER,
        &[
            ArgType::NewId,
            ArgType::Uint,
            ArgType::Uint,
            ArgType::Uint,
            ArgType::Uint,
        ],
        &[
            Arg::NewId(ObjectId(20)),
            Arg::Uint(0x1122_3344),
            Arg::Uint(0x5566_7788),
            Arg::Uint(0x99aa_bbcc),
            Arg::Uint(u32::MAX),
        ],
    );
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    let buffer = client.buffer(ObjectId(20)).expect("the buffer was made");
    assert_eq!((buffer.width, buffer.height, buffer.stride), (1, 1, 4));
    // Little-endian ARGB: blue, green, red, alpha, each the top byte of the
    // protocol's 32-bit channel.
    assert_eq!(buffer.solid, Some([0x99, 0x55, 0x11, 0xff]));
    assert_eq!(buffer.range(), None, "it covers no part of a pool");
}

/// `wp_alpha_modifier_v1` makes a surface see-through, and destroying the
/// object puts it back.
#[test]
fn the_alpha_modifier_makes_a_surface_see_through() {
    let mut client = desktop_client();
    let mut bytes = create_surface(20);
    bytes.extend(request(
        11,
        compositor_protocol::alpha_modifier::wp_alpha_modifier_v1::request::GET_SURFACE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(21)), Arg::Object(ObjectId(20))],
    ));
    bytes.extend(request(
        21,
        compositor_protocol::alpha_modifier::wp_alpha_modifier_surface_v1::request::SET_MULTIPLIER,
        &[ArgType::Uint],
        &[Arg::Uint(u32::MAX / 2)],
    ));
    bytes.extend(commit(20));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    let alpha = client
        .surface_alpha(ObjectId(20))
        .expect("half see-through");
    assert!((alpha - 0.5).abs() < 0.001, "{alpha}");

    // Destroying it puts the surface back to opaque, on the next commit as
    // every other part of a surface's state is.
    let mut bytes = request(
        21,
        compositor_protocol::alpha_modifier::wp_alpha_modifier_surface_v1::request::DESTROY,
        &[],
        &[],
    );
    bytes.extend(commit(20));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.surface_alpha(ObjectId(20)), None);
}

/// KDE's decoration manager is answered with the server's mode, which is
/// the same answer `xdg-decoration` gets: a tiling compositor draws the
/// border, and a client that drew its own would draw a second one inside
/// it.
#[test]
fn kde_decorations_are_the_servers() {
    let mut client = desktop_client();
    let mut bytes = create_surface(20);
    bytes.extend(request(
        12,
        compositor_protocol::kde_decoration::org_kde_kwin_server_decoration_manager::request::CREATE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(21)), Arg::Object(ObjectId(20))],
    ));
    // Asking for client-side gets the same answer.
    bytes.extend(request(
        21,
        compositor_protocol::kde_decoration::org_kde_kwin_server_decoration::request::REQUEST_MODE,
        &[ArgType::Uint],
        &[Arg::Uint(
            compositor_protocol::kde_decoration::org_kde_kwin_server_decoration::mode::CLIENT,
        )],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    let told: Vec<Sent> = sent(&mut client)
        .into_iter()
        .filter(|event| event.sender == ObjectId(21))
        .collect();
    assert_eq!(told.len(), 2, "once on create and once when asked");
    for event in &told {
        assert_eq!(
            event.args,
            [format!(
                "Uint({})",
                compositor_protocol::kde_decoration::org_kde_kwin_server_decoration::mode::SERVER
            )]
        );
    }
}

// -- The pointer and keyboard protocols beyond `wl_seat` ----------------------

/// A connection with those globals bound: `wl_seat` is object 13 and its
/// `wl_pointer` object 14, the managers 15 upwards, and a test's own
/// objects start at 20.
fn input_client() -> Client {
    let mut globals = globals();
    for (interface, version, role) in [
        (
            &compositor_protocol::relative_pointer::ZWP_RELATIVE_POINTER_MANAGER_V1,
            1,
            Role::RelativePointerManager,
        ),
        (
            &compositor_protocol::pointer_constraints::ZWP_POINTER_CONSTRAINTS_V1,
            1,
            Role::PointerConstraints,
        ),
        (
            &compositor_protocol::shortcuts_inhibit::ZWP_KEYBOARD_SHORTCUTS_INHIBIT_MANAGER_V1,
            1,
            Role::ShortcutsInhibitManager,
        ),
        (
            &compositor_protocol::virtual_keyboard::ZWP_VIRTUAL_KEYBOARD_MANAGER_V1,
            1,
            Role::VirtualKeyboardManager,
        ),
        (
            &compositor_protocol::virtual_pointer::ZWLR_VIRTUAL_POINTER_MANAGER_V1,
            2,
            Role::VirtualPointerManager,
        ),
    ] {
        assert!(globals.add(interface, version, role).is_some());
    }
    let mut client = Client::new(globals);
    client.set_seat_capabilities(
        core::wl_seat::capability::POINTER | core::wl_seat::capability::KEYBOARD,
    );
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_compositor", 6, 4));
    bytes.extend(bind(2, 3, "wl_seat", 7, 13));
    bytes.extend(request(
        13,
        core::wl_seat::request::GET_POINTER,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(14))],
    ));
    for (name, interface, version, id) in [
        (5u32, "zwp_relative_pointer_manager_v1", 1u32, 15u32),
        (6, "zwp_pointer_constraints_v1", 1, 16),
        (7, "zwp_keyboard_shortcuts_inhibit_manager_v1", 1, 17),
        (8, "zwp_virtual_keyboard_manager_v1", 1, 18),
        (9, "zwlr_virtual_pointer_manager_v1", 2, 19),
    ] {
        bytes.extend(bind(2, name, interface, version, id));
    }
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(!client.is_finished(), "{:?}", client.fatal());
    let _ = client.take_outgoing();
    let _ = client.take_events();
    client
}

/// `zwp_relative_pointer_v1` carries the distance, in microseconds, with
/// the accelerated and unaccelerated movements both given.
#[test]
fn a_relative_pointer_is_told_how_far_the_pointer_moved() {
    let mut client = input_client();
    let bytes = request(
        15,
        compositor_protocol::relative_pointer::zwp_relative_pointer_manager_v1::request::GET_RELATIVE_POINTER,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(20)), Arg::Object(ObjectId(14))],
    );
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    client.relative_motion(0x1_0000_0002, -3.5, 7.0);
    let events = sent(&mut client);
    let moved = events
        .iter()
        .find(|event| event.sender == ObjectId(20))
        .expect("the movement was sent");
    assert_eq!(
        moved.args,
        [
            "Uint(1)",
            "Uint(2)",
            "Fixed(-3.5)",
            "Fixed(7)",
            "Fixed(-3.5)",
            "Fixed(7)"
        ],
        "the microseconds split in two, then the distance twice"
    );
}

/// A one-shot pointer lock is told `locked` when the compositor puts it in
/// force, `unlocked` when it takes it away, and is gone after that; a
/// persistent one stays and can come back.
#[test]
fn a_pointer_lock_is_told_when_it_is_in_force() {
    for (lifetime, survives) in [
        (
            compositor_protocol::pointer_constraints::zwp_pointer_constraints_v1::lifetime::ONESHOT,
            false,
        ),
        (
            compositor_protocol::pointer_constraints::zwp_pointer_constraints_v1::lifetime::PERSISTENT,
            true,
        ),
    ] {
        let mut client = input_client();
        let mut bytes = create_surface(20);
        bytes.extend(request(
            16,
            compositor_protocol::pointer_constraints::zwp_pointer_constraints_v1::request::LOCK_POINTER,
            &[
                ArgType::NewId,
                ArgType::Object { nullable: false },
                ArgType::Object { nullable: false },
                ArgType::Object { nullable: true },
                ArgType::Uint,
            ],
            &[
                Arg::NewId(ObjectId(21)),
                Arg::Object(ObjectId(20)),
                Arg::Object(ObjectId(14)),
                Arg::Object(ObjectId::NULL),
                Arg::Uint(lifetime),
            ],
        ));
        assert_eq!(client.read(&bytes, &[]), bytes.len());
        assert_eq!(client.fatal(), None);
        let _ = sent(&mut client);
        assert!(client.constraint_on(ObjectId(20)).is_some());

        // Told once when it comes into force, and not again.
        client.constrain(ObjectId(20), true);
        assert_eq!(
            sent(&mut client).iter().map(|e| e.opcode).collect::<Vec<u16>>(),
            [compositor_protocol::pointer_constraints::zwp_locked_pointer_v1::event::LOCKED]
        );
        client.constrain(ObjectId(20), true);
        assert_eq!(sent(&mut client), []);

        // And once when it stops. A one-shot lock is destroyed by that,
        // which is what the protocol's `oneshot` lifetime means.
        client.constrain(ObjectId(20), false);
        // A one-shot lock is destroyed by the event that ends it, so the
        // test says what the object was, as a real client's proxy knows.
        let told: Vec<u16> = sent_knowing(
            &mut client,
            &[(
                ObjectId(21),
                &compositor_protocol::pointer_constraints::ZWP_LOCKED_POINTER_V1,
            )],
        )
        .iter()
        .filter(|event| event.sender == ObjectId(21))
        .map(|event| event.opcode)
        .collect();
        assert_eq!(
            told,
            [compositor_protocol::pointer_constraints::zwp_locked_pointer_v1::event::UNLOCKED]
        );
        assert_eq!(client.constraint_on(ObjectId(20)).is_some(), survives);
    }
}

/// A shortcuts inhibitor is told `active` at once, and the compositor can
/// ask which surface it covers.
#[test]
fn a_shortcuts_inhibitor_is_active_from_the_start() {
    let mut client = input_client();
    let mut bytes = create_surface(20);
    bytes.extend(request(
        17,
        compositor_protocol::shortcuts_inhibit::zwp_keyboard_shortcuts_inhibit_manager_v1::request::INHIBIT_SHORTCUTS,
        &[
            ArgType::NewId,
            ArgType::Object { nullable: false },
            ArgType::Object { nullable: false },
        ],
        &[
            Arg::NewId(ObjectId(21)),
            Arg::Object(ObjectId(20)),
            Arg::Object(ObjectId(13)),
        ],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert_eq!(
        sent(&mut client)
            .iter()
            .filter(|event| event.sender == ObjectId(21))
            .map(|event| event.opcode)
            .collect::<Vec<u16>>(),
        [compositor_protocol::shortcuts_inhibit::zwp_keyboard_shortcuts_inhibitor_v1::event::ACTIVE]
    );
    assert!(client.inhibits_shortcuts(ObjectId(20)));
    assert!(!client.inhibits_shortcuts(ObjectId(4)));

    // Destroying it gives the keybinds back.
    let bytes = request(
        21,
        compositor_protocol::shortcuts_inhibit::zwp_keyboard_shortcuts_inhibitor_v1::request::DESTROY,
        &[],
        &[],
    );
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(!client.inhibits_shortcuts(ObjectId(20)));
}

/// A virtual keyboard's keys and a virtual pointer's movements come up as
/// input for the seat, in the order the client sent them.
#[test]
fn a_virtual_device_reports_input_for_the_seat() {
    let mut client = input_client();
    let mut bytes = request(
        18,
        compositor_protocol::virtual_keyboard::zwp_virtual_keyboard_manager_v1::request::CREATE_VIRTUAL_KEYBOARD,
        &[ArgType::Object { nullable: false }, ArgType::NewId],
        &[Arg::Object(ObjectId(13)), Arg::NewId(ObjectId(20))],
    );
    bytes.extend(request(
        19,
        compositor_protocol::virtual_pointer::zwlr_virtual_pointer_manager_v1::request::CREATE_VIRTUAL_POINTER,
        &[ArgType::Object { nullable: true }, ArgType::NewId],
        &[Arg::Object(ObjectId(13)), Arg::NewId(ObjectId(21))],
    ));
    bytes.extend(request(
        20,
        compositor_protocol::virtual_keyboard::zwp_virtual_keyboard_v1::request::KEY,
        &[ArgType::Uint, ArgType::Uint, ArgType::Uint],
        &[
            Arg::Uint(7),
            Arg::Uint(30),
            Arg::Uint(core::wl_keyboard::key_state::PRESSED),
        ],
    ));
    bytes.extend(request(
        21,
        compositor_protocol::virtual_pointer::zwlr_virtual_pointer_v1::request::MOTION,
        &[ArgType::Uint, ArgType::Fixed, ArgType::Fixed],
        &[
            Arg::Uint(7),
            Arg::Fixed(Fixed::from_f64(4.0)),
            Arg::Fixed(Fixed::from_f64(-2.0)),
        ],
    ));
    bytes.extend(request(
        21,
        compositor_protocol::virtual_pointer::zwlr_virtual_pointer_v1::request::BUTTON,
        &[ArgType::Uint, ArgType::Uint, ArgType::Uint],
        &[
            Arg::Uint(7),
            Arg::Uint(272),
            Arg::Uint(core::wl_pointer::button_state::PRESSED),
        ],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    let injected: Vec<crate::Injected> = client
        .take_events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Injected(what) => Some(what),
            _ => None,
        })
        .collect();
    assert_eq!(
        injected,
        [
            crate::Injected::Key {
                key: 30,
                pressed: true
            },
            crate::Injected::Motion {
                dx: Fixed::from_f64(4.0),
                dy: Fixed::from_f64(-2.0)
            },
            crate::Injected::Button {
                button: 272,
                pressed: true
            },
        ]
    );
}

// -- What a taskbar, a clipboard manager and a settings panel ask ------------

/// A connection with the screen and clipboard-manager globals bound:
/// `wl_seat` is 13, `wl_output` 14, the managers 15 upwards, and a test's
/// own objects start at 20.
fn watching_client() -> Client {
    let mut globals = globals();
    for (interface, version, role) in [
        (&core::WL_OUTPUT, 4, Role::Output),
        (
            &compositor_protocol::foreign_list::EXT_FOREIGN_TOPLEVEL_LIST_V1,
            1,
            Role::ForeignList,
        ),
        (
            &compositor_protocol::gamma_control::ZWLR_GAMMA_CONTROL_MANAGER_V1,
            1,
            Role::GammaControlManager,
        ),
        (
            &compositor_protocol::output_power::ZWLR_OUTPUT_POWER_MANAGER_V1,
            1,
            Role::OutputPowerManager,
        ),
        (
            &compositor_protocol::data_control::ZWLR_DATA_CONTROL_MANAGER_V1,
            2,
            Role::DataControlManager(crate::Flavour::Wlr),
        ),
        (
            &compositor_protocol::output_management::ZWLR_OUTPUT_MANAGER_V1,
            4,
            Role::OutputManager,
        ),
        (
            &compositor_protocol::ext_workspace::EXT_WORKSPACE_MANAGER_V1,
            1,
            Role::WorkspaceManager,
        ),
    ] {
        assert!(globals.add(interface, version, role).is_some());
    }
    let mut client = Client::new(globals);
    client.set_outputs(vec![crate::Output {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        refresh: 60_000,
        scale: 1,
        transform: 0,
        name: "DP-1".to_owned(),
        description: "Dell Inc. DELL U2415 XKV0P9BE2GLU (DP-1)".to_owned(),
        modes: Vec::new(),
    }]);
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_compositor", 6, 4));
    bytes.extend(bind(2, 5, "wl_output", 4, 14));
    for (name, interface, version, id) in [
        (6u32, "ext_foreign_toplevel_list_v1", 1u32, 15u32),
        (7, "zwlr_gamma_control_manager_v1", 1, 16),
        (8, "zwlr_output_power_manager_v1", 1, 17),
        (9, "zwlr_data_control_manager_v1", 2, 18),
        (11, "ext_workspace_manager_v1", 1, 19),
    ] {
        bytes.extend(bind(2, name, interface, version, id));
    }
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(!client.is_finished(), "{:?}", client.fatal());
    let _ = client.take_outgoing();
    let _ = client.take_events();
    client
}

/// The events one interface sent, which is how a test tells two events
/// that share an opcode apart.
fn sent_by(client: &mut Client, interface: &str) -> Vec<Sent> {
    let told = sent(client);
    told.into_iter()
        .filter(|event| {
            client
                .objects()
                .get(event.sender)
                .is_some_and(|entry| entry.interface.name == interface)
        })
        .collect()
}

/// One window, as a bar sees it.
fn a_window(window: u64, title: &str) -> crate::ForeignToplevel {
    crate::ForeignToplevel {
        window,
        title: title.to_owned(),
        app_id: "rocks.magical.pattern".to_owned(),
        activated: false,
        fullscreen: false,
        maximized: false,
        minimized: false,
    }
}

/// `ext-foreign-toplevel-list-v1` names each window once, says nothing when
/// nothing changed, and closes a handle whose window has gone.
#[test]
fn the_newer_window_list_names_each_window_once() {
    let mut client = watching_client();
    assert!(client.lists_toplevels());

    client.list_toplevels(&[a_window(1, "one"), a_window(2, "two")]);
    let told = sent(&mut client);
    let titles: Vec<String> = told
        .iter()
        .filter(|event| {
            event.opcode
                == compositor_protocol::foreign_list::ext_foreign_toplevel_handle_v1::event::TITLE
        })
        .filter_map(|event| event.args.first().cloned())
        .collect();
    assert_eq!(
        titles,
        [
            "Str(Some(\"one\"))".to_owned(),
            "Str(Some(\"two\"))".to_owned()
        ]
    );
    // The identifier is the compositor's own handle, in hex.
    assert!(
        told.iter().any(|event| event.args == ["Str(Some(\"2\"))"]),
        "the second window's identifier"
    );

    // Nothing changed, so nothing is said.
    client.list_toplevels(&[a_window(1, "one"), a_window(2, "two")]);
    assert_eq!(sent(&mut client), []);

    // One window gone: its handle is closed and nothing else is said.
    client.list_toplevels(&[a_window(1, "one")]);
    let told = sent(&mut client);
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(
        told.first().map(|event| event.opcode),
        Some(compositor_protocol::foreign_list::ext_foreign_toplevel_handle_v1::event::CLOSED)
    );
}

/// `zwlr_gamma_control_v1` says how big a ramp is at once -- a client that
/// was not told cannot write one -- and hands the descriptor up.
#[test]
fn a_gamma_control_is_told_its_size_and_hands_the_ramps_up() {
    let mut client = watching_client();
    let mut bytes = request(
        16,
        compositor_protocol::gamma_control::zwlr_gamma_control_manager_v1::request::GET_GAMMA_CONTROL,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(20)), Arg::Object(ObjectId(14))],
    );
    bytes.extend(request(
        20,
        compositor_protocol::gamma_control::zwlr_gamma_control_v1::request::SET_GAMMA,
        &[ArgType::Fd],
        &[Arg::Fd(Fd(7))],
    ));
    assert_eq!(client.read(&bytes, &[Fd(7)]), bytes.len());
    assert_eq!(client.fatal(), None);

    let told = sent(&mut client);
    assert_eq!(
        told.iter()
            .find(|event| event.sender == ObjectId(20))
            .map(|event| event.args.clone()),
        Some(vec![format!("Uint({})", crate::GAMMA_SIZE)])
    );
    assert!(
        client.take_events().iter().any(|event| matches!(
            event,
            Event::Gamma {
                output: 0,
                table: Some(Fd(7))
            }
        )),
        "the descriptor goes up for the compositor to read"
    );
}

/// `zwlr_output_power_v1` turns a screen off, and is told what it is now.
#[test]
fn output_power_turns_a_screen_off() {
    let mut client = watching_client();
    let mut bytes = request(
        17,
        compositor_protocol::output_power::zwlr_output_power_manager_v1::request::GET_OUTPUT_POWER,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(20)), Arg::Object(ObjectId(14))],
    );
    bytes.extend(request(
        20,
        compositor_protocol::output_power::zwlr_output_power_v1::request::SET_MODE,
        &[ArgType::Uint],
        &[Arg::Uint(
            compositor_protocol::output_power::zwlr_output_power_v1::mode::OFF,
        )],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(
        client.take_events().iter().any(|event| matches!(
            event,
            Event::OutputPower {
                output: 0,
                on: false
            }
        )),
        "the compositor is asked to turn it off"
    );

    client.output_powered(0, false);
    assert_eq!(
        sent(&mut client)
            .iter()
            .find(|event| event.sender == ObjectId(20))
            .map(|event| event.args.clone()),
        Some(vec![format!(
            "Uint({})",
            compositor_protocol::output_power::zwlr_output_power_v1::mode::OFF
        )])
    );
}

/// A clipboard manager is told what both selections hold whether or not it
/// has a window, which is the whole difference from `wl_data_device`.
#[test]
fn a_clipboard_manager_is_told_without_a_window() {
    let mut client = watching_client();
    let bytes = request(
        18,
        compositor_protocol::data_control::zwlr_data_control_manager_v1::request::GET_DATA_DEVICE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(20)), Arg::Object(ObjectId(3))],
    );
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(client.watches_clipboard());
    assert!(
        client
            .take_events()
            .iter()
            .any(|event| matches!(event, Event::DataControlBound { .. })),
        "the compositor is told to offer it both selections"
    );
    let _ = sent(&mut client);

    // This client has no surface, no keyboard and no focus, and is told.
    client.control_offer_selection(false, &["text/plain".to_owned()]);
    let told = sent(&mut client);
    let offer = told
        .iter()
        .find(|event| {
            event.sender == ObjectId(20)
                && event.opcode
                    == compositor_protocol::data_control::zwlr_data_control_device_v1::event::DATA_OFFER
        })
        .expect("an offer was made");
    assert_eq!(offer.args.len(), 1);
    assert!(
        told.iter().any(|event| {
            event.opcode
                == compositor_protocol::data_control::zwlr_data_control_device_v1::event::SELECTION
        }),
        "and named as the selection"
    );
}

/// `zwlr_output_manager_v1` publishes every screen with its mode, and
/// refuses a configuration made against a serial that is no longer current.
#[test]
fn output_management_publishes_the_screens_and_refuses_a_stale_serial() {
    let mut client = watching_client();
    let bytes = bind(2, 10, "zwlr_output_manager_v1", 4, 21);
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    let told = sent(&mut client);
    // The screen's name and its mode's size, which is what `wlr-randr`
    // prints.
    assert!(
        told.iter()
            .any(|event| event.args == ["Str(Some(\"DP-1\"))"]),
        "the screen's name: {told:?}"
    );
    assert!(
        told.iter()
            .any(|event| event.args == ["Int(1920)", "Int(1080)"]),
        "its mode's size: {told:?}"
    );
    let serial = told
        .iter()
        .rev()
        .find(|event| {
            event.sender == ObjectId(21)
                && event.opcode
                    == compositor_protocol::output_management::zwlr_output_manager_v1::event::DONE
        })
        .and_then(|event| event.args.first().cloned())
        .expect("a serial");
    assert_eq!(serial, "Uint(1)", "the first publication");

    // A configuration against a serial that is not the current one is
    // cancelled, which is this protocol's one safety rule.
    let bytes = request(
        21,
        compositor_protocol::output_management::zwlr_output_manager_v1::request::CREATE_CONFIGURATION,
        &[ArgType::NewId, ArgType::Uint],
        &[Arg::NewId(ObjectId(22)), Arg::Uint(99)],
    );
    let mut bytes = bytes;
    bytes.extend(request(
        22,
        compositor_protocol::output_management::zwlr_output_configuration_v1::request::APPLY,
        &[],
        &[],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(
        sent(&mut client)
            .iter()
            .filter(|event| event.sender == ObjectId(22))
            .map(|event| event.opcode)
            .collect::<Vec<u16>>(),
        [compositor_protocol::output_management::zwlr_output_configuration_v1::event::CANCELLED]
    );
}

/// `ext-workspace-v1` publishes one group a monitor and every workspace in
/// it, with the shown one marked.
#[test]
fn the_workspace_list_names_each_workspace_and_marks_the_shown_one() {
    let mut client = watching_client();
    assert!(client.watches_workspaces());
    client.publish_workspaces(
        1,
        &[
            crate::Workspace {
                id: 1,
                name: "1".to_owned(),
                group: 0,
                active: true,
                urgent: false,
            },
            crate::Workspace {
                id: 2,
                name: "browsing".to_owned(),
                group: 0,
                active: false,
                urgent: false,
            },
        ],
    );
    let told = sent_by(&mut client, "ext_workspace_handle_v1");
    let names: Vec<String> = told
        .iter()
        .filter(|event| {
            event.opcode == compositor_protocol::ext_workspace::ext_workspace_handle_v1::event::NAME
        })
        .filter_map(|event| event.args.first().cloned())
        .collect();
    assert_eq!(
        names,
        [
            "Str(Some(\"1\"))".to_owned(),
            "Str(Some(\"browsing\"))".to_owned()
        ]
    );
    let states: Vec<String> = told
        .iter()
        .filter(|event| {
            event.opcode
                == compositor_protocol::ext_workspace::ext_workspace_handle_v1::event::STATE
        })
        .filter_map(|event| event.args.first().cloned())
        .collect();
    assert_eq!(
        states,
        [
            format!(
                "Uint({})",
                compositor_protocol::ext_workspace::ext_workspace_handle_v1::state::ACTIVE
            ),
            "Uint(0)".to_owned()
        ],
        "the shown one is marked and the other is not"
    );
    // And nothing is said a second time.
    client.publish_workspaces(
        1,
        &[crate::Workspace {
            id: 1,
            name: "1".to_owned(),
            group: 0,
            active: true,
            urgent: false,
        }],
    );
    let told = sent(&mut client);
    assert!(
        told.iter().any(|event| {
            event.opcode
                == compositor_protocol::ext_workspace::ext_workspace_handle_v1::event::REMOVED
        }),
        "the workspace that went is removed: {told:?}"
    );
}

// -- Hyprland's own protocols ------------------------------------------------

/// A connection with Hyprland's own globals bound, at 15 upwards.
fn hypr_client() -> Client {
    let mut globals = globals();
    for (interface, version, role) in [
        (
            &compositor_protocol::global_shortcuts::HYPRLAND_GLOBAL_SHORTCUTS_MANAGER_V1,
            1,
            Role::GlobalShortcuts,
        ),
        (
            &compositor_protocol::focus_grab::HYPRLAND_FOCUS_GRAB_MANAGER_V1,
            1,
            Role::FocusGrabManager,
        ),
        (
            &compositor_protocol::lock_notify::HYPRLAND_LOCK_NOTIFIER_V1,
            1,
            Role::LockNotifier,
        ),
        (
            &compositor_protocol::hyprland_surface::HYPRLAND_SURFACE_MANAGER_V1,
            2,
            Role::HyprlandSurfaceManager,
        ),
        (
            &compositor_protocol::toplevel_export::HYPRLAND_TOPLEVEL_EXPORT_MANAGER_V1,
            2,
            Role::ToplevelExportManager,
        ),
    ] {
        assert!(globals.add(interface, version, role).is_some());
    }
    let mut client = Client::new(globals);
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_compositor", 6, 4));
    for (name, interface, version, id) in [
        (5u32, "hyprland_global_shortcuts_manager_v1", 1u32, 15u32),
        (6, "hyprland_focus_grab_manager_v1", 1, 16),
        (7, "hyprland_lock_notifier_v1", 1, 17),
        (8, "hyprland_surface_manager_v1", 2, 18),
        (9, "hyprland_toplevel_export_manager_v1", 2, 19),
    ] {
        bytes.extend(bind(2, name, interface, version, id));
    }
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(!client.is_finished(), "{:?}", client.fatal());
    let _ = client.take_outgoing();
    let _ = client.take_events();
    client
}

/// A global shortcut is a *name* a program registers, fired by
/// `dispatch global <app_id>:<id>` and not by reading the keyboard.
#[test]
fn a_global_shortcut_is_fired_by_its_name() {
    let mut client = hypr_client();
    let bytes = request(
        15,
        compositor_protocol::global_shortcuts::hyprland_global_shortcuts_manager_v1::request::REGISTER_SHORTCUT,
        &[
            ArgType::NewId,
            ArgType::Str { nullable: false },
            ArgType::Str { nullable: false },
            ArgType::Str { nullable: false },
            ArgType::Str { nullable: false },
        ],
        &[
            Arg::NewId(ObjectId(20)),
            Arg::Str(Some("record")),
            Arg::Str(Some("rocks.magical.cast")),
            Arg::Str(Some("Start recording")),
            Arg::Str(Some("SUPER R")),
        ],
    );
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert_eq!(client.shortcut_names(), ["rocks.magical.cast:record"]);

    assert!(!client.fire_shortcut("somebody.else:record", (5, 0)));
    assert_eq!(sent(&mut client), []);
    assert!(client.fire_shortcut("rocks.magical.cast:record", (5, 6)));
    assert_eq!(
        sent(&mut client)
            .iter()
            .map(|event| (event.opcode, event.args.clone()))
            .collect::<Vec<(u16, Vec<String>)>>(),
        [
            (
                compositor_protocol::global_shortcuts::hyprland_global_shortcut_v1::event::PRESSED,
                vec![
                    "Uint(0)".to_owned(),
                    "Uint(5)".to_owned(),
                    "Uint(6)".to_owned()
                ]
            ),
            (
                compositor_protocol::global_shortcuts::hyprland_global_shortcut_v1::event::RELEASED,
                vec![
                    "Uint(0)".to_owned(),
                    "Uint(5)".to_owned(),
                    "Uint(6)".to_owned()
                ]
            ),
        ],
        "a key bound to `global` is a moment, not a hold"
    );

    // The same name twice is a client that has lost track of its own
    // shortcuts, which the protocol calls an error.
    let bytes = request(
        15,
        compositor_protocol::global_shortcuts::hyprland_global_shortcuts_manager_v1::request::REGISTER_SHORTCUT,
        &[
            ArgType::NewId,
            ArgType::Str { nullable: false },
            ArgType::Str { nullable: false },
            ArgType::Str { nullable: false },
            ArgType::Str { nullable: false },
        ],
        &[
            Arg::NewId(ObjectId(21)),
            Arg::Str(Some("record")),
            Arg::Str(Some("rocks.magical.cast")),
            Arg::Str(Some("")),
            Arg::Str(Some("")),
        ],
    );
    let _ = client.read(&bytes, &[]);
    assert!(client.is_finished());
}

/// A focus grab collects surfaces and puts them in force with `commit`,
/// and is told `cleared` when the compositor takes it away.
#[test]
fn a_focus_grab_holds_the_surfaces_it_committed() {
    let mut client = hypr_client();
    let mut bytes = create_surface(20);
    bytes.extend(create_surface(21));
    bytes.extend(request(
        16,
        compositor_protocol::focus_grab::hyprland_focus_grab_manager_v1::request::CREATE_GRAB,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(22))],
    ));
    for surface in [20u32, 21] {
        bytes.extend(request(
            22,
            compositor_protocol::focus_grab::hyprland_focus_grab_v1::request::ADD_SURFACE,
            &[ArgType::Object { nullable: false }],
            &[Arg::Object(ObjectId(surface))],
        ));
    }
    bytes.extend(request(
        22,
        compositor_protocol::focus_grab::hyprland_focus_grab_v1::request::COMMIT,
        &[],
        &[],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(
        client.take_events().iter().any(|event| matches!(
            event,
            Event::FocusGrabbed { grab, surfaces }
                if *grab == ObjectId(22) && surfaces.len() == 2
        )),
        "the compositor is told what it covers"
    );
    assert_eq!(
        client.grabbed(),
        [(ObjectId(22), vec![ObjectId(20), ObjectId(21)])]
    );

    client.grab_cleared(ObjectId(22));
    assert_eq!(
        sent(&mut client)
            .iter()
            .map(|event| event.opcode)
            .collect::<Vec<u16>>(),
        [compositor_protocol::focus_grab::hyprland_focus_grab_v1::event::CLEARED]
    );
    assert_eq!(client.grabbed(), []);
}

/// `hyprland-lock-notify-v1` tells a program that is not the locker when
/// the screen locks, which nothing else does.
#[test]
fn a_lock_notification_is_told_both_ways() {
    let mut client = hypr_client();
    let bytes = request(
        17,
        compositor_protocol::lock_notify::hyprland_lock_notifier_v1::request::GET_LOCK_NOTIFICATION,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(20))],
    );
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(client.watches_lock());

    client.lock_changed(true);
    client.lock_changed(false);
    assert_eq!(
        sent(&mut client)
            .iter()
            .map(|event| event.opcode)
            .collect::<Vec<u16>>(),
        [
            compositor_protocol::lock_notify::hyprland_lock_notification_v1::event::LOCKED,
            compositor_protocol::lock_notify::hyprland_lock_notification_v1::event::UNLOCKED,
        ]
    );
}

/// `hyprland_surface_v1.set_opacity` reaches the same field
/// `wp_alpha_modifier_v1` sets, and destroying it puts the surface back.
#[test]
fn a_hyprland_surface_sets_its_own_opacity() {
    let mut client = hypr_client();
    let mut bytes = create_surface(20);
    bytes.extend(request(
        18,
        compositor_protocol::hyprland_surface::hyprland_surface_manager_v1::request::GET_HYPRLAND_SURFACE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(21)), Arg::Object(ObjectId(20))],
    ));
    bytes.extend(request(
        21,
        compositor_protocol::hyprland_surface::hyprland_surface_v1::request::SET_OPACITY,
        &[ArgType::Fixed],
        &[Arg::Fixed(Fixed::from_f64(0.25))],
    ));
    bytes.extend(commit(20));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    let alpha = client.surface_alpha(ObjectId(20)).expect("a quarter");
    assert!((alpha - 0.25).abs() < 0.001, "{alpha}");
}

/// `hyprland-toplevel-export-v1` is `zwlr_screencopy_v1` for one window:
/// the compositor says what buffer to make, the client makes it and hands
/// it over, and a frame may be copied into once.
#[test]
fn a_window_capture_is_offered_a_buffer_and_filled_once() {
    let mut client = hypr_client();
    let bytes = request(
        19,
        compositor_protocol::toplevel_export::hyprland_toplevel_export_manager_v1::request::CAPTURE_TOPLEVEL,
        &[ArgType::NewId, ArgType::Int, ArgType::Uint],
        &[Arg::NewId(ObjectId(20)), Arg::Int(0), Arg::Uint(7)],
    );
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(
        client.take_events().iter().any(|event| matches!(
            event,
            Event::ToplevelExportAsked {
                frame: ObjectId(20),
                window: 7
            }
        )),
        "the compositor is asked for window 7"
    );

    client.export_buffer(ObjectId(20), 640, 480);
    let told = sent(&mut client);
    assert_eq!(
        told.first().map(|event| event.args.clone()),
        Some(vec![
            format!("Uint({})", crate::Format::Xrgb8888.to_wl_shm()),
            "Uint(640)".to_owned(),
            "Uint(480)".to_owned(),
            "Uint(2560)".to_owned(),
        ])
    );

    // Handing over a buffer asks for the copy; a second `copy` is a client
    // that has lost track of an object it owns.
    let copy = request(
        20,
        compositor_protocol::toplevel_export::hyprland_toplevel_export_frame_v1::request::COPY,
        &[ArgType::Object { nullable: false }, ArgType::Int],
        &[Arg::Object(ObjectId(21)), Arg::Int(0)],
    );
    assert_eq!(client.read(&copy, &[]), copy.len());
    assert!(
        client.take_events().iter().any(|event| matches!(
            event,
            Event::ToplevelExportCopy {
                frame: ObjectId(20),
                window: 7,
                buffer: ObjectId(21)
            }
        )),
        "the compositor is handed the buffer"
    );
    let _ = client.read(&copy, &[]);
    assert!(client.is_finished(), "a second copy is refused");
}

// -- Drag and drop -----------------------------------------------------------

/// A client with `wl_data_device_manager` (9) bound, a surface at 20, a data
/// device at 21 and a source at 22.
fn dragging_client() -> Client {
    let mut client =
        full_client(core::wl_seat::capability::POINTER | core::wl_seat::capability::KEYBOARD);
    let mut bytes = create_surface(20);
    bytes.extend(request(
        9,
        core::wl_data_device_manager::request::GET_DATA_DEVICE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(21)), Arg::Object(ObjectId(7))],
    ));
    bytes.extend(request(
        9,
        core::wl_data_device_manager::request::CREATE_DATA_SOURCE,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(22))],
    ));
    for mime in ["text/uri-list", "text/plain"] {
        bytes.extend(request(
            22,
            core::wl_data_source::request::OFFER,
            &[ArgType::Str { nullable: false }],
            &[Arg::Str(Some(mime))],
        ));
    }
    bytes.extend(request(
        22,
        core::wl_data_source::request::SET_ACTIONS,
        &[ArgType::Uint],
        &[Arg::Uint(
            core::wl_data_device_manager::dnd_action::COPY
                | core::wl_data_device_manager::dnd_action::MOVE,
        )],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(!client.is_finished(), "{:?}", client.fatal());
    let _ = client.take_outgoing();
    let _ = client.take_events();
    client
}

/// `start_drag` tells the compositor what is being dragged, with the types
/// the source offered and the icon it gave.
#[test]
fn start_drag_names_what_is_being_dragged() {
    let mut client = dragging_client();
    let mut bytes = create_surface(23);
    bytes.extend(request(
        21,
        core::wl_data_device::request::START_DRAG,
        &[
            ArgType::Object { nullable: true },
            ArgType::Object { nullable: false },
            ArgType::Object { nullable: true },
            ArgType::Uint,
        ],
        &[
            Arg::Object(ObjectId(22)),
            Arg::Object(ObjectId(20)),
            Arg::Object(ObjectId(23)),
            Arg::Uint(7),
        ],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    let started = client
        .take_events()
        .into_iter()
        .find_map(|event| match event {
            Event::DragStarted {
                source,
                origin,
                icon,
                serial,
                mimes,
            } => Some((source, origin, icon, serial, mimes)),
            _ => None,
        })
        .expect("the compositor is told a drag began");
    assert_eq!(started.0, Some(ObjectId(22)));
    assert_eq!(started.1, ObjectId(20));
    assert_eq!(started.2, Some(ObjectId(23)));
    assert_eq!(started.3, 7);
    assert_eq!(started.4, ["text/uri-list", "text/plain"]);
    assert_eq!(
        client.source_actions(ObjectId(22)),
        core::wl_data_device_manager::dnd_action::COPY
            | core::wl_data_device_manager::dnd_action::MOVE
    );
}

/// The client under the pointer is given an offer with every type on it and
/// what the source can do, then entered -- in that order, because a client
/// reads the types inside its `enter` handler.
#[test]
fn a_drag_over_a_window_offers_it_every_type() {
    let mut client = dragging_client();
    let mimes = ["text/uri-list".to_owned(), "text/plain".to_owned()];
    let offer = client
        .drag_enter(
            ObjectId(20),
            (12.0, 34.0),
            &mimes,
            core::wl_data_device_manager::dnd_action::COPY,
        )
        .expect("an offer was made");
    let told = sent(&mut client);
    let opcodes: Vec<(ObjectId, u16)> = told
        .iter()
        .map(|event| (event.sender, event.opcode))
        .collect();
    assert_eq!(
        opcodes,
        [
            (ObjectId(21), core::wl_data_device::event::DATA_OFFER),
            (offer, core::wl_data_offer::event::OFFER),
            (offer, core::wl_data_offer::event::OFFER),
            (offer, core::wl_data_offer::event::SOURCE_ACTIONS),
            (ObjectId(21), core::wl_data_device::event::ENTER),
        ],
        "the offer, its types, what the source can do, then the enter"
    );
    let entered = told.last().expect("the enter").args.clone();
    assert_eq!(entered[1], format!("Object({:?})", ObjectId(20)));
    assert_eq!(entered[2], "Fixed(12)");
    assert_eq!(entered[3], "Fixed(34)");
    assert_eq!(client.drag_offer(), Some(offer));

    // Moving inside the same surface is a motion and nothing else.
    client.drag_motion(9, (13.0, 35.0));
    assert_eq!(
        sent(&mut client)
            .iter()
            .map(|event| event.opcode)
            .collect::<Vec<u16>>(),
        [core::wl_data_device::event::MOTION]
    );

    // Leaving ends the offer but does not take it back: the client destroys
    // it, and until then it is a live object the client may send to.
    client.drag_leave();
    assert_eq!(
        sent(&mut client)
            .iter()
            .map(|event| event.opcode)
            .collect::<Vec<u16>>(),
        [core::wl_data_device::event::LEAVE]
    );
    assert_eq!(client.drag_offer(), None);
    assert!(client.objects().get(offer).is_some());
}

/// An offer the drag has left is the client's to destroy, and until it does
/// it may still be sent to.
///
/// `wl_data_device.leave` says the client "must destroy the `wl_data_offer`
/// introduced at enter time at this point" -- the client, by its own
/// `destroy`. Chrome, dragging its own selected text, sent
/// `wl_data_offer.set_actions` in the same breath as the `leave` arrived;
/// a server that had already let the offer go answered with
/// `invalid_object` and ended the connection, and Chrome with it. What an
/// offer that has been left asks for is nobody's any more, so the
/// compositor is not told.
#[test]
fn an_offer_the_drag_left_can_still_be_sent_to_and_destroyed() {
    let mut client = dragging_client();
    let offer = client
        .drag_enter(ObjectId(20), (0.0, 0.0), &["text/plain".to_owned()], 3)
        .expect("an offer");
    let _ = sent(&mut client);
    client.drag_leave();
    let _ = sent(&mut client);
    let _ = client.take_events();

    let mut bytes = request(
        offer.0,
        core::wl_data_offer::request::SET_ACTIONS,
        &[ArgType::Uint, ArgType::Uint],
        &[Arg::Uint(1), Arg::Uint(1)],
    );
    bytes.extend(request(
        offer.0,
        core::wl_data_offer::request::ACCEPT,
        &[ArgType::Uint, ArgType::Str { nullable: true }],
        &[Arg::Uint(1), Arg::Str(None)],
    ));
    bytes.extend(request(
        offer.0,
        core::wl_data_offer::request::DESTROY,
        &[],
        &[],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(
        !client.take_events().iter().any(|event| matches!(
            event,
            Event::DragActions { .. } | Event::DragAccepted { .. }
        )),
        "a left offer steers no drag"
    );
    assert!(client.objects().get(offer).is_none(), "the destroy took it");
}

/// A source or device the client destroyed is sent nothing more, even once
/// its number belongs to another object.
///
/// Chrome destroyed the primary source it had replaced and made a frame
/// callback under the same number; the compositor, still holding the old
/// source as the selection, sent it `cancelled` at the next copy, and
/// libwayland read that as `wl_callback` event 1 and ended the connection.
/// The clipboard's twin is tested here because this client has one.
#[test]
fn a_destroyed_source_or_device_is_sent_nothing() {
    let mut client = dragging_client();
    let mut bytes = request(22, core::wl_data_source::request::DESTROY, &[], &[]);
    bytes.extend(request(
        21,
        core::wl_data_device::request::RELEASE,
        &[],
        &[],
    ));
    // The number again, as the frame callback Chrome made.
    bytes.extend(request(
        20,
        core::wl_surface::request::FRAME,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(22))],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    let _ = sent(&mut client);

    client.cancel_selection(ObjectId(22));
    client.offer_selection(&["text/plain".to_owned()]);
    assert_eq!(sent(&mut client), [], "nothing reaches 22 or 21");
    assert!(!client.has_data_device());
}

/// What the target says comes back for the compositor: the type it will
/// take, what it will do with it, and that it has finished.
#[test]
fn the_target_says_what_it_will_take() {
    let mut client = dragging_client();
    let mimes = ["text/plain".to_owned()];
    let offer = client
        .drag_enter(ObjectId(20), (0.0, 0.0), &mimes, 3)
        .expect("an offer");
    let _ = sent(&mut client);
    let _ = client.take_events();

    let mut bytes = request(
        offer.0,
        core::wl_data_offer::request::ACCEPT,
        &[ArgType::Uint, ArgType::Str { nullable: true }],
        &[Arg::Uint(1), Arg::Str(Some("text/plain"))],
    );
    bytes.extend(request(
        offer.0,
        core::wl_data_offer::request::SET_ACTIONS,
        &[ArgType::Uint, ArgType::Uint],
        &[
            Arg::Uint(
                core::wl_data_device_manager::dnd_action::COPY
                    | core::wl_data_device_manager::dnd_action::MOVE,
            ),
            Arg::Uint(core::wl_data_device_manager::dnd_action::MOVE),
        ],
    ));
    bytes.extend(request(
        offer.0,
        core::wl_data_offer::request::FINISH,
        &[],
        &[],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    let events = client.take_events();
    assert!(events.iter().any(|event| matches!(
        event,
        Event::DragAccepted { mime: Some(mime), .. } if mime == "text/plain"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::DragActions { preferred, .. }
            if *preferred == core::wl_data_device_manager::dnd_action::MOVE
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::DragFinished { .. }))
    );

    // And the drop, which is what tells the target it may take it. The drag
    // leaves with it, and the offer stays the one to take the data from.
    assert!(client.drag_drop());
    assert_eq!(
        sent(&mut client)
            .iter()
            .map(|event| event.opcode)
            .collect::<Vec<u16>>(),
        [
            core::wl_data_device::event::DROP,
            core::wl_data_device::event::LEAVE
        ]
    );
    assert_eq!(client.drag_offer(), Some(offer));
    let finish = request(offer.0, core::wl_data_offer::request::FINISH, &[], &[]);
    assert_eq!(client.read(&finish, &[]), finish.len());
    assert!(
        client
            .take_events()
            .iter()
            .any(|event| matches!(event, Event::DragFinished { .. })),
        "a dropped offer is not a left one: its `finish` counts"
    );
}

// -- Where the pointer goes and how a frame is scheduled ---------------------

/// A connection with the presentation globals bound, at 15 upwards, and a
/// surface at 20.
fn frames_client() -> Client {
    let mut globals = globals();
    for (interface, version, role) in [
        (
            &compositor_protocol::pointer_warp::WP_POINTER_WARP_V1,
            1,
            Role::PointerWarp,
        ),
        (
            &compositor_protocol::background_effect::EXT_BACKGROUND_EFFECT_MANAGER_V1,
            1,
            Role::BackgroundEffectManager,
        ),
        (
            &compositor_protocol::tearing_control::WP_TEARING_CONTROL_MANAGER_V1,
            1,
            Role::TearingManager,
        ),
        (
            &compositor_protocol::hotkey::VICINAE_HOTKEY_MANAGER_V1,
            1,
            Role::HotkeyManager,
        ),
    ] {
        assert!(globals.add(interface, version, role).is_some());
    }
    let mut client = Client::new(globals);
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_compositor", 6, 4));
    for (name, interface, version, id) in [
        (5u32, "wp_pointer_warp_v1", 1u32, 15u32),
        (6, "ext_background_effect_manager_v1", 1, 16),
        (7, "wp_tearing_control_manager_v1", 1, 17),
        (8, "vicinae_hotkey_manager_v1", 1, 18),
    ] {
        bytes.extend(bind(2, name, interface, version, id));
    }
    bytes.extend(create_surface(20));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(!client.is_finished(), "{:?}", client.fatal());
    let _ = client.take_outgoing();
    let _ = client.take_events();
    client
}

/// `warp_pointer` names a surface the client owns and a place inside it,
/// and a surface it does not own is a protocol error.
#[test]
fn a_client_may_warp_the_pointer_inside_its_own_window() {
    let mut client = frames_client();
    let bytes = request(
        15,
        compositor_protocol::pointer_warp::wp_pointer_warp_v1::request::WARP_POINTER,
        &[
            ArgType::Object { nullable: false },
            ArgType::Object { nullable: false },
            ArgType::Fixed,
            ArgType::Fixed,
            ArgType::Uint,
        ],
        &[
            Arg::Object(ObjectId(20)),
            Arg::Object(ObjectId(4)),
            Arg::Fixed(Fixed::from_f64(10.5)),
            Arg::Fixed(Fixed::from_f64(20.0)),
            Arg::Uint(3),
        ],
    );
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(
        client.take_events().iter().any(|event| matches!(
            event,
            Event::PointerWarped { surface, at, serial }
                if *surface == ObjectId(20)
                    && at.0 == Fixed::from_f64(10.5)
                    && *serial == 3
        )),
        "the compositor is asked to move the pointer"
    );

    // A surface this client does not own is not one it may warp into.
    let bytes = request(
        15,
        compositor_protocol::pointer_warp::wp_pointer_warp_v1::request::WARP_POINTER,
        &[
            ArgType::Object { nullable: false },
            ArgType::Object { nullable: false },
            ArgType::Fixed,
            ArgType::Fixed,
            ArgType::Uint,
        ],
        &[
            Arg::Object(ObjectId(4)),
            Arg::Object(ObjectId(4)),
            Arg::Fixed(Fixed::from_f64(0.0)),
            Arg::Fixed(Fixed::from_f64(0.0)),
            Arg::Uint(3),
        ],
    );
    let _ = client.read(&bytes, &[]);
    assert!(client.is_finished());
}

/// `ext-background-effect-v1` says what it can do at bind, and a client's
/// blur region reaches the surface on the next commit.
#[test]
fn a_background_effect_blurs_what_is_behind_a_surface() {
    let mut client = frames_client();
    // The capabilities went out at bind, before any request.
    let mut fresh = Client::new({
        let mut globals = globals();
        assert!(
            globals
                .add(
                    &compositor_protocol::background_effect::EXT_BACKGROUND_EFFECT_MANAGER_V1,
                    1,
                    Role::BackgroundEffectManager
                )
                .is_some()
        );
        globals
    });
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 5, "ext_background_effect_manager_v1", 1, 3));
    assert_eq!(fresh.read(&bytes, &[]), bytes.len());
    assert!(
        sent(&mut fresh).iter().any(|event| {
            event.args
                == [format!(
                    "Uint({})",
                    compositor_protocol::background_effect::ext_background_effect_manager_v1::capability::BLUR
                )]
        }),
        "a client told nothing must assume the compositor can do none"
    );

    let mut bytes = request(
        16,
        compositor_protocol::background_effect::ext_background_effect_manager_v1::request::GET_BACKGROUND_EFFECT,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(21)), Arg::Object(ObjectId(20))],
    );
    // A null region is the whole surface, which is what a client asking for
    // a frosted panel sends.
    bytes.extend(request(
        21,
        compositor_protocol::background_effect::ext_background_effect_surface_v1::request::SET_BLUR_REGION,
        &[ArgType::Object { nullable: true }],
        &[Arg::Object(ObjectId::NULL)],
    ));
    bytes.extend(commit(20));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(
        client
            .surface(ObjectId(20))
            .is_some_and(|state| state.current.blur)
    );
}

/// A tearing hint is recorded on the surface it was made for.
#[test]
fn a_tearing_hint_is_recorded_on_its_surface() {
    let mut client = frames_client();
    let mut bytes = request(
        17,
        compositor_protocol::tearing_control::wp_tearing_control_manager_v1::request::GET_TEARING_CONTROL,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(21)), Arg::Object(ObjectId(20))],
    );
    bytes.extend(request(
        21,
        compositor_protocol::tearing_control::wp_tearing_control_v1::request::SET_PRESENTATION_HINT,
        &[ArgType::Uint],
        &[Arg::Uint(
            compositor_protocol::tearing_control::wp_tearing_control_v1::presentation_hint::ASYNC,
        )],
    ));
    bytes.extend(commit(20));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(
        client
            .surface(ObjectId(20))
            .is_some_and(|state| state.current.tearing)
    );
}

/// A launcher's hotkey is bound at once and fires on the key it named.
#[test]
fn a_hotkey_is_bound_and_fires_on_its_key() {
    let mut client = frames_client();
    let bytes = request(
        18,
        compositor_protocol::hotkey::vicinae_hotkey_manager_v1::request::BIND,
        &[
            ArgType::NewId,
            ArgType::Uint,
            ArgType::Uint,
            ArgType::Object { nullable: false },
            ArgType::Str { nullable: false },
            ArgType::Str { nullable: false },
        ],
        &[
            Arg::NewId(ObjectId(21)),
            Arg::Uint(0x0020),
            Arg::Uint(64),
            Arg::Object(ObjectId(4)),
            Arg::Str(Some("vicinae")),
            Arg::Str(Some("Open the launcher")),
        ],
    );
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert_eq!(
        sent(&mut client)
            .iter()
            .map(|event| event.opcode)
            .collect::<Vec<u16>>(),
        [compositor_protocol::hotkey::vicinae_hotkey_v1::event::BOUND],
        "a launcher has to know whether to wait for the key"
    );
    assert_eq!(
        client.hotkeys().first().map(|held| held.app_id.clone()),
        Some("vicinae".to_owned())
    );

    assert!(!client.fire_hotkey(0x0020, 0, 5), "the modifiers differ");
    assert!(client.fire_hotkey(0x0020, 64, 5));
    assert_eq!(
        sent(&mut client)
            .iter()
            .map(|event| event.opcode)
            .collect::<Vec<u16>>(),
        [
            compositor_protocol::hotkey::vicinae_hotkey_v1::event::PRESSED,
            compositor_protocol::hotkey::vicinae_hotkey_v1::event::RELEASED,
        ]
    );
}

// -- Screenshots as the `ext` namespace has them -----------------------------

/// A connection with the capture globals bound, at 15 upwards, and a
/// `wl_output` at 14.
fn capturing_client() -> Client {
    let mut globals = globals();
    for (interface, version, role) in [
        (&core::WL_OUTPUT, 4, Role::Output),
        (
            &compositor_protocol::capture_source::EXT_OUTPUT_IMAGE_CAPTURE_SOURCE_MANAGER_V1,
            1,
            Role::OutputCaptureSourceManager,
        ),
        (
            &compositor_protocol::image_copy::EXT_IMAGE_COPY_CAPTURE_MANAGER_V1,
            1,
            Role::CaptureManager,
        ),
    ] {
        assert!(globals.add(interface, version, role).is_some());
    }
    let mut client = Client::new(globals);
    client.set_outputs(vec![crate::Output::default()]);
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 5, "wl_output", 4, 14));
    bytes.extend(bind(
        2,
        6,
        "ext_output_image_capture_source_manager_v1",
        1,
        15,
    ));
    bytes.extend(bind(2, 7, "ext_image_copy_capture_manager_v1", 1, 16));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(!client.is_finished(), "{:?}", client.fatal());
    let _ = client.take_outgoing();
    let _ = client.take_events();
    client
}

/// A source, a session and a frame: the compositor says what buffer to
/// make, the client makes one and hands it over, and the frame is answered.
#[test]
fn a_capture_session_offers_a_buffer_and_answers_a_frame() {
    let mut client = capturing_client();
    let mut bytes = request(
        15,
        compositor_protocol::capture_source::ext_output_image_capture_source_manager_v1::request::CREATE_SOURCE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(20)), Arg::Object(ObjectId(14))],
    );
    bytes.extend(request(
        16,
        compositor_protocol::image_copy::ext_image_copy_capture_manager_v1::request::CREATE_SESSION,
        &[
            ArgType::NewId,
            ArgType::Object { nullable: false },
            ArgType::Uint,
        ],
        &[
            Arg::NewId(ObjectId(21)),
            Arg::Object(ObjectId(20)),
            Arg::Uint(0),
        ],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(
        client.take_events().iter().any(|event| matches!(
            event,
            Event::CaptureSession {
                session: ObjectId(21),
                source: crate::Source::Screen(0)
            }
        )),
        "the compositor is told what is being captured"
    );

    // The size, every format, then `done`: a client waits for `done` and
    // reads everything before it.
    client.capture_offer(ObjectId(21), 640, 480);
    assert_eq!(
        sent(&mut client)
            .iter()
            .map(|event| (event.opcode, event.args.clone()))
            .collect::<Vec<(u16, Vec<String>)>>(),
        [
            (
                compositor_protocol::image_copy::ext_image_copy_capture_session_v1::event::BUFFER_SIZE,
                vec!["Uint(640)".to_owned(), "Uint(480)".to_owned()]
            ),
            (
                compositor_protocol::image_copy::ext_image_copy_capture_session_v1::event::SHM_FORMAT,
                vec![format!("Uint({})", crate::Format::Xrgb8888.to_wl_shm())]
            ),
            (
                compositor_protocol::image_copy::ext_image_copy_capture_session_v1::event::DONE,
                vec![]
            ),
        ]
    );

    // A frame, its buffer, and the `capture` that asks for it.
    let mut bytes = request(
        21,
        compositor_protocol::image_copy::ext_image_copy_capture_session_v1::request::CREATE_FRAME,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(22))],
    );
    bytes.extend(request(
        22,
        compositor_protocol::image_copy::ext_image_copy_capture_frame_v1::request::ATTACH_BUFFER,
        &[ArgType::Object { nullable: false }],
        &[Arg::Object(ObjectId(23))],
    ));
    bytes.extend(request(
        22,
        compositor_protocol::image_copy::ext_image_copy_capture_frame_v1::request::CAPTURE,
        &[],
        &[],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(
        client.take_events().iter().any(|event| matches!(
            event,
            Event::CaptureAsked {
                frame: ObjectId(22),
                buffer: ObjectId(23),
                ..
            }
        )),
        "the compositor is handed the buffer"
    );

    client.capture_ready(ObjectId(22), (9, 500));
    assert_eq!(
        sent(&mut client)
            .iter()
            .map(|event| event.opcode)
            .collect::<Vec<u16>>(),
        [
            compositor_protocol::image_copy::ext_image_copy_capture_frame_v1::event::PRESENTATION_TIME,
            compositor_protocol::image_copy::ext_image_copy_capture_frame_v1::event::READY,
        ]
    );
}

/// `capture` with no buffer is a client that lost track of its own frame.
#[test]
fn a_capture_with_no_buffer_is_refused() {
    let mut client = capturing_client();
    let mut bytes = request(
        15,
        compositor_protocol::capture_source::ext_output_image_capture_source_manager_v1::request::CREATE_SOURCE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(20)), Arg::Object(ObjectId(14))],
    );
    bytes.extend(request(
        16,
        compositor_protocol::image_copy::ext_image_copy_capture_manager_v1::request::CREATE_SESSION,
        &[
            ArgType::NewId,
            ArgType::Object { nullable: false },
            ArgType::Uint,
        ],
        &[
            Arg::NewId(ObjectId(21)),
            Arg::Object(ObjectId(20)),
            Arg::Uint(0),
        ],
    ));
    bytes.extend(request(
        21,
        compositor_protocol::image_copy::ext_image_copy_capture_session_v1::request::CREATE_FRAME,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(22))],
    ));
    bytes.extend(request(
        22,
        compositor_protocol::image_copy::ext_image_copy_capture_frame_v1::request::CAPTURE,
        &[],
        &[],
    ));
    let _ = client.read(&bytes, &[]);
    assert!(client.is_finished());
}

#[test]
fn a_child_declared_older_than_its_manager_is_made_at_its_own_version() {
    // pointer-gestures has its manager at 3 and its swipe at 2. A request's
    // new object takes its parent's version, which is above what the swipe
    // interface has; libwayland makes it all the same, and Chrome, which
    // does this, lost its connection when this refused it.
    use compositor_protocol::pointer_gestures;
    let mut globals = Globals::new();
    let seat = globals.add(&core::WL_SEAT, 7, Role::Seat).unwrap();
    let gestures = globals
        .add(
            &pointer_gestures::ZWP_POINTER_GESTURES_V1,
            3,
            Role::PointerGestures,
        )
        .unwrap();
    let mut client = Client::new(globals);
    client.set_seat_capabilities(core::wl_seat::capability::POINTER);
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, seat, "wl_seat", 7, 3));
    bytes.extend(bind(2, gestures, "zwp_pointer_gestures_v1", 3, 4));
    bytes.extend(request(
        3,
        core::wl_seat::request::GET_POINTER,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(5))],
    ));
    bytes.extend(request(
        4,
        pointer_gestures::zwp_pointer_gestures_v1::request::GET_SWIPE_GESTURE,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(6)), Arg::Object(ObjectId(5))],
    ));
    let _ = client.read(&bytes, &[]);
    assert_eq!(client.fatal(), None);
    assert_eq!(
        client.objects().get(ObjectId(6)).map(|entry| entry.version),
        Some(2)
    );
}

/// A keyboard found after a client bound the seat: the client is told the
/// seat has one now, and may then ask for it. A seat that says the same
/// thing again says nothing.
#[test]
fn a_seat_that_gains_a_keyboard_says_so_to_a_bound_client() {
    let mut globals = Globals::new();
    assert!(globals.add(&core::WL_SEAT, 7, Role::Seat).is_some());
    let mut client = Client::new(globals);
    client.set_seat_capabilities(core::wl_seat::capability::POINTER);
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_seat", 7, 7));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    let _ = sent(&mut client);

    let both = core::wl_seat::capability::POINTER | core::wl_seat::capability::KEYBOARD;
    client.change_seat_capabilities(both);
    let events = sent(&mut client);
    assert_eq!(events.len(), 1, "one seat, one event: {events:?}");
    assert_eq!(events[0].opcode, core::wl_seat::event::CAPABILITIES);
    assert_eq!(events[0].args, [format!("Uint({both})")]);

    client.change_seat_capabilities(both);
    assert!(
        sent(&mut client).is_empty(),
        "nothing changed, nothing said"
    );

    let asked = request(
        7,
        core::wl_seat::request::GET_KEYBOARD,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(11))],
    );
    assert_eq!(client.read(&asked, &[]), asked.len());
    assert_eq!(
        client.fatal(),
        None,
        "the keyboard it was told of is its to ask for"
    );
}

/// A cursor shape is drawn as the compositor's arrow, which is what a client
/// that has said nothing gets; only `set_cursor` with a null surface hides
/// the pointer. Chrome names a shape for every cursor, and with shapes
/// hiding it, its windows had no pointer at all.
#[test]
fn a_cursor_shape_shows_the_arrow_and_a_null_cursor_hides_it() {
    let mut globals = Globals::new();
    for (interface, version, role) in [
        (&core::WL_SEAT, 7, Role::Seat),
        (
            &compositor_protocol::cursor_shape::WP_CURSOR_SHAPE_MANAGER_V1,
            1,
            Role::CursorShapeManager,
        ),
    ] {
        assert!(globals.add(interface, version, role).is_some());
    }
    let mut client = Client::new(globals);
    client.set_seat_capabilities(core::wl_seat::capability::POINTER);
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_seat", 7, 3));
    bytes.extend(bind(2, 2, "wp_cursor_shape_manager_v1", 1, 4));
    bytes.extend(request(
        3,
        core::wl_seat::request::GET_POINTER,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(5))],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(!client.said_cursor(), "nothing said: the arrow");

    let hide = request(
        5,
        core::wl_pointer::request::SET_CURSOR,
        &[
            ArgType::Uint,
            ArgType::Object { nullable: true },
            ArgType::Int,
            ArgType::Int,
        ],
        &[
            Arg::Uint(0),
            Arg::Object(ObjectId::NULL),
            Arg::Int(0),
            Arg::Int(0),
        ],
    );
    assert_eq!(client.read(&hide, &[]), hide.len());
    assert!(
        client.said_cursor() && client.cursor().is_none(),
        "a null cursor is asked for no pointer"
    );

    let mut shaped = request(
        4,
        compositor_protocol::cursor_shape::wp_cursor_shape_manager_v1::request::GET_POINTER,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(6)), Arg::Object(ObjectId(5))],
    );
    shaped.extend(request(
        6,
        compositor_protocol::cursor_shape::wp_cursor_shape_device_v1::request::SET_SHAPE,
        &[ArgType::Uint, ArgType::Uint],
        &[
            Arg::Uint(0),
            Arg::Uint(compositor_protocol::cursor_shape::wp_cursor_shape_device_v1::shape::DEFAULT),
        ],
    ));
    assert_eq!(client.read(&shaped, &[]), shaped.len());
    assert_eq!(client.fatal(), None);
    assert!(
        !client.said_cursor() && client.cursor().is_none(),
        "a shape is drawn as the arrow, not hidden"
    );
}

/// A client may destroy a pool, go on drawing with a buffer cut from it, and
/// make a new pool that libwayland gives the freed id. The buffer still
/// names the first pool, which is still in use, and the compositor is told
/// that pool was retired and a different one made: Chrome's window was read
/// out of its next pool's few kilobytes, and drawn blank, when they were
/// both known by the id.
#[test]
fn a_pool_made_under_a_destroyed_pools_id_is_another_pool() {
    let mut client = drawing_client();
    let mut bytes = create_pool(6, 4096);
    bytes.extend(create_buffer(6, 7, 0, 16, 16, 64, 1));
    assert_eq!(client.read(&bytes, &[Fd(3)]), bytes.len());
    let first = client.pool(ObjectId(6)).expect("a pool").key;
    assert_eq!(client.buffer(ObjectId(7)).expect("a buffer").pool, first);
    let _ = client.take_events();

    let destroy = request(6, core::wl_shm_pool::request::DESTROY, &[], &[]);
    assert_eq!(client.read(&destroy, &[]), destroy.len());
    assert!(
        client
            .take_events()
            .contains(&Event::PoolRetired { pool: first }),
        "the compositor is told which pool went"
    );

    let again = create_pool(6, 64);
    assert_eq!(client.read(&again, &[Fd(4)]), again.len());
    assert_eq!(client.fatal(), None);
    let second = client.pool(ObjectId(6)).expect("the new pool").key;
    assert_ne!(second, first, "a pool's key is never given again");
    assert!(
        client
            .take_events()
            .iter()
            .any(|event| matches!(event, Event::PoolCreated { pool, .. } if *pool == second))
    );
    assert_eq!(
        client.buffer(ObjectId(7)).expect("the buffer").pool,
        first,
        "the buffer is still the first pool's"
    );
    assert!(client.pool_in_use(first), "its buffer keeps the first pool");
    assert!(!client.pool_in_use(second));
}

/// A popup's `set_window_geometry` is its menu, inside the shadow its client
/// draws around it, as a toplevel's is its window: Chrome's menu said
/// `(24, 12, 406, 667)`, and drawn whole its rows sat 24 pixels right of and
/// 12 below where the positioner had put them. A surface with no xdg role
/// has no geometry.
#[test]
fn a_toplevel_and_a_popup_each_say_where_their_window_is() {
    let mut client = drawing_client();
    let geometry = |xdg: u32, x: i32, y: i32, width: i32, height: i32| {
        request(
            xdg,
            xdg_shell::xdg_surface::request::SET_WINDOW_GEOMETRY,
            &[ArgType::Int, ArgType::Int, ArgType::Int, ArgType::Int],
            &[Arg::Int(x), Arg::Int(y), Arg::Int(width), Arg::Int(height)],
        )
    };
    let xdg_surface = |id: u32, surface: u32| {
        request(
            6,
            xdg_shell::xdg_wm_base::request::GET_XDG_SURFACE,
            &[ArgType::NewId, ArgType::Object { nullable: false }],
            &[Arg::NewId(ObjectId(id)), Arg::Object(ObjectId(surface))],
        )
    };
    let mut bytes = bind(2, 4, "xdg_wm_base", 6, 6);
    bytes.extend(create_surface(3));
    bytes.extend(xdg_surface(7, 3));
    bytes.extend(request(
        7,
        xdg_shell::xdg_surface::request::GET_TOPLEVEL,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(8))],
    ));
    bytes.extend(geometry(7, 10, 10, 100, 100));
    bytes.extend(create_surface(9));
    bytes.extend(xdg_surface(10, 9));
    bytes.extend(request(
        6,
        xdg_shell::xdg_wm_base::request::CREATE_POSITIONER,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(11))],
    ));
    bytes.extend(request(
        11,
        xdg_shell::xdg_positioner::request::SET_SIZE,
        &[ArgType::Int, ArgType::Int],
        &[Arg::Int(50), Arg::Int(20)],
    ));
    bytes.extend(request(
        11,
        xdg_shell::xdg_positioner::request::SET_ANCHOR_RECT,
        &[ArgType::Int, ArgType::Int, ArgType::Int, ArgType::Int],
        &[Arg::Int(0), Arg::Int(0), Arg::Int(1), Arg::Int(1)],
    ));
    bytes.extend(request(
        10,
        xdg_shell::xdg_surface::request::GET_POPUP,
        &[
            ArgType::NewId,
            ArgType::Object { nullable: true },
            ArgType::Object { nullable: false },
        ],
        &[
            Arg::NewId(ObjectId(12)),
            Arg::Object(ObjectId(7)),
            Arg::Object(ObjectId(11)),
        ],
    ));
    bytes.extend(geometry(10, 24, 12, 50, 20));
    bytes.extend(create_surface(13));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);

    assert_eq!(
        client.window_geometry(ObjectId(3)),
        Some((10, 10, 100, 100))
    );
    assert_eq!(client.window_geometry(ObjectId(9)), Some((24, 12, 50, 20)));
    assert_eq!(client.window_geometry(ObjectId(13)), None, "no xdg role");
}

// -- ext-session-lock-v1 -----------------------------------------------------

/// A connection with `ext_session_lock_manager_v1` bound at 15, and a lock
/// asked for on 20.
fn locking_client() -> Client {
    use compositor_protocol::session_lock;
    let mut globals = globals();
    assert!(
        globals
            .add(
                &session_lock::EXT_SESSION_LOCK_MANAGER_V1,
                1,
                Role::SessionLockManager,
            )
            .is_some()
    );
    let mut client = Client::new(globals);
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 5, "ext_session_lock_manager_v1", 1, 15));
    bytes.extend(request(
        15,
        session_lock::ext_session_lock_manager_v1::request::LOCK,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(20))],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert!(
        client
            .take_events()
            .contains(&Event::SessionLocked { lock: ObjectId(20) })
    );
    let _ = client.take_outgoing();
    client
}

/// `unlock_and_destroy` on lock 20.
fn unlock_20() -> Vec<u8> {
    request(
        20,
        compositor_protocol::session_lock::ext_session_lock_v1::request::UNLOCK_AND_DESTROY,
        &[],
        &[],
    )
}

/// The unlocks among a client's events.
fn unlocks(client: &mut Client) -> Vec<Event> {
    client
        .take_events()
        .into_iter()
        .filter(|event| matches!(event, Event::SessionUnlocked { .. }))
        .collect()
}

/// The holder's unlock names the lock it was asked on, which is how the
/// compositor tells it from anyone else's (`docs/AUTH.md` §3.7).
#[test]
fn an_unlock_names_the_lock_it_lets_go() {
    let mut client = locking_client();
    let bytes = unlock_20();
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert_eq!(
        unlocks(&mut client),
        [Event::SessionUnlocked {
            lock: ObjectId(20),
            asked: true,
        }]
    );
}

/// A lock the compositor refused is let go of when `finished` is sent: its
/// `unlock_and_destroy` unlocks nothing.
#[test]
fn a_refused_lock_unlocks_nothing() {
    let mut client = locking_client();
    client.session_lock_refused(ObjectId(20));
    let bytes = unlock_20();
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    assert_eq!(unlocks(&mut client), []);
}

/// And its `destroy` is the answer the protocol asks for after `finished`,
/// not `invalid_destroy`.
#[test]
fn a_refused_lock_may_be_destroyed() {
    let mut client = locking_client();
    client.session_lock_refused(ObjectId(20));
    let bytes = request(
        20,
        compositor_protocol::session_lock::ext_session_lock_v1::request::DESTROY,
        &[],
        &[],
    );
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
}

/// A monitor plugged in is a `wl_output` global more, told to every
/// registry; one unplugged is a `global_remove` of its name, its bound
/// objects name no monitor, and the next monitor's objects move down a
/// place (docs/DESKTOP.md §3.2).
#[test]
fn outputs_come_and_go_while_a_client_runs() {
    let mut globals = Globals::new();
    assert!(globals.add(&core::WL_OUTPUT, 4, Role::Output).is_some());
    assert!(globals.add(&core::WL_OUTPUT, 4, Role::Output).is_some());
    let mut client = Client::new(globals);
    let mut bytes = get_registry(2);
    bytes.extend(bind(2, 1, "wl_output", 4, 3));
    bytes.extend(bind(2, 2, "wl_output", 4, 4));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    let _ = sent(&mut client);
    assert_eq!(client.output_of(ObjectId(3)), Some(0));
    assert_eq!(client.output_of(ObjectId(4)), Some(1));

    client.output_added();
    let added: Vec<_> = sent(&mut client)
        .into_iter()
        .filter(|event| event.sender == ObjectId(2))
        .collect();
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].opcode, core::wl_registry::event::GLOBAL);
    assert_eq!(added[0].args[0], "Uint(3)", "a name never used before");

    client.output_removed(0);
    let removed: Vec<_> = sent(&mut client)
        .into_iter()
        .filter(|event| event.sender == ObjectId(2))
        .collect();
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].opcode, core::wl_registry::event::GLOBAL_REMOVE);
    assert_eq!(removed[0].args, ["Uint(1)"]);
    assert_eq!(client.output_of(ObjectId(3)), None, "inert");
    assert_eq!(client.output_of(ObjectId(4)), Some(0), "moved down");

    // A bind of the removed name now makes nothing of it.
    let late = bind(2, 1, "wl_output", 4, 5);
    let _ = client.read(&late, &[]);
    assert_eq!(client.output_of(ObjectId(5)), None);
}

/// A screen's every mode is published, the first preferred and the running
/// one current, and `set_mode` on one asks for its size and refresh: what
/// `wlr-randr --mode` sends (docs/DESKTOP.md §3.2).
#[test]
fn output_management_lists_every_mode_and_set_mode_names_one() {
    use compositor_protocol::output_management::{
        zwlr_output_configuration_head_v1, zwlr_output_configuration_v1, zwlr_output_head_v1,
        zwlr_output_manager_v1, zwlr_output_mode_v1,
    };
    let mut client = watching_client();
    client.set_outputs(vec![crate::Output {
        width: 1920,
        height: 1080,
        refresh: 60_000,
        name: "DP-1".to_owned(),
        modes: vec![
            (2560, 1440, 59_951),
            (1920, 1080, 60_000),
            (1280, 720, 60_000),
        ],
        ..crate::Output::default()
    }]);
    let bytes = bind(2, 10, "zwlr_output_manager_v1", 4, 21);
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    let told = sent(&mut client);
    let sizes: Vec<&Sent> = told
        .iter()
        .filter(|event| event.opcode == zwlr_output_mode_v1::event::SIZE && event.args.len() == 2)
        .collect();
    assert_eq!(sizes.len(), 3, "every mode: {told:?}");
    let preferred = told
        .iter()
        .filter(|event| {
            event.opcode == zwlr_output_mode_v1::event::PREFERRED && event.args.is_empty()
        })
        .count();
    assert!(preferred >= 1);
    let head = told
        .iter()
        .find(|event| event.opcode == zwlr_output_head_v1::event::CURRENT_MODE)
        .map(|event| event.sender)
        .expect("a current mode");
    // The mode of 1280x720: its object is the sender of that SIZE.
    let small = sizes
        .iter()
        .find(|event| event.args == ["Int(1280)", "Int(720)"])
        .map(|event| event.sender)
        .expect("the 1280x720 mode");
    let serial = told
        .iter()
        .rev()
        .find(|event| {
            event.sender == ObjectId(21) && event.opcode == zwlr_output_manager_v1::event::DONE
        })
        .and_then(|event| event.args.first().cloned())
        .expect("a serial");
    let serial: u32 = serial
        .trim_start_matches("Uint(")
        .trim_end_matches(')')
        .parse()
        .expect("a number");
    let mut bytes = request(
        21,
        zwlr_output_manager_v1::request::CREATE_CONFIGURATION,
        &[ArgType::NewId, ArgType::Uint],
        &[Arg::NewId(ObjectId(40)), Arg::Uint(serial)],
    );
    bytes.extend(request(
        40,
        zwlr_output_configuration_v1::request::ENABLE_HEAD,
        &[ArgType::NewId, ArgType::Object { nullable: false }],
        &[Arg::NewId(ObjectId(41)), Arg::Object(head)],
    ));
    bytes.extend(request(
        41,
        zwlr_output_configuration_head_v1::request::SET_MODE,
        &[ArgType::Object { nullable: false }],
        &[Arg::Object(small)],
    ));
    bytes.extend(request(
        40,
        zwlr_output_configuration_v1::request::APPLY,
        &[],
        &[],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert_eq!(client.fatal(), None);
    let asked = client
        .take_events()
        .into_iter()
        .find_map(|event| match event {
            Event::OutputConfigured { heads, .. } => Some(heads),
            _ => None,
        })
        .expect("a configuration");
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].1.size, Some((1280, 720)));
    assert_eq!(asked[0].1.refresh, Some(60_000));
}
