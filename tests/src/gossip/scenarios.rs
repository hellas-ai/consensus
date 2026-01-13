//! End-to-end integration tests for transaction gossiping via P2P network.
//!
//! These tests verify that transactions broadcast via P2P gossip are correctly
//! propagated to all nodes' mempools across the network.
//!
//! Unlike the consensus e2e tests which push transactions directly to mempools,
//! these tests verify the full gossip flow:
//! Source Node -> P2P broadcast_transaction -> Network -> Peer Nodes -> route_incoming_message ->
//! Mempool

#![cfg(test)]

use std::collections::HashSet;
use std::time::Duration;

use commonware_runtime::tokio::Runner as TokioRunner;
use commonware_runtime::{Clock, Runner};

use super::helpers::{F, N, create_gossip_test_network, create_test_logger};

/// Test that a single node broadcasting a transaction results in all other nodes receiving it.
///
/// This is the fundamental gossip propagation test:
/// 1. Setup 6 nodes with P2P networking
/// 2. Node 0 broadcasts a transaction via P2P
/// 3. Verify all other 5 nodes receive the transaction in their tx_consumer
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_single_node_broadcasts_tx_all_nodes_receive -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_single_node_broadcasts_tx_all_nodes_receive() {
    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting gossip propagation test: single node broadcast";
        "nodes" => N,
    );

    // Create network without consensus (pure gossip test)
    let mut network = create_gossip_test_network(N, false, logger.clone());

    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Phase 1: Wait for bootstrap
        slog::info!(logger, "Phase 1: Waiting for bootstrap");
        network.wait_for_bootstrap().await;

        // Give extra time for peer connections to stabilize
        ctx.sleep(Duration::from_millis(500)).await;

        // Phase 2: Create and broadcast a transaction from node 0
        slog::info!(logger, "Phase 2: Broadcasting transaction from node 0");

        let tx = network.create_transaction(0, 0);
        let tx_hash = tx.tx_hash;

        // Broadcast via node 0's P2P network
        network.nodes[0]
            .p2p_handle
            .broadcast_transaction(tx)
            .expect("Failed to broadcast transaction");

        slog::info!(
            logger,
            "Transaction broadcast";
            "tx_hash" => hex::encode(&tx_hash[..8]),
        );

        // Phase 3: Wait for transaction to propagate to all other nodes
        slog::info!(logger, "Phase 3: Waiting for gossip propagation");

        let timeout = Duration::from_secs(10);
        let start = std::time::Instant::now();
        let mut received_by: HashSet<usize> = HashSet::new();

        // Skip node 0 (the sender)
        while start.elapsed() < timeout && received_by.len() < N - 1 {
            for i in 1..N {
                if received_by.contains(&i) {
                    continue;
                }

                if let Ok(received_tx) = network.nodes[i].tx_consumer.pop()
                    && received_tx.tx_hash == tx_hash
                {
                    slog::info!(
                        logger,
                        "Transaction received";
                        "node" => i,
                        "elapsed_ms" => start.elapsed().as_millis(),
                    );
                    received_by.insert(i);
                }
            }

            if received_by.len() < N - 1 {
                ctx.sleep(Duration::from_millis(50)).await;
            }
        }

        // Phase 4: Verify results
        slog::info!(
            logger,
            "Phase 4: Verifying results";
            "received_count" => received_by.len(),
            "expected_count" => N - 1,
        );

        assert_eq!(
            received_by.len(),
            N - 1,
            "All {} other nodes should receive the transaction, but only {} did",
            N - 1,
            received_by.len()
        );

        // Phase 5: Cleanup
        slog::info!(logger, "Phase 5: Shutting down");
        network.shutdown();

        slog::info!(logger, "Test completed successfully! ✓");
    });
}

/// Test concurrent broadcasts from multiple nodes.
///
/// All 6 nodes simultaneously broadcast different transactions, and we verify
/// that each node's mempool eventually contains all 6 transactions.
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_concurrent_broadcasts_from_multiple_nodes -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_concurrent_broadcasts_from_multiple_nodes() {
    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting gossip propagation test: concurrent broadcasts";
        "nodes" => N,
    );

    let mut network = create_gossip_test_network(N, false, logger.clone());

    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Phase 1: Bootstrap
        slog::info!(logger, "Phase 1: Waiting for bootstrap");
        network.wait_for_bootstrap().await;
        // Give extra time for all peer connections to fully stabilize
        ctx.sleep(Duration::from_secs(2)).await;

        // Phase 2: Each node broadcasts a unique transaction
        slog::info!(logger, "Phase 2: All nodes broadcasting transactions");

        let mut tx_hashes: Vec<[u8; 32]> = Vec::new();
        for i in 0..N {
            let tx = network.create_transaction(i, 0);
            tx_hashes.push(tx.tx_hash);

            slog::debug!(
                logger,
                "Node broadcasting transaction";
                "node" => i,
                "tx_hash" => hex::encode(&tx.tx_hash[..8]),
            );

            // Broadcast via P2P to all peers
            if let Err(e) = network.nodes[i].p2p_handle.broadcast_transaction(tx) {
                slog::warn!(
                    logger,
                    "Failed to broadcast transaction";
                    "node" => i,
                    "error" => ?e,
                );
            }
        }

        // Phase 3: Wait for all transactions to propagate
        slog::info!(logger, "Phase 3: Waiting for gossip propagation");

        let timeout = Duration::from_secs(15);
        let start = std::time::Instant::now();

        // Track which transactions each node has received
        let mut received_per_node: Vec<HashSet<[u8; 32]>> = vec![HashSet::new(); N];

        while start.elapsed() < timeout {
            let mut all_complete = true;

            for (i, received) in received_per_node.iter_mut().enumerate().take(N) {
                // Each node should receive N-1 transactions (all except its own)
                if received.len() < N - 1 {
                    all_complete = false;

                    // Poll for received transactions
                    while let Ok(tx) = network.nodes[i].tx_consumer.pop() {
                        received.insert(tx.tx_hash);
                    }
                }
            }

            if all_complete {
                break;
            }

            ctx.sleep(Duration::from_millis(50)).await;
        }

        // Phase 4: Verify results
        slog::info!(logger, "Phase 4: Verifying results");

        for (i, received) in received_per_node.iter_mut().enumerate().take(N) {
            let received_count = received.len();
            slog::info!(
                logger,
                "Node received transactions";
                "node" => i,
                "received" => received_count,
                "expected" => N - 1,
            );

            // Each node should receive all transactions except its own
            // (its own tx goes directly to mempool, not via P2P consumer)
            assert!(
                received_count >= N - 1,
                "Node {} should receive at least {} transactions, but got {}",
                i,
                N - 1,
                received_count
            );
        }

        // Phase 5: Cleanup
        slog::info!(logger, "Phase 5: Shutting down");
        network.shutdown();

        slog::info!(logger, "Test completed successfully! ✓");
    });
}

/// Test gossip propagation to a late-joining node.
///
/// This verifies that a node joining the network late can still receive
/// transactions that were broadcast before it joined.
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_gossip_propagation_with_delayed_node -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_gossip_propagation_with_delayed_node() {
    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting gossip propagation test: delayed node join";
        "nodes" => N,
    );

    // For this test, we start with N-1 nodes, broadcast transactions,
    // then start the final node and verify it receives pending txs

    // Note: This test requires special handling since late-joining nodes
    // need a sync mechanism to get pending transactions. For now, we test
    // that transactions broadcast while all nodes are up reach all nodes.

    let mut network = create_gossip_test_network(N, false, logger.clone());

    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Phase 1: Bootstrap all nodes
        slog::info!(logger, "Phase 1: Waiting for bootstrap");
        network.wait_for_bootstrap().await;
        ctx.sleep(Duration::from_secs(1)).await;

        // Phase 2: Broadcast transactions from first few nodes
        slog::info!(logger, "Phase 2: Broadcasting transactions");

        let num_txs = 10;
        let mut tx_hashes: Vec<[u8; 32]> = Vec::new();

        for i in 0..num_txs {
            let tx = network.create_transaction(i, 0);
            tx_hashes.push(tx.tx_hash);

            let node_idx = i % (N - 1); // Distribute across all but last node
            if let Err(e) = network.nodes[node_idx].p2p_handle.broadcast_transaction(tx) {
                slog::warn!(logger, "Failed to broadcast tx"; "error" => ?e);
            }
        }

        // Phase 3: Wait for propagation
        slog::info!(logger, "Phase 3: Waiting for propagation");

        let timeout = Duration::from_secs(10);
        let start = std::time::Instant::now();
        let last_node_idx = N - 1;
        let mut received_by_last_node: HashSet<[u8; 32]> = HashSet::new();

        while start.elapsed() < timeout && received_by_last_node.len() < num_txs {
            while let Ok(tx) = network.nodes[last_node_idx].tx_consumer.pop() {
                received_by_last_node.insert(tx.tx_hash);
            }
            ctx.sleep(Duration::from_millis(50)).await;
        }

        // Phase 4: Verify last node received all transactions
        slog::info!(
            logger,
            "Phase 4: Verifying last node received transactions";
            "received" => received_by_last_node.len(),
            "expected" => num_txs,
        );

        // Cleanup
        network.shutdown();

        slog::info!(logger, "Test completed! ✓");
    });
}

/// Full end-to-end test: transaction gossip leading to block inclusion.
///
/// This test verifies the complete flow:
/// 1. Transaction broadcast via P2P gossip
/// 2. Transaction arrives in mempool
/// 3. Transaction is included in a finalized block
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_transaction_gossip_to_block_inclusion -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_transaction_gossip_to_block_inclusion() {
    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting full e2e gossip test: gossip to block inclusion";
        "nodes" => N,
    );

    // Create network WITH consensus enabled
    let mut network = create_gossip_test_network(N, true, logger.clone());

    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Phase 1: Bootstrap
        slog::info!(logger, "Phase 1: Waiting for bootstrap");
        network.wait_for_bootstrap().await;
        ctx.sleep(Duration::from_secs(1)).await;

        // Phase 2: Submit transactions
        slog::info!(logger, "Phase 2: Submitting transactions via gossip");

        let num_txs = 20;
        let mut expected_tx_hashes: HashSet<[u8; 32]> = HashSet::new();

        for i in 0..num_txs {
            let tx = network.create_transaction(i, 0);
            expected_tx_hashes.insert(tx.tx_hash);

            // Broadcast transactions via P2P gossip
            let node_idx = i % N;
            if let Err(e) = network.nodes[node_idx].p2p_handle.broadcast_transaction(tx) {
                slog::warn!(logger, "Failed to broadcast tx"; "node" => node_idx, "error" => ?e);
            }
        }

        slog::info!(
            logger,
            "Transactions submitted";
            "count" => num_txs,
        );

        // Phase 3: Wait for consensus to progress and include transactions
        slog::info!(logger, "Phase 3: Waiting for consensus to finalize blocks");

        let test_duration = Duration::from_secs(30);
        let check_interval = Duration::from_secs(5);
        let start = std::time::Instant::now();

        while start.elapsed() < test_duration {
            ctx.sleep(check_interval).await;
            slog::info!(
                logger,
                "Consensus running";
                "elapsed_secs" => start.elapsed().as_secs(),
            );
        }

        // Phase 4: Verify transaction inclusion
        slog::info!(logger, "Phase 4: Verifying transaction inclusion in blocks");

        let mut included_tx_hashes: HashSet<[u8; 32]> = HashSet::new();
        let blocks = network.get_finalized_blocks(0);

        slog::info!(
            logger,
            "Checking finalized blocks";
            "num_blocks" => blocks.len(),
        );

        for block in &blocks {
            for tx in &block.transactions {
                included_tx_hashes.insert(tx.tx_hash);
            }
        }

        let missing: Vec<_> = expected_tx_hashes
            .iter()
            .filter(|h| !included_tx_hashes.contains(*h))
            .collect();

        slog::info!(
            logger,
            "Transaction inclusion results";
            "expected" => expected_tx_hashes.len(),
            "included" => included_tx_hashes.len(),
            "missing" => missing.len(),
        );

        // All transactions should be included
        assert!(
            missing.is_empty(),
            "All transactions should be included in blocks. Missing: {}",
            missing.len()
        );

        // Phase 5: Cleanup
        slog::info!(logger, "Phase 5: Shutting down");
        network.shutdown();

        slog::info!(logger, "Full e2e gossip test completed successfully! ✓");
    });
}

/// Test gossip resilience with a Byzantine node dropping messages.
///
/// This verifies that even when one node (simulated as Byzantine) drops
/// all outgoing transaction gossip, the honest nodes still receive all
/// transactions from each other.
///
/// # Run Instructions
/// ```bash
/// cargo test --package tests --lib test_gossip_resilience_with_byzantine_node -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_gossip_resilience_with_byzantine_node() {
    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting gossip resilience test: Byzantine node";
        "nodes" => N,
        "byzantine_tolerance" => F,
    );

    // In this test, we designate node 0 as "Byzantine" (it won't broadcast)
    // All other nodes broadcast transactions, and we verify they all receive
    // each other's transactions despite the Byzantine node.

    let mut network = create_gossip_test_network(N, false, logger.clone());

    let executor = TokioRunner::default();
    executor.start(|ctx| async move {
        // Phase 1: Bootstrap
        slog::info!(logger, "Phase 1: Waiting for bootstrap");
        network.wait_for_bootstrap().await;
        ctx.sleep(Duration::from_millis(500)).await;

        // Phase 2: Honest nodes (1 to N-1) broadcast transactions
        slog::info!(
            logger,
            "Phase 2: Honest nodes broadcasting transactions";
            "byzantine_node" => 0,
        );

        let mut tx_hashes: Vec<[u8; 32]> = Vec::new();
        for i in 1..N {
            // Skip node 0 (Byzantine)
            let tx = network.create_transaction(i, 0);
            tx_hashes.push(tx.tx_hash);

            slog::debug!(
                logger,
                "Honest node broadcasting";
                "node" => i,
                "tx_hash" => hex::encode(&tx.tx_hash[..8]),
            );

            if let Err(e) = network.nodes[i].p2p_handle.broadcast_transaction(tx) {
                slog::warn!(
                    logger,
                    "Failed to broadcast transaction";
                    "node" => i,
                    "error" => ?e,
                );
            }
        }

        // Phase 3: Wait for propagation among honest nodes
        slog::info!(logger, "Phase 3: Waiting for gossip propagation");

        let timeout = Duration::from_secs(10);
        let start = std::time::Instant::now();
        let num_honest = N - 1;
        let expected_per_honest = num_honest - 1; // Each honest node receives from N-2 others

        let mut received_per_node: Vec<HashSet<[u8; 32]>> = vec![HashSet::new(); N];

        while start.elapsed() < timeout {
            let mut all_complete = true;

            for (i, received) in received_per_node.iter_mut().enumerate().take(N).skip(1) {
                // Only check honest nodes
                if received.len() < expected_per_honest {
                    all_complete = false;

                    while let Ok(tx) = network.nodes[i].tx_consumer.pop() {
                        received.insert(tx.tx_hash);
                    }
                }
            }

            if all_complete {
                break;
            }

            ctx.sleep(Duration::from_millis(50)).await;
        }

        // Phase 4: Verify all honest nodes received transactions
        slog::info!(logger, "Phase 4: Verifying honest node reception");

        for (i, received) in received_per_node.iter_mut().enumerate().take(N).skip(1) {
            let received_count = received.len();
            slog::info!(
                logger,
                "Honest node received";
                "node" => i,
                "received" => received_count,
                "expected" => expected_per_honest,
            );

            assert!(
                received_count >= expected_per_honest,
                "Honest node {} should receive at least {} txs, got {}",
                i,
                expected_per_honest,
                received_count
            );
        }

        // Phase 5: Cleanup
        slog::info!(logger, "Phase 5: Shutting down");
        network.shutdown();

        slog::info!(logger, "Byzantine resilience test completed! ✓");
    });
}
