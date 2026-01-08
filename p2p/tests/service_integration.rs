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

use commonware_cryptography::{Signer, ed25519};
use commonware_runtime::{Clock, Runner};
use consensus::consensus::ConsensusMessage;
use consensus::crypto::aggregated::{BlsSecretKey, PeerId};
use consensus::state::block::Block;
use consensus::state::transaction::Transaction;
use p2p::config::{P2PConfig, ValidatorPeerInfo};
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
        cluster_id: "test-cluster".to_string(),
        max_message_size: 1024 * 1024,
        message_backlog: 1024,
        consensus_rate_per_second: 10000,
        tx_rate_per_second: 50000,
    }
}

/// Test that P2P service can be spawned and shutdown gracefully.
#[test]
fn test_p2p_service_spawn_and_shutdown() {
    let executor = commonware_runtime::tokio::Runner::default();

    executor.start(|ctx| async move {
        let signer = ed25519::PrivateKey::from_seed(7001);
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
            signer,
            consensus_prod,
            tx_prod,
            broadcast_cons,
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
        let signer1 = ed25519::PrivateKey::from_seed(7002);
        let signer2 = ed25519::PrivateKey::from_seed(7003);

        let pk1 = signer1.public_key();
        let pk2 = signer2.public_key();
        let pk1_hex = hex::encode(pk1.as_ref());
        let pk2_hex = hex::encode(pk2.as_ref());

        let port1: u16 = 19601;
        let port2: u16 = 19602;

        // Set up configs with mutual bootstrappers
        let mut config1 = create_test_config(port1);
        config1.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk2_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port2).parse().unwrap()),
            bls_peer_id: PeerId::default(),
        });

        let mut config2 = create_test_config(port2);
        config2.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk1_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port1).parse().unwrap()),
            bls_peer_id: PeerId::default(),
        });

        let logger = create_test_logger();

        // Create P2P service for node1 (the one that will broadcast)
        let (consensus_prod, _consensus_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod, _tx_cons) = RingBuffer::<Transaction>::new(100);
        let (mut broadcast_prod, broadcast_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        let handle1 = spawn(
            commonware_runtime::tokio::Runner::default(),
            config1,
            signer1,
            consensus_prod,
            tx_prod,
            broadcast_cons,
            logger.clone(),
        );

        // Create network service for node2 (the receiver)
        let (mut network2, mut receivers2) =
            p2p::network::NetworkService::new(ctx.clone(), signer2, config2, logger).await;

        // Give services time to initialize and connect
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
        let signer1 = ed25519::PrivateKey::from_seed(7003);
        let signer2 = ed25519::PrivateKey::from_seed(7004);

        let pk1 = signer1.public_key();
        let pk2 = signer2.public_key();
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
            bls_peer_id: PeerId::default(),
        });

        // Create P2P service for node2 (receiver) - runs in separate thread+runtime
        let (consensus_prod, mut consensus_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod, _tx_cons) = RingBuffer::<Transaction>::new(100);
        let (_broadcast_prod, broadcast_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        let handle2 = spawn(
            commonware_runtime::tokio::Runner::default(),
            config2,
            signer2,
            consensus_prod,
            tx_prod,
            broadcast_cons,
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
            bls_peer_id: PeerId::default(),
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
        let signer1 = ed25519::PrivateKey::from_seed(7005);
        let signer2 = ed25519::PrivateKey::from_seed(7006);

        let pk1 = signer1.public_key();
        let pk2 = signer2.public_key();
        let pk1_hex = hex::encode(pk1.as_ref());
        let pk2_hex = hex::encode(pk2.as_ref());

        let port1: u16 = 19604;
        let port2: u16 = 19605;

        let mut config2 = create_test_config(port2);
        config2.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk1_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port1).parse().unwrap()),
            bls_peer_id: PeerId::default(),
        });

        let (consensus_prod, _consensus_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);
        let (tx_prod, mut tx_cons) = RingBuffer::<Transaction>::new(100);
        let (_broadcast_prod, broadcast_cons) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(100);

        let handle2 = spawn(
            commonware_runtime::tokio::Runner::default(),
            config2,
            signer2,
            consensus_prod,
            tx_prod,
            broadcast_cons,
            create_test_logger(),
        );

        let mut config1 = create_test_config(port1);
        config1.validators.push(ValidatorPeerInfo {
            ed25519_public_key: pk2_hex.clone(),
            address: Some(format!("127.0.0.1:{}", port2).parse().unwrap()),
            bls_peer_id: PeerId::default(),
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
        let signer = ed25519::PrivateKey::from_seed(7007);
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
            signer,
            consensus_prod,
            tx_prod,
            broadcast_cons,
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
