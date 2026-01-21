//! E2E integration tests for the BlockService.

mod test_helpers;

use std::sync::Arc;

use grpc_client::proto::{Empty, GetBlockByHeightRequest, GetBlockRequest, GetBlocksRequest};
use test_helpers::{
    create_test_block, create_test_transaction, generate_test_keypair, spawn_test_server,
};
use tokio_stream::StreamExt;

use consensus::state::address::Address;

/// Test getting a block by hash.
#[tokio::test]
async fn get_block_by_hash() {
    let mut server = spawn_test_server().await;

    // Create and store a block
    let block = create_test_block(1, 1, [0u8; 32], vec![]);
    let block_hash = block.get_hash();
    server.store.put_finalized_block(&block).unwrap();

    let mut client = server.block_client().await.unwrap();

    let request = GetBlockRequest {
        hash: hex::encode(block_hash),
    };

    let response = client.get_block(request).await.unwrap();
    let resp = response.into_inner();

    assert_eq!(resp.height, 1);
    assert_eq!(resp.view, 1);
    assert!(resp.is_finalized);
    assert_eq!(resp.hash, hex::encode(block_hash));
}

/// Test getting a block with transactions.
#[tokio::test]
async fn get_block_with_transactions() {
    let mut server = spawn_test_server().await;

    // Create a block with a transaction
    let sk = generate_test_keypair();
    let recipient = Address::from_bytes([7u8; 32]);
    let tx = Arc::new(create_test_transaction(&sk, recipient, 100, 0, 10));

    let block = create_test_block(1, 1, [0u8; 32], vec![tx.clone()]);
    let block_hash = block.get_hash();
    server.store.put_finalized_block(&block).unwrap();

    let mut client = server.block_client().await.unwrap();

    let request = GetBlockRequest {
        hash: hex::encode(block_hash),
    };

    let response = client.get_block(request).await.unwrap();
    let resp = response.into_inner();

    assert_eq!(resp.tx_count, 1);
    assert_eq!(resp.transactions.len(), 1);
    assert!(!resp.transactions[0].tx_hash.is_empty());
}

/// Test getting a block that doesn't exist returns NotFound.
#[tokio::test]
async fn get_block_not_found() {
    let mut server = spawn_test_server().await;
    let mut client = server.block_client().await.unwrap();

    let request = GetBlockRequest {
        hash: "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
    };

    let result = client.get_block(request).await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

/// Test getting block with invalid hash returns InvalidArgument.
#[tokio::test]
async fn get_block_invalid_hash() {
    let mut server = spawn_test_server().await;
    let mut client = server.block_client().await.unwrap();

    let request = GetBlockRequest {
        hash: "not_valid_hex".to_string(),
    };

    let result = client.get_block(request).await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
}

/// Test getting the latest block.
#[tokio::test]
async fn get_latest_block() {
    let mut server = spawn_test_server().await;

    // Create multiple blocks
    let block1 = create_test_block(1, 1, [0u8; 32], vec![]);
    let block2 = create_test_block(2, 2, block1.get_hash(), vec![]);
    let block3 = create_test_block(3, 3, block2.get_hash(), vec![]);

    server.store.put_finalized_block(&block1).unwrap();
    server.store.put_finalized_block(&block2).unwrap();
    server.store.put_finalized_block(&block3).unwrap();

    let mut client = server.block_client().await.unwrap();

    let response = client.get_latest_block(Empty {}).await.unwrap();
    let resp = response.into_inner();

    assert_eq!(resp.height, 3);
    assert_eq!(resp.view, 3);
}

/// Test getting latest block when no blocks exist returns NotFound.
#[tokio::test]
async fn get_latest_block_empty_chain() {
    let mut server = spawn_test_server().await;
    let mut client = server.block_client().await.unwrap();

    let result = client.get_latest_block(Empty {}).await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

/// Test getting block by height.
#[tokio::test]
async fn get_block_by_height() {
    let mut server = spawn_test_server().await;

    // Create blocks at different heights
    let block1 = create_test_block(1, 1, [0u8; 32], vec![]);
    let block2 = create_test_block(2, 2, block1.get_hash(), vec![]);
    let block3 = create_test_block(3, 3, block2.get_hash(), vec![]);

    server.store.put_finalized_block(&block1).unwrap();
    server.store.put_finalized_block(&block2).unwrap();
    server.store.put_finalized_block(&block3).unwrap();

    let mut client = server.block_client().await.unwrap();

    let request = GetBlockByHeightRequest { height: 2 };

    let response = client.get_block_by_height(request).await.unwrap();
    let resp = response.into_inner();

    assert_eq!(resp.height, 2);
    assert_eq!(resp.view, 2);
}

/// Test getting block by height that doesn't exist.
#[tokio::test]
async fn get_block_by_height_not_found() {
    let mut server = spawn_test_server().await;
    let mut client = server.block_client().await.unwrap();

    let request = GetBlockByHeightRequest { height: 999 };

    let result = client.get_block_by_height(request).await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

/// Test getting a range of blocks.
#[tokio::test]
async fn get_blocks_range() {
    let mut server = spawn_test_server().await;

    // Create blocks 1-5
    let mut prev_hash = [0u8; 32];
    for i in 1..=5 {
        let block = create_test_block(i, i, prev_hash, vec![]);
        prev_hash = block.get_hash();
        server.store.put_finalized_block(&block).unwrap();
    }

    let mut client = server.block_client().await.unwrap();

    let request = GetBlocksRequest {
        from_height: 2,
        to_height: 4,
        limit: 100,
    };

    let response = client.get_blocks(request).await.unwrap();
    let resp = response.into_inner();

    assert_eq!(resp.blocks.len(), 3); // Heights 2, 3, 4
    assert!(!resp.has_more);
}

/// Test block range pagination with limit.
#[tokio::test]
async fn get_blocks_pagination() {
    let mut server = spawn_test_server().await;

    // Create blocks 1-10
    let mut prev_hash = [0u8; 32];
    for i in 1..=10 {
        let block = create_test_block(i, i, prev_hash, vec![]);
        prev_hash = block.get_hash();
        server.store.put_finalized_block(&block).unwrap();
    }

    let mut client = server.block_client().await.unwrap();

    let request = GetBlocksRequest {
        from_height: 1,
        to_height: 10,
        limit: 3,
    };

    let response = client.get_blocks(request).await.unwrap();
    let resp = response.into_inner();

    assert_eq!(resp.blocks.len(), 3);
    assert!(resp.has_more);
}

/// Test streaming blocks.
#[tokio::test]
async fn stream_blocks() {
    let mut server = spawn_test_server().await;

    // Create blocks 1-3
    let mut prev_hash = [0u8; 32];
    for i in 1..=3 {
        let block = create_test_block(i, i, prev_hash, vec![]);
        prev_hash = block.get_hash();
        server.store.put_finalized_block(&block).unwrap();
    }

    let mut client = server.block_client().await.unwrap();

    let request = GetBlocksRequest {
        from_height: 1,
        to_height: 3,
        limit: 0,
    };

    let response = client.stream_blocks(request).await.unwrap();
    let mut stream = response.into_inner();

    let mut count = 0;
    while let Some(result) = stream.next().await {
        assert!(result.is_ok());
        count += 1;
    }

    assert_eq!(count, 3);
}

/// Test getting blocks from empty chain.
#[tokio::test]
async fn get_blocks_empty_chain() {
    let mut server = spawn_test_server().await;
    let mut client = server.block_client().await.unwrap();

    let request = GetBlocksRequest {
        from_height: 0,
        to_height: 10,
        limit: 100,
    };

    let response = client.get_blocks(request).await.unwrap();
    let resp = response.into_inner();

    assert!(resp.blocks.is_empty());
    assert!(!resp.has_more);
}
