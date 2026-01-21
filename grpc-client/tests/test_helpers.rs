//! E2E test helpers for gRPC services.
//!
//! Provides infrastructure for spinning up a real gRPC server
//! and making client requests against it.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use consensus::crypto::aggregated::BlsSecretKey;
use consensus::crypto::transaction_crypto::TxSecretKey;
use consensus::state::address::Address;
use consensus::state::block::Block;
use consensus::state::transaction::Transaction;
use consensus::storage::store::ConsensusStore;
use consensus::validation::pending_state::PendingStateWriter;
use p2p::service::P2PHandle;
use rtrb::RingBuffer;
use slog::Logger;
use tokio::sync::Notify;
use tonic::transport::Channel;

use grpc_client::config::{Network, RpcConfig};
use grpc_client::proto::account_service_client::AccountServiceClient;
use grpc_client::proto::block_service_client::BlockServiceClient;
use grpc_client::proto::node_service_client::NodeServiceClient;
use grpc_client::proto::transaction_service_client::TransactionServiceClient;
use grpc_client::server::{RpcContext, RpcServer};

/// Test server handle that manages a running gRPC server.
pub struct TestServerHandle {
    /// Temporary directory for the database (must be kept alive)
    pub temp_dir: PathBuf,
    /// Storage reference for test setup
    pub store: Arc<ConsensusStore>,
    /// PendingState writer for test setup
    pub _pending_state_writer: PendingStateWriter,
    /// Server address for client connections
    pub addr: SocketAddr,
    /// Client channel (reusable)
    channel: Option<Channel>,
    /// Shutdown flag
    shutdown: Arc<AtomicBool>,
}

impl TestServerHandle {
    /// Get a transaction service client.
    #[allow(dead_code)]
    pub async fn transaction_client(
        &mut self,
    ) -> Result<TransactionServiceClient<Channel>, tonic::transport::Error> {
        let channel = self.get_or_create_channel().await?;
        Ok(TransactionServiceClient::new(channel))
    }

    /// Get a block service client.
    #[allow(dead_code)]
    pub async fn block_client(
        &mut self,
    ) -> Result<BlockServiceClient<Channel>, tonic::transport::Error> {
        let channel = self.get_or_create_channel().await?;
        Ok(BlockServiceClient::new(channel))
    }

    /// Get a node service client.
    #[allow(dead_code)]
    pub async fn node_client(
        &mut self,
    ) -> Result<NodeServiceClient<Channel>, tonic::transport::Error> {
        let channel = self.get_or_create_channel().await?;
        Ok(NodeServiceClient::new(channel))
    }

    /// Get an account service client.
    #[allow(dead_code)]
    pub async fn account_client(
        &mut self,
    ) -> Result<AccountServiceClient<Channel>, tonic::transport::Error> {
        let channel = self.get_or_create_channel().await?;
        Ok(AccountServiceClient::new(channel))
    }

    async fn get_or_create_channel(&mut self) -> Result<Channel, tonic::transport::Error> {
        if let Some(ref channel) = self.channel {
            return Ok(channel.clone());
        }

        let endpoint = format!("http://{}", self.addr);
        let channel = Channel::from_shared(endpoint)
            .expect("valid endpoint")
            .connect()
            .await?;

        self.channel = Some(channel.clone());
        Ok(channel)
    }
}

impl Drop for TestServerHandle {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        // Clean up temp directory
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

/// Create a mock P2P handle for tests.
pub fn create_mock_p2p_handle() -> P2PHandle {
    let (tx_broadcast_producer, _tx_broadcast_consumer) = RingBuffer::<Transaction>::new(1000);

    P2PHandle {
        thread_handle: std::thread::spawn(|| {}),
        shutdown: Arc::new(AtomicBool::new(false)),
        shutdown_notify: Arc::new(Notify::new()),
        broadcast_notify: Arc::new(Notify::new()),
        tx_broadcast_producer,
        tx_broadcast_notify: Arc::new(Notify::new()),
        ready_notify: Arc::new(Notify::new()),
        is_ready: Arc::new(AtomicBool::new(true)),
    }
}

/// Create a test logger that discards output.
pub fn create_test_logger() -> Logger {
    Logger::root(slog::Discard, slog::o!())
}

/// Generate a unique temporary database path.
pub fn temp_db_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("grpc_e2e_test_{}", rand::random::<u64>()));
    std::fs::create_dir_all(&p).expect("create temp dir");
    p.push("consensus.redb");
    p
}

/// Spawn a test gRPC server on a random available port.
///
/// Returns a handle with the server address and storage for test setup.
pub async fn spawn_test_server() -> TestServerHandle {
    // Create temporary storage
    let db_path = temp_db_path();
    let temp_dir = db_path.parent().unwrap().to_path_buf();
    let store = Arc::new(ConsensusStore::open(&db_path).expect("open store"));

    // Create pending state
    let (pending_state_writer, pending_state_reader) =
        PendingStateWriter::new(Arc::clone(&store), 0);

    // Create P2P handle
    let p2p_handle = create_mock_p2p_handle();
    let p2p_ready = Arc::new(AtomicBool::new(true));

    // Find an available port
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind to random port");
    let addr = listener.local_addr().expect("get local addr");
    drop(listener); // Release the port for the server

    let logger = create_test_logger();

    // Create RPC context
    let context = RpcContext::new(
        Arc::clone(&store),
        pending_state_reader,
        None, // mempool_stats
        None, // peer_stats
        None, // block_events
        None, // consensus_events
        None, // tx_events
        p2p_handle,
        Arc::clone(&p2p_ready),
        logger,
    );

    let config = RpcConfig {
        listen_addr: addr,
        max_concurrent_streams: 100,
        request_timeout_secs: 30,
        peer_id: 0,
        network: Network::Local,
        total_validators: 4,
        f: 1,
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    // Spawn server in background
    tokio::spawn(async move {
        let server = RpcServer::new(config, context);
        let _ = server.serve().await;
    });

    // Give the server a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    TestServerHandle {
        temp_dir,
        store,
        _pending_state_writer: pending_state_writer,
        addr,
        channel: None,
        shutdown: shutdown_clone,
    }
}

/// Create a signed test transaction.
#[allow(dead_code)]
pub fn create_test_transaction(
    sender_sk: &TxSecretKey,
    recipient: Address,
    amount: u64,
    nonce: u64,
    fee: u64,
) -> Transaction {
    let sender = Address::from_public_key(&sender_sk.public_key());
    Transaction::new_transfer(sender, recipient, amount, nonce, fee, sender_sk)
}

/// Create a test block with the given transactions.
pub fn create_test_block(
    height: u64,
    view: u64,
    parent_hash: [u8; 32],
    txs: Vec<Arc<Transaction>>,
) -> Block {
    let sk = BlsSecretKey::generate(&mut rand::thread_rng());
    let leader_signature = sk.sign(b"block proposal");

    Block::new(
        view,
        0, // leader
        parent_hash,
        txs,
        1234567890 + height,
        leader_signature,
        true, // is_finalized
        height,
    )
}

/// Generate a new test keypair.
#[allow(dead_code)]
pub fn generate_test_keypair() -> TxSecretKey {
    TxSecretKey::generate(&mut rand::rngs::OsRng)
}

/// Serialize a transaction to bytes using rkyv.
///
/// This matches what gRPC clients would send to the server.
#[allow(dead_code)]
pub fn serialize_transaction(tx: &Transaction) -> Vec<u8> {
    consensus::storage::conversions::serialize_for_db(tx)
        .expect("serialize transaction")
        .to_vec()
}
