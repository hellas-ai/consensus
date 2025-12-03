use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use ark_serialize::CanonicalSerialize;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use rkyv::de::Pool;
use rkyv::rancor::Strategy;
use rkyv::util::AlignedVec;
use rkyv::{Archive, Serialize, deserialize};

use crate::crypto::transaction_crypto::TxPublicKey;
use crate::state::account::Account;
use crate::state::notarizations::Vote;
use crate::state::nullify::Nullify;
use crate::state::{
    block::Block, leader::Leader, notarizations::MNotarization, nullify::Nullification,
    transaction::Transaction, view::View,
};
use crate::storage::config::StorageConfig;
use crate::storage::conversions::Storable;
use crate::storage::tables::{ACCOUNTS, NULLIFIED_BLOCKS, NULLIFIES};

use super::{
    conversions::access_archived,
    tables::{
        FINALIZED_BLOCKS, LEADERS, MEMPOOL, NON_FINALIZED_BLOCKS, NOTARIZATIONS, NULLIFICATIONS,
        STATE, VIEWS, VOTES,
    },
};

/// [`ConsensusStore`] is a wrapper around the redb database that provides a convenient interface
/// for storing and retrieving consensus data.
///
/// It provides methods for storing and retrieving blocks, leaders, views, transactions,
/// notarizations, nullifications, and accounts.
///
/// # Examples
///
/// ```rust,ignore
/// let store = ConsensusStore::open("path/to/database").unwrap();
/// let block = Block::new(1, 0, [0; 32], vec![], 100, false, 1);
/// store.pub_block(&block).unwrap();
/// let fetched = store.get_block(&block.get_hash()).unwrap().expect("get block");
/// assert_eq!(fetched.view(), 1);
/// ```
#[derive(Clone)]
pub struct ConsensusStore {
    db: Arc<Database>,
}

impl ConsensusStore {
    /// Opens a database from a path to the database file.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let db = if path.as_ref().exists() {
            Database::open(path).context("Failed to open database")?
        } else {
            Database::create(path).context("Failed to create database")?
        };
        let consensus_store = Self { db: Arc::new(db) };
        consensus_store.init_tables()?;
        Ok(consensus_store)
    }

    /// Opens a database from a configuration path.
    pub fn from_config_path<P: AsRef<Path>>(config_path: P) -> Result<Self> {
        let config = StorageConfig::from_path(config_path)?;
        Self::open(config.path)
    }

    /// Initializes the tables in the database
    fn init_tables(&self) -> Result<()> {
        let write_txn = self
            .db
            .begin_write()
            .context("Failed to begin write transaction")?;
        {
            write_txn
                .open_table(FINALIZED_BLOCKS)
                .context("Failed to open finalized blocks table")?;
            write_txn
                .open_table(NON_FINALIZED_BLOCKS)
                .context("Failed to open non-finalized blocks table")?;
            write_txn
                .open_table(NULLIFIED_BLOCKS)
                .context("Failed to open nullified blocks table")?;
            write_txn
                .open_table(LEADERS)
                .context("Failed to open leaders table")?;
            write_txn
                .open_table(MEMPOOL)
                .context("Failed to open mempool table")?;
            write_txn
                .open_table(NOTARIZATIONS)
                .context("Failed to open notarizations table")?;
            write_txn
                .open_table(NULLIFICATIONS)
                .context("Failed to open nullifications table")?;
            write_txn
                .open_table(STATE)
                .context("Failed to open state table")?;
            write_txn
                .open_table(VIEWS)
                .context("Failed to open views table")?;
            write_txn
                .open_table(VOTES)
                .context("Failed to open votes table")?;
        }
        write_txn
            .commit()
            .context("Failed to commit write transaction")
    }

    /// Puts a value into the database.
    /// The value is owned by the caller and will be freed when the database is closed.
    fn put_value<T>(&self, table: TableDefinition<&[u8], &[u8]>, value: &T) -> Result<()>
    where
        T: for<'a> Serialize<
                rkyv::api::high::HighSerializer<
                    AlignedVec,
                    rkyv::ser::allocator::ArenaHandle<'a>,
                    rkyv::rancor::Error,
                >,
            > + Archive,
        T: Storable,
    {
        let key = value.key();
        let bytes = value.value()?;

        let write_txn = self
            .db
            .begin_write()
            .context("Failed to begin write transaction")?;
        {
            let mut table = write_txn
                .open_table(table)
                .context("Failed to open table")?;
            table
                .insert(key.as_ref(), bytes.as_ref())
                .context("Failed to insert value")?;
        }
        write_txn
            .commit()
            .context("Failed to commit write transaction")
    }

    /// Gets a value from the database.
    /// The value is owned by the database and will be freed when the database is closed.
    unsafe fn get_blob_value<T, K>(
        &self,
        table: TableDefinition<&[u8], &[u8]>,
        key: K,
    ) -> Result<Option<T>>
    where
        T: Archive,
        <T as Archive>::Archived: rkyv::Deserialize<T, Strategy<Pool, rkyv::rancor::Error>>,
        K: AsRef<[u8]>,
    {
        let read = self.db.begin_read()?;
        let t = read.open_table(table)?;
        if let Some(row) = t.get(key.as_ref())? {
            let val = row.value();
            let mut aligned = AlignedVec::<1024>::with_capacity(val.len());
            aligned.extend_from_slice(val);
            let val = unsafe { access_archived::<T>(aligned.as_slice()) };
            Ok(Some(deserialize(val).map_err(|e| {
                anyhow::anyhow!("Failed to deserialize: {:?}", e)
            })?))
        } else {
            Ok(None)
        }
    }

    /// Gets an aligned value from the database.
    /// The value is owned by the database and will be freed when the database is closed.
    #[allow(unused)]
    unsafe fn get_blob_aligned<T, K>(
        &self,
        table: TableDefinition<&[u8], &[u8]>,
        key: K,
    ) -> Result<Option<AlignedVec<1024>>>
    where
        T: Archive,
        <T as Archive>::Archived: rkyv::Deserialize<T, Strategy<Pool, rkyv::rancor::Error>>,
        K: AsRef<[u8]>,
    {
        let read = self.db.begin_read()?;
        let t = read.open_table(table)?;
        if let Some(row) = t.get(key.as_ref())? {
            let val = row.value();
            let mut aligned = AlignedVec::<1024>::with_capacity(val.len());
            aligned.extend_from_slice(val);
            Ok(Some(aligned))
        } else {
            Ok(None)
        }
    }

    /// Puts a finalized block into the database.
    pub fn put_finalized_block(&self, block: &Block) -> Result<()> {
        self.put_value(FINALIZED_BLOCKS, block)
    }

    /// Retrieves a finalized block from the database, if it exists.
    pub fn get_finalized_block(&self, hash: &[u8; blake3::OUT_LEN]) -> Result<Option<Block>> {
        unsafe { self.get_blob_value::<Block, _>(FINALIZED_BLOCKS, *hash) }
    }

    /// Retrieves all finalized blocks from the database.
    pub fn get_all_finalized_blocks(&self) -> Result<Vec<Block>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(FINALIZED_BLOCKS)?;
        let mut blocks = Vec::new();

        for result in table.iter()? {
            let (_, value) = result?;
            let val = value.value();
            let mut aligned = AlignedVec::<1024>::with_capacity(val.len());
            aligned.extend_from_slice(val);
            let archived = unsafe { access_archived::<Block>(aligned.as_slice()) };
            let block: Block = deserialize(archived).map_err(|e: rkyv::rancor::Error| {
                anyhow::anyhow!("Failed to deserialize: {:?}", e)
            })?;
            blocks.push(block);
        }

        // Sort blocks by view number to ensure consistent order
        blocks.sort_by_key(|b| b.view());

        Ok(blocks)
    }

    /// Puts a non-finalized block into the database.
    pub fn put_non_finalized_block(&self, block: &Block) -> Result<()> {
        self.put_value(NON_FINALIZED_BLOCKS, block)
    }

    /// Retrieves a non-finalized block from the database, if it exists.
    pub fn get_non_finalized_block(&self, hash: &[u8; blake3::OUT_LEN]) -> Result<Option<Block>> {
        unsafe { self.get_blob_value::<Block, _>(NON_FINALIZED_BLOCKS, *hash) }
    }

    /// Puts a nullified block into the database.
    pub fn put_nullified_block(&self, block: &Block) -> Result<()> {
        self.put_value(NULLIFIED_BLOCKS, block)
    }

    /// Retrieves a nullified block from the database, if it exists.
    pub fn get_nullified_block(&self, hash: &[u8; blake3::OUT_LEN]) -> Result<Option<Block>> {
        unsafe { self.get_blob_value::<Block, _>(NULLIFIED_BLOCKS, *hash) }
    }

    /// Puts a vote into the database.
    pub fn put_vote(&self, vote: &Vote) -> Result<()> {
        self.put_value(VOTES, vote)
    }

    /// Retrieves a vote from the database, if it exists.
    pub fn get_vote(&self, vote_key: [u8; blake3::OUT_LEN]) -> Result<Option<Vote>> {
        unsafe { self.get_blob_value::<Vote, _>(VOTES, vote_key) }
    }

    /// Puts a leader into the database.
    pub fn put_leader(&self, leader: &Leader) -> Result<()> {
        self.put_value(LEADERS, leader)
    }

    /// Retrieves a leader from the database, if it exists.
    pub fn get_leader(&self, view: u64) -> Result<Option<Leader>> {
        unsafe { self.get_blob_value::<Leader, _>(LEADERS, view.to_le_bytes()) }
    }

    /// Puts a view into the database.
    pub fn put_view(&self, view: &View) -> Result<()> {
        self.put_value(VIEWS, view)
    }

    /// Retrieves a view from the database, if it exists.
    pub fn get_view(&self, view: u64) -> Result<Option<View>> {
        unsafe { self.get_blob_value::<View, _>(VIEWS, view.to_le_bytes()) }
    }

    /// Puts a transaction into the database.
    pub fn put_transaction(&self, transaction: &Transaction) -> Result<()> {
        self.put_value(MEMPOOL, transaction)
    }

    /// Retrieves a transaction from the database, if it exists.
    pub fn get_transaction(&self, hash: &[u8; blake3::OUT_LEN]) -> Result<Option<Transaction>> {
        unsafe { self.get_blob_value::<Transaction, _>(MEMPOOL, *hash) }
    }

    /// Puts a notarization into the database.
    pub fn put_notarization<const N: usize, const F: usize, const M_SIZE: usize>(
        &self,
        notarization: &MNotarization<N, F, M_SIZE>,
    ) -> Result<()> {
        self.put_value(NOTARIZATIONS, notarization)
    }

    /// Retrieves a notarization from the database, if it exists.
    pub fn get_notarization<const N: usize, const F: usize, const M_SIZE: usize>(
        &self,
        hash: &[u8; blake3::OUT_LEN],
    ) -> Result<Option<MNotarization<N, F, M_SIZE>>> {
        unsafe { self.get_blob_value::<MNotarization<N, F, M_SIZE>, _>(NOTARIZATIONS, *hash) }
    }

    /// Puts a nullify message into the database.
    pub fn put_nullify(&self, nullify: &Nullify) -> Result<()> {
        self.put_value(NULLIFIES, nullify)
    }

    /// Retrieves a nullify message from the database, if it exists.
    pub fn get_nullify(&self, view: u64) -> Result<Option<Nullify>> {
        unsafe { self.get_blob_value::<Nullify, _>(NULLIFIES, view.to_le_bytes()) }
    }

    /// Puts a nullification into the database.
    pub fn put_nullification<const N: usize, const F: usize, const L_SIZE: usize>(
        &self,
        nullification: &Nullification<N, F, L_SIZE>,
    ) -> Result<()> {
        self.put_value(NULLIFICATIONS, nullification)
    }

    /// Retrieves a nullification from the database, if it exists.
    pub fn get_nullification<const N: usize, const F: usize, const L_SIZE: usize>(
        &self,
        view: u64,
    ) -> Result<Option<Nullification<N, F, L_SIZE>>> {
        unsafe {
            self.get_blob_value::<Nullification<N, F, L_SIZE>, _>(
                NULLIFICATIONS,
                view.to_le_bytes(),
            )
        }
    }

    /// Puts an account into the database.
    pub fn put_account(&self, account: &Account) -> Result<()> {
        self.put_value(ACCOUNTS, account)
    }

    /// Retrieves an account from the database, if it exists.
    pub fn get_account(&self, public_key: &TxPublicKey) -> Result<Option<Account>> {
        let mut writer = Vec::new();
        public_key
            .to_bytes()
            .serialize_compressed(&mut writer)
            .unwrap();
        unsafe { self.get_blob_value::<Account, _>(ACCOUNTS, writer) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::{
            aggregated::{AggregatedSignature, BlsPublicKey, BlsSecretKey, PeerId},
            transaction_crypto::{TxPublicKey, TxSecretKey},
        },
        state::{address::Address, peer::PeerSet},
    };

    fn temp_db_path(suffix: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "consensus_store_test-{}-{}.redb",
            suffix,
            rand::random::<u64>()
        ));
        p.to_string_lossy().to_string()
    }

    fn gen_keypair() -> (TxSecretKey, TxPublicKey) {
        let sk = TxSecretKey::generate();
        let pk = sk.public_key();
        (sk, pk)
    }

    fn gen_bls_keypair() -> (BlsSecretKey, BlsPublicKey) {
        let sk = BlsSecretKey::generate(&mut rand::thread_rng());
        let pk = sk.public_key();
        (sk, pk)
    }

    fn serialize_tx_pk(pk: &TxPublicKey) -> Vec<u8> {
        let mut v = Vec::new();
        pk.to_bytes().serialize_compressed(&mut v).unwrap();
        v
    }

    fn serialize_bls_pk(pk: &BlsPublicKey) -> Vec<u8> {
        let mut v = Vec::new();
        pk.serialize_compressed(&mut v).unwrap();
        v
    }

    #[test]
    fn open_creates_and_initializes_tables() {
        let path = temp_db_path("open");
        {
            let store = ConsensusStore::open(&path).expect("open/create db");
            // touching multiple tables via a write ensures tables are usable
            let (_sk, pk) = gen_keypair();
            let acct = Account::new(pk.clone(), 100, 1);
            store.put_account(&acct).expect("pub account");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn finalized_block_roundtrip() {
        let path = temp_db_path("block");
        {
            let store = ConsensusStore::open(&path).unwrap();

            // Prepare a transaction to embed in block
            let (sk, pk) = gen_keypair();
            let tx = Transaction::new_transfer(
                Address::from_public_key(&pk),
                Address::from_bytes([7u8; 32]),
                42,
                9,
                1_000,
                &sk,
            );

            let parent: [u8; blake3::OUT_LEN] = [1u8; blake3::OUT_LEN];
            let (leader_sk, leader_pk) = gen_bls_keypair();
            let signature = leader_sk.sign(b"block proposal");
            let block = Block::new(
                5,
                leader_pk.to_peer_id(),
                parent,
                vec![tx],
                123456,
                signature,
                false,
                1,
            );

            store.put_finalized_block(&block).unwrap();
            let h = block.get_hash();
            let fetched = store
                .get_finalized_block(&h)
                .unwrap()
                .expect("get finalized block");
            assert_eq!(fetched.get_hash(), block.get_hash());
            assert_eq!(fetched.view(), 5);
            assert_eq!(fetched.parent_block_hash(), parent);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn non_finalized_block_roundtrip() {
        let path = temp_db_path("block");
        {
            let store = ConsensusStore::open(&path).unwrap();

            // Prepare a transaction to embed in block
            let (sk, pk) = gen_keypair();
            let tx = Transaction::new_transfer(
                Address::from_public_key(&pk),
                Address::from_bytes([7u8; 32]),
                42,
                9,
                1_000,
                &sk,
            );

            let parent: [u8; blake3::OUT_LEN] = [1u8; blake3::OUT_LEN];
            let (leader_sk, _leader_pk) = gen_bls_keypair();
            let signature = leader_sk.sign(b"block proposal");
            let block = Block::new(5, 0, parent, vec![tx], 123456, signature, false, 1);

            store.put_non_finalized_block(&block).unwrap();
            let h = block.get_hash();
            let fetched = store
                .get_non_finalized_block(&h)
                .unwrap()
                .expect("get non-finalized block");
            assert_eq!(fetched.get_hash(), block.get_hash());
            assert_eq!(fetched.view(), 5);
            assert_eq!(fetched.parent_block_hash(), parent);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn nullified_block_roundtrip() {
        let path = temp_db_path("block");
        {
            let store = ConsensusStore::open(&path).unwrap();

            // Prepare a transaction to embed in block
            let (sk, pk) = gen_keypair();
            let tx = Transaction::new_transfer(
                Address::from_public_key(&pk),
                Address::from_bytes([7u8; 32]),
                42,
                9,
                1_000,
                &sk,
            );

            let parent: [u8; blake3::OUT_LEN] = [1u8; blake3::OUT_LEN];
            let (leader_sk, _leader_pk) = gen_bls_keypair();
            let signature = leader_sk.sign(b"block proposal");
            let block = Block::new(5, 0, parent, vec![tx], 123456, signature, false, 1);

            store.put_nullified_block(&block).unwrap();
            let h = block.get_hash();
            let fetched = store
                .get_nullified_block(&h)
                .unwrap()
                .expect("get nullified block");
            assert_eq!(fetched.get_hash(), block.get_hash());
            assert_eq!(fetched.view(), 5);
            assert_eq!(fetched.parent_block_hash(), parent);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn leader_roundtrip() {
        let path = temp_db_path("leader");
        {
            let store = ConsensusStore::open(&path).unwrap();

            let (_leader_sk, leader_pk) = gen_bls_keypair();
            let leader_id = leader_pk.to_peer_id();
            let leader = Leader::new(leader_id, 10);

            store.put_leader(&leader).unwrap();
            let fetched = store.get_leader(10).unwrap().expect("get leader");

            // Compare fields; `BlsPublicKey` lacks PartialEq, compare bytes instead.
            assert_eq!(fetched.view(), 10);
            assert_eq!(fetched.peer_id(), leader_pk.to_peer_id());
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn view_roundtrip() {
        let path = temp_db_path("view");
        {
            let store = ConsensusStore::open(&path).unwrap();

            let (_leader_sk, leader_pk) = gen_bls_keypair();
            let view = View::new(11, leader_pk.clone(), true, false);

            store.put_view(&view).unwrap();
            let fetched = store.get_view(11).unwrap().expect("get view");

            assert_eq!(fetched.view(), 11);
            assert!(fetched.is_current_view());
            assert!(!fetched.is_nullified());
            assert_eq!(
                serialize_bls_pk(fetched.leader()),
                serialize_bls_pk(&leader_pk.clone())
            );
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn transaction_roundtrip() {
        let path = temp_db_path("tx");
        {
            let store = ConsensusStore::open(&path).unwrap();

            let (sk, pk) = gen_keypair();
            let tx = Transaction::new_transfer(
                Address::from_public_key(&pk),
                Address::from_bytes([9u8; 32]),
                100,
                2,
                2_000,
                &sk,
            );

            store.put_transaction(&tx).unwrap();
            let fetched = store.get_transaction(&tx.tx_hash).unwrap().expect("get tx");

            assert_eq!(fetched, tx);
            assert!(fetched.verify());
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn notarization_roundtrip() {
        const N: usize = 3;
        const F: usize = 1;
        const M_SIZE: usize = 3;

        let path = temp_db_path("mnotar");
        {
            let store = ConsensusStore::open(&path).unwrap();

            // Base block
            let parent: [u8; blake3::OUT_LEN] = [2u8; blake3::OUT_LEN];
            let (sk0, pk0) = gen_keypair();
            let tx0 = Transaction::new_transfer(
                Address::from_public_key(&pk0),
                Address::from_bytes([1u8; 32]),
                1,
                0,
                1,
                &sk0,
            );
            let (leader_sk, leader_pk) = gen_bls_keypair();
            let signature = leader_sk.sign(b"block proposal");
            let block = Block::new(
                6,
                leader_pk.to_peer_id(),
                parent,
                vec![tx0],
                999,
                signature,
                false,
                1,
            );

            // 3 signers aggregate over block hash
            let (sk1, pk1) = gen_bls_keypair();
            let (sk2, pk2) = gen_bls_keypair();
            let (sk3, pk3) = gen_bls_keypair();

            let msg = block.get_hash();
            let s1 = sk1.sign(&msg);
            let s2 = sk2.sign(&msg);
            let s3 = sk3.sign(&msg);

            let pks_vec = vec![pk1.clone(), pk2.clone(), pk3.clone()];
            let pks: [BlsPublicKey; M_SIZE] = pks_vec.try_into().unwrap();
            let sigs = vec![s1, s2, s3];

            let agg = AggregatedSignature::<M_SIZE>::new(pks.clone(), &msg, &sigs).expect("agg");
            let m = MNotarization::<N, F, M_SIZE>::new(
                6,
                block.get_hash(),
                agg.aggregated_signature,
                pks.iter()
                    .map(|pk| pk.to_peer_id())
                    .collect::<Vec<PeerId>>()
                    .try_into()
                    .unwrap(),
                0,
            );

            store.put_notarization(&m).unwrap();
            let h = block.get_hash();
            let fetched = store
                .get_notarization::<N, F, M_SIZE>(&h)
                .unwrap()
                .expect("get mnotar");

            assert_eq!(fetched.block_hash, block.get_hash());
            assert!(fetched.verify(&PeerSet::new(pks.to_vec())));
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn account_roundtrip() {
        let path = temp_db_path("account");
        {
            let store = ConsensusStore::open(&path).unwrap();

            let (_sk, pk) = gen_keypair();
            let acct = Account::new(pk.clone(), 1234, 7);

            store.put_account(&acct).unwrap();
            let fetched = store.get_account(&pk).unwrap().expect("get account");

            assert_eq!(
                serialize_tx_pk(&TxPublicKey::from_bytes(&fetched.public_key.bytes).unwrap()),
                serialize_tx_pk(&TxPublicKey::from_bytes(&acct.public_key.bytes).unwrap())
            );
            assert_eq!(fetched.balance, 1234);
            assert_eq!(fetched.nonce, 7);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn nullification_roundtrip() {
        const N: usize = 3;
        const F: usize = 1;
        const M_SIZE: usize = 3;

        let path = temp_db_path("nullif");
        {
            let store = ConsensusStore::open(&path).unwrap();

            // Generate matching keypairs
            let (sk1, pk1) = gen_bls_keypair();
            let (sk2, pk2) = gen_bls_keypair();
            let (sk3, pk3) = gen_bls_keypair();

            // Aggregate signature over view bytes
            let view: u64 = 77;
            let leader_id: PeerId = 1;
            let msg = blake3::hash(&[view.to_le_bytes(), leader_id.to_le_bytes()].concat());

            // Sign with the corresponding secret keys
            let s1 = sk1.sign(msg.as_bytes());
            let s2 = sk2.sign(msg.as_bytes());
            let s3 = sk3.sign(msg.as_bytes());

            let pks_vec = vec![pk1.clone(), pk2.clone(), pk3.clone()];
            let pks: [BlsPublicKey; M_SIZE] = pks_vec.try_into().unwrap();

            let agg =
                AggregatedSignature::<M_SIZE>::new(pks.clone(), msg.as_bytes(), &[s1, s2, s3])
                    .expect("Failed to create aggregated signature");

            let nullif = Nullification::<N, F, M_SIZE>::new(
                view,
                1,
                agg.aggregated_signature,
                pks.iter()
                    .map(|pk| pk.to_peer_id())
                    .collect::<Vec<PeerId>>()
                    .try_into()
                    .unwrap(),
            );
            store.put_nullification(&nullif).unwrap();

            let fetched = store
                .get_nullification::<N, F, M_SIZE>(view)
                .unwrap()
                .expect("get nullification");
            assert_eq!(fetched.view, view);
            assert_eq!(fetched.leader_id, 1);
            assert!(fetched.verify(&PeerSet::new(pks.to_vec())));
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn vote_roundtrip() {
        let path = temp_db_path("vote");
        {
            let store = ConsensusStore::open(&path).unwrap();

            let (sk, pk) = gen_bls_keypair();
            let view: u64 = 42;
            let block_hash: [u8; blake3::OUT_LEN] = [5u8; blake3::OUT_LEN];
            let sig = sk.sign(&block_hash);

            let vote = Vote::new(view, block_hash, sig, pk.to_peer_id(), 1);

            store.put_vote(&vote).unwrap();
            let vote_key = vote.key();
            let fetched = store.get_vote(vote_key).unwrap().expect("get vote");

            assert_eq!(fetched.view, view);
            assert_eq!(fetched.block_hash, block_hash);
            assert_eq!(fetched.peer_id, pk.to_peer_id());
            assert_eq!(fetched.leader_id, 1);
            assert!(fetched.verify(&pk));
        }
        std::fs::remove_file(&path).ok();
    }
}
