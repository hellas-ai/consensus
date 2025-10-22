use std::path::Path;

use anyhow::{Context, Result};
use ark_serialize::CanonicalSerialize;
use redb::{Database, ReadableDatabase, TableDefinition};
use rkyv::de::Pool;
use rkyv::rancor::Strategy;
use rkyv::util::AlignedVec;
use rkyv::{Archive, Serialize, deserialize};

use crate::crypto::aggregated::BlsPublicKey;
use crate::state::account::Account;
use crate::state::{
    block::Block, leader::Leader, notarizations::MNotarization, nullify::Nullification,
    transaction::Transaction, view::View,
};
use crate::storage::config::StorageConfig;
use crate::storage::conversions::Storable;
use crate::storage::tables::ACCOUNTS;

use super::{
    conversions::access_archived,
    tables::{BLOCKS, LEADERS, MEMPOOL, NOTARIZATIONS, NULLIFICATIONS, STATE, VIEWS, VOTES},
};

pub struct ConsensusStore {
    db: Database,
}

impl ConsensusStore {
    /// Opens a database from a path to the database file.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let db = if path.as_ref().exists() {
            Database::open(path).context("Failed to open database")?
        } else {
            Database::create(path).context("Failed to create database")?
        };
        let consensus_store = Self { db };
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
                .open_table(BLOCKS)
                .context("Failed to open blocks table")?;
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

    /// Puts a block into the database.
    pub fn pub_block(&self, block: &Block) -> Result<()> {
        self.put_value(BLOCKS, block)
    }

    /// Retrieves a block from the database, if it exists.
    pub fn get_block(&self, hash: &[u8; blake3::OUT_LEN]) -> Result<Option<Block>> {
        unsafe { self.get_blob_value::<Block, _>(BLOCKS, *hash) }
    }

    /// Puts a leader into the database.
    pub fn pub_leader(&self, leader: &Leader) -> Result<()> {
        self.put_value(LEADERS, leader)
    }

    /// Retrieves a leader from the database, if it exists.
    pub fn get_leader(&self, view: u64) -> Result<Option<Leader>> {
        unsafe { self.get_blob_value::<Leader, _>(LEADERS, view.to_le_bytes()) }
    }

    /// Puts a view into the database.
    pub fn pub_view(&self, view: &View) -> Result<()> {
        self.put_value(VIEWS, view)
    }

    /// Retrieves a view from the database, if it exists.
    pub fn get_view(&self, view: u64) -> Result<Option<View>> {
        unsafe { self.get_blob_value::<View, _>(VIEWS, view.to_le_bytes()) }
    }

    /// Puts a transaction into the database.
    pub fn pub_transaction(&self, transaction: &Transaction) -> Result<()> {
        self.put_value(MEMPOOL, transaction)
    }

    /// Retrieves a transaction from the database, if it exists.
    pub fn get_transaction(&self, hash: &[u8; blake3::OUT_LEN]) -> Result<Option<Transaction>> {
        unsafe { self.get_blob_value::<Transaction, _>(MEMPOOL, *hash) }
    }

    /// Puts a notarization into the database.
    pub fn pub_notarization<const N: usize, const F: usize, const M_SIZE: usize>(
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

    /// Puts a nullification into the database.
    pub fn pub_nullification<const N: usize, const F: usize, const L_SIZE: usize>(
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
    pub fn pub_account(&self, account: &Account) -> Result<()> {
        self.put_value(ACCOUNTS, account)
    }

    /// Retrieves an account from the database, if it exists.
    pub fn get_account(&self, public_key: &BlsPublicKey) -> Result<Option<Account>> {
        let mut writer = Vec::new();
        public_key.serialize_compressed(&mut writer).unwrap();
        unsafe { self.get_blob_value::<Account, _>(ACCOUNTS, writer) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::aggregated::{AggregatedSignature, BlsPublicKey, BlsSecretKey, PeerId},
        state::peer::PeerSet,
    };
    use rand::thread_rng;

    fn temp_db_path(suffix: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "consensus_store_test-{}-{}.redb",
            suffix,
            rand::random::<u64>()
        ));
        p.to_string_lossy().to_string()
    }

    fn gen_keypair() -> (BlsSecretKey, BlsPublicKey) {
        let mut rng = thread_rng();
        let sk = BlsSecretKey::generate(&mut rng);
        let pk = sk.public_key();
        (sk, pk)
    }

    fn serialize_pk(pk: &BlsPublicKey) -> Vec<u8> {
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
            store.pub_account(&acct).expect("pub account");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn block_roundtrip() {
        let path = temp_db_path("block");
        {
            let store = ConsensusStore::open(&path).unwrap();

            // Prepare a transaction to embed in block
            let (sk, pk) = gen_keypair();
            let body = b"tx-body-1";
            let tx_hash: [u8; blake3::OUT_LEN] = blake3::hash(body).into();
            let sig = sk.sign(&tx_hash);
            let tx = Transaction::new(pk.clone(), [7u8; 32], 42, 9, 1_000, 3, tx_hash, sig);

            let parent: [u8; blake3::OUT_LEN] = [1u8; blake3::OUT_LEN];
            let block = Block::new(5, 0, parent, vec![tx], 123456, false, 1);

            store.pub_block(&block).unwrap();
            let h = block.get_hash();
            let fetched = store.get_block(&h).unwrap().expect("get block");
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

            let (_sk, _pk) = gen_keypair();
            let leader = Leader::new(10, true, 10);

            store.pub_leader(&leader).unwrap();
            let fetched = store.get_leader(10).unwrap().expect("get leader");

            // Compare fields; `BlsPublicKey` lacks PartialEq, compare bytes instead.
            assert_eq!(fetched.view(), 10);
            assert!(fetched.is_current());
            assert_eq!(fetched.peer_id(), leader.peer_id());
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn view_roundtrip() {
        let path = temp_db_path("view");
        {
            let store = ConsensusStore::open(&path).unwrap();

            let (_sk, pk) = gen_keypair();
            let view = View::new(11, pk.clone(), true, false);

            store.pub_view(&view).unwrap();
            let fetched = store.get_view(11).unwrap().expect("get view");

            assert_eq!(fetched.view(), 11);
            assert!(fetched.is_current_view());
            assert!(!fetched.is_nullified());
            assert_eq!(serialize_pk(fetched.leader()), serialize_pk(view.leader()));
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn transaction_roundtrip() {
        let path = temp_db_path("tx");
        {
            let store = ConsensusStore::open(&path).unwrap();

            let (sk, pk) = gen_keypair();
            let body = b"tx-body-2";
            let tx_hash: [u8; blake3::OUT_LEN] = blake3::hash(body).into();
            let sig = sk.sign(&tx_hash);
            let tx = Transaction::new(pk.clone(), [9u8; 32], 100, 2, 2_000, 5, tx_hash, sig);

            store.pub_transaction(&tx).unwrap();
            let fetched = store.get_transaction(&tx_hash).unwrap().expect("get tx");

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
            let tx_hash0: [u8; blake3::OUT_LEN] = blake3::hash(b"mbody").into();
            let sig0 = sk0.sign(&tx_hash0);
            let tx0 = Transaction::new(pk0.clone(), [1u8; 32], 1, 0, 1, 0, tx_hash0, sig0);
            let block = Block::new(6, 0, parent, vec![tx0], 999, false, 1);

            // 3 signers aggregate over block hash
            let (sk1, pk1) = gen_keypair();
            let (sk2, pk2) = gen_keypair();
            let (sk3, pk3) = gen_keypair();

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
            );

            store.pub_notarization(&m).unwrap();
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

            store.pub_account(&acct).unwrap();
            let fetched = store.get_account(&pk).unwrap().expect("get account");

            assert_eq!(
                serialize_pk(&fetched.public_key),
                serialize_pk(&acct.public_key)
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
            let (sk1, pk1) = gen_keypair();
            let (sk2, pk2) = gen_keypair();
            let (sk3, pk3) = gen_keypair();

            // Aggregate signature over view bytes
            let view: u64 = 77;
            let msg = view.to_le_bytes();

            // Sign with the corresponding secret keys
            let s1 = sk1.sign(&msg);
            let s2 = sk2.sign(&msg);
            let s3 = sk3.sign(&msg);

            let pks_vec = vec![pk1.clone(), pk2.clone(), pk3.clone()];
            let pks: [BlsPublicKey; M_SIZE] = pks_vec.try_into().unwrap();

            let agg = AggregatedSignature::<M_SIZE>::new(pks.clone(), &msg, &[s1, s2, s3])
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
            store.pub_nullification(&nullif).unwrap();

            let fetched = store
                .get_nullification::<N, F, M_SIZE>(view)
                .unwrap()
                .expect("get nullification");
            assert_eq!(fetched.view, view);
            assert!(fetched.verify(&PeerSet::new(pks.to_vec())));
        }
        std::fs::remove_file(&path).ok();
    }
}
