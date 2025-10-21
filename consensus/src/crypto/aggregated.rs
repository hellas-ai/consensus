use std::str::FromStr;

use ark_bls12_381::{Bls12_381, Fr, G1Affine, G1Projective, G2Affine, G2Projective};
use ark_ec::{AffineRepr, CurveGroup, PrimeGroup, pairing::Pairing};
use ark_ff::{PrimeField, UniformRand, Zero};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};

pub type PeerId = u64;

#[derive(Clone, Debug, CanonicalDeserialize, CanonicalSerialize)]
pub struct BlsPublicKey(pub G2Affine);

#[derive(Clone, Debug, CanonicalDeserialize, CanonicalSerialize)]
pub struct BlsSignature(pub G1Affine);

#[derive(Clone, Debug, CanonicalDeserialize, CanonicalSerialize)]
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
        let mut buff = [0u8; blake3::OUT_LEN];
        let mut hasher = blake3::Hasher::new();
        self.0.serialize_compressed(&mut buff[..]).unwrap();
        hasher.update(&buff[..]);
        let hash = hasher.finalize();
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
    pub fn aggregate(signatures: &[BlsSignature]) -> BlsSignature {
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

        let aggregated_signature = BlsSignature::aggregate(signatures);

        Some(AggregatedSignature {
            aggregated_signature,
            public_keys: public_keys.clone(),
        })
    }

    /// Verify the aggregated signature
    pub fn verify(&self, message: &[u8]) -> bool {
        BlsPublicKey::verify_aggregate(&self.public_keys, message, &self.aggregated_signature)
    }
}
