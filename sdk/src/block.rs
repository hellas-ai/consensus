//! Block-related operations

use crate::error::{Error, Result};
use crate::transaction::{TxInfo, TxType};
use crate::types::{Address, Hash};
use grpc_client::proto::block_service_client::BlockServiceClient;
use grpc_client::proto::{
    Empty, GetBlockByHeightRequest, GetBlockRequest, GetBlocksRequest, TransactionType,
};
use tonic::transport::Channel;

/// Block information.
#[derive(Debug, Clone)]
pub struct Block {
    /// Block hash.
    pub hash: Hash,
    /// Block height.
    pub height: u64,
    /// Consensus view number.
    pub view: u64,
    /// Parent block hash.
    pub parent_hash: Hash,
    /// Block timestamp.
    pub timestamp: u64,
    /// Leader peer ID.
    pub leader_id: u64,
    /// Transactions in the block.
    pub transactions: Vec<TxInfo>,
}

/// Client for block operations.
pub struct BlockClient {
    client: BlockServiceClient<Channel>,
}

impl BlockClient {
    pub(crate) fn new(channel: Channel) -> Self {
        Self {
            client: BlockServiceClient::new(channel),
        }
    }

    /// Get block by hash.
    pub async fn get(&mut self, hash: &Hash) -> Result<Option<Block>> {
        let request = GetBlockRequest {
            hash: hash.to_hex(),
        };

        match self.client.get_block(request).await {
            Ok(response) => Ok(Some(Self::response_to_block(response.into_inner())?)),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
            Err(e) => Err(Error::Grpc(e)),
        }
    }

    /// Get block by height.
    pub async fn get_by_height(&mut self, height: u64) -> Result<Option<Block>> {
        let request = GetBlockByHeightRequest { height };

        match self.client.get_block_by_height(request).await {
            Ok(response) => Ok(Some(Self::response_to_block(response.into_inner())?)),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
            Err(e) => Err(Error::Grpc(e)),
        }
    }

    /// Get the latest finalized block.
    pub async fn get_latest(&mut self) -> Result<Option<Block>> {
        match self.client.get_latest_block(Empty {}).await {
            Ok(response) => Ok(Some(Self::response_to_block(response.into_inner())?)),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
            Err(e) => Err(Error::Grpc(e)),
        }
    }

    /// Get a range of blocks.
    ///
    /// # Arguments
    /// * `from_height` - Starting height (inclusive)
    /// * `limit` - Maximum number of blocks to return
    pub async fn get_range(&mut self, from_height: u64, limit: u32) -> Result<Vec<Block>> {
        let request = GetBlocksRequest {
            from_height,
            limit,
            ..Default::default()
        };

        let response = self.client.get_blocks(request).await?.into_inner();

        response
            .blocks
            .into_iter()
            .map(Self::response_to_block)
            .collect()
    }

    fn response_to_block(resp: grpc_client::proto::BlockResponse) -> Result<Block> {
        let hash = parse_hash(&resp.hash)?;
        let parent_hash = parse_hash(&resp.parent_hash)?;

        let transactions = resp
            .transactions
            .into_iter()
            .map(|tx_info| {
                let tx_hash = parse_hash(&tx_info.tx_hash).unwrap_or_default();
                let sender =
                    Address::from_hex(&tx_info.sender).unwrap_or_else(|_| Address::default());

                let tx_type = match TransactionType::try_from(tx_info.r#type).unwrap_or_default() {
                    TransactionType::Transfer => TxType::Transfer {
                        to: Address::from_hex(&tx_info.recipient)
                            .unwrap_or_else(|_| Address::default()),
                        amount: tx_info.amount,
                    },
                    TransactionType::Mint => TxType::Mint {
                        to: Address::from_hex(&tx_info.recipient)
                            .unwrap_or_else(|_| Address::default()),
                        amount: tx_info.amount,
                    },
                    TransactionType::Burn => TxType::Burn {
                        address: Address::from_hex(&tx_info.recipient)
                            .unwrap_or_else(|_| Address::default()),
                        amount: tx_info.amount,
                    },
                    TransactionType::CreateAccount => TxType::CreateAccount {
                        address: Address::from_hex(&tx_info.recipient)
                            .unwrap_or_else(|_| Address::default()),
                    },
                    _ => TxType::Unknown,
                };

                TxInfo {
                    tx_hash,
                    sender,
                    tx_type,
                    nonce: tx_info.nonce,
                    fee: tx_info.fee,
                    timestamp: tx_info.timestamp,
                }
            })
            .collect();

        Ok(Block {
            hash,
            height: resp.height,
            view: resp.view,
            parent_hash,
            timestamp: resp.timestamp,
            leader_id: resp.leader_id,
            transactions,
        })
    }
}

fn parse_hash(hex: &str) -> Result<Hash> {
    Hash::from_hex(hex)
}
