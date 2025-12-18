//! End-to-End Consensus Integration Tests
//!
//! This module contains integration tests that verify the correctness
//! of the Minimmit BFT consensus protocol by simulating networks of replicas.

use super::{network_simulator::LocalNetwork, test_helpers::*};
use crate::{
    consensus_manager::{config::GenesisAccount, consensus_engine::ConsensusEngine},
    crypto::transaction_crypto::{TxPublicKey, TxSecretKey},
    state::{address::Address, transaction::Transaction},
};
use slog::{Drain, Level, Logger, o};
use std::{
    collections::{HashMap, HashSet},
    env,
    str::FromStr,
    sync::Arc,
    thread,
    time::Duration,
};

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

    // Phase 1: Setup test environment with funded accounts
    slog::info!(
        logger,
        "Phase 1: Creating test fixture with funded genesis accounts"
    );

    // Create funded transactions FIRST (we need genesis accounts for the fixture)
    let num_transactions = 30;
    let (transactions, genesis_accounts) = create_funded_test_transactions(num_transactions);

    let fixture = TestFixture::with_genesis_accounts(genesis_accounts);

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
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => peer_id));
        let setup = ReplicaSetup::new(peer_id, secret_key, replica_logger);

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
    let mut validation_services = Vec::with_capacity(N);
    let mut mempool_services = Vec::with_capacity(N);
    let mut stores = Vec::with_capacity(N);

    for (i, setup) in replica_setups.into_iter().enumerate() {
        let replica_id = setup.replica_id;

        // Keep a clone of the storage for verification
        stores.push(setup.storage.clone());

        // Register with network - routes BlockProposal to Block Validator, others to Consensus
        network.register_replica(
            replica_id,
            setup.message_producer,
            setup.block_producer,
            setup.broadcast_consumer,
        );

        // Create consensus engine
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => replica_id));

        let engine = ConsensusEngine::<N, F, M_SIZE>::new(
            fixture.config.clone(),
            replica_id,
            setup.secret_key,
            setup.message_consumer,
            setup.broadcast_producer,
            setup.validation_result_consumer,
            setup.proposal_req_producer,
            setup.proposal_resp_consumer,
            setup.finalized_producer,
            setup.persistence_writer,
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
        transaction_producers.push(setup.transaction_producer);
        validation_services.push(setup.validation_service);
        mempool_services.push(setup.mempool_service);
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

    // Phase 5: Submit transactions (already created with funded accounts)
    slog::info!(
        logger,
        "Phase 5: Submitting transactions";
        "count" => num_transactions,
    );

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

    // 3. Shutdown validation services
    for mut service in validation_services {
        service.shutdown();
    }

    // 4. Shutdown mempool services
    for mut service in mempool_services {
        service.shutdown();
    }

    // 5. Now wait for each engine to finish its thread.
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
            assert_eq!(
                blocks.len(),
                first_blocks.len(),
                "Replica {} has different number of blocks than replica 0",
                i
            );
            for (j, (b1, b2)) in blocks.iter().zip(first_blocks.iter()).enumerate() {
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

    // Phase 1: Setup test environment with funded accounts
    // Pre-allocate a large pool of funded transactions (enough for the test duration)
    // Assuming ~5 txs every 100ms for 60 seconds = ~3000 txs, we allocate more to be safe
    let max_transactions = 5000;
    let (mut all_transactions, genesis_accounts) =
        create_funded_test_transactions(max_transactions);

    slog::info!(
        logger,
        "Phase 1: Creating test fixture with funded genesis accounts"
    );
    let fixture = TestFixture::with_genesis_accounts(genesis_accounts);

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
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => peer_id));
        let setup = ReplicaSetup::new(peer_id, secret_key, replica_logger);

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
    let mut validation_services = Vec::with_capacity(N);
    let mut mempool_services = Vec::with_capacity(N);
    let mut stores = Vec::with_capacity(N);

    for (i, setup) in replica_setups.into_iter().enumerate() {
        let replica_id = setup.replica_id;

        // Keep transaction producer for later
        let tx_producer = setup.transaction_producer;

        // Keep a clone of the storage for verification
        stores.push(setup.storage.clone());

        // Register with network - routes BlockProposal to Block Validator, others to Consensus
        network.register_replica(
            replica_id,
            setup.message_producer,
            setup.block_producer,
            setup.broadcast_consumer,
        );

        // Create consensus engine
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => replica_id));

        let engine = ConsensusEngine::<N, F, M_SIZE>::new(
            fixture.config.clone(),
            replica_id,
            setup.secret_key,
            setup.message_consumer,
            setup.broadcast_producer,
            setup.validation_result_consumer,
            setup.proposal_req_producer,
            setup.proposal_resp_consumer,
            setup.finalized_producer,
            setup.persistence_writer,
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
        validation_services.push(setup.validation_service);
        mempool_services.push(setup.mempool_service);
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

    while start_time.elapsed() < test_duration && !all_transactions.is_empty() {
        // Submit a batch of transactions from pre-allocated pool
        let batch_size = std::cmp::min(5, all_transactions.len());

        for _ in 0..batch_size {
            if let Some(tx) = all_transactions.pop() {
                let tx_hash = tx.tx_hash;
                // Distribute transactions across replicas (round-robin)
                let replica_idx = tx_index % N;
                tx_index += 1;

                if transaction_producers[replica_idx].push(tx).is_ok() {
                    tx_count += 1;
                    expected_tx_hashes.insert(tx_hash);
                }
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
                "remaining_txs" => all_transactions.len(),
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

    // 3. Shutdown validation services
    for mut service in validation_services {
        service.shutdown();
    }

    // 4. Shutdown mempool services
    for mut service in mempool_services {
        service.shutdown();
    }

    // 5. Now wait for each engine to finish its thread.
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

    // Phase 1: Setup test environment with funded accounts
    slog::info!(
        logger,
        "Phase 1: Creating test fixture with funded genesis accounts"
    );

    // Create funded transactions FIRST (we need genesis accounts for the fixture)
    let num_transactions = 30;
    let (transactions, genesis_accounts) = create_funded_test_transactions(num_transactions);

    let fixture = TestFixture::with_genesis_accounts(genesis_accounts);

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
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => peer_id));
        let setup = ReplicaSetup::new(peer_id, secret_key, replica_logger);

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
    let mut validation_services: Vec<Option<_>> = Vec::with_capacity(N);
    let mut mempool_services: Vec<Option<_>> = Vec::with_capacity(N);
    let mut stores = Vec::with_capacity(N);

    for (i, setup) in replica_setups.into_iter().enumerate() {
        let replica_id = setup.replica_id;

        // Keep transaction producer for later
        let tx_producer = setup.transaction_producer;

        // Keep a clone of the storage for verification
        stores.push(setup.storage.clone());

        // Register with network - routes BlockProposal to Block Validator, others to Consensus
        network.register_replica(
            replica_id,
            setup.message_producer,
            setup.block_producer,
            setup.broadcast_consumer,
        );

        // Create consensus engine
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => replica_id));

        let engine = ConsensusEngine::<N, F, M_SIZE>::new(
            fixture.config.clone(),
            replica_id,
            setup.secret_key,
            setup.message_consumer,
            setup.broadcast_producer,
            setup.validation_result_consumer,
            setup.proposal_req_producer,
            setup.proposal_resp_consumer,
            setup.finalized_producer,
            setup.persistence_writer,
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
        validation_services.push(Some(setup.validation_service));
        mempool_services.push(Some(setup.mempool_service));
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
    slog::info!(
        logger,
        "Phase 6: Submitting transactions";
        "count" => num_transactions,
    );

    // Keep a copy of transaction hashes for verification
    let expected_tx_hashes: std::collections::HashSet<_> =
        transactions.iter().map(|tx| tx.tx_hash).collect();

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

    // 3. Shutdown validation services
    for mut service in validation_services.into_iter().flatten() {
        service.shutdown();
    }

    // 4. Shutdown mempool services
    for mut service in mempool_services.into_iter().flatten() {
        service.shutdown();
    }

    // 5. Wait for each healthy engine to finish
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

    // The replica index that will act as a Byzantine equivocating leader
    // Replica 1 is the leader for view 1 (round-robin: view % N)
    const BYZANTINE_LEADER_IDX: usize = 1;

    slog::info!(
        logger,
        "Starting end-to-end consensus test (equivocating leader)";
        "replicas" => N,
        "byzantine_tolerance" => F,
        "byzantine_leader" => BYZANTINE_LEADER_IDX,
        "target_view" => 1,
    );

    // Phase 1: Setup test environment with funded accounts
    slog::info!(
        logger,
        "Phase 1: Creating test fixture with funded genesis accounts"
    );

    // Create funded transactions FIRST (we need genesis accounts for the fixture)
    let num_transactions = 30;
    let (transactions, genesis_accounts) = create_funded_test_transactions(num_transactions);

    let fixture = TestFixture::with_genesis_accounts(genesis_accounts);

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
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => peer_id));
        let setup = ReplicaSetup::new(peer_id, secret_key, replica_logger);

        slog::debug!(
            logger,
            "Created replica setup";
            "replica_index" => i,
            "peer_id" => peer_id,
        );

        replica_setups.push(setup);
    }

    // Phase 3: Register replicas and start engines
    // IMPORTANT: We do NOT start the Byzantine leader - we'll manually inject its messages
    slog::info!(
        logger,
        "Phase 3: Registering replicas and starting consensus engines (excluding Byzantine leader)"
    );
    let mut engines = Vec::with_capacity(N);
    let mut transaction_producers = Vec::with_capacity(N);
    let mut validation_services: Vec<Option<_>> = Vec::with_capacity(N);
    let mut mempool_services: Vec<Option<_>> = Vec::with_capacity(N);
    let mut stores = Vec::with_capacity(N);

    // Store the Byzantine leader's secret key for signing blocks
    let mut byzantine_leader_secret_key: Option<crate::crypto::aggregated::BlsSecretKey> = None;
    let mut byzantine_leader_peer_id: Option<u64> = None;

    for (i, setup) in replica_setups.into_iter().enumerate() {
        let replica_id = setup.replica_id;

        // Keep a clone of the storage for verification
        stores.push(setup.storage.clone());

        if i == BYZANTINE_LEADER_IDX {
            // Don't start the Byzantine leader's engine - we'll manually control it
            // But we DO register it with the network so other replicas can send to it
            // (even though it won't process anything)
            byzantine_leader_peer_id = Some(replica_id);
            byzantine_leader_secret_key = Some(setup.secret_key.clone());

            // Register Byzantine leader with network (so messages can be routed)
            network.register_replica(
                replica_id,
                setup.message_producer,
                setup.block_producer,
                setup.broadcast_consumer,
            );

            engines.push(None);
            transaction_producers.push(None);
            validation_services.push(Some(setup.validation_service));
            mempool_services.push(Some(setup.mempool_service));

            slog::info!(
                logger,
                "Byzantine leader registered (engine NOT started)";
                "replica" => i,
                "peer_id" => replica_id,
            );
            continue;
        }

        // Keep transaction producer for later
        let tx_producer = setup.transaction_producer;

        // Register with network - routes BlockProposal to Block Validator, others to Consensus
        network.register_replica(
            replica_id,
            setup.message_producer,
            setup.block_producer,
            setup.broadcast_consumer,
        );

        // Create consensus engine
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => replica_id));

        let engine = ConsensusEngine::<N, F, M_SIZE>::new(
            fixture.config.clone(),
            replica_id,
            setup.secret_key,
            setup.message_consumer,
            setup.broadcast_producer,
            setup.validation_result_consumer,
            setup.proposal_req_producer,
            setup.proposal_resp_consumer,
            setup.finalized_producer,
            setup.persistence_writer,
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
        transaction_producers.push(Some(tx_producer));
        validation_services.push(Some(setup.validation_service));
        mempool_services.push(Some(setup.mempool_service));
    }

    let byzantine_secret_key =
        byzantine_leader_secret_key.expect("Byzantine leader secret key should exist");
    let byzantine_peer_id = byzantine_leader_peer_id.expect("Byzantine peer ID should exist");

    slog::info!(
        logger,
        "Honest replicas registered and engines started";
        "honest_count" => N - 1,
        "byzantine_leader_peer_id" => byzantine_peer_id,
    );

    // Phase 4: Start network routing
    slog::info!(logger, "Phase 4: Starting network routing");
    network.start();
    assert!(network.is_running(), "Network should be running");
    slog::info!(logger, "Network routing thread active");

    // Give honest replicas a moment to initialize
    thread::sleep(Duration::from_millis(100));

    // Phase 5: Inject equivocating blocks from Byzantine leader
    // The Byzantine leader will propose TWO DIFFERENT blocks for view 1 to different replicas
    slog::info!(
        logger,
        "Phase 5: Injecting equivocating blocks from Byzantine leader";
        "target_view" => 1,
    );

    // Create two different blocks for the same view (equivocation)
    use crate::state::block::Block;
    use crate::state::transaction::Transaction;

    let parent_hash = Block::genesis_hash();
    let view_number = 1u64;

    // Create two different transactions to make the blocks different
    let tx1 = {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        Transaction::new_transfer(
            Address::from_public_key(&pk),
            Address::from_bytes([1u8; 32]),
            1,
            1,
            1000,
            &sk,
        )
    };

    let tx2 = {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        Transaction::new_transfer(
            Address::from_public_key(&pk),
            Address::from_bytes([2u8; 32]),
            2,
            2,
            2000,
            &sk,
        )
    };

    // Create Block 1
    let temp_block1 = Block::new(
        view_number,
        byzantine_peer_id,
        parent_hash,
        vec![Arc::new(tx1)],
        1234567890,
        byzantine_secret_key.sign(b"temp1"),
        false,
        1,
    );
    let block1_hash = temp_block1.get_hash();
    let block1 = Block::new(
        view_number,
        byzantine_peer_id,
        parent_hash,
        temp_block1.transactions.clone(),
        1234567890,
        byzantine_secret_key.sign(&block1_hash),
        false,
        1,
    );

    // Create Block 2 (different transactions, same view - EQUIVOCATION!)
    let temp_block2 = Block::new(
        view_number,
        byzantine_peer_id,
        parent_hash,
        vec![Arc::new(tx2)],
        1234567891, // Different timestamp
        byzantine_secret_key.sign(b"temp2"),
        false,
        1,
    );
    let block2_hash = temp_block2.get_hash();
    let block2 = Block::new(
        view_number,
        byzantine_peer_id,
        parent_hash,
        temp_block2.transactions.clone(),
        1234567891,
        byzantine_secret_key.sign(&block2_hash),
        false,
        1,
    );

    assert_ne!(
        block1.get_hash(),
        block2.get_hash(),
        "Equivocating blocks should have different hashes"
    );

    slog::info!(
        logger,
        "Created equivocating blocks";
        "block1_hash" => hex::encode(&block1.get_hash()[..8]),
        "block2_hash" => hex::encode(&block2.get_hash()[..8]),
    );

    // Inject equivocating blocks directly to specific replicas via the network's message producers
    // Block 1 -> replicas 0, 2, 3 (partition A)
    // Block 2 -> replicas 4, 5 (partition B)
    // Note: Replica 1 is the Byzantine leader, so we skip it
    use crate::consensus::ConsensusMessage;

    let msg1 = ConsensusMessage::BlockProposal(block1.clone());
    let msg2 = ConsensusMessage::BlockProposal(block2.clone());

    // Access the network's message producers directly to inject messages to specific replicas
    {
        let mut producers = network.message_producers.lock().unwrap();

        for (i, &target_peer_id) in fixture.peer_set.sorted_peer_ids.iter().enumerate() {
            if i == BYZANTINE_LEADER_IDX {
                continue; // Skip Byzantine leader itself
            }

            // Determine which block to send based on partition
            // Partition A (replicas 0, 2, 3) gets block1
            // Partition B (replicas 4, 5) gets block2
            let (msg, block_name) = if i == 0 || i == 2 || i == 3 {
                (msg1.clone(), "block1")
            } else {
                (msg2.clone(), "block2")
            };

            if let Some(producer) = producers.get_mut(&target_peer_id) {
                match producer.push(msg) {
                    Ok(_) => {
                        slog::info!(
                            logger,
                            "Injected equivocating block to replica";
                            "target_replica" => i,
                            "target_peer_id" => target_peer_id,
                            "block" => block_name,
                        );
                    }
                    Err(e) => {
                        slog::error!(
                            logger,
                            "Failed to inject equivocating block";
                            "target_replica" => i,
                            "error" => ?e,
                        );
                        panic!(
                            "Failed to inject equivocating block to replica {}: {:?}",
                            i, e
                        );
                    }
                }
            } else {
                slog::warn!(
                    logger,
                    "No producer found for replica";
                    "target_replica" => i,
                    "target_peer_id" => target_peer_id,
                );
            }
        }
    }

    slog::info!(
        logger,
        "Equivocating blocks injected";
        "partition_a" => "replicas 0, 2, 3 received block1",
        "partition_b" => "replicas 4, 5 received block2",
    );

    // Phase 6: Submit transactions to honest replicas (already created with funded accounts)
    slog::info!(
        logger,
        "Phase 6: Submitting transactions to honest replicas";
        "count" => num_transactions,
    );

    let expected_tx_hashes: std::collections::HashSet<_> =
        transactions.iter().map(|tx| tx.tx_hash).collect();

    for (i, tx) in transactions.into_iter().enumerate() {
        // Distribute transactions across honest replicas only (skip Byzantine leader)
        let mut replica_idx = i % N;
        if replica_idx == BYZANTINE_LEADER_IDX {
            replica_idx = (replica_idx + 1) % N;
        }

        if let Some(ref mut tx_producer) = transaction_producers[replica_idx] {
            tx_producer.push(tx).expect("Failed to submit transaction");

            slog::debug!(
                logger,
                "Transaction submitted";
                "tx_index" => i,
                "target_replica" => replica_idx,
            );
        }
    }

    slog::info!(
        logger,
        "All transactions submitted";
        "total" => num_transactions,
    );

    // Phase 7: Allow consensus to progress - honest replicas should detect equivocation
    // and nullify view 1, then continue making progress
    slog::info!(
        logger,
        "Phase 7: Waiting for consensus to detect equivocation and recover";
        "duration_secs" => 45,
    );

    let test_duration = Duration::from_secs(45);
    let check_interval = Duration::from_secs(5);
    let start_time = std::time::Instant::now();

    while start_time.elapsed() < test_duration {
        thread::sleep(check_interval);

        let elapsed = start_time.elapsed().as_secs();
        let msgs_routed = network.stats.messages_routed();
        let msgs_dropped = network.stats.messages_dropped();

        slog::info!(
            logger,
            "Consensus progress check (equivocation recovery)";
            "elapsed_secs" => elapsed,
            "messages_routed" => msgs_routed,
            "messages_dropped" => msgs_dropped,
        );
    }

    // Phase 8: Verify honest replicas are still running
    slog::info!(logger, "Phase 8: Verifying honest replicas");

    for (i, engine_opt) in engines.iter().enumerate() {
        if i == BYZANTINE_LEADER_IDX {
            assert!(
                engine_opt.is_none(),
                "Byzantine leader {} should not have an engine",
                i
            );
            continue;
        }

        if let Some(engine) = engine_opt {
            let is_running = engine.is_running();
            slog::info!(
                logger,
                "Honest replica check";
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

    slog::info!(
        logger,
        "Final network statistics";
        "messages_routed" => final_msgs_routed,
        "messages_dropped" => final_msgs_dropped,
    );

    assert!(final_msgs_routed > 0, "Network should have routed messages");

    // Phase 10: Graceful shutdown
    slog::info!(logger, "Phase 10: Shutting down consensus engines");

    for engine in engines.iter().flatten() {
        engine.shutdown();
    }

    network.shutdown();

    // Shutdown validation services
    for mut service in validation_services.into_iter().flatten() {
        service.shutdown();
    }

    // Shutdown mempool services
    for mut service in mempool_services.into_iter().flatten() {
        service.shutdown();
    }

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

    slog::info!(logger, "All engines shut down successfully");

    // Phase 11: Verify state consistency and that views 1 and 2 were nullified
    slog::info!(
        logger,
        "Phase 11: Verifying state consistency and equivocation handling"
    );

    let mut first_honest_blocks: Option<Vec<crate::state::block::Block>> = None;
    let mut view_1_nullified_count = 0;
    let mut view_2_nullified_count = 0;

    for (i, store) in stores.iter().enumerate() {
        if i == BYZANTINE_LEADER_IDX {
            slog::info!(
                logger,
                "Skipping Byzantine leader in consistency check";
                "replica" => i,
            );
            continue;
        }

        let blocks = store
            .get_all_finalized_blocks()
            .expect("Failed to get finalized blocks from store");

        slog::info!(
            logger,
            "Honest replica state check";
            "replica" => i,
            "finalized_blocks" => blocks.len(),
            "highest_view" => blocks.last().map(|b| b.view()).unwrap_or(0),
            "first_finalized_view" => blocks.first().map(|b| b.view()).unwrap_or(0),
        );

        // Check that we have finalized blocks (progress was made despite Byzantine leader)
        assert!(
            !blocks.is_empty(),
            "Honest replica {} should have finalized blocks despite equivocating leader",
            i
        );

        // Check if view 1 was nullified (expected behavior for equivocation)
        let view_1_block = blocks.iter().find(|b| b.view() == 1);
        if view_1_block.is_none() {
            let nullification = store
                .get_nullification::<N, F, M_SIZE>(1)
                .expect("Failed to query nullification for view 1");

            if nullification.is_some() {
                view_1_nullified_count += 1;
                slog::info!(
                    logger,
                    "View 1 was correctly nullified due to equivocation";
                    "replica" => i,
                );
            }
        } else {
            slog::warn!(
                logger,
                "View 1 was finalized (unexpected - one partition got quorum?)";
                "replica" => i,
                "view_1_block_hash" => hex::encode(&view_1_block.unwrap().get_hash()[..8]),
            );
            panic!("View 1 was finalized (unexpected - one partition got quorum?)");
        }

        // Check if view 2 was also nullified (cascading nullification)
        let view_2_block = blocks.iter().find(|b| b.view() == 2);
        if view_2_block.is_none() {
            let nullification_v2 = store
                .get_nullification::<N, F, M_SIZE>(2)
                .expect("Failed to query nullification for view 2");

            if nullification_v2.is_some() {
                view_2_nullified_count += 1;
                slog::info!(
                    logger,
                    "View 2 was correctly nullified due to cascading nullification";
                    "replica" => i,
                );
            }
        } else {
            slog::warn!(
                logger,
                "View 2 was finalized (unexpected - should have been cascade nullified)";
                "replica" => i,
                "view_2_block_hash" => hex::encode(&view_2_block.unwrap().get_hash()[..8]),
            );
            panic!("View 2 was finalized (unexpected - should have been cascade nullified)");
        }

        // Verify first finalized non-genesis block starts from view 3 or later
        // (views 1 and 2 should have been nullified)
        let first_non_genesis_block = blocks.iter().find(|b| b.view() > 0);
        if let Some(first_block) = first_non_genesis_block {
            assert!(
                first_block.view() >= 3,
                "First non-genesis finalized block for replica {} should be view 3 or later (views 1-2 nullified), got view {}",
                i,
                first_block.view()
            );
            slog::info!(
                logger,
                "First non-genesis finalized block is at expected view";
                "replica" => i,
                "first_view" => first_block.view(),
            );
        }

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

        // Check consistency across honest replicas
        if let Some(ref first_blocks) = first_honest_blocks {
            // Use common prefix check since replicas might be at different heights
            let min_len = std::cmp::min(blocks.len(), first_blocks.len());
            for j in 0..min_len {
                assert_eq!(
                    blocks[j].get_hash(),
                    first_blocks[j].get_hash(),
                    "Block mismatch at index {} between honest replicas (view {})",
                    j,
                    blocks[j].view()
                );
            }
            slog::info!(
                logger,
                "Cross-replica consistency verified";
                "replica" => i,
                "compared_blocks" => min_len,
            );
        } else {
            first_honest_blocks = Some(blocks);
        }
    }

    let finalized_count = first_honest_blocks.as_ref().map(|b| b.len()).unwrap_or(0);
    let honest_replica_count = N - 1;

    // Verify ALL honest replicas nullified view 1
    assert_eq!(
        view_1_nullified_count, honest_replica_count,
        "All {} honest replicas should have nullified view 1, but only {} did",
        honest_replica_count, view_1_nullified_count
    );

    // Verify ALL honest replicas nullified view 2 (cascading)
    assert_eq!(
        view_2_nullified_count, honest_replica_count,
        "All {} honest replicas should have nullified view 2 (cascading), but only {} did",
        honest_replica_count, view_2_nullified_count
    );

    slog::info!(
        logger,
        "State consistency verification passed! ✓";
        "honest_replicas" => honest_replica_count,
        "finalized_blocks" => finalized_count,
        "view_1_nullified_all" => view_1_nullified_count == honest_replica_count,
        "view_2_nullified_all" => view_2_nullified_count == honest_replica_count,
    );

    // Verify transaction inclusion (may be lower due to equivocation handling)
    let mut included_tx_hashes = std::collections::HashSet::new();
    if let Some(ref blocks) = first_honest_blocks {
        for block in blocks {
            for tx in &block.transactions {
                included_tx_hashes.insert(tx.tx_hash);
            }
        }
    }

    let included_count = included_tx_hashes.intersection(&expected_tx_hashes).count();
    let inclusion_rate = if expected_tx_hashes.is_empty() {
        1.0
    } else {
        included_count as f64 / expected_tx_hashes.len() as f64
    };

    slog::info!(
        logger,
        "Transaction inclusion check";
        "total_submitted" => expected_tx_hashes.len(),
        "included_finalized" => included_count,
        "inclusion_rate" => format!("{:.2}%", inclusion_rate * 100.0)
    );

    // With equivocation, we expect lower inclusion rate but still reasonable progress
    assert!(
        inclusion_rate > 0.5,
        "Transaction inclusion rate too low after equivocation: {:.2}%",
        inclusion_rate * 100.0
    );

    // Final success message
    slog::info!(
        logger,
        "Test completed successfully! ✓";
        "total_messages_routed" => final_msgs_routed,
        "test_duration_secs" => 45,
        "byzantine_leader" => BYZANTINE_LEADER_IDX,
        "honest_replicas" => honest_replica_count,
        "finalized_blocks" => finalized_count,
        "view_1_nullified" => view_1_nullified_count == honest_replica_count,
        "view_2_nullified" => view_2_nullified_count == honest_replica_count,
        "equivocation_handled" => "Protocol correctly recovered from Byzantine equivocation",
    );
}

#[test]
#[ignore] // Run with: cargo test --lib test_e2e_consensus_with_persistent_equivocating_leader -- --ignored --nocapture
fn test_e2e_consensus_with_persistent_equivocating_leader() {
    // Create logger for test
    let logger = create_test_logger();

    // The replica index that will act as a persistent Byzantine equivocating leader
    // Replica 1 is the leader for views 1, 7, 13, 19, ... (round-robin: view % N == 1)
    const BYZANTINE_LEADER_IDX: usize = 1;

    slog::info!(
        logger,
        "Starting end-to-end consensus test (persistent equivocating leader)";
        "replicas" => N,
        "byzantine_tolerance" => F,
        "byzantine_leader" => BYZANTINE_LEADER_IDX,
        "byzantine_views" => "1, 7, 13, 19, ... (view % 6 == 1)",
    );

    // Phase 1: Setup test environment with funded accounts
    slog::info!(
        logger,
        "Phase 1: Creating test fixture with funded genesis accounts"
    );

    // Create funded transactions FIRST (we need genesis accounts for the fixture)
    let num_transactions = 50;
    let (transactions, genesis_accounts) = create_funded_test_transactions(num_transactions);

    let fixture = TestFixture::with_genesis_accounts(genesis_accounts);

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
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => peer_id));
        let setup = ReplicaSetup::new(peer_id, secret_key, replica_logger);

        slog::debug!(
            logger,
            "Created replica setup";
            "replica_index" => i,
            "peer_id" => peer_id,
        );

        replica_setups.push(setup);
    }

    // Phase 3: Register replicas and start engines
    slog::info!(
        logger,
        "Phase 3: Registering replicas and starting consensus engines (excluding Byzantine leader)"
    );
    let mut engines = Vec::with_capacity(N);
    let mut transaction_producers = Vec::with_capacity(N);
    let mut validation_services: Vec<Option<_>> = Vec::with_capacity(N);
    let mut mempool_services: Vec<Option<_>> = Vec::with_capacity(N);
    let mut stores = Vec::with_capacity(N);

    // Store the Byzantine leader's secret key for signing blocks
    let mut byzantine_leader_secret_key: Option<crate::crypto::aggregated::BlsSecretKey> = None;
    let mut byzantine_leader_peer_id: Option<u64> = None;

    for (i, setup) in replica_setups.into_iter().enumerate() {
        let replica_id = setup.replica_id;

        // Keep a clone of the storage for verification
        stores.push(setup.storage.clone());

        if i == BYZANTINE_LEADER_IDX {
            byzantine_leader_peer_id = Some(replica_id);
            byzantine_leader_secret_key = Some(setup.secret_key.clone());

            network.register_replica(
                replica_id,
                setup.message_producer,
                setup.block_producer,
                setup.broadcast_consumer,
            );

            engines.push(None);
            transaction_producers.push(None);
            validation_services.push(Some(setup.validation_service));
            mempool_services.push(Some(setup.mempool_service));

            slog::info!(
                logger,
                "Byzantine leader registered (engine NOT started)";
                "replica" => i,
                "peer_id" => replica_id,
            );
            continue;
        }

        let tx_producer = setup.transaction_producer;

        // Register with network - routes BlockProposal to Block Validator, others to Consensus
        network.register_replica(
            replica_id,
            setup.message_producer,
            setup.block_producer,
            setup.broadcast_consumer,
        );

        let replica_logger = logger.new(o!("replica" => i, "peer_id" => replica_id));

        let engine = ConsensusEngine::<N, F, M_SIZE>::new(
            fixture.config.clone(),
            replica_id,
            setup.secret_key,
            setup.message_consumer,
            setup.broadcast_producer,
            setup.validation_result_consumer,
            setup.proposal_req_producer,
            setup.proposal_resp_consumer,
            setup.finalized_producer,
            setup.persistence_writer,
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
        transaction_producers.push(Some(tx_producer));
        validation_services.push(Some(setup.validation_service));
        mempool_services.push(Some(setup.mempool_service));
    }

    let byzantine_secret_key =
        byzantine_leader_secret_key.expect("Byzantine leader secret key should exist");
    let byzantine_peer_id = byzantine_leader_peer_id.expect("Byzantine peer ID should exist");

    slog::info!(
        logger,
        "Honest replicas registered and engines started";
        "honest_count" => N - 1,
        "byzantine_leader_peer_id" => byzantine_peer_id,
    );

    // Phase 4: Start network routing
    slog::info!(logger, "Phase 4: Starting network routing");
    network.start();
    assert!(network.is_running(), "Network should be running");
    slog::info!(logger, "Network routing thread active");

    // Give honest replicas a moment to initialize
    thread::sleep(Duration::from_millis(100));

    // Phase 5: Start Byzantine equivocation thread
    // This thread continuously monitors view progress and injects equivocating blocks
    // whenever the Byzantine leader's turn comes up
    slog::info!(logger, "Phase 5: Starting Byzantine equivocation thread");

    use crate::consensus::ConsensusMessage;
    use crate::state::block::Block;
    use crate::state::transaction::Transaction;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let equivocations_injected = Arc::new(AtomicU64::new(0));

    // Clone what we need for the Byzantine thread
    let byzantine_thread_logger = logger.clone();
    let byzantine_thread_shutdown = shutdown_flag.clone();
    let byzantine_thread_equivocations = equivocations_injected.clone();
    let byzantine_thread_secret_key = byzantine_secret_key.clone();
    let byzantine_thread_peer_id = byzantine_peer_id;
    let message_producers = network.message_producers.clone();
    let sorted_peer_ids = fixture.peer_set.sorted_peer_ids.clone();

    // We need a reference store to check current chain tip
    // Use replica 0's store (first honest replica)
    let reference_store = stores[0].clone();

    let byzantine_thread = thread::spawn(move || {
        let mut last_injected_view: Option<u64> = None;
        let mut equivocation_counter = 0u64;

        while !byzantine_thread_shutdown.load(Ordering::Relaxed) {
            // Get current chain tip from reference store
            let finalized_blocks = reference_store
                .get_all_finalized_blocks()
                .unwrap_or_default();

            let (current_tip_view, current_tip_hash) =
                if let Some(last_block) = finalized_blocks.last() {
                    (last_block.view(), last_block.get_hash())
                } else {
                    (0, Block::genesis_hash())
                };

            // Calculate next Byzantine leader view
            // Byzantine leader is replica 1, which leads views where view % N == 1
            let next_byzantine_view = {
                let mut v = current_tip_view + 1;
                while v % (N as u64) != (BYZANTINE_LEADER_IDX as u64) {
                    v += 1;
                }
                v
            };

            // Only inject if we haven't already injected for this view
            // and the network is close to this view (within 3 views)
            let should_inject = last_injected_view.is_none_or(|last| next_byzantine_view > last)
                && next_byzantine_view <= current_tip_view + 3;

            if should_inject {
                // Determine parent hash for this view
                // We need to find the block that will be the parent
                let parent_hash = if next_byzantine_view == current_tip_view + 1 {
                    current_tip_hash
                } else {
                    // There's a gap - views might be getting nullified
                    // Use the current tip as parent (the protocol will handle this)
                    current_tip_hash
                };

                // Create two different transactions
                let tx1 = {
                    let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
                    let pk = sk.public_key();
                    Transaction::new_transfer(
                        Address::from_public_key(&pk),
                        Address::from_bytes([1u8; 32]),
                        equivocation_counter,
                        1000,
                        1,
                        &sk,
                    )
                };

                let tx2 = {
                    let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
                    let pk = sk.public_key();
                    Transaction::new_transfer(
                        Address::from_public_key(&pk),
                        Address::from_bytes([2u8; 32]),
                        2,
                        equivocation_counter + 1,
                        2000,
                        &sk,
                    )
                };

                // Create Block 1
                let temp_block1 = Block::new(
                    next_byzantine_view,
                    byzantine_thread_peer_id,
                    parent_hash,
                    vec![Arc::new(tx1)],
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64,
                    byzantine_thread_secret_key.sign(b"temp1"),
                    false,
                    1,
                );
                let block1_hash = temp_block1.get_hash();
                let block1 = Block::new(
                    next_byzantine_view,
                    byzantine_thread_peer_id,
                    parent_hash,
                    temp_block1.transactions.clone(),
                    temp_block1.header.timestamp,
                    byzantine_thread_secret_key.sign(&block1_hash),
                    false,
                    1,
                );

                // Create Block 2 (different transactions, same view - EQUIVOCATION!)
                let temp_block2 = Block::new(
                    next_byzantine_view,
                    byzantine_thread_peer_id,
                    parent_hash,
                    vec![Arc::new(tx2)],
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64
                        + 1,
                    byzantine_thread_secret_key.sign(b"temp2"),
                    false,
                    1,
                );
                let block2_hash = temp_block2.get_hash();
                let block2 = Block::new(
                    next_byzantine_view,
                    byzantine_thread_peer_id,
                    parent_hash,
                    temp_block2.transactions.clone(),
                    temp_block2.header.timestamp,
                    byzantine_thread_secret_key.sign(&block2_hash),
                    false,
                    1,
                );

                let msg1 = ConsensusMessage::BlockProposal(block1.clone());
                let msg2 = ConsensusMessage::BlockProposal(block2.clone());

                // Inject to partitions
                {
                    let mut producers = message_producers.lock().unwrap();

                    for (i, &target_peer_id) in sorted_peer_ids.iter().enumerate() {
                        if i == BYZANTINE_LEADER_IDX {
                            continue;
                        }

                        // Partition A (replicas 0, 2, 3) gets block1
                        // Partition B (replicas 4, 5) gets block2
                        let msg = if i == 0 || i == 2 || i == 3 {
                            msg1.clone()
                        } else {
                            msg2.clone()
                        };

                        if let Some(producer) = producers.get_mut(&target_peer_id) {
                            let _ = producer.push(msg);
                        }
                    }
                }

                slog::info!(
                    byzantine_thread_logger,
                    "Injected equivocating blocks";
                    "view" => next_byzantine_view,
                    "block1_hash" => hex::encode(&block1.get_hash()[..8]),
                    "block2_hash" => hex::encode(&block2.get_hash()[..8]),
                    "equivocation_count" => equivocation_counter / 2 + 1,
                );

                last_injected_view = Some(next_byzantine_view);
                equivocation_counter += 2;
                byzantine_thread_equivocations.fetch_add(1, Ordering::Relaxed);
            }

            thread::sleep(Duration::from_millis(50));
        }
    });

    // Phase 6: Submit transactions to honest replicas (already created with funded accounts)
    slog::info!(
        logger,
        "Phase 6: Submitting transactions to honest replicas";
        "count" => num_transactions,
    );

    let expected_tx_hashes: std::collections::HashSet<_> =
        transactions.iter().map(|tx| tx.tx_hash).collect();

    for (i, tx) in transactions.into_iter().enumerate() {
        let mut replica_idx = i % N;
        if replica_idx == BYZANTINE_LEADER_IDX {
            replica_idx = (replica_idx + 1) % N;
        }

        if let Some(ref mut tx_producer) = transaction_producers[replica_idx] {
            tx_producer.push(tx).expect("Failed to submit transaction");
        }
    }

    slog::info!(
        logger,
        "All transactions submitted";
        "total" => num_transactions,
    );

    // Phase 7: Let consensus run with persistent equivocation
    slog::info!(
        logger,
        "Phase 7: Running consensus with persistent Byzantine equivocation";
        "duration_secs" => 60,
    );

    let test_duration = Duration::from_secs(60);
    let check_interval = Duration::from_secs(10);
    let start_time = std::time::Instant::now();

    while start_time.elapsed() < test_duration {
        thread::sleep(check_interval);

        let elapsed = start_time.elapsed().as_secs();
        let msgs_routed = network.stats.messages_routed();
        let msgs_dropped = network.stats.messages_dropped();
        let equivocations = equivocations_injected.load(Ordering::Relaxed);

        slog::info!(
            logger,
            "Consensus progress check (persistent equivocation)";
            "elapsed_secs" => elapsed,
            "messages_routed" => msgs_routed,
            "messages_dropped" => msgs_dropped,
            "equivocations_injected" => equivocations,
        );
    }

    // Phase 8: Shutdown Byzantine thread
    slog::info!(
        logger,
        "Phase 8: Shutting down Byzantine equivocation thread"
    );
    shutdown_flag.store(true, Ordering::Relaxed);
    byzantine_thread.join().expect("Byzantine thread panicked");

    let total_equivocations = equivocations_injected.load(Ordering::Relaxed);
    slog::info!(
        logger,
        "Byzantine thread stopped";
        "total_equivocations" => total_equivocations,
    );

    // Phase 9: Verify honest replicas are still running
    slog::info!(logger, "Phase 9: Verifying honest replicas");

    for (i, engine_opt) in engines.iter().enumerate() {
        if i == BYZANTINE_LEADER_IDX {
            continue;
        }

        if let Some(engine) = engine_opt {
            let is_running = engine.is_running();
            slog::info!(
                logger,
                "Honest replica check";
                "replica" => i,
                "is_running" => is_running,
            );
            assert!(is_running, "Engine {} should still be running", i);
        }
    }

    // Phase 10: Collect statistics
    let final_msgs_routed = network.stats.messages_routed();
    let final_msgs_dropped = network.stats.messages_dropped();

    slog::info!(
        logger,
        "Final network statistics";
        "messages_routed" => final_msgs_routed,
        "messages_dropped" => final_msgs_dropped,
        "total_equivocations" => total_equivocations,
    );

    // Phase 11: Graceful shutdown
    slog::info!(logger, "Phase 11: Shutting down consensus engines");

    for engine in engines.iter().flatten() {
        engine.shutdown();
    }

    network.shutdown();

    // Shutdown validation services
    for mut service in validation_services.into_iter().flatten() {
        service.shutdown();
    }

    // Shutdown mempool services
    for mut service in mempool_services.into_iter().flatten() {
        service.shutdown();
    }

    for (i, engine_opt) in engines.into_iter().enumerate() {
        if let Some(engine) = engine_opt {
            engine
                .shutdown_and_wait(Duration::from_secs(5))
                .unwrap_or_else(|e| panic!("Engine {} failed to shutdown: {}", i, e));
        }
    }

    slog::info!(logger, "All engines shut down successfully");

    // Phase 12: Verify state consistency
    slog::info!(
        logger,
        "Phase 12: Verifying state consistency with persistent equivocation"
    );

    let mut first_honest_blocks: Option<Vec<crate::state::block::Block>> = None;
    let mut byzantine_views_nullified = 0u64;

    for (i, store) in stores.iter().enumerate() {
        if i == BYZANTINE_LEADER_IDX {
            continue;
        }

        let blocks = store
            .get_all_finalized_blocks()
            .expect("Failed to get finalized blocks from store");

        // Count how many Byzantine leader views were nullified
        let finalized_views: std::collections::HashSet<u64> =
            blocks.iter().map(|b| b.view()).collect();

        // Check all views where Byzantine leader should have been leader
        for view in (1..=blocks.last().map(|b| b.view()).unwrap_or(0)).step_by(N) {
            if !finalized_views.contains(&view) {
                let nullification = store
                    .get_nullification::<N, F, M_SIZE>(view)
                    .expect("Failed to query nullification");
                if nullification.is_some() {
                    byzantine_views_nullified += 1;
                }
            }
        }

        slog::info!(
            logger,
            "Honest replica state check";
            "replica" => i,
            "finalized_blocks" => blocks.len(),
            "highest_view" => blocks.last().map(|b| b.view()).unwrap_or(0),
        );

        assert!(
            !blocks.is_empty(),
            "Honest replica {} should have finalized blocks despite persistent equivocation",
            i
        );

        // Check chain integrity
        for window in blocks.windows(2) {
            let prev = &window[0];
            let curr = &window[1];
            assert_eq!(
                curr.parent_block_hash(),
                prev.get_hash(),
                "Chain broken for replica {} (view {} -> {})",
                i,
                prev.view(),
                curr.view()
            );
        }

        // Check consistency across honest replicas
        if let Some(ref first_blocks) = first_honest_blocks {
            let min_len = std::cmp::min(blocks.len(), first_blocks.len());
            for j in 0..min_len {
                assert_eq!(
                    blocks[j].get_hash(),
                    first_blocks[j].get_hash(),
                    "Block mismatch at index {} between honest replicas",
                    j
                );
            }
        } else {
            first_honest_blocks = Some(blocks);
        }
    }

    let finalized_count = first_honest_blocks.as_ref().map(|b| b.len()).unwrap_or(0);
    let honest_replica_count = N - 1;

    slog::info!(
        logger,
        "State consistency verification passed! ✓";
        "honest_replicas" => honest_replica_count,
        "finalized_blocks" => finalized_count,
        "byzantine_views_nullified" => byzantine_views_nullified,
        "equivocations_attempted" => total_equivocations,
    );

    // Verify transaction inclusion
    let mut included_tx_hashes = std::collections::HashSet::new();
    if let Some(ref blocks) = first_honest_blocks {
        for block in blocks {
            for tx in &block.transactions {
                included_tx_hashes.insert(tx.tx_hash);
            }
        }
    }

    let included_count = included_tx_hashes.intersection(&expected_tx_hashes).count();
    let inclusion_rate = if expected_tx_hashes.is_empty() {
        1.0
    } else {
        included_count as f64 / expected_tx_hashes.len() as f64
    };

    slog::info!(
        logger,
        "Transaction inclusion check";
        "total_submitted" => expected_tx_hashes.len(),
        "included_finalized" => included_count,
        "inclusion_rate" => format!("{:.2}%", inclusion_rate * 100.0)
    );

    // With persistent equivocation, we expect reasonable progress
    // The protocol should still make progress on non-Byzantine views
    assert!(
        inclusion_rate > 0.3,
        "Transaction inclusion rate too low with persistent equivocation: {:.2}%",
        inclusion_rate * 100.0
    );

    // Verify that at least some Byzantine views were nullified
    assert!(
        total_equivocations == 0 || byzantine_views_nullified > 0,
        "At least some Byzantine views should have been nullified"
    );

    // Final success message
    slog::info!(
        logger,
        "Test completed successfully! ✓";
        "total_messages_routed" => final_msgs_routed,
        "test_duration_secs" => 60,
        "byzantine_leader" => BYZANTINE_LEADER_IDX,
        "honest_replicas" => honest_replica_count,
        "finalized_blocks" => finalized_count,
        "equivocations_attempted" => total_equivocations,
        "byzantine_views_nullified" => byzantine_views_nullified,
        "result" => "Protocol correctly handled persistent Byzantine equivocation",
    );
}

#[test]
#[ignore] // Run with: cargo test --lib test_e2e_consensus_functional_blockchain -- --ignored --nocapture
fn test_e2e_consensus_functional_blockchain() {
    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting end-to-end consensus test (functional blockchain)";
        "replicas" => N,
        "byzantine_tolerance" => F,
    );

    // Phase 1: Create initial funded accounts
    // We'll create 10 "user" accounts with initial balances
    let num_initial_accounts = 10;
    let initial_balance = 100_000u64;

    let mut user_keys: Vec<(TxSecretKey, TxPublicKey)> = Vec::new();
    let mut genesis_accounts = Vec::new();

    for _ in 0..num_initial_accounts {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        genesis_accounts.push(GenesisAccount {
            public_key: hex::encode(pk.to_bytes()),
            balance: initial_balance,
        });
        user_keys.push((sk, pk));
    }

    slog::info!(
        logger,
        "Phase 1: Creating test fixture with funded genesis accounts";
        "num_accounts" => num_initial_accounts,
        "initial_balance_each" => initial_balance,
    );

    let fixture = TestFixture::with_genesis_accounts(genesis_accounts);

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
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => peer_id));
        let setup = ReplicaSetup::new(peer_id, secret_key, replica_logger);

        slog::debug!(
            logger,
            "Created replica setup";
            "replica_index" => i,
            "peer_id" => peer_id,
        );

        replica_setups.push(setup);
    }

    // Phase 3: Register replicas and start engines
    slog::info!(
        logger,
        "Phase 3: Registering replicas and starting consensus engines"
    );
    let mut engines = Vec::with_capacity(N);
    let mut transaction_producers = Vec::with_capacity(N);
    let mut validation_services = Vec::with_capacity(N);
    let mut mempool_services = Vec::with_capacity(N);
    let mut stores = Vec::with_capacity(N);
    let mut pending_state_readers = Vec::with_capacity(N);

    for (i, setup) in replica_setups.into_iter().enumerate() {
        let replica_id = setup.replica_id;

        // Keep a clone of the storage for verification
        stores.push(setup.storage.clone());

        // Keep the pending state reader for balance verification
        let pending_reader = setup.persistence_writer.reader();
        pending_state_readers.push(pending_reader);

        // Register with network - routes BlockProposal to Block Validator, others to Consensus
        network.register_replica(
            replica_id,
            setup.message_producer,
            setup.block_producer,
            setup.broadcast_consumer,
        );

        // Create consensus engine
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => replica_id));

        let engine = ConsensusEngine::<N, F, M_SIZE>::new(
            fixture.config.clone(),
            replica_id,
            setup.secret_key,
            setup.message_consumer,
            setup.broadcast_producer,
            setup.validation_result_consumer,
            setup.proposal_req_producer,
            setup.proposal_resp_consumer,
            setup.finalized_producer,
            setup.persistence_writer,
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
        transaction_producers.push(setup.transaction_producer);
        validation_services.push(setup.validation_service);
        mempool_services.push(setup.mempool_service);
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

    // Phase 5: Generate valid transactions with proper state tracking
    slog::info!(
        logger,
        "Phase 5: Generating and submitting valid transactions"
    );

    // Track local state for generating valid transactions
    let mut local_balances: HashMap<Address, u64> = HashMap::new();
    let mut local_nonces: HashMap<Address, u64> = HashMap::new();

    // Initialize with genesis state
    for (_, pk) in &user_keys {
        let addr = Address::from_public_key(pk);
        local_balances.insert(addr, initial_balance);
        local_nonces.insert(addr, 0);
    }

    let mut all_transactions = Vec::new();
    let mut new_account_keys: Vec<(TxSecretKey, TxPublicKey)> = Vec::new();

    // Generate valid transactions:
    // - 30 transfers between existing accounts
    // - 20 transfers that create new accounts (implicit account creation)

    // Part A: Transfers between existing accounts
    for i in 0..30 {
        let sender_idx = i % num_initial_accounts;
        let receiver_idx = (i + 1) % num_initial_accounts;

        let (sender_sk, sender_pk) = &user_keys[sender_idx];
        let (_, receiver_pk) = &user_keys[receiver_idx];

        let sender_addr = Address::from_public_key(sender_pk);
        let receiver_addr = Address::from_public_key(receiver_pk);

        let nonce = *local_nonces.get(&sender_addr).unwrap();
        let balance = *local_balances.get(&sender_addr).unwrap();

        let transfer_amount = 100u64;
        let fee = 10u64;

        // Ensure sender has enough balance
        if balance >= transfer_amount + fee {
            let tx = Transaction::new_transfer(
                sender_addr,
                receiver_addr,
                transfer_amount,
                nonce,
                fee,
                sender_sk,
            );

            // Update local state
            *local_balances.get_mut(&sender_addr).unwrap() -= transfer_amount + fee;
            *local_balances.get_mut(&receiver_addr).unwrap() += transfer_amount;
            *local_nonces.get_mut(&sender_addr).unwrap() += 1;

            all_transactions.push(tx);
        }
    }

    // Part B: Transfers that create new accounts
    for i in 0..20 {
        let sender_idx = i % num_initial_accounts;
        let (sender_sk, sender_pk) = &user_keys[sender_idx];
        let sender_addr = Address::from_public_key(sender_pk);

        // Create a new account
        let new_sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let new_pk = new_sk.public_key();
        let new_addr = Address::from_public_key(&new_pk);

        let nonce = *local_nonces.get(&sender_addr).unwrap();
        let balance = *local_balances.get(&sender_addr).unwrap();

        let transfer_amount = 500u64; // Give new account some funds
        let fee = 10u64;

        if balance >= transfer_amount + fee {
            let tx = Transaction::new_transfer(
                sender_addr,
                new_addr,
                transfer_amount,
                nonce,
                fee,
                sender_sk,
            );

            // Update local state
            *local_balances.get_mut(&sender_addr).unwrap() -= transfer_amount + fee;
            local_balances.insert(new_addr, transfer_amount);
            local_nonces.insert(new_addr, 0);
            *local_nonces.get_mut(&sender_addr).unwrap() += 1;

            new_account_keys.push((new_sk, new_pk));
            all_transactions.push(tx);
        }
    }

    // Part C: Some transactions FROM newly created accounts (chain of transfers)
    for (new_sk, new_pk) in &new_account_keys[..5.min(new_account_keys.len())] {
        let sender_addr = Address::from_public_key(new_pk);
        let balance = *local_balances.get(&sender_addr).unwrap_or(&0);
        let nonce = *local_nonces.get(&sender_addr).unwrap_or(&0);

        // Send back to first user
        let (_, receiver_pk) = &user_keys[0];
        let receiver_addr = Address::from_public_key(receiver_pk);

        let transfer_amount = 50u64;
        let fee = 10u64;

        if balance >= transfer_amount + fee {
            let tx = Transaction::new_transfer(
                sender_addr,
                receiver_addr,
                transfer_amount,
                nonce,
                fee,
                new_sk,
            );

            *local_balances.get_mut(&sender_addr).unwrap() -= transfer_amount + fee;
            *local_balances.get_mut(&receiver_addr).unwrap() += transfer_amount;
            *local_nonces.get_mut(&sender_addr).unwrap() += 1;

            all_transactions.push(tx);
        }
    }

    let expected_tx_hashes: HashSet<_> = all_transactions.iter().map(|tx| tx.tx_hash).collect();
    let total_valid_txs = all_transactions.len();

    slog::info!(
        logger,
        "Generated valid transactions";
        "total" => total_valid_txs,
        "between_existing" => 30,
        "creating_new_accounts" => new_account_keys.len(),
        "from_new_accounts" => new_account_keys.len().min(5),
    );

    // Submit transactions to ALL replicas (simulates P2P gossip)
    // Each transaction must reach all replicas so that any leader can include it
    for tx in all_transactions.into_iter() {
        for producer in &mut transaction_producers {
            // Clone the transaction and send to each replica
            // Use .ok() since the mempool will deduplicate anyway
            let _ = producer.push(tx.clone());
        }
    }

    slog::info!(
        logger,
        "All transactions submitted to all replicas";
        "total" => total_valid_txs,
        "replicas" => N,
    );

    // Phase 6: Wait for consensus to progress
    slog::info!(
        logger,
        "Phase 6: Waiting for consensus to progress";
        "duration_secs" => 30,
    );

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

    // Phase 8: Graceful shutdown
    slog::info!(logger, "Phase 8: Shutting down consensus engines");

    for engine in &engines {
        engine.shutdown();
    }

    network.shutdown();

    for mut service in validation_services {
        service.shutdown();
    }

    for mut service in mempool_services {
        service.shutdown();
    }

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

    // Phase 9: Verify state consistency
    slog::info!(logger, "Phase 9: Verifying state consistency");

    let mut first_replica_blocks: Option<Vec<crate::state::block::Block>> = None;

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

        // Check consistency across replicas
        if let Some(ref first_blocks) = first_replica_blocks {
            let min_len = std::cmp::min(blocks.len(), first_blocks.len());
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
        } else {
            first_replica_blocks = Some(blocks);
        }
    }

    slog::info!(logger, "State consistency verification passed! ✓");

    // Phase 10: Verify all transactions were included
    slog::info!(
        logger,
        "Phase 10: Verifying transaction inclusion and state consistency"
    );

    let mut included_tx_hashes = HashSet::new();
    if let Some(ref blocks) = first_replica_blocks {
        for block in blocks {
            for tx in &block.transactions {
                included_tx_hashes.insert(tx.tx_hash);
            }
        }
    }

    // Verify ALL valid transactions were included
    let missing_txs: Vec<_> = expected_tx_hashes.difference(&included_tx_hashes).collect();

    assert!(
        missing_txs.is_empty(),
        "Missing {} transactions out of {}",
        missing_txs.len(),
        total_valid_txs
    );

    slog::info!(
        logger,
        "Transaction inclusion verified! ✓";
        "total_valid_txs" => total_valid_txs,
        "all_included" => true,
        "new_accounts_created" => new_account_keys.len(),
    );

    // Phase 11: Verify final account balances using PendingStateReader
    slog::info!(logger, "Phase 11: Verifying final account balances");

    // Use the first replica's pending state reader
    let pending_reader = &pending_state_readers[0];

    // Verify balances for genesis accounts
    for (_, pk) in &user_keys {
        let addr = Address::from_public_key(pk);
        let expected_balance = *local_balances.get(&addr).unwrap();

        if let Some(account_state) = pending_reader.get_account(&addr) {
            slog::debug!(
                logger,
                "Account balance check";
                "address" => hex::encode(&addr.as_bytes()[..8]),
                "expected_balance" => expected_balance,
                "actual_balance" => account_state.balance,
            );

            assert_eq!(
                account_state.balance,
                expected_balance,
                "Balance mismatch for account {:?}: expected {}, got {}",
                hex::encode(&addr.as_bytes()[..8]),
                expected_balance,
                account_state.balance
            );
        } else {
            panic!(
                "Account {:?} not found in pending state",
                hex::encode(&addr.as_bytes()[..8])
            );
        }
    }

    // Verify balances for newly created accounts
    for (_, new_pk) in &new_account_keys {
        let addr = Address::from_public_key(new_pk);
        let expected_balance = *local_balances.get(&addr).unwrap_or(&0);

        if let Some(account_state) = pending_reader.get_account(&addr) {
            slog::debug!(
                logger,
                "New account balance check";
                "address" => hex::encode(&addr.as_bytes()[..8]),
                "expected_balance" => expected_balance,
                "actual_balance" => account_state.balance,
            );

            assert_eq!(
                account_state.balance,
                expected_balance,
                "Balance mismatch for new account {:?}: expected {}, got {}",
                hex::encode(&addr.as_bytes()[..8]),
                expected_balance,
                account_state.balance
            );
        } else if expected_balance > 0 {
            panic!(
                "New account {:?} with expected balance {} not found",
                hex::encode(&addr.as_bytes()[..8]),
                expected_balance
            );
        }
    }

    slog::info!(
        logger,
        "Account balance verification passed! ✓";
        "genesis_accounts_verified" => user_keys.len(),
        "new_accounts_verified" => new_account_keys.len(),
    );

    // Final success message
    slog::info!(
        logger,
        "Test completed successfully! ✓";
        "scenario" => "functional blockchain with account creation and balance verification",
        "total_transactions" => total_valid_txs,
        "new_accounts_created" => new_account_keys.len(),
    );
}

#[test]
#[ignore] // Run with: cargo test --lib test_e2e_consensus_invalid_tx_rejection -- --ignored --nocapture
fn test_e2e_consensus_invalid_tx_rejection() {
    let logger = create_test_logger();

    slog::info!(
        logger,
        "Starting end-to-end consensus test (invalid tx rejection)";
        "replicas" => N,
        "byzantine_tolerance" => F,
    );

    // Phase 1: Create funded accounts
    let num_accounts = 5;
    let initial_balance = 10_000u64;

    let mut user_keys: Vec<(TxSecretKey, TxPublicKey)> = Vec::new();
    let mut genesis_accounts = Vec::new();

    for _ in 0..num_accounts {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        genesis_accounts.push(GenesisAccount {
            public_key: hex::encode(pk.to_bytes()),
            balance: initial_balance,
        });
        user_keys.push((sk, pk));
    }

    slog::info!(
        logger,
        "Phase 1: Creating test fixture with funded genesis accounts";
        "num_accounts" => num_accounts,
        "initial_balance_each" => initial_balance,
    );

    let fixture = TestFixture::with_genesis_accounts(genesis_accounts);

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
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => peer_id));
        let setup = ReplicaSetup::new(peer_id, secret_key, replica_logger);

        slog::debug!(
            logger,
            "Created replica setup";
            "replica_index" => i,
            "peer_id" => peer_id,
        );

        replica_setups.push(setup);
    }

    // Phase 3: Register replicas and start engines
    slog::info!(
        logger,
        "Phase 3: Registering replicas and starting consensus engines"
    );
    let mut engines = Vec::with_capacity(N);
    let mut transaction_producers = Vec::with_capacity(N);
    let mut validation_services = Vec::with_capacity(N);
    let mut mempool_services = Vec::with_capacity(N);
    let mut stores = Vec::with_capacity(N);
    let mut pending_state_readers = Vec::with_capacity(N);

    for (i, setup) in replica_setups.into_iter().enumerate() {
        let replica_id = setup.replica_id;

        // Keep a clone of the storage for verification
        stores.push(setup.storage.clone());

        // Keep the pending state reader for balance verification
        let pending_reader = setup.persistence_writer.reader();
        pending_state_readers.push(pending_reader);

        // Register with network - routes BlockProposal to Block Validator, others to Consensus
        network.register_replica(
            replica_id,
            setup.message_producer,
            setup.block_producer,
            setup.broadcast_consumer,
        );

        // Create consensus engine
        let replica_logger = logger.new(o!("replica" => i, "peer_id" => replica_id));

        let engine = ConsensusEngine::<N, F, M_SIZE>::new(
            fixture.config.clone(),
            replica_id,
            setup.secret_key,
            setup.message_consumer,
            setup.broadcast_producer,
            setup.validation_result_consumer,
            setup.proposal_req_producer,
            setup.proposal_resp_consumer,
            setup.finalized_producer,
            setup.persistence_writer,
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
        transaction_producers.push(setup.transaction_producer);
        validation_services.push(setup.validation_service);
        mempool_services.push(setup.mempool_service);
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

    // Phase 5: Generate mix of valid and invalid transactions
    slog::info!(logger, "Phase 5: Generating valid and invalid transactions");

    // Track state for valid tx generation
    let mut local_nonces: HashMap<Address, u64> = HashMap::new();
    let mut local_balances: HashMap<Address, u64> = HashMap::new();

    for (_, pk) in &user_keys {
        let addr = Address::from_public_key(pk);
        local_nonces.insert(addr, 0);
        local_balances.insert(addr, initial_balance);
    }

    let mut valid_transactions = Vec::new();
    let mut invalid_transactions = Vec::new();
    let mut valid_tx_hashes = HashSet::new();
    let mut invalid_tx_hashes = HashSet::new();

    // Generate 20 VALID transactions
    for i in 0..20 {
        let sender_idx = i % num_accounts;
        let receiver_idx = (i + 1) % num_accounts;

        let (sender_sk, sender_pk) = &user_keys[sender_idx];
        let (_, receiver_pk) = &user_keys[receiver_idx];

        let sender_addr = Address::from_public_key(sender_pk);
        let receiver_addr = Address::from_public_key(receiver_pk);

        let nonce = *local_nonces.get(&sender_addr).unwrap();
        let balance = *local_balances.get(&sender_addr).unwrap();

        let amount = 100u64;
        let fee = 10u64;

        if balance >= amount + fee {
            let tx = Transaction::new_transfer(
                sender_addr,
                receiver_addr,
                amount,
                nonce,
                fee,
                sender_sk,
            );

            valid_tx_hashes.insert(tx.tx_hash);
            valid_transactions.push(tx);

            // Update state
            *local_balances.get_mut(&sender_addr).unwrap() -= amount + fee;
            *local_balances.get_mut(&receiver_addr).unwrap() += amount;
            *local_nonces.get_mut(&sender_addr).unwrap() += 1;
        }
    }

    // Generate INVALID transactions of various types:

    // Type 1: Bad nonce (too high - future nonce)
    {
        let (sender_sk, sender_pk) = &user_keys[0];
        let (_, receiver_pk) = &user_keys[1];
        let sender_addr = Address::from_public_key(sender_pk);
        let receiver_addr = Address::from_public_key(receiver_pk);

        let current_nonce = *local_nonces.get(&sender_addr).unwrap();
        let bad_nonce = current_nonce + 100; // Way too high

        let tx = Transaction::new_transfer(
            sender_addr,
            receiver_addr,
            50,
            bad_nonce, // Invalid: nonce gap
            10,
            sender_sk,
        );

        slog::debug!(logger, "Created invalid tx: future nonce"; 
            "expected_nonce" => current_nonce, 
            "used_nonce" => bad_nonce);

        invalid_tx_hashes.insert(tx.tx_hash);
        invalid_transactions.push(tx);
    }

    // Type 2: Bad nonce (stale - already used)
    {
        let (sender_sk, sender_pk) = &user_keys[1];
        let (_, receiver_pk) = &user_keys[2];
        let sender_addr = Address::from_public_key(sender_pk);
        let receiver_addr = Address::from_public_key(receiver_pk);

        let tx = Transaction::new_transfer(
            sender_addr,
            receiver_addr,
            50,
            0, // Invalid: nonce 0 already used
            10,
            sender_sk,
        );

        slog::debug!(logger, "Created invalid tx: stale nonce");

        invalid_tx_hashes.insert(tx.tx_hash);
        invalid_transactions.push(tx);
    }

    // Type 3: Insufficient balance
    {
        let (sender_sk, sender_pk) = &user_keys[2];
        let (_, receiver_pk) = &user_keys[3];
        let sender_addr = Address::from_public_key(sender_pk);
        let receiver_addr = Address::from_public_key(receiver_pk);

        let current_nonce = *local_nonces.get(&sender_addr).unwrap();
        let current_balance = *local_balances.get(&sender_addr).unwrap();

        let tx = Transaction::new_transfer(
            sender_addr,
            receiver_addr,
            current_balance + 1_000_000, // Way more than available
            current_nonce,
            10,
            sender_sk,
        );

        slog::debug!(logger, "Created invalid tx: insufficient balance";
            "balance" => current_balance,
            "trying_to_send" => current_balance + 1_000_000);

        invalid_tx_hashes.insert(tx.tx_hash);
        invalid_transactions.push(tx);
    }

    // Type 4: Transaction from non-existent account (no balance)
    {
        let ghost_sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let ghost_pk = ghost_sk.public_key();
        let ghost_addr = Address::from_public_key(&ghost_pk);

        let (_, receiver_pk) = &user_keys[0];
        let receiver_addr = Address::from_public_key(receiver_pk);

        let tx = Transaction::new_transfer(ghost_addr, receiver_addr, 100, 0, 10, &ghost_sk);

        slog::debug!(logger, "Created invalid tx: non-existent sender");

        invalid_tx_hashes.insert(tx.tx_hash);
        invalid_transactions.push(tx);
    }

    slog::info!(
        logger,
        "Generated transactions";
        "valid_count" => valid_transactions.len(),
        "invalid_count" => invalid_transactions.len(),
    );

    // Combine and shuffle transactions
    let mut all_transactions = Vec::new();
    all_transactions.extend(valid_transactions);
    all_transactions.extend(invalid_transactions);

    // Shuffle to interleave valid and invalid
    use rand::seq::SliceRandom;
    all_transactions.shuffle(&mut rand::thread_rng());

    // Submit all transactions to ALL replicas (simulates P2P gossip)
    // Each transaction must reach all replicas so that any leader can include it
    for tx in all_transactions.into_iter() {
        for producer in &mut transaction_producers {
            // Clone the transaction and send to each replica
            // Use .ok() since the mempool will deduplicate anyway
            let _ = producer.push(tx.clone());
        }
    }

    slog::info!(
        logger,
        "All transactions submitted to all replicas";
        "valid" => valid_tx_hashes.len(),
        "invalid" => invalid_tx_hashes.len(),
        "replicas" => N,
    );

    // Phase 6: Wait for consensus to progress
    slog::info!(
        logger,
        "Phase 6: Waiting for consensus to progress";
        "duration_secs" => 30,
    );

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

    // Phase 8: Graceful shutdown
    slog::info!(logger, "Phase 8: Shutting down consensus engines");

    for engine in &engines {
        engine.shutdown();
    }

    network.shutdown();

    for mut service in validation_services {
        service.shutdown();
    }

    for mut service in mempool_services {
        service.shutdown();
    }

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

    // Phase 9: Verify state consistency
    slog::info!(logger, "Phase 9: Verifying state consistency");

    let mut first_replica_blocks: Option<Vec<crate::state::block::Block>> = None;

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

        // Check consistency across replicas
        if let Some(ref first_blocks) = first_replica_blocks {
            let min_len = std::cmp::min(blocks.len(), first_blocks.len());
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
        } else {
            first_replica_blocks = Some(blocks);
        }
    }

    slog::info!(logger, "State consistency verification passed! ✓");

    // Phase 10: Verify ONLY valid transactions were included
    slog::info!(
        logger,
        "Phase 10: Verifying transaction inclusion correctness"
    );

    let mut included_tx_hashes = HashSet::new();
    if let Some(ref blocks) = first_replica_blocks {
        for block in blocks {
            for tx in &block.transactions {
                included_tx_hashes.insert(tx.tx_hash);
            }
        }
    }

    // Check 1: All valid transactions should be included
    let missing_valid: Vec<_> = valid_tx_hashes.difference(&included_tx_hashes).collect();

    if !missing_valid.is_empty() {
        slog::warn!(
            logger,
            "Some valid transactions missing";
            "missing_count" => missing_valid.len(),
        );
    }

    // Allow some valid txs to be missing if they arrived late
    let valid_inclusion_rate =
        (valid_tx_hashes.len() - missing_valid.len()) as f64 / valid_tx_hashes.len() as f64;

    assert!(
        valid_inclusion_rate > 0.9,
        "Valid transaction inclusion rate too low: {:.2}%",
        valid_inclusion_rate * 100.0
    );

    // Check 2: NO invalid transactions should be included
    let included_invalid: Vec<_> = invalid_tx_hashes
        .intersection(&included_tx_hashes)
        .collect();

    assert!(
        included_invalid.is_empty(),
        "Invalid transactions were incorrectly included! Found {} invalid txs in finalized blocks",
        included_invalid.len()
    );

    slog::info!(
        logger,
        "Transaction validation verified! ✓";
        "valid_submitted" => valid_tx_hashes.len(),
        "valid_included" => valid_tx_hashes.len() - missing_valid.len(),
        "valid_inclusion_rate" => format!("{:.2}%", valid_inclusion_rate * 100.0),
        "invalid_submitted" => invalid_tx_hashes.len(),
        "invalid_included" => 0,
        "invalid_correctly_rejected" => true,
    );

    // Verify protocol made progress despite invalid transactions
    assert!(
        !first_replica_blocks.as_ref().unwrap().is_empty(),
        "Protocol should have finalized blocks despite invalid transactions"
    );

    // Phase 11: Verify account balances are consistent (invalid txs didn't affect state)
    slog::info!(logger, "Phase 11: Verifying account balances are correct");

    let pending_reader = &pending_state_readers[0];

    for (_, pk) in &user_keys {
        let addr = Address::from_public_key(pk);
        let expected_balance = *local_balances.get(&addr).unwrap();

        if let Some(account_state) = pending_reader.get_account(&addr) {
            slog::debug!(
                logger,
                "Account balance check";
                "address" => hex::encode(&addr.as_bytes()[..8]),
                "expected_balance" => expected_balance,
                "actual_balance" => account_state.balance,
            );

            assert_eq!(
                account_state.balance,
                expected_balance,
                "Balance mismatch for account {:?}: expected {}, got {}. Invalid txs may have affected state!",
                hex::encode(&addr.as_bytes()[..8]),
                expected_balance,
                account_state.balance
            );
        } else {
            panic!(
                "Account {:?} not found in pending state",
                hex::encode(&addr.as_bytes()[..8])
            );
        }
    }

    slog::info!(
        logger,
        "Account balance verification passed! ✓";
        "accounts_verified" => user_keys.len(),
        "invalid_txs_had_no_effect" => true,
    );

    // Final success message
    slog::info!(
        logger,
        "Test completed successfully! ✓";
        "scenario" => "invalid transaction rejection with balance verification",
        "protocol_made_progress" => true,
        "all_invalid_rejected" => true,
        "valid_inclusion_rate" => format!("{:.2}%", valid_inclusion_rate * 100.0),
    );
}
