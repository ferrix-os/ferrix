//! Tests for `netboot`: what it reads off an image, the scripts it writes,
//! the stick's layout and the server, each without iPXE or QEMU.

use std::io::{Read, Write};
use std::net::TcpStream;

use super::*;

fn files() -> BTreeMap<String, Vec<u8>> {
    [
        ("EFI/BOOT/BOOTX64.EFI", b"loader".to_vec()),
        ("FERRIX/CMDLINE.TXT", b"ferrix.live".to_vec()),
        ("FERRIX/INITRD.IMG", vec![7; 3000]),
        ("FERRIX/KERNEL.ELF", vec![9; 70_000]),
    ]
    .into_iter()
    .map(|(path, bytes)| (path.to_owned(), bytes))
    .collect()
}

#[test]
fn the_files_come_back_off_the_image_as_they_were_written() {
    let mut fat = fat::Fat32::new(40 << 20).unwrap();
    for (path, bytes) in &files() {
        fat.add_file(path, bytes).unwrap();
    }
    fat.add_file("EMPTY.TXT", b"").unwrap();
    let directory = std::env::temp_dir().join(format!("nb-volume-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let image = directory.join("image.img");
    std::fs::write(&image, fat.finish()).unwrap();
    let read = volume::files(&image).unwrap();
    std::fs::remove_dir_all(&directory).unwrap();
    let mut wanted = files();
    let _ = wanted.insert("EMPTY.TXT".to_owned(), Vec::new());
    assert_eq!(read, wanted, "every file, and only them, byte for byte");
}

#[test]
fn boot_script_fetches_each_file_by_its_volume_path_and_starts_the_loader_last() {
    let script = boot_script(&files(), "abc123").unwrap();
    assert!(script.starts_with("#!ipxe\n"), "{script}");
    assert!(
        script.contains("echo Ferrix netboot: commit abc123\n"),
        "{script}"
    );
    assert!(
        script.contains("imgfetch --name FERRIX\\KERNEL.ELF FERRIX/KERNEL.ELF || goto failed\n"),
        "{script}"
    );
    assert!(script.contains("pciscan pcidev"), "{script}");
    let chain = script
        .find("chain --name EFI\\BOOT\\BOOTX64.EFI EFI/BOOT/BOOTX64.EFI")
        .unwrap();
    for name in ["KERNEL.ELF", "INITRD.IMG", "CMDLINE.TXT"] {
        assert!(
            script.find(name).unwrap() < chain,
            "{name} before the loader"
        );
    }
    assert!(
        !script.contains("imgfetch --name EFI"),
        "the loader is chained, not fetched"
    );
}

#[test]
fn boot_script_refuses_an_image_without_a_loader() {
    let mut files = files();
    let _ = files.remove("EFI/BOOT/BOOTX64.EFI");
    assert!(boot_script(&files, "x").is_err(), "no loader, no script");
}

#[test]
fn the_embedded_script_retries_and_names_the_server() {
    let script = embedded_script("10.0.2.2:8086");
    assert!(script.contains("chain http://10.0.2.2:8086/ferrix/boot.ipxe || goto failed"));
    assert!(script.contains("dhcp || goto failed"), "{script}");
    assert!(script.ends_with("goto retry\n"), "{script}");
}

#[test]
fn server_is_a_host_and_a_port() {
    assert_eq!(server_of("192.168.8.74").unwrap(), "192.168.8.74:8086");
    assert_eq!(server_of("nazuna.lan:9000").unwrap(), "nazuna.lan:9000");
    for wrong in ["", ":80", "a b:80", "host:0", "host:http", "x;shell"] {
        assert!(server_of(wrong).is_err(), "{wrong:?}");
    }
}

#[test]
fn listen_takes_an_address_or_a_port() {
    assert_eq!(
        listen_of(Some("127.0.0.1:9")).unwrap(),
        "127.0.0.1:9".parse().unwrap()
    );
    assert!(
        listen_of(Some("nonsense")).is_err(),
        "neither address nor port"
    );
}

#[test]
fn the_stick_is_a_partition_table_with_the_volume_at_one_mebibyte() {
    let fat = fat::Fat32::new(STICK_BYTES).unwrap().finish();
    let medium = lay_out(fat.clone()).unwrap();
    assert_eq!(medium.len() % (1 << 20), 0, "whole MiB");
    assert_eq!(&medium[512..520], b"EFI PART", "the primary GPT header");
    let at = (STICK_FIRST * SECTOR) as usize;
    // The volume, with only its hidden-sector count changed.
    assert_eq!(&medium[at + 82..at + 90], b"FAT32   ");
    assert_eq!(
        u32::from_le_bytes(medium[at + 28..at + 32].try_into().unwrap()),
        STICK_FIRST as u32
    );
    let mut hidden = fat.clone();
    crate::installer::hide(&mut hidden, STICK_FIRST).unwrap();
    assert!(
        medium[at..at + fat.len()] == hidden[..],
        "the volume as written, its two boot sectors told where it starts"
    );
}

#[test]
fn a_request_head_is_read_for_method_path_and_keep_alive() {
    let head = http::parse(b"GET /ferrix/boot.ipxe?x=1 HTTP/1.1\r\nHost: h\r\n\r\n");
    assert_eq!(
        head,
        http::Head {
            method: "GET".to_owned(),
            path: "/ferrix/boot.ipxe".to_owned(),
            keep: true
        }
    );
    assert!(!http::parse(b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n").keep);
    assert!(!http::parse(b"GET / HTTP/1.0\r\n\r\n").keep);
}

/// One request on a fresh connection; the status and the body.
fn get(server: &http::Server, path: &str) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(server.address()).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap();
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer).unwrap();
    let end = answer.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&answer[..end]).into_owned();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, answer[end + 4..].to_vec())
}

#[test]
fn the_server_serves_its_files_under_its_prefix_and_logs_each_request() {
    let server = http::Server::start("127.0.0.1:0".parse().unwrap(), PREFIX, files()).unwrap();
    assert_eq!(
        get(&server, "/ferrix/FERRIX/KERNEL.ELF"),
        (200, vec![9; 70_000])
    );
    assert_eq!(get(&server, "/ferrix/FERRIX/NONE.ELF").0, 404);
    assert_eq!(
        get(&server, "/FERRIX/KERNEL.ELF").0,
        404,
        "outside the prefix"
    );
    assert_eq!(
        get(&server, "/ferrix/report/pci/0000:00:03.0/1af4:1041/020000").0,
        200
    );
    server.serve_under("/wrong/");
    assert_eq!(get(&server, "/ferrix/FERRIX/KERNEL.ELF").0, 404, "moved");
    assert_eq!(get(&server, "/wrong/FERRIX/KERNEL.ELF").0, 200);
    let requests = server.stop();
    let seen: Vec<(String, u16)> = requests.into_iter().map(|r| (r.path, r.status)).collect();
    assert_eq!(seen.len(), 6, "{seen:?}");
    assert_eq!(seen[0], ("/ferrix/FERRIX/KERNEL.ELF".to_owned(), 200));
    assert_eq!(seen[4], ("/ferrix/FERRIX/KERNEL.ELF".to_owned(), 404));
}

#[test]
fn the_server_keeps_a_connection_for_the_next_request() {
    let server = http::Server::start("127.0.0.1:0".parse().unwrap(), PREFIX, files()).unwrap();
    let mut stream = TcpStream::connect(server.address()).unwrap();
    write!(
        stream,
        "GET /ferrix/FERRIX/CMDLINE.TXT HTTP/1.1\r\n\r\n\
         GET /ferrix/FERRIX/CMDLINE.TXT HTTP/1.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer).unwrap();
    let text = String::from_utf8_lossy(&answer);
    assert_eq!(text.matches("HTTP/1.1 200 OK").count(), 2, "{text}");
    assert_eq!(text.matches("ferrix.live").count(), 2, "{text}");
    assert_eq!(server.stop().len(), 2, "both answered on one connection");
}

#[test]
fn the_negative_control_wants_an_ipxe_error_code() {
    let refused = [http::Request {
        path: "/ferrix/boot.ipxe".to_owned(),
        status: 404,
    }];
    let banner =
        vec!["iPXE 2.0.0 -- Open Source Network Boot Firmware -- https://ipxe.org".to_owned()];
    assert!(
        check_refused(&banner, &refused).is_err(),
        "the banner is not an error"
    );
    let startup = vec!["file:autoexec.ipxe... Not found (https://ipxe.org/7f4de18e)".to_owned()];
    assert!(
        check_refused(&startup, &refused).is_err(),
        "iPXE's look for autoexec.ipxe is not the script's error"
    );
    let error = vec![
        "http://10.0.2.2:8086/ferrix/boot.ipxe... Not found (https://ipxe.org/2d0c618e)".to_owned(),
    ];
    assert_eq!(
        check_refused(&error, &refused).unwrap(),
        "http://10.0.2.2:8086/ferrix/boot.ipxe... Not found (https://ipxe.org/2d0c618e)"
    );
    assert!(check_refused(&error, &[]).is_err(), "no 404, no control");
}
