//! E2E integration tests for the NodeService.

mod test_helpers;

use grpc_client::proto::Empty;
use test_helpers::spawn_test_server;

/// Test health check returns healthy status.
#[tokio::test]
async fn health_check() {
    let mut server = spawn_test_server().await;
    let mut client = server.node_client().await.unwrap();

    let response = client.health(Empty {}).await.unwrap();
    let resp = response.into_inner();

    assert!(resp.healthy);
    assert!(resp.components.contains_key("p2p"));
    assert!(resp.components.contains_key("storage"));
    assert!(resp.components.contains_key("consensus"));
}

/// Test sync status endpoint.
#[tokio::test]
async fn get_sync_status() {
    let mut server = spawn_test_server().await;
    let mut client = server.node_client().await.unwrap();

    let response = client.get_sync_status(Empty {}).await.unwrap();
    let resp = response.into_inner();

    // Empty chain should have view 0
    assert_eq!(resp.highest_finalized_view, 0);
    assert_eq!(resp.highest_finalized_height, 0);
}

/// Test sync status with blocks.
#[tokio::test]
async fn get_sync_status_with_blocks() {
    let mut server = spawn_test_server().await;

    // Create a finalized block
    let block = test_helpers::create_test_block(1, 1, [0u8; 32], vec![]);
    server.store.put_finalized_block(&block).unwrap();

    let mut client = server.node_client().await.unwrap();

    let response = client.get_sync_status(Empty {}).await.unwrap();
    let resp = response.into_inner();

    assert_eq!(resp.highest_finalized_height, 1);
}

/// Test consensus status returns proper parameters.
#[tokio::test]
async fn get_consensus_status() {
    let mut server = spawn_test_server().await;
    let mut client = server.node_client().await.unwrap();

    let response = client.get_consensus_status(Empty {}).await.unwrap();
    let resp = response.into_inner();

    // Verify consensus parameters match config
    assert_eq!(resp.peer_id, 0);
    assert_eq!(resp.n, 4);
    assert_eq!(resp.f, 1);
    assert_eq!(resp.total_validators, 4);
}

/// Test node info returns version and network.
#[tokio::test]
async fn get_node_info() {
    let mut server = spawn_test_server().await;
    let mut client = server.node_client().await.unwrap();

    let response = client.get_node_info(Empty {}).await.unwrap();
    let resp = response.into_inner();

    assert!(!resp.version.is_empty());
    assert_eq!(resp.network, "local");
    assert_eq!(resp.peer_id, 0);
    // Uptime should be small since we just started
    assert!(resp.uptime_seconds < 60);
}

/// Test peers endpoint returns required peer count.
#[tokio::test]
async fn get_peers() {
    let mut server = spawn_test_server().await;
    let mut client = server.node_client().await.unwrap();

    let response = client.get_peers(Empty {}).await.unwrap();
    let resp = response.into_inner();

    // Required peers = N - F = 4 - 1 = 3
    assert_eq!(resp.required_peers, 3);
    // No actual peers in e2e test environment
    assert!(resp.peers.is_empty());
}

/// Test mempool stats endpoint.
#[tokio::test]
async fn get_mempool_stats() {
    let mut server = spawn_test_server().await;
    let mut client = server.node_client().await.unwrap();

    let response = client.get_mempool_stats(Empty {}).await.unwrap();
    let resp = response.into_inner();

    // Default fallback values when no real mempool stats
    assert_eq!(resp.capacity, 10_000);
    assert_eq!(resp.pending_count, 0);
}
