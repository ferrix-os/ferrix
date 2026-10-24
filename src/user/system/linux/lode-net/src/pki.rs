//! The certificates `lode-receive pair` makes (`docs/LOGGING.md` §10.3):
//! a CA of the receiver's own, the receiver's server certificate, and a
//! client certificate for each machine paired with it. All Ed25519, the one
//! scheme [`crate::provider`] speaks; keys from the kernel's random numbers,
//! written 0600 and never printed.
//!
//! ```text
//! <root>/.pki/ca.pem  ca.key           the CA, made once
//! <root>/.pki/server.pem  server.key   remade when its names change
//! <root>/.pki/server.names             the names it was made for
//! <root>/.pki/clients/NAME.pem  NAME.key
//! ```
//!
//! Pairing again reuses what is there: the CA is never replaced, and a
//! machine's certificate stays the one it was given.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use ed25519_dalek::Signer as _;
use ed25519_dalek::pkcs8::{DecodePrivateKey as _, EncodePrivateKey as _};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyUsagePurpose, PublicKeyData, SanType, SigningKey,
};

/// An Ed25519 key rcgen signs with.
struct Key(ed25519_dalek::SigningKey, [u8; 32]);

impl PublicKeyData for Key {
    fn der_bytes(&self) -> &[u8] {
        &self.1
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ED25519
    }
}

impl SigningKey for Key {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        Ok(self.0.sign(message).to_bytes().to_vec())
    }
}

impl Key {
    /// A new key from the kernel's random numbers.
    fn new() -> Result<Key, String> {
        let mut seed = zeroize::Zeroizing::new([0_u8; 32]);
        crate::random::fill(seed.as_mut()).map_err(|error| format!("getrandom: {error}"))?;
        Ok(Key::from(ed25519_dalek::SigningKey::from_bytes(&seed)))
    }

    /// The key in the PEM file at `path`.
    fn read(path: &Path) -> Result<Key, String> {
        let text =
            fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
        ed25519_dalek::SigningKey::from_pkcs8_pem(&text)
            .map(Key::from)
            .map_err(|_| format!("{}: not an Ed25519 key", path.display()))
    }

    /// Write it to `path` as PKCS#8 PEM, mode 0600 from the moment the
    /// file exists.
    fn write(&self, path: &Path) -> Result<(), String> {
        let pem = self
            .0
            .to_pkcs8_pem(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)
            .map_err(|_| format!("{}: the key would not encode", path.display()))?;
        write_private(path, pem.as_bytes())
    }
}

impl From<ed25519_dalek::SigningKey> for Key {
    fn from(key: ed25519_dalek::SigningKey) -> Key {
        let public = key.verifying_key().to_bytes();
        Key(key, public)
    }
}

/// Write `bytes` to `path` with mode 0600, through a new file renamed
/// over it, so no reader ever sees it wider or half written.
///
/// # Errors
///
/// A write that failed.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let fresh = path.with_extension("new");
    let _ = fs::remove_file(&fresh);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&fresh)
        .map_err(|error| format!("{}: {error}", fresh.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("{}: {error}", fresh.display()))?;
    fs::set_permissions(&fresh, fs::Permissions::from_mode(0o600))
        .and_then(|()| fs::rename(&fresh, path))
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Write a public file (a certificate), mode 0644.
fn write_public(path: &Path, bytes: &[u8]) -> Result<(), String> {
    crate::store::replace(path, bytes)
        .and_then(|()| fs::set_permissions(path, fs::Permissions::from_mode(0o644)))
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Where a receiver's certificates are.
#[derive(Debug, Clone)]
pub struct Pki {
    /// `<root>/.pki`.
    pub dir: PathBuf,
}

/// What [`Pki::pair`] made or found.
#[derive(Debug, Clone)]
pub struct Paired {
    /// The CA certificate.
    pub ca: PathBuf,
    /// The machine's certificate.
    pub client_cert: PathBuf,
    /// Its key.
    pub client_key: PathBuf,
    /// The CA certificate's SHA-256.
    pub ca_fingerprint: String,
    /// The server certificate's.
    pub server_fingerprint: String,
}

impl Pki {
    /// The certificates of the receiver at `root`.
    #[must_use]
    pub fn at(root: &Path) -> Pki {
        Pki {
            dir: root.join(".pki"),
        }
    }

    /// The CA certificate.
    #[must_use]
    pub fn ca(&self) -> PathBuf {
        self.dir.join("ca.pem")
    }

    /// The server certificate and its key.
    #[must_use]
    pub fn server(&self) -> (PathBuf, PathBuf) {
        (self.dir.join("server.pem"), self.dir.join("server.key"))
    }

    /// Machine `name`'s certificate and key.
    #[must_use]
    pub fn client(&self, name: &str) -> (PathBuf, PathBuf) {
        let clients = self.dir.join("clients");
        (
            clients.join(format!("{name}.pem")),
            clients.join(format!("{name}.key")),
        )
    }

    /// Every paired machine's name and certificate's SHA-256.
    #[must_use]
    pub fn clients(&self) -> Vec<(String, String)> {
        let Ok(entries) = fs::read_dir(self.dir.join("clients")) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".pem"))
            else {
                continue;
            };
            if let Ok(certs) = crate::tls::certificates(&path)
                && let Some(cert) = certs.first()
            {
                out.push((name.to_owned(), crate::tls::fingerprint(cert)));
            }
        }
        out.sort();
        out
    }

    /// Pair machine `name` with this receiver, reached as `servers`: the CA
    /// made if there is none, the server certificate made if there is none
    /// or its names changed, the machine's certificate made if it has none.
    ///
    /// # Errors
    ///
    /// A name that is not one, or a file that cannot be read or written.
    pub fn pair(&self, name: &str, servers: &[String]) -> Result<Paired, String> {
        if name.is_empty()
            || name.len() > 32
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(format!(
                "{name:?}: a machine's name is 1 to 32 of a-z, 0-9 and -"
            ));
        }
        if servers.is_empty() {
            return Err(String::from(
                "--server-name: at least one name or address the machine dials",
            ));
        }
        for dir in [self.dir.clone(), self.dir.join("clients")] {
            fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("{}: {error}", dir.display()))?;
        }
        let ca_key_path = self.dir.join("ca.key");
        if !ca_key_path.exists() || !self.ca().exists() {
            let key = Key::new()?;
            let cert = ca_params()
                .self_signed(&key)
                .map_err(|error| error.to_string())?;
            key.write(&ca_key_path)?;
            write_public(&self.ca(), cert.pem().as_bytes())?;
        }
        // The issuer is the CA's own parameters and key again: the same
        // name and the same key identifier, so what it signs chains to the
        // certificate on disk.
        let issuer = Issuer::new(ca_params(), Key::read(&ca_key_path)?);
        let (server_cert, server_key) = self.server();
        let names_path = self.dir.join("server.names");
        let wanted_names = servers.join("\n");
        let same = fs::read_to_string(&names_path).is_ok_and(|names| names == wanted_names);
        if !same || !server_cert.exists() || !server_key.exists() {
            let key = Key::new()?;
            let mut params =
                CertificateParams::new(servers.to_vec()).map_err(|error| error.to_string())?;
            params.serial_number = Some(serial()?);
            params.distinguished_name = named("lode-receive");
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            params.not_after = until(10);
            let cert = params
                .signed_by(&key, &issuer)
                .map_err(|error| error.to_string())?;
            key.write(&server_key)?;
            write_public(&server_cert, cert.pem().as_bytes())?;
            write_public(&names_path, wanted_names.as_bytes())?;
        }
        let (client_cert, client_key) = self.client(name);
        if !client_cert.exists() || !client_key.exists() {
            let key = Key::new()?;
            let mut params = CertificateParams::default();
            params.serial_number = Some(serial()?);
            params.distinguished_name = named(name);
            params.subject_alt_names = Vec::<SanType>::new();
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
            params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            params.not_after = until(10);
            let cert = params
                .signed_by(&key, &issuer)
                .map_err(|error| error.to_string())?;
            key.write(&client_key)?;
            write_public(&client_cert, cert.pem().as_bytes())?;
        }
        let first = |path: &Path| -> Result<String, String> {
            crate::tls::certificates(path)?
                .first()
                .map(|cert| crate::tls::fingerprint(cert))
                .ok_or_else(|| format!("{}: no certificate", path.display()))
        };
        Ok(Paired {
            ca_fingerprint: first(&self.ca())?,
            server_fingerprint: first(&server_cert)?,
            ca: self.ca(),
            client_cert,
            client_key,
        })
    }
}

/// A client certificate from `root`'s CA that ended in 2020, and its key,
/// for the tests that require it refused.
///
/// # Errors
///
/// A CA that is not there, or a file that cannot be written.
#[doc(hidden)]
pub fn expired_client_for_tests(root: &Path) -> Result<(PathBuf, PathBuf), String> {
    let pki = Pki::at(root);
    let issuer = Issuer::new(ca_params(), Key::read(&pki.dir.join("ca.key"))?);
    let key = Key::new()?;
    let mut params = CertificateParams::default();
    params.serial_number = Some(serial()?);
    params.distinguished_name = named("expired");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params.not_before = rcgen::date_time_ymd(2019, 1, 1);
    params.not_after = rcgen::date_time_ymd(2020, 1, 1);
    let cert = params
        .signed_by(&key, &issuer)
        .map_err(|error| error.to_string())?;
    let (cert_path, key_path) = pki.client("expired");
    key.write(&key_path)?;
    write_public(&cert_path, cert.pem().as_bytes())?;
    Ok((cert_path, key_path))
}

/// A certificate's serial number: 16 random bytes from the kernel, the top
/// bit clear so it reads as positive.
fn serial() -> Result<rcgen::SerialNumber, String> {
    let mut bytes = [0_u8; 16];
    crate::random::fill(&mut bytes).map_err(|error| format!("getrandom: {error}"))?;
    if let Some(first) = bytes.first_mut() {
        *first &= 0x7f;
    }
    Ok(rcgen::SerialNumber::from_slice(&bytes))
}

/// A distinguished name of one common name.
fn named(name: &str) -> DistinguishedName {
    let mut out = DistinguishedName::new();
    out.push(DnType::CommonName, name);
    out
}

/// The CA's parameters: the same at every pairing.
fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    // The same serial every time, as the rest: the issuer made again from
    // these is the certificate on disk.
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&[1]));
    params.distinguished_name = named("lode-receive CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    params.not_after = until(20);
    params
}

/// `years` from 2026-01-01, for a certificate's end: fixed, so the CA's
/// parameters are the same whenever they are made again.
fn until(years: i32) -> time::OffsetDateTime {
    rcgen::date_time_ymd(2026 + years, 1, 1)
}
