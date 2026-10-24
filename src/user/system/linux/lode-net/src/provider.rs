//! rustls's crypto for lode-net: TLS 1.3 with one cipher suite,
//! `TLS13_CHACHA20_POLY1305_SHA256`, one key exchange, X25519, and one
//! signature scheme, Ed25519 -- all pure Rust, on the RustCrypto crates.
//!
//! Why so few: we make both ends. `lode-receive pair` issues Ed25519
//! certificates, and `lode-upload` and `lode-receive` speak only to each
//! other, so every algorithm more would be code that never runs and is
//! never tested. Why pure Rust: ring and aws-lc need a C compiler for the
//! musl Arm targets `lode-upload` is built for, which the tree does not
//! have.
//!
//! The glue follows the RustCrypto organisation's `rustls-rustcrypto`
//! 0.0.2-alpha (<https://github.com/RustCrypto/rustls-rustcrypto>, MIT OR
//! Apache-2.0), its ChaCha20-Poly1305, SHA-256, HMAC, X25519 and Ed25519
//! parts, cut to those and brought to the current rustls. That crate is
//! not used whole: it is an alpha, it pins `rustls-webpki` 0.102 against
//! rustls's 0.103, and it brings RSA (RUSTSEC-2023-0071) and the NIST
//! curves, none of which we want.
//!
//! Randomness is the kernel's alone ([`crate::random`]): no seeding in
//! user space and no fallback; a failed read fails the handshake.

use std::sync::Arc;

use chacha20poly1305::{AeadInPlace as _, KeyInit as _};
use ed25519_dalek::Signer as _;
use ed25519_dalek::pkcs8::DecodePrivateKey as _;
use hmac::Mac as _;
use rustls::crypto::cipher::{
    self, AeadKey, BorrowedPayload, InboundOpaqueMessage, InboundPlainMessage, Iv,
    MessageDecrypter, MessageEncrypter, OutboundOpaqueMessage, OutboundPlainMessage,
    PrefixedPayload, Tls13AeadAlgorithm, UnsupportedOperationError,
};
use rustls::crypto::{
    self, ActiveKeyExchange, CipherSuiteCommon, CryptoProvider, GetRandomFailed, KeyProvider,
    SecureRandom, SharedSecret, SupportedKxGroup, WebPkiSupportedAlgorithms, hash, hmac as rhmac,
};
use rustls::pki_types::{
    AlgorithmIdentifier, InvalidSignature, PrivateKeyDer, SignatureVerificationAlgorithm, alg_id,
};
use rustls::sign::{Signer, SigningKey};
use rustls::{
    CipherSuite, ConnectionTrafficSecrets, ContentType, NamedGroup, ProtocolVersion,
    SignatureAlgorithm, SignatureScheme, SupportedCipherSuite, Tls13CipherSuite,
};
use sha2::Digest as _;

/// The provider: the one suite, the one group, Ed25519, the kernel's
/// random numbers.
#[must_use]
pub fn provider() -> CryptoProvider {
    CryptoProvider {
        cipher_suites: vec![TLS13_CHACHA20_POLY1305_SHA256],
        kx_groups: vec![&X25519],
        signature_verification_algorithms: ALGORITHMS,
        secure_random: &Kernel,
        key_provider: &Kernel,
    }
}

/// `TLS13_CHACHA20_POLY1305_SHA256`.
pub static TLS13_CHACHA20_POLY1305_SHA256: SupportedCipherSuite =
    SupportedCipherSuite::Tls13(&Tls13CipherSuite {
        common: CipherSuiteCommon {
            suite: CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
            hash_provider: &Sha256,
            confidentiality_limit: u64::MAX,
        },
        hkdf_provider: &crypto::tls13::HkdfUsingHmac(&HmacSha256),
        aead_alg: &ChaCha20Poly1305,
        quic: None,
    });

/// The signature algorithms certificates and handshakes may use: Ed25519.
pub static ALGORITHMS: WebPkiSupportedAlgorithms = WebPkiSupportedAlgorithms {
    all: &[&Ed25519Verify],
    mapping: &[(SignatureScheme::ED25519, &[&Ed25519Verify])],
};

/// The kernel's random numbers, and the key loader.
#[derive(Debug)]
struct Kernel;

impl SecureRandom for Kernel {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), GetRandomFailed> {
        crate::random::fill(bytes).map_err(|_| GetRandomFailed)
    }
}

impl KeyProvider for Kernel {
    fn load_private_key(
        &self,
        key_der: PrivateKeyDer<'static>,
    ) -> Result<Arc<dyn SigningKey>, rustls::Error> {
        let PrivateKeyDer::Pkcs8(der) = &key_der else {
            return Err(rustls::Error::General(
                "only an Ed25519 key in PKCS#8 is taken".into(),
            ));
        };
        let key = ed25519_dalek::SigningKey::from_pkcs8_der(der.secret_pkcs8_der())
            .map_err(|_| rustls::Error::General("not an Ed25519 key in PKCS#8".into()))?;
        Ok(Arc::new(Ed25519Key(Arc::new(key))))
    }
}

/// SHA-256.
#[derive(Debug)]
struct Sha256;

impl hash::Hash for Sha256 {
    fn start(&self) -> Box<dyn hash::Context> {
        Box::new(Sha256Context(sha2::Sha256::new()))
    }

    fn hash(&self, data: &[u8]) -> hash::Output {
        hash::Output::new(&sha2::Sha256::digest(data))
    }

    fn output_len(&self) -> usize {
        32
    }

    fn algorithm(&self) -> hash::HashAlgorithm {
        hash::HashAlgorithm::SHA256
    }
}

/// A SHA-256 under way.
struct Sha256Context(sha2::Sha256);

impl hash::Context for Sha256Context {
    fn fork_finish(&self) -> hash::Output {
        hash::Output::new(&self.0.clone().finalize())
    }

    fn fork(&self) -> Box<dyn hash::Context> {
        Box::new(Sha256Context(self.0.clone()))
    }

    fn finish(self: Box<Self>) -> hash::Output {
        hash::Output::new(&self.0.finalize())
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
}

/// HMAC-SHA-256, for the HKDF of TLS 1.3's key schedule.
#[derive(Debug)]
struct HmacSha256;

impl rhmac::Hmac for HmacSha256 {
    fn with_key(&self, key: &[u8]) -> Box<dyn rhmac::Key> {
        // HMAC takes a key of any length; this never fails.
        match <hmac::Hmac<sha2::Sha256> as hmac::Mac>::new_from_slice(key) {
            Ok(mac) => Box::new(HmacKey(mac)),
            Err(_) => Box::new(HmacKey(
                <hmac::Hmac<sha2::Sha256> as hmac::digest::KeyInit>::new(&Default::default()),
            )),
        }
    }

    fn hash_output_len(&self) -> usize {
        32
    }
}

/// An HMAC-SHA-256 key.
struct HmacKey(hmac::Hmac<sha2::Sha256>);

impl rhmac::Key for HmacKey {
    fn sign_concat(&self, first: &[u8], middle: &[&[u8]], last: &[u8]) -> rhmac::Tag {
        let mut mac = self.0.clone();
        mac.update(first);
        for part in middle {
            mac.update(part);
        }
        mac.update(last);
        rhmac::Tag::new(&mac.finalize().into_bytes())
    }

    fn tag_len(&self) -> usize {
        32
    }
}

/// ChaCha20-Poly1305 for TLS 1.3's records.
#[derive(Debug)]
struct ChaCha20Poly1305;

/// Its tag.
const TAG: usize = 16;

impl ChaCha20Poly1305 {
    /// A cipher for `key`, which rustls makes the key length.
    fn cipher(key: &AeadKey, iv: Iv) -> Box<Cipher> {
        let key = chacha20poly1305::Key::clone_from_slice(key.as_ref());
        Box::new(Cipher(chacha20poly1305::ChaCha20Poly1305::new(&key), iv))
    }
}

impl Tls13AeadAlgorithm for ChaCha20Poly1305 {
    fn encrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageEncrypter> {
        Self::cipher(&key, iv)
    }

    fn decrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageDecrypter> {
        Self::cipher(&key, iv)
    }

    fn key_len(&self) -> usize {
        32
    }

    fn extract_keys(
        &self,
        key: AeadKey,
        iv: Iv,
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Ok(ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv })
    }
}

/// One direction's record protection.
struct Cipher(chacha20poly1305::ChaCha20Poly1305, Iv);

impl MessageEncrypter for Cipher {
    fn encrypt(
        &mut self,
        message: OutboundPlainMessage<'_>,
        seq: u64,
    ) -> Result<OutboundOpaqueMessage, rustls::Error> {
        let total = self.encrypted_payload_len(message.payload.len());
        let mut payload = PrefixedPayload::with_capacity(total);
        payload.extend_from_chunks(&message.payload);
        payload.extend_from_slice(&message.typ.to_array());
        let nonce = chacha20poly1305::Nonce::from(cipher::Nonce::new(&self.1, seq).0);
        let aad = cipher::make_tls13_aad(total);
        self.0
            .encrypt_in_place(&nonce, &aad, &mut Outbound(&mut payload))
            .map_err(|_| rustls::Error::EncryptError)?;
        Ok(OutboundOpaqueMessage::new(
            ContentType::ApplicationData,
            // TLS 1.3's records say 1.2 (RFC 8446 §5.1).
            ProtocolVersion::TLSv1_2,
            payload,
        ))
    }

    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + 1 + TAG
    }
}

impl MessageDecrypter for Cipher {
    fn decrypt<'a>(
        &mut self,
        mut message: InboundOpaqueMessage<'a>,
        seq: u64,
    ) -> Result<InboundPlainMessage<'a>, rustls::Error> {
        let payload = &mut message.payload;
        if payload.len() < TAG {
            return Err(rustls::Error::DecryptError);
        }
        let nonce = chacha20poly1305::Nonce::from(cipher::Nonce::new(&self.1, seq).0);
        let aad = cipher::make_tls13_aad(payload.len());
        self.0
            .decrypt_in_place(&nonce, &aad, &mut Inbound(payload))
            .map_err(|_| rustls::Error::DecryptError)?;
        message.into_tls13_unpadded_message()
    }
}

/// An outgoing record's buffer, as the AEAD writes into it.
struct Outbound<'a>(&'a mut PrefixedPayload);

impl AsRef<[u8]> for Outbound<'_> {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl AsMut<[u8]> for Outbound<'_> {
    fn as_mut(&mut self) -> &mut [u8] {
        self.0.as_mut()
    }
}

impl chacha20poly1305::aead::Buffer for Outbound<'_> {
    fn extend_from_slice(&mut self, other: &[u8]) -> chacha20poly1305::aead::Result<()> {
        self.0.extend_from_slice(other);
        Ok(())
    }

    fn truncate(&mut self, len: usize) {
        self.0.truncate(len);
    }
}

/// An incoming record's buffer, which decryption only shortens.
struct Inbound<'a, 'p>(&'a mut BorrowedPayload<'p>);

impl AsRef<[u8]> for Inbound<'_, '_> {
    fn as_ref(&self) -> &[u8] {
        self.0
    }
}

impl AsMut<[u8]> for Inbound<'_, '_> {
    fn as_mut(&mut self) -> &mut [u8] {
        self.0
    }
}

impl chacha20poly1305::aead::Buffer for Inbound<'_, '_> {
    fn extend_from_slice(&mut self, _: &[u8]) -> chacha20poly1305::aead::Result<()> {
        // Decryption in place never grows the buffer.
        Err(chacha20poly1305::aead::Error)
    }

    fn truncate(&mut self, len: usize) {
        self.0.truncate(len);
    }
}

/// X25519.
#[derive(Debug)]
struct X25519;

impl SupportedKxGroup for X25519 {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, rustls::Error> {
        let mut seed = zeroize::Zeroizing::new([0_u8; 32]);
        crate::random::fill(seed.as_mut()).map_err(|_| rustls::Error::FailedToGetRandomBytes)?;
        let secret = x25519_dalek::StaticSecret::from(*seed);
        let public = x25519_dalek::PublicKey::from(&secret);
        Ok(Box::new(X25519Exchange { secret, public }))
    }

    fn name(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

/// One X25519 exchange under way. The secret is used once and zeroed when
/// dropped (`x25519-dalek`'s `zeroize`).
struct X25519Exchange {
    /// Ours.
    secret: x25519_dalek::StaticSecret,
    /// Its public half.
    public: x25519_dalek::PublicKey,
}

impl ActiveKeyExchange for X25519Exchange {
    fn complete(self: Box<Self>, peer: &[u8]) -> Result<SharedSecret, rustls::Error> {
        let peer: [u8; 32] = peer
            .try_into()
            .map_err(|_| rustls::Error::from(rustls::PeerMisbehaved::InvalidKeyShare))?;
        let shared = self
            .secret
            .diffie_hellman(&x25519_dalek::PublicKey::from(peer));
        // A peer's low-order point gives all zeros: refused (RFC 7748 §6.1).
        if !shared.was_contributory() {
            return Err(rustls::PeerMisbehaved::InvalidKeyShare.into());
        }
        Ok(SharedSecret::from(shared.as_bytes().as_slice()))
    }

    fn pub_key(&self) -> &[u8] {
        self.public.as_bytes()
    }

    fn group(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

/// An Ed25519 private key.
#[derive(Debug)]
struct Ed25519Key(Arc<ed25519_dalek::SigningKey>);

impl SigningKey for Ed25519Key {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&SignatureScheme::ED25519)
            .then(|| Box::new(Ed25519Signer(Arc::clone(&self.0))) as Box<dyn Signer>)
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ED25519
    }
}

/// An Ed25519 key signing a handshake.
#[derive(Debug)]
struct Ed25519Signer(Arc<ed25519_dalek::SigningKey>);

impl Signer for Ed25519Signer {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        Ok(self.0.sign(message).to_bytes().to_vec())
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ED25519
    }
}

/// Ed25519 verification, for certificates and handshakes.
#[derive(Debug)]
struct Ed25519Verify;

impl SignatureVerificationAlgorithm for Ed25519Verify {
    fn verify_signature(
        &self,
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), InvalidSignature> {
        let public_key: &[u8; 32] = public_key.try_into().map_err(|_| InvalidSignature)?;
        let signature =
            ed25519_dalek::Signature::from_slice(signature).map_err(|_| InvalidSignature)?;
        ed25519_dalek::VerifyingKey::from_bytes(public_key)
            .map_err(|_| InvalidSignature)?
            .verify_strict(message, &signature)
            .map_err(|_| InvalidSignature)
    }

    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        alg_id::ED25519
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        alg_id::ED25519
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "tests")]

    use rustls::crypto::hash::Hash as _;
    use rustls::crypto::hmac::Hmac as _;
    use rustls::crypto::tls13::Hkdf as _;
    use rustls::pki_types::SignatureVerificationAlgorithm as _;

    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    /// FIPS 180-2's "abc".
    #[test]
    fn sha256_of_abc() {
        assert_eq!(
            Sha256.hash(b"abc").as_ref(),
            hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        let mut context = Sha256.start();
        context.update(b"a");
        let fork = context.fork();
        context.update(b"bc");
        assert_eq!(context.finish().as_ref(), Sha256.hash(b"abc").as_ref());
        assert_eq!(fork.fork_finish().as_ref(), Sha256.hash(b"a").as_ref());
    }

    /// RFC 4231 test case 2.
    #[test]
    fn hmac_sha256() {
        let key = HmacSha256.with_key(b"Jefe");
        assert_eq!(
            key.sign_concat(b"what do ya want ", &[b"for "], b"nothing?")
                .as_ref(),
            hex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
        );
    }

    /// RFC 5869 test case 1, through rustls's HKDF on our HMAC.
    #[test]
    fn hkdf_sha256() {
        let hkdf = crypto::tls13::HkdfUsingHmac(&HmacSha256);
        let ikm = hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let salt = hex("000102030405060708090a0b0c");
        let info = hex("f0f1f2f3f4f5f6f7f8f9");
        let expander = hkdf.extract_from_secret(Some(&salt), &ikm);
        let mut okm = [0_u8; 42];
        expander.expand_slice(&[&info], &mut okm).unwrap();
        assert_eq!(
            okm.to_vec(),
            hex(
                "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
            )
        );
    }

    /// RFC 7748 §6.1.
    #[test]
    fn x25519_agrees_and_refuses_a_low_order_point() {
        let alice = x25519_dalek::StaticSecret::from(
            <[u8; 32]>::try_from(hex(
                "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a",
            ))
            .unwrap(),
        );
        let bob_public = hex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
        let exchange = Box::new(X25519Exchange {
            public: x25519_dalek::PublicKey::from(&alice),
            secret: alice,
        });
        assert_eq!(
            exchange.complete(&bob_public).unwrap().secret_bytes(),
            hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742")
        );
        let ours = X25519.start().unwrap();
        assert!(ours.complete(&[0; 32]).is_err());
        assert!(X25519.start().unwrap().complete(&[1; 31]).is_err());
    }

    /// RFC 8032 §7.1 test 1.
    #[test]
    fn ed25519_verifies_and_refuses() {
        let public = hex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
        let signature = hex(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        );
        assert!(
            Ed25519Verify
                .verify_signature(&public, b"", &signature)
                .is_ok()
        );
        assert!(
            Ed25519Verify
                .verify_signature(&public, b"x", &signature)
                .is_err()
        );
        assert!(
            Ed25519Verify
                .verify_signature(&public[..31], b"", &signature)
                .is_err()
        );
    }

    /// A record sealed and opened again; a flipped bit, another sequence
    /// number or a short record refused.
    #[test]
    fn chacha20_poly1305_records() {
        let key = || AeadKey::from([7_u8; 32]);
        let iv = || Iv::from([9_u8; 12]);
        let mut sealer = ChaCha20Poly1305.encrypter(key(), iv());
        let plain = OutboundPlainMessage {
            typ: ContentType::ApplicationData,
            version: ProtocolVersion::TLSv1_2,
            payload: cipher::OutboundChunks::Single(b"hello"),
        };
        let sealed = sealer.encrypt(plain, 3).unwrap().encode();
        let open = |bytes: &mut Vec<u8>, seq: u64| -> Result<Vec<u8>, rustls::Error> {
            let mut opener = ChaCha20Poly1305.decrypter(key(), iv());
            let message = InboundOpaqueMessage::new(
                ContentType::ApplicationData,
                ProtocolVersion::TLSv1_2,
                &mut bytes[5..],
            );
            opener
                .decrypt(message, seq)
                .map(|plain| plain.payload.to_vec())
        };
        assert_eq!(open(&mut sealed.clone(), 3).unwrap(), b"hello");
        assert!(open(&mut sealed.clone(), 4).is_err());
        let mut flipped = sealed.clone();
        flipped[7] ^= 1;
        assert!(open(&mut flipped, 3).is_err());
        let mut short = sealed[..10].to_vec();
        assert!(open(&mut short, 3).is_err());
    }

    #[test]
    fn the_kernel_gives_random_bytes() {
        let mut a = [0_u8; 64];
        let mut b = [0_u8; 64];
        Kernel.fill(&mut a).unwrap();
        Kernel.fill(&mut b).unwrap();
        assert_ne!(a, b);
    }
}
