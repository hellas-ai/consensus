use std::{fmt::Debug, marker::PhantomData};

use redb::{TableDefinition, TypeName};
use rkyv::{
    Archive, Archived, Deserialize, Serialize, api::high::to_bytes_with_alloc, rancor::Error,
    ser::allocator::Arena,
};

use crate::{
    BlsSignature,
    crypto::{
        aggregated::{AggregatedSignature, BlsPublicKey},
        conversions::ArkSerdeWrapper,
    },
};
use redb::Value;

pub const ACCOUNTS: TableDefinition<&str, &[u8]> = TableDefinition::new("accounts");
pub const BLOCKS: TableDefinition<&str, &[u8]> = TableDefinition::new("blocks");
pub const VOTES: TableDefinition<&str, &[u8]> = TableDefinition::new("votes");
pub const NOTARIZATIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("notarizations");
pub const NULLIFICATIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("nullifications");
pub const VIEWS: TableDefinition<&str, &[u8]> = TableDefinition::new("views");
pub const LEADERS: TableDefinition<&str, &[u8]> = TableDefinition::new("leaders");
pub const STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("state");
pub const MEMPOOL: TableDefinition<&str, &[u8]> = TableDefinition::new("mempool");

#[derive(Archive, Deserialize, Serialize)]
pub struct LNotarization<const L_SIZE: usize> {
    pub view: u64,
    pub block_hash: [u8; blake3::OUT_LEN],
    pub signature: AggregatedSignature<L_SIZE>,
}

#[derive(Archive, Deserialize, Serialize)]
pub struct MNotarization<const M_SIZE: usize> {
    pub view: u64,
    pub block_hash: [u8; blake3::OUT_LEN],
    pub signature: AggregatedSignature<M_SIZE>,
}

#[derive(Archive, Deserialize, Serialize)]
pub struct Mempool {
    pub tx_hash: [u8; blake3::OUT_LEN],
    pub transaction: Transaction,
    pub timestamp: u64,
    pub fee: u64,
    pub nonce: u64,
    pub view: u64,
}

#[derive(Archive, Deserialize, Serialize)]
pub struct State {
    pub data: Vec<u8>,
}
