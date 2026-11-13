//! The numbers against Linux's headers, and the convention.

use crate::{Errno, encode};

#[test]
fn errno_values_are_the_generic_ones() {
    assert_eq!(Errno::EPERM.0, 1, "EPERM is 1");
    assert_eq!(Errno::ENOENT.0, 2, "ENOENT is 2");
    assert_eq!(
        Errno::EAGAIN.0,
        11,
        "EAGAIN, which is also EWOULDBLOCK, is 11"
    );
    assert_eq!(Errno::EINVAL.0, 22, "EINVAL is 22");
    assert_eq!(Errno::ERANGE.0, 34, "ERANGE closes the errno-base range");
    assert_eq!(
        Errno::ENOSYS.0,
        38,
        "ENOSYS is what an unknown call number returns"
    );
    assert_eq!(Errno::ENODATA.0, 61, "ENODATA is what a missing xattr is");
    assert_eq!(Errno::EOVERFLOW.0, 75, "EOVERFLOW is 75");
    assert_eq!(Errno::EBADFD.0, 77, "EBADFD is 77, asm-generic/errno.h");
    assert_eq!(Errno::ESTRPIPE.0, 86, "ESTRPIPE is 86, asm-generic/errno.h");
    assert_eq!(Errno::ECONNREFUSED.0, 111, "ECONNREFUSED is 111");
    assert_eq!(Errno::ECANCELED.0, 125, "ECANCELED is 125");
}

#[test]
fn errno_encodes_as_a_negative_return_value() {
    assert_eq!(Errno::EPERM.as_return_value(), -1, "EPERM returns as -1");
    assert_eq!(
        Errno::ENOSYS.as_return_value(),
        -38,
        "ENOSYS returns as -38"
    );
    assert!(
        Errno::EINPROGRESS.as_return_value() > -(crate::MAX_ERRNO as isize),
        "every error stays inside the range reserved for them"
    );
}

#[test]
fn encode_maps_results_onto_the_return_register() {
    assert_eq!(encode(Ok(0)), 0, "a successful zero is returned unchanged");
    assert_eq!(
        encode(Ok(4096)),
        4096,
        "a successful count is returned unchanged"
    );
    assert_eq!(encode(Err(Errno::ENOENT)), -2, "ENOENT is returned as -2");
    assert_eq!(encode(Err(Errno::EPERM)), -1, "EPERM is returned as -1");
}
