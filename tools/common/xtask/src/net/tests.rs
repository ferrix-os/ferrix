//! What the host half of the networking test must get right before a guest
//! can be blamed for anything.

use std::net::{Ipv4Addr, UdpSocket};
use std::time::Duration;

use super::{
    BIG_BYTES, FAULT_MARKER, HELLO_BODY, NAME, NAME_ANSWER, PACED_BYTES, PACED_PATH, Servers,
    answer_query, big_body, cksum, commands, path_of, read_name, restarted, text_body,
};

/// The values the host's own `cksum` prints for these inputs. A digest this
/// build tool computes differently from the guest's `cksum` is a test that
/// fails on a transfer that was perfect.
#[test]
fn cksum_matches_the_one_posix_specifies() {
    assert_eq!(cksum(b""), 4_294_967_295);
    assert_eq!(cksum(b"a"), 1_220_704_766);
    assert_eq!(cksum(b"ferrix"), 216_309_028);
    assert_eq!(cksum(b"The quick brown fox"), 2_672_498_166);
}

#[test]
fn the_body_is_the_length_it_says() {
    let body = big_body();
    assert_eq!(body.len(), BIG_BYTES);
    assert!(body.is_ascii());
    // Made the same way every time, or the digest in the command list is a
    // digest of something else.
    assert_eq!(body, big_body());
}

#[test]
fn a_request_line_gives_up_its_path() {
    assert_eq!(
        path_of(b"GET /hello HTTP/1.1\r\nHost: x\r\n\r\n").as_deref(),
        Some("/hello")
    );
    assert_eq!(path_of(b"POST /hello HTTP/1.1\r\n\r\n"), None);
    assert_eq!(path_of(b""), None);
}

#[test]
fn a_name_is_read_out_of_its_labels() {
    let query = b"\x06ferrix\x04test\x00rest";
    let (name, after) = read_name(query, 0).unwrap();
    assert_eq!(name, "ferrix.test");
    assert_eq!(after, 13);
    // A label that runs off the end is not a name.
    assert_eq!(read_name(b"\x06fer", 0), None);
}

/// The one name gets an A record with the gateway in it; anything else gets a
/// refusal with no answer, so a guest that asked for the wrong name cannot
/// pass by reading a record that was not there.
#[test]
fn the_stub_answers_the_name_it_knows_and_nothing_else() {
    let mut query = vec![0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
    query.extend_from_slice(b"\x06ferrix\x04test\x00\x00\x01\x00\x01");
    let mut out = Vec::new();
    assert!(answer_query(&query, &mut out));
    assert_eq!(out.get(..2), Some([0x12, 0x34].as_slice()));
    assert_eq!(out.get(6..8), Some([0x00, 0x01].as_slice()));
    assert!(out.ends_with(&NAME_ANSWER.octets()));

    let mut other = vec![0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
    other.extend_from_slice(b"\x05other\x04test\x00\x00\x01\x00\x01");
    out.clear();
    assert!(answer_query(&other, &mut out));
    assert_eq!(out.get(6..8), Some([0x00, 0x00].as_slice()));
    // A name nobody has: NXDOMAIN.
    assert_eq!(out.get(3).map(|byte| byte & 0x0F), Some(3));

    // The name, asked for a record it has not got: no answers, but no
    // NXDOMAIN either, because the name exists. `getaddrinfo` asks A and AAAA
    // together and takes NXDOMAIN to either as the name not existing at all.
    let mut quad_a = vec![0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
    quad_a.extend_from_slice(b"\x06ferrix\x04test\x00\x00\x1C\x00\x01");
    out.clear();
    assert!(answer_query(&quad_a, &mut out));
    assert_eq!(out.get(6..8), Some([0x00, 0x00].as_slice()));
    assert_eq!(out.get(3).map(|byte| byte & 0x0F), Some(0));

    // A truncated question is dropped rather than answered.
    out.clear();
    assert!(!answer_query(&[0x12, 0x34], &mut out));
}

/// End to end on the host: the stubs really serve what the command list says
/// they serve. A guest that fails these commands has then failed for a reason
/// on its own side.
#[test]
fn the_stubs_serve_what_the_commands_expect() {
    let servers = Servers::start().unwrap();

    let mut fetched = String::new();
    let mut stream = std::net::TcpStream::connect(servers.http).unwrap();
    use std::io::{Read, Write};
    stream.write_all(b"GET /hello HTTP/1.0\r\n\r\n").unwrap();
    let _ = stream.read_to_string(&mut fetched).unwrap();
    assert!(fetched.ends_with(&format!("{HELLO_BODY}\n")), "{fetched}");

    let mut body = Vec::new();
    let mut stream = std::net::TcpStream::connect(servers.http).unwrap();
    stream.write_all(b"GET /big HTTP/1.0\r\n\r\n").unwrap();
    let _ = stream.read_to_end(&mut body).unwrap();
    assert!(body.ends_with(&big_body()));

    let asking = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    asking
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut query = vec![0xAB, 0xCD, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
    query.extend_from_slice(b"\x06ferrix\x04test\x00\x00\x01\x00\x01");
    let _ = asking.send_to(&query, servers.dns()).unwrap();
    let mut answer = [0_u8; 512];
    let (read, _) = asking.recv_from(&mut answer).unwrap();
    assert!(answer[..read].ends_with(&NAME_ANSWER.octets()));

    let _ = asking.send_to(b"ferrix", servers.udp).unwrap();
    let (read, _) = asking.recv_from(&mut answer).unwrap();
    assert_eq!(&answer[..read], b"ferrix-udp: ferrix");
}

/// Every command names a program, and the ones with a port in them got one.
#[test]
fn the_command_list_is_well_formed() {
    let servers = Servers::start().unwrap();
    let without = commands(&servers, false, false);
    let list = commands(&servers, true, true);
    assert!(!without.is_empty());
    assert!(
        without
            .iter()
            .all(|command| !command.argv.join(" ").contains("curl")),
        "no curl without curl in the image"
    );
    assert!(list.len() > without.len(), "curl adds its programs");
    for command in &list {
        assert!(!command.argv.is_empty());
        for arg in command.argv {
            assert!(!arg.is_empty());
            assert!(!arg.contains(":0/"), "a port of zero reached {arg:?}");
        }
    }
    let joined = list
        .iter()
        .flat_map(|command| command.argv.iter())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(joined.contains(NAME));
    // The address is the gateway's to give, by DHCP, not the list's to name.
    assert!(joined.contains("udhcpc -i eth0"));
    assert!(joined.contains(&format!("curl -sS http://{NAME}:")));
    // The clone over the network names the gateway and the port this
    // process is listening on, not a server in the guest.
    assert!(joined.contains("git clone -q http://10.0.2.2:"));
    assert!(joined.contains("/repo.git /tmp/fetched"));
    assert!(!joined.contains("10.0.2.15/24"));
}

/// The paced body is the text body, whole, and slow enough that a fault asked
/// for after its first quarter megabyte lands before its end.
#[test]
fn the_paced_body_arrives_whole_and_slowly() {
    use std::io::{Read, Write};
    let servers = Servers::start().unwrap();
    let started = std::time::Instant::now();
    let mut answer = Vec::new();
    let mut stream = std::net::TcpStream::connect(servers.http).unwrap();
    stream
        .write_all(format!("GET {PACED_PATH} HTTP/1.0\r\n\r\n").as_bytes())
        .unwrap();
    let _ = stream.read_to_end(&mut answer).unwrap();
    assert!(answer.ends_with(&text_body(PACED_BYTES)));
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

/// The restart program asks for the fault with the marker the driver knows,
/// and is last, so a dead network fails it alone.
#[test]
fn the_restart_program_is_last_and_asks_for_the_fault() {
    let servers = Servers::start().unwrap();
    let list = commands(&servers, false, false);
    let last = list.last().unwrap().argv.join(" ");
    assert!(
        last.contains(&format!("echo {FAULT_MARKER} | nc -u")),
        "{last}"
    );
    assert!(last.contains(PACED_PATH), "{last}");
    let driver = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../src/user/system/native/drivers/net/virtio-net/src/main.rs"
    ));
    assert!(
        driver.contains(&format!("b\"{FAULT_MARKER}\"")),
        "the driver's marker is not test-net's"
    );
}

/// The kernel's lines must show the driver ending because its ring was ended,
/// then its restart; either alone, or in the other order, is not enough.
#[test]
fn a_restart_is_the_end_then_the_start() {
    let lines = |text: &[&str]| {
        text.iter()
            .map(|line| (*line).to_owned())
            .collect::<Vec<_>>()
    };
    let ended = "  devmgr   the driver of 0x00000060 ended with status 9; the device is quiesced";
    let started = "  devmgr   the driver of 0x00000060 was started again and published (restart 1)";
    let other = "  devmgr   the driver of 0x00000060 ended with status 7; the device is quiesced";
    assert!(restarted(&lines(&[ended, started])).is_ok());
    assert!(restarted(&lines(&[started, ended])).is_err());
    assert!(restarted(&lines(&[ended])).is_err());
    assert!(restarted(&lines(&[other, started])).is_err());
}
