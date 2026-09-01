//! Proving who published a root or a bundle.
//!
//! A hash says the bytes are the bytes that were named. It says nothing about
//! who did the naming. Signing adds that, and only that: a signature is a
//! claim by a key that it published this root, checked against a set of keys
//! the verifier already trusts.
//!
//! It does not replace verification by hash. A signed bundle whose objects do
//! not hash correctly is still refused — the signature only says a key vouched
//! for the id, and an id that does not match its contents is a broken bundle
//! whoever vouched for it.
//!
//! # What is signed
//!
//! ```text
//! root:   Ed25519("cavs-root-signature-v1"   ‖ kind ‖ root_id ‖ metadata_hash)
//! bundle: Ed25519("cavs-bundle-signature-v1" ‖ bundle checksum)
//! ```
//!
//! Both carry a domain tag, so a signature made over a root can never be
//! replayed as one over a bundle, and the object kind is inside the root
//! message so a signature over a commit cannot be presented as one over a tree
//! with the same digest.
//!
//! # Revocation
//!
//! A revoked key's signatures do not become invalid — they were valid when
//! they were made, and rewriting the past is not something a signature scheme
//! can do. Revocation is a property of the verifier: the key ring holds the
//! revoked list, outside anything that was signed, and a verifier consulting a
//! current ring rejects what a stale one would have accepted.

use std::collections::{BTreeMap, BTreeSet};

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};

use crate::bundle::Signature;
use crate::error::{ObjectError, Result};
use crate::id::{ObjectId, ObjectKind};

pub const ROOT_DOMAIN: &[u8] = b"cavs-root-signature-v1";
pub const BUNDLE_DOMAIN: &[u8] = b"cavs-bundle-signature-v1";

/// A short, stable name for a key, so a verifier can find the right public
/// key instead of trying every one it holds.
pub type KeyId = [u8; 8];

/// The id of a public key: the first eight bytes of its hash.
pub fn key_id(public: &VerifyingKey) -> KeyId {
    let digest = cavs_hash::hash_chunk(public.as_bytes());
    let mut id = [0u8; 8];
    id.copy_from_slice(&digest[..8]);
    id
}

/// The exact bytes a root signature covers.
pub fn root_message(kind: ObjectKind, root: &ObjectId, metadata_hash: &[u8; 32]) -> Vec<u8> {
    let mut message = Vec::with_capacity(ROOT_DOMAIN.len() + 4 + 32 + 32);
    message.extend_from_slice(ROOT_DOMAIN);
    message.extend_from_slice(&kind.tag().to_le_bytes());
    message.extend_from_slice(&root.digest);
    message.extend_from_slice(metadata_hash);
    message
}

/// The exact bytes a bundle signature covers.
pub fn bundle_message(checksum: &[u8; 32]) -> Vec<u8> {
    let mut message = Vec::with_capacity(BUNDLE_DOMAIN.len() + 32);
    message.extend_from_slice(BUNDLE_DOMAIN);
    message.extend_from_slice(checksum);
    message
}

/// Sign a root.
pub fn sign_root(
    key: &SigningKey,
    kind: ObjectKind,
    root: &ObjectId,
    metadata_hash: &[u8; 32],
) -> Signature {
    let message = root_message(kind, root, metadata_hash);
    Signature {
        key_id: key_id(&key.verifying_key()),
        bytes: key.sign(&message).to_bytes(),
    }
}

/// Sign a bundle checksum.
pub fn sign_bundle_checksum(key: &SigningKey, checksum: &[u8; 32]) -> Signature {
    Signature {
        key_id: key_id(&key.verifying_key()),
        bytes: key.sign(&bundle_message(checksum)).to_bytes(),
    }
}

/// The keys a verifier is willing to believe, and the ones it no longer is.
#[derive(Debug, Clone, Default)]
pub struct KeyRing {
    keys: BTreeMap<KeyId, VerifyingKey>,
    revoked: BTreeSet<KeyId>,
}

impl KeyRing {
    pub fn new() -> Self {
        KeyRing::default()
    }

    pub fn trust(&mut self, public: VerifyingKey) -> KeyId {
        let id = key_id(&public);
        self.keys.insert(id, public);
        id
    }

    /// Stop believing a key. Its past signatures do not become forgeries —
    /// they were made by that key — but this verifier will no longer accept
    /// them, which is the only thing revocation can mean.
    pub fn revoke(&mut self, id: KeyId) {
        self.revoked.insert(id);
    }

    pub fn is_revoked(&self, id: &KeyId) -> bool {
        self.revoked.contains(id)
    }

    pub fn get(&self, id: &KeyId) -> Option<&VerifyingKey> {
        self.keys.get(id)
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }
}

/// What checking a set of signatures found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignatureCheck {
    /// Signatures that verified against a trusted, unrevoked key.
    pub accepted: Vec<KeyId>,
    /// Signatures whose key is not in the ring. Not a forgery — just not
    /// something this verifier can speak to.
    pub unknown_keys: Vec<KeyId>,
    /// Signatures by a key the ring has revoked.
    pub revoked: Vec<KeyId>,
    /// Signatures that did not verify. These are the ones that matter.
    pub invalid: Vec<KeyId>,
}

impl SignatureCheck {
    /// Was this signed by at least one key the verifier trusts, with nothing
    /// actively wrong?
    pub fn is_trusted(&self) -> bool {
        !self.accepted.is_empty() && self.invalid.is_empty()
    }

    pub fn has_failures(&self) -> bool {
        !self.invalid.is_empty()
    }
}

/// Check signatures over a message against a ring.
pub fn verify_signatures(
    message: &[u8],
    signatures: &[Signature],
    ring: &KeyRing,
) -> SignatureCheck {
    let mut check = SignatureCheck::default();
    for signature in signatures {
        if ring.is_revoked(&signature.key_id) {
            check.revoked.push(signature.key_id);
            continue;
        }
        let Some(public) = ring.get(&signature.key_id) else {
            check.unknown_keys.push(signature.key_id);
            continue;
        };
        let parsed = ed25519_dalek::Signature::from_bytes(&signature.bytes);
        if public.verify(message, &parsed).is_ok() {
            check.accepted.push(signature.key_id);
        } else {
            check.invalid.push(signature.key_id);
        }
    }
    check
}

/// Check a root's signatures.
pub fn verify_root(
    kind: ObjectKind,
    root: &ObjectId,
    metadata_hash: &[u8; 32],
    signatures: &[Signature],
    ring: &KeyRing,
) -> SignatureCheck {
    verify_signatures(&root_message(kind, root, metadata_hash), signatures, ring)
}

/// Read a signing key from 32 bytes of secret material.
pub fn signing_key_from_bytes(bytes: &[u8]) -> Result<SigningKey> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| ObjectError::Corrupt("an Ed25519 secret key is 32 bytes".into()))?;
    Ok(SigningKey::from_bytes(&bytes))
}

/// Read a public key from 32 bytes.
pub fn verifying_key_from_bytes(bytes: &[u8]) -> Result<VerifyingKey> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| ObjectError::Corrupt("an Ed25519 public key is 32 bytes".into()))?;
    VerifyingKey::from_bytes(&bytes)
        .map_err(|e| ObjectError::Corrupt(format!("not a usable Ed25519 public key: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn root() -> ObjectId {
        ObjectId::from_blake3([0x42; 32])
    }

    #[test]
    fn a_signature_verifies_against_the_key_that_made_it() {
        let signer = key(1);
        let mut ring = KeyRing::new();
        ring.trust(signer.verifying_key());

        let signature = sign_root(&signer, ObjectKind::Commit, &root(), &[0u8; 32]);
        let check = verify_root(ObjectKind::Commit, &root(), &[0u8; 32], &[signature], &ring);
        assert!(check.is_trusted());
        assert_eq!(check.accepted.len(), 1);
    }

    #[test]
    fn a_signature_over_a_different_root_does_not_verify() {
        let signer = key(1);
        let mut ring = KeyRing::new();
        ring.trust(signer.verifying_key());

        let signature = sign_root(&signer, ObjectKind::Commit, &root(), &[0u8; 32]);
        let elsewhere = ObjectId::from_blake3([0x43; 32]);
        let check = verify_root(
            ObjectKind::Commit,
            &elsewhere,
            &[0u8; 32],
            &[signature],
            &ring,
        );
        assert!(!check.is_trusted());
        assert_eq!(check.invalid.len(), 1);
    }

    /// The kind is inside the signed message, so vouching for a commit does
    /// not vouch for a tree that happens to share the digest.
    #[test]
    fn a_signature_does_not_carry_across_object_kinds() {
        let signer = key(1);
        let mut ring = KeyRing::new();
        ring.trust(signer.verifying_key());
        let signature = sign_root(&signer, ObjectKind::Commit, &root(), &[0u8; 32]);
        let check = verify_root(ObjectKind::Tree, &root(), &[0u8; 32], &[signature], &ring);
        assert!(check.has_failures());
    }

    /// The domain tags keep a root signature from being presented as a bundle
    /// signature over the same 32 bytes.
    #[test]
    fn a_root_signature_is_not_a_bundle_signature() {
        let signer = key(1);
        let mut ring = KeyRing::new();
        ring.trust(signer.verifying_key());

        let checksum = [0x42u8; 32];
        let as_root = sign_root(&signer, ObjectKind::Commit, &root(), &[0u8; 32]);
        let check = verify_signatures(&bundle_message(&checksum), &[as_root], &ring);
        assert!(check.has_failures());
    }

    #[test]
    fn a_key_the_ring_does_not_hold_is_unknown_rather_than_wrong() {
        let signature = sign_root(&key(2), ObjectKind::Commit, &root(), &[0u8; 32]);
        let mut ring = KeyRing::new();
        ring.trust(key(1).verifying_key());
        let check = verify_root(ObjectKind::Commit, &root(), &[0u8; 32], &[signature], &ring);
        assert_eq!(check.unknown_keys.len(), 1);
        assert!(check.invalid.is_empty());
        assert!(!check.is_trusted());
    }

    #[test]
    fn a_revoked_key_is_refused_without_being_called_a_forgery() {
        let signer = key(3);
        let mut ring = KeyRing::new();
        let id = ring.trust(signer.verifying_key());
        let signature = sign_root(&signer, ObjectKind::Commit, &root(), &[0u8; 32]);

        assert!(verify_root(
            ObjectKind::Commit,
            &root(),
            &[0u8; 32],
            std::slice::from_ref(&signature),
            &ring
        )
        .is_trusted());

        ring.revoke(id);
        let check = verify_root(ObjectKind::Commit, &root(), &[0u8; 32], &[signature], &ring);
        assert_eq!(check.revoked.len(), 1);
        assert!(check.invalid.is_empty());
        assert!(!check.is_trusted());
    }

    #[test]
    fn several_keys_can_sign_one_root() {
        let mut ring = KeyRing::new();
        let signers = [key(1), key(2), key(3)];
        for signer in &signers {
            ring.trust(signer.verifying_key());
        }
        let signatures: Vec<_> = signers
            .iter()
            .map(|s| sign_root(s, ObjectKind::Commit, &root(), &[0u8; 32]))
            .collect();
        let check = verify_root(ObjectKind::Commit, &root(), &[0u8; 32], &signatures, &ring);
        assert_eq!(check.accepted.len(), 3);
        assert!(check.is_trusted());
    }

    /// One bad signature among good ones is not something to average out.
    #[test]
    fn one_broken_signature_spoils_the_set() {
        let good = key(1);
        let bad = key(2);
        let mut ring = KeyRing::new();
        ring.trust(good.verifying_key());
        ring.trust(bad.verifying_key());

        let mut forged = sign_root(&bad, ObjectKind::Commit, &root(), &[0u8; 32]);
        forged.bytes[0] ^= 0xff;
        let signatures = vec![
            sign_root(&good, ObjectKind::Commit, &root(), &[0u8; 32]),
            forged,
        ];

        let check = verify_root(ObjectKind::Commit, &root(), &[0u8; 32], &signatures, &ring);
        assert_eq!(check.accepted.len(), 1);
        assert_eq!(check.invalid.len(), 1);
        assert!(!check.is_trusted());
    }

    #[test]
    fn key_ids_are_stable_and_distinct() {
        assert_eq!(
            key_id(&key(1).verifying_key()),
            key_id(&key(1).verifying_key())
        );
        assert_ne!(
            key_id(&key(1).verifying_key()),
            key_id(&key(2).verifying_key())
        );
    }

    #[test]
    fn key_material_round_trips() {
        let signer = key(9);
        let restored = signing_key_from_bytes(signer.as_bytes()).unwrap();
        assert_eq!(restored.verifying_key(), signer.verifying_key());
        let public = verifying_key_from_bytes(signer.verifying_key().as_bytes()).unwrap();
        assert_eq!(public, signer.verifying_key());
        assert!(signing_key_from_bytes(b"too short").is_err());
    }

    #[test]
    fn nothing_signed_is_nothing_trusted() {
        let check = verify_root(
            ObjectKind::Commit,
            &root(),
            &[0u8; 32],
            &[],
            &KeyRing::new(),
        );
        assert!(!check.is_trusted());
        assert!(!check.has_failures());
    }
}
