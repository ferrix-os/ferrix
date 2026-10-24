//! A machine paired with a receiver, end to end on 127.0.0.1: uploads
//! kept once; a wrong CA, an expired certificate and an unpaired machine
//! refused; keys written 0600 and never printed; and the provider against
//! rustls's own ring provider, both ways.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ferrix_lode_core::format::{Output, write};
use ferrix_lode_core::record::Record;
use ferrix_lode_core::time::Zone;
use ferrix_lode_core::upload::{self, Format};
use lode_net::pki::Pki;
use lode_net::receive::{self, Receiver};
use lode_net::store;

/// A directory of its own for one test, under the target directory.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A receiver on 127.0.0.1, its port.
fn start(root: &Path, unpaired: bool) -> u16 {
    let pki = Pki::at(root);
    let (cert, key) = pki.server();
    let tls = lode_net::tls::server(&cert, &key, &pki.ca(), unpaired).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let receiver = Arc::new(Receiver::new(root));
    let _ = std::thread::spawn(move || receive::serve(&receiver, &listener, &tls));
    port
}

fn records(from: u64, count: u64) -> Vec<Record> {
    (from..from + count)
        .map(|seq| {
            let mut record = Record::new(6, 1_761_228_062_000_000 + seq, seq);
            record.seqnum = seq;
            record.boot_id = [3; 16];
            record.host = std::rc::Rc::from("box");
            record.push("MESSAGE", format!("line {seq}").as_bytes());
            record
        })
        .collect()
}

fn post(port: u16, tls: &Arc<rustls::ClientConfig>, records: &[Record]) -> Result<(), String> {
    let line = format!("r https://localhost:{port}/upload export ca=/unused");
    let destination = upload::parse_config(&line).unwrap().remove(0);
    let mut body = Vec::new();
    for record in records {
        write(record, Output::Export, &Zone::utc(), &mut body);
    }
    let request = upload::request(
        &destination.url,
        Format::Export,
        &body,
        "0123456789abcdef0123456789abcdef",
        "box",
    );
    lode_net::upload::send(&destination, Some(tls), &request)
}

fn stored(root: &Path) -> Vec<u64> {
    let dir = root.join("0123456789abcdef0123456789abcdef");
    store::segments(&[dir])
        .iter()
        .flat_map(store::Segment::records)
        .map(|record| record.seqnum)
        .collect()
}

#[test]
fn a_paired_machine_s_records_are_kept_once() {
    let root = scratch("kept-once");
    let paired = Pki::at(&root)
        .pair("box", &["localhost".to_owned()])
        .unwrap();
    let port = start(&root, false);
    let client =
        lode_net::tls::client(&paired.ca, Some((&paired.client_cert, &paired.client_key))).unwrap();
    post(port, &client, &records(1, 3)).unwrap();
    // The answer lost, the same sent again, and the next with it.
    post(port, &client, &records(1, 5)).unwrap();
    assert_eq!(stored(&root), [1, 2, 3, 4, 5]);
    let label =
        std::fs::read_to_string(root.join("0123456789abcdef0123456789abcdef/machine")).unwrap();
    assert!(
        label.contains("hostname=box\n") && label.contains("paired=box\n"),
        "{label}"
    );
    // Pairing again changes nothing that was made.
    let again = Pki::at(&root)
        .pair("box", &["localhost".to_owned()])
        .unwrap();
    assert_eq!(again.server_fingerprint, paired.server_fingerprint);
    assert_eq!(
        std::fs::read(&again.client_key).unwrap(),
        std::fs::read(&paired.client_key).unwrap()
    );
}

#[test]
fn a_wrong_ca_an_expired_or_an_unpaired_machine_is_refused() {
    let root = scratch("refused");
    let paired = Pki::at(&root)
        .pair("box", &["localhost".to_owned()])
        .unwrap();
    let port = start(&root, false);
    // No client certificate.
    let unpaired = lode_net::tls::client(&paired.ca, None).unwrap();
    assert!(post(port, &unpaired, &records(1, 1)).is_err());
    // Another receiver's CA: the server is not trusted.
    let other = scratch("refused-other");
    let stranger = Pki::at(&other)
        .pair("box", &["localhost".to_owned()])
        .unwrap();
    let wrong = lode_net::tls::client(
        &stranger.ca,
        Some((&paired.client_cert, &paired.client_key)),
    )
    .unwrap();
    assert!(post(port, &wrong, &records(1, 1)).is_err());
    // A certificate from another CA: the client is not trusted.
    let foreign = lode_net::tls::client(
        &paired.ca,
        Some((&stranger.client_cert, &stranger.client_key)),
    )
    .unwrap();
    assert!(post(port, &foreign, &records(1, 1)).is_err());
    // An expired one from the right CA.
    let (expired_cert, expired_key) = lode_net::pki::expired_client_for_tests(&root).unwrap();
    let expired = lode_net::tls::client(&paired.ca, Some((&expired_cert, &expired_key))).unwrap();
    assert!(post(port, &expired, &records(1, 1)).is_err());
    assert!(stored(&root).is_empty());
}

#[test]
fn keys_are_0600_and_never_printed() {
    let root = scratch("keys");
    let paired = Pki::at(&root)
        .pair("box", &["localhost".to_owned()])
        .unwrap();
    for key in [
        paired.client_key.clone(),
        root.join(".pki/ca.key"),
        root.join(".pki/server.key"),
    ] {
        let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", key.display());
    }
    let mode = std::fs::metadata(root.join(".pki"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
    // What `pair` prints, as the viewer reads it: no key's bytes.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lode-receive"))
        .args([
            "pair",
            "--machine",
            "box",
            "--server-name",
            "localhost",
            "--json",
            "--root",
        ])
        .arg(&root)
        .output()
        .unwrap();
    assert!(output.status.success());
    let printed = String::from_utf8(output.stdout).unwrap();
    assert!(printed.contains("\"remote_config_line\""), "{printed}");
    for key in [
        paired.client_key,
        root.join(".pki/ca.key"),
        root.join(".pki/server.key"),
    ] {
        let text = std::fs::read_to_string(&key).unwrap();
        let body: String = text
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect();
        assert!(!printed.contains(&body), "{} printed", key.display());
    }
}

#[test]
fn our_provider_and_ring_s_talk_both_ways() {
    let root = scratch("interop");
    let paired = Pki::at(&root)
        .pair("box", &["localhost".to_owned()])
        .unwrap();
    let pki = Pki::at(&root);
    let (cert, key) = pki.server();
    let ring = Arc::new(rustls::crypto::ring::default_provider());
    // A ring client against our server.
    let port = start(&root, false);
    let client = rustls::ClientConfig::builder_with_provider(Arc::clone(&ring))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(lode_net::tls::roots(&paired.ca).unwrap())
        .with_client_auth_cert(
            lode_net::tls::certificates(&paired.client_cert).unwrap(),
            lode_net::tls::private_key(&paired.client_key).unwrap(),
        )
        .unwrap();
    post(port, &Arc::new(client), &records(1, 2)).unwrap();
    assert_eq!(stored(&root), [1, 2]);
    // Our client against a ring server.
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(lode_net::tls::roots(&paired.ca).unwrap()),
        Arc::clone(&ring),
    )
    .build()
    .unwrap();
    let server = rustls::ServerConfig::builder_with_provider(ring)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            lode_net::tls::certificates(&cert).unwrap(),
            lode_net::tls::private_key(&key).unwrap(),
        )
        .unwrap();
    let other = scratch("interop-ring");
    std::fs::create_dir_all(other.join(".pki/clients")).unwrap();
    let _ = std::fs::copy(&paired.client_cert, other.join(".pki/clients/box.pem")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let ring_port = listener.local_addr().unwrap().port();
    let receiver = Arc::new(Receiver::new(&other));
    let server = Arc::new(server);
    let _ = std::thread::spawn(move || receive::serve(&receiver, &listener, &server));
    let ours =
        lode_net::tls::client(&paired.ca, Some((&paired.client_cert, &paired.client_key))).unwrap();
    post(ring_port, &ours, &records(1, 2)).unwrap();
    assert_eq!(stored(&other), [1, 2]);
}
