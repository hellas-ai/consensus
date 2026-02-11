//! P2P node service orchestrating network and protocols.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use bytes::Bytes;
use commonware_codec::ReadExt;
use commonware_cryptography::{Signer, ed25519};
use commonware_p2p::Receiver;
use commonware_runtime::{Clock, Metrics, Network, Resolver, Runner, Spawner};
use consensus::consensus::ConsensusMessage;
use consensus::crypto::aggregated::PeerId;
use consensus::state::transaction::Transaction;
use consensus::storage::store::ConsensusStore;
use crossbeam::queue::ArrayQueue;
use rand::{CryptoRng, RngCore};
use rtrb::{Consumer, Producer};
use slog::Logger;
use tokio::sync::Notify;

use crate::ValidatorIdentity;
use crate::config::P2PConfig;
use crate::error::P2PError;
use crate::message::{P2PMessage, deserialize_message, serialize_message};
use crate::network::{NetworkReceivers, NetworkService};

/// P2P service handle returned after spawning.
pub struct P2PHandle {
    /// Thread join handle.
    pub thread_handle: JoinHandle<()>,
    /// Shutdown signal.
    pub shutdown: Arc<AtomicBool>,
    /// Notify to wake up the service when shutdown is requested.
    /// This ensures the service exits immediately rather than waiting for a recv timeout.
    pub shutdown_notify: Arc<Notify>,
    /// Notify to wake up the service when consensus broadcast queue has data.
    /// Producer should call `broadcast_notify.notify_one()` after pushing.
    pub broadcast_notify: Arc<Notify>,
    /// Lock-free queue for outgoing transaction broadcasts (MPSC: multiple producers, single
    /// consumer). Uses crossbeam ArrayQueue which is Sync, allowing multiple gRPC handlers to
    /// push without a Mutex.
    pub tx_broadcast_queue: Arc<ArrayQueue<Transaction>>,
    /// Notify to wake up the service when transaction broadcast queue has data.
    pub tx_broadcast_notify: Arc<Notify>,
    /// Notify signaled when P2P is ready (bootstrap phase completed).
    pub ready_notify: Arc<Notify>,
    /// Flag indicating if P2P is ready.
    pub is_ready: Arc<AtomicBool>,
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

    /// Wait for the P2P service to be ready (bootstrap phase completed).
    /// Returns immediately if already ready.
    pub async fn wait_ready(&self) {
        if self.is_ready() {
            return;
        }
        self.ready_notify.notified().await;
    }

    /// Check if P2P is ready without blocking.
    pub fn is_ready(&self) -> bool {
        self.is_ready.load(Ordering::Acquire)
    }

    /// Broadcast a transaction to all peers.
    ///
    /// This queues the transaction for broadcast via the P2P network.
    /// The transaction will be sent to all connected peers.
    ///
    /// Note: This method takes `&self` (not `&mut self`) because the underlying
    /// ArrayQueue is lock-free and Sync, allowing concurrent pushes from multiple
    /// gRPC handlers without requiring a Mutex.
    pub fn broadcast_transaction(&self, tx: Transaction) -> Result<(), P2PError> {
        self.tx_broadcast_queue
            .push(tx)
            .map_err(|_| P2PError::QueueFull)?;
        self.tx_broadcast_notify.notify_one();
        Ok(())
    }
}

/// Spawn the P2P service on a new thread.
#[allow(clippy::too_many_arguments)]
pub fn spawn<E, const N: usize, const F: usize, const M_SIZE: usize>(
    runner: E,
    config: P2PConfig,
    identity: ValidatorIdentity,
    consensus_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,
    tx_producer: Producer<Transaction>,
    broadcast_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,
    store: Option<Arc<ConsensusStore>>,
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
    let tx_broadcast_notify = Arc::new(Notify::new());
    let tx_broadcast_notify_clone = Arc::clone(&tx_broadcast_notify);
    let ready_notify = Arc::new(Notify::new());
    let ready_notify_clone = Arc::clone(&ready_notify);
    let is_ready = Arc::new(AtomicBool::new(false));
    let is_ready_clone = Arc::clone(&is_ready);

    // Create transaction broadcast queue (lock-free MPSC)
    let tx_broadcast_queue = Arc::new(ArrayQueue::new(config.tx_broadcast_queue_size));
    let tx_broadcast_queue_clone = Arc::clone(&tx_broadcast_queue);

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
                    tx_broadcast_queue_clone,
                    store,
                    shutdown_clone,
                    shutdown_notify_clone,
                    broadcast_notify_clone,
                    tx_broadcast_notify_clone,
                    ready_notify_clone,
                    is_ready_clone,
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
        tx_broadcast_queue,
        tx_broadcast_notify,
        ready_notify,
        is_ready,
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
    tx_broadcast_queue: Arc<ArrayQueue<Transaction>>,
    store: Option<Arc<ConsensusStore>>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
    broadcast_notify: Arc<Notify>,
    tx_broadcast_notify: Arc<Notify>,
    ready_notify: Arc<Notify>,
    is_ready: Arc<AtomicBool>,
    logger: Logger,
) where
    C: Spawner + Clock + Network + Resolver + Metrics + RngCore + CryptoRng,
{
    slog::info!(logger, "Starting P2P service";
        "ed25519_public_key" => ?signer.public_key(),
        "bls_peer_id" => ?peer_id
    );

    // Parse expected peer public keys from config
    let expected_peers: HashSet<ed25519::PublicKey> = config
        .validators
        .iter()
        .filter_map(|v| {
            let pk_bytes = v.parse_public_key_bytes()?;
            ed25519::PublicKey::read(&mut pk_bytes.as_slice()).ok()
        })
        .filter(|pk| pk != &signer.public_key()) // Exclude self
        .collect();

    // For reliable consensus startup, we should wait for all peers to be connected.
    //
    // Wait for all expected peers (n - 1, excluding ourselves)
    let n = config.total_number_peers;
    let min_other_peers = n.saturating_sub(1);
    // But also cap at the number of expected peers we actually have in config
    let min_peers = min_other_peers.min(expected_peers.len());

    // Initialize Network
    let (mut network, mut receivers) = NetworkService::<C>::new(
        context.clone(),
        signer.clone(),
        config.clone(),
        logger.clone(),
    )
    .await;

    // Bootstrap phase: wait for peer readiness
    let bootstrap_success = run_bootstrap_phase::<C, N, F, M_SIZE>(
        &context,
        &mut network,
        &mut receivers,
        &expected_peers,
        min_peers,
        Duration::from_millis(config.bootstrap_timeout_ms),
        Duration::from_millis(config.ping_interval_ms),
        &shutdown,
        &shutdown_notify,
        &logger,
    )
    .await;

    if shutdown.load(Ordering::Relaxed) {
        slog::info!(logger, "Shutdown during bootstrap");
        drop(network);
        return;
    }

    if bootstrap_success {
        slog::info!(logger, "Bootstrap complete, P2P ready"; "peers" => min_peers);
    } else {
        slog::warn!(logger, "Bootstrap timeout, proceeding anyway"; "min_peers" => min_peers);
    }

    // Signal readiness
    is_ready.store(true, Ordering::Release);
    ready_notify.notify_waiters();

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

            // 4. Outgoing Transaction Broadcasts
            _ = tx_broadcast_notify.notified() => {
                while let Some(tx) = tx_broadcast_queue.pop() {
                    match crate::message::serialize_message(&P2PMessage::<N, F, M_SIZE>::Transaction(tx)) {
                        Ok(bytes) => {
                            network.broadcast_transaction(bytes, vec![]).await;
                        }
                        Err(e) => {
                            slog::error!(logger, "Failed to serialize transaction broadcast"; "error" => ?e);
                        }
                    }
                }
            }

            // 5. Incoming Sync Messages (lowest priority)
            res = receivers.sync.recv() => {
                match res {
                    Ok((sender, msg)) => {
                        let msg: Bytes = msg;
                        // Handle Ping/Pong for liveness checks
                        if let Ok(p2p_msg) = deserialize_message::<N, F, M_SIZE>(&msg) {
                            match p2p_msg {
                                P2PMessage::Ping(ts) => {
                                    // Respond with Pong
                                    if let Ok(pong_bytes) = serialize_message(&P2PMessage::<N, F, M_SIZE>::Pong(ts)) {
                                        network.send_sync(pong_bytes, vec![sender]).await;
                                    }
                                }
                                P2PMessage::Pong(_ts) => {
                                    // Liveness confirmed (could track for metrics)
                                }
                                P2PMessage::BlockRequest(req) => {
                                    // Handle block request from RPC nodes
                                    // Note: req.view is actually the block height in current usage
                                    if let Some(ref st) = store {
                                        let response = match st.get_finalized_block_by_height(req.view) {
                                            Ok(Some(block)) => {
                                                // Serialize block
                                                let block_bytes = consensus::storage::conversions::serialize_for_db(&block)
                                                    .map(|b| b.to_vec())
                                                    .unwrap_or_default();

                                                // Get L-notarization for this block (if hash is available)
                                                let l_notarization_bytes = block.hash.as_ref()
                                                    .and_then(|hash| {
                                                        st.get_l_notarization::<N, F>(hash)
                                                            .ok()
                                                            .flatten()
                                                    })
                                                    .and_then(|l_not| {
                                                        consensus::storage::conversions::serialize_for_db(&l_not)
                                                            .map(|b| b.to_vec())
                                                            .ok()
                                                    });

                                                slog::debug!(logger, "Responding to block request";
                                                    "view" => req.view,
                                                    "has_l_notarization" => l_notarization_bytes.is_some()
                                                );

                                                crate::message::BlockResponse::Found {
                                                    block_bytes,
                                                    l_notarization_bytes,
                                                }
                                            }
                                            Ok(None) => {
                                                slog::debug!(logger, "Block not found for request";
                                                    "view" => req.view
                                                );
                                                crate::message::BlockResponse::NotFound { view: req.view }
                                            }
                                            Err(e) => {
                                                slog::warn!(logger, "Error fetching block for request";
                                                    "view" => req.view,
                                                    "error" => ?e
                                                );
                                                crate::message::BlockResponse::NotFound { view: req.view }
                                            }
                                        };

                                        // Send response back to requester
                                        if let Ok(response_bytes) = serialize_message(&P2PMessage::<N, F, M_SIZE>::BlockResponse(response)) {
                                            network.send_sync(response_bytes, vec![sender]).await;
                                        }
                                    } else {
                                        slog::debug!(logger, "Received block request but no store available"; "view" => req.view);
                                    }
                                }
                                _ => {
                                    slog::debug!(logger, "Received sync message"; "peer" => ?sender, "len" => msg.len());
                                }
                            }
                        }
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

/// Bootstrap phase: discover peers and verify connectivity.
///
/// Returns `true` if minimum peers were discovered, `false` on timeout.
#[allow(clippy::too_many_arguments)]
async fn run_bootstrap_phase<C, const N: usize, const F: usize, const M_SIZE: usize>(
    context: &C,
    network: &mut NetworkService<C>,
    receivers: &mut NetworkReceivers,
    expected_peers: &HashSet<ed25519::PublicKey>,
    min_peers: usize,
    timeout: Duration,
    ping_interval: Duration,
    shutdown: &Arc<AtomicBool>,
    shutdown_notify: &Arc<Notify>,
    logger: &Logger,
) -> bool
where
    C: Spawner + Clock + Network + Resolver + Metrics + RngCore + CryptoRng,
{
    use crate::message::{P2PMessage, serialize_message};

    if min_peers == 0 {
        slog::info!(logger, "No peers required, skipping bootstrap");
        return true;
    }

    slog::info!(logger, "Starting bootstrap phase";
        "expected_peers" => expected_peers.len(),
        "min_peers" => min_peers,
        "timeout_ms" => timeout.as_millis()
    );

    let mut ready_peers: HashSet<ed25519::PublicKey> = HashSet::new();
    let start = std::time::Instant::now();
    let mut last_ping = std::time::Instant::now() - ping_interval; // Trigger immediate ping

    loop {
        // Check shutdown
        if shutdown.load(Ordering::Relaxed) {
            return false;
        }

        // Check timeout
        if start.elapsed() >= timeout {
            slog::warn!(logger, "Bootstrap timeout"; "ready_peers" => ready_peers.len());
            return false;
        }

        // Check if we have enough peers
        if ready_peers.len() >= min_peers {
            slog::info!(logger, "Bootstrap threshold reached"; "ready_peers" => ready_peers.len());
            return true;
        }

        // Send pings periodically
        if last_ping.elapsed() >= ping_interval {
            let timestamp = context
                .current()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;

            if let Ok(ping_bytes) = serialize_message(&P2PMessage::<N, F, M_SIZE>::Ping(timestamp))
            {
                // Broadcast ping to all peers
                network.send_sync(ping_bytes, vec![]).await;
                slog::debug!(logger, "Sent bootstrap ping"; "timestamp" => timestamp);
            }
            last_ping = std::time::Instant::now();
        }

        // Wait for responses (with short timeout for responsiveness)
        let recv_timeout = Duration::from_millis(100);

        tokio::select! {
            biased;

            _ = shutdown_notify.notified() => {
                return false;
            }

            // Prioritize message reception over sleep to avoid delaying peer discovery.
            // When both a message and the sleep timer are ready simultaneously,
            // biased select picks the first matching branch.
            res = receivers.sync.recv() => {
                if let Ok((sender, msg)) = res && let Ok(p2p_msg) = deserialize_message::<N, F, M_SIZE>(&msg) {
                    match p2p_msg {
                        P2PMessage::Ping(ts) => {
                            // Respond with Pong
                            if let Ok(pong_bytes) = serialize_message(&P2PMessage::<N, F, M_SIZE>::Pong(ts)) {
                                network.send_sync(pong_bytes, vec![sender.clone()]).await;
                            }
                            // Also mark peer as ready (they can reach us)
                            if expected_peers.contains(&sender) && ready_peers.insert(sender.clone()) {
                                slog::info!(logger, "Peer ready (received ping)"; "peer" => ?sender, "total" => ready_peers.len());
                            }
                        }
                        P2PMessage::Pong(_ts) => {
                            // Peer responded to our ping
                            if expected_peers.contains(&sender) && ready_peers.insert(sender.clone()) {
                                slog::info!(logger, "Peer ready (received pong)"; "peer" => ?sender, "total" => ready_peers.len());
                            }
                        }
                        _ => {}
                    }
                }
            }

            _ = context.sleep(recv_timeout) => {
                // No messages received within timeout, continue loop
            }

            // NOTE: We intentionally do NOT listen on consensus or tx channels during bootstrap.
            // Consuming messages from those channels would drop them, as they can't be re-sent.
            // The sync channel ping/pong is sufficient for readiness detection.
        }
    }
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

    /// Helper to create a P2PHandle for testing.
    fn create_test_handle(
        shutdown: Arc<AtomicBool>,
        shutdown_notify: Arc<Notify>,
        is_ready: Arc<AtomicBool>,
        ready_notify: Arc<Notify>,
    ) -> P2PHandle {
        P2PHandle {
            thread_handle: std::thread::spawn(|| {}),
            shutdown,
            shutdown_notify,
            broadcast_notify: Arc::new(Notify::new()),
            tx_broadcast_queue: Arc::new(ArrayQueue::new(100)),
            tx_broadcast_notify: Arc::new(Notify::new()),
            ready_notify,
            is_ready,
        }
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
        let is_ready = Arc::new(AtomicBool::new(false));

        // Create a dummy handle (we can't easily test the full spawn without a real runtime)
        // But we can test the shutdown method
        let handle = create_test_handle(
            shutdown.clone(),
            shutdown_notify.clone(),
            is_ready.clone(),
            Arc::new(Notify::new()),
        );

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
        let is_ready = Arc::new(AtomicBool::new(false));

        let handle = create_test_handle(
            shutdown.clone(),
            shutdown_notify.clone(),
            is_ready.clone(),
            Arc::new(Notify::new()),
        );

        // Call shutdown multiple times - should be idempotent
        handle.shutdown();
        handle.shutdown();
        handle.shutdown();

        assert!(shutdown.load(Ordering::Relaxed));

        // Cleanup
        let _ = handle.join();
    }

    #[test]
    fn test_p2p_handle_is_ready() {
        let is_ready = Arc::new(AtomicBool::new(false));

        let handle = create_test_handle(
            Arc::new(AtomicBool::new(false)),
            Arc::new(Notify::new()),
            is_ready.clone(),
            Arc::new(Notify::new()),
        );

        // Initially not ready
        assert!(!handle.is_ready());

        // Simulate becoming ready
        is_ready.store(true, Ordering::Release);
        assert!(handle.is_ready());

        // Cleanup
        let _ = handle.join();
    }

    #[tokio::test]
    async fn test_p2p_handle_wait_ready_already_ready() {
        let is_ready = Arc::new(AtomicBool::new(true));
        let ready_notify = Arc::new(Notify::new());

        let handle = create_test_handle(
            Arc::new(AtomicBool::new(false)),
            Arc::new(Notify::new()),
            is_ready.clone(),
            ready_notify.clone(),
        );

        // Should return immediately if already ready
        let start = std::time::Instant::now();
        handle.wait_ready().await;
        let elapsed = start.elapsed();

        // Should return almost immediately (< 10ms)
        assert!(elapsed < Duration::from_millis(10));

        // Cleanup
        let _ = handle.join();
    }

    #[tokio::test]
    async fn test_p2p_handle_wait_ready_becomes_ready() {
        let is_ready = Arc::new(AtomicBool::new(false));
        let ready_notify = Arc::new(Notify::new());

        let handle = create_test_handle(
            Arc::new(AtomicBool::new(false)),
            Arc::new(Notify::new()),
            is_ready.clone(),
            ready_notify.clone(),
        );

        // Spawn a task that will signal ready after a delay
        let ready_notify_clone = ready_notify.clone();
        let is_ready_clone = is_ready.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            is_ready_clone.store(true, Ordering::Release);
            ready_notify_clone.notify_waiters();
        });

        // wait_ready should block until ready
        let start = std::time::Instant::now();
        handle.wait_ready().await;
        let elapsed = start.elapsed();

        // Should have waited at least 50ms
        assert!(elapsed >= Duration::from_millis(45));
        assert!(elapsed < Duration::from_millis(200)); // But not too long
        assert!(handle.is_ready());

        // Cleanup
        let _ = handle.join();
    }
}
