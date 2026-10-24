//! `ferrix-softsign`: the development soft signer (`docs/SIGNING.md` §6.4).
//!
//! LMS keys in files, behind the PKCS #11 3.1 HSS subset the product path
//! drives an HSM through ([`token::Token`]), so that the tooling is the same
//! with either. It signs gates and development images only and **is never
//! used for a release** (the consultant's (D4), ledger line 3731). Its keys
//! are development keys, anchor ids `0x8000` and up (D1); its state follows
//! the HSM's rule, the next leaf committed before a signature is released
//! (D2); and its private keys live outside the repository and every image
//! (D5). It is a host tool: no image links this crate.

pub mod lms;
pub mod manifest;
pub mod token;

#[cfg(test)]
mod tests;
