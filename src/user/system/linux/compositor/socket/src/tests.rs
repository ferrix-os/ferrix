//! The socket, against itself: a pair of connections carrying bytes and
//! descriptors.
//!
//! These run on the development host and are the one part of the compositor
//! whose behaviour on Ferrix is a different question from its behaviour here:
//! what they check is that the framing is right, and Ferrix's own
//! `AF_UNIX` tests check that the kernel carries it.

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use compositor_wire::Fd;

use crate::{Connection, Listener, ListenerError, MAX_QUEUED, RecvError, SendError, socket_path};

/// A pair of connected sockets.
fn pair() -> (Connection, Connection) {
    let (a, b) = UnixStream::pair().expect("a socket pair");
    (
        Connection::new(a).expect("a connection"),
        Connection::new(b).expect("a connection"),
    )
}

/// A descriptor with known contents, so the far side can prove it is the
/// same open file and not merely the same number.
fn readable(contents: &[u8]) -> OwnedFd {
    use std::io::Write;
    let (read, mut write) = UnixStream::pair().expect("a socket pair");
    write.write_all(contents).expect("written");
    // Closing the writing end makes the reading end give end-of-file after
    // the bytes, so `read_to_end` on the far side returns.
    drop(write);
    OwnedFd::from(read)
}

#[test]
fn bytes_arrive_and_are_consumed_a_message_at_a_time() {
    let (mut a, mut b) = pair();
    a.send(b"hello wayland", &[]).expect("sent");
    assert!(!a.has_pending_writes());

    let read = b.receive().expect("read");
    assert_eq!(read, 13);
    assert_eq!(b.bytes(), b"hello wayland");

    // The server takes the first word and leaves the rest.
    b.consume(6, 0);
    assert_eq!(b.bytes(), b"wayland");
    b.consume(7, 0);
    assert!(b.bytes().is_empty());
}

#[test]
fn a_descriptor_arrives_beside_the_bytes_and_is_the_same_open_file() {
    let (mut a, mut b) = pair();
    let carried = readable(b"the other side");
    a.send(b"msg", &[Fd(carried.as_raw_fd())]).expect("sent");

    let _ = b.receive().expect("read");
    assert_eq!(b.bytes(), b"msg");
    let fds = b.fds();
    assert_eq!(fds.len(), 1, "one descriptor arrived");
    assert_ne!(
        fds[0].0,
        carried.as_raw_fd(),
        "a received descriptor is a new number in this process"
    );

    // It is the same open file: the bytes written before it was sent are
    // still there to read.
    #[expect(
        unsafe_code,
        reason = "AUDIT: a test taking the descriptor the connection is about to hand on, so it can prove it is the file that was sent"
    )]
    // SAFETY: `fds[0]` is a descriptor `receive` owns and `consume` is about
    // to hand on; nothing else holds it.
    let mut got = unsafe { UnixStream::from_raw_fd(fds[0].0) };
    let mut text = Vec::new();
    let _ = got.read_to_end(&mut text);
    assert_eq!(text, b"the other side");

    // `consume` hands it on rather than closing it, which the read above
    // already relied on.
    b.consume(3, 1);
    assert!(b.fds().is_empty());
    core::mem::forget(got);
}

#[test]
fn several_descriptors_arrive_in_the_order_they_were_sent() {
    let (mut a, mut b) = pair();
    let first = readable(b"first");
    let second = readable(b"second");
    a.send(b"two", &[Fd(first.as_raw_fd()), Fd(second.as_raw_fd())])
        .expect("sent");

    let _ = b.receive().expect("read");
    let fds = b.fds();
    assert_eq!(fds.len(), 2);
    for (fd, expected) in fds.iter().zip([&b"first"[..], &b"second"[..]]) {
        #[expect(
            unsafe_code,
            reason = "AUDIT: as in the test above, reading the file the descriptor names to show the order was kept"
        )]
        // SAFETY: the connection owns these and has not handed them on.
        let mut got = unsafe { UnixStream::from_raw_fd(fd.0) };
        let mut text = Vec::new();
        let _ = got.read_to_end(&mut text);
        assert_eq!(text, expected);
        core::mem::forget(got);
    }
}

/// Everything `b` can read now, bytes and descriptors, taken.
fn drain(b: &mut Connection) -> (Vec<u8>, Vec<OwnedFd>) {
    let mut bytes = Vec::new();
    let mut fds = Vec::new();
    loop {
        match b.receive() {
            Ok(_) => {}
            Err(RecvError::WouldBlock) => break,
            Err(error) => panic!("reading: {error:?}"),
        }
        bytes.extend_from_slice(b.bytes());
        for fd in b.fds() {
            #[expect(
                unsafe_code,
                reason = "AUDIT: a test taking the descriptors the connection hands on at `consume`"
            )]
            // SAFETY: `consume` below hands these on without closing them, so
            // this is the one owner.
            fds.push(unsafe { OwnedFd::from_raw_fd(fd.0) });
        }
        let (read, claimed) = (b.bytes().len(), b.fds().len());
        b.consume(read, claimed);
    }
    (bytes, fds)
}

/// A client whose socket is full is busy, not gone: what the socket would
/// not take waits, and arrives, in order, once the client reads.
///
/// A full socket used to be an error, and the compositor dropped the client
/// for it: Chrome, flooded by a drag, went with the whole browser.
#[test]
fn a_full_socket_keeps_the_rest_for_later() {
    let (mut a, mut b) = pair();
    let chunk: Vec<u8> = (0..=255u8).cycle().take(64 * 1024).collect();
    let mut sent = Vec::new();
    while !a.has_pending_writes() {
        a.send(&chunk, &[]).expect("a full socket is not an error");
        sent.extend_from_slice(&chunk);
    }
    // One more, onto the queue.
    a.send(&chunk, &[]).expect("queued");
    sent.extend_from_slice(&chunk);

    let mut got = Vec::new();
    while a.has_pending_writes() {
        got.extend(drain(&mut b).0);
        a.flush().expect("flushed");
    }
    got.extend(drain(&mut b).0);
    assert_eq!(got.len(), sent.len());
    assert!(got == sent, "every byte, in the order it was sent");
}

/// A descriptor sent while the socket is full waits with its bytes, and is
/// the same open file when it arrives -- though the caller closed its own at
/// once, as the clipboard does with the pipe it hands on.
#[test]
fn a_descriptor_behind_a_full_socket_still_arrives() {
    let (mut a, mut b) = pair();
    let filler = vec![0u8; 64 * 1024];
    while !a.has_pending_writes() {
        a.send(&filler, &[]).expect("filling");
    }
    let carried = readable(b"after the wait");
    a.send(b"fd!", &[Fd(carried.as_raw_fd())]).expect("queued");
    drop(carried);

    let mut fds = Vec::new();
    let mut bytes = Vec::new();
    loop {
        let (more, arrived) = drain(&mut b);
        bytes.extend(more);
        fds.extend(arrived);
        if !a.has_pending_writes() {
            break;
        }
        a.flush().expect("flushed");
    }
    let (more, arrived) = drain(&mut b);
    bytes.extend(more);
    fds.extend(arrived);
    assert!(bytes.ends_with(b"fd!"));
    assert_eq!(fds.len(), 1, "the descriptor came with its bytes");
    let mut text = Vec::new();
    let _ = UnixStream::from(fds.remove(0)).read_to_end(&mut text);
    assert_eq!(text, b"after the wait");
}

/// A client that never reads is given up on once more than [`MAX_QUEUED`]
/// waits for it, as Hyprland gives up at the same size.
#[test]
fn a_client_that_stops_reading_is_given_up_on() {
    let (mut a, _b) = pair();
    let chunk = vec![7u8; 64 * 1024];
    let mut offered = 0usize;
    let error = loop {
        match a.send(&chunk, &[]) {
            Ok(()) => offered += chunk.len(),
            Err(error) => break error,
        }
        assert!(offered <= 64 * MAX_QUEUED, "the queue never filled");
    };
    assert!(
        matches!(error, SendError::Overflow(waiting) if waiting > MAX_QUEUED),
        "{error:?}"
    );
}

#[test]
fn a_read_with_nothing_waiting_would_block_rather_than_ending_the_connection() {
    let (_a, mut b) = pair();
    assert!(matches!(b.receive(), Err(RecvError::WouldBlock)));
}

#[test]
fn a_closed_socket_is_told_apart_from_an_empty_one() {
    let (a, mut b) = pair();
    drop(a);
    assert!(matches!(b.receive(), Err(RecvError::Closed)));
}

#[test]
fn a_message_that_arrives_in_pieces_is_put_back_together() {
    let (mut a, mut b) = pair();
    a.send(b"half", &[]).expect("sent");
    let _ = b.receive().expect("read");
    assert_eq!(b.bytes(), b"half");
    a.send(b"-and-half", &[]).expect("sent");
    let _ = b.receive().expect("read");
    assert_eq!(b.bytes(), b"half-and-half");
}

#[test]
fn the_socket_path_follows_wayland_s_rule() {
    // A name with a slash is a path; anything else is joined to
    // XDG_RUNTIME_DIR, as `wl_display_connect` does it.
    let absolute = socket_path("/tmp/wayland-test").expect("a path");
    assert_eq!(absolute.to_str(), Some("/tmp/wayland-test"));
    let nested = socket_path("/run/user/1000/wayland-9").expect("a path");
    assert_eq!(nested.to_str(), Some("/run/user/1000/wayland-9"));

    // The relative case reads the environment, which a test cannot set
    // without racing every other thread in the process. So it is checked
    // against whatever the environment says rather than by setting it: the
    // rule under test is the joining, not the variable's value.
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => {
            let relative = socket_path("wayland-1").expect("a path");
            assert_eq!(relative, std::path::Path::new(&runtime).join("wayland-1"));
        }
        None => {
            assert!(
                matches!(socket_path("wayland-1"), Err(ListenerError::NoRuntimeDir)),
                "with no XDG_RUNTIME_DIR there is nowhere to put the socket"
            );
        }
    }
}

#[test]
fn a_listener_makes_its_socket_and_takes_it_away_again() {
    let directory =
        std::env::temp_dir().join(format!("ferrix-compositor-socket-{}", std::process::id()));
    let path = directory.join("wayland-test");
    let name = path.to_str().expect("a utf-8 path").to_owned();

    let listener = Listener::bind(&name).expect("a socket");
    assert!(path.exists(), "the socket file is there while it is bound");
    assert_eq!(listener.path(), path);
    // Nothing is waiting on a fresh socket, and asking does not block.
    assert!(listener.accept().expect("accept").is_none());

    // A client connects and the listener hands it over.
    let client = UnixStream::connect(&path).expect("connect");
    let accepted = listener.accept().expect("accept");
    assert!(accepted.is_some(), "the connection was there to take");
    drop(client);

    // Binding the same name again takes over the stale file rather than
    // failing, which is what a compositor restarted after a crash needs.
    let again = Listener::bind(&name).expect("a socket again");
    assert!(path.exists());
    drop(again);
    drop(listener);
    assert!(!path.exists(), "a socket file left behind serves nothing");
    let _ = std::fs::remove_dir_all(&directory);
}

/// A sender on another thread writes whole messages into the connection's
/// own queue: nothing of one lands inside the other's, and what each sent
/// arrives in the order it sent it.
#[test]
fn a_sender_on_another_thread_keeps_messages_whole() {
    let (mut a, mut b) = pair();
    let sender = a.sender();
    let other = std::thread::spawn(move || {
        for _ in 0..500 {
            sender.send(b"[sender message]").expect("sent");
        }
    });
    for _ in 0..500 {
        a.send(b"(connection message)", &[]).expect("sent");
    }
    other.join().expect("the sender's thread");
    while a.has_pending_writes() {
        let _ = b.receive();
        a.flush().expect("flushed");
    }
    let mut got = Vec::new();
    loop {
        match b.receive() {
            Ok(_) => {}
            Err(RecvError::WouldBlock) => break,
            Err(error) => panic!("reading: {error:?}"),
        }
    }
    got.extend_from_slice(b.bytes());
    let text = String::from_utf8(got).expect("text");
    let rest = text
        .replace("[sender message]", "")
        .replace("(connection message)", "");
    assert_eq!(rest, "", "a message was cut by another");
    assert_eq!(text.matches("[sender message]").count(), 500);
    assert_eq!(text.matches("(connection message)").count(), 500);
}
