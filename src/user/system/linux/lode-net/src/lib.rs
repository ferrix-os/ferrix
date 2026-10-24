//! lode over the network (`docs/LOGGING.md` §10.2, §10.3).
//!
//! * [`upload`]: `lode-upload`, on a Ferrix machine: what follows each
//!   destination's cursor in the store, sent over HTTP or HTTPS, the cursor
//!   moved on the destination's `2xx`.
//! * [`receive`]: `lode-receive`, on a Linux host: the uploads taken in over
//!   TLS and kept, a lode store for each machine that sends.
//! * [`pki`]: the CA and the certificates `lode-receive pair` makes.
//! * [`provider`] and [`random`]: rustls's crypto, pure Rust, TLS 1.3 with
//!   ChaCha20-Poly1305, X25519 and Ed25519 alone; the kernel's random
//!   numbers alone.
//! * [`tls`]: the client's and the server's TLS configuration.
//! * [`store`]: a lode store on a file system, read and written with
//!   `ferrix-lode-core`.

pub mod pki;
pub mod provider;
pub mod random;
pub mod receive;
pub mod store;
pub mod tls;
pub mod upload;
