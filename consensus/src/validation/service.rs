//! Block Validation Service - Thread Management
//!
//! Spawns a dedicated OS thread for block validation to avoid blocking
//! the consensus engine. Uses lock-free rtrb channels for communication.
//!
//! ## Data Flow
//!
//!
//! P2P Thread → [block_channel] → Validator Thread → [validated_channel] → Consensus Thread

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use rtrb::{Consumer, Producer, RingBuffer};
use slog::Logger;

use crate::{state::block::Block, storage::store::ConsensusStore};

use super::{BlockValidator, PendingStateReader, PendingStateWriter, types::ValidatedBlock};

const RING_BUFFER_SIZE: usize = 64;

/// Block validation service running on a dedicated OS thread.
///
/// The service validates incoming blocks (including verifying Ed25519 signatures, balances, nonces,
/// etc.) and produces `ValidatedBlock`s with pre-computed `StateDiff`s for the
/// consensus engine.
pub struct BlockValidationService {
    handle: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    /// Unparker to wake up the validation thread immediately on shutdown.
    shutdown_unparker: crossbeam::sync::Unparker,
}

impl BlockValidationService {
    /// Spawns the validation service on a new OS thread.
    ///
    /// # Returns
    ///
    /// A tuple containing:
    /// - `ValidationService` - handle to manage the service lifecycle
    /// - `Producer<Block>` - for P2P to submit blocks for validation
    /// - `Consumer<ValidatedBlock>` - for Consensus to receive validated blocks
    /// - `PendingStateWriter` - for Consensus to manage pending state (m-notarized diffs)
    ///
    /// # Example
    ///
    ///
    /// let store = Arc::new(ConsensusStore::open(&path)?);
    /// let (service, block_producer, validated_consumer, pending_state_writer) =
    ///     ValidationService::spawn(store, 0);
    ///
    /// // P2P thread pushes blocks:
    /// block_producer.push(block).ok();
    ///
    /// // Consensus thread receives validated blocks:
    /// if let Ok(validated) = validated_consumer.pop() {
    ///     // Vote on the block, apply state_diff on finalization
    /// }
    ///
    /// // Consensus manages pending state:
    /// pending_state_writer.add_m_notarized_diff(view, state_diff);
    /// pending_state_writer.finalize_up_to(finalized_view);
    pub fn spawn(
        store: Arc<ConsensusStore>,
        last_finalized_view: u64,
        shutdown: Arc<AtomicBool>,
        logger: Logger,
    ) -> (
        Self,
        Producer<Block>,
        Consumer<ValidatedBlock>,
        PendingStateWriter,
    ) {
        // Create block input channel (P2P service -> Validator thread)
        let (block_producer, block_consumer) = RingBuffer::<Block>::new(RING_BUFFER_SIZE);

        // Create validated output channel (Validator → Consensus)
        let (validated_producer, validated_consumer) =
            RingBuffer::<ValidatedBlock>::new(RING_BUFFER_SIZE);

        // Create pending state manager (lock-free shared state)
        let (pending_state_writer, pending_state_reader) =
            PendingStateWriter::new(Arc::clone(&store), last_finalized_view);

        // Create parker/unparker pair for instant shutdown wake-up
        let parker = crossbeam::sync::Parker::new();
        let shutdown_unparker = parker.unparker().clone();

        let logger_clone = logger.clone();
        let shutdown_clone = Arc::clone(&shutdown);

        let handle = thread::Builder::new()
            .name("block-validator".into())
            .spawn(move || {
                validation_loop(
                    pending_state_reader,
                    block_consumer,
                    validated_producer,
                    shutdown_clone,
                    parker,
                    logger_clone,
                );
            })
            .expect("Failed to spawn block validator thread");

        (
            Self {
                handle: Some(handle),
                shutdown,
                shutdown_unparker,
            },
            block_producer,
            validated_consumer,
            pending_state_writer,
        )
    }

    /// Signals shutdown and waits for the thread to terminate.
    ///
    /// This method:
    /// 1. Sets the shutdown flag
    /// 2. Wakes up the thread immediately via unpark
    /// 3. Joins the thread to ensure clean shutdown
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.shutdown_unparker.unpark();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Returns true if the service is still running.
    pub fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }
}

impl Drop for BlockValidationService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Main validation event loop.
///
/// # Shutdown Behavior
///
/// When shutdown is signaled, the parker is unparked immediately for instant wake-up.
fn validation_loop(
    pending_state_reader: PendingStateReader,
    mut block_consumer: Consumer<Block>,
    mut validated_producer: Producer<ValidatedBlock>,
    shutdown: Arc<AtomicBool>,
    parker: crossbeam::sync::Parker,
    logger: Logger,
) {
    let validator = BlockValidator::new(pending_state_reader);
    let mut idle_count = 0_u32;

    while !shutdown.load(Ordering::Acquire) {
        match block_consumer.pop() {
            Ok(block) => {
                idle_count = 0;
                let view = block.view();
                let block_hash = block.get_hash();

                match validator.validate_block(&block) {
                    Ok(state_diff) => {
                        let validated = ValidatedBlock {
                            block,
                            state_diff,
                            validated_at: Instant::now(),
                        };

                        // Send to consensus with back pressure handling
                        let mut validated = validated;
                        loop {
                            match validated_producer.push(validated) {
                                Ok(()) => break,
                                Err(rtrb::PushError::Full(returned)) => {
                                    if shutdown.load(Ordering::Acquire) {
                                        return;
                                    }
                                    // Yield to let consensus catch up
                                    std::thread::yield_now();
                                    validated = returned;
                                }
                            }
                        }

                        slog::debug!(
                            logger,
                            "Block validated successfully";
                            "view" => view,
                            "block_hash" => ?block_hash,
                        );
                    }
                    Err(errors) => {
                        slog::warn!(
                            logger,
                            "Block validation failed";
                            "view" => view,
                            "block_hash" => ?block_hash,
                            "error_count" => errors.len(),
                            "first_error" => ?errors.first(),
                        );
                        // Drop invalid blocks - don't forward to consensus
                    }
                }
            }
            Err(_) => {
                // No blocks available - progressive backoff
                // Uses parker.park_timeout() for instant shutdown wake-up
                idle_count = idle_count.saturating_add(1);

                if idle_count < 10 {
                    std::hint::spin_loop();
                } else if idle_count < 100 {
                    std::thread::yield_now();
                } else {
                    // Park with timeout - can be woken immediately by shutdown_unparker.unpark()
                    parker.park_timeout(std::time::Duration::from_micros(100));
                }
            }
        }
    }

    slog::info!(logger, "Block validator thread shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::crypto::aggregated::BlsSecretKey;
    use crate::crypto::transaction_crypto::TxSecretKey;
    use crate::state::account::Account;
    use crate::state::address::Address;
    use crate::state::transaction::Transaction;
    use crate::validation::StateDiff;

    fn temp_db_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "validation_service_test_{}.redb",
            rand::random::<u64>()
        ));
        p
    }

    fn gen_keypair() -> (TxSecretKey, Address) {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);
        (sk, addr)
    }

    fn create_test_block(view: u64, transactions: Vec<Arc<Transaction>>) -> Block {
        let sk = BlsSecretKey::generate(&mut rand::thread_rng());
        let sig = sk.sign(b"test block");
        Block::new(view, 0, [0u8; 32], transactions, 0, sig, false, view)
    }

    #[test]
    fn service_starts_and_stops() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        let (mut service, _producer, _consumer, _writer) = BlockValidationService::spawn(
            store,
            0,
            Arc::new(AtomicBool::new(false)),
            slog::Logger::root(slog::Discard, slog::o!()),
        );

        assert!(service.is_running());

        service.shutdown();

        // After shutdown, handle is taken
        assert!(service.handle.is_none());
    }

    #[test]
    fn validates_empty_block() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        let (mut service, mut producer, mut consumer, _writer) = BlockValidationService::spawn(
            store,
            0,
            Arc::new(AtomicBool::new(false)),
            slog::Logger::root(slog::Discard, slog::o!()),
        );

        // Create and send empty block
        let block = create_test_block(1, vec![]);
        producer.push(block.clone()).unwrap();

        // Wait for validation
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Receive validated block
        let validated = consumer.pop().unwrap();
        assert_eq!(validated.block.view(), 1);
        assert_eq!(validated.state_diff.num_updates(), 0);

        service.shutdown();
    }

    #[test]
    fn validates_transfer_block() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        // Create funded account
        let (sk, sender_addr) = gen_keypair();
        let sender_pk = sk.public_key();
        store
            .put_account(&Account::new(sender_pk, 1000, 0))
            .unwrap();

        let (_, recipient_addr) = gen_keypair();

        let (mut service, mut producer, mut consumer, _writer) = BlockValidationService::spawn(
            Arc::clone(&store),
            0,
            Arc::new(AtomicBool::new(false)),
            slog::Logger::root(slog::Discard, slog::o!()),
        );

        // Create block with transfer
        let tx = Transaction::new_transfer(sender_addr, recipient_addr, 100, 0, 10, &sk);
        let block = create_test_block(1, vec![Arc::new(tx)]);
        producer.push(block).unwrap();

        // Wait for validation
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Check result
        let validated = consumer.pop().unwrap();
        assert_eq!(validated.state_diff.total_fees, 10);
        assert!(validated.state_diff.updates.contains_key(&sender_addr));
        assert!(validated.state_diff.updates.contains_key(&recipient_addr));

        service.shutdown();
    }

    #[test]
    fn rejects_invalid_block() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        // Don't fund the sender account

        let (mut service, mut producer, mut consumer, _writer) = BlockValidationService::spawn(
            store,
            0,
            Arc::new(AtomicBool::new(false)),
            slog::Logger::root(slog::Discard, slog::o!()),
        );

        // Create block with transfer from unfunded account
        let tx = Transaction::new_transfer(sender_addr, recipient_addr, 100, 0, 10, &sk);
        let block = create_test_block(1, vec![Arc::new(tx)]);
        producer.push(block).unwrap();

        // Wait for validation
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Should not receive anything (invalid block dropped)
        assert!(consumer.pop().is_err());

        service.shutdown();
    }

    #[test]
    fn validates_against_pending_state() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        let (mut service, mut producer, mut consumer, mut writer) = BlockValidationService::spawn(
            Arc::clone(&store),
            0,
            Arc::new(AtomicBool::new(false)),
            slog::Logger::root(slog::Discard, slog::o!()),
        );

        // Fund account via pending state (simulating m-notarized block)
        let mut diff = StateDiff::new();
        diff.add_created_account(sender_addr, 1000);
        writer.add_m_notarized_diff(1, Arc::new(diff));

        // Create block with transfer - should work against pending state
        let tx = Transaction::new_transfer(sender_addr, recipient_addr, 100, 0, 10, &sk);
        let block = create_test_block(2, vec![Arc::new(tx)]);
        producer.push(block).unwrap();

        // Wait for validation
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Should succeed
        let validated = consumer.pop().unwrap();
        assert_eq!(validated.state_diff.total_fees, 10);

        service.shutdown();
    }

    #[test]
    fn processes_multiple_blocks_in_order() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        let (mut service, mut producer, mut consumer, _writer) = BlockValidationService::spawn(
            store,
            0,
            Arc::new(AtomicBool::new(false)),
            slog::Logger::root(slog::Discard, slog::o!()),
        );

        // Send multiple empty blocks
        for view in 1..=5 {
            let block = create_test_block(view, vec![]);
            producer.push(block).unwrap();
        }

        // Wait for all to be processed
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Verify all received in order
        for expected_view in 1..=5 {
            let validated = consumer.pop().expect("Should receive block");
            assert_eq!(validated.block.view(), expected_view);
        }

        service.shutdown();
    }

    #[test]
    fn graceful_shutdown_with_pending_blocks() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let shutdown = Arc::new(AtomicBool::new(false));

        let (mut service, mut producer, _consumer, _writer) = BlockValidationService::spawn(
            store,
            0,
            Arc::clone(&shutdown),
            slog::Logger::root(slog::Discard, slog::o!()),
        );

        // Push some blocks
        for view in 1..=3 {
            let block = create_test_block(view, vec![]);
            producer.push(block).unwrap();
        }

        // Immediately signal shutdown
        shutdown.store(true, Ordering::Release);

        // Should complete without hanging
        service.shutdown();
        assert!(!service.is_running());
    }

    #[test]
    fn handles_rapid_block_submission() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        let (mut service, mut producer, mut consumer, _writer) = BlockValidationService::spawn(
            store,
            0,
            Arc::new(AtomicBool::new(false)),
            slog::Logger::root(slog::Discard, slog::o!()),
        );

        let num_blocks = RING_BUFFER_SIZE - 1; // Just under capacity

        // Rapidly push blocks
        for view in 1..=num_blocks as u64 {
            let block = create_test_block(view, vec![]);
            producer.push(block).unwrap();
        }

        // Wait for all blocks to be processed with timeout
        // Use a polling loop instead of fixed sleep for reliability
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(2);
        let mut received = 0;

        while received < num_blocks && start.elapsed() < timeout {
            while consumer.pop().is_ok() {
                received += 1;
            }
            if received < num_blocks {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }

        assert_eq!(received, num_blocks);
        service.shutdown();
    }

    #[test]
    fn filters_invalid_blocks_passes_valid() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        // Create two funded accounts
        let (sk1, sender1) = gen_keypair();
        let (sk2, sender2) = gen_keypair();
        store
            .put_account(&Account::new(sk1.public_key(), 10_000, 0))
            .unwrap();
        store
            .put_account(&Account::new(sk2.public_key(), 10_000, 0))
            .unwrap();

        let (_, recipient_addr) = gen_keypair();
        let (bad_sk, bad_sender) = gen_keypair(); // Unfunded

        let (mut service, mut producer, mut consumer, _writer) = BlockValidationService::spawn(
            Arc::clone(&store),
            0,
            Arc::new(AtomicBool::new(false)),
            slog::Logger::root(slog::Discard, slog::o!()),
        );

        // View 1: Valid transfer
        let valid_tx = Transaction::new_transfer(sender1, recipient_addr, 100, 0, 10, &sk1);
        producer
            .push(create_test_block(1, vec![Arc::new(valid_tx)]))
            .unwrap();

        // View 2: Invalid (unfunded sender)
        let invalid_tx = Transaction::new_transfer(bad_sender, recipient_addr, 100, 0, 10, &bad_sk);
        producer
            .push(create_test_block(2, vec![Arc::new(invalid_tx)]))
            .unwrap();

        // View 3: Valid transfer from sender2 (independent, nonce 0)
        let valid_tx2 = Transaction::new_transfer(sender2, recipient_addr, 50, 0, 5, &sk2);
        producer
            .push(create_test_block(3, vec![Arc::new(valid_tx2)]))
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        // Should receive views 1 and 3, not 2
        let v1 = consumer.pop().unwrap();
        assert_eq!(v1.block.view(), 1);

        let v3 = consumer.pop().unwrap();
        assert_eq!(v3.block.view(), 3);

        // No more
        assert!(consumer.pop().is_err());

        service.shutdown();
    }

    #[test]
    fn pending_state_visible_during_validation() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        let (sk1, addr1) = gen_keypair();
        let (sk2, addr2) = gen_keypair();
        let (_, addr3) = gen_keypair();

        // Fund addr1 in DB
        store
            .put_account(&Account::new(sk1.public_key(), 1000, 0))
            .unwrap();

        let (mut service, mut producer, mut consumer, mut writer) = BlockValidationService::spawn(
            Arc::clone(&store),
            0,
            Arc::new(AtomicBool::new(false)),
            slog::Logger::root(slog::Discard, slog::o!()),
        );

        // Simulate m-notarization: addr1 sends to addr2 (implicitly creates addr2)
        let mut diff1 = StateDiff::new();
        diff1.add_balance_change(addr1, -110, 1); // -100 transfer - 10 fee, nonce 0→1
        diff1.add_balance_change(addr2, 100, 0); // Receives 100
        diff1.total_fees = 10;
        writer.add_m_notarized_diff(1, Arc::new(diff1));

        // Now addr2 should be able to send (balance 100 from pending state)
        let tx = Transaction::new_transfer(addr2, addr3, 50, 0, 5, &sk2);
        let block = create_test_block(2, vec![Arc::new(tx)]);
        producer.push(block).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let validated = consumer.pop().unwrap();
        assert_eq!(validated.block.view(), 2);
        assert!(validated.state_diff.updates.contains_key(&addr2));
        assert!(validated.state_diff.updates.contains_key(&addr3));

        service.shutdown();
    }

    #[test]
    fn validated_block_has_timestamp() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        let (mut service, mut producer, mut consumer, _writer) = BlockValidationService::spawn(
            store,
            0,
            Arc::new(AtomicBool::new(false)),
            slog::Logger::root(slog::Discard, slog::o!()),
        );

        let before = Instant::now();
        producer.push(create_test_block(1, vec![])).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));

        let validated = consumer.pop().unwrap();
        let after = Instant::now();

        // validated_at should be between before and after
        assert!(validated.validated_at >= before);
        assert!(validated.validated_at <= after);

        service.shutdown();
    }
}
