//! Hellas network client

use crate::account::AccountClient;
use crate::block::BlockClient;
use crate::error::{Error, Result};
use crate::subscription::SubscriptionClient;
use crate::transaction::{SignedTransaction, TxReceipt, TxStatus};
use crate::types::Hash;
use grpc_client::proto::transaction_service_client::TransactionServiceClient;
use grpc_client::proto::{GetTransactionRequest, SubmitTransactionRequest};
use std::time::Duration;
use tonic::transport::Channel;

/// Configuration for connecting to a Hellas node.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// Node endpoint (e.g., "http://localhost:50051").
    pub endpoint: String,
    /// Request timeout.
    pub timeout: Duration,
    /// Maximum retry attempts.
    pub max_retries: u32,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:50051".into(),
            timeout: Duration::from_secs(30),
            max_retries: 3,
        }
    }
}

impl ClientConfig {
    /// Create config with endpoint.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            ..Default::default()
        }
    }

    /// Set timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Client for interacting with a Hellas node.
///
/// # Example
/// ```ignore
/// use hellas_sdk::HellasClient;
///
/// let client = HellasClient::connect("http://localhost:50051").await?;
/// let balance = client.account().get_balance(&address).await?;
/// ```
pub struct HellasClient {
    channel: Channel,
    #[allow(dead_code)]
    config: ClientConfig,
}

impl HellasClient {
    /// Connect to a node with default config.
    ///
    /// # Arguments
    /// * `endpoint` - Node gRPC endpoint (e.g., "http://localhost:50051")
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let config = ClientConfig::new(endpoint);
        Self::connect_with_config(config).await
    }

    /// Connect with custom configuration.
    pub async fn connect_with_config(config: ClientConfig) -> Result<Self> {
        let channel = Channel::from_shared(config.endpoint.clone())?
            .timeout(config.timeout)
            .connect()
            .await
            .map_err(Error::ConnectionFailed)?;

        Ok(Self { channel, config })
    }

    /// Get account operations client.
    pub fn account(&self) -> AccountClient {
        AccountClient::new(self.channel.clone())
    }

    /// Get block operations client.
    pub fn blocks(&self) -> BlockClient {
        BlockClient::new(self.channel.clone())
    }

    /// Get subscription client for event streams.
    pub fn subscribe(&self) -> SubscriptionClient {
        SubscriptionClient::new(self.channel.clone())
    }

    /// Submit a signed transaction.
    ///
    /// Returns the transaction hash if accepted by the node.
    pub async fn submit_transaction(&self, tx: SignedTransaction) -> Result<Hash> {
        let mut client = TransactionServiceClient::new(self.channel.clone());

        let request = SubmitTransactionRequest {
            transaction_bytes: tx.bytes,
        };

        let response = client.submit_transaction(request).await?.into_inner();

        if !response.success {
            return Err(Error::TxRejected(response.error_message));
        }

        Ok(tx.tx_hash)
    }

    /// Submit a transaction and wait for it to be finalized.
    ///
    /// Polls the transaction status until it's finalized or times out.
    /// Note: Transaction may briefly show as NotFound between mempool removal
    /// and block persistence - this is handled by retrying.
    pub async fn submit_and_wait(
        &self,
        tx: SignedTransaction,
        timeout: Duration,
    ) -> Result<TxReceipt> {
        let tx_hash = self.submit_transaction(tx).await?;

        let start = std::time::Instant::now();
        let poll_interval = Duration::from_millis(500);
        let mut retries_remaining = self.config.max_retries;

        loop {
            if start.elapsed() > timeout {
                return Err(Error::Timeout);
            }

            if retries_remaining == 0 {
                return Err(Error::MaxRetriesExceeded {
                    num_retries: self.config.max_retries,
                });
            }

            match self.get_transaction_status(&tx_hash).await? {
                TxStatus::Finalized {
                    block_hash,
                    block_height,
                } => {
                    return Ok(TxReceipt {
                        tx_hash,
                        block_hash,
                        block_height,
                        tx_index: 0, // TODO: Get actual index from response
                    });
                }
                // NotFound is treated as transient: there's a brief window between
                // when the mempool removes a tx (after finalization) and when the
                // block is persisted to storage. We retry instead of failing.
                TxStatus::NotFound | TxStatus::Pending | TxStatus::MNotarized { .. } => {
                    retries_remaining -= 1;
                    tokio::time::sleep(poll_interval).await;
                }
            }
        }
    }

    /// Get transaction status.
    pub async fn get_transaction_status(&self, tx_hash: &Hash) -> Result<TxStatus> {
        let mut client = TransactionServiceClient::new(self.channel.clone());

        let request = GetTransactionRequest {
            tx_hash: tx_hash.to_hex(),
        };

        let response = client.get_transaction_status(request).await?.into_inner();

        use grpc_client::proto::TransactionStatus;

        match TransactionStatus::try_from(response.status).unwrap_or(TransactionStatus::Unspecified)
        {
            TransactionStatus::PendingMempool => Ok(TxStatus::Pending),
            TransactionStatus::PendingBlock => {
                let block_hash = Hash::from_hex(&response.block_hash).unwrap_or_default();
                Ok(TxStatus::MNotarized {
                    block_hash,
                    block_height: response.block_height,
                })
            }
            TransactionStatus::Finalized => {
                let block_hash = Hash::from_hex(&response.block_hash).unwrap_or_default();
                Ok(TxStatus::Finalized {
                    block_hash,
                    block_height: response.block_height,
                })
            }
            TransactionStatus::NotFound | TransactionStatus::Unspecified => Ok(TxStatus::NotFound),
        }
    }
}
