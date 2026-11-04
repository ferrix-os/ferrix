//! `ferrix-driver`'s derives and register macros (`docs/DRIVER-MODEL.md`;
//! the driver model's design page).
//!
//! * `#[derive(Resources)]` turns a struct of a driver's resources -- its
//!   registers, interrupts, clocks, resets, pin states, supplies and parent
//!   bus, each named by a field attribute -- into an implementation of
//!   `ferrix_driver::Resources`: the requirement table devmgr checks before it
//!   starts the driver, embedded in the image's `.ferrix.requirements`
//!   section, and the lookup that resolves each field from the node.
//! * `#[derive(PinStates)]` turns an enum of a binding's pin states into the
//!   names the tree gives them (`Default` is `default`; `IdleMode` is
//!   `idle_mode`), each variant marked `#[optional]` allowed to be absent.
//! * `register_map!` lays a device's registers out as a struct of typed
//!   fields at their offsets, and `bitfields!` a register's value as named
//!   fields, with value encodings such as `OffsetByOne<MIN, MAX>`.
//!
//! The generated code names `::ferrix_driver`, so a crate that uses these
//! depends on `ferrix-driver`, which re-exports them. Tree and datasheet
//! names appear only in `tree_name = "..."` strings and comments.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as Tokens};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Attribute, Data, DeriveInput, Error, Fields, Ident, LitInt, LitStr, Meta, Token, Type,
    Visibility, braced, parse_macro_input,
};

/// `#[derive(Resources)]`: see the crate's documentation.
#[proc_macro_derive(
    Resources,
    attributes(
        registers,
        interrupt,
        clock,
        reset,
        pin_control,
        power_supply,
        parent_bus
    )
)]
pub fn derive_resources(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    resources(&input)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

/// `#[derive(PinStates)]`: see the crate's documentation.
#[proc_macro_derive(PinStates, attributes(optional))]
pub fn derive_pin_states(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    pin_states(&input)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

/// `register_map!`: see the crate's documentation.
#[proc_macro]
pub fn register_map(input: TokenStream) -> TokenStream {
    let maps = parse_macro_input!(input as Maps);
    maps.expand().into()
}

/// `bitfields!`: see the crate's documentation.
#[proc_macro]
pub fn bitfields(input: TokenStream) -> TokenStream {
    let values = parse_macro_input!(input as Values);
    values
        .expand()
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

/// What a field asks for, as `ferrix_devmgr_proto::requirements::What`
/// names it.
const KINDS: [(&str, &str); 7] = [
    ("registers", "Registers"),
    ("interrupt", "Interrupt"),
    ("clock", "Clock"),
    ("reset", "Reset"),
    ("pin_control", "PinControl"),
    ("power_supply", "PowerSupply"),
    ("parent_bus", "ParentBus"),
];

/// A field's requirement, as tokens building a `Requirement`.
fn requirement(attribute: &Attribute, optional: bool) -> Result<Tokens, Error> {
    let name = attribute
        .path()
        .get_ident()
        .map(Ident::to_string)
        .unwrap_or_default();
    let what = KINDS
        .iter()
        .find(|(attribute, _)| *attribute == name)
        .map(|(_, what)| Ident::new(what, Span::call_site()))
        .ok_or_else(|| Error::new_spanned(attribute, "not a resource attribute"))?;
    let mut index: Option<LitInt> = None;
    let mut tree_name: Option<LitStr> = None;
    let mut marked_optional = false;
    if let Meta::List(_) = attribute.meta {
        attribute.parse_nested_meta(|item| {
            if item.path.is_ident("index") {
                index = Some(item.value()?.parse()?);
            } else if item.path.is_ident("tree_name") {
                tree_name = Some(item.value()?.parse()?);
            } else if item.path.is_ident("optional") {
                marked_optional = true;
            } else {
                return Err(item.error("expected `index = N`, `tree_name = \"...\"` or `optional`"));
            }
            Ok(())
        })?;
    }
    let base = match (name.as_str(), index, tree_name) {
        ("pin_control", None, None) => quote! {
            ::ferrix_driver::Requirement::named(::ferrix_driver::What::PinControl, "default")
        },
        ("parent_bus", None, None) => quote! {
            ::ferrix_driver::Requirement::at(::ferrix_driver::What::ParentBus, 0)
        },
        (_, Some(index), None) => quote! {
            ::ferrix_driver::Requirement::at(::ferrix_driver::What::#what, #index)
        },
        (_, None, Some(tree_name)) => quote! {
            ::ferrix_driver::Requirement::named(::ferrix_driver::What::#what, #tree_name)
        },
        _ => {
            return Err(Error::new_spanned(
                attribute,
                "a resource is named by exactly one of `index = N` or `tree_name = \"...\"` \
                 (`pin_control` and `parent_bus` by neither)",
            ));
        }
    };
    if optional || marked_optional {
        Ok(quote! { #base.optional() })
    } else {
        Ok(base)
    }
}

/// Whether `ty` is `Option<...>`.
fn is_option(ty: &Type) -> bool {
    match ty {
        Type::Path(path) => path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "Option"),
        _ => false,
    }
}

fn resources(input: &DeriveInput) -> Result<Tokens, Error> {
    let name = &input.ident;
    let Data::Struct(data) = &input.data else {
        return Err(Error::new_spanned(
            input,
            "Resources is derived for a struct",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(Error::new_spanned(input, "Resources needs named fields"));
    };
    let mut requirements = Vec::new();
    let mut resolved = Vec::new();
    for field in &fields.named {
        let attributes: Vec<&Attribute> = field
            .attrs
            .iter()
            .filter(|attribute| {
                KINDS
                    .iter()
                    .any(|(kind, _)| attribute.path().is_ident(kind))
            })
            .collect();
        let [attribute] = attributes.as_slice() else {
            return Err(Error::new_spanned(
                field,
                "each resource field takes one of #[registers], #[interrupt], #[clock], \
                 #[reset], #[pin_control], #[power_supply] or #[parent_bus]",
            ));
        };
        let requirement = requirement(attribute, is_option(&field.ty))?;
        let ident = field
            .ident
            .as_ref()
            .ok_or_else(|| Error::new_spanned(field, "a named field"))?;
        resolved.push(quote! {
            #ident: ::ferrix_driver::FromNode::from_node(node, &#requirement)?
        });
        requirements.push(requirement);
    }
    let count = requirements.len();
    Ok(quote! {
        impl ::ferrix_driver::Resources for #name {
            const REQUIREMENTS: &'static [::ferrix_driver::Requirement<'static>] = &[
                #(#requirements),*
            ];

            fn requirement_table() -> &'static [u8] {
                const NEEDS: [::ferrix_driver::Requirement<'static>; #count] = [
                    #(#requirements),*
                ];
                #[used]
                #[unsafe(link_section = ".ferrix.requirements")]
                static TABLE: [u8; ::ferrix_driver::requirements::size(&NEEDS)] =
                    ::ferrix_driver::requirements::encode(&NEEDS);
                &TABLE
            }

            fn resolve(node: &mut ::ferrix_driver::Node) -> ::ferrix_driver::Result<Self> {
                Ok(Self {
                    #(#resolved),*
                })
            }
        }
    })
}

// ---------------------------------------------------------------------------
// PinStates
// ---------------------------------------------------------------------------

/// `IdleMode` as the tree names it: `idle_mode`.
fn tree_name(variant: &Ident) -> String {
    let mut name = String::new();
    for (index, character) in variant.to_string().chars().enumerate() {
        if character.is_uppercase() && index > 0 {
            name.push('_');
        }
        name.extend(character.to_lowercase());
    }
    name
}

fn pin_states(input: &DeriveInput) -> Result<Tokens, Error> {
    let name = &input.ident;
    let Data::Enum(data) = &input.data else {
        return Err(Error::new_spanned(
            input,
            "PinStates is derived for an enum",
        ));
    };
    let mut states = Vec::new();
    let mut arms = Vec::new();
    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(Error::new_spanned(variant, "a pin state has no fields"));
        }
        let ident = &variant.ident;
        let tree = tree_name(ident);
        let optional = variant
            .attrs
            .iter()
            .any(|attribute| attribute.path().is_ident("optional"));
        states.push(quote! {
            ::ferrix_driver::PinStateName { tree_name: #tree, optional: #optional }
        });
        arms.push(quote! { Self::#ident => #tree });
    }
    Ok(quote! {
        impl ::ferrix_driver::PinStates for #name {
            const STATES: &'static [::ferrix_driver::PinStateName] = &[#(#states),*];

            fn tree_name(&self) -> &'static str {
                match self {
                    #(#arms),*
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// register_map!
// ---------------------------------------------------------------------------

/// One register of a map: its offset, name and access type.
struct Register {
    offset: LitInt,
    name: Ident,
    access: Type,
}

impl Parse for Register {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let offset = input.parse()?;
        let _: Token![=>] = input.parse()?;
        let name = input.parse()?;
        let _: Token![:] = input.parse()?;
        let access = input.parse()?;
        Ok(Register {
            offset,
            name,
            access,
        })
    }
}

/// One map: `Name { offset => name: Access<Value>, ... }`.
struct Map {
    visibility: Visibility,
    name: Ident,
    registers: Punctuated<Register, Token![,]>,
}

impl Parse for Map {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let visibility = input.parse()?;
        let name = input.parse()?;
        let body;
        let _ = braced!(body in input);
        let registers = body.parse_terminated(Register::parse, Token![,])?;
        Ok(Map {
            visibility,
            name,
            registers,
        })
    }
}

/// Every map of one `register_map!`.
struct Maps(Vec<Map>);

impl Parse for Maps {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut maps = Vec::new();
        while !input.is_empty() {
            maps.push(input.parse()?);
        }
        Ok(Maps(maps))
    }
}

impl Maps {
    fn expand(&self) -> Tokens {
        let maps = self.0.iter().map(|map| {
            let Map {
                visibility,
                name,
                registers,
            } = map;
            let fields = registers.iter().map(|register| {
                let (field, access) = (&register.name, &register.access);
                quote! {
                    /// A register of the map.
                    pub(crate) #field: #access
                }
            });
            let made = registers.iter().map(|register| {
                let (field, access, offset) = (&register.name, &register.access, &register.offset);
                quote! {
                    // SAFETY: (DEVICE) the caller's mapping holds `BYTES`
                    // from `base`, and every register lies inside them.
                    #field: unsafe { <#access as ::ferrix_driver::Access>::at(base + #offset) }
                }
            });
            let ends = registers.iter().map(|register| {
                let (access, offset) = (&register.access, &register.offset);
                quote! {
                    let end = #offset + <#access as ::ferrix_driver::Access>::BYTES;
                    if end > bytes {
                        bytes = end;
                    }
                }
            });
            quote! {
                /// A device's registers, as `register_map!` laid them out.
                #[derive(Debug)]
                #visibility struct #name {
                    #(#fields),*
                }

                impl ::ferrix_driver::RegisterMap for #name {
                    const BYTES: usize = {
                        let mut bytes = 0;
                        #(#ends)*
                        bytes
                    };

                    unsafe fn at(base: usize) -> Self {
                        Self {
                            #(#made),*
                        }
                    }
                }
            }
        });
        quote! { #(#maps)* }
    }
}

// ---------------------------------------------------------------------------
// bitfields!
// ---------------------------------------------------------------------------

/// One field of a value: `name: bit N` or `name: bits LOW..=HIGH [as Encoding]`.
struct Field {
    name: Ident,
    low: u32,
    high: u32,
    encoding: Option<Type>,
}

impl Parse for Field {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let name = input.parse()?;
        let _: Token![:] = input.parse()?;
        let kind: Ident = input.parse()?;
        let (low, high) = if kind == "bit" {
            let bit: LitInt = input.parse()?;
            let bit = bit.base10_parse()?;
            (bit, bit)
        } else if kind == "bits" {
            let low: LitInt = input.parse()?;
            let _: Token![..=] = input.parse()?;
            let high: LitInt = input.parse()?;
            (low.base10_parse()?, high.base10_parse()?)
        } else {
            return Err(Error::new_spanned(
                kind,
                "expected `bit N` or `bits LOW..=HIGH`",
            ));
        };
        let encoding = if input.peek(Token![as]) {
            let _: Token![as] = input.parse()?;
            Some(input.parse()?)
        } else {
            None
        };
        Ok(Field {
            name,
            low,
            high,
            encoding,
        })
    }
}

/// One value: `Name { field, ... }`.
struct Value {
    visibility: Visibility,
    name: Ident,
    fields: Punctuated<Field, Token![,]>,
}

impl Parse for Value {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let visibility = input.parse()?;
        let name = input.parse()?;
        let body;
        let _ = braced!(body in input);
        let fields = body.parse_terminated(Field::parse, Token![,])?;
        Ok(Value {
            visibility,
            name,
            fields,
        })
    }
}

/// Every value of one `bitfields!`.
struct Values(Vec<Value>);

impl Parse for Values {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut values = Vec::new();
        while !input.is_empty() {
            values.push(input.parse()?);
        }
        Ok(Values(values))
    }
}

impl Values {
    fn expand(&self) -> Result<Tokens, Error> {
        let mut out = Vec::new();
        for value in &self.0 {
            let Value {
                visibility,
                name,
                fields,
            } = value;
            let mut methods = Vec::new();
            for field in fields {
                if field.high < field.low || field.high > 31 {
                    return Err(Error::new_spanned(
                        &field.name,
                        "bits within 0..=31, low first",
                    ));
                }
                methods.push(field_methods(field));
            }
            out.push(quote! {
                /// A register's value, as `bitfields!` named its fields.
                #[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
                #visibility struct #name(u32);

                impl #name {
                    /// Every field zero.
                    #[must_use]
                    pub(crate) const fn new() -> Self {
                        Self(0)
                    }

                    /// The value the register holds.
                    #[must_use]
                    pub(crate) const fn bits(self) -> u32 {
                        self.0
                    }

                    /// The value of a register that holds `bits`.
                    #[must_use]
                    pub(crate) const fn from_bits(bits: u32) -> Self {
                        Self(bits)
                    }

                    #(#methods)*
                }

                impl ::ferrix_driver::Value for #name {
                    type Bits = u32;

                    fn into_bits(self) -> u32 {
                        self.0
                    }

                    fn from_bits(bits: u32) -> Self {
                        Self(bits)
                    }
                }
            });
        }
        Ok(quote! { #(#out)* })
    }
}

/// A field's setter, named as the field, and its reader, `<field>_value`.
fn field_methods(field: &Field) -> Tokens {
    let name = &field.name;
    let reader = format_ident!("{}_value", name);
    let low = field.low;
    let width = field.high - field.low + 1;
    let mask: u32 = if width == 32 {
        u32::MAX
    } else {
        (1u32 << width) - 1
    };
    if width == 1 && field.encoding.is_none() {
        return quote! {
            /// Set or clear the field.
            #[must_use]
            pub(crate) const fn #name(self, on: bool) -> Self {
                let bit = 1u32 << #low;
                if on { Self(self.0 | bit) } else { Self(self.0 & !bit) }
            }

            /// Whether the field is set.
            #[must_use]
            pub(crate) const fn #reader(self) -> bool {
                self.0 & (1u32 << #low) != 0
            }
        };
    }
    // The model's own encodings are found in `ferrix_driver` whether or
    // not the driver imported them; any other type where the driver names it.
    let encoding = field.encoding.as_ref().map(|encoding| {
        let named = quote! { #encoding }.to_string();
        if named.starts_with("OffsetByOne") {
            quote! { ::ferrix_driver::#encoding }
        } else {
            quote! { #encoding }
        }
    });
    let attempt = format_ident!("try_{}", name);
    let (encode, decode) = match &encoding {
        Some(encoding) => (
            quote! { <#encoding>::try_encode(value) },
            quote! { <#encoding>::decode(stored) },
        ),
        None => (
            quote! { if value <= #mask { Some(value) } else { None } },
            quote! { stored },
        ),
    };
    quote! {
        /// Set the field to `value`, as the field takes it. For a fixed
        /// configuration, built in a `const` item: a value the encoding or
        /// the field's width cannot hold fails the build there. A value known
        /// only at run time goes through the fallible setter instead, which
        /// refuses it; here it would stop the driver.
        #[must_use]
        pub(crate) const fn #name(self, value: u32) -> Self {
            match self.#attempt(value) {
                Ok(set) => set,
                Err(_) => panic!("the value is outside the field's range"),
            }
        }

        /// Set the field to `value`, a value known only at run time: one the
        /// encoding or the field's width cannot hold is refused.
        ///
        /// # Errors
        ///
        /// `OutOfRange`, naming the value.
        pub(crate) const fn #attempt(
            self,
            value: u32,
        ) -> ::core::result::Result<Self, ::ferrix_driver::OutOfRange> {
            let bits = match #encode {
                Some(bits) if bits <= #mask => bits,
                _ => return Err(::ferrix_driver::OutOfRange { value }),
            };
            Ok(Self((self.0 & !(#mask << #low)) | (bits << #low)))
        }

        /// The field's value, decoded.
        #[must_use]
        pub(crate) const fn #reader(self) -> u32 {
            let stored = (self.0 >> #low) & #mask;
            #decode
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_state_is_named_as_the_tree_names_it() {
        let name = |text: &str| tree_name(&Ident::new(text, Span::call_site()));
        assert_eq!(name("Default"), "default");
        assert_eq!(name("Sleep"), "sleep");
        assert_eq!(name("IdleMode"), "idle_mode");
    }

    #[test]
    fn fields_parse_with_and_without_an_encoding() {
        let values: Values = syn::parse_str(
            "Configuration { enable: bit 0, frame_size: bits 0..=4 as OffsetByOne<4, 32>, raw: bits 5..=8 }",
        )
        .unwrap();
        let fields: Vec<&Field> = values.0[0].fields.iter().collect();
        assert_eq!((fields[0].low, fields[0].high), (0, 0));
        assert_eq!((fields[1].low, fields[1].high), (0, 4));
        assert!(fields[1].encoding.is_some());
        assert!(fields[2].encoding.is_none());
        let expanded = values.expand().unwrap().to_string();
        assert!(expanded.contains(":: ferrix_driver :: OffsetByOne"));
        assert!(expanded.contains("fn frame_size_value"));
        assert!(expanded.contains("fn try_frame_size"));
        let backwards: Values = syn::parse_str("Bad { field: bits 5..=2 }").unwrap();
        assert!(backwards.expand().is_err());
    }

    #[test]
    fn a_map_spans_to_its_last_register() {
        let maps: Maps =
            syn::parse_str("Map { 0x00 => first: ReadWrite<u32>, 0x30 => last: ReadOnly<u8>, }")
                .unwrap();
        let expanded = maps.expand().to_string();
        assert!(expanded.contains("impl :: ferrix_driver :: RegisterMap for Map"));
        assert!(expanded.contains("0x30"));
    }
}
