use ark_bls12_381::{Bls12_381, Fr, G1Affine, G1Projective, G2Affine, G2Projective};
use ark_ec::{AffineRepr, CurveGroup, PrimeGroup, pairing::Pairing};
use ark_ff::{PrimeField, UniformRand, Zero};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use rand::rngs::OsRng;
use redb::{Database, ReadableDatabase, TableDefinition};
use rkyv::{
    Archive, Deserialize, Serialize, api::high::to_bytes_with_alloc, deserialize, rancor::Error,
    ser::allocator::Arena, util::AlignedVec,
};

use crate::{
    crypto::aggregated::PeerId,
    state::notarizations::{ArchivedMNotarization, MNotarization},
};

pub mod consensus;
pub mod crypto;
pub mod state;
pub mod storage;
pub mod view_manager;

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("blocks");

#[derive(Archive, Deserialize, Serialize, Debug)]
struct Block {
    view: u64,
    parent_hash: String,
    transactions: Vec<Transaction>,
}

#[derive(Archive, Deserialize, Serialize, Debug)]
struct Transaction {
    hash: String,
}

#[derive(Clone, Debug, CanonicalDeserialize, CanonicalSerialize)]
pub struct BlsPublicKey(pub G2Affine);

#[derive(Clone, Debug, CanonicalDeserialize, CanonicalSerialize)]
pub struct BlsSignature(pub G1Affine);

#[derive(Clone, Debug, CanonicalDeserialize, CanonicalSerialize)]
pub struct BlsSecretKey(pub Fr);

#[derive(Clone, Debug, CanonicalDeserialize, CanonicalSerialize)]
pub struct AggregatedSignature {
    pub aggregated_signature: BlsSignature,
    pub aggregated_public_key: BlsPublicKey,
    pub participant_count: usize,
}

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
    pub fn verify_aggregate(
        aggregated_public_key: &BlsPublicKey,
        message: &[u8],
        aggregated_signature: &BlsSignature,
    ) -> bool {
        // e(aggregated_signature, g2) == e(H(message), aggregated_public_key)
        let g2 = G2Affine::generator();
        let hash_point = Self::hash_to_g1(message);

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

impl AggregatedSignature {
    pub fn new(
        public_keys: &[BlsPublicKey],
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
        let aggregated_public_key = BlsPublicKey::aggregate(public_keys);

        Some(AggregatedSignature {
            aggregated_signature,
            aggregated_public_key,
            participant_count: public_keys.len(),
        })
    }

    /// Verify the aggregated signature
    pub fn verify(&self, message: &[u8]) -> bool {
        BlsPublicKey::verify_aggregate(
            &self.aggregated_public_key,
            message,
            &self.aggregated_signature,
        )
    }
}

fn main() {
    let mut public_keys = Vec::with_capacity(100);
    for _ in 0..100 {
        let secret_key = crate::crypto::aggregated::BlsSecretKey::generate(&mut OsRng);
        let public_key = secret_key.public_key();
        public_keys.push(public_key);
    }

    let block = crate::state::notarizations::MNotarization::<100, 1, 100> {
        view: 0,
        block_hash: [0; 32],
        aggregated_signature: crate::crypto::aggregated::BlsSignature(G1Affine::zero()),
        peer_ids: public_keys
            .iter()
            .map(|pk| pk.to_peer_id())
            .collect::<Vec<PeerId>>()
            .try_into()
            .unwrap(),
    };
    let mut arena = Arena::new();
    let start = std::time::Instant::now();
    let serialized =
        to_bytes_with_alloc::<_, Error>(&block, arena.acquire()).expect("Failed to serialize");
    let end = std::time::Instant::now();
    println!("Serialize time: {:?}", end - start);
    let start = std::time::Instant::now();
    let archived = rkyv::access::<ArchivedMNotarization<5, 1, 3>, Error>(&serialized[..])
        .expect("Failed to access");
    let end = std::time::Instant::now();
    println!("Access time: {:?}", end - start);
    let start = std::time::Instant::now();
    let _deserialized =
        deserialize::<MNotarization<5, 1, 3>, Error>(archived).expect("Failed to deserialize");
    let end = std::time::Instant::now();
    println!("Deserialize time: {:?}", end - start);

    // Ark BLS signature
    let secret_key = BlsSecretKey::generate(&mut OsRng);
    let public_key = secret_key.public_key();
    let message = b"Hello, world!";
    let start = std::time::Instant::now();
    let signature = secret_key.sign(message);
    let end = std::time::Instant::now();
    println!("Signature time: {:?}", end - start);

    // Verify single signature
    let start = std::time::Instant::now();
    let verified = public_key.verify(message, &signature);
    let end = std::time::Instant::now();
    println!("Verify time: {:?}", end - start);
    println!("Verified: {}", verified);

    // Ark BLS aggregation
    let mut public_keys = Vec::with_capacity(5);
    let mut signatures = Vec::with_capacity(5);
    for _ in 0..5 {
        let secret_key = BlsSecretKey::generate(&mut OsRng);
        let public_key = secret_key.public_key();
        let signature = secret_key.sign(message);

        public_keys.push(public_key.clone());
        signatures.push(signature.clone());
    }
    let aggregated_signature = AggregatedSignature::new(&public_keys, message, &signatures)
        .expect("Failed to aggregate signature");
    println!("Aggregated signature: {:?}", aggregated_signature);

    // Time for canonical serialization and deserialization
    let start = std::time::Instant::now();
    let mut canonical_serialized = Vec::new();
    aggregated_signature
        .serialize_uncompressed(&mut canonical_serialized)
        .expect("Failed to serialize");
    let end = std::time::Instant::now();
    println!("Canonical serialize time: {:?}", end - start);
    let start = std::time::Instant::now();
    let _canonical_deserialized =
        AggregatedSignature::deserialize_uncompressed(&canonical_serialized[..])
            .expect("Failed to deserialize");
    let end = std::time::Instant::now();
    println!("Canonical deserialize time: {:?}", end - start);

    let verified = aggregated_signature.verify(message);
    println!("Verified: {}", verified);

    std::fs::create_dir_all("data").expect("Failed to create database directory");
    Database::create("data/test.redb").expect("Failed to create database");
    let db = redb::Database::open("data/test.redb").expect("Failed to open database");
    let block_buffer = serialized.as_slice();

    let write_tx = db.begin_write().expect("Failed to begin write transaction");

    {
        let mut table = write_tx.open_table(TABLE).expect("Failed to create table");

        let start = std::time::Instant::now();
        table
            .insert("block", &block_buffer)
            .expect("Failed to put block");
        let end = std::time::Instant::now();
        println!("Put block time: {:?}", end - start);
    }

    let start = std::time::Instant::now();
    write_tx
        .commit()
        .expect("Failed to commit write transaction");
    let end = std::time::Instant::now();
    println!("Commit time: {:?}", end - start);

    let start = std::time::Instant::now();
    let read_tx = db.begin_read().expect("Failed to begin read transaction");
    let end = std::time::Instant::now();
    println!("Begin read transaction time: {:?}", end - start);

    let start = std::time::Instant::now();
    let table = read_tx.open_table(TABLE).expect("Failed to create table");
    let end = std::time::Instant::now();
    println!("Open table time: {:?}", end - start);

    let start = std::time::Instant::now();
    let block = table
        .get("block")
        .expect("Failed to get block")
        .expect("Failed to get block");
    let end = std::time::Instant::now();
    println!("Get block time: {:?}", end - start);

    let new_block_buffer = block.value();
    assert_eq!(block_buffer.len(), new_block_buffer.len());
    assert_eq!(block_buffer, new_block_buffer);

    let start = std::time::Instant::now();
    let mut aligned_buffer = AlignedVec::<1024>::with_capacity(new_block_buffer.len());
    let end = std::time::Instant::now();
    println!("Align vec time: {:?}", end - start);

    let start = std::time::Instant::now();
    aligned_buffer.extend_from_slice(new_block_buffer);
    let end = std::time::Instant::now();
    println!("Extend from slice time: {:?}", end - start);

    let start = std::time::Instant::now();
    let archived =
        unsafe { rkyv::access_unchecked::<ArchivedMNotarization<5, 1, 3>>(&aligned_buffer) };
    let end = std::time::Instant::now();
    println!("Access unchecked time: {:?}", end - start);

    let start = std::time::Instant::now();
    let _deserialized =
        deserialize::<MNotarization<5, 1, 3>, Error>(archived).expect("Failed to deserialize");
    let end = std::time::Instant::now();
    println!("Deserialize time: {:?}", end - start);
}
