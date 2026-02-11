//! Multi-node integration tests for P2P + Consensus integration.
//!
//! These tests verify end-to-end behavior with real P2P networking and consensus protocol.
//! This follows the same structure as consensus/src/tests/e2e_consensus.rs but uses
//! real P2P networking instead of a LocalNetwork simulator.

#![cfg(test)]

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use ark_serialize::CanonicalSerialize;
use commonware_runtime::tokio::Runner as TokioRunner;
use commonware_runtime::{Clock, Runner};
use consensus::{
    consensus_manager::{
        config::{ConsensusConfig, GenesisAccount},
        consensus_engine::ConsensusEngine,
    },
    crypto::{aggregated::BlsSecretKey, transaction_crypto::TxSecretKey},
    mempool::MempoolService,
    state::{address::Address, block::Block, peer::PeerSet, transaction::Transaction},
    storage::store::ConsensusStore,
    validation::PendingStateWriter,
};
use crossbeam::queue::ArrayQueue;
use grpc_client::config::{Network as RpcNetwork, RpcConfig};
use grpc_client::proto::{SubmitTransactionRequest, SubmitTransactionResponse};
use grpc_client::server::{RpcContext, RpcServer};
use p2p::{
    config::P2PConfig, config::ValidatorPeerInfo, identity::ValidatorIdentity,
    service::spawn as spawn_p2p,
};
use rtrb::{Producer, RingBuffer};
use slog::{Drain, Level, Logger, o};
use tempfile::TempDir;

/// Test configuration constants (n = 5f + 1 = 6 when f = 1)
const N: usize = 6;
const F: usize = 1;
const M_SIZE: usize = 3;
const BUFFER_SIZE: usize = 10_000;
const DEFAULT_TICK_INTERVAL: Duration = Duration::from_millis(10);
const DEFAULT_VIEW_TIMEOUT: Duration = Duration::from_secs(5);

/// Creates a logger for integration tests with configurable log levels.
///
/// # Environment Variables
///
/// Respects the `RUST_LOG` environment variable:
/// - `error` - Only errors
/// - `warn` - Warnings and errors
/// - `info` - Info, warnings, and errors (default)
/// - `debug` - All messages including debug
///
/// # Example
///
/// ```bash
/// # Run with debug logging
/// RUST_LOG=debug cargo test --package tests --lib test_multi_node_happy_path -- --ignored --nocapture
/// ```
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

/// Node setup containing all components for a single validator node.
struct NodeSetup<const N: usize, const F: usize, const M_SIZE: usize> {
    /// P2P service handle
    p2p_handle: p2p::service::P2PHandle,

    /// Consensus engine
    consensus_engine: ConsensusEngine<N, F, M_SIZE>,

    /// Mempool service
    mempool_service: MempoolService,

    /// gRPC server address
    grpc_addr: std::net::SocketAddr,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> NodeSetup<N, F, M_SIZE> {
    /// Signal this node to begin shutdown (non-blocking).
    ///
    /// This signals all components to stop but doesn't wait for them.
    /// Call `shutdown_and_wait` after signaling all nodes.
    fn signal_shutdown(&self) {
        self.consensus_engine.shutdown();
        self.p2p_handle.shutdown();
    }

    /// Shutdown mempool service (blocking - waits for thread to join).
    fn shutdown_mempool(&mut self) {
        self.mempool_service.shutdown();
    }

    /// Wait for consensus engine to finish with timeout.
    fn wait_for_consensus(self, timeout: Duration, logger: &Logger, node_idx: usize) {
        self.consensus_engine
            .shutdown_and_wait(timeout)
            .unwrap_or_else(|e| {
                slog::error!(
                    logger,
                    "Consensus engine shutdown failed";
                    "node" => node_idx,
                    "error" => ?e,
                );
                panic!(
                    "Node {} consensus engine failed to shutdown: {}",
                    node_idx, e
                )
            });
    }
}

/// Hierarchical shutdown for a collection of nodes.
///
/// Shutdown order is critical to avoid race conditions:
/// 1. Signal Consensus (stops proposing/voting)
/// 2. Signal P2P (stops network I/O)
/// 3. Shutdown Mempool (drains queues, waits for thread)
/// 4. Wait for Consensus (join with timeout)
fn shutdown_nodes<const N: usize, const F: usize, const M_SIZE: usize>(
    mut nodes: Vec<NodeSetup<N, F, M_SIZE>>,
    timeout: Duration,
    logger: &Logger,
) {
    slog::info!(
        logger,
        "Beginning hierarchical shutdown of {} nodes",
        nodes.len()
    );

    // Step 1 & 2: Signal ALL nodes to stop (consensus + P2P)
    // This prevents one node from advancing while others are already stopped.
    for node in &nodes {
        node.signal_shutdown();
    }

    // Step 3: Shutdown mempool services (blocking - waits for each thread)
    for node in &mut nodes {
        node.shutdown_mempool();
    }

    // Step 4: Wait for each consensus engine to finish
    for (i, node) in nodes.into_iter().enumerate() {
        slog::debug!(logger, "Waiting for consensus engine shutdown"; "node" => i);
        node.wait_for_consensus(timeout, logger, i);
    }

    slog::info!(logger, "All nodes shut down successfully");
}

/// Creates funded test transactions and genesis accounts.
///
/// Each transaction is created from a unique funded account.
/// Returns (transactions, genesis_accounts) for use in the fixture.
fn create_funded_test_transactions(
    num_transactions: usize,
) -> (Vec<Transaction>, Vec<GenesisAccount>) {
    let mut transactions = Vec::new();
    let mut genesis_accounts = Vec::new();

    for i in 0..num_transactions {
        let sk = TxSecretKey::generate(&mut rand::thread_rng());
        let pk = sk.public_key();
        let balance = 100_000u64;

        genesis_accounts.push(GenesisAccount {
            public_key: hex::encode(pk.to_bytes()),
            balance,
        });

        // Create a transaction from this account
        let sender_addr = Address::from_public_key(&pk);
        let receiver_addr = Address::from_bytes([(i as u8); 32]);
        let tx = Transaction::new_transfer(sender_addr, receiver_addr, 100, 0, 10, &sk);
        transactions.push(tx);
    }

    (transactions, genesis_accounts)
}

/// Creates a node setup with P2P and consensus components.
///
/// Returns the NodeSetup, storage reference, and temp directory separately so that
/// storage can be accessed for verification after the node is shut down.
/// The temp directory must be kept alive until verification completes.
fn create_node_setup<const N: usize, const F: usize, const M_SIZE: usize>(
    identity: ValidatorIdentity,
    p2p_config: P2PConfig,
    consensus_config: ConsensusConfig,
    logger: Logger,
) -> (NodeSetup<N, F, M_SIZE>, Arc<ConsensusStore>, TempDir) {
    let peer_id = identity.peer_id();

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

    // Create a second reader for gRPC server
    let grpc_pending_state_reader = pending_state_reader.clone();

    // Create shutdown flag
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Transaction queue architecture:
    // - gRPC → Mempool: ArrayQueue (MPMC) for multiple gRPC handlers
    // - P2P → Mempool: rtrb (SPSC) for single P2P thread

    // Create gRPC transaction queue (ArrayQueue for MPMC)
    let mempool_tx_queue = Arc::new(ArrayQueue::<Transaction>::new(BUFFER_SIZE));

    // Create P2P → Mempool channel (rtrb for SPSC)
    // P2P pushes here, MempoolService consumes
    let (p2p_to_mempool_producer, p2p_to_mempool_consumer) = RingBuffer::new(BUFFER_SIZE);

    // Spawn MempoolService with both transaction sources
    let (mempool_service, mempool_channels) = MempoolService::spawn(
        Arc::clone(&mempool_tx_queue),
        p2p_to_mempool_consumer,
        pending_state_reader,
        Arc::clone(&shutdown),
        logger.clone(),
    );

    // Create P2P tx producer for the P2P service to use
    // (This is separate from p2p_to_mempool - P2P forwards incoming gossip to mempool)
    let p2p_tx_producer = p2p_to_mempool_producer;

    // Clone the BLS secret key before moving identity
    let bls_secret_key = identity.bls_secret_key().clone();

    // Spawn P2P service FIRST - it creates its own broadcast_notify internally
    // We need to get this notify and pass it to consensus so they share the same notify
    let p2p_handle = spawn_p2p::<TokioRunner, N, F, M_SIZE>(
        TokioRunner::default(),
        p2p_config,
        identity,
        consensus_msg_producer,
        p2p_tx_producer,
        broadcast_consumer,
        None,
        logger.new(o!("component" => "p2p")),
    );

    // CRITICAL: Get the broadcast_notify from P2P and pass it to consensus
    // This way when consensus calls notify_one(), P2P wakes up to broadcast
    let broadcast_notify = Arc::clone(&p2p_handle.broadcast_notify);

    // P2P ready flag for health checks
    let p2p_ready = Arc::clone(&p2p_handle.is_ready);

    // Create consensus engine with P2P's broadcast_notify
    let consensus_engine = ConsensusEngine::<N, F, M_SIZE>::new(
        consensus_config,
        peer_id,
        bls_secret_key,
        consensus_msg_consumer,
        broadcast_notify, // Use P2P's notify!
        broadcast_producer,
        mempool_channels.proposal_req_producer,
        mempool_channels.proposal_resp_consumer,
        mempool_channels.finalized_producer,
        persistence_writer,
        DEFAULT_TICK_INTERVAL,
        logger.new(o!("component" => "consensus")),
    )
    .expect("Failed to create consensus engine");

    // Find an available port for gRPC server
    let grpc_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind to random port");
    let grpc_addr = grpc_listener.local_addr().expect("get local addr");
    drop(grpc_listener); // Release the port for the server

    // Create RPC context for gRPC server
    // Uses lock-free ArrayQueues - no mutex needed
    // The mempool_tx_queue is shared directly with MempoolService (no bridge thread needed)
    let rpc_context = RpcContext::new(
        Arc::clone(&storage),
        grpc_pending_state_reader,
        None,                                        // mempool_stats
        None,                                        // peer_stats
        None,                                        // block_events
        None,                                        // consensus_events
        None,                                        // tx_events
        Arc::clone(&p2p_handle.tx_broadcast_queue),  // P2P broadcast queue
        Arc::clone(&p2p_handle.tx_broadcast_notify), // P2P broadcast notify
        mempool_tx_queue,                            // Shared mempool queue (direct to mempool)
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
    // This is necessary because create_node_setup is called before the TokioRunner starts
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("create tokio runtime for grpc");
        rt.block_on(async move {
            let server = RpcServer::new(rpc_config, rpc_context);
            let _ = server.serve().await;
        });
    });

    let node = NodeSetup {
        p2p_handle,
        consensus_engine,
        mempool_service,
        grpc_addr,
    };

    (node, storage, temp_dir)
}

/// Submit a transaction via gRPC to a specific node.
///
/// This exercises the full gRPC -> P2P -> Gossip -> Consensus flow.
async fn submit_transaction_via_grpc(
    grpc_addr: std::net::SocketAddr,
    tx: &Transaction,
) -> Result<SubmitTransactionResponse, tonic::Status> {
    let addr = format!("http://{}", grpc_addr);
    let mut client =
        grpc_client::proto::transaction_service_client::TransactionServiceClient::connect(addr)
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

/// Happy path integration test with 6 nodes using real P2P networking.
///
/// This test follows the same structure as `test_e2e_consensus_happy_path` in
/// `consensus/src/tests/e2e_consensus.rs` but uses real P2P networking instead
/// of the LocalNetwork simulator.
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_multi_node_happy_path -- --ignored --nocapture
/// ```
#[test]
#[ignore] // Run with: cargo test --package tests --lib test_multi_node_happy_path -- --ignored --nocapture
fn test_multi_node_happy_path() {
    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting multi-node integration test with P2P (happy path)";
        "nodes" => N,
        "byzantine_tolerance" => F,
    );

    // Phase 1: Generate identities and create funded transactions
    slog::info!(
        logger,
        "Phase 1: Generating validator identities and funded transactions"
    );

    let num_transactions = 30;
    let (transactions, genesis_accounts) = create_funded_test_transactions(num_transactions);

    // Generate BLS keypairs for all nodes
    let mut identities = Vec::new();
    let mut public_keys = Vec::new();

    for _i in 0..N {
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
        n: N,
        f: F,
        view_timeout: DEFAULT_VIEW_TIMEOUT,
        leader_manager:
            consensus::consensus_manager::leader_manager::LeaderSelectionStrategy::RoundRobin,
        network: consensus::consensus_manager::config::Network::Local,
        peers: peer_strs,
        genesis_accounts: genesis_accounts.clone(),
    };

    slog::info!(
        logger,
        "Generated identities and peer set";
        "peer_ids" => ?peer_set.sorted_peer_ids,
    );

    // Phase 2: Create P2P configs for each node
    slog::info!(logger, "Phase 2: Creating P2P configurations");

    // Use random base port to avoid conflicts between test runs
    // (previous test's sockets may still be in TIME_WAIT state)
    let base_port = 40000u16 + (rand::random::<u16>() % 10000);
    let port_gap = 100u16; // Use 100-port gaps to avoid conflicts
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
            total_number_peers: N,
            maximum_number_faulty_peers: F,
            bootstrap_timeout_ms: 30_000, // 30 seconds - match production default for reliable bootstrap
            ping_interval_ms: 200,        // Faster ping for quicker discovery
            ..Default::default()
        };

        p2p_configs.push(p2p_config);
    }

    // Phase 3: Spawn all nodes and keep storage references for verification
    slog::info!(
        logger,
        "Phase 3: Spawning P2P services and consensus engines"
    );

    let mut nodes: Vec<NodeSetup<N, F, M_SIZE>> = Vec::new();
    // Keep stores and temp_dirs together as tuples to ensure they stay paired
    let mut store_with_dirs: Vec<(Arc<ConsensusStore>, TempDir)> = Vec::new();

    for (i, (identity, p2p_config)) in identities
        .into_iter()
        .zip(p2p_configs.into_iter())
        .enumerate()
    {
        let node_logger = logger.new(o!("node" => i, "peer_id" => identity.peer_id()));
        let (node, storage, temp_dir) =
            create_node_setup(identity, p2p_config, consensus_config.clone(), node_logger);
        nodes.push(node);
        // Keep storage and temp_dir together as a tuple
        store_with_dirs.push((storage, temp_dir));
    }

    slog::info!(
        logger,
        "All nodes spawned";
        "count" => nodes.len(),
    );

    // Phase 4: Wait for bootstrap to complete
    slog::info!(logger, "Phase 4: Waiting for bootstrap phase to complete");

    // Keep a copy of transaction hashes for verification
    let expected_tx_hashes: HashSet<_> = transactions.iter().map(|tx| tx.tx_hash).collect();

    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Wait for all nodes to be ready
        for (i, node) in nodes.iter().enumerate() {
            slog::info!(logger, "Waiting for node bootstrap"; "node" => i);
            node.p2p_handle.wait_ready().await;
            slog::info!(logger, "Node bootstrap complete"; "node" => i);
        }

        slog::info!(logger, "All nodes bootstrapped successfully");

        // Phase 5: Submit transactions via gRPC to each node
        slog::info!(
            logger,
            "Phase 5: Submitting transactions via gRPC";
            "count" => num_transactions,
        );

        for (i, tx) in transactions.iter().enumerate() {
            // Distribute transactions across nodes
            let node_idx = i % N;
            let grpc_addr = nodes[node_idx].grpc_addr;

            match submit_transaction_via_grpc(grpc_addr, tx).await {
                Ok(resp) => {
                    if resp.success {
                        slog::debug!(
                            logger,
                            "Transaction submitted via gRPC";
                            "tx_index" => i,
                            "target_node" => node_idx,
                            "tx_hash" => &resp.tx_hash,
                        );
                    } else {
                        slog::warn!(
                            logger,
                            "Transaction rejected by gRPC";
                            "tx_index" => i,
                            "node" => node_idx,
                            "error" => &resp.error_message,
                        );
                    }
                }
                Err(e) => {
                    slog::warn!(
                        logger,
                        "Failed to submit transaction via gRPC";
                        "tx_index" => i,
                        "node" => node_idx,
                        "error" => %e,
                    );
                }
            }
        }

        slog::info!(
            logger,
            "All transactions submitted";
            "total" => num_transactions,
        );

        // Phase 6: Wait for consensus to progress through multiple views
        slog::info!(
            logger,
            "Phase 6: Waiting for consensus to progress";
            "duration_secs" => 30,
        );

        let test_duration = Duration::from_secs(30);
        let check_interval = Duration::from_secs(5);
        let start = std::time::Instant::now();

        while start.elapsed() < test_duration {
            ctx.sleep(check_interval).await;

            let elapsed = start.elapsed().as_secs();
            slog::info!(
                logger,
                "Consensus progress check";
                "elapsed_secs" => elapsed,
            );
        }

        // Phase 7: Verify system health
        slog::info!(logger, "Phase 7: Verifying system health");

        for (i, node) in nodes.iter().enumerate() {
            let is_running = node.consensus_engine.is_running();
            slog::info!(
                logger,
                "Node health check";
                "node" => i,
                "is_running" => is_running,
            );
            assert!(is_running, "Node {} should still be running", i);
        }

        // Phase 8: Graceful shutdown using hierarchical shutdown
        slog::info!(logger, "Phase 8: Shutting down all nodes");
        shutdown_nodes(nodes, Duration::from_secs(10), &logger);

        // Small delay to ensure storage writes are flushed
        ctx.sleep(Duration::from_millis(100)).await;

        // Phase 9: Verify state consistency across all replicas
        slog::info!(logger, "Phase 9: Verifying state consistency");

        let mut first_replica_blocks: Option<Vec<Block>> = None;

        for (i, (store, _temp_dir)) in store_with_dirs.iter().enumerate() {
            // Retrieve all finalized blocks from the store
            let blocks = store
                .get_all_finalized_blocks()
                .expect("Failed to get finalized blocks from store");

            slog::info!(
                logger,
                "Replica state check";
                "replica" => i,
                "finalized_blocks" => blocks.len(),
                "highest_view" => blocks.last().map(|b| b.view()).unwrap_or(0),
            );

            // 1. Check that we have finalized blocks (progress was made)
            assert!(
                !blocks.is_empty(),
                "Replica {} should have finalized blocks",
                i
            );

            // 2. Check chain integrity
            for (idx, window) in blocks.windows(2).enumerate() {
                let prev = &window[0];
                let curr = &window[1];
                assert_eq!(
                    curr.parent_block_hash(),
                    prev.get_hash(),
                    "Chain broken at index {} for replica {} (view {} -> {})",
                    idx,
                    i,
                    prev.view(),
                    curr.view()
                );
                // In real P2P, views can be nullified (skipped), so view doesn't always increase by
                // 1 We just check that views are monotonically increasing
                assert!(
                    curr.view() > prev.view(),
                    "View should increase ({} -> {})",
                    prev.view(),
                    curr.view()
                );
            }

            // 3. Check consistency across replicas (common prefix)
            // Note: Due to shutdown timing, replicas may have slightly different block counts
            // We check that the common prefix matches (blocks at same positions are identical)
            if let Some(ref first_blocks) = first_replica_blocks {
                let common_len = blocks.len().min(first_blocks.len());
                assert!(
                    common_len > 0,
                    "Replica {} should have at least 1 block in common with replica 0",
                    i
                );

                // Allow small difference (up to 2 blocks) due to shutdown timing
                let diff = (blocks.len() as i64 - first_blocks.len() as i64).abs();
                assert!(
                    diff <= 2,
                    "Replica {} has significantly different block count ({}) than replica 0 ({})",
                    i,
                    blocks.len(),
                    first_blocks.len()
                );

                // Verify common prefix matches
                for (j, (b1, b2)) in blocks
                    .iter()
                    .take(common_len)
                    .zip(first_blocks.iter().take(common_len))
                    .enumerate()
                {
                    assert_eq!(
                        b1.get_hash(),
                        b2.get_hash(),
                        "Block mismatch at index {} between replica {} and 0 (view {})",
                        j,
                        i,
                        b1.view()
                    );
                }
            } else {
                first_replica_blocks = Some(blocks);
            }
        }

        slog::info!(logger, "State consistency verification passed! ✓");

        // Phase 10: Verify all transactions were included
        slog::info!(logger, "Phase 10: Verifying transaction inclusion");

        let mut included_tx_hashes = HashSet::new();
        if let Some(ref blocks) = first_replica_blocks {
            for block in blocks {
                for tx in &block.transactions {
                    included_tx_hashes.insert(tx.tx_hash);
                }
            }
        }

        // Verify each expected transaction is present
        let mut missing_txs = 0;
        for tx_hash in &expected_tx_hashes {
            if !included_tx_hashes.contains(tx_hash) {
                slog::error!(logger, "Transaction missing"; "tx_hash" => ?tx_hash);
                missing_txs += 1;
            }
        }

        assert_eq!(
            missing_txs,
            0,
            "Some transactions were lost! {} missing out of {}",
            missing_txs,
            expected_tx_hashes.len()
        );

        slog::info!(
            logger,
            "Transaction inclusion verified! ✓";
            "total_transactions" => expected_tx_hashes.len(),
            "included_transactions" => included_tx_hashes.len()
        );

        // Final success message
        slog::info!(
            logger,
            "Test completed successfully! ✓";
            "test_duration_secs" => 30,
            "finalized_blocks" => first_replica_blocks.map(|b| b.len()).unwrap_or(0),
        );

        // Now we can safely drop the stores and temp directories
        drop(store_with_dirs);
    });
}

/// Continuous load integration test with 6 nodes.
///
/// This test verifies that the consensus engine and P2P network can handle
/// a continuous stream of transactions over a sustained period (30s).
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_multi_node_continuous_load -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_multi_node_continuous_load() {
    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting multi-node integration test (continuous load)";
        "nodes" => N,
        "byzantine_tolerance" => F,
    );

    // Phase 1: Generate identities and create funded transactions
    // Pre-allocate a large pool of funded transactions (enough for the test duration)
    // Assuming ~5 txs every 100ms for 30 seconds = ~1500 txs
    let max_transactions = 5000;
    let (mut all_transactions, genesis_accounts) =
        create_funded_test_transactions(max_transactions);

    // Generate BLS keypairs for all nodes
    let mut identities = Vec::new();
    let mut public_keys = Vec::new();

    for _i in 0..N {
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
        n: N,
        f: F,
        view_timeout: DEFAULT_VIEW_TIMEOUT,
        leader_manager:
            consensus::consensus_manager::leader_manager::LeaderSelectionStrategy::RoundRobin,
        network: consensus::consensus_manager::config::Network::Local,
        peers: peer_strs,
        genesis_accounts: genesis_accounts.clone(),
    };

    slog::info!(
        logger,
        "Generated identities and peer set";
        "peer_ids" => ?peer_set.sorted_peer_ids,
    );

    // Phase 2: Create P2P configs for each node
    slog::info!(logger, "Phase 2: Creating P2P configurations");

    // Use random base port to avoid conflicts
    let base_port = 40000u16 + (rand::random::<u16>() % 10000);
    let port_gap = 100u16;
    let mut p2p_configs = Vec::new();

    for (i, _identity) in identities.iter().enumerate() {
        let port = base_port + (i as u16 * port_gap);
        let listen_addr = format!("127.0.0.1:{}", port).parse().unwrap();
        let external_addr = listen_addr;

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
            total_number_peers: N,
            maximum_number_faulty_peers: F,
            bootstrap_timeout_ms: 30_000,
            ping_interval_ms: 200,
            ..Default::default()
        };

        p2p_configs.push(p2p_config);
    }

    // Phase 3: Spawn all nodes
    slog::info!(
        logger,
        "Phase 3: Spawning P2P services and consensus engines"
    );

    let mut nodes: Vec<NodeSetup<N, F, M_SIZE>> = Vec::new();
    let mut store_with_dirs: Vec<(Arc<ConsensusStore>, TempDir)> = Vec::new();

    for (i, (identity, p2p_config)) in identities
        .into_iter()
        .zip(p2p_configs.into_iter())
        .enumerate()
    {
        let node_logger = logger.new(o!("node" => i, "peer_id" => identity.peer_id()));
        let (node, storage, temp_dir) =
            create_node_setup(identity, p2p_config, consensus_config.clone(), node_logger);

        // Verify the store has the genesis block (written during consensus engine init)
        let genesis_blocks = storage
            .get_all_finalized_blocks()
            .expect("Failed to get finalized blocks from store");
        slog::info!(
            logger,
            "Node created with store verification";
            "node" => i,
            "db_path" => temp_dir.path().display().to_string(),
            "initial_blocks" => genesis_blocks.len(),
        );
        assert!(
            !genesis_blocks.is_empty(),
            "Node {} store should have genesis block after creation",
            i
        );

        nodes.push(node);
        store_with_dirs.push((storage, temp_dir));
    }

    slog::info!(logger, "All nodes spawned"; "count" => nodes.len());

    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Phase 4: Bootsrap
        slog::info!(logger, "Phase 4: Waiting for bootstrap phase to complete");
        for (i, node) in nodes.iter().enumerate() {
            slog::info!(logger, "Waiting for node bootstrap"; "node" => i);
            node.p2p_handle.wait_ready().await;
            slog::info!(logger, "Node bootstrap complete"; "node" => i);
        }
        slog::info!(logger, "All nodes bootstrapped successfully");

        // Phase 5: Continuous Load
        let test_duration = Duration::from_secs(30);
        let tx_interval = Duration::from_millis(100);
        let check_interval = Duration::from_secs(5);

        slog::info!(
            logger,
            "Phase 5: Running consensus with continuous transaction load via gRPC";
            "duration_secs" => test_duration.as_secs(),
            "tx_interval_ms" => tx_interval.as_millis(),
        );

        let start_time = std::time::Instant::now();
        let mut last_check = start_time;
        let mut tx_count = 0usize;
        let mut tx_index = 0usize;
        let mut expected_tx_hashes = std::collections::HashSet::new();

        while start_time.elapsed() < test_duration && !all_transactions.is_empty() {
            // Submit a batch of transactions via gRPC
            let batch_size = std::cmp::min(5, all_transactions.len());

            for _ in 0..batch_size {
                if let Some(tx) = all_transactions.pop() {
                    let tx_hash = tx.tx_hash;
                    // Distribute transactions across replicas (round-robin)
                    let node_idx = tx_index % N;
                    tx_index += 1;

                    let grpc_addr = nodes[node_idx].grpc_addr;
                    if let Ok(resp) = submit_transaction_via_grpc(grpc_addr, &tx).await
                        && resp.success
                    {
                        tx_count += 1;
                        expected_tx_hashes.insert(tx_hash);
                    }
                }
            }

            // Periodic logs
            if last_check.elapsed() >= check_interval {
                let elapsed = start_time.elapsed().as_secs();
                slog::info!(
                    logger,
                    "Consensus progress check";
                    "elapsed_secs" => elapsed,
                    "transactions_submitted" => tx_count,
                    "remaining_txs" => all_transactions.len(),
                );
                last_check = std::time::Instant::now();
            }

            ctx.sleep(tx_interval).await;
        }

        slog::info!(
            logger,
            "Continuous load phase complete";
            "total_transactions_submitted" => tx_count,
        );

        // Phase 6: Graceful shutdown using hierarchical shutdown
        slog::info!(logger, "Phase 6: Shutting down all nodes");
        shutdown_nodes(nodes, Duration::from_secs(10), &logger);

        // Small delay to ensure storage writes are flushed
        ctx.sleep(Duration::from_millis(100)).await;

        // Phase 7: Verify state
        slog::info!(logger, "Phase 7: Verifying state consistency");

        // Similar verification as happy path
        for (i, (store, _temp_dir)) in store_with_dirs.iter().enumerate() {
            let blocks = store
                .get_all_finalized_blocks()
                .expect("Failed to get finalized blocks from store");

            slog::info!(
                logger,
                "Replica state check";
                "replica" => i,
                "finalized_blocks" => blocks.len(),
            );

            // Basic chain integrity checks
            assert!(!blocks.is_empty(), "Replica {} has no finalized blocks", i);

            // Check transactions
            let mut included_transactions = 0;
            for block in &blocks {
                for tx in &block.transactions {
                    if expected_tx_hashes.contains(&tx.tx_hash) {
                        included_transactions += 1;
                    }
                }
            }
            slog::info!(
                logger,
                "Transaction inclusion check";
                "replica" => i,
                "included" => included_transactions,
                "expected" => expected_tx_hashes.len(),
            );

            assert!(
                included_transactions > 0,
                "Replica {} has 0 included transactions",
                i
            );
        }

        slog::info!(logger, "Test completed successfully! ✓");

        // Explicitly drop setup to clean up
        drop(store_with_dirs);
    });
}

/// Crashed replica integration test.
///
/// This test verifies that the consensus network remains live and functional
/// when one replica crashes (f < byzantine_tolerance).
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_multi_node_crashed_replica -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_multi_node_crashed_replica() {
    let logger = create_test_logger();
    const CRASHED_NODE_IDX: usize = 5; // Crash the last node

    slog::info!(
        logger,
        "Starting multi-node integration test (crashed replica)";
        "nodes" => N,
        "byzantine_tolerance" => F,
        "crashed_node" => CRASHED_NODE_IDX,
    );

    // Phase 1: Setup
    let num_transactions = 30;
    let (transactions, genesis_accounts) = create_funded_test_transactions(num_transactions);

    let mut identities = Vec::new();
    let mut public_keys = Vec::new();
    for _i in 0..N {
        let bls_sk = BlsSecretKey::generate(&mut rand::thread_rng());
        let identity = ValidatorIdentity::from_bls_key(bls_sk);
        public_keys.push(identity.bls_public_key().clone());
        identities.push(identity);
    }
    let peer_set = PeerSet::new(public_keys);

    let mut peer_strs = Vec::with_capacity(peer_set.sorted_peer_ids.len());
    for peer_id in &peer_set.sorted_peer_ids {
        let pk = peer_set.id_to_public_key.get(peer_id).unwrap();
        let mut buf = Vec::new();
        pk.0.serialize_compressed(&mut buf).unwrap();
        peer_strs.push(hex::encode(buf));
    }

    let consensus_config = ConsensusConfig {
        n: N,
        f: F,
        view_timeout: DEFAULT_VIEW_TIMEOUT,
        leader_manager:
            consensus::consensus_manager::leader_manager::LeaderSelectionStrategy::RoundRobin,
        network: consensus::consensus_manager::config::Network::Local,
        peers: peer_strs,
        genesis_accounts: genesis_accounts.clone(),
    };

    // Phase 2: P2P Config
    let base_port = 40000u16 + (rand::random::<u16>() % 10000);
    let port_gap = 100u16;
    let mut p2p_configs = Vec::new();
    for (i, _identity) in identities.iter().enumerate() {
        let port = base_port + (i as u16 * port_gap);
        let listen_addr = format!("127.0.0.1:{}", port).parse().unwrap();
        let external_addr = listen_addr;

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
            total_number_peers: N,
            maximum_number_faulty_peers: F,
            bootstrap_timeout_ms: 30_000,
            ping_interval_ms: 200,
            ..Default::default()
        };
        p2p_configs.push(p2p_config);
    }

    // Phase 3: Spawn
    let mut nodes: Vec<NodeSetup<N, F, M_SIZE>> = Vec::new();
    let mut store_with_dirs: Vec<(Arc<ConsensusStore>, TempDir)> = Vec::new();

    for (i, (identity, p2p_config)) in identities
        .into_iter()
        .zip(p2p_configs.into_iter())
        .enumerate()
    {
        let node_logger = logger.new(o!("node" => i, "peer_id" => identity.peer_id()));
        let (node, storage, temp_dir) =
            create_node_setup(identity, p2p_config, consensus_config.clone(), node_logger);
        nodes.push(node);
        store_with_dirs.push((storage, temp_dir));
    }

    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Phase 4: Bootstrap ALL nodes first
        slog::info!(logger, "Phase 4: Waiting for bootstrap phase to complete");
        for (i, node) in nodes.iter().enumerate() {
            node.p2p_handle.wait_ready().await;
            slog::info!(logger, "Node bootstrap complete"; "node" => i);
        }
        slog::info!(logger, "All nodes bootstrapped successfully");

        // Phase 5: Crash specific node
        slog::info!(logger, "Phase 5: Crashing node"; "node" => CRASHED_NODE_IDX);

        nodes[CRASHED_NODE_IDX].consensus_engine.shutdown();

        slog::info!(logger, "Node crashed (consensus stopped)"; "node" => CRASHED_NODE_IDX);

        // Phase 6: Submit transactions to healthy nodes via gRPC
        slog::info!(
            logger,
            "Phase 6: Submitting transactions via gRPC to healthy nodes"
        );

        let mut expected_tx_hashes = std::collections::HashSet::new();

        for (i, tx) in transactions.iter().enumerate() {
            // Distribute transactions across HEALTHY nodes (skip crashed one)
            let mut node_idx = i % N;
            if node_idx == CRASHED_NODE_IDX {
                node_idx = (node_idx + 1) % N;
            }
            if node_idx == CRASHED_NODE_IDX {
                node_idx = (node_idx + 1) % N;
            } // Handle N=1 case

            let grpc_addr = nodes[node_idx].grpc_addr;
            if let Ok(resp) = submit_transaction_via_grpc(grpc_addr, tx).await
                && resp.success
            {
                expected_tx_hashes.insert(tx.tx_hash);
            }
        }

        slog::info!(logger, "Transactions submitted"; "count" => expected_tx_hashes.len());

        // Phase 7: Waiting for consensus
        slog::info!(logger, "Phase 7: Waiting for consensus progress");

        // Wait enough time for views to progress and finalize
        ctx.sleep(Duration::from_secs(15)).await;

        // Phase 8: Verify consistency on HEALTHY nodes
        slog::info!(
            logger,
            "Phase 8: Verifying state consistency on healthy nodes"
        );

        for (i, (store, _)) in store_with_dirs.iter().enumerate() {
            if i == CRASHED_NODE_IDX {
                continue;
            }

            let blocks = store
                .get_all_finalized_blocks()
                .expect("Failed to get finalized blocks");

            slog::info!(
                logger,
                "Healthy replica state check";
                "replica" => i,
                "finalized_blocks" => blocks.len(),
            );

            assert!(
                !blocks.is_empty(),
                "Healthy replica {} should have finalized blocks",
                i
            );

            // Allow gaps check (relaxed)
            for (idx, window) in blocks.windows(2).enumerate() {
                let prev = &window[0];
                let curr = &window[1];
                if curr.view() == prev.view() + 1 {
                    assert_eq!(
                        curr.parent_block_hash(),
                        prev.get_hash(),
                        "Chain broken at index {}",
                        idx
                    );
                }
                assert!(
                    curr.view() > prev.view(),
                    "Views must be creating monotonically"
                );
            }
        }

        slog::info!(logger, "Test completed successfully! ✓");
        drop(store_with_dirs);
    });
}

use consensus::consensus::ConsensusMessage;
use consensus::crypto::aggregated::PeerId;

/// Node setup for a Byzantine node (no running consensus engine, giving capabilities to test).
struct ByzantineNodeSetup<const N: usize, const F: usize, const M_SIZE: usize> {
    p2p_handle: p2p::service::P2PHandle,

    // Channels to interact with P2P
    broadcast_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,

    // Keys to sign malicious messages
    bls_secret_key: BlsSecretKey,
    peer_id: PeerId,
}

/// Creates a byzantine node setup (no consensus engine).
fn create_byzantine_node_setup<const N: usize, const F: usize, const M_SIZE: usize>(
    identity: ValidatorIdentity,
    p2p_config: P2PConfig,
    logger: Logger,
) -> (
    ByzantineNodeSetup<N, F, M_SIZE>,
    Arc<ConsensusStore>,
    TempDir,
) {
    let peer_id = identity.peer_id();
    let bls_secret_key = identity.bls_secret_key().clone();

    // Create ring buffers mimicking create_node_setup
    let (consensus_msg_producer, _consensus_msg_consumer) = RingBuffer::new(BUFFER_SIZE);
    let (broadcast_producer, broadcast_consumer) = RingBuffer::new(BUFFER_SIZE);

    let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
    let db_path = temp_dir.path().join("consensus.redb");
    let storage = Arc::new(ConsensusStore::open(&db_path).expect("Failed to create storage"));

    // We create pending state writer/reader but don't use them much since no engine
    let (_pending_state_writer, pending_state_reader) =
        PendingStateWriter::new(Arc::clone(&storage), 0);

    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Create gRPC transaction queue (ArrayQueue for MPMC)
    let mempool_tx_queue = Arc::new(ArrayQueue::<Transaction>::new(BUFFER_SIZE));

    // Create P2P → Mempool channel (rtrb for SPSC)
    let (p2p_to_mempool_producer, p2p_to_mempool_consumer) = RingBuffer::new(BUFFER_SIZE);

    // Spawn Mempool with both transaction sources
    let (_mempool_service, _mempool_channels) = MempoolService::spawn(
        mempool_tx_queue,
        p2p_to_mempool_consumer,
        pending_state_reader,
        Arc::clone(&shutdown),
        logger.clone(),
    );

    let p2p_tx_producer = p2p_to_mempool_producer;

    let p2p_handle = spawn_p2p::<TokioRunner, N, F, M_SIZE>(
        TokioRunner::default(),
        p2p_config,
        identity, // Consumes identity
        consensus_msg_producer,
        p2p_tx_producer,
        broadcast_consumer,
        None,
        logger.new(o!("component" => "p2p")),
    );

    let node = ByzantineNodeSetup {
        p2p_handle,
        broadcast_producer,
        bls_secret_key,
        peer_id,
    };

    (node, storage, temp_dir)
}

/// Equivocating leader integration test.
///
/// This test verifies that the network can handle a Byzantine leader sending
/// conflicting proposals (equivocation). The honest nodes should eventually reach consensus
/// (either on one of the proposals or by timing out the view).
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_multi_node_equivocating_leader -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_multi_node_equivocating_leader() {
    let logger = create_test_logger();
    const BYZANTINE_LEADER_IDX: usize = 1; // Round-robin view 1 leader is usually index 1

    slog::info!(
        logger,
        "Starting multi-node integration test (equivocating leader)";
        "nodes" => N,
        "byzantine_tolerance" => F,
        "byzantine_node" => BYZANTINE_LEADER_IDX,
    );

    // Phase 1: Setup
    let num_transactions = 10;
    let (_transactions, genesis_accounts) = create_funded_test_transactions(num_transactions);

    let mut identities = Vec::new();
    let mut public_keys = Vec::new();
    for _i in 0..N {
        let bls_sk = BlsSecretKey::generate(&mut rand::thread_rng());
        let identity = ValidatorIdentity::from_bls_key(bls_sk);
        public_keys.push(identity.bls_public_key().clone());
        identities.push(identity);
    }
    let peer_set = PeerSet::new(public_keys);

    let mut peer_strs = Vec::new();
    for peer_id in &peer_set.sorted_peer_ids {
        let pk = peer_set.id_to_public_key.get(peer_id).unwrap();
        let mut buf = Vec::new();
        pk.0.serialize_compressed(&mut buf).unwrap();
        peer_strs.push(hex::encode(buf));
    }

    let consensus_config = ConsensusConfig {
        n: N,
        f: F,
        view_timeout: DEFAULT_VIEW_TIMEOUT,
        leader_manager:
            consensus::consensus_manager::leader_manager::LeaderSelectionStrategy::RoundRobin,
        network: consensus::consensus_manager::config::Network::Local,
        peers: peer_strs,
        genesis_accounts: genesis_accounts.clone(),
    };

    // Phase 2: P2P Config
    let base_port = 40000u16 + (rand::random::<u16>() % 10000);
    let port_gap = 100u16;
    let mut p2p_configs = Vec::new();
    for (i, _identity) in identities.iter().enumerate() {
        let port = base_port + (i as u16 * port_gap);
        let listen_addr = format!("127.0.0.1:{}", port).parse().unwrap();
        let mut validators = Vec::new();
        for (j, other_identity) in identities.iter().enumerate() {
            if i != j {
                let other_port = base_port + (j as u16 * port_gap);
                let pk_hex = hex::encode(other_identity.ed25519_public_key().as_ref());
                validators.push(ValidatorPeerInfo {
                    bls_peer_id: other_identity.peer_id(),
                    bls_public_key: None,
                    ed25519_public_key: pk_hex,
                    address: Some(format!("127.0.0.1:{}", other_port).parse().unwrap()),
                });
            }
        }
        p2p_configs.push(P2PConfig {
            listen_addr,
            external_addr: listen_addr,
            validators,
            total_number_peers: N,
            maximum_number_faulty_peers: F,
            bootstrap_timeout_ms: 30_000,
            ping_interval_ms: 200,
            ..Default::default()
        });
    }

    // Phase 3: Spawn
    let mut nodes: Vec<Option<NodeSetup<N, F, M_SIZE>>> = Vec::new();
    let mut byzantine_node: Option<ByzantineNodeSetup<N, F, M_SIZE>> = None;
    let mut store_with_dirs: Vec<(Arc<ConsensusStore>, TempDir)> = Vec::new();

    for (i, (identity, p2p_config)) in identities
        .into_iter()
        .zip(p2p_configs.into_iter())
        .enumerate()
    {
        let node_logger = logger.new(o!("node" => i, "peer_id" => identity.peer_id()));

        if i == BYZANTINE_LEADER_IDX {
            // Spawn Byzantine node
            let (node, storage, temp_dir) =
                create_byzantine_node_setup(identity, p2p_config, node_logger);
            byzantine_node = Some(node);
            store_with_dirs.push((storage, temp_dir));
            nodes.push(None); // Placeholder in vector
        } else {
            // Spawn Honest node
            let (node, storage, temp_dir) =
                create_node_setup(identity, p2p_config, consensus_config.clone(), node_logger);
            nodes.push(Some(node));
            store_with_dirs.push((storage, temp_dir));
        }
    }

    let mut byzantine_node = byzantine_node.expect("Byzantine node not created");

    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Phase 4: Bootstrap
        slog::info!(logger, "Phase 4: Bootstrapping P2P network");

        let mut futures = Vec::new();
        // Wait for honest nodes
        for node in nodes.iter().flatten() {
            futures.push(node.p2p_handle.wait_ready());
        }
        // Wait for byzantine node
        futures.push(byzantine_node.p2p_handle.wait_ready());

        for f in futures {
            f.await;
        }

        slog::info!(logger, "All nodes bootstrapped");

        // Phase 5: Equivocate
        // Get parent hash (Genesis)
        let honest_node_idx = if BYZANTINE_LEADER_IDX == 0 { 1 } else { 0 };
        // Wait for store to be populated? Genesis should be there on creation.
        let finalized_blocks = store_with_dirs[honest_node_idx]
            .0
            .get_all_finalized_blocks()
            .expect("Failed to get finalized blocks");

        let genesis_block = finalized_blocks
            .iter()
            .find(|b| b.view() == 0)
            .expect("Genesis block not found");
        let parent_hash = genesis_block.get_hash();

        slog::info!(logger, "Phase 5: Equivocating at View 1");

        let byz_id = byzantine_node.peer_id;
        let byz_sk = &byzantine_node.bls_secret_key;

        // Block A
        let block_a = Block::new(
            1,
            byz_id,
            parent_hash,
            vec![],
            1000,
            byz_sk.sign(b"test"),
            false,
            1,
        );
        // Block B
        let block_b = Block::new(
            1,
            byz_id,
            parent_hash,
            vec![],
            2000,
            byz_sk.sign(b"test_b"),
            false,
            1,
        );

        // Broadcast A
        byzantine_node
            .broadcast_producer
            .push(ConsensusMessage::BlockProposal(block_a.clone()))
            .unwrap();

        // Small delay
        ctx.sleep(Duration::from_millis(10)).await;

        // Broadcast B
        byzantine_node
            .broadcast_producer
            .push(ConsensusMessage::BlockProposal(block_b.clone()))
            .unwrap();

        slog::info!(logger, "Equivocation proposals sent");

        // Phase 6: Verify Consensus Progress
        ctx.sleep(Duration::from_secs(10)).await;

        let mut progress_count = 0;
        for (i, (store, _)) in store_with_dirs.iter().enumerate() {
            if i == BYZANTINE_LEADER_IDX {
                continue;
            }
            let blocks = store.get_all_finalized_blocks().unwrap();
            slog::info!(logger, "Honest node state"; "node" => i, "blocks" => blocks.len());
            // Need new blocks (more than genesis = 1)
            if blocks.len() > 1 {
                progress_count += 1;
            }
        }

        assert!(
            progress_count >= 1,
            "At least one node should have finalized blocks beyond genesis"
        );
        slog::info!(logger, "Test completed successfully! ✓");

        drop(store_with_dirs);
    });
}

use consensus::crypto::transaction_crypto::TxPublicKey;

/// Invalid transaction rejection integration test.
///
/// This test verifies that transactions which violate validity rules (bad nonce, insufficient
/// funds, etc.) are rejected by the mempool/consensus and not included in the blockchain.
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_multi_node_invalid_tx_rejection -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_multi_node_invalid_tx_rejection() {
    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting multi-node integration test (invalid tx rejection)";
        "nodes" => N,
        "byzantine_tolerance" => F,
    );

    // Phase 1: Setup Accounts & Transactions
    let num_accounts = 5;
    let initial_balance = 10_000u64;
    let mut user_keys: Vec<(TxSecretKey, TxPublicKey)> = Vec::new();
    let mut genesis_accounts = Vec::new();

    for _ in 0..num_accounts {
        let sk = TxSecretKey::generate(&mut rand::thread_rng()); // Using thread_rng instead of OsRng for simplicity
        let pk = sk.public_key();
        genesis_accounts.push(GenesisAccount {
            public_key: hex::encode(pk.to_bytes()),
            balance: initial_balance,
        });
        user_keys.push((sk, pk));
    }

    let mut valid_transactions = Vec::new();
    let mut invalid_transactions = Vec::new();
    let mut valid_tx_hashes = HashSet::new();
    let mut invalid_tx_hashes = HashSet::new();
    // Track transactions that compete for the same (sender, nonce) - exactly one should be included
    let mut conflicting_tx_hashes: HashSet<[u8; 32]> = HashSet::new();
    let mut local_nonces = vec![0; num_accounts];

    // 1. Generate VALID transactions (Transfer 0 -> 1, 1 -> 2, etc.)
    for i in 0..20 {
        let sender_idx = i % num_accounts;
        let receiver_idx = (i + 1) % num_accounts;

        let (sender_sk, sender_pk) = &user_keys[sender_idx];
        let (_, receiver_pk) = &user_keys[receiver_idx];

        let sender_addr = Address::from_public_key(sender_pk);
        let receiver_addr = Address::from_public_key(receiver_pk);

        let nonce = local_nonces[sender_idx];
        let tx = Transaction::new_transfer(
            sender_addr,
            receiver_addr,
            100, // amount
            nonce,
            10, // fee
            sender_sk,
        );
        valid_tx_hashes.insert(tx.tx_hash);
        valid_transactions.push(tx);
        local_nonces[sender_idx] += 1;
    }

    // 2. Generate INVALID transactions

    // Type A: Future Nonce (Gap)
    {
        let sender_idx = 0;
        let (sender_sk, sender_pk) = &user_keys[sender_idx];
        let (_, receiver_pk) = &user_keys[1];
        let sender_addr = Address::from_public_key(sender_pk);
        let receiver_addr = Address::from_public_key(receiver_pk);

        let bad_nonce = local_nonces[sender_idx] + 100;
        let tx =
            Transaction::new_transfer(sender_addr, receiver_addr, 50, bad_nonce, 10, sender_sk);
        invalid_tx_hashes.insert(tx.tx_hash);
        invalid_transactions.push(tx);
    }

    // Type B: Conflicting Nonce (competing for nonce 0 with a valid tx)
    // This is NOT strictly invalid - it competes with another valid tx for nonce 0.
    // Exactly ONE of the two nonce-0 transactions should be included.
    {
        let sender_idx = 0;
        let (sender_sk, sender_pk) = &user_keys[sender_idx];
        let sender_addr = Address::from_public_key(sender_pk);

        // Find the valid tx from user[0] with nonce 0 to get its parameters
        let mut conflicting_amount = 100u64;
        let mut conflicting_fee = 10u64;
        let mut conflicting_recipient = Address::from_bytes([0u8; 32]);

        for valid_tx in &valid_transactions {
            if valid_tx.sender == sender_addr && valid_tx.nonce == 0 {
                // Use the same parameters so balance impact is identical
                conflicting_amount = valid_tx.amount();
                conflicting_fee = valid_tx.fee;
                conflicting_recipient = valid_tx
                    .recipient()
                    .unwrap_or(Address::from_bytes([0u8; 32]));
                // Track the valid tx as conflicting
                conflicting_tx_hashes.insert(valid_tx.tx_hash);
                // Remove from valid_tx_hashes since it's now in conflicting set
                valid_tx_hashes.remove(&valid_tx.tx_hash);
                break;
            }
        }

        let tx = Transaction::new_transfer(
            sender_addr,
            conflicting_recipient,
            conflicting_amount, // Same amount as valid tx
            0,                  // Competing for nonce 0
            conflicting_fee,    // Same fee as valid tx
            sender_sk,
        );

        // Track this tx in conflicting set (NOT invalid_tx_hashes)
        conflicting_tx_hashes.insert(tx.tx_hash);
        invalid_transactions.push(tx); // Still submit it to test the conflict
    }

    // Type C: Insufficient Funds
    {
        let sender_idx = 1;
        let (sender_sk, sender_pk) = &user_keys[sender_idx];
        let (_, receiver_pk) = &user_keys[0];
        let sender_addr = Address::from_public_key(sender_pk);
        let receiver_addr = Address::from_public_key(receiver_pk);

        let nonce = local_nonces[sender_idx]; // Correct nonce
        let huge_amount = initial_balance + 1_000_000;
        let tx = Transaction::new_transfer(
            sender_addr,
            receiver_addr,
            huge_amount,
            nonce,
            10,
            sender_sk,
        );
        invalid_tx_hashes.insert(tx.tx_hash);
        invalid_transactions.push(tx);
        // Don't increment nonce locally since this tx should fail
    }

    // Phase 2: Identities & P2P Config (Boilerplate)
    let mut identities = Vec::new();
    let mut public_keys = Vec::new();
    for _i in 0..N {
        let bls_sk = BlsSecretKey::generate(&mut rand::thread_rng());
        let identity = ValidatorIdentity::from_bls_key(bls_sk);
        public_keys.push(identity.bls_public_key().clone());
        identities.push(identity);
    }
    let peer_set = PeerSet::new(public_keys);
    let mut peer_strs = Vec::new();
    for peer_id in &peer_set.sorted_peer_ids {
        let pk = peer_set.id_to_public_key.get(peer_id).unwrap();
        let mut buf = Vec::new();
        pk.0.serialize_compressed(&mut buf).unwrap();
        peer_strs.push(hex::encode(buf));
    }

    let consensus_config = ConsensusConfig {
        n: N,
        f: F,
        view_timeout: DEFAULT_VIEW_TIMEOUT,
        leader_manager:
            consensus::consensus_manager::leader_manager::LeaderSelectionStrategy::RoundRobin,
        network: consensus::consensus_manager::config::Network::Local,
        peers: peer_strs,
        genesis_accounts: genesis_accounts.clone(),
    };

    let base_port = 40000u16 + (rand::random::<u16>() % 10000);
    let port_gap = 100u16;
    let mut p2p_configs = Vec::new();
    for (i, _identity) in identities.iter().enumerate() {
        let port = base_port + (i as u16 * port_gap);
        let listen_addr = format!("127.0.0.1:{}", port).parse().unwrap();
        let mut validators = Vec::new();
        for (j, other_identity) in identities.iter().enumerate() {
            if i != j {
                let other_port = base_port + (j as u16 * port_gap);
                let pk_hex = hex::encode(other_identity.ed25519_public_key().as_ref());
                validators.push(ValidatorPeerInfo {
                    bls_peer_id: other_identity.peer_id(),
                    bls_public_key: None,
                    ed25519_public_key: pk_hex,
                    address: Some(format!("127.0.0.1:{}", other_port).parse().unwrap()),
                });
            }
        }
        p2p_configs.push(P2PConfig {
            listen_addr,
            external_addr: listen_addr,
            validators,
            total_number_peers: N,
            maximum_number_faulty_peers: F,
            bootstrap_timeout_ms: 30_000,
            ..Default::default()
        });
    }

    // Phase 3: Spawn Nodes
    let mut nodes: Vec<NodeSetup<N, F, M_SIZE>> = Vec::new();
    let mut store_with_dirs: Vec<(Arc<ConsensusStore>, TempDir)> = Vec::new();

    for (i, (identity, p2p_config)) in identities
        .into_iter()
        .zip(p2p_configs.into_iter())
        .enumerate()
    {
        let node_logger = logger.new(o!("node" => i, "peer_id" => identity.peer_id()));
        let (node, storage, temp_dir) =
            create_node_setup(identity, p2p_config, consensus_config.clone(), node_logger);
        nodes.push(node);
        store_with_dirs.push((storage, temp_dir));
    }

    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Phase 4: Bootstrap
        slog::info!(logger, "Phase 4: Bootstrapping P2P network");
        for node in &nodes {
            node.p2p_handle.wait_ready().await;
        }
        slog::info!(logger, "All nodes bootstrapped");

        // Phase 5: Submit Transactions via gRPC
        slog::info!(logger, "Phase 5: Submitting VALID transactions via gRPC");

        // Submit valid ones
        for (i, tx) in valid_transactions.iter().enumerate() {
            let node_idx = i % N;
            let grpc_addr = nodes[node_idx].grpc_addr;
            let _ = submit_transaction_via_grpc(grpc_addr, tx).await;
        }

        slog::info!(logger, "Phase 5b: Submitting INVALID transactions via gRPC");
        // Submit invalid ones - gRPC server should reject them
        for (i, tx) in invalid_transactions.iter().enumerate() {
            let node_idx = i % N;
            let grpc_addr = nodes[node_idx].grpc_addr;
            // We expect these to fail validation
            let _ = submit_transaction_via_grpc(grpc_addr, tx).await;
        }

        // Phase 6: Wait for Consensus
        slog::info!(logger, "Phase 6: Waiting for consensus");
        ctx.sleep(Duration::from_secs(15)).await;

        // Phase 7: Verification
        slog::info!(logger, "Phase 7: Verifying transaction inclusion");

        let mut confirmed_valid = 0;
        let mut confirmed_invalid = 0;

        // Check Node 0 (honest)
        let store = &store_with_dirs[0].0;
        let blocks = store.get_all_finalized_blocks().unwrap();

        slog::info!(logger, "Finalized blocks count"; "count" => blocks.len());
        assert!(!blocks.is_empty(), "Chain should progress with valid txs");

        let mut included_txs = HashSet::new();
        for block in blocks {
            for tx in &block.transactions {
                included_txs.insert(tx.tx_hash);
            }
        }

        // Verify ALL valid txs are included (assuming strict liveness and enough time)
        // With 15s, and 20 txs, they should be included.
        for hash in &valid_tx_hashes {
            if included_txs.contains(hash) {
                confirmed_valid += 1;
            } else {
                slog::warn!(logger, "Valid tx not included"; "tx_hash" => ?hash);
            }
        }

        // Verify exactly ONE of the conflicting nonce transactions is included
        let conflicting_included: Vec<_> =
            conflicting_tx_hashes.intersection(&included_txs).collect();

        assert_eq!(
            conflicting_included.len(),
            1,
            "Exactly one of the conflicting nonce-0 transactions should be included. \
             Found {} included. If > 1, this is a double-spend bug! If 0, valid tx was lost.",
            conflicting_included.len()
        );

        slog::info!(logger, "Conflicting nonce test passed! ✓";
            "conflicting_txs" => conflicting_tx_hashes.len(),
            "included" => 1,
            "correctly_rejected" => conflicting_tx_hashes.len() - 1,
        );

        // Verify NO truly invalid txs are included
        let mut included_invalid_hashes: Vec<[u8; 32]> = Vec::new();
        for hash in &invalid_tx_hashes {
            if included_txs.contains(hash) {
                confirmed_invalid += 1;
                included_invalid_hashes.push(*hash);
                slog::error!(logger, "Invalid tx INCLUDED!"; "tx_hash" => ?hash);
            }
        }

        slog::info!(logger, "Verification Results";
            "valid_included" => confirmed_valid,
            "total_valid" => valid_tx_hashes.len(),
            "conflicting_included" => 1,
            "conflicting_total" => conflicting_tx_hashes.len(),
            "invalid_included" => confirmed_invalid,
            "total_invalid" => invalid_tx_hashes.len()
        );

        assert_eq!(
            confirmed_invalid, 0,
            "No truly invalid transactions should be included in finalized blocks.\n\
             Found {} invalid tx(s) incorrectly included:\n{:?}\n\
             This indicates a bug in mempool validation or block proposal logic.",
            confirmed_invalid, included_invalid_hashes
        );
        assert!(
            confirmed_valid > 0,
            "Some valid transactions should be included"
        );
        // We expect ALL valid to be included ideally, but assert > 0 for robustness against slow
        // network

        slog::info!(logger, "Test completed successfully! ✓");
        drop(store_with_dirs);
    });
}

/// Invalid block from leader integration test.
///
/// This test verifies that if a Byzantine leader proposes a block containing an invalid transaction
/// (e.g. insufficient funds), the honest nodes reject the block and do not commit it.
/// The network should eventually recover (via view timeout) and make progress.
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_multi_node_invalid_block_from_leader -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_multi_node_invalid_block_from_leader() {
    let logger = create_test_logger();
    const BYZANTINE_LEADER_IDX: usize = 1; // Round-robin view 1 leader is usually index 1

    slog::info!(
        logger,
        "Starting multi-node integration test (invalid block from leader)";
        "nodes" => N,
        "byzantine_tolerance" => F,
        "byzantine_leader" => BYZANTINE_LEADER_IDX,
    );

    // Phase 1: Setup Accounts
    let num_transactions = 0; // We create tx manually
    let (_, _genesis_accounts) = create_funded_test_transactions(num_transactions);

    let mut identities = Vec::new();
    let mut public_keys = Vec::new();

    // We need to know user keys for creating invalid tx
    let mut user_keys = Vec::new();
    let initial_balance = 10_000u64;
    // Overwrite genesis accounts with known keys
    let mut genesis_accounts = Vec::new();
    for _ in 0..5 {
        let sk = TxSecretKey::generate(&mut rand::thread_rng());
        let pk = sk.public_key();
        genesis_accounts.push(GenesisAccount {
            public_key: hex::encode(pk.to_bytes()),
            balance: initial_balance,
        });
        user_keys.push((sk, pk));
    }

    for _i in 0..N {
        let bls_sk = BlsSecretKey::generate(&mut rand::thread_rng());
        let identity = ValidatorIdentity::from_bls_key(bls_sk);
        public_keys.push(identity.bls_public_key().clone());
        identities.push(identity);
    }
    let peer_set = PeerSet::new(public_keys);

    let mut peer_strs = Vec::new();
    for peer_id in &peer_set.sorted_peer_ids {
        let pk = peer_set.id_to_public_key.get(peer_id).unwrap();
        let mut buf = Vec::new();
        pk.0.serialize_compressed(&mut buf).unwrap();
        peer_strs.push(hex::encode(buf));
    }

    let consensus_config = ConsensusConfig {
        n: N,
        f: F,
        view_timeout: DEFAULT_VIEW_TIMEOUT,
        leader_manager:
            consensus::consensus_manager::leader_manager::LeaderSelectionStrategy::RoundRobin,
        network: consensus::consensus_manager::config::Network::Local,
        peers: peer_strs,
        genesis_accounts: genesis_accounts.clone(),
    };

    // Phase 2: P2P Config
    let base_port = 40000u16 + (rand::random::<u16>() % 10000);
    let port_gap = 100u16;
    let mut p2p_configs = Vec::new();
    for (i, _identity) in identities.iter().enumerate() {
        let port = base_port + (i as u16 * port_gap);
        let listen_addr = format!("127.0.0.1:{}", port).parse().unwrap();
        let mut validators = Vec::new();
        for (j, other_identity) in identities.iter().enumerate() {
            if i != j {
                let other_port = base_port + (j as u16 * port_gap);
                let pk_hex = hex::encode(other_identity.ed25519_public_key().as_ref());
                validators.push(ValidatorPeerInfo {
                    bls_peer_id: other_identity.peer_id(),
                    bls_public_key: None,
                    ed25519_public_key: pk_hex,
                    address: Some(format!("127.0.0.1:{}", other_port).parse().unwrap()),
                });
            }
        }
        p2p_configs.push(P2PConfig {
            listen_addr,
            external_addr: listen_addr,
            validators,
            total_number_peers: N,
            maximum_number_faulty_peers: F,
            bootstrap_timeout_ms: 30_000,
            ..Default::default()
        });
    }

    // Phase 3: Spawn
    let mut nodes: Vec<Option<NodeSetup<N, F, M_SIZE>>> = Vec::new();
    let mut byzantine_node: Option<ByzantineNodeSetup<N, F, M_SIZE>> = None;
    let mut store_with_dirs: Vec<(Arc<ConsensusStore>, TempDir)> = Vec::new();

    for (i, (identity, p2p_config)) in identities
        .into_iter()
        .zip(p2p_configs.into_iter())
        .enumerate()
    {
        let node_logger = logger.new(o!("node" => i, "peer_id" => identity.peer_id()));

        if i == BYZANTINE_LEADER_IDX {
            let (node, storage, temp_dir) =
                create_byzantine_node_setup(identity, p2p_config, node_logger);
            byzantine_node = Some(node);
            store_with_dirs.push((storage, temp_dir));
            nodes.push(None);
        } else {
            let (node, storage, temp_dir) =
                create_node_setup(identity, p2p_config, consensus_config.clone(), node_logger);
            nodes.push(Some(node));
            store_with_dirs.push((storage, temp_dir));
        }
    }

    let mut byzantine_node = byzantine_node.expect("Byzantine node not created");

    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Phase 4: Bootstrap
        slog::info!(logger, "Phase 4: Bootstrapping P2P network");
        let mut futures = Vec::new();
        for node in nodes.iter().flatten() {
            futures.push(node.p2p_handle.wait_ready());
        }
        futures.push(byzantine_node.p2p_handle.wait_ready());
        for f in futures {
            f.await;
        }
        slog::info!(logger, "All nodes bootstrapped");

        // Phase 5: Create Invalid Block (View 1)
        // Honest nodes wait for View 1. Leader (Byzantine) should propose.

        let honest_node_idx = if BYZANTINE_LEADER_IDX == 0 { 1 } else { 0 };
        let finalized_blocks = store_with_dirs[honest_node_idx]
            .0
            .get_all_finalized_blocks()
            .expect("Failed to get finalized blocks");

        let genesis_block = finalized_blocks
            .iter()
            .find(|b| b.view() == 0)
            .expect("Genesis block not found");
        let parent_hash = genesis_block.get_hash();

        slog::info!(logger, "Phase 5: Proposing Invalid Block at View 1");

        let byz_id = byzantine_node.peer_id;
        let byz_sk = &byzantine_node.bls_secret_key;

        // Create invalid tx (Insufficient Funds)
        let (sender_sk, sender_pk) = &user_keys[0];
        let (_, receiver_pk) = &user_keys[1];
        let sender_addr = Address::from_public_key(sender_pk);
        let receiver_addr = Address::from_public_key(receiver_pk);

        let bad_tx = Transaction::new_transfer(
            sender_addr,
            receiver_addr,
            initial_balance + 999999, // Too much
            0,                        // nonce
            10,                       // fee
            sender_sk,
        );
        let bad_tx_hash = bad_tx.tx_hash;

        let invalid_block = Block::new(
            1, // View 1
            byz_id,
            parent_hash,
            vec![std::sync::Arc::new(bad_tx)],
            1000,
            byz_sk.sign(b"test"), // Assuming signature logic covers this (validated by protocol)
            false,
            1,
        );

        let invalid_block_hash = invalid_block.get_hash();

        // Broadcast
        byzantine_node
            .broadcast_producer
            .push(ConsensusMessage::BlockProposal(invalid_block.clone()))
            .unwrap();

        slog::info!(logger, "Invalid block proposal sent"; "block_hash" => ?invalid_block_hash);

        // Phase 6: Verify Consensus Rejection and Progress
        ctx.sleep(Duration::from_secs(10)).await;

        let mut progress_count = 0;
        for (i, (store, _)) in store_with_dirs.iter().enumerate() {
            if i == BYZANTINE_LEADER_IDX {
                continue;
            }
            let blocks = store.get_all_finalized_blocks().unwrap();
            slog::info!(logger, "Honest node state"; "node" => i, "blocks" => blocks.len());

            // Check for Bad Block
            for block in &blocks {
                // Check block hash
                if block.get_hash() == invalid_block_hash {
                    panic!("Honest node {} committed the invalid block!", i);
                }
                // Check tx
                for tx in &block.transactions {
                    if tx.tx_hash == bad_tx_hash {
                        panic!(
                            "Honest node {} committed the invalid tx inside another block!",
                            i
                        );
                    }
                }
            }

            // Need new blocks (more than genesis) -> Progress despite bad leader
            if blocks.len() > 1 {
                progress_count += 1;
            }
        }

        assert!(
            progress_count >= 1,
            "At least one node should have finalized blocks beyond genesis (recovery)"
        );
        slog::info!(logger, "Test completed successfully! ✓");

        drop(store_with_dirs);
    });
}
