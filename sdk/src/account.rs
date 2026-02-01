//! Account-related operations

use crate::client::GrpcChannel;
use crate::error::Result;
use crate::types::Address;
use grpc_client::proto::account_service_client::AccountServiceClient;
use grpc_client::proto::{GetAccountRequest, GetNonceRequest};

/// Account information.
#[derive(Debug, Clone)]
pub struct Account {
    /// Account address.
    pub address: Address,
    /// Account balance.
    pub balance: u64,
    /// Account nonce (number of transactions sent).
    pub nonce: u64,
}

/// Client for account operations.
pub struct AccountClient {
    client: AccountServiceClient<GrpcChannel>,
}

impl AccountClient {
    pub(crate) fn new(channel: GrpcChannel) -> Self {
        Self {
            client: AccountServiceClient::new(channel),
        }
    }

    /// Get account by address (finalized state).
    ///
    /// Returns `None` if the account doesn't exist.
    pub async fn get(&mut self, address: &Address) -> Result<Option<Account>> {
        let request = GetAccountRequest {
            address: address.to_hex(),
        };

        let response = self.client.get_account(request).await?.into_inner();

        if !response.exists {
            return Ok(None);
        }

        Ok(Some(Account {
            address: *address,
            balance: response.balance,
            nonce: response.nonce,
        }))
    }

    /// Get account with pending state overlay.
    ///
    /// Includes effects of M-notarized (but not yet finalized) transactions.
    pub async fn get_pending(&mut self, address: &Address) -> Result<Option<Account>> {
        let request = GetAccountRequest {
            address: address.to_hex(),
        };

        let response = self.client.get_account_pending(request).await?.into_inner();

        if !response.exists {
            return Ok(None);
        }

        Ok(Some(Account {
            address: *address,
            balance: response.balance,
            nonce: response.nonce,
        }))
    }

    /// Get the next valid nonce for an address.
    ///
    /// Considers both finalized state and pending mempool transactions.
    /// Use this when building a new transaction.
    pub async fn get_nonce(&mut self, address: &Address) -> Result<u64> {
        let request = GetNonceRequest {
            address: address.to_hex(),
        };

        let response = self.client.get_nonce(request).await?.into_inner();
        Ok(response.next_nonce)
    }

    /// Get balance for an address.
    ///
    /// Returns 0 if the account doesn't exist.
    pub async fn get_balance(&mut self, address: &Address) -> Result<u64> {
        match self.get(address).await? {
            Some(account) => Ok(account.balance),
            None => Ok(0),
        }
    }
}
