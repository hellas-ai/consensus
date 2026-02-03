//! Integration tests for P2P service lifecycle and message routing.
//!
//! # Known Limitation: Runtime Cleanup Panics
//!
//! These tests may produce tokio runtime cleanup panics in worker threads:
//! ```text
//! Cannot drop a runtime in a context where blocking is not allowed.
//! ```
//!
//! This occurs because:
//! 1. The `spawn()` function creates a P2P service with its own tokio runtime
//! 2. When the service shuts down, `network.shutdown()` aborts internal tasks
//! 3. During task abort, `commonware_p2p` actors are dropped during poll
//! 4. If the dropped actor holds the last reference to the runtime's Context, the runtime tries to
//!    shut down from within an async context
//!
//! This is a limitation of nested tokio runtimes and doesn't affect test correctness.
//! The tests still pass and validate the expected behavior.

use std::time::Duration;

use commonware_runtime::{Clock, Runner};
use consensus::consensus::ConsensusMessage;
use consensus::crypto::aggregated::BlsSecretKey;
use consensus::state::block::Block;
use consensus::state::transaction::Transaction;
use p2p::config::{P2PConfig, ValidatorPeerInfo};
use p2p::identity::ValidatorIdentity;
use p2p::service::spawn;
use rtrb::RingBuffer;
use slog::Logger;

const N: usize = 6;
const F: usize = 1;
const M_SIZE: usize = 3;

fn create_test_logger() -> Logger {
    Logger::root(slog::Discard, slog::o!())
}

fn create_test_config(port: u16) -> P2PConfig {
    P2PConfig {
        listen_addr: format!("127.0.0.1:{}", port).parse().unwrap(),
        external_addr: format!("127.0.0.1:{}", port).parse().unwrap(),
        validators: vec![],
        total_number_peers: 6,          // 5f + 1 for Minimmit (f=1)
        maximum_number_faulty_peers: 1, // f = 1
        cluster_id: "test-cluster".to_string(),
        max_message_size: 1024 * 1024,
        message_backlog: 1024,
        consensus_rate_per_second: 10000,
        tx_rate_per_second: 50000,
        bootstrap_timeout_ms: 5000, // 5s timeout for integration tests
        ping_interval_ms: 200,      // Ping every 200ms during bootstrap
        tx_broadcast_queue_size: 1_000,
        rpc_mode: false,
        max_rpc_connections: 100,
    }
}

/// Test that P2P service can be spawned and shutdown gracefully.
#[test]
fn test_p2p_service_spawn_and_shutdown() {
    let executor = commonware_runtime::tokio::Runner::default();

    executor.start(|ctx| async move {
        // Create identity from BLS key (ed25519 is derived)
        let bls_key = BlsSecretKey::generate(&mut rand::thread_rng());
        let identity = ValidatorIdentity::from_bls_key(bls_key);
        let config = create_test_config(19600);
        let logger = create_test_logger();

        let (consensus_prod, _consensus_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod, _tx_cons) = RingBuffer::<Transaction>::new(100);
        let (_broadcast_prod, broadcast_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        let handle = spawn(
            commonware_runtime::tokio::Runner::default(),
            config,
            identity,
            consensus_prod,
            tx_prod,
            broadcast_cons,
            None,
            logger,
        );

        // Give service time to initialize
        ctx.sleep(Duration::from_millis(500)).await;

        // Verify we can signal shutdown
        handle.shutdown();
        // Don't join from async context - let the P2P thread clean up on its own
        std::mem::forget(handle);

        // Give runtime time to clean up before executor's async block completes
        ctx.sleep(Duration::from_millis(100)).await;
    });
}

/// Test that broadcast notifications wake up the service and messages are actually sent.
#[test]
fn test_p2p_service_broadcast_notification() {
    use commonware_p2p::Receiver;

    let executor = commonware_runtime::tokio::Runner::default();

    executor.start(|ctx| async move {
        // Create identities from BLS keys (ed25519 is derived deterministically)
        let bls_key1 = BlsSecretKey::generate(&mut rand::thread_rng());
        let bls_key2 = BlsSecretKey::generate(&mut rand::thread_rng());
        let identity1 = ValidatorIdentity::from_bls_key(bls_key1);
        let identity2 = ValidatorIdentity::from_bls_key(bls_key2);

        let pk1 = identity1.ed25519_public_key();
        let pk2 = identity2.ed25519_public_key();
        let pk1_hex = hex::encode(pk1.as_ref());
        let pk2_hex = hex::encode(pk2.as_ref());

        let port1: u16 = 19601;
        let port2: u16 = 19602;

        // Set up configs with mutual bootstrappers
        let mut config1 = create_test_config(port1);
        config1.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk2_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port2).parse().unwrap()),
            bls_peer_id: identity2.peer_id(),
            bls_public_key: None,
        });

        let mut config2 = create_test_config(port2);
        config2.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk1_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port1).parse().unwrap()),
            bls_peer_id: identity1.peer_id(),
            bls_public_key: None,
        });

        let logger = create_test_logger();

        // Create P2P service for node1 (the one that will broadcast)
        let (consensus_prod, _consensus_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod, _tx_cons) = RingBuffer::<Transaction>::new(100);
        let (mut broadcast_prod, broadcast_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        // Get the ed25519 key for node2 before consuming identity2
        let signer2 = identity2.clone_ed25519_private_key();

        let handle1 = spawn(
            commonware_runtime::tokio::Runner::default(),
            config1,
            identity1,
            consensus_prod,
            tx_prod,
            broadcast_cons,
            None,
            logger.clone(),
        );

        // Create network service for node2 (the receiver) - uses raw ed25519
        let (mut network2, mut receivers2) =
            p2p::network::NetworkService::new(ctx.clone(), signer2, config2, logger).await;

        // Wait for handle1's P2P service to complete bootstrap phase
        // This is important because broadcast_notify is only processed in the main loop
        handle1.wait_ready().await;

        // Give services additional time to establish connections
        ctx.sleep(Duration::from_millis(1000)).await;

        // Create a consensus message to broadcast
        let block = Block::new(
            1,
            12345,
            [0u8; 32],
            vec![],
            1234567890,
            BlsSecretKey::generate(&mut rand::thread_rng()).sign(b"test"),
            false,
            1,
        );
        let consensus_msg = ConsensusMessage::<N, F, M_SIZE>::BlockProposal(block.clone());

        // Push to broadcast channel and notify
        broadcast_prod.push(consensus_msg).unwrap();
        handle1.broadcast_notify.notify_one();

        // Give service time to process and send via network
        ctx.sleep(Duration::from_millis(500)).await;

        // Verify node2 received the broadcast message
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            receivers2.consensus.recv().await
        })
        .await;

        match result {
            Ok(Ok((sender, msg_bytes))) => {
                // Verify sender is node1
                assert_eq!(sender, pk1, "Message should come from node1");

                // Deserialize and verify it's the same block
                // Convert Bytes to Vec for proper alignment (rkyv requirement)
                use p2p::message::{P2PMessage, deserialize_message};
                let msg_vec: Vec<u8> = msg_bytes.to_vec();
                let p2p_msg: P2PMessage<N, F, M_SIZE> = deserialize_message(&msg_vec).unwrap();
                match p2p_msg {
                    P2PMessage::Consensus(ConsensusMessage::BlockProposal(received_block)) => {
                        assert_eq!(
                            received_block.view(),
                            block.view(),
                            "Received block should match sent block"
                        );
                    }
                    _ => panic!("Expected Consensus BlockProposal message"),
                }
            }
            Ok(Err(e)) => panic!("Receiver error: {:?}", e),
            Err(_) => {
                panic!("Timeout waiting for broadcast message - service may not have processed it")
            }
        }

        // Cleanup - signal shutdown but don't join to avoid nested runtime issues
        handle1.shutdown();
        network2.shutdown();
        std::mem::forget(handle1);

        // Give runtime time to clean up before executor's async block completes
        ctx.sleep(Duration::from_millis(100)).await;
    });
}

/// Test that incoming consensus messages are routed correctly through the P2P service.
///
/// This test verifies the full message flow:
/// network1 (sender) -> TCP -> handle2's NetworkService -> route_incoming_message -> consensus_cons
///
/// The P2P service (handle2) runs in a separate thread with its own tokio runtime,
/// which requires careful timing to ensure proper connection establishment.
#[test]
fn test_p2p_service_routes_consensus_messages() {
    use p2p::message::P2PMessage;
    use p2p::message::serialize_message;

    let executor = commonware_runtime::tokio::Runner::default();

    executor.start(|ctx| async move {
        // Create identities from BLS keys
        let bls_key1 = BlsSecretKey::generate(&mut rand::thread_rng());
        let bls_key2 = BlsSecretKey::generate(&mut rand::thread_rng());
        let identity1 = ValidatorIdentity::from_bls_key(bls_key1);
        let identity2 = ValidatorIdentity::from_bls_key(bls_key2);

        let pk1 = identity1.ed25519_public_key();
        let pk2 = identity2.ed25519_public_key();
        let pk1_hex = hex::encode(pk1.as_ref());
        let pk2_hex = hex::encode(pk2.as_ref());

        // Use unique ports for this test
        let port1: u16 = 19702;
        let port2: u16 = 19703;

        // IMPORTANT: Both nodes need to know about each other for peer discovery
        // Set up node2 config with node1 as a known peer
        let mut config2 = create_test_config(port2);
        config2.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk1_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port1).parse().unwrap()),
            bls_peer_id: identity1.peer_id(),
            bls_public_key: None,
        });

        // Get the ed25519 key for node1 before consuming identity1
        let signer1 = identity1.clone_ed25519_private_key();

        // Create P2P service for node2 (receiver) - runs in separate thread+runtime
        let (consensus_prod, mut consensus_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod, _tx_cons) = RingBuffer::<Transaction>::new(100);
        let (_broadcast_prod, broadcast_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        let handle2 = spawn(
            commonware_runtime::tokio::Runner::default(),
            config2,
            identity2,
            consensus_prod,
            tx_prod,
            broadcast_cons,
            None,
            create_test_logger(),
        );

        // Wait for handle2's thread to start and its internal NetworkService to initialize
        // This is critical because spawn() creates a new thread that needs time to:
        // 1. Start the thread
        // 2. Initialize the tokio runtime
        // 3. Create the NetworkService
        // 4. Start listening on the port
        ctx.sleep(Duration::from_millis(2000)).await;

        // Set up node1 config with node2 as a known peer
        let mut config1 = create_test_config(port1);
        config1.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk2_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port2).parse().unwrap()),
            bls_peer_id: identity1.peer_id(),
            bls_public_key: None,
        });

        // Create network service for node1 (sender) in the test's runtime
        let (mut network1, _receivers1) =
            p2p::network::NetworkService::new(ctx.clone(), signer1, config1, create_test_logger())
                .await;

        // Give peers time to discover each other and establish TCP connections
        ctx.sleep(Duration::from_millis(2000)).await;

        // Create and send a consensus message
        let block = Block::new(
            1,
            12345,
            [0u8; 32],
            vec![],
            1234567890,
            BlsSecretKey::generate(&mut rand::thread_rng()).sign(b"test"),
            false,
            1,
        );
        let consensus_msg = ConsensusMessage::<N, F, M_SIZE>::BlockProposal(block.clone());
        let p2p_msg: P2PMessage<N, F, M_SIZE> = P2PMessage::Consensus(consensus_msg);
        let bytes = serialize_message(&p2p_msg).unwrap();

        // Send to all connected peers (empty vec = broadcast to all)
        network1.broadcast_consensus(bytes, vec![]).await;

        // Poll for message with timeout
        // The message flow: network1 -> TCP -> handle2's NetworkService -> route_incoming_message
        // -> consensus_cons
        let start = std::time::Instant::now();
        let mut received = false;
        while start.elapsed() < Duration::from_secs(5) {
            match consensus_cons.pop() {
                Ok(received_msg) => match received_msg {
                    ConsensusMessage::BlockProposal(b) => {
                        assert_eq!(b.view(), block.view());
                        received = true;
                        break;
                    }
                    _ => panic!("Unexpected message type"),
                },
                Err(_) => {
                    ctx.sleep(Duration::from_millis(50)).await;
                }
            }
        }

        assert!(received, "Failed to receive routed consensus message");

        // Cleanup - signal shutdown but don't join to avoid nested runtime issues
        network1.shutdown();
        handle2.shutdown();
        // Don't join handle2 from async context - let it clean up on its own
        // Joining from an async context can cause tokio runtime panics
        std::mem::forget(handle2);

        // Give runtime time to clean up before executor's async block completes
        ctx.sleep(Duration::from_millis(100)).await;
    });
}

/// Test that incoming transaction messages are routed correctly.
///
/// NOTE: This test may be flaky due to timing issues with network connection establishment.
/// The service runs in a separate thread with its own runtime, which can cause delays.
#[test]
fn test_p2p_service_routes_transaction_messages() {
    use p2p::message::P2PMessage;
    use p2p::message::serialize_message;

    let executor = commonware_runtime::tokio::Runner::default();

    executor.start(|ctx| async move {
        // Create identities from BLS keys
        let bls_key1 = BlsSecretKey::generate(&mut rand::thread_rng());
        let bls_key2 = BlsSecretKey::generate(&mut rand::thread_rng());
        let identity1 = ValidatorIdentity::from_bls_key(bls_key1);
        let identity2 = ValidatorIdentity::from_bls_key(bls_key2);

        let pk1 = identity1.ed25519_public_key();
        let pk2 = identity2.ed25519_public_key();
        let pk1_hex = hex::encode(pk1.as_ref());
        let pk2_hex = hex::encode(pk2.as_ref());

        let port1: u16 = 19604;
        let port2: u16 = 19605;

        let mut config2 = create_test_config(port2);
        config2.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk1_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port1).parse().unwrap()),
            bls_peer_id: identity1.peer_id(),
            bls_public_key: None,
        });

        // Get the ed25519 key for node1 before consuming identity1
        let signer1 = identity1.clone_ed25519_private_key();

        let (consensus_prod, _consensus_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod, mut tx_cons) = RingBuffer::<Transaction>::new(100);
        let (_broadcast_prod, broadcast_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        let handle2 = spawn(
            commonware_runtime::tokio::Runner::default(),
            config2,
            identity2,
            consensus_prod,
            tx_prod,
            broadcast_cons,
            None,
            create_test_logger(),
        );

        let mut config1 = create_test_config(port1);
        config1.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk2_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port2).parse().unwrap()),
            bls_peer_id: identity1.peer_id(),
            bls_public_key: None,
        });

        let (mut network1, _receivers1) =
            p2p::network::NetworkService::new(ctx.clone(), signer1, config1, create_test_logger())
                .await;

        ctx.sleep(Duration::from_millis(1000)).await;

        // Create and send a transaction
        use consensus::crypto::transaction_crypto::TxSecretKey;
        use consensus::state::address::Address;
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let tx = Transaction::new_transfer(
            Address::from_public_key(&sk.public_key()),
            Address::from_bytes([1u8; 32]),
            100,
            0,
            10,
            &sk,
        );

        let p2p_msg: P2PMessage<N, F, M_SIZE> = P2PMessage::Transaction(tx.clone());
        let bytes = serialize_message(&p2p_msg).unwrap();

        network1
            .broadcast_transaction(bytes, vec![pk2.clone()])
            .await;

        // Poll for message with timeout
        let start = std::time::Instant::now();
        let mut received = false;
        while start.elapsed() < Duration::from_secs(5) {
            match tx_cons.pop() {
                Ok(received_tx) => {
                    assert_eq!(received_tx.tx_hash, tx.tx_hash);
                    received = true;
                    break;
                }
                Err(_) => {
                    ctx.sleep(Duration::from_millis(100)).await;
                }
            }
        }

        assert!(received, "Failed to receive routed transaction message");

        // Cleanup - signal shutdown but don't join to avoid nested runtime issues
        network1.shutdown();
        handle2.shutdown();
        std::mem::forget(handle2);

        // Give runtime time to clean up before executor's async block completes
        ctx.sleep(Duration::from_millis(100)).await;
    });
}

/// Test that service handles shutdown signal correctly.
#[test]
fn test_p2p_service_handles_shutdown_signal() {
    let executor = commonware_runtime::tokio::Runner::default();

    executor.start(|ctx| async move {
        // Create identity from BLS key
        let bls_key = BlsSecretKey::generate(&mut rand::thread_rng());
        let identity = ValidatorIdentity::from_bls_key(bls_key);
        let config = create_test_config(19606);
        let logger = create_test_logger();

        let (consensus_prod, _consensus_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod, _tx_cons) = RingBuffer::<Transaction>::new(100);
        let (_broadcast_prod, broadcast_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        let handle = spawn(
            commonware_runtime::tokio::Runner::default(),
            config,
            identity,
            consensus_prod,
            tx_prod,
            broadcast_cons,
            None,
            logger,
        );

        // Give service time to initialize
        ctx.sleep(Duration::from_millis(500)).await;

        // Verify service is running (shutdown flag not set)
        assert!(!handle.shutdown.load(std::sync::atomic::Ordering::Relaxed));

        // Send shutdown signal
        handle.shutdown();

        // Verify shutdown flag is set
        assert!(handle.shutdown.load(std::sync::atomic::Ordering::Relaxed));

        // Don't join from async context - let the P2P thread clean up on its own
        std::mem::forget(handle);

        // Give runtime time to clean up before executor's async block completes
        ctx.sleep(Duration::from_millis(100)).await;
    });
}

/// Test that bootstrap completes successfully when enough peers respond.
///
/// This test verifies:
/// 1. Two nodes can discover each other via ping/pong
/// 2. Bootstrap completes when minimum peers are reached
/// 3. Ready signal is properly set
#[test]
fn test_bootstrap_completes_with_sufficient_peers() {
    let executor = commonware_runtime::tokio::Runner::default();

    executor.start(|ctx| async move {
        // Create identities from BLS keys
        let bls_key1 = BlsSecretKey::generate(&mut rand::thread_rng());
        let bls_key2 = BlsSecretKey::generate(&mut rand::thread_rng());
        let identity1 = ValidatorIdentity::from_bls_key(bls_key1);
        let identity2 = ValidatorIdentity::from_bls_key(bls_key2);

        let pk1 = identity1.ed25519_public_key();
        let pk2 = identity2.ed25519_public_key();
        let pk1_hex = hex::encode(pk1.as_ref());
        let pk2_hex = hex::encode(pk2.as_ref());

        let port1: u16 = 19901;
        let port2: u16 = 19902;

        // Configure node1 with node2 as validator
        let mut config1 = create_test_config(port1);
        config1.total_number_peers = 6; // n = 6
        config1.maximum_number_faulty_peers = 1; // f = 1
        // For n=6, f=1: need n-f-1 = 4 other peers, but we only have 1
        // So min_peers will be capped at 1
        config1.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk2_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port2).parse().unwrap()),
            bls_peer_id: identity2.peer_id(),
            bls_public_key: None,
        });

        // Configure node2 with node1 as validator
        let mut config2 = create_test_config(port2);
        config2.total_number_peers = 6;
        config2.maximum_number_faulty_peers = 1;
        config2.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk1_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port1).parse().unwrap()),
            bls_peer_id: identity1.peer_id(),
            bls_public_key: None,
        });

        let logger = create_test_logger();

        // Create P2P service for node1
        let (consensus_prod1, _consensus_cons1) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod1, _tx_cons1) = RingBuffer::<Transaction>::new(100);
        let (_broadcast_prod1, broadcast_cons1) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        let handle1 = spawn(
            commonware_runtime::tokio::Runner::default(),
            config1,
            identity1,
            consensus_prod1,
            tx_prod1,
            broadcast_cons1,
            None,
            logger.clone(),
        );

        // Create P2P service for node2
        let (consensus_prod2, _consensus_cons2) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod2, _tx_cons2) = RingBuffer::<Transaction>::new(100);
        let (_broadcast_prod2, broadcast_cons2) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        let handle2 = spawn(
            commonware_runtime::tokio::Runner::default(),
            config2,
            identity2,
            consensus_prod2,
            tx_prod2,
            broadcast_cons2,
            None,
            logger,
        );

        // Wait for both services to complete bootstrap
        let start = std::time::Instant::now();
        handle1.wait_ready().await;
        handle2.wait_ready().await;
        let elapsed = start.elapsed();

        // Both should become ready
        assert!(handle1.is_ready(), "Node1 should be ready");
        assert!(handle2.is_ready(), "Node2 should be ready");

        // Bootstrap should complete reasonably quickly (< 5 seconds)
        assert!(
            elapsed < Duration::from_secs(5),
            "Bootstrap took too long: {:?}",
            elapsed
        );

        // Cleanup
        handle1.shutdown();
        handle2.shutdown();
        std::mem::forget(handle1);
        std::mem::forget(handle2);

        ctx.sleep(Duration::from_millis(100)).await;
    });
}

/// Test that bootstrap times out when not enough peers respond.
///
/// This test verifies:
/// 1. Bootstrap waits for the configured timeout
/// 2. Service becomes ready even if timeout occurs (for single-node testing)
/// 3. Ready signal is still set after timeout
#[test]
fn test_bootstrap_timeout_when_insufficient_peers() {
    let executor = commonware_runtime::tokio::Runner::default();

    executor.start(|ctx| async move {
        // Create identity from BLS key
        let bls_key = BlsSecretKey::generate(&mut rand::thread_rng());
        let identity = ValidatorIdentity::from_bls_key(bls_key);

        // Create a non-existent validator identity for testing timeout
        let non_existent_bls_key = BlsSecretKey::generate(&mut rand::thread_rng());
        let non_existent_identity = ValidatorIdentity::from_bls_key(non_existent_bls_key);
        let non_existent_pk = non_existent_identity.ed25519_public_key();
        let non_existent_pk_hex = hex::encode(non_existent_pk.as_ref());

        let port: u16 = 19903;

        // Configure with a validator that won't respond (wrong port)
        // This ensures min_peers > 0, so bootstrap will actually wait
        let mut config = create_test_config(port);
        config.total_number_peers = 6;
        config.maximum_number_faulty_peers = 1;
        config.bootstrap_timeout_ms = 500; // Short timeout for test
        // Add a validator that won't respond (port that's not listening)
        config.validators.push(ValidatorPeerInfo {
            ed25519_public_key: non_existent_pk_hex,
            address: Some(format!("127.0.0.1:{}", 19999).parse().unwrap()), // Port not listening
            bls_peer_id: non_existent_identity.peer_id(),
            bls_public_key: None,
        });

        let logger = create_test_logger();

        let (consensus_prod, _consensus_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod, _tx_cons) = RingBuffer::<Transaction>::new(100);
        let (_broadcast_prod, broadcast_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        let handle = spawn(
            commonware_runtime::tokio::Runner::default(),
            config,
            identity,
            consensus_prod,
            tx_prod,
            broadcast_cons,
            None,
            logger,
        );

        // Wait for bootstrap to complete (should timeout)
        let start = std::time::Instant::now();
        handle.wait_ready().await;
        let elapsed = start.elapsed();

        // Should become ready after timeout (even without peer responses)
        assert!(handle.is_ready(), "Service should be ready after timeout");

        // Should have waited at least the timeout duration
        assert!(
            elapsed >= Duration::from_millis(450),
            "Should wait at least timeout duration: {:?}",
            elapsed
        );
        // But not too much longer
        assert!(
            elapsed < Duration::from_secs(2),
            "Should not wait too long after timeout: {:?}",
            elapsed
        );

        // Cleanup
        handle.shutdown();
        std::mem::forget(handle);

        ctx.sleep(Duration::from_millis(100)).await;
    });
}

/// Test that bootstrap skips when min_peers is 0.
///
/// This test verifies that when no peers are required, bootstrap completes immediately.
#[test]
fn test_bootstrap_skips_when_no_peers_required() {
    let executor = commonware_runtime::tokio::Runner::default();

    executor.start(|ctx| async move {
        // Create identity from BLS key
        let bls_key = BlsSecretKey::generate(&mut rand::thread_rng());
        let identity = ValidatorIdentity::from_bls_key(bls_key);

        let port: u16 = 19904;

        // Configure with total_number_peers = 1 (only ourselves)
        let mut config = create_test_config(port);
        config.total_number_peers = 1; // Only ourselves
        config.maximum_number_faulty_peers = 0; // f = 0
        // min_other_peers = 1 - 0 - 1 = 0, so bootstrap should skip
        config.validators = vec![];

        let logger = create_test_logger();

        let (consensus_prod, _consensus_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod, _tx_cons) = RingBuffer::<Transaction>::new(100);
        let (_broadcast_prod, broadcast_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        let handle = spawn(
            commonware_runtime::tokio::Runner::default(),
            config,
            identity,
            consensus_prod,
            tx_prod,
            broadcast_cons,
            None,
            logger,
        );

        // Wait for bootstrap - should complete almost immediately
        let start = std::time::Instant::now();
        handle.wait_ready().await;
        let elapsed = start.elapsed();

        // Should become ready very quickly (bootstrap skipped)
        assert!(handle.is_ready(), "Service should be ready immediately");
        assert!(
            elapsed < Duration::from_millis(500),
            "Bootstrap should skip quickly"
        );

        // Cleanup
        handle.shutdown();
        std::mem::forget(handle);

        ctx.sleep(Duration::from_millis(100)).await;
    });
}

/// Test bootstrap with three nodes (more realistic scenario).
///
/// This test verifies:
/// 1. Multiple nodes can bootstrap together
/// 2. All nodes become ready when minimum threshold is reached
/// 3. Ping/pong works across multiple nodes
#[test]
fn test_bootstrap_with_three_nodes() {
    let executor = commonware_runtime::tokio::Runner::default();

    executor.start(|ctx| async move {
        // Create identities from BLS keys
        let bls_key1 = BlsSecretKey::generate(&mut rand::thread_rng());
        let bls_key2 = BlsSecretKey::generate(&mut rand::thread_rng());
        let bls_key3 = BlsSecretKey::generate(&mut rand::thread_rng());
        let identity1 = ValidatorIdentity::from_bls_key(bls_key1);
        let identity2 = ValidatorIdentity::from_bls_key(bls_key2);
        let identity3 = ValidatorIdentity::from_bls_key(bls_key3);

        let pk1 = identity1.ed25519_public_key();
        let pk2 = identity2.ed25519_public_key();
        let pk3 = identity3.ed25519_public_key();
        let pk1_hex = hex::encode(pk1.as_ref());
        let pk2_hex = hex::encode(pk2.as_ref());
        let pk3_hex = hex::encode(pk3.as_ref());

        let port1: u16 = 19905;
        let port2: u16 = 19906;
        let port3: u16 = 19907;

        // Configure node1 with nodes 2 and 3 as validators
        let mut config1 = create_test_config(port1);
        config1.total_number_peers = 6;
        config1.maximum_number_faulty_peers = 1;
        config1.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk2_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port2).parse().unwrap()),
            bls_peer_id: identity2.peer_id(),
            bls_public_key: None,
        });
        config1.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk3_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port3).parse().unwrap()),
            bls_peer_id: identity3.peer_id(),
            bls_public_key: None,
        });

        // Configure node2 with nodes 1 and 3 as validators
        let mut config2 = create_test_config(port2);
        config2.total_number_peers = 6;
        config2.maximum_number_faulty_peers = 1;
        config2.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk1_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port1).parse().unwrap()),
            bls_peer_id: identity1.peer_id(),
            bls_public_key: None,
        });
        config2.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk3_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port3).parse().unwrap()),
            bls_peer_id: identity3.peer_id(),
            bls_public_key: None,
        });

        // Configure node3 with nodes 1 and 2 as validators
        let mut config3 = create_test_config(port3);
        config3.total_number_peers = 6;
        config3.maximum_number_faulty_peers = 1;
        config3.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk1_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port1).parse().unwrap()),
            bls_peer_id: identity1.peer_id(),
            bls_public_key: None,
        });
        config3.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk2_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port2).parse().unwrap()),
            bls_peer_id: identity2.peer_id(),
            bls_public_key: None,
        });

        let logger = create_test_logger();

        // Create all three P2P services
        let (consensus_prod1, _consensus_cons1) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod1, _tx_cons1) = RingBuffer::<Transaction>::new(100);
        let (_broadcast_prod1, broadcast_cons1) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        let handle1 = spawn(
            commonware_runtime::tokio::Runner::default(),
            config1,
            identity1,
            consensus_prod1,
            tx_prod1,
            broadcast_cons1,
            None,
            logger.clone(),
        );

        let (consensus_prod2, _consensus_cons2) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod2, _tx_cons2) = RingBuffer::<Transaction>::new(100);
        let (_broadcast_prod2, broadcast_cons2) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        let handle2 = spawn(
            commonware_runtime::tokio::Runner::default(),
            config2,
            identity2,
            consensus_prod2,
            tx_prod2,
            broadcast_cons2,
            None,
            logger.clone(),
        );

        let (consensus_prod3, _consensus_cons3) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod3, _tx_cons3) = RingBuffer::<Transaction>::new(100);
        let (_broadcast_prod3, broadcast_cons3) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        let handle3 = spawn(
            commonware_runtime::tokio::Runner::default(),
            config3,
            identity3,
            consensus_prod3,
            tx_prod3,
            broadcast_cons3,
            None,
            logger,
        );

        // Wait for all services to complete bootstrap
        let start = std::time::Instant::now();
        handle1.wait_ready().await;
        handle2.wait_ready().await;
        handle3.wait_ready().await;
        let elapsed = start.elapsed();

        // All should become ready
        assert!(handle1.is_ready(), "Node1 should be ready");
        assert!(handle2.is_ready(), "Node2 should be ready");
        assert!(handle3.is_ready(), "Node3 should be ready");

        // Bootstrap should complete reasonably quickly
        assert!(
            elapsed < Duration::from_secs(10),
            "Bootstrap took too long: {:?}",
            elapsed
        );

        // Cleanup
        handle1.shutdown();
        handle2.shutdown();
        handle3.shutdown();
        std::mem::forget(handle1);
        std::mem::forget(handle2);
        std::mem::forget(handle3);

        ctx.sleep(Duration::from_millis(100)).await;
    });
}
