use std::path::Path;

use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, TableDefinition};
use rkyv::de::Pool;
use rkyv::rancor::Strategy;
use rkyv::util::AlignedVec;
use rkyv::{Archive, Serialize, deserialize};

use crate::state::{
    block::Block, leader::Leader, notarizations::MNotarization, nullify::Nullification,
    transaction::Transaction, view::View,
};
use crate::storage::config::StorageConfig;
use crate::storage::conversions::Storable;

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
    unsafe fn get_blob<T, K>(
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
            let val = unsafe { access_archived::<T>(val) };
            Ok(Some(deserialize(val).map_err(|e| {
                anyhow::anyhow!("Failed to deserialize: {:?}", e)
            })?))
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
        unsafe { self.get_blob::<Block, _>(BLOCKS, *hash) }
    }

    /// Puts a leader into the database.
    pub fn pub_leader(&self, leader: &Leader) -> Result<()> {
        self.put_value(LEADERS, leader)
    }

    /// Retrieves a leader from the database, if it exists.
    pub fn get_leader(&self, view: u64) -> Result<Option<Leader>> {
        unsafe { self.get_blob::<Leader, _>(LEADERS, view.to_le_bytes()) }
    }

    /// Puts a view into the database.
    pub fn pub_view(&self, view: &View) -> Result<()> {
        self.put_value(VIEWS, view)
    }

    /// Retrieves a view from the database, if it exists.
    pub fn get_view(&self, view: u64) -> Result<Option<View>> {
        unsafe { self.get_blob::<View, _>(VIEWS, view.to_le_bytes()) }
    }

    /// Puts a transaction into the database.
    pub fn pub_transaction(&self, transaction: &Transaction) -> Result<()> {
        self.put_value(MEMPOOL, transaction)
    }

    /// Retrieves a transaction from the database, if it exists.
    pub fn get_transaction(&self, hash: &[u8; blake3::OUT_LEN]) -> Result<Option<Transaction>> {
        unsafe { self.get_blob::<Transaction, _>(MEMPOOL, *hash) }
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
        unsafe { self.get_blob::<MNotarization<N, F, M_SIZE>, _>(NOTARIZATIONS, *hash) }
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
        hash: &[u8; blake3::OUT_LEN],
    ) -> Result<Option<Nullification<N, F, L_SIZE>>> {
        unsafe { self.get_blob::<Nullification<N, F, L_SIZE>, _>(NULLIFICATIONS, *hash) }
    }
}
