//! The TLS configurations: TLS 1.3 alone, on [`crate::provider`].

use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore, ServerConfig};

/// What went wrong setting TLS up: a file that does not read, or rustls's
/// refusal, said in a line.
pub type Error = String;

/// Every certificate in the PEM file at `path`.
///
/// # Errors
///
/// A file that does not read, or holds none.
pub fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, Error> {
    let certificates = CertificateDer::pem_file_iter(path)
        .map_err(|error| format!("{}: {error}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if certificates.is_empty() {
        return Err(format!("{}: no certificate", path.display()));
    }
    Ok(certificates)
}

/// The private key in the PEM file at `path`.
///
/// # Errors
///
/// A file that does not read, or holds no key.
pub fn private_key(path: &Path) -> Result<PrivateKeyDer<'static>, Error> {
    PrivateKeyDer::from_pem_file(path).map_err(|_| format!("{}: no private key", path.display()))
}

/// The roots: the CA certificate(s) at `path`.
///
/// # Errors
///
/// A file that does not read, or a certificate rustls will not take.
pub fn roots(path: &Path) -> Result<RootCertStore, Error> {
    let mut roots = RootCertStore::empty();
    for certificate in certificates(path)? {
        roots
            .add(certificate)
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(roots)
}

/// A client's configuration: the server's certificate must chain to `ca`;
/// `identity`, a certificate and its key, is shown when the server asks.
///
/// # Errors
///
/// A file that does not read, or a configuration rustls refuses.
pub fn client(ca: &Path, identity: Option<(&Path, &Path)>) -> Result<Arc<ClientConfig>, Error> {
    let builder = ClientConfig::builder_with_provider(Arc::new(crate::provider::provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| error.to_string())?
        .with_root_certificates(roots(ca)?);
    let config = match identity {
        Some((cert, key)) => builder
            .with_client_auth_cert(certificates(cert)?, private_key(key)?)
            .map_err(|error| format!("{}: {error}", cert.display()))?,
        None => builder.with_no_client_auth(),
    };
    Ok(Arc::new(config))
}

/// A server's configuration: its certificate and key, and the CA whose
/// clients it requires, or takes without one when `unpaired`.
///
/// # Errors
///
/// A file that does not read, or a configuration rustls refuses.
pub fn server(
    cert: &Path,
    key: &Path,
    ca: &Path,
    unpaired: bool,
) -> Result<Arc<ServerConfig>, Error> {
    let provider = Arc::new(crate::provider::provider());
    let verifier =
        WebPkiClientVerifier::builder_with_provider(Arc::new(roots(ca)?), Arc::clone(&provider));
    let verifier = if unpaired {
        verifier.allow_unauthenticated()
    } else {
        verifier
    }
    .build()
    .map_err(|error| error.to_string())?;
    let config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| error.to_string())?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certificates(cert)?, private_key(key)?)
        .map_err(|error| format!("{}: {error}", cert.display()))?;
    Ok(Arc::new(config))
}

/// A certificate's SHA-256, as `sha256:<hex>`.
#[must_use]
pub fn fingerprint(der: &[u8]) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(der);
    let mut out = String::from("sha256:");
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
