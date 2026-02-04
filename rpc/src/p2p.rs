//! RPC node P2P service for block synchronization.
//!
//! This module provides a simplified P2P service for RPC nodes that only
//! handles block sync (no consensus messages, no transaction gossip).
//!
//! The service exposes:
//! - A receiver for incoming block responses
//! - A sender for outgoing block requests

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use bytes::Bytes;
use commonware_cryptography::ed25519;
use commonware_p2p::Receiver as ReceiverTrait;
use commonware_runtime::{Clock, Metrics, Network, Resolver, Runner, Spawner};
use rand::{CryptoRng, RngCore};
use slog::Logger;
use tokio::sync::Notify;

use p2p::config::P2PConfig;
use p2p::message::{
    BlockRequest, BlockResponse, P2PMessage, deserialize_message, serialize_message,
};
use p2p::network::NetworkService;

use crate::RpcIdentity;

/// Command to send a block request to a specific validator.
pub struct BlockRequestCommand {
    /// The validator to send the request to.
    pub target: ed25519::PublicKey,
    /// The block request.
    pub request: BlockRequest,
}

/// P2P handle for RPC nodes.
pub struct RpcP2PHandle {
    /// Thread handle for the P2P service.
    thread_handle: JoinHandle<()>,
    /// Shutdown flag.
    shutdown: Arc<AtomicBool>,
    /// Shutdown notification.
    shutdown_notify: Arc<Notify>,
    /// Ready flag (bootstrap complete).
    is_ready: Arc<AtomicBool>,
    /// Ready notification.
    ready_notify: Arc<Notify>,
}

impl RpcP2PHandle {
    /// Signal shutdown.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.shutdown_notify.notify_one();
    }

    /// Wait for shutdown to complete.
    pub fn join(self) -> std::thread::Result<()> {
        self.thread_handle.join()
    }

    /// Wait for P2P to be ready (bootstrap complete).
    pub async fn wait_ready(&self) {
        if !self.is_ready.load(Ordering::Acquire) {
            self.ready_notify.notified().await;
        }
    }

    /// Check if P2P is ready.
    pub fn is_ready(&self) -> bool {
        self.is_ready.load(Ordering::Acquire)
    }
}

/// Spawn the RPC P2P service on a new thread.
///
/// This is a simplified version of the validator P2P service that only
/// handles block synchronization (no consensus, no tx gossip).
///
/// # Type Parameters
///
/// * `E` - The commonware runtime runner type
/// * `N` - Minimmit parameter: total number of validators (5f+1)
/// * `F` - Minimmit parameter: maximum faulty validators
/// * `M_SIZE` - Maximum mempool size per proposal
///
/// # Returns
///
/// A tuple of (handle, request_sender, response_receiver) for:
/// - Controlling the P2P lifecycle
/// - Sending block requests to validators
/// - Receiving block responses
pub fn spawn_rpc_p2p<E, const N: usize, const F: usize, const M_SIZE: usize>(
    runner: E,
    config: P2PConfig,
    identity: RpcIdentity,
    logger: Logger,
) -> (
    RpcP2PHandle,
    tokio::sync::mpsc::UnboundedSender<BlockRequestCommand>,
    tokio::sync::mpsc::UnboundedReceiver<(ed25519::PublicKey, BlockResponse)>,
)
where
    E: Runner + Send + 'static,
    E::Context:
        Spawner + Clock + Network + Resolver + Metrics + RngCore + CryptoRng + Send + 'static,
{
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let shutdown_notify = Arc::new(Notify::new());
    let shutdown_notify_clone = Arc::clone(&shutdown_notify);
    let is_ready = Arc::new(AtomicBool::new(false));
    let is_ready_clone = Arc::clone(&is_ready);
    let ready_notify = Arc::new(Notify::new());
    let ready_notify_clone = Arc::clone(&ready_notify);

    // Channel for sync responses (inbound)
    let (sync_tx, sync_rx) = tokio::sync::mpsc::unbounded_channel();

    // Channel for block requests (outbound)
    let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();

    // Extract the ed25519 key for network transport
    let signer = identity.clone_ed25519_private_key();

    let thread_handle = std::thread::Builder::new()
        .name("hellas-rpc-p2p-thread".to_string())
        .spawn(move || {
            runner.start(move |ctx| async move {
                run_rpc_p2p_service::<E::Context, N, F, M_SIZE>(
                    ctx,
                    config,
                    signer,
                    sync_tx,
                    request_rx,
                    shutdown_clone,
                    shutdown_notify_clone,
                    ready_notify_clone,
                    is_ready_clone,
                    logger,
                )
                .await;
            });
        })
        .expect("Failed to spawn RPC P2P thread");

    let handle = RpcP2PHandle {
        thread_handle,
        shutdown,
        shutdown_notify,
        is_ready,
        ready_notify,
    };

    (handle, request_tx, sync_rx)
}

/// Run the RPC P2P service main loop.
#[allow(clippy::too_many_arguments)]
async fn run_rpc_p2p_service<C, const N: usize, const F: usize, const M_SIZE: usize>(
    context: C,
    config: P2PConfig,
    signer: ed25519::PrivateKey,
    sync_tx: tokio::sync::mpsc::UnboundedSender<(ed25519::PublicKey, BlockResponse)>,
    mut request_rx: tokio::sync::mpsc::UnboundedReceiver<BlockRequestCommand>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
    ready_notify: Arc<Notify>,
    is_ready: Arc<AtomicBool>,
    logger: Logger,
) where
    C: Spawner + Clock + Network + Resolver + Metrics + RngCore + CryptoRng + Clone,
{
    use commonware_cryptography::Signer;
    use std::collections::HashSet;

    let public_key = signer.public_key();
    slog::info!(logger, "Starting RPC P2P service"; "public_key" => ?public_key);

    // Create network service
    let (mut network, mut receivers) =
        NetworkService::new(context.clone(), signer, config.clone(), logger.clone()).await;

    // Build expected peer set from validator config
    let expected_peers: HashSet<ed25519::PublicKey> = config
        .validators
        .iter()
        .filter_map(|v| {
            use commonware_codec::ReadExt;
            let pk_bytes = v.parse_public_key_bytes()?;
            ed25519::PublicKey::read(&mut pk_bytes.as_slice()).ok()
        })
        .collect();

    // Bootstrap phase: wait for at least one peer or timeout
    // Use config values for timeouts
    let bootstrap_timeout = Duration::from_millis(config.bootstrap_timeout_ms);
    let ping_interval = Duration::from_millis(config.ping_interval_ms);

    slog::info!(logger, "RPC P2P bootstrap starting";
        "expected_peers" => expected_peers.len(),
        "timeout_secs" => bootstrap_timeout.as_secs()
    );

    let ready_peers = run_bootstrap_phase::<C, N, F, M_SIZE>(
        &context,
        &mut network,
        &mut receivers,
        &expected_peers,
        bootstrap_timeout,
        ping_interval,
        &shutdown,
        &shutdown_notify,
        &logger,
    )
    .await;

    if ready_peers.is_empty() {
        slog::warn!(logger, "RPC P2P bootstrap timeout - no peers connected");
    } else {
        slog::info!(logger, "RPC P2P bootstrap complete"; "ready_peers" => ready_peers.len());
    }

    // Mark as ready
    is_ready.store(true, Ordering::Release);
    ready_notify.notify_waiters();
    slog::info!(logger, "RPC P2P service ready");

    // Main loop: process sync channel messages and outbound requests
    let mut tick_interval = tokio::time::interval(Duration::from_millis(100));

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        tokio::select! {
            // Shutdown signal
            _ = shutdown_notify.notified() => {
                break;
            }

            // Outbound block request
            Some(cmd) = request_rx.recv() => {
                let msg = P2PMessage::<N, F, M_SIZE>::BlockRequest(cmd.request);
                match serialize_message(&msg) {
                    Ok(bytes) => {
                        slog::debug!(logger, "Sending block request";
                            "target" => ?cmd.target
                        );
                        network.send_sync(bytes, vec![cmd.target]).await;
                    }
                    Err(e) => {
                        slog::warn!(logger, "Failed to serialize block request"; "error" => ?e);
                    }
                }
            }

            // Sync channel message (inbound)
            res = ReceiverTrait::recv(&mut receivers.sync) => {
                match res {
                    Ok((sender, bytes)) => {
                        if let Err(e) = handle_sync_message::<N, F, M_SIZE>(
                            &bytes,
                            sender,
                            &sync_tx,
                            &logger,
                        ) {
                            slog::warn!(logger, "Failed to handle sync message"; "error" => %e);
                        }
                    }
                    Err(e) => {
                        slog::warn!(logger, "Sync recv error"; "error" => ?e);
                    }
                }
            }

            // Tick (for keepalive, etc.)
            _ = tick_interval.tick() => {
                // Could send pings here if needed
            }
        }
    }

    // Cleanup
    network.shutdown();
    slog::info!(logger, "RPC P2P service stopped");
}

/// Handle an incoming sync message.
fn handle_sync_message<const N: usize, const F: usize, const M_SIZE: usize>(
    bytes: &Bytes,
    sender: ed25519::PublicKey,
    sync_tx: &tokio::sync::mpsc::UnboundedSender<(ed25519::PublicKey, BlockResponse)>,
    logger: &Logger,
) -> Result<(), anyhow::Error> {
    let msg: P2PMessage<N, F, M_SIZE> = deserialize_message(bytes)?;

    match msg {
        P2PMessage::BlockResponse(response) => {
            slog::debug!(logger, "Received block response"; "from" => ?sender);
            let _ = sync_tx.send((sender, response));
            Ok(())
        }
        P2PMessage::BlockRequest(_) => {
            // RPC nodes don't serve block requests (only validators do)
            slog::debug!(
                logger,
                "Ignoring block request (RPC nodes don't serve blocks)"
            );
            Ok(())
        }
        _ => {
            // Ignore other message types on sync channel
            slog::debug!(logger, "Ignoring non-sync message on sync channel");
            Ok(())
        }
    }
}

/// Run the P2P bootstrap phase.
///
/// Sends pings to discover peers and waits for at least one peer to respond,
/// or times out. Returns the set of ready peers discovered.
#[allow(clippy::too_many_arguments)]
async fn run_bootstrap_phase<C, const N: usize, const F: usize, const M_SIZE: usize>(
    context: &C,
    network: &mut NetworkService<C>,
    receivers: &mut p2p::network::NetworkReceivers,
    expected_peers: &std::collections::HashSet<ed25519::PublicKey>,
    timeout: Duration,
    ping_interval: Duration,
    shutdown: &AtomicBool,
    shutdown_notify: &Notify,
    logger: &Logger,
) -> std::collections::HashSet<ed25519::PublicKey>
where
    C: Spawner + Clock + Network + Resolver + Metrics + RngCore + CryptoRng + Clone,
{
    use std::collections::HashSet;

    let mut ready_peers: HashSet<ed25519::PublicKey> = HashSet::new();
    let start = std::time::Instant::now();
    let mut last_ping = std::time::Instant::now() - ping_interval;

    while start.elapsed() < timeout && ready_peers.is_empty() {
        if shutdown.load(Ordering::Relaxed) {
            break;
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
                network.send_sync(ping_bytes, vec![]).await;
                slog::debug!(logger, "Sent RPC bootstrap ping"; "timestamp" => timestamp);
            }
            last_ping = std::time::Instant::now();
        }

        // Wait for responses with a short timeout
        tokio::select! {
            biased;

            _ = shutdown_notify.notified() => break,

            _ = context.sleep(Duration::from_millis(100)) => {
                // Continue loop
            }

            res = ReceiverTrait::recv(&mut receivers.sync) => {
                if let Ok((sender, msg)) = res && let Ok(p2p_msg) = deserialize_message::<N, F, M_SIZE>(&msg) {
                        handle_bootstrap_message::<N, F, M_SIZE, C>(
                            p2p_msg,
                            sender,
                            network,
                            expected_peers,
                            &mut ready_peers,
                            logger,
                        )
                        .await;
                }
            }
        }
    }

    ready_peers
}

/// Handle a message during bootstrap phase.
async fn handle_bootstrap_message<const N: usize, const F: usize, const M_SIZE: usize, C>(
    msg: P2PMessage<N, F, M_SIZE>,
    sender: ed25519::PublicKey,
    network: &mut NetworkService<C>,
    expected_peers: &std::collections::HashSet<ed25519::PublicKey>,
    ready_peers: &mut std::collections::HashSet<ed25519::PublicKey>,
    logger: &Logger,
) where
    C: Spawner + Clock + Network + Resolver + Metrics + RngCore + CryptoRng + Clone,
{
    match msg {
        P2PMessage::Ping(ts) => {
            // Respond with Pong
            if let Ok(pong_bytes) = serialize_message(&P2PMessage::<N, F, M_SIZE>::Pong(ts)) {
                network.send_sync(pong_bytes, vec![sender.clone()]).await;
            }
            if expected_peers.contains(&sender) && ready_peers.insert(sender.clone()) {
                slog::info!(logger, "RPC peer ready (ping)"; "peer" => ?sender);
            }
        }
        P2PMessage::Pong(_ts) => {
            if expected_peers.contains(&sender) && ready_peers.insert(sender.clone()) {
                slog::info!(logger, "RPC peer ready (pong)"; "peer" => ?sender);
            }
        }
        _ => {}
    }
}
