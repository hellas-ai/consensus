//! Test helpers for mempool transaction gossiping tests.
//!
//! Provides infrastructure for setting up multi-node networks where transactions
//! are broadcast via P2P gossip and verified to propagate to all nodes' mempools.

#![allow(dead_code)]

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use ark_serialize::CanonicalSerialize;
use commonware_runtime::tokio::Runner as TokioRunner;
use consensus::{
    consensus_manager::config::{ConsensusConfig, GenesisAccount, Network as NetworkType},
    consensus_manager::consensus_engine::ConsensusEngine,
    consensus_manager::leader_manager::LeaderSelectionStrategy,
    crypto::{aggregated::BlsSecretKey, transaction_crypto::TxSecretKey},
    mempool::MempoolService,
    state::{address::Address, block::Block, peer::PeerSet, transaction::Transaction},
    storage::store::ConsensusStore,
    validation::PendingStateWriter,
};
use crossbeam::queue::ArrayQueue;
use p2p::{
    config::P2PConfig, config::ValidatorPeerInfo, identity::ValidatorIdentity,
    service::spawn as spawn_p2p,
};
use rtrb::{Consumer, Producer, RingBuffer};
use slog::{Drain, Level, Logger, o};
use tempfile::TempDir;
use tonic::transport::Channel;

use grpc_client::config::{Network as RpcNetwork, RpcConfig};
use grpc_client::proto::transaction_service_client::TransactionServiceClient;
use grpc_client::proto::{SubmitTransactionRequest, SubmitTransactionResponse};
use grpc_client::server::{RpcContext, RpcServer};

/// Test configuration constants (n = 5f + 1 = 6 when f = 1)
pub const N: usize = 6;
pub const F: usize = 1;
pub const M_SIZE: usize = 3;
const BUFFER_SIZE: usize = 10_000;
const DEFAULT_TICK_INTERVAL: Duration = Duration::from_millis(10);
const DEFAULT_VIEW_TIMEOUT: Duration = Duration::from_secs(5);

/// Creates a logger for integration tests with configurable log levels.
///
/// Respects the `RUST_LOG` environment variable:
/// - `error` - Only errors
/// - `warn` - Warnings and errors
/// - `info` - Info, warnings, and errors (default)
/// - `debug` - All messages including debug
pub fn create_test_logger() -> Logger {
    let log_level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|env_str| Level::from_str(&env_str).ok())
        .unwrap_or(Level::Info);

    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain)
        .build()
        .fuse()
        .filter_level(log_level)
        .fuse();

    slog::Logger::root(drain, o!())
}

/// A test node setup for gossip testing.
///
/// Unlike the consensus e2e tests which push directly to mempool, this setup
/// provides access to the P2P broadcast capabilities for testing true gossip
/// propagation.
pub struct GossipTestNode<const N: usize, const F: usize, const M_SIZE: usize> {
    /// P2P service handle
    pub p2p_handle: p2p::service::P2PHandle,

    /// Consensus engine (optional - some tests may not need it)
    pub consensus_engine: Option<ConsensusEngine<N, F, M_SIZE>>,

    /// Transaction consumer - receives transactions from P2P gossip
    /// This is where transactions arrive after being received via P2P
    pub tx_consumer: Consumer<Transaction>,

    /// Mempool service
    pub mempool_service: MempoolService,

    /// Peer ID (BLS-based)
    pub peer_id: consensus::crypto::aggregated::PeerId,

    /// Node index (0-based)
    pub node_idx: usize,

    /// gRPC server address for this node
    pub grpc_addr: std::net::SocketAddr,

    /// P2P ready flag (shared with RpcServer for health checks)
    pub p2p_ready: Arc<std::sync::atomic::AtomicBool>,

    /// Logger for this node
    pub logger: Logger,
}

/// Result of setting up a gossip test network.
pub struct GossipTestNetwork<const N: usize, const F: usize, const M_SIZE: usize> {
    /// All nodes in the network
    pub nodes: Vec<GossipTestNode<N, F, M_SIZE>>,

    /// Storage references for each node (for verification)
    pub stores: Vec<Arc<ConsensusStore>>,

    /// Temp directories (must be kept alive until test completes)
    pub temp_dirs: Vec<TempDir>,

    /// Genesis accounts (for creating valid transactions)
    pub genesis_accounts: Vec<GenesisAccount>,

    /// Pre-funded secret keys for creating test transactions
    pub funded_keys: Vec<TxSecretKey>,

    /// Logger
    pub logger: Logger,
}

/// Creates funded test transactions and genesis accounts.
///
/// Returns (transactions, genesis_accounts, secret_keys) where secret_keys
/// can be reused to create additional transactions.
pub fn create_funded_accounts(
    num_accounts: usize,
    balance: u64,
) -> (Vec<GenesisAccount>, Vec<TxSecretKey>) {
    let mut genesis_accounts = Vec::new();
    let mut secret_keys = Vec::new();

    for _ in 0..num_accounts {
        let sk = TxSecretKey::generate(&mut rand::thread_rng());
        let pk = sk.public_key();

        genesis_accounts.push(GenesisAccount {
            public_key: hex::encode(pk.to_bytes()),
            balance,
        });

        secret_keys.push(sk);
    }

    (genesis_accounts, secret_keys)
}

/// Creates a signed transaction from a funded account.
pub fn create_test_transaction(
    sender_sk: &TxSecretKey,
    recipient: Address,
    amount: u64,
    nonce: u64,
    fee: u64,
) -> Transaction {
    let sender_addr = Address::from_public_key(&sender_sk.public_key());
    Transaction::new_transfer(sender_addr, recipient, amount, nonce, fee, sender_sk)
}

/// Creates a gossip test network with the specified number of nodes.
///
/// This sets up N nodes with P2P networking where transactions can be
/// gossiped between nodes and verified to arrive in each node's mempool.
///
/// # Arguments
/// * `num_nodes` - Number of nodes to create (typically 6 for n=5f+1, f=1)
/// * `with_consensus` - Whether to start consensus engines (false for pure gossip tests)
/// * `logger` - Logger for the test
pub fn create_gossip_test_network(
    num_nodes: usize,
    with_consensus: bool,
    logger: Logger,
) -> GossipTestNetwork<N, F, M_SIZE> {
    // Create funded accounts for testing (enough for many transactions)
    let num_funded_accounts = 100;
    let (genesis_accounts, funded_keys) = create_funded_accounts(num_funded_accounts, 1_000_000);

    // Generate BLS keypairs and identities for all nodes
    let mut identities = Vec::new();
    let mut public_keys = Vec::new();

    for _i in 0..num_nodes {
        let bls_sk = BlsSecretKey::generate(&mut rand::thread_rng());
        let identity = ValidatorIdentity::from_bls_key(bls_sk);
        public_keys.push(identity.bls_public_key().clone());
        identities.push(identity);
    }

    // Create peer set
    let peer_set = PeerSet::new(public_keys);

    // Create consensus config with hex-encoded public keys
    let mut peer_strs = Vec::with_capacity(peer_set.sorted_peer_ids.len());
    for peer_id in &peer_set.sorted_peer_ids {
        let pk = peer_set.id_to_public_key.get(peer_id).unwrap();
        let mut buf = Vec::new();
        pk.0.serialize_compressed(&mut buf).unwrap();
        peer_strs.push(hex::encode(buf));
    }

    let consensus_config = ConsensusConfig {
        n: num_nodes,
        f: F,
        view_timeout: DEFAULT_VIEW_TIMEOUT,
        leader_manager: LeaderSelectionStrategy::RoundRobin,
        network: NetworkType::Local,
        peers: peer_strs,
        genesis_accounts: genesis_accounts.clone(),
    };

    // Use random base port to avoid conflicts between test runs
    let base_port = 50000u16 + (rand::random::<u16>() % 10000);
    let port_gap = 100u16;

    // Create P2P configs for each node
    let mut p2p_configs = Vec::new();
    for (i, _identity) in identities.iter().enumerate() {
        let port = base_port + (i as u16 * port_gap);
        let listen_addr = format!("127.0.0.1:{}", port).parse().unwrap();
        let external_addr = listen_addr;

        // Build validator list (all other nodes)
        let mut validators = Vec::new();
        for (j, other_identity) in identities.iter().enumerate() {
            if i != j {
                let other_port = base_port + (j as u16 * port_gap);
                let ed25519_pk = other_identity.ed25519_public_key();
                let pk_hex = hex::encode(ed25519_pk.as_ref());
                validators.push(ValidatorPeerInfo {
                    bls_peer_id: other_identity.peer_id(),
                    bls_public_key: None,
                    ed25519_public_key: pk_hex,
                    address: Some(format!("127.0.0.1:{}", other_port).parse().unwrap()),
                });
            }
        }

        let p2p_config = P2PConfig {
            listen_addr,
            external_addr,
            validators,
            total_number_peers: num_nodes,
            maximum_number_faulty_peers: F,
            bootstrap_timeout_ms: 20_000,
            ping_interval_ms: 200,
            ..Default::default()
        };

        p2p_configs.push(p2p_config);
    }

    // Spawn all nodes
    let mut nodes = Vec::new();
    let mut stores = Vec::new();
    let mut temp_dirs = Vec::new();

    for (i, (identity, p2p_config)) in identities
        .into_iter()
        .zip(p2p_configs.into_iter())
        .enumerate()
    {
        let node_logger = logger.new(o!("node" => i, "peer_id" => identity.peer_id()));

        let (node, storage, temp_dir) = create_gossip_node(
            i,
            identity,
            p2p_config,
            consensus_config.clone(),
            with_consensus,
            node_logger,
        );

        nodes.push(node);
        stores.push(storage);
        temp_dirs.push(temp_dir);
    }

    GossipTestNetwork {
        nodes,
        stores,
        temp_dirs,
        genesis_accounts,
        funded_keys,
        logger,
    }
}

/// Creates a single gossip test node.
fn create_gossip_node(
    node_idx: usize,
    identity: ValidatorIdentity,
    p2p_config: P2PConfig,
    consensus_config: ConsensusConfig,
    with_consensus: bool,
    logger: Logger,
) -> (GossipTestNode<N, F, M_SIZE>, Arc<ConsensusStore>, TempDir) {
    let _peer_id = identity.peer_id();

    // Create ring buffers for consensus messages
    let (consensus_msg_producer, consensus_msg_consumer) = RingBuffer::new(BUFFER_SIZE);
    let (broadcast_producer, broadcast_consumer) = RingBuffer::new(BUFFER_SIZE);

    // Create temporary directory and storage
    let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
    let db_path = temp_dir.path().join("consensus.redb");
    let storage = Arc::new(ConsensusStore::open(&db_path).expect("Failed to open storage"));

    // Create PendingStateWriter
    let (persistence_writer, pending_state_reader) =
        PendingStateWriter::new(Arc::clone(&storage), 0);

    // Create shutdown flag
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Create transaction routing following the two-source architecture:
    //
    // ## Architecture
    // - gRPC → Mempool: ArrayQueue (MPMC) for multiple gRPC handlers
    // - P2P → Mempool: rtrb (SPSC) for single P2P thread
    //
    // For testing, we also fork P2P transactions to a test buffer for verification.
    //
    // ```
    // gRPC handlers ──► ArrayQueue ──────────────────────────────► MempoolService
    //                                                                    ▲
    // P2P ──► rtrb ──► Forwarder ──► rtrb (mempool_p2p_consumer) ────────┘
    //                      │
    //                      └──► rtrb (test_tx_consumer) ──► Test verification
    // ```

    // Lock-free queue for gRPC transactions (ArrayQueue for MPMC)
    let grpc_tx_queue = Arc::new(ArrayQueue::<Transaction>::new(BUFFER_SIZE));

    // Channel from P2P to forwarder (P2P thread → forwarder thread)
    let (p2p_tx_producer, p2p_from_network_consumer) = RingBuffer::new(BUFFER_SIZE);

    // Channel from forwarder to MempoolService (forwarder thread → mempool thread)
    let (mempool_p2p_producer, mempool_p2p_consumer) = RingBuffer::new(BUFFER_SIZE);

    // Channel from forwarder to test verification (for observing gossiped txs)
    let (test_tx_producer, test_tx_consumer) = RingBuffer::new(BUFFER_SIZE);

    // Test only: Forwarder thread for observing P2P gossip in tests.
    //
    // In production, P2P pushes directly to the mempool's rtrb channel.
    // In tests, we need to observe which transactions arrived via P2P gossip,
    // so we interpose a forwarder that routes to BOTH:
    // - The mempool (for actual processing)
    // - A test buffer (for test verification via `tx_consumer`)
    //
    // This forwarder does NOT exist in production code.
    let forwarder_shutdown = Arc::clone(&shutdown);
    std::thread::spawn(move || {
        let mut p2p_consumer: Consumer<Transaction> = p2p_from_network_consumer;
        let mut mempool_producer: Producer<Transaction> = mempool_p2p_producer;
        let mut test_producer: Producer<Transaction> = test_tx_producer;

        while !forwarder_shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            match p2p_consumer.pop() {
                Ok(tx) => {
                    // Forward to mempool (via rtrb - SPSC)
                    let _ = mempool_producer.push(tx.clone());
                    // Forward to test consumer (for verification)
                    let _ = test_producer.push(tx);
                }
                Err(_) => {
                    // No transactions available, sleep briefly
                    std::thread::sleep(Duration::from_micros(100));
                }
            }
        }
    });

    // Spawn MempoolService with both transaction sources:
    // - grpc_tx_queue: ArrayQueue for gRPC-submitted transactions
    // - mempool_p2p_consumer: rtrb Consumer for P2P-gossipped transactions (via forwarder)
    let (mempool_service, mempool_channels) = MempoolService::spawn(
        Arc::clone(&grpc_tx_queue),
        mempool_p2p_consumer,
        pending_state_reader.clone(),
        Arc::clone(&shutdown),
        logger.clone(),
    );

    // Extract values from identity before consuming it
    let peer_id = identity.peer_id();
    let bls_secret_key = identity.bls_secret_key().clone();

    // Spawn P2P service (consumes identity)
    let p2p_handle = spawn_p2p::<TokioRunner, N, F, M_SIZE>(
        TokioRunner::default(),
        p2p_config,
        identity, // identity is consumed here
        consensus_msg_producer,
        p2p_tx_producer, // P2P routes incoming txs here
        broadcast_consumer,
        Some(Arc::clone(&storage)),
        logger.new(o!("component" => "p2p")),
    );

    // Get broadcast_notify from P2P
    let broadcast_notify = Arc::clone(&p2p_handle.broadcast_notify);

    // P2P ready flag for health checks
    let p2p_ready = Arc::clone(&p2p_handle.is_ready);

    // Optionally create consensus engine
    let consensus_engine = if with_consensus {
        Some(
            ConsensusEngine::<N, F, M_SIZE>::new(
                consensus_config,
                peer_id,
                bls_secret_key,
                consensus_msg_consumer,
                broadcast_notify,
                broadcast_producer,
                mempool_channels.proposal_req_producer,
                mempool_channels.proposal_resp_consumer,
                mempool_channels.finalized_producer,
                persistence_writer,
                DEFAULT_TICK_INTERVAL,
                logger.new(o!("component" => "consensus")),
            )
            .expect("Failed to create consensus engine"),
        )
    } else {
        None
    };

    // Find an available port for gRPC server
    let grpc_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind to random port");
    let grpc_addr = grpc_listener.local_addr().expect("get local addr");
    drop(grpc_listener); // Release the port for the server

    // Create RPC context for gRPC server
    // Uses lock-free ArrayQueues - no mutex or bridge thread needed
    // gRPC pushes directly to grpc_tx_queue (shared with MempoolService)
    let rpc_context = RpcContext::new(
        Arc::clone(&storage),
        pending_state_reader,
        None,                                        // mempool_stats
        None,                                        // peer_stats
        None,                                        // block_events
        None,                                        // consensus_events
        None,                                        // tx_events
        Arc::clone(&p2p_handle.tx_broadcast_queue),  // P2P broadcast queue (gRPC → P2P)
        Arc::clone(&p2p_handle.tx_broadcast_notify), // P2P broadcast notify
        grpc_tx_queue,                               // Mempool queue (gRPC → Mempool)
        Arc::clone(&p2p_ready),
        logger.new(o!("component" => "grpc")),
    );

    let rpc_config = RpcConfig {
        listen_addr: grpc_addr,
        max_concurrent_streams: 100,
        request_timeout_secs: 30,
        peer_id,
        network: RpcNetwork::Local,
        total_validators: N as u32,
        f: F as u32,
    };

    // Spawn gRPC server in a separate thread with its own Tokio runtime
    // This is necessary because create_gossip_node is called before the TokioRunner starts
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("create tokio runtime for grpc");
        rt.block_on(async move {
            let server = RpcServer::new(rpc_config, rpc_context);
            let _ = server.serve().await;
        });
    });

    // Create a minimal identity info struct to store in node
    // (the actual identity was consumed by spawn_p2p)
    let node = GossipTestNode {
        p2p_handle,
        consensus_engine,
        tx_consumer: test_tx_consumer,
        mempool_service,
        peer_id,
        node_idx,
        grpc_addr,
        p2p_ready,
        logger,
    };

    (node, storage, temp_dir)
}

impl<const N: usize, const F: usize, const M_SIZE: usize> GossipTestNetwork<N, F, M_SIZE> {
    /// Wait for all nodes to complete bootstrap.
    pub async fn wait_for_bootstrap(&self) {
        for (i, node) in self.nodes.iter().enumerate() {
            slog::info!(self.logger, "Waiting for node bootstrap"; "node" => i);
            node.p2p_handle.wait_ready().await;
            slog::info!(self.logger, "Node bootstrap complete"; "node" => i);
        }
        slog::info!(self.logger, "All nodes bootstrapped successfully");
    }

    /// Shutdown all nodes gracefully.
    ///
    /// Shutdown order is critical to avoid race conditions:
    /// 1. Signal consensus to stop (prevents new proposals/votes)
    /// 2. Signal P2P to stop (prevents new messages)
    /// 3. Shutdown mempool (drains queues, stops bridge threads)
    /// 4. Wait for consensus threads to finish
    pub fn shutdown(mut self) {
        // Step 1: Signal consensus engines to stop
        for node in &self.nodes {
            if let Some(ref engine) = node.consensus_engine {
                engine.shutdown();
            }
        }

        // Step 2: Signal P2P services to stop
        for node in &self.nodes {
            node.p2p_handle.shutdown();
        }

        // Step 3: Shutdown mempool services (also stops bridge threads)
        for node in &mut self.nodes {
            node.mempool_service.shutdown();
        }

        // Step 4: Wait for consensus engines to finish
        for (i, node) in self.nodes.into_iter().enumerate() {
            if let Some(engine) = node.consensus_engine {
                let _ = engine.shutdown_and_wait(Duration::from_secs(10));
            }
            slog::debug!(self.logger, "Node shutdown complete"; "node" => i);
        }

        slog::info!(self.logger, "All nodes shut down successfully");
    }

    /// Create a new test transaction using the next available funded account.
    pub fn create_transaction(&self, account_idx: usize, nonce: u64) -> Transaction {
        let sk = &self.funded_keys[account_idx % self.funded_keys.len()];
        let recipient = Address::from_bytes([((account_idx + 1) % 256) as u8; 32]);
        create_test_transaction(sk, recipient, 100, nonce, 10)
    }

    /// Get all finalized blocks from a node's storage.
    pub fn get_finalized_blocks(&self, node_idx: usize) -> Vec<Block> {
        self.stores[node_idx]
            .get_all_finalized_blocks()
            .expect("Failed to get finalized blocks")
    }

    /// Get a gRPC transaction service client for a specific node.
    pub async fn transaction_client(
        &self,
        node_idx: usize,
    ) -> Result<TransactionServiceClient<Channel>, tonic::transport::Error> {
        let addr = self.nodes[node_idx].grpc_addr;
        let endpoint = format!("http://{}", addr);
        let channel = Channel::from_shared(endpoint)
            .expect("valid endpoint")
            .connect()
            .await?;
        Ok(TransactionServiceClient::new(channel))
    }

    /// Submit a transaction via gRPC to a specific node.
    ///
    /// This exercises the full gRPC -> P2P -> Gossip flow.
    pub async fn submit_transaction_via_grpc(
        &self,
        node_idx: usize,
        tx: &Transaction,
    ) -> Result<SubmitTransactionResponse, tonic::Status> {
        let mut client = self
            .transaction_client(node_idx)
            .await
            .map_err(|e| tonic::Status::unavailable(format!("failed to connect: {}", e)))?;

        let tx_bytes = consensus::storage::conversions::serialize_for_db(tx)
            .expect("serialize transaction")
            .to_vec();

        let request = SubmitTransactionRequest {
            transaction_bytes: tx_bytes,
        };

        let response = client.submit_transaction(request).await?;
        Ok(response.into_inner())
    }

    /// Serialize a transaction for gRPC submission.
    pub fn serialize_transaction(tx: &Transaction) -> Vec<u8> {
        consensus::storage::conversions::serialize_for_db(tx)
            .expect("serialize transaction")
            .to_vec()
    }
}

/// Waits for a transaction to appear in a node's tx_consumer.
///
/// Returns true if the transaction was received within the timeout.
pub fn wait_for_tx_in_consumer(
    tx_consumer: &mut Consumer<Transaction>,
    expected_tx_hash: [u8; 32],
    timeout: Duration,
) -> bool {
    let start = std::time::Instant::now();

    while start.elapsed() < timeout {
        match tx_consumer.pop() {
            Ok(tx) => {
                if tx.tx_hash == expected_tx_hash {
                    return true;
                }
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    false
}

/// Checks how many nodes received a specific transaction within a timeout.
pub fn count_nodes_with_tx(
    nodes: &mut [GossipTestNode<N, F, M_SIZE>],
    expected_tx_hash: [u8; 32],
    timeout: Duration,
) -> usize {
    let mut count = 0;
    let start = std::time::Instant::now();

    // Poll all nodes until timeout
    let mut received: HashSet<usize> = HashSet::new();

    while start.elapsed() < timeout && received.len() < nodes.len() {
        for (i, node) in nodes.iter_mut().enumerate() {
            if received.contains(&i) {
                continue;
            }

            if let Ok(tx) = node.tx_consumer.pop()
                && tx.tx_hash == expected_tx_hash
            {
                received.insert(i);
                count += 1;
            }
        }

        if received.len() < nodes.len() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    count
}

/// Drains any pending transactions from all node consumers.
/// Useful for clearing state between test phases.
pub fn drain_all_consumers(nodes: &mut [GossipTestNode<N, F, M_SIZE>]) {
    for node in nodes.iter_mut() {
        while node.tx_consumer.pop().is_ok() {}
    }
}
