use super::*;

/// Mangle `segments` the legacy way, with a hash on the end.
fn mangle(segments: &[&str]) -> String {
    let mut name = String::from("_ZN");
    for segment in segments.iter().copied().chain(["h0123456789abcdef"]) {
        name.push_str(&format!("{}{segment}", segment.len()));
    }
    name.push('E');
    name
}

#[test]
fn a_plain_path_loses_its_hash() {
    assert_eq!(
        demangle(mangle(&["ferrix_kernel", "kmain"]).as_bytes()),
        "ferrix_kernel::kmain"
    );
}

#[test]
fn generic_and_reference_escapes_are_decoded() {
    let name = mangle(&[
        "core",
        "ptr",
        "drop_in_place$LT$alloc..vec..Vec$LT$u8$GT$$GT$",
    ]);
    assert_eq!(
        demangle(name.as_bytes()),
        "core::ptr::drop_in_place<alloc::vec::Vec<u8>>"
    );

    let name = mangle(&["_$LT$$RF$T$u20$as$u20$core..fmt..Display$GT$", "fmt"]);
    assert_eq!(demangle(name.as_bytes()), "<&T as core::fmt::Display>::fmt");
}

#[test]
fn a_closure_keeps_its_braces() {
    let name = mangle(&[
        "ferrix_kernel_core",
        "smp",
        "run_everywhere",
        "$u7b$$u7b$closure$u7d$$u7d$",
    ]);
    assert_eq!(
        demangle(name.as_bytes()),
        "ferrix_kernel_core::smp::run_everywhere::{{closure}}"
    );
}

#[test]
fn a_suffix_after_the_path_is_dropped() {
    let name = format!("{}.llvm.4711", mangle(&["a", "b"]));
    assert_eq!(demangle(name.as_bytes()), "a::b");
}

#[test]
fn a_name_that_is_not_legacy_mangled_comes_back_unchanged() {
    for name in ["_start", "vectors", "_ZN99tooshortE", "_ZN"] {
        assert_eq!(demangle(name.as_bytes()), name);
    }
}

#[test]
fn only_a_trace_line_yields_an_address() {
    assert_eq!(
        trace_address("  trace     #0  0xffffffff80012345"),
        Some(0xffff_ffff_8001_2345)
    );
    assert_eq!(trace_address("trace #12 0x10"), Some(0x10));
    assert_eq!(
        trace_address("  at        src/kernel/src/main.rs:12:5"),
        None
    );
    assert_eq!(
        trace_address("  trace     no frame pointer chain to follow"),
        None
    );
    assert_eq!(trace_address("FERRIX-PANIC trace 0x10"), None);
}

/// Symbols taken from a build of this kernel (the four crates of
/// `docs/KERNEL-CRATES.md`, 2026-10-15), with what a backtrace should call
/// them.
const V0_NAMES: &[(&str, &str)] = &[
    (
        "_RNvCs9wFQrvczXsK_7___rustc17rust_begin_unwind",
        "__rustc::rust_begin_unwind",
    ),
    (
        "_RNvNtCs8ROlWsJZPDz_4core9panicking9panic_fmt",
        "core::panicking::panic_fmt",
    ),
    (
        "_RNvCs6hdX3FSbJSK_13ferrix_kernel5kmain",
        "ferrix_kernel::kmain",
    ),
    (
        "_RNvNtNtCs11Es365otsQ_18ferrix_kernel_core2mm5check12memory_check",
        "ferrix_kernel_core::mm::check::memory_check",
    ),
    (
        "_RINvNtCs11Es365otsQ_18ferrix_kernel_core9backtrace4walkNCNvNtB4_5panic16report_backtrace0EB4_",
        "ferrix_kernel_core::backtrace::walk",
    ),
    (
        "_RINvMs1_CsbCleW4jqUNW_11ferrix_heapNtB6_4Heap10deallocateNtNtCs11Es365otsQ_18ferrix_kernel_core2mm11KernelPagesEBX_",
        "ferrix_heap::Heap::deallocate",
    ),
    (
        "_RINvMs5_Cs5I2HcVrCRTN_13ferrix_pagingINtB6_6MapperNtNtB6_6x86_646X86_64E9map_rangeNtNtCs11Es365otsQ_18ferrix_kernel_core2mm13KernelPhysMemEB1m_",
        "ferrix_paging::Mapper::map_range",
    ),
    (
        "_RINvNtCs8ROlWsJZPDz_4core3ptr9drop_glueNtNtCs11Es365otsQ_18ferrix_kernel_core4vmap5ArenaEBF_",
        "core::ptr::drop_glue",
    ),
];

#[test]
fn v0_names_become_their_path_without_generics() {
    for &(mangled, expected) in V0_NAMES {
        assert_eq!(demangle(mangled.as_bytes()), expected, "{mangled}");
    }
}

#[test]
fn a_v0_name_that_does_not_parse_comes_back_whole() {
    for mangled in ["_R", "_RNvC", "_RQQQ", "_RNvCs9wFQrvczXsK_99short"] {
        assert_eq!(demangle(mangled.as_bytes()), mangled);
    }
}
