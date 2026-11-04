//! The selectors a program sees: the kernel's GDT's, as K1's context checks them.

pub const USER_CODE32: u16 = 0x20;
pub const USER_DATA: u16 = 0x28;
pub const USER_CODE: u16 = 0x30;
pub const TLS_FIRST_SLOT: usize = 12;
pub const TLS_SLOTS: usize = 3;

/// Whether a program may load `selector`: K1 checks it again at entry.
pub fn loadable(selector: u16, tls: &[u64; TLS_SLOTS]) -> u16 {
    let index = usize::from(selector >> 3);
    let good = selector & 3 == 3
        && (index == 4
            || index == 5
            || index == 6
            || (TLS_FIRST_SLOT..TLS_FIRST_SLOT + TLS_SLOTS).contains(&index));
    if good { selector } else { 0 }
}
