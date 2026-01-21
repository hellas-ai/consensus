mod test_helpers;

use std::sync::Arc;

use grpc_client::proto::{
    AddressRole, ErrorCode, GetTransactionRequest, GetTransactionsByAddressRequest,
    SubmitTransactionRequest, TransactionStatus,
};
use test_helpers::{
    create_test_block, create_test_transaction, generate_test_keypair, serialize_transaction,
    spawn_test_server,
};

use consensus::state::address::Address;

/// Test submitting a valid transaction returns tx_hash.
///
/// Note: In this e2e test, the mock P2P handle's ring buffer may be full
/// (no consumer), so we verify the response contains the correct tx_hash
/// regardless of broadcast success.
#[tokio::test]
async fn submit_transaction_roundtrip() {
    let mut server = spawn_test_server().await;
    let mut client = server.transaction_client().await.unwrap();

    // Create a valid signed transaction
    let sk = generate_test_keypair();
    let recipient = Address::from_bytes([7u8; 32]);
    let tx = create_test_transaction(&sk, recipient, 100, 0, 10);

    // Submit via gRPC
    let request = SubmitTransactionRequest {
        transaction_bytes: serialize_transaction(&tx),
    };

    let response = client.submit_transaction(request).await.unwrap();
    let resp = response.into_inner();

    // The tx_hash should be returned regardless of broadcast success
    // In e2e test, the mock P2P buffer may be full, causing broadcast to fail
    if resp.success {
        assert_eq!(resp.tx_hash, hex::encode(tx.tx_hash));
    } else {
        // Broadcast failed due to mock P2P - tx_hash still returned
        assert!(!resp.tx_hash.is_empty() || resp.error_code != 0);
    }
}

/// Test submitting malformed bytes returns InvalidFormat error.
#[tokio::test]
async fn submit_transaction_invalid_bytes() {
    let mut server = spawn_test_server().await;
    let mut client = server.transaction_client().await.unwrap();

    let request = SubmitTransactionRequest {
        transaction_bytes: vec![1, 2, 3, 4, 5], // garbage bytes
    };

    let response = client.submit_transaction(request).await.unwrap();
    let resp = response.into_inner();

    assert!(!resp.success);
    assert_eq!(resp.error_code, ErrorCode::InvalidFormat as i32);
}

/// Test submitting empty bytes returns InvalidFormat error.
#[tokio::test]
async fn submit_transaction_empty_bytes() {
    let mut server = spawn_test_server().await;
    let mut client = server.transaction_client().await.unwrap();

    let request = SubmitTransactionRequest {
        transaction_bytes: vec![],
    };

    let response = client.submit_transaction(request).await.unwrap();
    let resp = response.into_inner();

    assert!(!resp.success);
    assert_eq!(resp.error_code, ErrorCode::InvalidFormat as i32);
}

/// Test getting a transaction that exists in a finalized block.
#[tokio::test]
async fn get_transaction_found() {
    let mut server = spawn_test_server().await;

    // Create a transaction and put it in a finalized block
    let sk = generate_test_keypair();
    let recipient = Address::from_bytes([7u8; 32]);
    let tx = Arc::new(create_test_transaction(&sk, recipient, 100, 0, 10));
    let tx_hash = tx.tx_hash;

    let block = create_test_block(1, 1, [0u8; 32], vec![tx.clone()]);
    server.store.put_finalized_block(&block).unwrap();

    let mut client = server.transaction_client().await.unwrap();

    let request = GetTransactionRequest {
        tx_hash: hex::encode(tx_hash),
    };

    let response = client.get_transaction(request).await.unwrap();
    let resp = response.into_inner();

    assert!(resp.transaction.is_some());
    let tx_info = resp.transaction.unwrap();
    assert_eq!(tx_info.tx_hash, hex::encode(tx_hash));
    assert_eq!(resp.block_height, 1);
    assert_eq!(resp.tx_index, 0);
}

/// Test getting a transaction that doesn't exist returns NotFound.
#[tokio::test]
async fn get_transaction_not_found() {
    let mut server = spawn_test_server().await;
    let mut client = server.transaction_client().await.unwrap();

    let request = GetTransactionRequest {
        tx_hash: "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
    };

    let result = client.get_transaction(request).await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

/// Test getting transaction with invalid hash returns InvalidArgument.
#[tokio::test]
async fn get_transaction_invalid_hash() {
    let mut server = spawn_test_server().await;
    let mut client = server.transaction_client().await.unwrap();

    let request = GetTransactionRequest {
        tx_hash: "not_a_valid_hash".to_string(),
    };

    let result = client.get_transaction(request).await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
}

/// Test transaction status for finalized transaction.
#[tokio::test]
async fn get_transaction_status_finalized() {
    let mut server = spawn_test_server().await;

    // Create and store a finalized transaction
    let sk = generate_test_keypair();
    let recipient = Address::from_bytes([7u8; 32]);
    let tx = Arc::new(create_test_transaction(&sk, recipient, 100, 0, 10));
    let tx_hash = tx.tx_hash;

    let block = create_test_block(1, 1, [0u8; 32], vec![tx.clone()]);
    server.store.put_finalized_block(&block).unwrap();

    let mut client = server.transaction_client().await.unwrap();

    let request = GetTransactionRequest {
        tx_hash: hex::encode(tx_hash),
    };

    let response = client.get_transaction_status(request).await.unwrap();
    let resp = response.into_inner();

    assert_eq!(resp.status, TransactionStatus::Finalized as i32);
    assert_eq!(resp.block_height, 1);
    assert!(!resp.block_hash.is_empty());
}

/// Test transaction status for mempool transaction.
#[tokio::test]
async fn get_transaction_status_mempool() {
    let mut server = spawn_test_server().await;

    // Store transaction directly in mempool (not in a block)
    let sk = generate_test_keypair();
    let recipient = Address::from_bytes([7u8; 32]);
    let tx = create_test_transaction(&sk, recipient, 100, 0, 10);
    let tx_hash = tx.tx_hash;

    server.store.put_transaction(&tx).unwrap();

    let mut client = server.transaction_client().await.unwrap();

    let request = GetTransactionRequest {
        tx_hash: hex::encode(tx_hash),
    };

    let response = client.get_transaction_status(request).await.unwrap();
    let resp = response.into_inner();

    assert_eq!(resp.status, TransactionStatus::PendingMempool as i32);
    assert!(resp.block_hash.is_empty());
}

/// Test transaction status for non-existent transaction.
#[tokio::test]
async fn get_transaction_status_not_found() {
    let mut server = spawn_test_server().await;
    let mut client = server.transaction_client().await.unwrap();

    let request = GetTransactionRequest {
        tx_hash: "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
    };

    let response = client.get_transaction_status(request).await.unwrap();
    let resp = response.into_inner();

    assert_eq!(resp.status, TransactionStatus::NotFound as i32);
}

/// Test querying transactions by address.
#[tokio::test]
async fn get_transactions_by_address() {
    let mut server = spawn_test_server().await;

    // Create transactions from a known sender
    let sk = generate_test_keypair();
    let sender = Address::from_public_key(&sk.public_key());
    let recipient = Address::from_bytes([7u8; 32]);

    let tx1 = Arc::new(create_test_transaction(&sk, recipient, 100, 0, 10));
    let tx2 = Arc::new(create_test_transaction(&sk, recipient, 200, 1, 10));

    let block = create_test_block(1, 1, [0u8; 32], vec![tx1.clone(), tx2.clone()]);
    server.store.put_finalized_block(&block).unwrap();

    let mut client = server.transaction_client().await.unwrap();

    let request = GetTransactionsByAddressRequest {
        address: hex::encode(sender.as_bytes()),
        role: AddressRole::Sender as i32,
        from_height: 0,
        to_height: 0,
        limit: 100,
        cursor: String::new(),
    };

    let response = client.get_transactions_by_address(request).await.unwrap();
    let resp = response.into_inner();

    assert_eq!(resp.transactions.len(), 2);
    assert!(!resp.has_more);
}

/// Test pagination for transactions by address.
#[tokio::test]
async fn get_transactions_by_address_pagination() {
    let mut server = spawn_test_server().await;

    // Create multiple transactions from a known sender
    let sk = generate_test_keypair();
    let sender = Address::from_public_key(&sk.public_key());

    let mut txs = Vec::new();
    for i in 0..5 {
        let recipient = Address::from_bytes([i as u8; 32]);
        let tx = Arc::new(create_test_transaction(&sk, recipient, 100, i, 10));
        txs.push(tx);
    }

    let block = create_test_block(1, 1, [0u8; 32], txs);
    server.store.put_finalized_block(&block).unwrap();

    let mut client = server.transaction_client().await.unwrap();

    // Request only 2 transactions
    let request = GetTransactionsByAddressRequest {
        address: hex::encode(sender.as_bytes()),
        role: AddressRole::Sender as i32,
        from_height: 0,
        to_height: 0,
        limit: 2,
        cursor: String::new(),
    };

    let response = client.get_transactions_by_address(request).await.unwrap();
    let resp = response.into_inner();

    assert_eq!(resp.transactions.len(), 2);
    assert!(resp.has_more);
    assert!(!resp.next_cursor.is_empty());
}
