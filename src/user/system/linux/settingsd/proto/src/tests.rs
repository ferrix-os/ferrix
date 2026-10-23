//! Every record round-trips, every damaged packet is refused, and every
//! request rule has a case each way.

use crate::{
    DecodeError, MAX_RECORD, MAX_TEXT, Refusal, RefusalKind, Request, SWITCHABLE_UNITS, Secret,
    ToClient, ToServer, valid_hostname,
};

fn requests() -> Vec<Request> {
    vec![
        Request::SetHostname {
            name: "box".to_owned(),
        },
        Request::ServiceEnablePersistent {
            unit: "sshd.service".to_owned(),
            enabled: true,
        },
        Request::ServiceEnablePersistent {
            unit: "sshd.socket".to_owned(),
            enabled: false,
        },
    ]
}

fn replies() -> Vec<ToClient> {
    let mut all = vec![
        ToClient::Prompt {
            visible: false,
            text: "Password: ".to_owned(),
        },
        ToClient::Info("wait 16 s".to_owned()),
        ToClient::Done {
            changed: true,
            text: "the computer is called box (was ferrix)".to_owned(),
        },
    ];
    for kind in [
        RefusalKind::Invalid,
        RefusalKind::NotPermitted,
        RefusalKind::AuthenticationFailed,
        RefusalKind::Cancelled,
        RefusalKind::Unavailable,
        RefusalKind::Failed,
        RefusalKind::Unsupported,
    ] {
        all.push(ToClient::Refused(Refusal {
            kind,
            retry_after_ms: 2000,
            text: format!("{} here", kind.name()),
        }));
    }
    all
}

#[test]
fn requests_round_trip() {
    for dry_run in [false, true] {
        for request in requests() {
            let packet = ToServer::Request {
                dry_run,
                request: request.clone(),
            }
            .encode()
            .unwrap();
            match ToServer::decode(packet.bytes()).unwrap() {
                ToServer::Request {
                    dry_run: d,
                    request: r,
                } => assert_eq!((d, r), (dry_run, request)),
                other => panic!("{other:?}"),
            }
        }
    }
}

#[test]
fn every_request_name_decodes() {
    for request in requests() {
        assert!(Request::NAMES.contains(&request.name()));
    }
    assert_eq!(Request::NAMES.len(), 2, "a new request needs a case above");
}

#[test]
fn respond_and_cancel_round_trip() {
    let secret = Secret::from_bytes(b"horse battery").unwrap();
    let packet = ToServer::Respond(secret).encode().unwrap();
    match ToServer::decode(packet.bytes()).unwrap() {
        ToServer::Respond(back) => assert_eq!(back.expose(), b"horse battery"),
        other => panic!("{other:?}"),
    }
    let packet = ToServer::Cancel.encode().unwrap();
    assert!(matches!(
        ToServer::decode(packet.bytes()).unwrap(),
        ToServer::Cancel
    ));
}

#[test]
fn replies_round_trip_and_say_when_they_end() {
    for reply in replies() {
        let packet = reply.encode().unwrap();
        let back = ToClient::decode(packet.bytes()).unwrap();
        assert_eq!(back.is_final(), reply.is_final());
        assert_eq!(back, reply);
    }
}

#[test]
fn every_cut_and_every_extra_byte_is_refused() {
    let mut packets: Vec<Vec<u8>> = requests()
        .into_iter()
        .map(|request| {
            ToServer::Request {
                dry_run: false,
                request,
            }
            .encode()
            .unwrap()
            .bytes()
            .to_vec()
        })
        .collect();
    packets.extend(
        replies()
            .into_iter()
            .map(|reply| reply.encode().unwrap().bytes().to_vec()),
    );
    for packet in packets {
        for cut in 0..packet.len() {
            let short = packet.get(..cut).unwrap();
            assert!(
                ToServer::decode(short).is_err() && ToClient::decode(short).is_err()
                    || cut == 0 && short.is_empty(),
                "{short:?}"
            );
        }
        let mut long = packet.clone();
        long.push(0);
        assert!(
            matches!(
                (ToServer::decode(&long), ToClient::decode(&long)),
                (Err(_), Err(_))
            ),
            "{long:?}"
        );
    }
}

#[test]
fn damaged_fields_are_refused() {
    let good = ToServer::Request {
        dry_run: true,
        request: Request::SetHostname {
            name: "box".to_owned(),
        },
    }
    .encode()
    .unwrap()
    .bytes()
    .to_vec();
    // kind, version, flag, name...
    let mut version = good.clone();
    *version.get_mut(1).unwrap() = 2;
    assert_eq!(
        ToServer::decode(&version).unwrap_err(),
        DecodeError::Version
    );
    let mut flag = good.clone();
    *flag.get_mut(2).unwrap() = 2;
    assert_eq!(ToServer::decode(&flag).unwrap_err(), DecodeError::Flag);
    let mut kind = good.clone();
    *kind.get_mut(0).unwrap() = 99;
    assert_eq!(ToServer::decode(&kind).unwrap_err(), DecodeError::Kind);
    assert_eq!(ToServer::decode(&[]).unwrap_err(), DecodeError::Length);
    assert_eq!(
        ToServer::decode(&vec![1; MAX_RECORD + 1]).unwrap_err(),
        DecodeError::Length
    );
    // An unknown request name.
    let mut unknown = vec![1, crate::VERSION, 0];
    unknown.extend_from_slice(&(8_u16).to_le_bytes());
    unknown.extend_from_slice(b"rm-rf-me");
    assert_eq!(
        ToServer::decode(&unknown).unwrap_err(),
        DecodeError::Request
    );
    // A control character in a text.
    let mut escape = vec![33];
    escape.extend_from_slice(&(3_u16).to_le_bytes());
    escape.extend_from_slice(b"\x1b[J");
    assert_eq!(ToClient::decode(&escape).unwrap_err(), DecodeError::Field);
}

#[test]
fn unencodable_fields_are_refused_on_the_way_out() {
    assert!(ToClient::Info("\x1b[2J".to_owned()).encode().is_err());
    assert!(ToClient::Info("x".repeat(MAX_TEXT + 1)).encode().is_err());
    assert!(ToClient::Info("x".repeat(MAX_TEXT)).encode().is_ok());
}

#[test]
fn host_names_keep_rfc_1123() {
    for good in ["box", "ferrix", "a", "my-pc", "lab.example.org", "x1", "1x"] {
        assert!(valid_hostname(good).is_ok(), "{good}");
    }
    let long_label = "a".repeat(64);
    let long_name = format!("{}.{}", "a".repeat(40), "b".repeat(30));
    for bad in [
        "",
        "-box",
        "box-",
        "a..b",
        ".box",
        "box.",
        "my pc",
        "box/../etc",
        "bøx",
        "box\n",
        long_label.as_str(),
        long_name.as_str(),
    ] {
        assert!(valid_hostname(bad).is_err(), "{bad:?}");
    }
    assert!(valid_hostname(&"a".repeat(63)).is_ok());
}

#[test]
fn only_the_listed_units_may_be_switched() {
    for unit in SWITCHABLE_UNITS {
        let request = Request::ServiceEnablePersistent {
            unit: (*unit).to_owned(),
            enabled: true,
        };
        assert!(request.validate().is_ok(), "{unit}");
    }
    for unit in [
        "getty@.service",
        "auth.service",
        "sshd.service.d/x.conf",
        "",
        "../sshd.service",
    ] {
        let request = Request::ServiceEnablePersistent {
            unit: unit.to_owned(),
            enabled: true,
        };
        assert!(request.validate().is_err(), "{unit}");
    }
}

#[test]
fn each_area_has_its_own_service() {
    let services: Vec<&str> = requests()
        .iter()
        .map(|request| request.area().service())
        .collect();
    assert_eq!(
        services,
        [
            "settings.hostname",
            "settings.services",
            "settings.services"
        ]
    );
}
