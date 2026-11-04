//! Fuzz `netd`'s switch (`docs/microkernel/M8.md` §2, N.4): every frame a partition or the
//! wire can send. Beyond not panicking: no frame goes back to its sender, only the uplink's
//! frame reaches the wire, under its own address, and the wire reaches only the uplink.

#![no_main]

use ferrix_netswitch::Switch;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let Some((&count, frame)) = bytes.split_first() else {
        return;
    };
    let count = usize::from(count % 4) + 1;
    let switch = Switch::new([2, 0, 0, 0, 0, 1], count);
    assert_eq!(switch.from_wire(frame).ports & !1, 0, "the wire reached the internal link");
    for from in 0..count {
        let route = switch.from_partition(from, frame);
        assert_eq!(route.ports & (1 << from), 0, "a frame went back to its sender");
        if route.wire {
            assert_eq!(from, 0, "the internal link reached the wire");
            assert_eq!(frame.get(6..12), switch.mac(from).as_ref().map(|mac| &mac[..]));
        }
    }
});
