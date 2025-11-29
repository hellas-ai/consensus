//! End-to-End Consensus Integration Tests
//!
//! This module contains integration tests that verify the correctness
//! of the Minimmit BFT consensus protocol by simulating networks of replicas.

use super::{network_simulator::LocalNetwork, test_helpers::*};
use crate::consensus_manager::consensus_engine::ConsensusEngine;
use slog::{Drain, Level, Logger, o};
use std::{env, str::FromStr, thread, time::Duration};

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
///
/// # Run with debug logging
/// RUST_LOG=debug cargo test test_e2e_consensus_happy_path -- --ignored --nocapture
/// ///
/// Uses async logging for better performance in multi-threaded tests.
pub fn create_test_logger() -> Logger {
    let log_level = env::var("RUST_LOG")
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

/// Helper to create a test logger that discards output (for quiet tests)
#[allow(dead_code)]
fn create_quiet_logger() -> Logger {
    Logger::root(slog::Discard, o!())
}

#[test]
#[ignore] // Run with: cargo test --lib test_e2e_consensus_happy_path -- --ignored --nocapture
fn test_e2e_consensus_happy_path() {
    // Create logger for test
    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting end-to-end consensus test (happy path)";
        "replicas" => N,
        "byzantine_tolerance" => F,
    );

    // Phase 1: Setup test environment
    slog::info!(logger, "Phase 1: Creating test fixture");
    let fixture = TestFixture::default();

    slog::info!(
        logger,
        "Generated keypairs and peer set";
        "total_replicas" => N,
        "peer_ids" => ?fixture.peer_set.sorted_peer_ids,
    );

    // Phase 2: Initialize network simulator
    slog::info!(logger, "Phase 2: Setting up network simulator");
    let mut network = LocalNetwork::<N, F, M_SIZE>::new();
    let mut replica_setups = Vec::with_capacity(N);

    let mut peer_id_to_secret_key = std::collections::HashMap::new();
    for kp in &fixture.keypairs {
        peer_id_to_secret_key.insert(kp.public_key.to_peer_id(), kp.secret_key.clone());
    }

    for (i, &peer_id) in fixture.peer_set.sorted_peer_ids.iter().enumerate() {
        let secret_key = peer_id_to_secret_key
            .get(&peer_id)
            .expect("Secret key not found")
            .clone();
        let setup = ReplicaSetup::new(peer_id, secret_key);

        slog::debug!(
            logger,
            "Created replica setup";
            "replica_index" => i,
            "peer_id" => peer_id,
        );

        replica_setups.push(setup);
    }

    // Phase 3: Register replicas and start engines, keeping transaction producers
    slog::info!(
        logger,
        "Phase 3: Registering replicas and starting consensus engines"
    );
    let mut engines = Vec::with_capacity(N);
    let mut transaction_producers = Vec::with_capacity(N);

    let mut stores = Vec::with_capacity(N);

    for (i, setup) in replica_setups.into_iter().enumerate() {
        let replica_id = setup.replica_id;

        // Keep transaction producer for later
        let tx_producer = setup.transaction_producer;

        // Keep a clone of the storage for verification
        stores.push(setup.storage.clone());

        // Register with network
        network.register_replica(replica_id, setup.message_producer, setup.broadcast_consumer);

        // Create consensus engine
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => replica_id));

        let engine = ConsensusEngine::<N, F, M_SIZE>::new(
            fixture.config.clone(),
            replica_id,
            setup.secret_key,
            setup.storage,
            setup.message_consumer,
            setup.broadcast_producer,
            setup.transaction_consumer,
            DEFAULT_TICK_INTERVAL,
            replica_logger,
        )
        .expect("Failed to create consensus engine");

        slog::debug!(
            logger,
            "Consensus engine started";
            "replica" => i,
            "peer_id" => replica_id,
        );

        engines.push(engine);
        transaction_producers.push(tx_producer);
    }

    slog::info!(
        logger,
        "All replicas registered and engines started";
        "count" => engines.len(),
    );

    // Phase 4: Start network routing
    slog::info!(logger, "Phase 4: Starting network routing");
    network.start();
    assert!(network.is_running(), "Network should be running");
    slog::info!(logger, "Network routing thread active");

    // Phase 5: Submit transactions
    let num_transactions = 30;
    slog::info!(
        logger,
        "Phase 5: Submitting transactions";
        "count" => num_transactions,
    );

    let transactions = create_test_transactions(&fixture.keypairs, num_transactions);

    // Keep a copy of transaction hashes for verification
    let expected_tx_hashes: std::collections::HashSet<_> =
        transactions.iter().map(|tx| tx.tx_hash).collect();

    for (i, tx) in transactions.into_iter().enumerate() {
        // Distribute transactions across replicas (simulating different clients)
        let replica_idx = i % N;

        transaction_producers[replica_idx]
            .push(tx)
            .expect("Failed to submit transaction");

        slog::debug!(
            logger,
            "Transaction submitted";
            "tx_index" => i,
            "target_replica" => replica_idx,
        );
    }

    slog::info!(
        logger,
        "All transactions submitted";
        "total" => num_transactions,
    );

    // Phase 6: Allow consensus to progress through multiple views
    slog::info!(
        logger,
        "Phase 6: Waiting for consensus to progress";
        "duration_secs" => 30,
    );

    // Wait and check progress periodically
    let test_duration = Duration::from_secs(30);
    let check_interval = Duration::from_secs(5);
    let start_time = std::time::Instant::now();

    while start_time.elapsed() < test_duration {
        thread::sleep(check_interval);

        let elapsed = start_time.elapsed().as_secs();
        let msgs_routed = network.stats.messages_routed();
        let msgs_dropped = network.stats.messages_dropped();

        slog::info!(
            logger,
            "Consensus progress check";
            "elapsed_secs" => elapsed,
            "messages_routed" => msgs_routed,
            "messages_dropped" => msgs_dropped,
        );
    }

    // Phase 7: Verify system health
    slog::info!(logger, "Phase 7: Verifying system health");

    for (i, engine) in engines.iter().enumerate() {
        let is_running = engine.is_running();

        slog::info!(
            logger,
            "Engine health check";
            "replica" => i,
            "is_running" => is_running,
        );

        assert!(is_running, "Engine {} should still be running", i);
    }

    // Phase 8: Collect final statistics
    slog::info!(logger, "Phase 8: Collecting final statistics");

    let final_msgs_routed = network.stats.messages_routed();
    let final_msgs_dropped = network.stats.messages_dropped();
    let drop_rate = if final_msgs_routed > 0 {
        (final_msgs_dropped as f64 / (final_msgs_routed + final_msgs_dropped) as f64) * 100.0
    } else {
        0.0
    };

    slog::info!(
        logger,
        "Final network statistics";
        "messages_routed" => final_msgs_routed,
        "messages_dropped" => final_msgs_dropped,
        "drop_rate_percent" => format!("{:.2}", drop_rate),
    );

    // Assert reasonable performance
    assert!(final_msgs_routed > 0, "Network should have routed messages");

    // Drop rate should be low (< 1% is good)
    assert!(
        drop_rate < 5.0,
        "Message drop rate too high: {:.2}%",
        drop_rate
    );

    // Phase 9: Graceful shutdown
    slog::info!(logger, "Phase 9: Shutting down consensus engines");

    // 1. Signal ALL engines to stop immediately.
    // This prevents one replica from advancing while others are already stopped.
    for engine in &engines {
        engine.shutdown();
    }

    // 2. Stop the network BEFORE waiting for engines to finish.
    // This prevents new messages from being delivered during shutdown,
    // which would cause some replicas to finalize more blocks than others.
    network.shutdown();

    // 3. Now wait for each engine to finish its thread.
    for (i, engine) in engines.into_iter().enumerate() {
        slog::debug!(logger, "Waiting for engine shutdown"; "replica" => i);

        engine
            .shutdown_and_wait(Duration::from_secs(5))
            .unwrap_or_else(|e| {
                slog::error!(
                    logger,
                    "Engine shutdown failed";
                    "replica" => i,
                    "error" => ?e,
                );
                panic!("Engine {} failed to shutdown: {}", i, e)
            });
    }

    slog::info!(logger, "All engines shut down successfully");

    // Phase 10: Verify state consistency
    slog::info!(logger, "Phase 10: Verifying state consistency");

    let mut first_replica_blocks: Option<Vec<crate::state::block::Block>> = None;

    for (i, store) in stores.iter().enumerate() {
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
            assert!(
                curr.view() == prev.view() + 1,
                "View should increase by 1 ({} -> {})",
                prev.view(),
                curr.view()
            );
        }

        // 3. Check consistency across replicas
        if let Some(ref first_blocks) = first_replica_blocks {
            // Allow for 1 block difference due to shutdown race
            let diff = (blocks.len() as i32 - first_blocks.len() as i32).abs();
            assert!(
                diff <= 1,
                "Replica {} block count {} differs too much from replica 0 count {}",
                i,
                blocks.len(),
                first_blocks.len()
            );

            // Verify common prefix
            let min_len = std::cmp::min(blocks.len(), first_blocks.len());
            for (j, (b1, b2)) in blocks
                .iter()
                .take(min_len)
                .zip(first_blocks.iter().take(min_len))
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

    // Verify all transactions were included
    let mut included_tx_hashes = std::collections::HashSet::new();
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
        "total_messages_routed" => final_msgs_routed,
        "test_duration_secs" => 30,
    );
}

#[test]
#[ignore] // Run with: cargo test --lib test_e2e_consensus_continuous_load -- --ignored --nocapture
fn test_e2e_consensus_continuous_load() {
    // Create logger for test
    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting end-to-end consensus test (continuous load)";
        "replicas" => N,
        "byzantine_tolerance" => F,
    );

    // Phase 1: Setup test environment
    slog::info!(logger, "Phase 1: Creating test fixture");
    let fixture = TestFixture::default();

    slog::info!(
        logger,
        "Generated keypairs and peer set";
        "total_replicas" => N,
        "peer_ids" => ?fixture.peer_set.sorted_peer_ids,
    );

    // Phase 2: Initialize network simulator
    slog::info!(logger, "Phase 2: Setting up network simulator");
    let mut network = LocalNetwork::<N, F, M_SIZE>::new();
    let mut replica_setups = Vec::with_capacity(N);

    let mut peer_id_to_secret_key = std::collections::HashMap::new();
    for kp in &fixture.keypairs {
        peer_id_to_secret_key.insert(kp.public_key.to_peer_id(), kp.secret_key.clone());
    }

    for (i, &peer_id) in fixture.peer_set.sorted_peer_ids.iter().enumerate() {
        let secret_key = peer_id_to_secret_key
            .get(&peer_id)
            .expect("Secret key not found")
            .clone();
        let setup = ReplicaSetup::new(peer_id, secret_key);

        slog::debug!(
            logger,
            "Created replica setup";
            "replica_index" => i,
            "peer_id" => peer_id,
        );

        replica_setups.push(setup);
    }

    // Phase 3: Register replicas and start engines, keeping transaction producers
    slog::info!(
        logger,
        "Phase 3: Registering replicas and starting consensus engines"
    );
    let mut engines = Vec::with_capacity(N);
    let mut transaction_producers = Vec::with_capacity(N);
    let mut stores = Vec::with_capacity(N);

    for (i, setup) in replica_setups.into_iter().enumerate() {
        let replica_id = setup.replica_id;

        // Keep transaction producer for later
        let tx_producer = setup.transaction_producer;

        // Keep a clone of the storage for verification
        stores.push(setup.storage.clone());

        // Register with network
        network.register_replica(replica_id, setup.message_producer, setup.broadcast_consumer);

        // Create consensus engine
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => replica_id));

        let engine = ConsensusEngine::<N, F, M_SIZE>::new(
            fixture.config.clone(),
            replica_id,
            setup.secret_key,
            setup.storage,
            setup.message_consumer,
            setup.broadcast_producer,
            setup.transaction_consumer,
            DEFAULT_TICK_INTERVAL,
            replica_logger,
        )
        .expect("Failed to create consensus engine");

        slog::debug!(
            logger,
            "Consensus engine started";
            "replica" => i,
            "peer_id" => replica_id,
        );

        engines.push(engine);
        transaction_producers.push(tx_producer);
    }

    slog::info!(
        logger,
        "All replicas registered and engines started";
        "count" => engines.len(),
    );

    // Phase 4: Start network routing
    slog::info!(logger, "Phase 4: Starting network routing");
    network.start();
    assert!(network.is_running(), "Network should be running");
    slog::info!(logger, "Network routing thread active");

    // Phase 5: Continuous transaction submission during consensus
    let test_duration = Duration::from_secs(30);
    let tx_interval = Duration::from_millis(100); // Submit a transaction every 100ms
    let check_interval = Duration::from_secs(5);

    slog::info!(
        logger,
        "Phase 5: Running consensus with continuous transaction load";
        "duration_secs" => test_duration.as_secs(),
        "tx_interval_ms" => tx_interval.as_millis(),
    );

    let start_time = std::time::Instant::now();
    let mut last_check = start_time;
    let mut tx_count = 0usize;
    let mut tx_index = 0usize;
    let mut expected_tx_hashes = std::collections::HashSet::new();

    while start_time.elapsed() < test_duration {
        // Submit a batch of transactions
        let batch_size = 5;
        let transactions = create_test_transactions(&fixture.keypairs, batch_size);

        for tx in transactions {
            let tx_hash = tx.tx_hash;
            // Distribute transactions across replicas (round-robin)
            let replica_idx = tx_index % N;
            tx_index += 1;

            if transaction_producers[replica_idx].push(tx).is_ok() {
                tx_count += 1;
                expected_tx_hashes.insert(tx_hash);
            }
        }

        // Periodic progress check
        if last_check.elapsed() >= check_interval {
            let elapsed = start_time.elapsed().as_secs();
            let msgs_routed = network.stats.messages_routed();
            let msgs_dropped = network.stats.messages_dropped();

            slog::info!(
                logger,
                "Consensus progress check";
                "elapsed_secs" => elapsed,
                "messages_routed" => msgs_routed,
                "messages_dropped" => msgs_dropped,
                "transactions_submitted" => tx_count,
            );

            last_check = std::time::Instant::now();
        }

        thread::sleep(tx_interval);
    }

    slog::info!(
        logger,
        "Continuous load phase complete";
        "total_transactions_submitted" => tx_count,
    );

    // Phase 6: Verify system health
    slog::info!(logger, "Phase 6: Verifying system health");

    for (i, engine) in engines.iter().enumerate() {
        let is_running = engine.is_running();

        slog::info!(
            logger,
            "Engine health check";
            "replica" => i,
            "is_running" => is_running,
        );

        assert!(is_running, "Engine {} should still be running", i);
    }

    // Phase 7: Collect final statistics
    slog::info!(logger, "Phase 7: Collecting final statistics");

    let final_msgs_routed = network.stats.messages_routed();
    let final_msgs_dropped = network.stats.messages_dropped();
    let drop_rate = if final_msgs_routed > 0 {
        (final_msgs_dropped as f64 / (final_msgs_routed + final_msgs_dropped) as f64) * 100.0
    } else {
        0.0
    };

    slog::info!(
        logger,
        "Final network statistics";
        "messages_routed" => final_msgs_routed,
        "messages_dropped" => final_msgs_dropped,
        "drop_rate_percent" => format!("{:.2}", drop_rate),
        "total_transactions" => tx_count,
    );

    // Assert reasonable performance
    assert!(final_msgs_routed > 0, "Network should have routed messages");
    assert!(
        drop_rate < 5.0,
        "Message drop rate too high: {:.2}%",
        drop_rate
    );

    // Phase 8: Graceful shutdown
    slog::info!(logger, "Phase 8: Shutting down consensus engines");

    // 1. Signal ALL engines to stop immediately.
    for engine in &engines {
        engine.shutdown();
    }

    // 2. Stop the network BEFORE waiting for engines to finish.
    network.shutdown();

    // 3. Now wait for each engine to finish its thread.
    for (i, engine) in engines.into_iter().enumerate() {
        slog::debug!(logger, "Waiting for engine shutdown"; "replica" => i);

        engine
            .shutdown_and_wait(Duration::from_secs(5))
            .unwrap_or_else(|e| {
                slog::error!(
                    logger,
                    "Engine shutdown failed";
                    "replica" => i,
                    "error" => ?e,
                );
                panic!("Engine {} failed to shutdown: {}", i, e)
            });
    }

    slog::info!(logger, "All engines shut down successfully");

    // Phase 9: Verify state consistency (safety check - common prefix)
    slog::info!(logger, "Phase 9: Verifying state consistency");

    let mut all_replica_blocks: Vec<Vec<crate::state::block::Block>> = Vec::new();

    for (i, store) in stores.iter().enumerate() {
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

        // Check that we have finalized blocks (progress was made)
        assert!(
            !blocks.is_empty(),
            "Replica {} should have finalized blocks",
            i
        );

        // Check chain integrity
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
        }

        all_replica_blocks.push(blocks);
    }

    // Safety check: all replicas must agree on the common prefix.
    let min_len = all_replica_blocks.iter().map(|b| b.len()).min().unwrap();
    let max_len = all_replica_blocks.iter().map(|b| b.len()).max().unwrap();

    slog::info!(
        logger,
        "Checking common prefix";
        "min_chain_length" => min_len,
        "max_chain_length" => max_len,
    );

    let first_blocks = &all_replica_blocks[0];
    for (i, blocks) in all_replica_blocks.iter().enumerate().skip(1) {
        for j in 0..min_len {
            assert_eq!(
                blocks[j].get_hash(),
                first_blocks[j].get_hash(),
                "Block mismatch at index {} between replica {} and 0 (view {})",
                j,
                i,
                blocks[j].view()
            );
        }
    }

    slog::info!(logger, "State consistency verification passed! ✓");

    // Verify transaction inclusion
    let mut included_tx_hashes = std::collections::HashSet::new();
    let first_blocks = &all_replica_blocks[0];
    for block in first_blocks {
        for tx in &block.transactions {
            included_tx_hashes.insert(tx.tx_hash);
        }
    }

    let included_count = included_tx_hashes.intersection(&expected_tx_hashes).count();
    let inclusion_rate = included_count as f64 / expected_tx_hashes.len() as f64;

    slog::info!(
        logger,
        "Transaction inclusion check";
        "total_submitted" => expected_tx_hashes.len(),
        "included_finalized" => included_count,
        "inclusion_rate" => format!("{:.2}%", inclusion_rate * 100.0)
    );

    // We expect most transactions to be finalized (e.g. > 90% given 30s duration vs 400ms block
    // time)
    assert!(
        inclusion_rate > 0.9,
        "Transaction inclusion rate too low: {:.2}%",
        inclusion_rate * 100.0
    );

    // Final success message
    slog::info!(
        logger,
        "Test completed successfully! ✓";
        "total_messages_routed" => final_msgs_routed,
        "total_transactions" => tx_count,
        "finalized_blocks" => min_len,
        "test_duration_secs" => test_duration.as_secs(),
    );
}

#[test]
#[ignore] // Run with: cargo test --lib test_e2e_consensus_with_crashed_replica -- --ignored --nocapture
fn test_e2e_consensus_with_crashed_replica() {
    // Create logger for test
    let logger = create_test_logger();

    // The replica index that will be crashed (0-indexed, so replica 5 is the 6th)
    const CRASHED_REPLICA_IDX: usize = 5;

    slog::info!(
        logger,
        "Starting end-to-end consensus test (with crashed replica)";
        "replicas" => N,
        "byzantine_tolerance" => F,
        "crashed_replica" => CRASHED_REPLICA_IDX,
    );

    // Phase 1: Setup test environment
    slog::info!(logger, "Phase 1: Creating test fixture");
    let fixture = TestFixture::default();

    slog::info!(
        logger,
        "Generated keypairs and peer set";
        "total_replicas" => N,
        "peer_ids" => ?fixture.peer_set.sorted_peer_ids,
    );

    // Phase 2: Initialize network simulator
    slog::info!(logger, "Phase 2: Setting up network simulator");
    let mut network = LocalNetwork::<N, F, M_SIZE>::new();
    let mut replica_setups = Vec::with_capacity(N);

    let mut peer_id_to_secret_key = std::collections::HashMap::new();
    for kp in &fixture.keypairs {
        peer_id_to_secret_key.insert(kp.public_key.to_peer_id(), kp.secret_key.clone());
    }

    for (i, &peer_id) in fixture.peer_set.sorted_peer_ids.iter().enumerate() {
        let secret_key = peer_id_to_secret_key
            .get(&peer_id)
            .expect("Secret key not found")
            .clone();
        let setup = ReplicaSetup::new(peer_id, secret_key);

        slog::debug!(
            logger,
            "Created replica setup";
            "replica_index" => i,
            "peer_id" => peer_id,
        );

        replica_setups.push(setup);
    }

    // Phase 3: Register replicas and start engines, keeping transaction producers
    slog::info!(
        logger,
        "Phase 3: Registering replicas and starting consensus engines"
    );
    let mut engines = Vec::with_capacity(N);
    let mut transaction_producers = Vec::with_capacity(N);
    let mut stores = Vec::with_capacity(N);

    for (i, setup) in replica_setups.into_iter().enumerate() {
        let replica_id = setup.replica_id;

        // Keep transaction producer for later
        let tx_producer = setup.transaction_producer;

        // Keep a clone of the storage for verification
        stores.push(setup.storage.clone());

        // Register with network
        network.register_replica(replica_id, setup.message_producer, setup.broadcast_consumer);

        // Create consensus engine
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => replica_id));

        let engine = ConsensusEngine::<N, F, M_SIZE>::new(
            fixture.config.clone(),
            replica_id,
            setup.secret_key,
            setup.storage,
            setup.message_consumer,
            setup.broadcast_producer,
            setup.transaction_consumer,
            DEFAULT_TICK_INTERVAL,
            replica_logger,
        )
        .expect("Failed to create consensus engine");

        slog::debug!(
            logger,
            "Consensus engine started";
            "replica" => i,
            "peer_id" => replica_id,
        );

        engines.push(Some(engine));
        transaction_producers.push(tx_producer);
    }

    slog::info!(
        logger,
        "All replicas registered and engines started";
        "count" => engines.len(),
    );

    // Phase 4: Start network routing
    slog::info!(logger, "Phase 4: Starting network routing");
    network.start();
    assert!(network.is_running(), "Network should be running");
    slog::info!(logger, "Network routing thread active");

    // Phase 5: Crash replica 5 immediately (simulate Byzantine/crash fault)
    slog::info!(
        logger,
        "Phase 5: Crashing replica to simulate Byzantine fault";
        "crashed_replica" => CRASHED_REPLICA_IDX,
    );

    if let Some(crashed_engine) = engines[CRASHED_REPLICA_IDX].take() {
        crashed_engine.shutdown();
        crashed_engine
            .shutdown_and_wait(Duration::from_secs(5))
            .expect("Failed to shutdown crashed replica");
    }

    slog::info!(
        logger,
        "Replica crashed (shutdown)";
        "crashed_replica" => CRASHED_REPLICA_IDX,
    );

    // Phase 6: Submit transactions (only to healthy replicas)
    let num_transactions = 30;
    slog::info!(
        logger,
        "Phase 6: Submitting transactions";
        "count" => num_transactions,
    );

    let transactions = create_test_transactions(&fixture.keypairs, num_transactions);
    // Keep a copy of transaction hashes for verification
    let expected_tx_hashes: std::collections::HashSet<_> =
        transactions.iter().map(|tx| tx.tx_hash).collect();

    let transactions = create_test_transactions(&fixture.keypairs, num_transactions);

    for (i, tx) in transactions.into_iter().enumerate() {
        // Distribute transactions across healthy replicas only (skip crashed one)
        let mut replica_idx = i % N;
        if replica_idx == CRASHED_REPLICA_IDX {
            replica_idx = (replica_idx + 1) % N;
        }

        transaction_producers[replica_idx]
            .push(tx)
            .expect("Failed to submit transaction");

        slog::debug!(
            logger,
            "Transaction submitted";
            "tx_index" => i,
            "target_replica" => replica_idx,
        );
    }

    slog::info!(
        logger,
        "All transactions submitted";
        "total" => num_transactions,
    );

    // Phase 7: Allow consensus to progress through multiple views
    slog::info!(
        logger,
        "Phase 7: Waiting for consensus to progress (with one replica down)";
        "duration_secs" => 30,
    );

    // Wait and check progress periodically
    let test_duration = Duration::from_secs(30);
    let check_interval = Duration::from_secs(5);
    let start_time = std::time::Instant::now();

    while start_time.elapsed() < test_duration {
        thread::sleep(check_interval);

        let elapsed = start_time.elapsed().as_secs();
        let msgs_routed = network.stats.messages_routed();
        let msgs_dropped = network.stats.messages_dropped();

        slog::info!(
            logger,
            "Consensus progress check";
            "elapsed_secs" => elapsed,
            "messages_routed" => msgs_routed,
            "messages_dropped" => msgs_dropped,
            "healthy_replicas" => N - 1,
        );
    }

    // Phase 8: Verify healthy replicas are still running
    slog::info!(logger, "Phase 8: Verifying healthy replicas");

    for (i, engine_opt) in engines.iter().enumerate() {
        if i == CRASHED_REPLICA_IDX {
            assert!(
                engine_opt.is_none(),
                "Crashed replica {} should have been taken",
                i
            );
            slog::info!(
                logger,
                "Crashed replica confirmed down";
                "replica" => i,
            );
        } else if let Some(engine) = engine_opt {
            let is_running = engine.is_running();
            slog::info!(
                logger,
                "Healthy replica check";
                "replica" => i,
                "is_running" => is_running,
            );
            assert!(is_running, "Engine {} should still be running", i);
        }
    }

    // Phase 9: Collect final statistics
    slog::info!(logger, "Phase 9: Collecting final statistics");

    let final_msgs_routed = network.stats.messages_routed();
    let final_msgs_dropped = network.stats.messages_dropped();
    let drop_rate = if final_msgs_routed > 0 {
        (final_msgs_dropped as f64 / (final_msgs_routed + final_msgs_dropped) as f64) * 100.0
    } else {
        0.0
    };

    slog::info!(
        logger,
        "Final network statistics";
        "messages_routed" => final_msgs_routed,
        "messages_dropped" => final_msgs_dropped,
        "drop_rate_percent" => format!("{:.2}", drop_rate),
    );

    // Assert reasonable performance
    assert!(final_msgs_routed > 0, "Network should have routed messages");

    // Phase 10: Graceful shutdown of remaining engines
    slog::info!(logger, "Phase 10: Shutting down healthy consensus engines");

    // 1. Signal all healthy engines to stop
    for engine in engines.iter().flatten() {
        engine.shutdown();
    }

    // 2. Stop the network
    network.shutdown();

    // 3. Wait for each healthy engine to finish
    for (i, engine_opt) in engines.into_iter().enumerate() {
        if let Some(engine) = engine_opt {
            slog::debug!(logger, "Waiting for engine shutdown"; "replica" => i);

            engine
                .shutdown_and_wait(Duration::from_secs(5))
                .unwrap_or_else(|e| {
                    slog::error!(
                        logger,
                        "Engine shutdown failed";
                        "replica" => i,
                        "error" => ?e,
                    );
                    panic!("Engine {} failed to shutdown: {}", i, e)
                });
        }
    }

    slog::info!(logger, "All healthy engines shut down successfully");

    // Phase 11: Verify state consistency among healthy replicas
    slog::info!(
        logger,
        "Phase 11: Verifying state consistency among healthy replicas"
    );

    let mut first_healthy_blocks: Option<Vec<crate::state::block::Block>> = None;
    let mut healthy_replica_blocks: Vec<(usize, Vec<crate::state::block::Block>)> = Vec::new();

    for (i, store) in stores.iter().enumerate() {
        if i == CRASHED_REPLICA_IDX {
            // Skip the crashed replica - it may have incomplete state
            slog::info!(
                logger,
                "Skipping crashed replica in consistency check";
                "replica" => i,
            );
            continue;
        }

        // Retrieve all finalized blocks from the store
        let blocks = store
            .get_all_finalized_blocks()
            .expect("Failed to get finalized blocks from store");

        slog::info!(
            logger,
            "Healthy replica state check";
            "replica" => i,
            "finalized_blocks" => blocks.len(),
            "highest_view" => blocks.last().map(|b| b.view()).unwrap_or(0),
        );

        // 1. Check that we have finalized blocks (progress was made despite crashed replica)
        assert!(
            !blocks.is_empty(),
            "Healthy replica {} should have finalized blocks (BFT should make progress with {} replicas)",
            i,
            N - 1
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
            assert!(
                curr.view() > prev.view(),
                "View should increase monotonically ({} -> {})",
                prev.view(),
                curr.view()
            );

            // Verify that skipped views were properly nullified
            for skipped_view in (prev.view() + 1)..curr.view() {
                let nullification = store
                    .get_nullification::<N, F, M_SIZE>(skipped_view)
                    .expect("Failed to query nullification");

                assert!(
                    nullification.is_some(),
                    "Skipped view {} should be nullified (replica {})",
                    skipped_view,
                    i
                );

                slog::info!(
                    logger,
                    "Verified nullification";
                    "view" => skipped_view,
                    "replica" => i,
                );
            }
        }

        healthy_replica_blocks.push((i, blocks.clone()));

        // 3. Check consistency across healthy replicas
        if let Some(ref first_blocks) = first_healthy_blocks {
            assert_eq!(
                blocks.len(),
                first_blocks.len(),
                "Healthy replica {} has different number of blocks than first healthy replica",
                i
            );
            for (j, (b1, b2)) in blocks.iter().zip(first_blocks.iter()).enumerate() {
                assert_eq!(
                    b1.get_hash(),
                    b2.get_hash(),
                    "Block mismatch at index {} between healthy replicas (view {})",
                    j,
                    b1.view()
                );
            }
        } else {
            first_healthy_blocks = Some(blocks);
        }
    }

    let finalized_count = first_healthy_blocks.as_ref().map(|b| b.len()).unwrap_or(0);

    slog::info!(
        logger,
        "State consistency verification passed! ✓";
        "healthy_replicas" => N - 1,
        "finalized_blocks" => finalized_count,
    );

    // Verify all transactions were included
    let mut included_tx_hashes = std::collections::HashSet::new();
    if let Some(ref blocks) = first_healthy_blocks {
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
        "total_messages_routed" => final_msgs_routed,
        "test_duration_secs" => 30,
        "crashed_replica" => CRASHED_REPLICA_IDX,
        "healthy_replicas" => N - 1,
        "finalized_blocks" => finalized_count,
        "bft_assumption" => "n >= 5f + 1 (6 >= 6)",
    );
}

#[test]
#[ignore] // Run with: cargo test --lib test_e2e_consensus_with_equivocating_leader -- --ignored --nocapture
fn test_e2e_consensus_with_equivocating_leader() {
    // Create logger for test
    let logger = create_test_logger();

    // The replica index that will be faulty (leader of View 1)
    const FAULTY_REPLICA_IDX: usize = 0;

    slog::info!(
        logger,
        "Starting end-to-end consensus test (equivocating leader)";
        "replicas" => N,
        "byzantine_tolerance" => F,
        "faulty_replica" => FAULTY_REPLICA_IDX,
    );

    // Phase 1: Setup test environment
    let fixture = TestFixture::default();

    // Phase 2: Initialize network simulator
    let mut network = LocalNetwork::<N, F, M_SIZE>::new();
    let mut replica_setups = Vec::with_capacity(N);

    let mut peer_id_to_secret_key = std::collections::HashMap::new();
    for kp in &fixture.keypairs {
        peer_id_to_secret_key.insert(kp.public_key.to_peer_id(), kp.secret_key.clone());
    }

    for peer_id in fixture.peer_set.sorted_peer_ids.iter() {
        let secret_key = peer_id_to_secret_key
            .get(peer_id)
            .expect("Secret key not found")
            .clone();
        let setup = ReplicaSetup::new(*peer_id, secret_key);
        replica_setups.push(setup);
    }

    // Phase 3: Register replicas and start engines (except faulty one)
    let mut engines = Vec::with_capacity(N);
    let mut transaction_producers = Vec::with_capacity(N);
    let mut stores = Vec::with_capacity(N);

    // Keep peer IDs for targeting
    let peer_ids: Vec<crate::crypto::aggregated::PeerId> =
        replica_setups.iter().map(|s| s.replica_id).collect();

    for (i, setup) in replica_setups.into_iter().enumerate() {
        let replica_id = setup.replica_id;
        let tx_producer = setup.transaction_producer;
        stores.push(setup.storage.clone());

        network.register_replica(replica_id, setup.message_producer, setup.broadcast_consumer);

        let replica_logger = logger.new(o!("replica" => i, "peer_id" => replica_id));

        // Do NOT start engine for faulty replica
        if i == FAULTY_REPLICA_IDX {
            slog::info!(logger, "Skipping engine start for faulty replica"; "replica" => i);
            engines.push(None);
            transaction_producers.push(tx_producer);
            continue;
        }

        let engine = ConsensusEngine::<N, F, M_SIZE>::new(
            fixture.config.clone(),
            replica_id,
            setup.secret_key,
            setup.storage,
            setup.message_consumer,
            setup.broadcast_producer,
            setup.transaction_consumer,
            DEFAULT_TICK_INTERVAL,
            replica_logger,
        )
        .expect("Failed to create consensus engine");

        engines.push(Some(engine));
        transaction_producers.push(tx_producer);
    }

    // Phase 4: Start network
    network.start();

    // Phase 5: Inject equivocation (conflicting block proposals)
    slog::info!(logger, "Phase 5: Injecting conflicting blocks for View 1");

    let view = 1;
    let faulty_peer_id = fixture.peer_set.sorted_peer_ids[FAULTY_REPLICA_IDX];
    let faulty_sk = peer_id_to_secret_key.get(&faulty_peer_id).unwrap();
    let parent_hash = crate::state::block::Block::genesis_hash();

    // Block A
    let block_a = create_test_block(view, faulty_peer_id, parent_hash, faulty_sk.clone(), 100);
    // Block B (different payload -> different hash)
    let block_b = create_test_block(view, faulty_peer_id, parent_hash, faulty_sk.clone(), 200);

    assert_ne!(
        block_a.get_hash(),
        block_b.get_hash(),
        "Blocks must have different hashes"
    );

    // Split votes: 2 for A, 2 for B. Neither reaches M=3.
    // Send Block A to replicas 1, 2
    for &peer_id in &peer_ids[1..=2] {
        slog::info!(logger, "Sending Block A to replica"; "replica" => peer_id);
        network
            .inject_message(
                peer_id,
                crate::consensus::ConsensusMessage::BlockProposal(block_a.clone()),
            )
            .unwrap();
    }

    // Send Block B to replicas 3, 4
    for &peer_id in &peer_ids[3..=4] {
        slog::info!(logger, "Sending Block B to replica"; "replica" => peer_id);
        network
            .inject_message(
                peer_id,
                crate::consensus::ConsensusMessage::BlockProposal(block_b.clone()),
            )
            .unwrap();
    }

    // Replica 5 gets nothing (or gets one of them, doesn't matter much if split is 2-2-1)

    // Phase 6: Wait for progress
    // Expect View 1 to timeout and nullify
    let test_duration = Duration::from_secs(30);
    let check_interval = Duration::from_secs(5);
    let start_time = std::time::Instant::now();

    while start_time.elapsed() < test_duration {
        thread::sleep(check_interval);
        let msgs_routed = network.stats.messages_routed();
        slog::info!(logger, "Consensus progress check"; "messages_routed" => msgs_routed);
    }

    // Phase 7: Shutdown
    for engine in engines.iter().flatten() {
        engine.shutdown();
    }

    network.shutdown();

    for engine in engines.into_iter().flatten() {
        engine.shutdown_and_wait(Duration::from_secs(5)).unwrap();
    }

    // Phase 8: Verify state
    slog::info!(logger, "Phase 8: Verifying state consistency");

    // We expect healthy replicas to have moved past View 1 via nullification
    for (i, store) in stores.iter().enumerate() {
        if i == FAULTY_REPLICA_IDX {
            continue;
        }

        let nullification = store
            .get_nullification::<N, F, M_SIZE>(view)
            .expect("Failed to query nullification");

        assert!(
            nullification.is_some(),
            "View {} should have been nullified for replica {} due to equivocation/split votes",
            view,
            i
        );
        slog::info!(logger, "Verified nullification for view 1"; "replica" => i);

        // Check they progressed further (View > 1)
        let blocks = store.get_all_finalized_blocks().unwrap();
        if let Some(last_block) = blocks.last() {
            assert!(
                last_block.view() > 1,
                "Replica should have progressed past view 1"
            );
        }
    }
}
