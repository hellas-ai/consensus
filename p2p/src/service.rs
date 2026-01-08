//! P2P node service orchestrating network and protocols.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use bytes::Bytes;
use commonware_cryptography::{Signer, ed25519};
use commonware_p2p::Receiver;
use commonware_runtime::{Clock, Metrics, Network, Resolver, Runner, Spawner};
use consensus::consensus::ConsensusMessage;
use consensus::crypto::aggregated::PeerId;
use consensus::state::transaction::Transaction;
use rand::{CryptoRng, RngCore};
use rtrb::{Consumer, Producer};
use slog::Logger;
use tokio::sync::Notify;

use crate::ValidatorIdentity;
use crate::config::P2PConfig;
use crate::error::P2PError;
use crate::message::{P2PMessage, deserialize_message};
use crate::network::NetworkService;

/// P2P service handle returned after spawning.
pub struct P2PHandle {
    /// Thread join handle.
    pub thread_handle: JoinHandle<()>,
    /// Shutdown signal.
    pub shutdown: Arc<AtomicBool>,
    /// Notify to wake up the service when shutdown is requested.
    /// This ensures the service exits immediately rather than waiting for a recv timeout.
    pub shutdown_notify: Arc<Notify>,
    /// Notify to wake up the service when broadcast queue has data.
    /// Producer should call `broadcast_notify.notify_one()` after pushing.
    pub broadcast_notify: Arc<Notify>,
}

impl P2PHandle {
    /// Signal the P2P thread to shutdown and wake it immediately.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.shutdown_notify.notify_one();
    }

    /// Wait for the P2P thread to finish.
    pub fn join(self) -> std::thread::Result<()> {
        self.thread_handle.join()
    }
}

/// Spawn the P2P service on a new thread.
pub fn spawn<E, const N: usize, const F: usize, const M_SIZE: usize>(
    runner: E,
    config: P2PConfig,
    identity: ValidatorIdentity,
    consensus_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,
    tx_producer: Producer<Transaction>,
    broadcast_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,
    logger: Logger,
) -> P2PHandle
where
    E: Runner + Send + 'static,
    E::Context:
        Spawner + Clock + Network + Resolver + Metrics + RngCore + CryptoRng + Send + 'static,
{
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let shutdown_notify = Arc::new(Notify::new());
    let shutdown_notify_clone = Arc::clone(&shutdown_notify);
    let broadcast_notify = Arc::new(Notify::new());
    let broadcast_notify_clone = Arc::clone(&broadcast_notify);

    // Extract the ed25519 key for network transport
    let signer = identity.clone_ed25519_private_key();
    let peer_id = identity.peer_id();

    let thread_handle = std::thread::Builder::new()
        .name("hellas-validator-p2p-thread".to_string())
        .spawn(move || {
            // Run the context
            runner.start(move |ctx| async move {
                run_p2p_service::<E::Context, N, F, M_SIZE>(
                    ctx,
                    config,
                    signer,
                    peer_id,
                    consensus_producer,
                    tx_producer,
                    broadcast_consumer,
                    shutdown_clone,
                    shutdown_notify_clone,
                    broadcast_notify_clone,
                    logger,
                )
                .await;
            });
        })
        .expect("Failed to spawn P2P thread");

    P2PHandle {
        thread_handle,
        shutdown,
        shutdown_notify,
        broadcast_notify,
    }
}

/// Run the P2P service main loop.
#[allow(clippy::too_many_arguments)]
async fn run_p2p_service<C, const N: usize, const F: usize, const M_SIZE: usize>(
    context: C,
    config: P2PConfig,
    signer: ed25519::PrivateKey,
    peer_id: PeerId,
    mut consensus_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,
    mut tx_producer: Producer<Transaction>,
    mut broadcast_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
    broadcast_notify: Arc<Notify>,
    logger: Logger,
) where
    C: Spawner + Clock + Network + Resolver + Metrics + RngCore + CryptoRng,
{
    slog::info!(logger, "Starting P2P service";
        "ed25519_public_key" => ?signer.public_key(),
        "bls_peer_id" => ?peer_id
    );

    // Initialize Network
    let (mut network, mut receivers) =
        NetworkService::<C>::new(context.clone(), signer, config, logger.clone()).await;

    // Main event loop
    while !shutdown.load(Ordering::Relaxed) {
        tokio::select! {
            biased;

            // 0. Shutdown notification (highest priority)
            _ = shutdown_notify.notified() => {
                slog::info!(logger, "Received shutdown signal");
                break;
            }

            // 1. Incoming Consensus Messages
            res = receivers.consensus.recv() => {
                match res {
                    Ok((sender, msg)) => {
                        let msg: Bytes = msg;
                        if let Err(e) = route_incoming_message::<N, F, M_SIZE>(&msg, &mut consensus_producer, &mut tx_producer, &logger) {
                            slog::debug!(logger, "Failed to route consensus message"; "error" => %e, "peer" => ?sender);
                        }
                    }
                    Err(e) => {
                         slog::error!(logger, "Consensus receiver error"; "error" => ?e);
                         break;
                    }
                }
            }

            // 2. Outgoing Broadcasts (high priority - our proposals/votes)
            _ = broadcast_notify.notified() => {
                 while let Ok(msg) = broadcast_consumer.pop() {
                    match crate::message::serialize_message(&P2PMessage::Consensus(msg)) {
                        Ok(bytes) => {
                             network.broadcast_consensus(bytes, vec![]).await;
                        }
                        Err(e) => {
                             slog::error!(logger, "Failed to serialize broadcast"; "error" => ?e);
                        }
                    }
                }
            }

            // 3. Incoming Transactions
            res = receivers.tx.recv() => {
                 match res {
                    Ok((sender, msg)) => {
                        let msg: Bytes = msg;
                        if let Err(e) = route_incoming_message::<N, F, M_SIZE>(&msg, &mut consensus_producer, &mut tx_producer, &logger) {
                           slog::debug!(logger, "Failed to route transaction"; "error" => %e, "peer" => ?sender);
                       }
                    }
                    Err(e) => {
                         slog::error!(logger, "Transaction receiver error"; "error" => ?e);
                         break;
                    }
                }
            }

             // 4. Incoming Sync Messages (lowest priority)
            res = receivers.sync.recv() => {
                 match res {
                    Ok((sender, msg)) => {
                        let msg: Bytes = msg;
                        slog::debug!(logger, "Received sync message"; "peer" => ?sender, "len" => msg.len());
                    }
                    Err(e) => {
                         slog::error!(logger, "Sync receiver error"; "error" => ?e);
                         break;
                    }
                }
            }
        }
    }

    // Let network be dropped naturally when this function returns
    // Calling network.shutdown() here would abort tasks during poll,
    // which can cause "Cannot drop a runtime in a context where blocking is not allowed" panics
    // The network handle will be dropped when it goes out of scope
    drop(network);
    slog::info!(logger, "P2P service shut down");
}

/// Route an incoming network message to the appropriate channel.
pub fn route_incoming_message<const N: usize, const F: usize, const M_SIZE: usize>(
    bytes: &[u8],
    consensus_producer: &mut Producer<ConsensusMessage<N, F, M_SIZE>>,
    tx_producer: &mut Producer<Transaction>,
    logger: &Logger,
) -> Result<(), P2PError> {
    let msg = deserialize_message::<N, F, M_SIZE>(bytes)?;

    match msg {
        P2PMessage::Consensus(consensus_msg) => {
            if let Err(_e) = consensus_producer.push(consensus_msg) {
                slog::warn!(logger, "Consensus channel full");
                return Err(P2PError::SendError("Consensus channel full".to_string()));
            }
        }
        P2PMessage::Transaction(tx) => {
            if let Err(_e) = tx_producer.push(tx) {
                slog::warn!(logger, "Transaction channel full");
                return Err(P2PError::SendError("Transaction channel full".to_string()));
            }
        }
        // TODO: ... (handle other types)
        _ => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use consensus::crypto::aggregated::BlsSecretKey;
    use consensus::state::block::Block;
    use consensus::state::transaction::Transaction;
    use rtrb::RingBuffer;
    use slog::Logger;

    const N: usize = 6;
    const F: usize = 1;
    const M_SIZE: usize = 3;

    fn create_test_logger() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn test_route_consensus_message() {
        let logger = create_test_logger();
        let (mut consensus_prod, mut consensus_cons) = RingBuffer::new(100);
        let (mut tx_prod, _tx_cons) = RingBuffer::new(100);

        // Create a test consensus message
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
        let consensus_msg = ConsensusMessage::<N, F, M_SIZE>::BlockProposal(block);

        // Serialize it
        let p2p_msg = P2PMessage::Consensus(consensus_msg.clone());
        let bytes = crate::message::serialize_message(&p2p_msg).unwrap();

        // Route it
        let result = route_incoming_message::<N, F, M_SIZE>(
            &bytes,
            &mut consensus_prod,
            &mut tx_prod,
            &logger,
        );

        assert!(result.is_ok(), "Routing should succeed");

        // Verify message was pushed to consensus channel (check consumer side)
        let received = consensus_cons.pop().unwrap();
        match (received, consensus_msg) {
            (ConsensusMessage::BlockProposal(b1), ConsensusMessage::BlockProposal(b2)) => {
                assert_eq!(b1.view(), b2.view());
            }
            _ => panic!("Message type mismatch"),
        }
    }

    #[test]
    fn test_route_transaction_message() {
        let logger = create_test_logger();
        let (mut consensus_prod, _consensus_cons) = RingBuffer::new(100);
        let (mut tx_prod, mut tx_cons) = RingBuffer::new(100);

        // Create a test transaction
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

        // Serialize it
        let p2p_msg: P2PMessage<N, F, M_SIZE> = P2PMessage::Transaction(tx.clone());
        let bytes = crate::message::serialize_message(&p2p_msg).unwrap();

        // Route it
        let result = route_incoming_message::<N, F, M_SIZE>(
            &bytes,
            &mut consensus_prod,
            &mut tx_prod,
            &logger,
        );

        assert!(result.is_ok(), "Routing should succeed");

        // Verify message was pushed to transaction channel (check consumer side)
        let received = tx_cons.pop().unwrap();
        assert_eq!(received.tx_hash, tx.tx_hash);
    }

    #[test]
    fn test_route_message_channel_full() {
        let logger = create_test_logger();
        let (mut consensus_prod, _consensus_cons) = RingBuffer::new(1); // Small buffer
        let (mut tx_prod, _tx_cons) = RingBuffer::new(100);

        // Fill the consensus channel
        let block1 = Block::new(
            1,
            12345,
            [0u8; 32],
            vec![],
            1234567890,
            BlsSecretKey::generate(&mut rand::thread_rng()).sign(b"test1"),
            false,
            1,
        );
        let msg1 = ConsensusMessage::<N, F, M_SIZE>::BlockProposal(block1);
        consensus_prod.push(msg1).unwrap();

        // Try to route another message - should fail with channel full
        let block2 = Block::new(
            2,
            12345,
            [0u8; 32],
            vec![],
            1234567891,
            BlsSecretKey::generate(&mut rand::thread_rng()).sign(b"test2"),
            false,
            1,
        );
        let p2p_msg =
            P2PMessage::Consensus(ConsensusMessage::<N, F, M_SIZE>::BlockProposal(block2));
        let bytes = crate::message::serialize_message(&p2p_msg).unwrap();

        let result = route_incoming_message::<N, F, M_SIZE>(
            &bytes,
            &mut consensus_prod,
            &mut tx_prod,
            &logger,
        );

        assert!(result.is_err(), "Routing should fail when channel is full");
        match result.unwrap_err() {
            P2PError::SendError(msg) => {
                assert!(msg.contains("Consensus channel full"));
            }
            _ => panic!("Expected SendError"),
        }
    }

    #[test]
    fn test_route_invalid_message() {
        let logger = create_test_logger();
        let (mut consensus_prod, _consensus_cons) = RingBuffer::new(100);
        let (mut tx_prod, _tx_cons) = RingBuffer::new(100);

        // Invalid bytes
        let invalid_bytes = b"not a valid message".to_vec();

        let result = route_incoming_message::<N, F, M_SIZE>(
            &invalid_bytes,
            &mut consensus_prod,
            &mut tx_prod,
            &logger,
        );

        assert!(result.is_err(), "Routing invalid message should fail");
    }

    #[test]
    fn test_route_unsupported_message_type() {
        let logger = create_test_logger();
        let (mut consensus_prod, mut consensus_cons) = RingBuffer::new(100);
        let (mut tx_prod, mut tx_cons) = RingBuffer::new(100);

        // Create a Ping message (not currently handled)
        let p2p_msg = P2PMessage::<N, F, M_SIZE>::Ping(12345);
        let bytes = crate::message::serialize_message(&p2p_msg).unwrap();

        // Should succeed but not push anything (returns Ok(()))
        let result = route_incoming_message::<N, F, M_SIZE>(
            &bytes,
            &mut consensus_prod,
            &mut tx_prod,
            &logger,
        );

        assert!(result.is_ok(), "Unsupported message types should return Ok");
        // Verify nothing was pushed (consumers should be empty)
        assert!(consensus_cons.pop().is_err());
        assert!(tx_cons.pop().is_err());
    }

    #[test]
    fn test_p2p_handle_shutdown() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_notify = Arc::new(Notify::new());

        // Create a dummy handle (we can't easily test the full spawn without a real runtime)
        // But we can test the shutdown method
        let handle = P2PHandle {
            thread_handle: std::thread::spawn(|| {}),
            shutdown: shutdown.clone(),
            shutdown_notify: shutdown_notify.clone(),
            broadcast_notify: Arc::new(Notify::new()),
        };

        assert!(!shutdown.load(Ordering::Relaxed));
        handle.shutdown();
        assert!(shutdown.load(Ordering::Relaxed));

        // Cleanup
        let _ = handle.join();
    }

    #[test]
    fn test_p2p_handle_shutdown_idempotent() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_notify = Arc::new(Notify::new());

        let handle = P2PHandle {
            thread_handle: std::thread::spawn(|| {}),
            shutdown: shutdown.clone(),
            shutdown_notify: shutdown_notify.clone(),
            broadcast_notify: Arc::new(Notify::new()),
        };

        // Call shutdown multiple times - should be idempotent
        handle.shutdown();
        handle.shutdown();
        handle.shutdown();

        assert!(shutdown.load(Ordering::Relaxed));

        // Cleanup
        let _ = handle.join();
    }
}
