//! P2P node service orchestrating network and protocols.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use bytes::Bytes;
use commonware_cryptography::{Signer, ed25519};
use commonware_p2p::Receiver;
use commonware_runtime::{Clock, Metrics, Network, Resolver, Runner, Spawner};
use consensus::consensus::ConsensusMessage;
use consensus::state::transaction::Transaction;
use rand::{CryptoRng, RngCore};
use rtrb::{Consumer, Producer};
use slog::Logger;
use tokio::sync::Notify;

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
    /// Notify to wake up the service when broadcast queue has data.
    /// Producer should call `broadcast_notify.notify_one()` after pushing.
    pub broadcast_notify: Arc<Notify>,
}

impl P2PHandle {
    /// Signal the P2P thread to shutdown.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
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
    signer: ed25519::PrivateKey,
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
    let broadcast_notify = Arc::new(Notify::new());
    let broadcast_notify_clone = Arc::clone(&broadcast_notify);

    let thread_handle = std::thread::Builder::new()
        .name("hellas-validator-p2p-thread".to_string())
        .spawn(move || {
            // Run the context
            runner.start(move |ctx| async move {
                run_p2p_service::<E::Context, N, F, M_SIZE>(
                    ctx,
                    config,
                    signer,
                    consensus_producer,
                    tx_producer,
                    broadcast_consumer,
                    shutdown_clone,
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
        broadcast_notify,
    }
}

/// Run the P2P service main loop.
async fn run_p2p_service<C, const N: usize, const F: usize, const M_SIZE: usize>(
    context: C,
    config: P2PConfig,
    signer: ed25519::PrivateKey,
    mut consensus_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,
    mut tx_producer: Producer<Transaction>,
    mut broadcast_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,
    shutdown: Arc<AtomicBool>,
    broadcast_notify: Arc<Notify>,
    logger: Logger,
) where
    C: Spawner + Clock + Network + Resolver + Metrics + RngCore + CryptoRng,
{
    slog::info!(logger, "Starting P2P service"; "public_key" => ?signer.public_key());

    // Initialize Network
    let (mut network, mut receivers) =
        NetworkService::<C>::new(context.clone(), signer, config, logger.clone()).await;

    // Main event loop
    while !shutdown.load(Ordering::Relaxed) {
        tokio::select! {
            biased;

            // 1. Incoming Consensus Messages (highest priority)
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

    slog::info!(logger, "P2P service shutting down");
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
