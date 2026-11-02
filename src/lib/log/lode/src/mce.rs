//! The kernel's machine-check lines (`mce: cpu <n> bank <b> <class> ...`,
//! `docs/MACHINE-CHECK.md` §3.2), read into fields, and an AMD SMCA bank's
//! `ipid=` named (`docs/MACHINE-CHECK-AMD.md` §3: the kernel prints the
//! register, lode keeps the table).
//!
//! The table is Linux's `smca_hwid_mcatypes` and `smca_names`
//! (`arch/x86/kernel/cpu/mce/amd.c`): `MCA_IPID`'s `HWID` (bits 43:32) and
//! `McaType` (bits 63:48) name the block a bank reports for.

/// What a machine-check line says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MceLine<'a> {
    /// The processor.
    pub cpu: &'a str,
    /// The bank.
    pub bank: &'a str,
    /// The class word: `corrected`, `deferred`, `uncorrected-no-action`,
    /// `left` (from before this boot), and the like.
    pub class: &'a str,
    /// The bank's block, where the line carries a known `ipid=`.
    pub bank_type: Option<&'static str>,
    /// The bank's `status=`, where the line carries one.
    pub status: Option<u64>,
}

/// `(HWID << 16 | McaType, name)`, as Linux's table.
const SMCA_TYPES: &[(u32, &str)] = &[
    (0x2e << 16, "coherent_station"),
    ((0x2e << 16) | 0x2, "coherent_station"),
    (0x164 << 16, "dacc_be"),
    (0x157 << 16, "dacc_fe"),
    ((0xb0 << 16) | 0x3, "decode_unit"),
    (0x1e0 << 16, "eddr5_cmn"),
    ((0xb0 << 16) | 0x5, "execution_unit"),
    ((0xb0 << 16) | 0x6, "floating_point"),
    (0x241 << 16, "gmi_pcs"),
    (0x269 << 16, "gmi_phy"),
    ((0xb0 << 16) | 0x1, "insn_fetch"),
    ((0xb0 << 16) | 0x2, "l2_cache"),
    ((0xb0 << 16) | 0x7, "l3_cache"),
    (0xb0 << 16, "load_store"),
    ((0xb0 << 16) | 0x10, "load_store"),
    ((0x2e << 16) | 0x4, "ma_llc"),
    ((0x01 << 16) | 0x2, "mp5"),
    ((0xff << 16) | 0x2, "mpart"),
    (0xfd << 16, "mpasp"),
    ((0xfd << 16) | 0x1, "mpasp"),
    (0xbe << 16, "mpdacc"),
    ((0x01 << 16) | 0x3, "mpdma"),
    (0xf9 << 16, "mpm"),
    (0x12 << 16, "mpras"),
    (0x6c << 16, "nbif"),
    (0x18 << 16, "nbio"),
    (0x05 << 16, "param_block"),
    (0x46 << 16, "pcie"),
    ((0x46 << 16) | 0x1, "pcie"),
    (0x1e1 << 16, "pcie_pl"),
    ((0x2e << 16) | 0x1, "pie"),
    (0xff << 16, "psp"),
    ((0xff << 16) | 0x1, "psp"),
    (0, "reserved"),
    (0xa8 << 16, "sata"),
    (0x80 << 16, "shub"),
    (0x01 << 16, "smu"),
    ((0x01 << 16) | 0x1, "smu"),
    (0x5c << 16, "ssbdci"),
    (0x96 << 16, "umc"),
    ((0x96 << 16) | 0x1, "umc_v2"),
    (0xaa << 16, "usb"),
    (0x180 << 16, "usr_cp"),
    (0x170 << 16, "usr_dp"),
    (0x267 << 16, "wafl_phy"),
    (0x50 << 16, "xgmi_pcs"),
    (0x259 << 16, "xgmi_phy"),
];

/// The block an SMCA bank's `MCA_IPID` names, or `None` for one the table
/// does not know.
#[must_use]
pub fn bank_type(ipid: u64) -> Option<&'static str> {
    let high = ipid >> 32;
    let hwid = high & 0xfff;
    let mcatype = (high >> 16) & 0xffff;
    let key = u32::try_from((hwid << 16) | mcatype).ok()?;
    SMCA_TYPES
        .iter()
        .find(|&&(known, _)| known == key)
        .map(|&(_, name)| name)
}

/// A kernel line's machine-check fields, where it is one: `mce: cpu <n>
/// bank <b> <class>`, then `key=value` words, of which `ipid=` is read.
/// Bounded untrusted input: anything else is not a machine-check line.
#[must_use]
pub fn read(text: &[u8]) -> Option<MceLine<'_>> {
    let text = core::str::from_utf8(text).ok()?;
    let rest = text.strip_prefix("mce: cpu ")?;
    let mut words = rest.split(' ');
    let cpu = words.next().filter(|cpu| is_number(cpu))?;
    if words.next() != Some("bank") {
        return None;
    }
    let bank = words.next().filter(|bank| is_number(bank))?;
    let class = words.next().filter(|class| !class.contains('='))?;
    let hex = |key: &str| {
        rest.split(' ')
            .find_map(|word| word.strip_prefix(key))
            .and_then(|digits| u64::from_str_radix(digits, 16).ok())
    };
    Some(MceLine {
        cpu,
        bank,
        class,
        bank_type: hex("ipid=0x").and_then(bank_type),
        status: hex("status=0x"),
    })
}

/// Whether `word` is a non-empty run of ASCII digits.
fn is_number(word: &str) -> bool {
    !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::{bank_type, read};

    /// An IPID with `hwid` in 43:32 and `mcatype` in 63:48.
    fn ipid(hwid: u64, mcatype: u64) -> u64 {
        (mcatype << 48) | (hwid << 32) | 0x1234
    }

    #[test]
    fn every_type_in_linuxs_table_is_named() {
        let table = [
            (0x2e, 0x0, "coherent_station"),
            (0x2e, 0x2, "coherent_station"),
            (0x164, 0x0, "dacc_be"),
            (0x157, 0x0, "dacc_fe"),
            (0xb0, 0x3, "decode_unit"),
            (0x1e0, 0x0, "eddr5_cmn"),
            (0xb0, 0x5, "execution_unit"),
            (0xb0, 0x6, "floating_point"),
            (0x241, 0x0, "gmi_pcs"),
            (0x269, 0x0, "gmi_phy"),
            (0xb0, 0x1, "insn_fetch"),
            (0xb0, 0x2, "l2_cache"),
            (0xb0, 0x7, "l3_cache"),
            (0xb0, 0x0, "load_store"),
            (0xb0, 0x10, "load_store"),
            (0x2e, 0x4, "ma_llc"),
            (0x01, 0x2, "mp5"),
            (0xff, 0x2, "mpart"),
            (0xfd, 0x0, "mpasp"),
            (0xfd, 0x1, "mpasp"),
            (0xbe, 0x0, "mpdacc"),
            (0x01, 0x3, "mpdma"),
            (0xf9, 0x0, "mpm"),
            (0x12, 0x0, "mpras"),
            (0x6c, 0x0, "nbif"),
            (0x18, 0x0, "nbio"),
            (0x05, 0x0, "param_block"),
            (0x46, 0x0, "pcie"),
            (0x46, 0x1, "pcie"),
            (0x1e1, 0x0, "pcie_pl"),
            (0x2e, 0x1, "pie"),
            (0xff, 0x0, "psp"),
            (0xff, 0x1, "psp"),
            (0x00, 0x0, "reserved"),
            (0xa8, 0x0, "sata"),
            (0x80, 0x0, "shub"),
            (0x01, 0x0, "smu"),
            (0x01, 0x1, "smu"),
            (0x5c, 0x0, "ssbdci"),
            (0x96, 0x0, "umc"),
            (0x96, 0x1, "umc_v2"),
            (0xaa, 0x0, "usb"),
            (0x180, 0x0, "usr_cp"),
            (0x170, 0x0, "usr_dp"),
            (0x267, 0x0, "wafl_phy"),
            (0x50, 0x0, "xgmi_pcs"),
            (0x259, 0x0, "xgmi_phy"),
        ];
        for (hwid, mcatype, name) in table {
            assert_eq!(
                bank_type(ipid(hwid, mcatype)),
                Some(name),
                "{hwid:#x}/{mcatype:#x}"
            );
        }
    }

    #[test]
    fn an_unknown_type_has_no_name() {
        assert_eq!(bank_type(ipid(0xb0, 0x8)), None);
        assert_eq!(bank_type(ipid(0x333, 0x0)), None);
    }

    #[test]
    fn a_corrected_line_with_an_ipid_names_its_bank() {
        let line = read(
            b"mce: cpu 3 bank 2 corrected status=0x9c0000000000009f addr=0x1000 ipid=0x200b000000000 total=1",
        )
        .unwrap();
        assert_eq!(line.cpu, "3");
        assert_eq!(line.bank, "2");
        assert_eq!(line.class, "corrected");
        assert_eq!(line.bank_type, Some("l2_cache"));
    }

    #[test]
    fn a_line_without_an_ipid_has_no_bank_type() {
        let line =
            read(b"mce: cpu 0 bank 1 deferred status=0x8c00180000000000 addr=0x1000").unwrap();
        assert_eq!(line.class, "deferred");
        assert_eq!(line.bank_type, None);
    }

    #[test]
    fn a_left_record_keeps_its_first_word() {
        let line = read(b"mce: cpu 1 bank 4 left from before this boot status=0x9c00").unwrap();
        assert_eq!(line.class, "left");
    }

    #[test]
    fn other_lines_are_not_machine_check_lines() {
        for text in [
            &b"mce: polling on 4 processors"[..],
            b"mce: frame 0x1000 retired",
            b"mce: cpu x bank 1 corrected",
            b"mce: cpu 1 bank corrected",
            b"mce: cpu 1 bank 2 status=0x1",
            b"kernel: mce: cpu 1 bank 2 corrected",
            b"\xff\xfe",
        ] {
            assert_eq!(read(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_malformed_ipid_is_ignored() {
        let line = read(b"mce: cpu 0 bank 0 corrected ipid=0xzz").unwrap();
        assert_eq!(line.bank_type, None);
    }
}
