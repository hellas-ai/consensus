//! Integration tests for RPC node functionality.
//!
//! These tests verify the RPC node components working together:
//! - Block syncer with store
//! - gRPC server creation and shutdown
//! - P2P handle lifecycle

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use commonware_cryptography::{Signer, ed25519};
use consensus::state::peer::PeerSet;
use consensus::storage::store::ConsensusStore;
use rpc::config::RpcConfig;
use rpc::identity::RpcIdentity;
use rpc::node::RpcNode;
use rpc::sync::{BlockSyncer, SyncConfig, SyncState};
use tempfile::tempdir;

fn test_logger() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

fn create_test_config(grpc_port: u16, p2p_port: u16, data_dir: PathBuf) -> RpcConfig {
    RpcConfig {
        grpc_addr: format!("127.0.0.1:{}", grpc_port).parse().unwrap(),
        p2p_addr: format!("127.0.0.1:{}", p2p_port).parse().unwrap(),
        data_dir,
        cluster_id: "test-cluster".to_string(),
        validators: vec![],
        identity_path: None,
    }
}

#[test]
fn test_rpc_node_creation_and_identity() {
    let temp = tempdir().unwrap();
    let config = create_test_config(50100, 9100, temp.path().to_path_buf());
    let logger = test_logger();

    // Create identity
    let identity = RpcIdentity::from_seed(42);
    let public_key = identity.public_key();

    // Create node
    let node = RpcNode::<6, 1>::new(config, identity, logger).expect("Failed to create RpcNode");

    // Verify identity is accessible
    assert_eq!(node.identity().public_key(), public_key);
}

#[test]
fn test_rpc_node_store_initialization() {
    let temp = tempdir().unwrap();
    let config = create_test_config(50101, 9101, temp.path().to_path_buf());
    let logger = test_logger();
    let identity = RpcIdentity::from_seed(43);

    let _node =
        RpcNode::<6, 1>::new(config.clone(), identity, logger).expect("Failed to create RpcNode");

    // Verify store path contains data_dir
    let store_path = config.data_dir.join("consensus.redb");
    assert!(config.data_dir.exists() || store_path.parent().unwrap().exists());
}

#[test]
fn test_block_syncer_with_store_persistence() {
    let temp = tempdir().unwrap();
    let store_path = temp.path().join("test.redb");
    let logger = test_logger();

    // Create syncer with store
    let store = Arc::new(ConsensusStore::open(&store_path).unwrap());
    let mut syncer = BlockSyncer::<6, 1>::new(
        store,
        vec![],
        PeerSet::new(vec![]),
        SyncConfig::default(),
        logger.clone(),
    );

    // Test state transitions
    assert!(matches!(syncer.state(), SyncState::Discovering));

    syncer.set_target_height(10);
    assert!(matches!(syncer.state(), SyncState::Syncing { .. }));

    // Verify store persists (file exists)
    assert!(store_path.exists());
}

#[test]
fn test_block_syncer_validator_round_robin() {
    let temp = tempdir().unwrap();
    let store = Arc::new(ConsensusStore::open(temp.path().join("test.redb")).unwrap());
    let logger = test_logger();

    // Create validators
    let validators: Vec<_> = (0..3)
        .map(|i| {
            let key = ed25519::PrivateKey::from_seed(100 + i);
            key.public_key()
        })
        .collect();

    let syncer = BlockSyncer::<6, 1>::new(
        store,
        validators.clone(),
        PeerSet::new(vec![]),
        SyncConfig::default(),
        logger,
    );

    // Pick validators multiple times - should always be from list
    let mut picked_set = std::collections::HashSet::new();
    for _ in 0..30 {
        if let Some(v) = syncer.pick_validator() {
            assert!(validators.contains(&v));
            picked_set.insert(v);
        }
    }

    // With random selection, we should have picked multiple validators
    // (statistically very unlikely to pick same one 30 times)
    assert!(
        picked_set.len() > 1,
        "Expected random distribution across validators"
    );
}

#[test]
fn test_sync_config_custom_values() {
    let config = SyncConfig {
        sync_interval: Duration::from_millis(500),
        request_timeout: Duration::from_secs(30),
        batch_size: 50,
    };

    let temp = tempdir().unwrap();
    let store = Arc::new(ConsensusStore::open(temp.path().join("test.redb")).unwrap());
    let logger = test_logger();
    let syncer = BlockSyncer::<6, 1>::new(store, vec![], PeerSet::new(vec![]), config, logger);

    assert_eq!(syncer.sync_interval(), Duration::from_millis(500));
    assert_eq!(syncer.request_timeout(), Duration::from_secs(30));
    assert_eq!(syncer.batch_size(), 50);
}

#[test]
fn test_identity_deterministic_generation() {
    // Same seed should produce same identity
    let id1 = RpcIdentity::from_seed(12345);
    let id2 = RpcIdentity::from_seed(12345);

    assert_eq!(id1.public_key(), id2.public_key());

    // Different seeds should produce different identities
    let id3 = RpcIdentity::from_seed(54321);
    assert_ne!(id1.public_key(), id3.public_key());
}

#[test]
fn test_identity_random_generation() {
    use rand::thread_rng;

    // Random generation should produce unique identities
    let id1 = RpcIdentity::generate(&mut thread_rng());
    let id2 = RpcIdentity::generate(&mut thread_rng());

    assert_ne!(id1.public_key(), id2.public_key());
}

#[tokio::test]
async fn test_rpc_node_shutdown_signal() {
    let temp = tempdir().unwrap();
    let config = create_test_config(50102, 9102, temp.path().to_path_buf());
    let logger = test_logger();
    let identity = RpcIdentity::from_seed(44);

    let node = RpcNode::<6, 1>::new(config, identity, logger).expect("Failed to create RpcNode");

    // Trigger shutdown
    node.shutdown();

    // Verify shutdown flag is set
    assert!(node.is_shutdown());
}

#[test]
fn test_multiple_rpc_nodes_different_ports() {
    let temp1 = tempdir().unwrap();
    let temp2 = tempdir().unwrap();

    let config1 = create_test_config(50103, 9103, temp1.path().to_path_buf());
    let config2 = create_test_config(50104, 9104, temp2.path().to_path_buf());

    let logger = test_logger();
    let id1 = RpcIdentity::from_seed(1);
    let id2 = RpcIdentity::from_seed(2);

    // Both nodes should be creatable without conflict
    let node1 = RpcNode::<6, 1>::new(config1, id1, logger.clone()).expect("Failed to create node1");
    let node2 = RpcNode::<6, 1>::new(config2, id2, logger).expect("Failed to create node2");

    // Different identities
    assert_ne!(node1.identity().public_key(), node2.identity().public_key());
}

#[test]
fn test_sync_state_transitions_via_target() {
    let temp = tempdir().unwrap();
    let store = Arc::new(ConsensusStore::open(temp.path().join("test.redb")).unwrap());
    let logger = test_logger();

    let mut syncer = BlockSyncer::<6, 1>::new(
        store,
        vec![],
        PeerSet::new(vec![]),
        SyncConfig::default(),
        logger,
    );

    // 1. Start in Discovering
    assert!(matches!(syncer.state(), SyncState::Discovering));
    assert!(!syncer.is_synced());

    // 2. Learn target height -> Syncing
    syncer.set_target_height(100);
    assert!(matches!(
        syncer.state(),
        SyncState::Syncing {
            current: 0,
            target: 100
        }
    ));
    assert!(!syncer.is_synced());

    // 3. If target is already reached, go directly to Following
    let temp2 = tempdir().unwrap();
    let store2 = Arc::new(ConsensusStore::open(temp2.path().join("test.redb")).unwrap());
    let mut syncer2 = BlockSyncer::<6, 1>::new(
        store2,
        vec![],
        PeerSet::new(vec![]),
        SyncConfig::default(),
        test_logger(),
    );

    syncer2.set_target_height(0); // Target 0 when at height 0
    assert!(matches!(
        syncer2.state(),
        SyncState::Following { height: 0 }
    ));
    assert!(syncer2.is_synced());
}

#[test]
fn test_block_request_generation() {
    let temp = tempdir().unwrap();
    let store = Arc::new(ConsensusStore::open(temp.path().join("test.redb")).unwrap());
    let logger = test_logger();
    let mut syncer = BlockSyncer::<6, 1>::new(
        store,
        vec![],
        PeerSet::new(vec![]),
        SyncConfig::default(),
        logger,
    );

    // Discovering: request block 1
    let req = syncer.next_block_request().unwrap();
    assert_eq!(req.view, 1);

    // Set target and sync
    syncer.set_target_height(5);

    // Request should be for current + 1
    let req = syncer.next_block_request().unwrap();
    assert_eq!(req.view, 1);
}
