//! `zwp_linux_dmabuf_v1`: what the server checks and what it asks the
//! binary for (`docs/GPU.md` §3.13).

use compositor_protocol::core;
use compositor_protocol::linux_dmabuf::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1};
use compositor_wire::{Arg, ArgType, Fd, ObjectId, Reader, Writer};

use crate::{Client, Event, Fatal, Globals, Role};

/// One message's bytes, as a client would send them.
fn request(sender: u32, opcode: u16, signature: &'static [ArgType], args: &[Arg<'_>]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer
        .write(ObjectId(sender), opcode, signature, args)
        .expect("a message a client could send");
    writer.bytes().to_vec()
}

/// The opcodes of everything the server queued, with who sent each.
fn sent(client: &mut Client) -> Vec<(ObjectId, u16)> {
    let outgoing = client.take_outgoing();
    let mut reader = Reader::new(&outgoing.bytes, &outgoing.descriptors);
    let mut out = Vec::new();
    while !reader.is_done() {
        let header = reader.peek().expect("a header");
        let interface = client
            .objects()
            .get(header.sender)
            .map_or(&core::WL_DISPLAY, |entry| entry.interface);
        let method = interface.event(header.opcode).expect("an event");
        let _ = reader.read(method.signature).expect("it reads");
        out.push((header.sender, header.opcode));
    }
    out
}

/// A connection offered `zwp_linux_dmabuf_v1` at version 3, bound as
/// object 3, and what the bind sent.
fn dmabuf_client() -> (Client, Vec<(ObjectId, u16)>) {
    let (client, outgoing) = dmabuf_client_offered(None);
    (client, outgoing)
}

/// The modifiers a bind's `modifier` events named, a format at a time in
/// the order they came.
fn modifiers_told(client: &mut Client) -> Vec<(u32, u64)> {
    let outgoing = client.take_outgoing();
    let mut reader = Reader::new(&outgoing.bytes, &outgoing.descriptors);
    let mut out = Vec::new();
    while !reader.is_done() {
        let header = reader.peek().expect("a header");
        let interface = client
            .objects()
            .get(header.sender)
            .map_or(&core::WL_DISPLAY, |entry| entry.interface);
        let method = interface.event(header.opcode).expect("an event");
        let message = reader.read(method.signature).expect("it reads");
        if header.sender == ObjectId(3) && header.opcode == zwp_linux_dmabuf_v1::event::MODIFIER {
            let words: Vec<u32> = message.1.iter().filter_map(Arg::as_uint).collect();
            let [format, high, low] = words.as_slice() else {
                panic!("a modifier event of three words");
            };
            out.push((*format, (u64::from(*high) << 32) | u64::from(*low)));
        }
    }
    out
}

/// [`dmabuf_client`], with the compositor having said which modifiers it
/// offers when `offered` is some; what the bind sent is left unread.
fn dmabuf_client_unread(offered: Option<&[u64]>) -> Client {
    let mut globals = Globals::new();
    if let Some(offered) = offered {
        globals.offer_dmabuf_modifiers(offered);
    }
    assert!(
        globals
            .add(
                &compositor_protocol::linux_dmabuf::ZWP_LINUX_DMABUF_V1,
                3,
                Role::LinuxDmabuf,
            )
            .is_some()
    );
    let mut client = Client::new(globals);
    let mut bytes = request(
        1,
        core::wl_display::request::GET_REGISTRY,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(2))],
    );
    bytes.extend(request(
        2,
        core::wl_registry::request::BIND,
        &[ArgType::Uint, ArgType::AnyNewId],
        &[
            Arg::Uint(1),
            Arg::AnyNewId {
                interface: "zwp_linux_dmabuf_v1",
                version: 3,
                id: ObjectId(3),
            },
        ],
    ));
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    client
}

/// [`dmabuf_client_unread`], and what the bind sent.
fn dmabuf_client_offered(offered: Option<&[u64]>) -> (Client, Vec<(ObjectId, u16)>) {
    let mut client = dmabuf_client_unread(offered);
    let told = sent(&mut client);
    (client, told)
}

/// `create_params(id)` on object 3.
fn create_params(id: u32) -> Vec<u8> {
    request(
        3,
        zwp_linux_dmabuf_v1::request::CREATE_PARAMS,
        &[ArgType::NewId],
        &[Arg::NewId(ObjectId(id))],
    )
}

/// `add(fd, plane, offset, stride, modifier)` on `params`.
fn add_plane(params: u32, plane: u32, stride: u32, modifier: u64) -> Vec<u8> {
    request(
        params,
        zwp_linux_buffer_params_v1::request::ADD,
        &[
            ArgType::Fd,
            ArgType::Uint,
            ArgType::Uint,
            ArgType::Uint,
            ArgType::Uint,
            ArgType::Uint,
        ],
        &[
            Arg::Fd(Fd(0)),
            Arg::Uint(plane),
            Arg::Uint(0),
            Arg::Uint(stride),
            Arg::Uint(u32::try_from(modifier >> 32).expect("the high word")),
            Arg::Uint(u32::try_from(modifier & 0xffff_ffff).expect("the low word")),
        ],
    )
}

/// `create_immed(buffer, width, height, format, 0)` on `params`.
fn create_immed(params: u32, buffer: u32, width: i32, format: u32) -> Vec<u8> {
    request(
        params,
        zwp_linux_buffer_params_v1::request::CREATE_IMMED,
        &[
            ArgType::NewId,
            ArgType::Int,
            ArgType::Int,
            ArgType::Uint,
            ArgType::Uint,
        ],
        &[
            Arg::NewId(ObjectId(buffer)),
            Arg::Int(width),
            Arg::Int(32),
            Arg::Uint(format),
            Arg::Uint(0),
        ],
    )
}

/// Binding tells the client every format with each modifier, version 3's
/// way: `format`, and then `modifier` for linear and the implicit layout.
#[test]
fn a_bound_dmabuf_says_its_formats_and_modifiers() {
    let (_, told) = dmabuf_client();
    let opcodes: Vec<u16> = told
        .iter()
        .filter(|(sender, _)| *sender == ObjectId(3))
        .map(|(_, opcode)| *opcode)
        .collect();
    let (format, modifier) = (
        zwp_linux_dmabuf_v1::event::FORMAT,
        zwp_linux_dmabuf_v1::event::MODIFIER,
    );
    assert_eq!(
        opcodes,
        [format, modifier, modifier, format, modifier, modifier]
    );
}

/// A buffer made with `create_immed` is a `wl_buffer` at once, the
/// descriptor is handed to the binary when `add` takes it, and the import
/// is asked for with what the client said; destroying the buffer retires
/// the import.
#[test]
fn create_immed_makes_a_buffer_and_asks_for_the_import() {
    let (mut client, _) = dmabuf_client();
    let mut bytes = create_params(4);
    bytes.extend(add_plane(4, 0, 256, crate::MOD_LINEAR));
    bytes.extend(create_immed(4, 5, 64, crate::DRM_FORMAT_XRGB8888));
    assert_eq!(client.read(&bytes, &[Fd(9)]), bytes.len());
    assert_eq!(client.fatal(), None);
    let events = client.take_events();
    assert!(
        events.contains(&Event::DmabufPlane {
            params: ObjectId(4),
            fd: Fd(9),
        }),
        "{events:?}"
    );
    let asked = events
        .iter()
        .find_map(|event| match event {
            Event::DmabufCreated { dmabuf } => Some(*dmabuf),
            _ => None,
        })
        .expect("the import is asked for");
    assert_eq!((asked.width, asked.height), (64, 32));
    assert_eq!(asked.plane.stride, 256);
    assert_eq!(asked.format, crate::Format::Xrgb8888);
    let buffer = *client.buffer(ObjectId(5)).expect("the buffer is made");
    assert!(buffer.dmabuf);
    assert_eq!(buffer.pool, asked.pool);
    assert_eq!(buffer.range(), Some((0, 256 * 32)));

    client.dmabuf_imported(ObjectId(4), true);
    assert_eq!(client.fatal(), None);
    let bytes = request(5, core::wl_buffer::request::DESTROY, &[], &[]);
    assert_eq!(client.read(&bytes, &[]), bytes.len());
    assert!(
        client
            .take_events()
            .contains(&Event::PoolRetired { pool: asked.pool })
    );
}

/// A `create_immed` whose buffer could not be imported ends the connection
/// with `invalid_wl_buffer`.
#[test]
fn a_buffer_that_cannot_be_imported_ends_the_connection() {
    let (mut client, _) = dmabuf_client();
    let mut bytes = create_params(4);
    bytes.extend(add_plane(4, 0, 256, crate::MOD_INVALID));
    bytes.extend(create_immed(4, 5, 64, crate::DRM_FORMAT_ARGB8888));
    assert_eq!(client.read(&bytes, &[Fd(9)]), bytes.len());
    client.dmabuf_imported(ObjectId(4), false);
    assert!(matches!(
        client.fatal(),
        Some(Fatal::Interface { code, .. })
            if *code == zwp_linux_buffer_params_v1::error::INVALID_WL_BUFFER
    ));
}

/// `create` is answered only once the import has been: `created` with a
/// buffer the server made, or `failed`.
#[test]
fn create_is_answered_after_the_import() {
    for imported in [true, false] {
        let (mut client, _) = dmabuf_client();
        let mut bytes = create_params(4);
        bytes.extend(add_plane(4, 0, 256, crate::MOD_LINEAR));
        bytes.extend(request(
            4,
            zwp_linux_buffer_params_v1::request::CREATE,
            &[ArgType::Int, ArgType::Int, ArgType::Uint, ArgType::Uint],
            &[
                Arg::Int(64),
                Arg::Int(32),
                Arg::Uint(crate::DRM_FORMAT_ARGB8888),
                Arg::Uint(0),
            ],
        ));
        assert_eq!(client.read(&bytes, &[Fd(9)]), bytes.len());
        assert!(sent(&mut client).is_empty(), "nothing before the import");
        client.dmabuf_imported(ObjectId(4), imported);
        let wanted = if imported {
            zwp_linux_buffer_params_v1::event::CREATED
        } else {
            zwp_linux_buffer_params_v1::event::FAILED
        };
        assert_eq!(sent(&mut client), [(ObjectId(4), wanted)]);
        assert_eq!(client.fatal(), None);
    }
}

/// Each of the protocol's errors, for the mistake it names.
#[test]
fn a_dmabuf_is_held_to_the_protocols_errors() {
    use zwp_linux_buffer_params_v1::error;
    let linear = crate::MOD_LINEAR;
    let argb = crate::DRM_FORMAT_ARGB8888;
    let cases: [(&str, Vec<u8>, u32); 7] = [
        (
            "a second plane",
            add_plane(4, 1, 256, linear),
            error::PLANE_IDX,
        ),
        (
            "plane 0 twice",
            [add_plane(4, 0, 256, linear), add_plane(4, 0, 256, linear)].concat(),
            error::PLANE_SET,
        ),
        ("no plane", create_immed(4, 5, 64, argb), error::INCOMPLETE),
        (
            "a format not offered",
            [
                add_plane(4, 0, 256, linear),
                create_immed(4, 5, 64, 0x3231_5659),
            ]
            .concat(),
            error::INVALID_FORMAT,
        ),
        (
            "a tiled modifier",
            [
                add_plane(4, 0, 256, 0x0100_0000_0000_0001),
                create_immed(4, 5, 64, argb),
            ]
            .concat(),
            error::INVALID_FORMAT,
        ),
        (
            "no size",
            [add_plane(4, 0, 256, linear), create_immed(4, 5, 0, argb)].concat(),
            error::INVALID_DIMENSIONS,
        ),
        (
            "a stride shorter than a row",
            [add_plane(4, 0, 255, linear), create_immed(4, 5, 64, argb)].concat(),
            error::OUT_OF_BOUNDS,
        ),
    ];
    for (what, requests, code) in cases {
        let (mut client, _) = dmabuf_client();
        let mut bytes = create_params(4);
        bytes.extend(requests);
        let _ = client.read(&bytes, &[Fd(9), Fd(10)]);
        assert!(
            matches!(client.fatal(), Some(Fatal::Interface { code: got, .. }) if *got == code),
            "{what}: {:?}",
            client.fatal()
        );
    }
    // A parameters object makes one buffer.
    let (mut client, _) = dmabuf_client();
    let mut bytes = create_params(4);
    bytes.extend(add_plane(4, 0, 256, linear));
    bytes.extend(create_immed(4, 5, 64, argb));
    bytes.extend(create_immed(4, 6, 64, argb));
    let _ = client.read(&bytes, &[Fd(9)]);
    assert!(matches!(
        client.fatal(),
        Some(Fatal::Interface { code, .. }) if *code == error::ALREADY_USED
    ));
}

/// NVIDIA's block-linear modifiers for `B8G8R8A8`, as its EGL reports them
/// (580.173.02): six plain, six compressed, and linear for external
/// textures only.
fn nvidia_reported() -> Vec<crate::Reported> {
    let mut reported: Vec<crate::Reported> = (0x10..=0x15_u64)
        .flat_map(|low| [0x0300_0000_0060_6000 | low, 0x0300_0000_00e0_8000 | low])
        .map(|modifier| crate::Reported {
            modifier,
            external_only: false,
        })
        .collect();
    reported.push(crate::Reported {
        modifier: crate::MOD_LINEAR,
        external_only: true,
    });
    reported
}

/// What is offered follows how a buffer is taken in: the render node's own
/// way has linear and the implicit layout, a mapping linear alone, and a
/// GPU renderer's EGL what it reported for 2D textures and then linear --
/// never the implicit layout, never an external-only one, none twice.
#[test]
fn what_is_offered_is_what_the_importer_takes() {
    use crate::{MOD_INVALID, MOD_LINEAR, Reported, Taken, modifiers_offered};
    assert_eq!(modifiers_offered(Taken::Node), [MOD_LINEAR, MOD_INVALID]);
    assert_eq!(modifiers_offered(Taken::Mapped), [MOD_LINEAR]);

    let reported = nvidia_reported();
    let offered = modifiers_offered(Taken::Sampled(&reported));
    assert_eq!(offered.len(), 13);
    assert_eq!(offered.last(), Some(&MOD_LINEAR));
    assert_eq!(offered.first(), Some(&0x0300_0000_0060_6010));
    assert!(offered.contains(&0x0300_0000_00e0_8015));
    assert!(!offered.contains(&MOD_INVALID));

    // An EGL that reports nothing, or cannot be asked, leaves linear.
    assert_eq!(modifiers_offered(Taken::Sampled(&[])), [MOD_LINEAR]);
    // External-only, the implicit layout, and a second mention are left out;
    // linear is offered once however it was reported.
    let odd = [
        Reported {
            modifier: 7,
            external_only: true,
        },
        Reported {
            modifier: MOD_INVALID,
            external_only: false,
        },
        Reported {
            modifier: MOD_LINEAR,
            external_only: false,
        },
        Reported {
            modifier: 9,
            external_only: false,
        },
        Reported {
            modifier: 9,
            external_only: false,
        },
    ];
    assert_eq!(modifiers_offered(Taken::Sampled(&odd)), [9, MOD_LINEAR]);

    // More than a bind may announce: the first ones, and still linear.
    let many: Vec<Reported> = (1..=100_u64)
        .map(|modifier| Reported {
            modifier,
            external_only: false,
        })
        .collect();
    let offered = modifiers_offered(Taken::Sampled(&many));
    assert_eq!(offered.len(), crate::MODIFIERS_OFFERED_MOST);
    assert_eq!(offered.first(), Some(&1));
    assert_eq!(offered.last(), Some(&MOD_LINEAR));
}

/// A bind announces exactly the modifiers the compositor said it offers,
/// with each format and in its order; linear and the implicit layout when
/// it said nothing.
#[test]
fn a_bind_announces_the_modifiers_offered() {
    use crate::{DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888, MOD_INVALID, MOD_LINEAR};
    let each = |modifiers: &[u64]| -> Vec<(u32, u64)> {
        [DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888]
            .into_iter()
            .flat_map(|format| modifiers.iter().map(move |modifier| (format, *modifier)))
            .collect()
    };
    let mut unsaid = dmabuf_client_unread(None);
    assert_eq!(
        modifiers_told(&mut unsaid),
        each(&[MOD_LINEAR, MOD_INVALID])
    );

    let mapped = crate::modifiers_offered(crate::Taken::Mapped);
    let mut client = dmabuf_client_unread(Some(&mapped));
    assert_eq!(modifiers_told(&mut client), each(&[MOD_LINEAR]));

    let reported = nvidia_reported();
    let sampled = crate::modifiers_offered(crate::Taken::Sampled(&reported));
    let mut client = dmabuf_client_unread(Some(&sampled));
    let told = modifiers_told(&mut client);
    assert_eq!(told, each(&sampled));
    assert!(told.iter().all(|(_, modifier)| *modifier != MOD_INVALID));
}

/// A buffer with a modifier that was offered is taken to the import; one
/// that was not is the protocol's `invalid_format`, as a tiled one is where
/// only linear is offered.
#[test]
fn a_modifier_is_taken_only_where_it_was_offered() {
    let block_linear = 0x0300_0000_0060_6014_u64;
    let reported = nvidia_reported();
    let sampled = crate::modifiers_offered(crate::Taken::Sampled(&reported));
    let mapped = crate::modifiers_offered(crate::Taken::Mapped);
    for (offered, taken) in [(&sampled, true), (&mapped, false)] {
        let (mut client, _) = dmabuf_client_offered(Some(offered));
        let mut bytes = create_params(4);
        bytes.extend(add_plane(4, 0, 256, block_linear));
        bytes.extend(create_immed(4, 5, 64, crate::DRM_FORMAT_ARGB8888));
        assert_eq!(client.read(&bytes, &[Fd(9)]), bytes.len());
        let created = client.take_events().into_iter().any(|event| {
            matches!(event, Event::DmabufCreated { dmabuf } if dmabuf.plane.modifier == block_linear)
        });
        assert_eq!(created, taken);
        if taken {
            assert_eq!(client.fatal(), None);
        } else {
            assert!(matches!(
                client.fatal(),
                Some(Fatal::Interface { code, .. })
                    if *code == zwp_linux_buffer_params_v1::error::INVALID_FORMAT
            ));
        }
    }
}

/// A buffer of an offered modifier that the importer then would not take is
/// a `create` answered `failed`, and nothing else: the connection goes on,
/// and the next buffer is made.
#[test]
fn a_failed_import_of_an_offered_modifier_leaves_the_connection_usable() {
    let block_linear = 0x0300_0000_0060_6014_u64;
    let reported = nvidia_reported();
    let sampled = crate::modifiers_offered(crate::Taken::Sampled(&reported));
    let (mut client, _) = dmabuf_client_offered(Some(&sampled));
    let create = |params: u32| {
        let mut bytes = create_params(params);
        bytes.extend(add_plane(params, 0, 256, block_linear));
        bytes.extend(request(
            params,
            zwp_linux_buffer_params_v1::request::CREATE,
            &[ArgType::Int, ArgType::Int, ArgType::Uint, ArgType::Uint],
            &[
                Arg::Int(64),
                Arg::Int(32),
                Arg::Uint(crate::DRM_FORMAT_ARGB8888),
                Arg::Uint(0),
            ],
        ));
        bytes
    };
    let bytes = create(4);
    assert_eq!(client.read(&bytes, &[Fd(9)]), bytes.len());
    client.dmabuf_imported(ObjectId(4), false);
    assert_eq!(
        sent(&mut client),
        [(ObjectId(4), zwp_linux_buffer_params_v1::event::FAILED)]
    );
    assert_eq!(client.fatal(), None);
    // Answered once: a second answer for the same parameters says nothing.
    client.dmabuf_imported(ObjectId(4), false);
    assert!(sent(&mut client).is_empty());
    let bytes = create(6);
    assert_eq!(client.read(&bytes, &[Fd(10)]), bytes.len());
    client.dmabuf_imported(ObjectId(6), true);
    assert_eq!(
        sent(&mut client),
        [(ObjectId(6), zwp_linux_buffer_params_v1::event::CREATED)]
    );
    assert_eq!(client.fatal(), None);
}
