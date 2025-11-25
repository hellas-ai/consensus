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

    // 2. Now wait for each engine to finish its thread.
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
            assert!(curr.view() > prev.view(), "View should strictly increase");
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

    // Shutdown network
    slog::info!(logger, "Shutting down network");
    network.shutdown();

    // Final success message
    slog::info!(
        logger,
        "Test completed successfully! ✓";
        "total_messages_routed" => final_msgs_routed,
        "test_duration_secs" => 30,
    );
}
