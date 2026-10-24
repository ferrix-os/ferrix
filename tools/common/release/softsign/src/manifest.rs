//! Writing the 4 KiB signed manifest of `docs/SIGNING.md` §1.2: the same
//! bytes the product path writes (D3). `ferrix-sigverify` reads them.

use crate::token::{CKM_HSS, Error, ObjectHandle, Token};

/// The block's length and the signed body's.
pub const BLOCK_LEN: usize = 4096;
/// See [`BLOCK_LEN`].
pub const BODY_LEN: usize = 512;

/// The fields of a manifest's body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Body {
    /// 1 slot, 2 config, 3 driver, 4 revocation.
    pub kind: u16,
    /// The security version.
    pub security_version: u32,
    /// The signing key's anchor id (`0x8000` and up for development keys).
    pub anchor_id: u16,
    /// 1 max, 2 fast, 3 present, 4 desktop, 0xFF development.
    pub profile: u8,
    /// 1 x86-64, 2 AArch64, 3 ARMv7-A.
    pub arch: u8,
    /// The hardware family.
    pub family: u32,
    /// The release sequence.
    pub release: u64,
    /// The label, NUL padded.
    pub label: [u8; 32],
    /// The subject, NUL padded.
    pub subject: [u8; 32],
    /// The payload's length.
    pub payload_len: u64,
    /// The payload's SHA-256.
    pub payload_sha256: [u8; 32],
    /// Slot kind: the floors for kinds 1 to 4.
    pub floors: [u32; 4],
    /// Revocation kind: the target kind.
    pub target_kind: u16,
    /// Revocation kind: the revoked trees' identifiers, at most 20.
    pub revoked: Vec<[u8; 16]>,
}

fn put(block: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(slot) = block.get_mut(at..at + bytes.len()) {
        slot.copy_from_slice(bytes);
    }
}

impl Body {
    /// The 512 signed bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; BODY_LEN] {
        let mut b = [0u8; BODY_LEN];
        put(&mut b, 0, b"FXSIGMAN");
        put(&mut b, 8, &1u16.to_le_bytes());
        put(
            &mut b,
            10,
            &u16::try_from(BODY_LEN).unwrap_or(0).to_le_bytes(),
        );
        put(&mut b, 12, &self.kind.to_le_bytes());
        put(&mut b, 16, &self.security_version.to_le_bytes());
        put(&mut b, 20, &self.anchor_id.to_le_bytes());
        put(&mut b, 22, &[self.profile, self.arch]);
        put(&mut b, 24, &self.family.to_le_bytes());
        put(&mut b, 32, &self.release.to_le_bytes());
        put(&mut b, 40, &self.label);
        put(&mut b, 72, &self.subject);
        put(&mut b, 104, &self.payload_len.to_le_bytes());
        put(&mut b, 112, &self.payload_sha256);
        for (n, floor) in self.floors.iter().enumerate() {
            put(&mut b, 144 + 4 * n, &floor.to_le_bytes());
        }
        if self.kind == 4 {
            put(&mut b, 160, &self.target_kind.to_le_bytes());
            put(
                &mut b,
                162,
                &u16::try_from(self.revoked.len())
                    .unwrap_or(u16::MAX)
                    .to_le_bytes(),
            );
            for (n, id) in self.revoked.iter().enumerate() {
                put(&mut b, 176 + 16 * n, id);
            }
        }
        b
    }
}

/// A block from a body and its HSS signature.
#[must_use]
pub fn assemble(body: &[u8; BODY_LEN], signature: &[u8]) -> Vec<u8> {
    let mut block = vec![0u8; BLOCK_LEN];
    put(&mut block, 0, body);
    put(
        &mut block,
        BODY_LEN,
        &u32::try_from(signature.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    put(&mut block, BODY_LEN + 4, signature);
    block
}

/// Sign `body` with `key` on `token` (`C_Sign(CKM_HSS)` over bytes 0..512)
/// and assemble the block.
pub fn sign(token: &mut dyn Token, key: &ObjectHandle, body: &Body) -> Result<Vec<u8>, Error> {
    let bytes = body.encode();
    let signature = token.sign(key, CKM_HSS, &bytes)?;
    Ok(assemble(&bytes, &signature))
}
