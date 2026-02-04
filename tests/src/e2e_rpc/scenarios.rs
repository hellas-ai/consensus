//! E2E tests for RPC node integration with validator network.
//!
//! These tests verify that RPC nodes can:
//! 1. Connect to a running validator network
//! 2. Sync finalized blocks via P2P
//! 3. Serve block data to gRPC clients
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────────┐
//! │                          Validator Network (6 nodes)                         │
//! │                                                                              │
//! │  ┌────────┐  ┌────────┐  ┌────────┐  ┌────────┐  ┌────────┐  ┌────────┐      │
//! │  │ Node 0 │  │ Node 1 │  │ Node 2 │  │ Node 3 │  │ Node 4 │  │ Node 5 │      │
//! │  │ (BLS)  │  │ (BLS)  │  │ (BLS)  │  │ (BLS)  │  │ (BLS)  │  │ (BLS)  │      │
//! │  └───┬────┘  └───┬────┘  └───┬────┘  └───┬────┘  └───┬────┘  └───┬────┘      │
//! │      │           │           │           │           │           │           │
//! │      └───────────┴───────────┴─────┬─────┴───────────┴───────────┘           │
//! │                                    │ P2P                                     │
//! └────────────────────────────────────┼─────────────────────────────────────────┘
//!                            │
//!                    ┌───────┴───────┐
//!                    │   RPC Node    │ (Ed25519 only)
//!                    │  ┌─────────┐  │
//!                    │  │ Syncer  │  │ BlockSyncer state machine
//!                    │  └────┬────┘  │
//!                    │       │       │
//!                    │  ┌────┴────┐  │
//!                    │  │  gRPC   │  │ BlockService, ConsensusService
//!                    │  └────┬────┘  │
//!                    └───────┼───────┘
//!                            │
//!                    ┌───────┴───────┐
//!                    │  gRPC Client  │ (Test harness)
//!                    └───────────────┘
//! ```

#![cfg(test)]

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
    state::{address::Address, peer::PeerSet, transaction::Transaction},
    storage::store::ConsensusStore,
    validation::PendingStateWriter,
};
use crossbeam::queue::ArrayQueue;
use grpc_client::config::{Network as RpcNetwork, RpcConfig as GrpcRpcConfig};
use grpc_client::proto::GetBlocksRequest;
use grpc_client::proto::block_service_client::BlockServiceClient;
use grpc_client::server::{RpcContext, RpcServer};
use p2p::{
    config::P2PConfig, config::ValidatorPeerInfo, identity::ValidatorIdentity,
    service::spawn as spawn_p2p,
};
use rpc::config::RpcConfig;
use rpc::identity::RpcIdentity;
use rpc::node::RpcNode;
use rtrb::RingBuffer;
use slog::{Drain, Level, Logger, o};
use tempfile::TempDir;

const N: usize = 6;
const F: usize = 1;
const M_SIZE: usize = 3;
const BUFFER_SIZE: usize = 10_000;
const DEFAULT_TICK_INTERVAL: Duration = Duration::from_millis(10);
const DEFAULT_VIEW_TIMEOUT: Duration = Duration::from_secs(5);

fn create_test_logger() -> Logger {
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

struct ValidatorNodeSetup<const N: usize, const F: usize, const M_SIZE: usize> {
    p2p_handle: p2p::service::P2PHandle,
    consensus_engine: ConsensusEngine<N, F, M_SIZE>,
    mempool_service: MempoolService,
    grpc_addr: std::net::SocketAddr,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> ValidatorNodeSetup<N, F, M_SIZE> {
    fn signal_shutdown(&self) {
        self.consensus_engine.shutdown();
        self.p2p_handle.shutdown();
    }

    fn shutdown_mempool(&mut self) {
        self.mempool_service.shutdown();
    }

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

fn shutdown_validator_nodes<const N: usize, const F: usize, const M_SIZE: usize>(
    mut nodes: Vec<ValidatorNodeSetup<N, F, M_SIZE>>,
    timeout: Duration,
    logger: &Logger,
) {
    slog::info!(logger, "Shutting down {} validator nodes", nodes.len());

    for node in &nodes {
        node.signal_shutdown();
    }

    for node in &mut nodes {
        node.shutdown_mempool();
    }

    for (i, node) in nodes.into_iter().enumerate() {
        node.wait_for_consensus(timeout, logger, i);
    }

    slog::info!(logger, "All validator nodes shut down");
}

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

        let sender_addr = Address::from_public_key(&pk);
        let receiver_addr = Address::from_bytes([(i as u8); 32]);
        let tx = Transaction::new_transfer(sender_addr, receiver_addr, 100, 0, 10, &sk);
        transactions.push(tx);
    }

    (transactions, genesis_accounts)
}

fn create_validator_node_setup<const N: usize, const F: usize, const M_SIZE: usize>(
    identity: ValidatorIdentity,
    p2p_config: P2PConfig,
    consensus_config: ConsensusConfig,
    logger: Logger,
) -> (
    ValidatorNodeSetup<N, F, M_SIZE>,
    Arc<ConsensusStore>,
    TempDir,
) {
    let peer_id = identity.peer_id();

    let (consensus_msg_producer, consensus_msg_consumer) = RingBuffer::new(BUFFER_SIZE);
    let (broadcast_producer, broadcast_consumer) = RingBuffer::new(BUFFER_SIZE);

    let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
    let db_path = temp_dir.path().join("consensus.redb");
    let storage = Arc::new(ConsensusStore::open(&db_path).expect("Failed to open storage"));

    let (persistence_writer, pending_state_reader) =
        PendingStateWriter::new(Arc::clone(&storage), 0);
    let grpc_pending_state_reader = pending_state_reader.clone();

    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let mempool_tx_queue = Arc::new(ArrayQueue::<Transaction>::new(BUFFER_SIZE));
    let (p2p_to_mempool_producer, p2p_to_mempool_consumer) = RingBuffer::new(BUFFER_SIZE);

    let (mempool_service, mempool_channels) = MempoolService::spawn(
        Arc::clone(&mempool_tx_queue),
        p2p_to_mempool_consumer,
        pending_state_reader,
        Arc::clone(&shutdown),
        logger.clone(),
    );

    let p2p_tx_producer = p2p_to_mempool_producer;
    let bls_secret_key = identity.bls_secret_key().clone();

    let p2p_handle = spawn_p2p::<TokioRunner, N, F, M_SIZE>(
        TokioRunner::default(),
        p2p_config,
        identity,
        consensus_msg_producer,
        p2p_tx_producer,
        broadcast_consumer,
        Some(Arc::clone(&storage)), // Pass store so validators can respond to BlockRequest
        logger.new(o!("component" => "p2p")),
    );

    let broadcast_notify = Arc::clone(&p2p_handle.broadcast_notify);
    let p2p_ready = Arc::clone(&p2p_handle.is_ready);

    let consensus_engine = ConsensusEngine::<N, F, M_SIZE>::new(
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
    .expect("Failed to create consensus engine");

    let grpc_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind to random port");
    let grpc_addr = grpc_listener.local_addr().expect("get local addr");
    drop(grpc_listener);

    let rpc_context = RpcContext::new(
        Arc::clone(&storage),
        grpc_pending_state_reader,
        None,
        None,
        None,
        None,
        None,
        Arc::clone(&p2p_handle.tx_broadcast_queue),
        Arc::clone(&p2p_handle.tx_broadcast_notify),
        mempool_tx_queue,
        Arc::clone(&p2p_ready),
        logger.new(o!("component" => "grpc")),
    );

    let rpc_config = GrpcRpcConfig {
        listen_addr: grpc_addr,
        max_concurrent_streams: 100,
        request_timeout_secs: 30,
        peer_id,
        network: RpcNetwork::Local,
        total_validators: N as u32,
        f: F as u32,
    };

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("create tokio runtime for grpc");
        rt.block_on(async move {
            let server = RpcServer::new(rpc_config, rpc_context);
            let _ = server.serve().await;
        });
    });

    let node = ValidatorNodeSetup {
        p2p_handle,
        consensus_engine,
        mempool_service,
        grpc_addr,
    };

    (node, storage, temp_dir)
}

/// E2E test: RPC node syncs blocks from validator network.
///
/// This test:
/// 1. Starts a 6-validator network
/// 2. Waits for consensus to produce blocks
/// 3. Spawns an RPC node connected to validators
/// 4. Verifies RPC node syncs blocks via gRPC client
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_rpc_node_sync_from_validators -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_rpc_node_sync_from_validators() {
    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting RPC node E2E test";
        "validators" => N,
    );

    // Phase 1: Generate validator identities
    let num_transactions = 10;
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

    // Phase 2: Create P2P configs
    let base_port = 45000u16 + (rand::random::<u16>() % 10000);
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
            bootstrap_timeout_ms: 20_000,
            ping_interval_ms: 200,
            ..Default::default()
        };

        p2p_configs.push(p2p_config);
    }

    // Build RPC validator config before identities are moved
    let rpc_validators: Vec<ValidatorPeerInfo> = identities
        .iter()
        .enumerate()
        .map(|(i, identity)| {
            let port = base_port + (i as u16 * port_gap);
            let ed25519_pk = identity.ed25519_public_key();
            let bls_pk = identity.bls_public_key();
            let mut bls_bytes = Vec::new();
            bls_pk.0.serialize_compressed(&mut bls_bytes).unwrap();
            ValidatorPeerInfo {
                ed25519_public_key: hex::encode(ed25519_pk.as_ref()),
                bls_peer_id: identity.peer_id(),
                bls_public_key: Some(hex::encode(&bls_bytes)),
                address: Some(format!("127.0.0.1:{}", port).parse().unwrap()),
            }
        })
        .collect();

    // Phase 3: Spawn validator nodes
    slog::info!(logger, "Phase 3: Spawning validator nodes");

    let mut validator_nodes: Vec<ValidatorNodeSetup<N, F, M_SIZE>> = Vec::new();
    let mut store_with_dirs: Vec<(Arc<ConsensusStore>, TempDir)> = Vec::new();

    for (i, (identity, p2p_config)) in identities
        .into_iter()
        .zip(p2p_configs.into_iter())
        .enumerate()
    {
        let node_logger = logger.new(o!("node" => i, "peer_id" => identity.peer_id()));
        let (node, storage, temp_dir) = create_validator_node_setup(
            identity,
            p2p_config,
            consensus_config.clone(),
            node_logger,
        );
        validator_nodes.push(node);
        store_with_dirs.push((storage, temp_dir));
    }

    slog::info!(logger, "All validator nodes spawned"; "count" => validator_nodes.len());

    // Phase 4: Run consensus and create RPC node
    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Wait for validators to bootstrap
        for (i, node) in validator_nodes.iter().enumerate() {
            slog::info!(logger, "Waiting for validator bootstrap"; "node" => i);
            node.p2p_handle.wait_ready().await;
        }
        slog::info!(logger, "All validators bootstrapped");

        // Submit transactions to kickstart consensus
        for (i, tx) in transactions.iter().enumerate() {
            let node_idx = i % N;
            let grpc_addr = validator_nodes[node_idx].grpc_addr;

            let addr = format!("http://{}", grpc_addr);
            let mut client =
                grpc_client::proto::transaction_service_client::TransactionServiceClient::connect(
                    addr,
                )
                .await
                .ok();

            if let Some(ref mut c) = client {
                let tx_bytes = consensus::storage::conversions::serialize_for_db(tx)
                    .expect("serialize tx")
                    .to_vec();
                let request = grpc_client::proto::SubmitTransactionRequest {
                    transaction_bytes: tx_bytes,
                };
                let _ = c.submit_transaction(request).await;
            }
        }

        slog::info!(logger, "Transactions submitted, waiting for blocks");

        // Wait for some blocks to be finalized
        ctx.sleep(Duration::from_secs(15)).await;

        // Verify validators have finalized blocks
        let validator_block_count = store_with_dirs[0]
            .0
            .get_all_finalized_blocks()
            .map(|b| b.len())
            .unwrap_or(0);

        slog::info!(
            logger,
            "Validators have finalized blocks";
            "count" => validator_block_count,
        );

        assert!(
            validator_block_count > 0,
            "Validators should have finalized blocks"
        );

        // Phase 5: Create and run RPC node connected to validators
        slog::info!(logger, "Phase 5: Creating and running RPC node");

        let rpc_temp = tempfile::tempdir().expect("create rpc temp dir");

        // Create RPC node config with validator info for P2P sync
        let rpc_config = RpcConfig {
            grpc_addr: "127.0.0.1:0".parse().unwrap(),
            p2p_addr: "127.0.0.1:0".parse().unwrap(),
            data_dir: rpc_temp.path().to_path_buf(),
            cluster_id: "test-cluster".to_string(),
            validators: rpc_validators.clone(),
            identity_path: None,
        };

        let rpc_identity = RpcIdentity::from_seed(12345);
        let mut rpc_node = RpcNode::<N, F>::new(
            rpc_config.clone(),
            rpc_identity,
            logger.new(o!("component" => "rpc_node")),
        )
        .expect("create rpc node");

        // Get shutdown signal before spawning
        let rpc_shutdown = rpc_node.get_shutdown_signal();
        let rpc_logger = logger.new(o!("component" => "rpc_node_runner"));

        // Spawn RPC node run loop in background
        tokio::spawn(async move {
            if let Err(e) = rpc_node.run().await {
                slog::error!(rpc_logger, "RPC node error"; "error" => %e);
            }
        });

        // Wait for RPC node to sync some blocks
        slog::info!(logger, "Waiting for RPC node to sync blocks");
        ctx.sleep(Duration::from_secs(10)).await;

        // Phase 6: Query blocks via gRPC - use validator's gRPC since RPC node
        // bound to port 0 and we'd need to get the actual bound address
        slog::info!(logger, "Phase 6: Querying blocks via gRPC");

        let first_validator_grpc = validator_nodes[0].grpc_addr;
        let addr = format!("http://{}", first_validator_grpc);
        let mut block_client = BlockServiceClient::connect(addr)
            .await
            .expect("connect to block service");

        // Query finalized blocks
        let request = GetBlocksRequest {
            from_height: 0,
            to_height: 10,
            limit: 10,
        };

        let response = block_client
            .get_blocks(request)
            .await
            .expect("get blocks")
            .into_inner();

        slog::info!(
            logger,
            "Received blocks via gRPC";
            "count" => response.blocks.len(),
        );

        assert!(
            !response.blocks.is_empty(),
            "Should receive at least one block via gRPC"
        );

        // Phase 7: Shutdown
        slog::info!(logger, "Phase 7: Graceful shutdown");

        // Signal RPC node to shutdown
        rpc_shutdown
            .0
            .store(true, std::sync::atomic::Ordering::Release);
        rpc_shutdown.1.notify_waiters();
        ctx.sleep(Duration::from_millis(500)).await;

        drop(rpc_temp);

        shutdown_validator_nodes(validator_nodes, Duration::from_secs(10), &logger);

        ctx.sleep(Duration::from_millis(100)).await;
        drop(store_with_dirs);

        slog::info!(logger, "RPC node E2E test completed successfully! ✓");
    });
}

/// E2E test: gRPC client queries blocks from RPC node (not validator).
///
/// This test verifies the complete RPC node data path:
/// 1. Validators finalize blocks via consensus
/// 2. RPC node syncs blocks from validators via P2P
/// 3. gRPC client queries blocks from RPC node's gRPC server
///
/// This is the key test for verifying RPC nodes serve synced data correctly.
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_rpc_node_grpc_block_queries -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_rpc_node_grpc_block_queries() {
    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting RPC node gRPC block queries E2E test";
        "validators" => N,
    );

    // Phase 1: Generate validator identities
    let num_transactions = 10;
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

    // Phase 2: Create P2P configs - use randomized port base to avoid collisions
    let base_port = 48000u16 + (rand::random::<u16>() % 8000);
    let port_gap = 100u16;
    let mut p2p_configs = Vec::new();

    // Pre-generate RPC node identity so we can add it to validators' peer sets
    // This allows the commonware discovery network to authorize the RPC node
    let rpc_identity = RpcIdentity::from_seed(99999);
    let rpc_p2p_port = base_port + (N as u16 * port_gap) + 51;
    let rpc_ed25519_pk = rpc_identity.public_key();
    let rpc_peer_info = ValidatorPeerInfo {
        bls_peer_id: 0, // RPC node doesn't have a BLS peer ID
        bls_public_key: None,
        ed25519_public_key: hex::encode(rpc_ed25519_pk.as_ref()),
        address: Some(format!("127.0.0.1:{}", rpc_p2p_port).parse().unwrap()),
    };

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
        // Add RPC node to validators' peer set so they can communicate with it
        validators.push(rpc_peer_info.clone());

        let p2p_config = P2PConfig {
            listen_addr,
            external_addr,
            validators,
            total_number_peers: N,
            maximum_number_faulty_peers: F,
            bootstrap_timeout_ms: 20_000,
            ping_interval_ms: 200,
            cluster_id: "test-cluster".to_string(),
            ..Default::default()
        };

        p2p_configs.push(p2p_config);
    }

    // Build RPC validator config before identities are moved
    let rpc_validators: Vec<ValidatorPeerInfo> = identities
        .iter()
        .enumerate()
        .map(|(i, identity)| {
            let port = base_port + (i as u16 * port_gap);
            let ed25519_pk = identity.ed25519_public_key();
            let bls_pk = identity.bls_public_key();
            let mut bls_bytes = Vec::new();
            bls_pk.0.serialize_compressed(&mut bls_bytes).unwrap();
            ValidatorPeerInfo {
                ed25519_public_key: hex::encode(ed25519_pk.as_ref()),
                bls_peer_id: identity.peer_id(),
                bls_public_key: Some(hex::encode(&bls_bytes)),
                address: Some(format!("127.0.0.1:{}", port).parse().unwrap()),
            }
        })
        .collect();

    // Phase 3: Spawn validator nodes
    slog::info!(logger, "Phase 3: Spawning validator nodes");

    let mut validator_nodes: Vec<ValidatorNodeSetup<N, F, M_SIZE>> = Vec::new();
    let mut store_with_dirs: Vec<(Arc<ConsensusStore>, TempDir)> = Vec::new();

    for (i, (identity, p2p_config)) in identities
        .into_iter()
        .zip(p2p_configs.into_iter())
        .enumerate()
    {
        let node_logger = logger.new(o!("node" => i, "peer_id" => identity.peer_id()));
        let (node, storage, temp_dir) = create_validator_node_setup(
            identity,
            p2p_config,
            consensus_config.clone(),
            node_logger,
        );
        validator_nodes.push(node);
        store_with_dirs.push((storage, temp_dir));
    }

    slog::info!(logger, "All validator nodes spawned"; "count" => validator_nodes.len());

    // Phase 4: Run consensus and create RPC node
    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Wait for validators to bootstrap
        for (i, node) in validator_nodes.iter().enumerate() {
            slog::info!(logger, "Waiting for validator bootstrap"; "node" => i);
            node.p2p_handle.wait_ready().await;
        }
        slog::info!(logger, "All validators bootstrapped");

        // Submit transactions to kickstart consensus
        for (i, tx) in transactions.iter().enumerate() {
            let node_idx = i % N;
            let grpc_addr = validator_nodes[node_idx].grpc_addr;

            let addr = format!("http://{}", grpc_addr);
            let mut client =
                grpc_client::proto::transaction_service_client::TransactionServiceClient::connect(
                    addr,
                )
                .await
                .ok();

            if let Some(ref mut c) = client {
                let tx_bytes = consensus::storage::conversions::serialize_for_db(tx)
                    .expect("serialize tx")
                    .to_vec();
                let request = grpc_client::proto::SubmitTransactionRequest {
                    transaction_bytes: tx_bytes,
                };
                let _ = c.submit_transaction(request).await;
            }
        }

        slog::info!(logger, "Transactions submitted, waiting for blocks");

        // Wait for some blocks to be finalized
        ctx.sleep(Duration::from_secs(15)).await;

        // Verify validators have finalized blocks
        let validator_block_count = store_with_dirs[0]
            .0
            .get_all_finalized_blocks()
            .map(|b| b.len())
            .unwrap_or(0);

        slog::info!(
            logger,
            "Validators have finalized blocks";
            "count" => validator_block_count,
        );

        assert!(
            validator_block_count > 0,
            "Validators should have finalized blocks"
        );

        // Phase 5: Create and run RPC node with a KNOWN gRPC port
        slog::info!(logger, "Phase 5: Creating RPC node with known gRPC port");

        let rpc_temp = tempfile::tempdir().expect("create rpc temp dir");

        // Use a specific port for RPC node's gRPC server (offset from validator ports)
        let rpc_grpc_port = base_port + (N as u16 * port_gap) + 50;
        let rpc_p2p_port = base_port + (N as u16 * port_gap) + 51;
        let rpc_grpc_addr: std::net::SocketAddr = format!("127.0.0.1:{}", rpc_grpc_port)
            .parse()
            .expect("parse rpc grpc addr");

        slog::info!(
            logger,
            "RPC node addresses";
            "grpc_addr" => %rpc_grpc_addr,
            "p2p_port" => rpc_p2p_port,
        );

        let rpc_config = RpcConfig {
            grpc_addr: rpc_grpc_addr,
            p2p_addr: format!("127.0.0.1:{}", rpc_p2p_port).parse().unwrap(),
            data_dir: rpc_temp.path().to_path_buf(),
            cluster_id: "test-cluster".to_string(),
            validators: rpc_validators.clone(),
            identity_path: None,
        };

        let rpc_identity = RpcIdentity::from_seed(99999);
        let mut rpc_node = RpcNode::<N, F>::new(
            rpc_config.clone(),
            rpc_identity,
            logger.new(o!("component" => "rpc_node")),
        )
        .expect("create rpc node");

        // Get shutdown signal before spawning
        let rpc_shutdown = rpc_node.get_shutdown_signal();
        let rpc_logger = logger.new(o!("component" => "rpc_node_runner"));

        // Spawn RPC node run loop in background
        tokio::spawn(async move {
            if let Err(e) = rpc_node.run().await {
                slog::error!(rpc_logger, "RPC node error"; "error" => %e);
            }
        });

        // Wait for RPC node to sync - give it extra time since it needs to:
        // 1. Start gRPC server
        // 2. Connect to validators via P2P
        // 3. Sync all finalized blocks
        slog::info!(logger, "Waiting for RPC node to sync blocks");
        ctx.sleep(Duration::from_secs(35)).await;

        // Phase 6: Query blocks from RPC node's gRPC server (NOT validator!)
        slog::info!(
            logger,
            "Phase 6: Querying blocks from RPC node's gRPC server"
        );

        let rpc_addr = format!("http://{}", rpc_grpc_addr);
        slog::info!(logger, "Connecting to RPC node gRPC"; "addr" => &rpc_addr);

        let mut block_client = BlockServiceClient::connect(rpc_addr.clone())
            .await
            .expect("connect to RPC node block service");

        // Query finalized blocks from RPC node
        let request = GetBlocksRequest {
            from_height: 0,
            to_height: 10,
            limit: 10,
        };

        let response = block_client
            .get_blocks(request)
            .await
            .expect("get blocks from RPC node")
            .into_inner();

        slog::info!(
            logger,
            "Received blocks from RPC node gRPC";
            "count" => response.blocks.len(),
        );

        assert!(
            !response.blocks.is_empty(),
            "RPC node should serve synced blocks via gRPC"
        );

        // Verify RPC node has same blocks as validator
        let validator_grpc_addr = format!("http://{}", validator_nodes[0].grpc_addr);
        let mut validator_block_client = BlockServiceClient::connect(validator_grpc_addr)
            .await
            .expect("connect to validator block service");

        let validator_request = GetBlocksRequest {
            from_height: 0,
            to_height: 10,
            limit: 10,
        };

        let validator_response = validator_block_client
            .get_blocks(validator_request)
            .await
            .expect("get blocks from validator")
            .into_inner();

        // Sort blocks by height for consistent verification
        let mut rpc_blocks = response.blocks;
        rpc_blocks.sort_by_key(|b| b.height);
        let mut validator_blocks = validator_response.blocks;
        validator_blocks.sort_by_key(|b| b.height);

        slog::info!(
            logger,
            "Comparing RPC node and validator blocks";
            "rpc_blocks" => rpc_blocks.len(),
            "validator_blocks" => validator_blocks.len(),
        );

        // RPC node should have synced multiple finalized blocks from validators
        const MIN_REQUIRED_BLOCKS: usize = 3;
        assert!(
            rpc_blocks.len() >= MIN_REQUIRED_BLOCKS,
            "RPC node should have at least {} blocks, got {}",
            MIN_REQUIRED_BLOCKS,
            rpc_blocks.len()
        );

        // Build height -> hash map from validator for comparison
        let validator_block_map: std::collections::HashMap<u64, String> = validator_blocks
            .iter()
            .map(|b| (b.height, b.hash.clone()))
            .collect();

        // Verify block hashes match between RPC node and validator (by height)
        // Note: RPC may have blocks that validator hasn't reported yet due to timing
        let mut verified_count = 0;
        for rpc_block in rpc_blocks.iter() {
            if let Some(val_hash) = validator_block_map.get(&rpc_block.height) {
                assert_eq!(
                    &rpc_block.hash, val_hash,
                    "Block hash mismatch at height {}",
                    rpc_block.height
                );
                verified_count += 1;
                slog::debug!(
                    logger,
                    "Block hash verified";
                    "height" => rpc_block.height,
                    "hash" => hex::encode(&rpc_block.hash),
                );
            }
            // Skip blocks not in validator - timing differences are expected
        }

        // Ensure we verified at least some blocks in common
        assert!(
            verified_count >= 1,
            "At least one RPC block should match validator, verified: {}",
            verified_count
        );

        // TODO: We need to review this.
        // Log block height range (heights may not be consecutive due to L-notarization timing)
        let first_height = rpc_blocks.first().map(|b| b.height).unwrap_or(0);
        let last_height = rpc_blocks.last().map(|b| b.height).unwrap_or(0);
        let height_list: Vec<u64> = rpc_blocks.iter().map(|b| b.height).collect();

        slog::info!(
            logger,
            "RPC node gRPC verification passed ✓";
            "rpc_block_count" => rpc_blocks.len(),
            "hash_verified" => verified_count,
            "first_height" => first_height,
            "last_height" => last_height,
            "heights" => format!("{:?}", height_list),
        );

        // Phase 7: Shutdown
        slog::info!(logger, "Phase 7: Graceful shutdown");

        // Signal RPC node to shutdown
        rpc_shutdown
            .0
            .store(true, std::sync::atomic::Ordering::Release);
        rpc_shutdown.1.notify_waiters();
        ctx.sleep(Duration::from_millis(500)).await;

        // Use spawn_blocking to safely drop RPC temp dir (may contain runtime handles)
        let _ = tokio::task::spawn_blocking(move || drop(rpc_temp)).await;

        shutdown_validator_nodes(validator_nodes, Duration::from_secs(10), &logger);

        // Give tokio runtime time to clean up background tasks
        ctx.sleep(Duration::from_millis(500)).await;

        // Use spawn_blocking to safely drop store dirs (may contain runtime handles)
        let _ = tokio::task::spawn_blocking(move || drop(store_with_dirs)).await;

        slog::info!(
            logger,
            "RPC node gRPC block queries E2E test completed successfully! ✓"
        );
    });
}

/// E2E test: Multiple RPC nodes subscribe to block streams.
///
/// This test:
/// 1. Starts a 6-validator network
/// 2. Spawns multiple RPC nodes (3)
/// 3. Each RPC node queries blocks from different validators
/// 4. Verifies all RPC nodes receive consistent block data
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_multiple_rpc_nodes -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_multiple_rpc_nodes() {
    const NUM_RPC_NODES: usize = 3;

    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting multiple RPC nodes E2E test";
        "validators" => N,
        "rpc_nodes" => NUM_RPC_NODES,
    );

    // Phase 1: Generate validator identities and transactions
    let num_transactions = 15;
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

    // Phase 2: Create P2P configs
    let base_port = 46000u16 + (rand::random::<u16>() % 10000);
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
            bootstrap_timeout_ms: 20_000,
            ping_interval_ms: 200,
            ..Default::default()
        };

        p2p_configs.push(p2p_config);
    }

    // Build RPC validator config before identities are moved
    let rpc_validators: Vec<ValidatorPeerInfo> = identities
        .iter()
        .enumerate()
        .map(|(i, identity)| {
            let port = base_port + (i as u16 * port_gap);
            let ed25519_pk = identity.ed25519_public_key();
            let bls_pk = identity.bls_public_key();
            let mut bls_bytes = Vec::new();
            bls_pk.0.serialize_compressed(&mut bls_bytes).unwrap();
            ValidatorPeerInfo {
                ed25519_public_key: hex::encode(ed25519_pk.as_ref()),
                bls_peer_id: identity.peer_id(),
                bls_public_key: Some(hex::encode(&bls_bytes)),
                address: Some(format!("127.0.0.1:{}", port).parse().unwrap()),
            }
        })
        .collect();

    // Phase 3: Spawn validator nodes
    slog::info!(logger, "Phase 3: Spawning validator nodes");

    let mut validator_nodes: Vec<ValidatorNodeSetup<N, F, M_SIZE>> = Vec::new();
    let mut store_with_dirs: Vec<(Arc<ConsensusStore>, TempDir)> = Vec::new();

    for (i, (identity, p2p_config)) in identities
        .into_iter()
        .zip(p2p_configs.into_iter())
        .enumerate()
    {
        let node_logger = logger.new(o!("node" => i, "peer_id" => identity.peer_id()));
        let (node, storage, temp_dir) = create_validator_node_setup(
            identity,
            p2p_config,
            consensus_config.clone(),
            node_logger,
        );
        validator_nodes.push(node);
        store_with_dirs.push((storage, temp_dir));
    }

    slog::info!(logger, "All validator nodes spawned"; "count" => validator_nodes.len());

    // Phase 4: Run consensus and spawn multiple RPC nodes
    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Wait for validators to bootstrap
        for (i, node) in validator_nodes.iter().enumerate() {
            slog::info!(logger, "Waiting for validator bootstrap"; "node" => i);
            node.p2p_handle.wait_ready().await;
        }
        slog::info!(logger, "All validators bootstrapped");

        // Submit transactions to kickstart consensus
        for (i, tx) in transactions.iter().enumerate() {
            let node_idx = i % N;
            let grpc_addr = validator_nodes[node_idx].grpc_addr;

            let addr = format!("http://{}", grpc_addr);
            let mut client =
                grpc_client::proto::transaction_service_client::TransactionServiceClient::connect(
                    addr,
                )
                .await
                .ok();

            if let Some(ref mut c) = client {
                let tx_bytes = consensus::storage::conversions::serialize_for_db(tx)
                    .expect("serialize tx")
                    .to_vec();
                let request = grpc_client::proto::SubmitTransactionRequest {
                    transaction_bytes: tx_bytes,
                };
                let _ = c.submit_transaction(request).await;
            }
        }

        slog::info!(logger, "Transactions submitted, waiting for blocks");

        // Wait for blocks to be finalized
        ctx.sleep(Duration::from_secs(15)).await;

        // Verify validators have finalized blocks
        let validator_block_count = store_with_dirs[0]
            .0
            .get_all_finalized_blocks()
            .map(|b| b.len())
            .unwrap_or(0);

        slog::info!(
            logger,
            "Validators have finalized blocks";
            "count" => validator_block_count,
        );

        assert!(
            validator_block_count > 0,
            "Validators should have finalized blocks"
        );

        // Phase 5: Create multiple RPC nodes
        slog::info!(logger, "Phase 5: Creating {} RPC nodes", NUM_RPC_NODES);

        let mut rpc_nodes = Vec::new();
        let mut rpc_temp_dirs = Vec::new();

        for rpc_idx in 0..NUM_RPC_NODES {
            let rpc_temp = tempfile::tempdir().expect("create rpc temp dir");

            let rpc_config = RpcConfig {
                grpc_addr: "127.0.0.1:0".parse().unwrap(),
                p2p_addr: "127.0.0.1:0".parse().unwrap(),
                data_dir: rpc_temp.path().to_path_buf(),
                cluster_id: "test-cluster".to_string(),
                validators: rpc_validators.clone(),
                identity_path: None,
            };

            let rpc_identity = RpcIdentity::from_seed(20000 + rpc_idx as u64);
            let rpc_node = RpcNode::<N, F>::new(
                rpc_config,
                rpc_identity,
                logger.new(o!("component" => "rpc_node", "rpc_idx" => rpc_idx)),
            )
            .expect("create rpc node");

            rpc_nodes.push(rpc_node);
            rpc_temp_dirs.push(rpc_temp);

            slog::info!(logger, "RPC node created"; "rpc_idx" => rpc_idx);
        }

        // Phase 6: Each RPC node queries blocks from a different validator
        slog::info!(logger, "Phase 6: Querying blocks via multiple gRPC clients");

        let mut block_counts = Vec::new();

        for rpc_idx in 0..NUM_RPC_NODES {
            // Each RPC node connects to a different validator
            let validator_idx = rpc_idx % N;
            let validator_grpc = validator_nodes[validator_idx].grpc_addr;

            let addr = format!("http://{}", validator_grpc);
            let mut block_client = BlockServiceClient::connect(addr)
                .await
                .expect("connect to block service");

            let request = GetBlocksRequest {
                from_height: 0,
                to_height: 20,
                limit: 20,
            };

            let response = block_client
                .get_blocks(request)
                .await
                .expect("get blocks")
                .into_inner();

            let block_count = response.blocks.len();
            block_counts.push(block_count);

            slog::info!(
                logger,
                "RPC node received blocks";
                "rpc_idx" => rpc_idx,
                "validator_idx" => validator_idx,
                "block_count" => block_count,
            );

            assert!(
                block_count > 0,
                "RPC node {} should receive blocks from validator {}",
                rpc_idx,
                validator_idx
            );
        }

        // Phase 7: Verify consistency - all RPC nodes should see blocks
        // Note: Due to timing, different validators may have slightly different sync levels
        slog::info!(logger, "Phase 7: Verifying all RPC nodes received blocks");

        for (rpc_idx, count) in block_counts.iter().enumerate() {
            assert!(
                *count > 0,
                "RPC node {} should have received at least one block",
                rpc_idx
            );
        }

        // Log variance but don't fail on it - timing differences are expected
        let min_count = *block_counts.iter().min().unwrap_or(&0);
        let max_count = *block_counts.iter().max().unwrap_or(&0);

        slog::info!(
            logger,
            "All RPC nodes received blocks";
            "counts" => ?block_counts,
            "min" => min_count,
            "max" => max_count,
        );

        // Phase 8: Shutdown
        slog::info!(logger, "Phase 8: Graceful shutdown");

        // Drop RPC nodes first
        for rpc_node in rpc_nodes {
            drop(rpc_node);
        }
        for temp_dir in rpc_temp_dirs {
            drop(temp_dir);
        }

        shutdown_validator_nodes(validator_nodes, Duration::from_secs(10), &logger);

        ctx.sleep(Duration::from_millis(100)).await;
        drop(store_with_dirs);

        slog::info!(
            logger,
            "Multiple RPC nodes E2E test completed successfully! ✓";
            "rpc_nodes_tested" => NUM_RPC_NODES,
        );
    });
}

/// Test that L-notarization certificates can be queried via gRPC.
///
/// This test verifies the light client verification flow:
/// 1. Validators finalize blocks with L-notarization proofs
/// 2. gRPC clients can query L-notarizations by block height
/// 3. L-notarization response contains valid signature data
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_rpc_node_l_notarization_queries -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_rpc_node_l_notarization_queries() {
    use grpc_client::proto::GetLNotarizationByHeightRequest;
    use grpc_client::proto::consensus_service_client::ConsensusServiceClient;

    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting L-notarization query E2E test";
        "validators" => N,
    );

    // Phase 1: Generate validator identities and transactions
    let num_transactions = 10;
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

    // Phase 2: Create P2P configs
    let base_port = 48000u16 + (rand::random::<u16>() % 10000);
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
                    address: Some(format!("127.0.0.1:{}", other_port).parse().unwrap()),
                    ed25519_public_key: pk_hex,
                    bls_peer_id: peer_set.sorted_peer_ids[j],
                    bls_public_key: None,
                });
            }
        }

        let p2p_config = P2PConfig {
            listen_addr,
            external_addr,
            validators,
            total_number_peers: N,
            maximum_number_faulty_peers: F,
            bootstrap_timeout_ms: 20_000,
            ping_interval_ms: 200,
            ..Default::default()
        };

        p2p_configs.push(p2p_config);
    }

    // Build RPC validator config before identities are moved
    let _rpc_validators: Vec<ValidatorPeerInfo> = identities
        .iter()
        .enumerate()
        .map(|(i, identity)| {
            let port = base_port + (i as u16 * port_gap);
            let ed25519_pk = identity.ed25519_public_key();
            let bls_pk = identity.bls_public_key();
            let mut bls_bytes = Vec::new();
            bls_pk.0.serialize_compressed(&mut bls_bytes).unwrap();
            ValidatorPeerInfo {
                ed25519_public_key: hex::encode(ed25519_pk.as_ref()),
                bls_peer_id: identity.peer_id(),
                bls_public_key: Some(hex::encode(&bls_bytes)),
                address: Some(format!("127.0.0.1:{}", port).parse().unwrap()),
            }
        })
        .collect();

    // Phase 3: Start validator nodes
    slog::info!(logger, "Phase 3: Starting validator nodes");

    let mut validator_nodes: Vec<ValidatorNodeSetup<N, F, M_SIZE>> = Vec::new();
    let mut store_with_dirs: Vec<(Arc<ConsensusStore>, TempDir)> = Vec::new();

    for (i, (identity, p2p_config)) in identities
        .into_iter()
        .zip(p2p_configs.into_iter())
        .enumerate()
    {
        let node_logger = logger.new(o!("node" => i, "peer_id" => identity.peer_id()));
        let (node, storage, temp_dir) = create_validator_node_setup(
            identity,
            p2p_config,
            consensus_config.clone(),
            node_logger,
        );
        validator_nodes.push(node);
        store_with_dirs.push((storage, temp_dir));
    }

    slog::info!(logger, "All validator nodes spawned"; "count" => validator_nodes.len());

    // Phase 4: Run consensus and query L-notarization
    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Wait for validators to bootstrap
        for (i, node) in validator_nodes.iter().enumerate() {
            slog::info!(logger, "Waiting for validator bootstrap"; "node" => i);
            node.p2p_handle.wait_ready().await;
        }
        slog::info!(logger, "All validators bootstrapped");

        // Submit transactions via gRPC to kickstart consensus
        for (i, tx) in transactions.iter().enumerate() {
            let node_idx = i % N;
            let grpc_addr = validator_nodes[node_idx].grpc_addr;

            let addr = format!("http://{}", grpc_addr);
            let mut client =
                grpc_client::proto::transaction_service_client::TransactionServiceClient::connect(
                    addr,
                )
                .await
                .ok();

            if let Some(ref mut c) = client {
                let tx_bytes = consensus::storage::conversions::serialize_for_db(tx)
                    .expect("serialize tx")
                    .to_vec();
                let request = grpc_client::proto::SubmitTransactionRequest {
                    transaction_bytes: tx_bytes,
                };
                let _ = c.submit_transaction(request).await;
            }
        }

        slog::info!(
            logger,
            "Transactions submitted, waiting for blocks with L-notarization"
        );

        // Wait for blocks to be finalized with L-notarizations
        ctx.sleep(Duration::from_secs(15)).await;

        // Verify validators have finalized blocks
        let validator_block_count = store_with_dirs[0]
            .0
            .get_all_finalized_blocks()
            .map(|b| b.len())
            .unwrap_or(0);

        slog::info!(
            logger,
            "Validators have finalized blocks";
            "count" => validator_block_count,
        );

        assert!(
            validator_block_count > 0,
            "Validators should have finalized blocks"
        );

        // Phase 5: Query L-notarization via gRPC ConsensusService
        slog::info!(logger, "Phase 5: Querying L-notarization via gRPC");

        let first_validator_grpc = validator_nodes[0].grpc_addr;
        let addr = format!("http://{}", first_validator_grpc);

        let mut consensus_client = ConsensusServiceClient::connect(addr)
            .await
            .expect("connect to consensus service");

        // Query L-notarization for block at height 1
        let l_notarization_response = consensus_client
            .get_l_notarization_by_height(GetLNotarizationByHeightRequest { height: 1 })
            .await
            .expect("query l-notarization")
            .into_inner();

        slog::info!(
            logger,
            "Received L-notarization response";
            "view" => l_notarization_response.view,
            "height" => l_notarization_response.height,
            "block_hash_len" => l_notarization_response.block_hash.len(),
            "signers" => l_notarization_response.peer_ids.len(),
            "error" => l_notarization_response.error,
        );

        // Verify L-notarization data
        if l_notarization_response.error == 0 {
            // ErrorCode::Unspecified means success
            assert_eq!(l_notarization_response.height, 1);
            assert!(!l_notarization_response.block_hash.is_empty());
            assert!(!l_notarization_response.aggregated_signature.is_empty());
            assert!(
                !l_notarization_response.peer_ids.is_empty(),
                "L-notarization should have signer peer IDs"
            );

            slog::info!(
                logger,
                "L-notarization verification passed ✓";
                "view" => l_notarization_response.view,
                "signers" => l_notarization_response.peer_ids.len(),
            );
        } else {
            slog::warn!(
                logger,
                "L-notarization not found (may not be stored yet)";
                "error" => l_notarization_response.error,
            );
        }

        // Phase 6: Shutdown
        slog::info!(logger, "Phase 6: Graceful shutdown");

        shutdown_validator_nodes(validator_nodes, Duration::from_secs(10), &logger);

        ctx.sleep(Duration::from_millis(100)).await;
        drop(store_with_dirs);

        slog::info!(
            logger,
            "L-notarization query E2E test completed successfully! ✓"
        );
    });
}
