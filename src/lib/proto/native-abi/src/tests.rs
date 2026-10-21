//! What the ABI promises, held against itself and against `src/lib/proto/linux-abi`.

use core::mem::{offset_of, size_of};

use ferrix_linux_abi::nr::{from_aarch64, from_arm, from_x86_64};

use crate::bootstrap::{
    INIT_HELLO_BYTES, INIT_HELLO_HANDLES, INIT_HELLO_VERSION, init_hello, init_hello_version,
};
use crate::handle::Handle;
use crate::nr::{self, ALL, FIRST, LAST, NativeCall};
use crate::rights::{Requested, Rights, SAME_RIGHTS};
use crate::signals::Signals;
use crate::status;
use crate::types::{
    APERTURE_INFO_BYTES, ApertureInfo, DEVICE_INFO_BYTES, DeviceInfo, IoMappingSpec,
    PROCESS_EXITED, PROCESS_KILLED, PROCESS_RUNNING, PortPacket, ProcessStatus, ReadActual,
};

#[test]
fn no_native_number_is_a_linux_number_on_any_architecture() {
    for number in FIRST..=LAST {
        assert_eq!(from_x86_64(number), None, "{number:#x} is an x86-64 call");
        assert_eq!(from_aarch64(number), None, "{number:#x} is an AArch64 call");
        assert_eq!(from_arm(number), None, "{number:#x} is an ARMv7-A call");
    }
}

#[test]
fn every_call_round_trips_through_its_number() {
    for call in ALL {
        let number = nr::number(call);
        assert!(nr::is_native(number), "{call:?} is outside the range");
        assert_eq!(nr::decode(number), Some(call), "{call:?} does not decode");
    }
}

#[test]
fn every_number_in_the_range_is_a_call_or_a_gap_and_no_call_is_listed_twice() {
    let decoded = (FIRST..=LAST).filter_map(nr::decode).count();
    assert_eq!(decoded, ALL.len(), "a number decodes to a call ALL omits");
    for (i, a) in ALL.iter().enumerate() {
        for b in ALL.iter().skip(i + 1) {
            assert_ne!(a, b, "{a:?} is listed twice");
        }
    }
}

#[test]
fn all_is_in_number_order() {
    let numbers = ALL.map(nr::number);
    assert!(numbers.is_sorted(), "ALL is out of order: {numbers:x?}");
}

#[test]
fn nothing_outside_the_range_decodes() {
    for number in [0, 1, FIRST - 1, LAST + 1, 0x0F_0000, usize::MAX] {
        assert!(!nr::is_native(number), "{number:#x} claimed as native");
        assert_eq!(nr::decode(number), None, "{number:#x} decoded");
    }
}

#[test]
fn pinning_sits_in_the_vmo_block() {
    assert_eq!(nr::decode(0x1025), Some(NativeCall::VmoPin));
    assert_eq!(nr::decode(0x1026), Some(NativeCall::VmoPinAddresses));
    assert_eq!(nr::decode(0x1048), Some(NativeCall::BlockRingCreate));
    assert_eq!(nr::decode(0x1049), Some(NativeCall::DeviceInfo));
    assert_eq!(nr::decode(0x104A), Some(NativeCall::DeviceQuiesce));
    assert_eq!(nr::decode(0x104F), Some(NativeCall::DeviceClock));
    assert_eq!(nr::decode(0x1057), Some(NativeCall::DeviceSetLimit));
    assert_eq!(nr::decode(0x1058), Some(NativeCall::DeviceGetLimit));
    assert_eq!(nr::decode(0x1059), Some(NativeCall::DeviceIsolation));
    assert_eq!(nr::decode(0x1027), None, "0x1027 stays free");
    assert!(
        !Rights::PIN.contains(Rights::TRANSFER),
        "a pin stays with the driver that made it"
    );
}

#[test]
fn process_creation_opens_its_block() {
    assert_eq!(nr::decode(0x1030), Some(NativeCall::ProcessCreate));
    assert_eq!(nr::decode(0x1031), Some(NativeCall::ProcessStart));
    assert_eq!(nr::decode(0x1032), Some(NativeCall::ProcessGive));
    assert_eq!(nr::decode(0x1033), Some(NativeCall::ProcessBootstrap));
    assert_eq!(nr::decode(0x1034), Some(NativeCall::ProcessStatus));
    for number in 0x1035..=0x1037 {
        assert_eq!(nr::decode(number), None, "{number:#x} was assigned");
    }
    assert_eq!(
        nr::decode(0x1029),
        Some(NativeCall::JobKill),
        "JobKill moved"
    );
}

#[test]
fn status_names_are_pairwise_distinct() {
    for (i, a) in status::ALL.iter().enumerate() {
        for b in status::ALL.iter().skip(i + 1) {
            assert_ne!(a, b, "two native failures share errno {}", a.0);
        }
    }
}

#[test]
fn a_handle_that_does_not_fit_is_refused_rather_than_truncated() {
    assert_eq!(
        Handle::from_register(0x1001),
        Handle(0x1001),
        "a real value"
    );
    assert_eq!(
        Handle::from_register(0x1_0000_1001),
        Handle::INVALID,
        "upper bits must not be dropped"
    );
    assert_eq!(Handle::from_register(u64::MAX), Handle::INVALID, "all ones");
    assert!(!Handle::INVALID.is_valid(), "zero names nothing");
}

#[test]
fn rights_only_shrink() {
    let held = Rights::CHANNEL;
    assert_eq!(Requested::Same.resolve(held), Some(held), "same");
    assert_eq!(
        Requested::Exactly(Rights::READ).resolve(held),
        Some(Rights::READ),
        "a subset"
    );
    assert_eq!(
        Requested::Exactly(Rights::READ | Rights::MAP).resolve(held),
        None,
        "MAP is not held"
    );
    assert_eq!(
        Requested::Exactly(Rights::NONE).resolve(Rights::NONE),
        Some(Rights::NONE),
        "nothing from nothing"
    );
}

#[test]
fn a_rights_request_with_an_unknown_bit_is_refused() {
    assert_eq!(
        Requested::from_register(u64::from(SAME_RIGHTS)),
        Some(Requested::Same),
        "the sentinel"
    );
    assert_eq!(
        Requested::from_register(u64::from(Rights::ALL.0)),
        Some(Requested::Exactly(Rights::ALL)),
        "every defined right"
    );
    assert_eq!(
        Requested::from_register(0x100),
        None,
        "bit 8 is not a right"
    );
    assert_eq!(
        Requested::from_register(1 << 12),
        None,
        "bit 12 is not a right"
    );
    assert_eq!(
        Requested::from_register(u64::from(Rights::SUPERVISE.0)),
        Some(Requested::Exactly(Rights::SUPERVISE)),
        "bit 11 is SUPERVISE"
    );
    assert_eq!(
        Requested::from_register(u64::from(SAME_RIGHTS | 1)),
        None,
        "the sentinel is exact"
    );
    assert_eq!(
        Requested::from_register(1 << 32),
        None,
        "wider than 32 bits"
    );
}

#[test]
fn default_rights_are_defined_rights() {
    for rights in [
        Rights::CHANNEL,
        Rights::PORT,
        Rights::VMO,
        Rights::JOB,
        Rights::INTERRUPT,
        Rights::IO_MAPPING,
        Rights::DEVICE,
        Rights::PROCESS,
        Rights::PROCESS_CREATED,
        Rights::JOB_CREATED,
        Rights::EXCEPTION_CHANNEL,
    ] {
        assert!(rights.is_known(), "{rights:?} has an undefined bit");
    }
    assert!(!Rights::INTERRUPT.contains(Rights::DUPLICATE), "one owner");
    assert!(
        !Rights::IO_MAPPING.contains(Rights::DUPLICATE),
        "one driver"
    );
    assert!(
        Rights::INTERRUPT.contains(Rights::TRANSFER),
        "devmgr hands it on"
    );
}

#[test]
fn a_wait_for_an_undefined_signal_is_refused() {
    assert_eq!(
        Signals::from_register(u64::from(Signals::ALL.0)),
        Some(Signals::ALL),
        "every defined signal"
    );
    assert_eq!(
        Signals::from_register(1 << 4),
        Some(Signals::EMPTY),
        "bit 4 is a job's EMPTY"
    );
    assert_eq!(Signals::from_register(1 << 5), None, "bit 5");
    assert_eq!(Signals::from_register(1 << 40), None, "a high bit");
    assert!(
        (Signals::READABLE | Signals::PEER_CLOSED).intersects(Signals::PEER_CLOSED),
        "any, not all"
    );
}

#[test]
fn layouts_have_no_padding_and_match_on_every_target() {
    assert_eq!(size_of::<PortPacket>(), 32, "PortPacket");
    assert_eq!(offset_of!(PortPacket, key), 0, "key");
    assert_eq!(offset_of!(PortPacket, kind), 8, "kind");
    assert_eq!(offset_of!(PortPacket, signals), 12, "signals");
    assert_eq!(offset_of!(PortPacket, data), 16, "data");

    assert_eq!(size_of::<ReadActual>(), 8, "ReadActual");
    assert_eq!(offset_of!(ReadActual, handles), 4, "handles");

    assert_eq!(size_of::<IoMappingSpec>(), 16, "IoMappingSpec");
    assert_eq!(offset_of!(IoMappingSpec, len), 8, "len");

    assert_eq!(size_of::<Handle>(), 4, "Handle");

    assert_eq!(size_of::<ProcessStatus>(), 8, "ProcessStatus");
    assert_eq!(offset_of!(ProcessStatus, value), 4, "value");

    assert_eq!(
        size_of::<ApertureInfo>(),
        APERTURE_INFO_BYTES,
        "ApertureInfo"
    );
    assert_eq!(offset_of!(ApertureInfo, len), 8, "len");
    assert_eq!(offset_of!(ApertureInfo, bar), 16, "bar");
    assert_eq!(offset_of!(ApertureInfo, flags), 17, "flags");
    assert_eq!(offset_of!(ApertureInfo, offset), 24, "offset");
}

/// `DeviceInfo` keeps its 96 bytes, which drivers built before
/// `device_aperture` pass to `device_info`; the new call is beside it.
#[test]
fn device_info_keeps_its_size_and_apertures_have_a_call_of_their_own() {
    assert_eq!(size_of::<DeviceInfo>(), DEVICE_INFO_BYTES);
    assert_eq!(DEVICE_INFO_BYTES, 96);
    assert_eq!(nr::decode(0x1054), Some(NativeCall::DeviceAperture));
    assert_eq!(nr::decode(0x1055), Some(NativeCall::DeviceConfigRead));
    assert_eq!(nr::decode(0x1056), Some(NativeCall::DeviceConfigWrite));
}

#[test]
fn a_process_status_says_running_exited_or_killed_and_nothing_else() {
    assert_eq!(
        ProcessStatus::default().state,
        PROCESS_RUNNING,
        "a zeroed status reads as running"
    );
    let states = [PROCESS_RUNNING, PROCESS_EXITED, PROCESS_KILLED];
    for (i, a) in states.iter().enumerate() {
        for b in states.iter().skip(i + 1) {
            assert_ne!(a, b, "two states share {a}");
        }
    }
}

#[test]
fn quotas_follow_the_job_calls() {
    assert_eq!(nr::decode(0x102A), Some(NativeCall::JobForCgroup));
    assert_eq!(nr::decode(0x102B), Some(NativeCall::JobSetLimit));
    assert_eq!(nr::decode(0x102C), Some(NativeCall::JobGetQuota));
    for number in 0x102D..=0x102F {
        assert_eq!(nr::decode(number), None, "{number:#x} was assigned");
    }
}

#[test]
fn init_hello_is_eight_fixed_bytes() {
    assert_eq!(INIT_HELLO_BYTES, 8, "the header's length");
    assert_eq!(INIT_HELLO_HANDLES, 0, "version 1 carries no handle");
    assert_eq!(
        init_hello(),
        *b"FXIN\x01\x00\x00\x00",
        "magic, then 1 little-endian"
    );
    assert_eq!(init_hello_version(&init_hello()), Some(INIT_HELLO_VERSION));
}

#[test]
fn init_hello_is_recognised_and_nothing_else_is() {
    let mut later = init_hello().to_vec();
    later[4] = 2;
    later.extend_from_slice(b"what version 2 adds");
    assert_eq!(
        init_hello_version(&later),
        Some(2),
        "a later version, longer"
    );
    assert_eq!(init_hello_version(&init_hello()[..7]), None, "short");
    assert_eq!(init_hello_version(b""), None, "empty");
    assert_eq!(init_hello_version(b"FXIM\x01\x00\x00\x00"), None, "magic");
    assert_eq!(
        init_hello_version(b"FXIN\x00\x00\x00\x00"),
        None,
        "version zero"
    );
}

#[test]
fn a_port_descriptor_sits_in_the_port_block() {
    assert_eq!(nr::decode(0x1018), Some(NativeCall::PortCreate));
    assert_eq!(nr::decode(0x101A), Some(NativeCall::PortWait));
    assert_eq!(nr::decode(0x101B), Some(NativeCall::PortFd));
    for number in 0x101C..=0x101F {
        assert_eq!(nr::decode(number), None, "{number:#x} was assigned");
    }
}

#[test]
fn the_messages_after_the_hello_read_back() {
    use crate::bootstrap::{
        AUDIT_MAGIC, DEVMGR_STARTER_MAGIC, ROOT_MAGIC, ROOT_SWITCHED, after_hello, read_after_hello,
    };
    let audit = after_hello(AUDIT_MAGIC, 1);
    assert_eq!(&audit[..4], b"FXAU");
    assert_eq!(read_after_hello(&audit), Some((AUDIT_MAGIC, 1)));
    let starter = after_hello(DEVMGR_STARTER_MAGIC, 1);
    assert_eq!(&starter[..4], b"FXDS");
    assert_eq!(read_after_hello(&starter), Some((DEVMGR_STARTER_MAGIC, 1)));
    let root = after_hello(ROOT_MAGIC, ROOT_SWITCHED);
    assert_eq!(root[4..], 1_u32.to_le_bytes());
    assert_eq!(read_after_hello(&root), Some((ROOT_MAGIC, ROOT_SWITCHED)));
    assert_eq!(read_after_hello(&root[..7]), None, "short");
    assert_eq!(read_after_hello(&[0; 9]), None, "long");
}

/// K4's attach: `SUPERVISE` only where a creator made the process or job,
/// never in the rights the kernel hands out with authority of its own, and
/// an exception channel's read end never writes.
#[test]
fn supervise_is_only_a_creators_and_the_read_end_never_writes() {
    assert_eq!(Rights::SUPERVISE, Rights(1 << 11));
    assert!(Rights::ALL.contains(Rights::SUPERVISE));
    assert!(
        !Rights::PROCESS.contains(Rights::SUPERVISE),
        "devmgr_start's"
    );
    assert!(
        !Rights::JOB.contains(Rights::SUPERVISE),
        "job_for_cgroup's and the root's"
    );
    assert!(Rights::PROCESS_CREATED.contains(Rights::PROCESS | Rights::SUPERVISE));
    assert!(Rights::JOB_CREATED.contains(Rights::JOB | Rights::SUPERVISE));
    assert!(!Rights::EXCEPTION_CHANNEL.contains(Rights::WRITE));
    assert!(Rights::EXCEPTION_CHANNEL.contains(Rights::READ | Rights::WAIT));
    assert_eq!(nr::decode(0x1081), Some(NativeCall::ExceptionChannelCreate));
    assert_eq!(nr::number(NativeCall::ExceptionChannelCreate), 0x1081);
    assert_eq!(nr::decode(0x1082), None, "with the fault chain");
    assert_eq!(nr::decode(0x1083), None, "with the fault chain");
}
