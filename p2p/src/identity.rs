//! Unified validator identity.
//!
//! BLS keys are the primary identity (used for consensus, fund custody).
//! ed25519 keys for P2P transport are derived deterministically from BLS.

use ark_serialize::CanonicalSerialize;
use commonware_cryptography::{Signer, ed25519};
use consensus::crypto::aggregated::{BlsPublicKey, BlsSecretKey, PeerId};

/// Domain separation tag for deriving ed25519 from BLS.
const ED25519_DERIVATION_DOMAIN: &[u8] = b"hellas-ed25519-from-bls-v1";

/// Unified validator identity holding both key types.
///
/// The BLS key is the primary identity (stored by validator, holds funds).
/// The ed25519 key for network transport is derived deterministically.
pub struct ValidatorIdentity {
    /// BLS secret key for consensus signatures.
    bls_secret_key: BlsSecretKey,
    /// BLS public key (cached).
    bls_public_key: BlsPublicKey,
    /// BLS peer ID (cached).
    peer_id: PeerId,
    /// Derived ed25519 key for P2P transport.
    ed25519_private_key: ed25519::PrivateKey,
}

impl ValidatorIdentity {
    /// Create a validator identity from a BLS secret key.
    ///
    /// The ed25519 key is derived deterministically using:
    /// `ed25519_seed = BLAKE3(domain || bls_scalar_bytes)[0..8] as u64`
    pub fn from_bls_key(bls_secret_key: BlsSecretKey) -> Self {
        let bls_public_key = bls_secret_key.public_key();
        let peer_id = bls_public_key.to_peer_id();

        // Derive ed25519 from BLS scalar
        let ed25519_private_key = Self::derive_ed25519(&bls_secret_key);

        Self {
            bls_secret_key,
            bls_public_key,
            peer_id,
            ed25519_private_key,
        }
    }

    /// Derive ed25519 private key from BLS secret key.
    ///
    /// Uses BLAKE3 with domain separation to create a u64 seed for commonware.
    fn derive_ed25519(bls_key: &BlsSecretKey) -> ed25519::PrivateKey {
        // Serialize BLS scalar (Fr) to bytes
        let mut scalar_bytes = Vec::new();
        bls_key
            .0
            .serialize_compressed(&mut scalar_bytes)
            .expect("BLS scalar serialization cannot fail");

        // Hash with domain separation
        let mut hasher = blake3::Hasher::new();
        hasher.update(ED25519_DERIVATION_DOMAIN);
        hasher.update(&scalar_bytes);
        let hash = hasher.finalize();

        // Take first 8 bytes as u64 seed for commonware
        let seed_bytes: [u8; 8] = hash.as_bytes()[..8].try_into().unwrap();
        let seed = u64::from_le_bytes(seed_bytes);

        ed25519::PrivateKey::from_seed(seed)
    }

    /// Derive the ed25519 seed that would be used for a given BLS key.
    /// Useful for computing expected ed25519 public key for config validation.
    pub fn derive_ed25519_seed(bls_key: &BlsSecretKey) -> u64 {
        let mut scalar_bytes = Vec::new();
        bls_key
            .0
            .serialize_compressed(&mut scalar_bytes)
            .expect("BLS scalar serialization cannot fail");

        let mut hasher = blake3::Hasher::new();
        hasher.update(ED25519_DERIVATION_DOMAIN);
        hasher.update(&scalar_bytes);
        let hash = hasher.finalize();

        let seed_bytes: [u8; 8] = hash.as_bytes()[..8].try_into().unwrap();
        u64::from_le_bytes(seed_bytes)
    }

    // ============== Accessors ==============

    /// Get the BLS secret key reference.
    pub fn bls_secret_key(&self) -> &BlsSecretKey {
        &self.bls_secret_key
    }

    /// Get the BLS public key.
    pub fn bls_public_key(&self) -> &BlsPublicKey {
        &self.bls_public_key
    }

    /// Get the BLS peer ID (used in consensus messages).
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Get the ed25519 private key (for P2P transport).
    pub fn ed25519_private_key(&self) -> &ed25519::PrivateKey {
        &self.ed25519_private_key
    }

    /// Get the ed25519 public key.
    pub fn ed25519_public_key(&self) -> ed25519::PublicKey {
        self.ed25519_private_key.public_key()
    }

    /// Clone the ed25519 private key (needed for passing to NetworkService).
    pub fn clone_ed25519_private_key(&self) -> ed25519::PrivateKey {
        // Reconstruct from the same seed to get a clone
        Self::derive_ed25519(&self.bls_secret_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::thread_rng;

    #[test]
    fn test_derivation_is_deterministic() {
        let bls_key = BlsSecretKey::generate(&mut thread_rng());

        let identity1 = ValidatorIdentity::from_bls_key(bls_key.clone());
        let identity2 = ValidatorIdentity::from_bls_key(bls_key);

        assert_eq!(
            identity1.ed25519_public_key(),
            identity2.ed25519_public_key(),
            "Same BLS key should produce same ed25519 key"
        );
    }

    #[test]
    fn test_different_bls_keys_produce_different_ed25519() {
        let bls_key1 = BlsSecretKey::generate(&mut thread_rng());
        let bls_key2 = BlsSecretKey::generate(&mut thread_rng());

        let identity1 = ValidatorIdentity::from_bls_key(bls_key1);
        let identity2 = ValidatorIdentity::from_bls_key(bls_key2);

        assert_ne!(
            identity1.ed25519_public_key(),
            identity2.ed25519_public_key(),
            "Different BLS keys should produce different ed25519 keys"
        );
    }

    #[test]
    fn test_peer_id_matches_bls_public_key() {
        let bls_key = BlsSecretKey::generate(&mut thread_rng());
        let expected_peer_id = bls_key.public_key().to_peer_id();

        let identity = ValidatorIdentity::from_bls_key(bls_key);

        assert_eq!(identity.peer_id(), expected_peer_id);
    }

    #[test]
    fn test_ed25519_seed_derivation() {
        let bls_key = BlsSecretKey::generate(&mut thread_rng());

        let seed1 = ValidatorIdentity::derive_ed25519_seed(&bls_key);
        let seed2 = ValidatorIdentity::derive_ed25519_seed(&bls_key);

        assert_eq!(seed1, seed2, "Same BLS key should derive same seed");

        // Verify the seed produces the expected key
        let identity = ValidatorIdentity::from_bls_key(bls_key.clone());
        let key_from_seed = ed25519::PrivateKey::from_seed(seed1);

        assert_eq!(
            identity.ed25519_public_key(),
            key_from_seed.public_key(),
            "Derived seed should produce same public key"
        );
    }
}
