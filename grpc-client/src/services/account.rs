//! Account service implementation.

use tonic::{Request, Response, Status};

use crate::proto::account_service_server::AccountService;
use crate::proto::{GetAccountRequest, GetAccountResponse, GetNonceRequest, GetNonceResponse};
use crate::server::ReadOnlyContext;

use super::utils::parse_address;

/// Implementation of the AccountService gRPC service.
pub struct AccountServiceImpl {
    context: ReadOnlyContext,
}

impl AccountServiceImpl {
    /// Create a new AccountService implementation.
    pub fn new(context: ReadOnlyContext) -> Self {
        Self { context }
    }
}

#[tonic::async_trait]
impl AccountService for AccountServiceImpl {
    /// Get account balance and nonce from finalized state.
    async fn get_account(
        &self,
        request: Request<GetAccountRequest>,
    ) -> Result<Response<GetAccountResponse>, Status> {
        let req = request.into_inner();
        let address = parse_address(&req.address)?;

        // Convert address to public key for finalized storage lookup
        let public_key = address
            .to_public_key()
            .ok_or_else(|| Status::invalid_argument("Address is not a valid Ed25519 public key"))?;

        // Query from finalized storage
        let account = self.context.store.get_account(&public_key).ok().flatten();

        match account {
            Some(acc) => Ok(Response::new(GetAccountResponse {
                exists: true,
                balance: acc.balance,
                nonce: acc.nonce,
                public_key: hex::encode(acc.public_key.bytes),
            })),
            None => Ok(Response::new(GetAccountResponse {
                exists: false,
                balance: 0,
                nonce: 0,
                public_key: String::new(),
            })),
        }
    }

    /// Get account with pending state overlay.
    /// Includes effects of M-notarized (but not yet finalized) transactions.
    async fn get_account_pending(
        &self,
        request: Request<GetAccountRequest>,
    ) -> Result<Response<GetAccountResponse>, Status> {
        let req = request.into_inner();
        let address = parse_address(&req.address)?;

        // Query from pending state (includes M-notarized transactions)
        let account_state = self.context.pending_state.get_account(&address);

        match account_state {
            Some(state) => Ok(Response::new(GetAccountResponse {
                exists: state.exists,
                balance: state.balance,
                nonce: state.nonce,
                // Public key not available from pending state
                public_key: req.address.clone(),
            })),
            None => Ok(Response::new(GetAccountResponse {
                exists: false,
                balance: 0,
                nonce: 0,
                public_key: String::new(),
            })),
        }
    }

    /// Get the next valid nonce for an account.
    /// Considers both finalized state and pending mempool transactions.
    async fn get_nonce(
        &self,
        request: Request<GetNonceRequest>,
    ) -> Result<Response<GetNonceResponse>, Status> {
        let req = request.into_inner();
        let address = parse_address(&req.address)?;

        // Get nonce from pending state (most up-to-date view)
        let account_state = self.context.pending_state.get_account(&address);

        let next_nonce = account_state.map(|state| state.nonce + 1).unwrap_or(0);

        Ok(Response::new(GetNonceResponse {
            next_nonce,
            // TODO: Query mempool for pending transaction count
            pending_tx_count: 0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use consensus::crypto::transaction_crypto::TxSecretKey;
    use consensus::state::account::Account;
    use consensus::state::address::Address;
    use consensus::storage::store::ConsensusStore;
    use consensus::validation::pending_state::PendingStateWriter;
    use consensus::validation::types::StateDiff;
    use slog::Logger;

    fn temp_db_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "account_service_test_{}.redb",
            rand::random::<u64>()
        ));
        p
    }

    fn create_test_logger() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    fn create_test_context() -> (ReadOnlyContext, Arc<ConsensusStore>, PendingStateWriter) {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let context = ReadOnlyContext {
            store: Arc::clone(&store),
            pending_state: reader,
            mempool_stats: None,
            peer_stats: None,
            block_events: None,
            consensus_events: None,
            tx_events: None,
            logger: create_test_logger(),
        };

        (context, store, writer)
    }

    #[tokio::test]
    async fn get_account_nonexistent_returns_empty() {
        let (context, _store, _writer) = create_test_context();
        let service = AccountServiceImpl::new(context);

        // Use a real Ed25519 public key that doesn't exist in the store
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let address_hex = hex::encode(pk.to_bytes());

        let request = Request::new(GetAccountRequest {
            address: address_hex,
        });

        let response = service.get_account(request).await.unwrap();
        let resp = response.into_inner();

        assert!(!resp.exists);
        assert_eq!(resp.balance, 0);
        assert_eq!(resp.nonce, 0);
        assert!(resp.public_key.is_empty());
    }

    #[tokio::test]
    async fn get_account_existing_returns_data() {
        let (context, store, _writer) = create_test_context();

        // Create an account in the store
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let account = Account::new(pk.clone(), 5000, 10);
        store.put_account(&account).unwrap();

        let service = AccountServiceImpl::new(context);
        let address_hex = hex::encode(pk.to_bytes());

        let request = Request::new(GetAccountRequest {
            address: address_hex,
        });

        let response = service.get_account(request).await.unwrap();
        let resp = response.into_inner();

        assert!(resp.exists);
        assert_eq!(resp.balance, 5000);
        assert_eq!(resp.nonce, 10);
        assert!(!resp.public_key.is_empty());
    }

    #[tokio::test]
    async fn get_account_invalid_address_returns_error() {
        let (context, _store, _writer) = create_test_context();
        let service = AccountServiceImpl::new(context);

        let request = Request::new(GetAccountRequest {
            address: "invalid_hex".to_string(),
        });

        let result = service.get_account(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn get_account_pending_sees_m_notarized_state() {
        let (context, _store, mut writer) = create_test_context();

        // Create account via pending diff (M-notarized but not finalized)
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        let mut diff = StateDiff::new();
        diff.add_created_account(addr, 3000);
        writer.add_m_notarized_diff(1, Arc::new(diff));

        let service = AccountServiceImpl::new(context);
        let address_hex = hex::encode(addr.as_bytes());

        let request = Request::new(GetAccountRequest {
            address: address_hex,
        });

        let response = service.get_account_pending(request).await.unwrap();
        let resp = response.into_inner();

        assert!(resp.exists);
        assert_eq!(resp.balance, 3000);
        assert_eq!(resp.nonce, 0);
    }

    #[tokio::test]
    async fn get_account_pending_overlays_on_finalized() {
        let (context, store, mut writer) = create_test_context();

        // Create account in finalized storage
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);
        let account = Account::new(pk, 1000, 5);
        store.put_account(&account).unwrap();

        // Add pending update
        let mut diff = StateDiff::new();
        diff.add_balance_change(addr, -200, 6);
        writer.add_m_notarized_diff(1, Arc::new(diff));

        let service = AccountServiceImpl::new(context);
        let address_hex = hex::encode(addr.as_bytes());

        let request = Request::new(GetAccountRequest {
            address: address_hex,
        });

        let response = service.get_account_pending(request).await.unwrap();
        let resp = response.into_inner();

        assert!(resp.exists);
        assert_eq!(resp.balance, 800); // 1000 - 200
        assert_eq!(resp.nonce, 6);
    }

    #[tokio::test]
    async fn get_nonce_nonexistent_returns_zero() {
        let (context, _store, _writer) = create_test_context();
        let service = AccountServiceImpl::new(context);

        let request = Request::new(GetNonceRequest {
            address: "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
        });

        let response = service.get_nonce(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.next_nonce, 0);
        assert_eq!(resp.pending_tx_count, 0);
    }

    #[tokio::test]
    async fn get_nonce_existing_returns_next() {
        let (context, store, _writer) = create_test_context();

        // Create account with nonce 5
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let account = Account::new(pk.clone(), 1000, 5);
        store.put_account(&account).unwrap();

        let service = AccountServiceImpl::new(context);
        let address_hex = hex::encode(pk.to_bytes());

        let request = Request::new(GetNonceRequest {
            address: address_hex,
        });

        let response = service.get_nonce(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.next_nonce, 6); // nonce + 1
    }

    #[tokio::test]
    async fn get_nonce_with_pending_updates() {
        let (context, store, mut writer) = create_test_context();

        // Create account with nonce 5
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);
        let account = Account::new(pk.clone(), 1000, 5);
        store.put_account(&account).unwrap();

        // Add pending tx that increments nonce to 6
        let mut diff = StateDiff::new();
        diff.add_balance_change(addr, -100, 6);
        writer.add_m_notarized_diff(1, Arc::new(diff));

        let service = AccountServiceImpl::new(context);
        let address_hex = hex::encode(pk.to_bytes());

        let request = Request::new(GetNonceRequest {
            address: address_hex,
        });

        let response = service.get_nonce(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.next_nonce, 7); // pending nonce 6 + 1
    }
}
