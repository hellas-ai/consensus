use std::str::FromStr;

use ark_bls12_381::{Bls12_381, Fr, G1Affine, G1Projective, G2Affine, G2Projective};
use ark_ec::{AffineRepr, CurveGroup, PrimeGroup, pairing::Pairing};
use ark_ff::{PrimeField, UniformRand, Zero};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::crypto::BLS_SIGNATURE_COMPRESSED_SIZE;

pub type PeerId = u64;

#[derive(Clone, Debug, CanonicalDeserialize, CanonicalSerialize, PartialEq, Eq)]
pub struct BlsPublicKey(pub G2Affine);

#[derive(Clone, Debug, CanonicalDeserialize, CanonicalSerialize)]
pub struct BlsSignature(pub G1Affine);

#[derive(Clone, Debug, CanonicalDeserialize, CanonicalSerialize, Zeroize, ZeroizeOnDrop)]
pub struct BlsSecretKey(pub Fr);

impl BlsSecretKey {
    /// Generate a new random secret key
    pub fn generate<R: rand::Rng>(rng: &mut R) -> Self {
        Self(Fr::rand(rng))
    }

    /// Derive the public key from this secret key
    pub fn public_key(&self) -> BlsPublicKey {
        let g2 = G2Projective::generator();
        let pk = g2 * self.0;
        BlsPublicKey(pk.into_affine())
    }

    /// Sign a message with this secret key
    pub fn sign(&self, message: &[u8]) -> BlsSignature {
        let hash_point = Self::hash_to_g1(message);
        let signature = hash_point * self.0;
        BlsSignature(signature.into_affine())
    }

    /// Hash a message to a point on the curve
    fn hash_to_g1(message: &[u8]) -> G1Projective {
        let hash = blake3::hash(message);
        let hash_bytes = hash.as_bytes();
        let hash_fr = Fr::from_le_bytes_mod_order(hash_bytes);
        let g1 = G1Projective::generator();
        g1 * hash_fr
    }
}

impl BlsPublicKey {
    /// Verify a signature for a message
    pub fn verify(&self, message: &[u8], signature: &BlsSignature) -> bool {
        // e(signature, g2) == e(H(message), pk)
        let g2 = G2Affine::generator();
        let hash_point = Self::hash_to_g1(message);

        // compute pairings
        let left = Bls12_381::pairing(signature.0, g2);
        let right = Bls12_381::pairing(hash_point, self.0);

        left == right
    }

    /// Hash a message to a a point on the G1 curve (same as in secret key)
    fn hash_to_g1(message: &[u8]) -> G1Projective {
        let hash = blake3::hash(message);
        let hash_bytes = hash.as_bytes();
        let hash_fr = Fr::from_le_bytes_mod_order(hash_bytes);
        let g1 = G1Projective::generator();
        g1 * hash_fr
    }

    /// Aggregate multiple public keys into a single public key
    pub fn aggregate(public_keys: &[BlsPublicKey]) -> BlsPublicKey {
        let mut aggregated = G2Projective::zero();
        for public_key in public_keys {
            aggregated += public_key.0;
        }
        BlsPublicKey(aggregated.into_affine())
    }

    /// Verify an aggregated signature against an aggregated public key
    pub fn verify_aggregate<const N: usize>(
        public_keys: &[BlsPublicKey; N],
        message: &[u8],
        aggregated_signature: &BlsSignature,
    ) -> bool {
        // e(aggregated_signature, g2) == e(H(message), aggregated_public_key)
        let g2 = G2Affine::generator();
        let hash_point = Self::hash_to_g1(message);

        let aggregated_public_key = Self::aggregate(public_keys);

        let left = Bls12_381::pairing(aggregated_signature.0, g2);
        let right = Bls12_381::pairing(hash_point, aggregated_public_key.0);

        left == right
    }

    /// Verify multiple individual signatures efficiently using batch verification
    pub fn batch_verify(
        public_keys: &[BlsPublicKey],
        messages: &[&[u8]],
        signatures: &[BlsSignature],
    ) -> bool {
        // e(aggregated_signature, g2) == e(H(message), aggregated_public_key)
        assert_eq!(public_keys.len(), messages.len());
        assert_eq!(public_keys.len(), signatures.len());

        if public_keys.is_empty() {
            return true;
        }

        for i in 0..public_keys.len() {
            if !public_keys[i].verify(messages[i], &signatures[i]) {
                return false;
            }
        }

        true
    }

    /// Convert the public key to a peer ID
    ///
    /// The peer ID is the first 8 bytes of the hash of the underlying
    /// compressed public key serialization.
    pub fn to_peer_id(&self) -> PeerId {
        let mut buff = [0u8; BLS_SIGNATURE_COMPRESSED_SIZE];
        self.0.serialize_compressed(&mut buff[..]).unwrap();
        let hash = blake3::hash(&buff);
        u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap())
    }
}

impl FromStr for BlsPublicKey {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s)?;
        let g2_affine = G2Affine::deserialize_compressed(&bytes[..]).map_err(anyhow::Error::msg)?;
        Ok(BlsPublicKey(g2_affine))
    }
}

impl BlsSignature {
    pub fn aggregate<'a>(signatures: impl Iterator<Item = &'a BlsSignature>) -> BlsSignature {
        let mut aggregated = G1Projective::zero();
        for signature in signatures {
            aggregated += signature.0;
        }
        BlsSignature(aggregated.into_affine())
    }
}

#[derive(Clone, Debug, CanonicalDeserialize, CanonicalSerialize)]
pub struct AggregatedSignature<const N: usize> {
    pub aggregated_signature: BlsSignature,
    pub public_keys: [BlsPublicKey; N],
}

impl<const N: usize> AggregatedSignature<N> {
    pub fn new(
        public_keys: [BlsPublicKey; N],
        message: &[u8],
        signatures: &[BlsSignature],
    ) -> Option<Self> {
        if public_keys.len() != signatures.len() || public_keys.is_empty() {
            return None;
        }

        // Verify all individual signatures first
        for i in 0..public_keys.len() {
            if !public_keys[i].verify(message, &signatures[i]) {
                return None;
            }
        }

        let aggregated_signature = BlsSignature::aggregate(signatures.iter());

        Some(AggregatedSignature {
            aggregated_signature,
            public_keys: public_keys.clone(),
        })
    }

    /// Creates a new [`AggregatedSignature`] from a set of public keys
    /// and an aggregated signature.
    ///
    /// No prior validation is performed on the aggregated signature, with the
    /// assumption that the caller has validated it beforehand.
    pub fn new_from_aggregated_signature(
        public_keys: [BlsPublicKey; N],
        aggregated_signature: BlsSignature,
    ) -> Self {
        Self {
            public_keys,
            aggregated_signature,
        }
    }

    /// Verify the aggregated signature
    pub fn verify(&self, message: &[u8]) -> bool {
        BlsPublicKey::verify_aggregate(&self.public_keys, message, &self.aggregated_signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn test_public_key_derivation() {
        let mut rng = StdRng::from_seed([42u8; 32]);
        let sk = BlsSecretKey::generate(&mut rng);
        let pk = sk.public_key();

        // Public key should not be the identity element
        assert!(!pk.0.is_zero());

        // Same secret key should always produce same public key
        let pk2 = sk.public_key();
        assert_eq!(pk.0, pk2.0);
    }

    #[test]
    fn test_sign_and_verify() {
        let mut rng = StdRng::from_seed([123u8; 32]);
        let sk = BlsSecretKey::generate(&mut rng);
        let pk = sk.public_key();

        let message = b"Hello, world!";
        let signature = sk.sign(message);

        // Signature should not be the identity element
        assert!(!signature.0.is_zero());

        // Signature should verify correctly
        assert!(pk.verify(message, &signature));

        // Same message with same key should produce same signature
        let signature2 = sk.sign(message);
        assert_eq!(signature.0, signature2.0);

        // Different message should produce different signature
        let different_message = b"Goodbye, world!";
        let signature3 = sk.sign(different_message);
        assert_ne!(signature.0, signature3.0);

        // Wrong message should not verify
        assert!(!pk.verify(different_message, &signature));

        // Wrong signature should not verify
        let mut rng2 = StdRng::from_seed([255u8; 32]);
        let sk2 = BlsSecretKey::generate(&mut rng2);
        let wrong_signature = sk2.sign(message);
        assert!(!pk.verify(message, &wrong_signature));
    }

    #[test]
    fn test_multiple_signatures() {
        let mut rng = StdRng::from_seed([255u8; 32]);
        let sk = BlsSecretKey::generate(&mut rng);
        let pk = sk.public_key();

        let messages = [b"message 1", b"message 2", b"message 3"];

        let signatures: Vec<BlsSignature> = messages.iter().map(|msg| sk.sign(*msg)).collect();

        // All signatures should verify
        for (msg, sig) in messages.iter().zip(signatures.iter()) {
            assert!(pk.verify(*msg, sig));
        }

        // Signatures should be different for different messages
        assert_ne!(signatures[0].0, signatures[1].0);
        assert_ne!(signatures[1].0, signatures[2].0);
        assert_ne!(signatures[0].0, signatures[2].0);
    }

    #[test]
    fn test_deterministic_hash_to_g1() {
        let message = b"test message";

        // Hash should be deterministic - same input produces same output
        let hash1 = BlsSecretKey::hash_to_g1(message);
        let hash2 = BlsSecretKey::hash_to_g1(message);
        assert_eq!(hash1, hash2);

        // Different messages should produce different hashes
        let different_message = b"different message";
        let hash3 = BlsSecretKey::hash_to_g1(different_message);
        assert_ne!(hash1, hash3);

        // Hash should not be the identity element
        assert!(!hash1.is_zero());
    }

    #[test]
    fn test_key_properties() {
        let mut rng = StdRng::from_seed([255u8; 32]);

        // Test with multiple random keys
        for _ in 0..10 {
            let sk = BlsSecretKey::generate(&mut rng);
            let pk = sk.public_key();

            // Secret key should be in the field
            assert!(sk.0.into_bigint() < Fr::MODULUS);

            // Public key should be on the curve
            assert!(pk.0.is_on_curve());

            // Public key should not be the identity
            assert!(!pk.0.is_zero());
        }
    }

    #[test]
    fn test_signature_properties() {
        let mut rng = StdRng::from_seed([111u8; 32]);
        let sk = BlsSecretKey::generate(&mut rng);
        let message = b"signature properties test";
        let signature = sk.sign(message);

        // Signature should be on the curve
        assert!(signature.0.is_on_curve());

        // Signature should not be the identity
        assert!(!signature.0.is_zero());

        // Test that signature is correctly computed as hash * secret_key
        let expected_hash = BlsSecretKey::hash_to_g1(message);
        let expected_sig = expected_hash * sk.0;
        assert_eq!(signature.0, expected_sig.into_affine());
    }

    #[test]
    fn test_public_key_aggregate() {
        let mut rng = StdRng::from_seed([1u8; 32]);

        // Create multiple secret keys and derive public keys
        let sk1 = BlsSecretKey::generate(&mut rng);
        let sk2 = BlsSecretKey::generate(&mut rng);
        let sk3 = BlsSecretKey::generate(&mut rng);

        let pk1 = sk1.public_key();
        let pk2 = sk2.public_key();
        let pk3 = sk3.public_key();

        // Test aggregating multiple public keys
        let aggregated = BlsPublicKey::aggregate(&[pk1.clone(), pk2.clone(), pk3.clone()]);

        // Aggregated key should not be zero
        assert!(!aggregated.0.is_zero());

        // Aggregated key should be on the curve
        assert!(aggregated.0.is_on_curve());

        // Test aggregating single key (should equal the key itself)
        let single_aggregated = BlsPublicKey::aggregate(std::slice::from_ref(&pk1));
        assert_eq!(single_aggregated.0, pk1.0);

        // Test aggregating empty set (should be zero)
        let empty_aggregated = BlsPublicKey::aggregate(&[]);
        assert!(empty_aggregated.0.is_zero());

        // Test that different key sets produce different aggregates
        let aggregated_different = BlsPublicKey::aggregate(&[pk1.clone(), pk2.clone()]);
        assert_ne!(aggregated.0, aggregated_different.0);
    }

    #[test]
    fn test_verify_aggregate() {
        let mut rng = StdRng::from_seed([2u8; 32]);

        // Create multiple key pairs
        let sk1 = BlsSecretKey::generate(&mut rng);
        let sk2 = BlsSecretKey::generate(&mut rng);
        let sk3 = BlsSecretKey::generate(&mut rng);

        let pk1 = sk1.public_key();
        let pk2 = sk2.public_key();
        let pk3 = sk3.public_key();

        let message = b"test message for aggregation";

        // Each key signs the message
        let sig1 = sk1.sign(message);
        let sig2 = sk2.sign(message);
        let sig3 = sk3.sign(message);

        // Aggregate signatures
        let aggregated_sig =
            BlsSignature::aggregate([sig1.clone(), sig2.clone(), sig3.clone()].iter());

        // Test verification of aggregated signature
        assert!(BlsPublicKey::verify_aggregate(
            &[pk1.clone(), pk2.clone(), pk3.clone()],
            message,
            &aggregated_sig
        ));

        // Test with wrong message
        let wrong_message = b"wrong message";
        assert!(!BlsPublicKey::verify_aggregate(
            &[pk1.clone(), pk2.clone(), pk3.clone()],
            wrong_message,
            &aggregated_sig
        ));

        // Test with wrong aggregated signature (aggregate only subset of signatures)
        let wrong_aggregated_sig = BlsSignature::aggregate([sig1.clone(), sig2.clone()].iter());
        assert!(!BlsPublicKey::verify_aggregate(
            &[pk1.clone(), pk2.clone(), pk3.clone()],
            message,
            &wrong_aggregated_sig
        ));

        // Test with wrong public keys (different set)
        let mut rng2 = StdRng::from_seed([3u8; 32]);
        let sk4 = BlsSecretKey::generate(&mut rng2);
        let pk4 = sk4.public_key();
        assert!(!BlsPublicKey::verify_aggregate(
            &[pk1.clone(), pk2.clone(), pk4.clone()],
            message,
            &aggregated_sig
        ));
    }

    #[test]
    fn test_batch_verify() {
        let mut rng = StdRng::from_seed([4u8; 32]);

        // Create multiple key pairs
        let sk1 = BlsSecretKey::generate(&mut rng);
        let sk2 = BlsSecretKey::generate(&mut rng);
        let sk3 = BlsSecretKey::generate(&mut rng);

        let pk1 = sk1.public_key();
        let pk2 = sk2.public_key();
        let pk3 = sk3.public_key();

        let messages = [b"msg1", b"msg2", b"msg3"];
        let sig1 = sk1.sign(messages[0]);
        let sig2 = sk2.sign(messages[1]);
        let sig3 = sk3.sign(messages[2]);

        let public_keys = [pk1.clone(), pk2.clone(), pk3.clone()];
        let signatures = [sig1.clone(), sig2.clone(), sig3.clone()];

        // Test valid batch verification
        assert!(BlsPublicKey::batch_verify(
            &public_keys,
            &messages.iter().map(|m| m.as_slice()).collect::<Vec<_>>(),
            &signatures
        ));

        // Test empty batch (should return true)
        assert!(BlsPublicKey::batch_verify(&[], &[], &[]));

        // Test batch with one invalid signature
        let mut wrong_signatures = signatures.clone();
        let mut rng2 = StdRng::from_seed([5u8; 32]);
        let sk_wrong = BlsSecretKey::generate(&mut rng2);
        wrong_signatures[1] = sk_wrong.sign(messages[1]); // Wrong signature for message 1
        assert!(!BlsPublicKey::batch_verify(
            &public_keys,
            &messages.iter().map(|m| m.as_slice()).collect::<Vec<_>>(),
            &wrong_signatures
        ));

        // Test single signature batch
        assert!(BlsPublicKey::batch_verify(
            std::slice::from_ref(&pk1),
            &[messages[0]],
            std::slice::from_ref(&sig1)
        ));
    }

    #[test]
    fn test_to_peer_id() {
        let mut rng = StdRng::from_seed([6u8; 32]);
        let sk1 = BlsSecretKey::generate(&mut rng);
        let sk2 = BlsSecretKey::generate(&mut rng);

        let pk1 = sk1.public_key();
        let pk2 = sk2.public_key();

        // Peer ID should be deterministic for the same public key
        let peer_id1 = pk1.to_peer_id();
        let peer_id2 = pk1.to_peer_id();
        assert_eq!(peer_id1, peer_id2);

        // Different public keys should produce different peer IDs (with very high probability)
        let peer_id3 = pk2.to_peer_id();
        assert_ne!(peer_id1, peer_id3);

        // Peer ID should not be zero (extremely unlikely)
        assert_ne!(peer_id1, 0u64);
        assert_ne!(peer_id3, 0u64);

        // Test that peer ID is derived from the first 8 bytes of the hash
        let mut buff = [0u8; BLS_SIGNATURE_COMPRESSED_SIZE];
        pk1.0.serialize_compressed(&mut buff[..]).unwrap();
        let hash = blake3::hash(&buff);
        let expected_peer_id = u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap());
        assert_eq!(peer_id1, expected_peer_id);
    }

    #[test]
    fn test_public_key_hash_to_g1_consistency() {
        let message = b"consistency test";

        // Both BlsSecretKey::hash_to_g1 and BlsPublicKey::hash_to_g1 should produce the same result
        let hash_sk = BlsSecretKey::hash_to_g1(message);
        let hash_pk = BlsPublicKey::hash_to_g1(message);

        assert_eq!(hash_sk, hash_pk);
    }

    #[test]
    fn test_aggregated_signature_integration() {
        let mut rng = StdRng::from_seed([7u8; 32]);

        // Create multiple key pairs
        let sk1 = BlsSecretKey::generate(&mut rng);
        let sk2 = BlsSecretKey::generate(&mut rng);

        let pk1 = sk1.public_key();
        let pk2 = sk2.public_key();

        let message = b"aggregated signature test";

        // Sign with individual keys
        let sig1 = sk1.sign(message);
        let sig2 = sk2.sign(message);

        // Create aggregated signature using the AggregatedSignature struct
        let aggregated = AggregatedSignature::new(
            [pk1.clone(), pk2.clone()],
            message,
            &[sig1.clone(), sig2.clone()],
        );
        assert!(aggregated.is_some());

        let aggregated = aggregated.unwrap();

        // Verify the aggregated signature
        assert!(aggregated.verify(message));

        // Test with wrong message
        assert!(!aggregated.verify(b"wrong message"));

        // Test creating from pre-aggregated signature
        let manual_aggregated_sig = BlsSignature::aggregate([sig1.clone(), sig2.clone()].iter());
        let from_manual = AggregatedSignature::new_from_aggregated_signature(
            [pk1.clone(), pk2.clone()],
            manual_aggregated_sig,
        );
        assert!(from_manual.verify(message));
    }

    #[test]
    fn test_aggregated_signature_edge_cases() {
        let mut rng = StdRng::from_seed([8u8; 32]);
        let sk = BlsSecretKey::generate(&mut rng);
        let pk = sk.public_key();
        let message = b"edge case test";
        let sig = sk.sign(message);

        // Test with empty arrays (should return None)
        let empty_result = AggregatedSignature::<0>::new([], message, &[]);
        assert!(empty_result.is_none());

        // Test with mismatched signature count
        let wrong_result =
            AggregatedSignature::new([pk.clone()], message, &[sig.clone(), sig.clone()]);
        assert!(wrong_result.is_none());

        // Test with invalid signature
        let mut rng2 = StdRng::from_seed([9u8; 32]);
        let sk_wrong = BlsSecretKey::generate(&mut rng2);
        let sig_wrong = sk_wrong.sign(b"different message");
        let invalid_result = AggregatedSignature::new([pk.clone()], message, &[sig_wrong]);
        assert!(invalid_result.is_none());
    }

    #[test]
    fn test_bls_signature_aggregate() {
        let mut rng = StdRng::from_seed([10u8; 32]);

        // Create multiple key pairs
        let sk1 = BlsSecretKey::generate(&mut rng);
        let sk2 = BlsSecretKey::generate(&mut rng);
        let sk3 = BlsSecretKey::generate(&mut rng);

        let message = b"signature aggregation test";

        // Create individual signatures
        let sig1 = sk1.sign(message);
        let sig2 = sk2.sign(message);
        let sig3 = sk3.sign(message);

        // Test aggregating multiple signatures
        let aggregated = BlsSignature::aggregate([sig1.clone(), sig2.clone(), sig3.clone()].iter());

        // Aggregated signature should not be zero
        assert!(!aggregated.0.is_zero());

        // Aggregated signature should be on the curve
        assert!(aggregated.0.is_on_curve());

        // Test aggregating single signature (should equal the signature itself)
        let single_aggregated = BlsSignature::aggregate(std::iter::once(&sig1));
        assert_eq!(single_aggregated.0, sig1.0);

        // Test aggregating empty set (should be zero)
        let empty_aggregated = BlsSignature::aggregate(std::iter::empty());
        assert!(empty_aggregated.0.is_zero());

        // Test that different signature sets produce different aggregates
        let aggregated_different = BlsSignature::aggregate([sig1.clone(), sig2.clone()].iter());
        assert_ne!(aggregated.0, aggregated_different.0);

        // Test that aggregating the same signatures in different order produces the same result
        let aggregated_reordered =
            BlsSignature::aggregate([sig2.clone(), sig1.clone(), sig3.clone()].iter());
        assert_eq!(aggregated.0, aggregated_reordered.0);
    }

    #[test]
    fn test_bls_signature_aggregate_mathematical_properties() {
        let mut rng = StdRng::from_seed([11u8; 32]);

        // Create key pairs
        let sk1 = BlsSecretKey::generate(&mut rng);
        let sk2 = BlsSecretKey::generate(&mut rng);

        let message = b"mathematical properties test";

        // Create individual signatures
        let sig1 = sk1.sign(message);
        let sig2 = sk2.sign(message);

        // Manual aggregation: sig1 + sig2
        let manual_aggregated = BlsSignature((sig1.0 + sig2.0).into_affine());

        // Function aggregation
        let func_aggregated = BlsSignature::aggregate([sig1.clone(), sig2.clone()].iter());

        // Should be the same
        assert_eq!(manual_aggregated.0, func_aggregated.0);

        // Test associativity: (sig1 + sig2) + sig3 = sig1 + (sig2 + sig3)
        let sk3 = BlsSecretKey::generate(&mut rng);
        let sig3 = sk3.sign(message);

        let left_assoc = BlsSignature::aggregate(
            [
                BlsSignature::aggregate([sig1.clone(), sig2.clone()].iter()),
                sig3.clone(),
            ]
            .iter(),
        );

        let right_assoc = BlsSignature::aggregate(
            [
                sig1.clone(),
                BlsSignature::aggregate([sig2.clone(), sig3.clone()].iter()),
            ]
            .iter(),
        );

        assert_eq!(left_assoc.0, right_assoc.0);
    }

    #[test]
    fn test_aggregated_signature_new_from_aggregated_signature() {
        let mut rng = StdRng::from_seed([12u8; 32]);

        // Create multiple key pairs
        let sk1 = BlsSecretKey::generate(&mut rng);
        let sk2 = BlsSecretKey::generate(&mut rng);

        let pk1 = sk1.public_key();
        let pk2 = sk2.public_key();

        let message = b"test message";

        // Create individual signatures and aggregate them manually
        let sig1 = sk1.sign(message);
        let sig2 = sk2.sign(message);
        let aggregated_sig = BlsSignature::aggregate([sig1.clone(), sig2.clone()].iter());

        // Create AggregatedSignature from pre-aggregated signature (no validation)
        let agg_sig = AggregatedSignature::new_from_aggregated_signature(
            [pk1.clone(), pk2.clone()],
            aggregated_sig.clone(),
        );

        // Should verify correctly
        assert!(agg_sig.verify(message));

        // Test with invalid pre-aggregated signature (doesn't validate)
        let mut rng2 = StdRng::from_seed([13u8; 32]);
        let sk_wrong = BlsSecretKey::generate(&mut rng2);
        let wrong_sig = sk_wrong.sign(b"different message");
        let invalid_agg_sig = AggregatedSignature::new_from_aggregated_signature(
            [pk1.clone(), pk2.clone()],
            wrong_sig,
        );

        // This should fail verification (signature is wrong)
        assert!(!invalid_agg_sig.verify(message));
    }

    #[test]
    fn test_aggregated_signature_different_sizes() {
        let mut rng = StdRng::from_seed([14u8; 32]);

        // Test with different const generic sizes
        let sk1 = BlsSecretKey::generate(&mut rng);
        let sk2 = BlsSecretKey::generate(&mut rng);
        let sk3 = BlsSecretKey::generate(&mut rng);

        let pk1 = sk1.public_key();
        let pk2 = sk2.public_key();
        let pk3 = sk3.public_key();

        let message = b"different sizes test";

        let sig1 = sk1.sign(message);
        let sig2 = sk2.sign(message);
        let sig3 = sk3.sign(message);

        // Test AggregatedSignature<2>
        let agg_2 = AggregatedSignature::<2>::new(
            [pk1.clone(), pk2.clone()],
            message,
            &[sig1.clone(), sig2.clone()],
        );
        assert!(agg_2.is_some());
        assert!(agg_2.unwrap().verify(message));

        // Test AggregatedSignature<3>
        let agg_3 = AggregatedSignature::<3>::new(
            [pk1.clone(), pk2.clone(), pk3.clone()],
            message,
            &[sig1.clone(), sig2.clone(), sig3.clone()],
        );
        assert!(agg_3.is_some());
        assert!(agg_3.unwrap().verify(message));

        // Test AggregatedSignature<1>
        let agg_1 =
            AggregatedSignature::<1>::new([pk1.clone()], message, std::slice::from_ref(&sig1));
        assert!(agg_1.is_some());
        assert!(agg_1.unwrap().verify(message));
    }

    #[test]
    fn test_aggregated_signature_verification_properties() {
        let mut rng = StdRng::from_seed([15u8; 32]);

        // Create key pairs
        let sk1 = BlsSecretKey::generate(&mut rng);
        let sk2 = BlsSecretKey::generate(&mut rng);

        let pk1 = sk1.public_key();
        let pk2 = sk2.public_key();

        let message = b"verification properties test";
        let wrong_message = b"wrong message";

        let sig1 = sk1.sign(message);
        let sig2 = sk2.sign(message);

        let aggregated = AggregatedSignature::new(
            [pk1.clone(), pk2.clone()],
            message,
            &[sig1.clone(), sig2.clone()],
        )
        .unwrap();

        // Should verify with correct message
        assert!(aggregated.verify(message));

        // Should not verify with wrong message
        assert!(!aggregated.verify(wrong_message));

        // Should not verify if we tamper with the aggregated signature
        let mut tampered = aggregated.clone();
        let mut rng2 = StdRng::from_seed([16u8; 32]);
        let sk_tamper = BlsSecretKey::generate(&mut rng2);
        tampered.aggregated_signature = sk_tamper.sign(message);
        assert!(!tampered.verify(message));

        // Should not verify if we change the public keys
        let mut rng3 = StdRng::from_seed([17u8; 32]);
        let sk3 = BlsSecretKey::generate(&mut rng3);
        let pk3 = sk3.public_key();
        let mut wrong_keys = aggregated.clone();
        wrong_keys.public_keys = [pk1.clone(), pk3];
        assert!(!wrong_keys.verify(message));
    }
}
