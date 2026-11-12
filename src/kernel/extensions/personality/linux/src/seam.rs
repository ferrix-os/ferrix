//! One composition-root seam, generated from one list (`docs/KERNEL-CRATES.md`
//! §6.3; the consultant's ledger line 3934, condition (1)).
//!
//! A seam is an interface whose implementation the binary's root chooses for
//! a build -- a removable crate's, or the load's `Absent` -- and whose callers,
//! here in the load, cannot name it. [`seam!`] takes the interface's entries
//! once, each a name, its symbol and its signature, and makes from that one
//! list everything the binding needs:
//!
//! * the trait, its entries associated functions with no `self`;
//! * one `unsafe extern "Rust"` declaration an entry, the symbol;
//! * one safe function an entry, the load's call sites' only way in, whose
//!   one `unsafe` block calls the symbol;
//! * the count of entries, `ENTRIES`;
//! * the binding macro the root invokes once with the chosen type, which
//!   defines each symbol as a `#[unsafe(no_mangle)]` Rust-ABI function
//!   calling that type's implementation.
//!
//! rustc does not compare a `no_mangle` definition with an `extern`
//! declaration in another crate, so a mismatch would link and be undefined
//! behaviour: the declarations and the definitions are both made here, from
//! the same tokens, so they cannot differ. A root that invokes the binding
//! macro not at all leaves the symbols undefined and does not link; one that
//! invokes it twice defines them twice and does not link either: the
//! registration is the linker's (ledger 3762 (2)).
//!
//! The list's types are written as `::ferrix_kernel_load::...` paths, which
//! resolve both here (`extern crate self as ferrix_kernel_load`) and in the
//! binary's root, where the binding macro expands.

/// One seam: its trait, symbols, wrappers, count and binding macro, from one
/// list of entries (see the module's documentation).
#[macro_export]
macro_rules! seam {
    (
        $d:tt;
        $(#[$trait_meta:meta])*
        trait $trait_name:ident;
        $(#[$bind_meta:meta])*
        bind $bind:ident in $module:ident;
        $(
            $(#[$meta:meta])*
            fn $name:ident as $symbol:ident ($($arg:ident: $ty:ty),* $(,)?) -> $ret:ty;
        )*
    ) => {
        $(#[$trait_meta])*
        pub trait $trait_name {
            $(
                $(#[$meta])*
                fn $name($($arg: $ty),*) -> $ret;
            )*
        }

        // SAFETY: (LINK) each declaration is defined, once, by the binding
        // macro below from these same tokens -- the same name, the same
        // signature, the Rust ABI -- in the binary's composition root; the
        // kernel does not link otherwise (ledger 3934 (1)).
        unsafe extern "Rust" {
            $(
                fn $symbol($($arg: $ty),*) -> $ret;
            )*
        }

        $(
            $(#[$meta])*
            #[inline]
            pub fn $name($($arg: $ty),*) -> $ret {
                // SAFETY: the binding macro below defines this symbol with
                // this declaration's signature, from the same tokens of
                // this one list, as a Rust-ABI function calling the chosen
                // type's implementation; the root invokes it once, or the
                // kernel does not link.
                unsafe { $symbol($($arg),*) }
            }
        )*

        /// How many entries the seam has.
        pub const ENTRIES: usize = [$(stringify!($name)),*].len();

        $(#[$bind_meta])*
        #[macro_export]
        macro_rules! $bind {
            ($d chosen:ty) => {
                $(
                    // Out of line, so the link keeps each symbol once, where
                    // the ELF check finds it (ledger 3934 (4)).
                    #[doc(hidden)]
                    #[inline(never)]
                    #[unsafe(no_mangle)]
                    fn $symbol($($arg: $ty),*) -> $ret {
                        <$d chosen as ::ferrix_kernel_load::$module::$trait_name>::$name($($arg),*)
                    }
                )*
            };
        }
    };
}
