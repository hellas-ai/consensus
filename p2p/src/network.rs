//! Core networking using Commonware discovery network.
//!
//! Provides the network service for authenticated peer-to-peer communication.

use std::sync::{Arc, Mutex};

use commonware_codec::ReadExt;
use commonware_cryptography::{Signer, ed25519};
use commonware_p2p::{Ingress, Recipients, Sender, authenticated::discovery};
use commonware_runtime::{Clock, Metrics, Network, Resolver, Spawner};

use governor::Quota;
use rand::{CryptoRng, RngCore};

use crate::config::P2PConfig;
use crate::message::channels;

/// Default quota for consensus messages per second.
const DEFAULT_CONSENSUS_QUOTA_PER_SECOND: u32 = 10_000;

/// Default quota for transaction messages per second.
const DEFAULT_TX_QUOTA_PER_SECOND: u32 = 50_000;

/// Default quota for sync messages per second.
const DEFAULT_SYNC_QUOTA_PER_SECOND: u32 = 1_000;

/// Default backlog for consensus messages.
const DEFAULT_CONSENSUS_BACKLOG: usize = 1_024;

/// Default backlog for transaction messages.
const DEFAULT_TX_BACKLOG: usize = 8_192;

/// Default backlog for sync messages.
const DEFAULT_SYNC_BACKLOG: usize = 1_28;

/// Core network service managing the commonware discovery network.
pub struct NetworkService<C: Network + Spawner + Clock + RngCore + CryptoRng + Resolver + Metrics> {
    /// Our ED25519 private key for identity.
    signer: ed25519::PrivateKey,

    /// Handle to the commonware network (for shutdown/metrics).
    /// Wrapped in Option because start() consumes the network builder.
    network_handle: Option<commonware_runtime::Handle<()>>,

    /// Oracle for managing authorized peers.
    oracle: Arc<Mutex<discovery::Oracle<ed25519::PublicKey>>>,

    /// Public key of this node.
    public_key: ed25519::PublicKey,

    /// Channel senders
    consensus_sender: discovery::Sender<ed25519::PublicKey, C>,
    tx_sender: discovery::Sender<ed25519::PublicKey, C>,
    sync_sender: discovery::Sender<ed25519::PublicKey, C>,
}

/// Receivers returned when creating the network service.
pub struct NetworkReceivers {
    pub consensus: discovery::Receiver<ed25519::PublicKey>,
    pub tx: discovery::Receiver<ed25519::PublicKey>,
    pub sync: discovery::Receiver<ed25519::PublicKey>,
}

impl<C: Network + Spawner + Clock + RngCore + CryptoRng + Resolver + Metrics> NetworkService<C> {
    /// Create a new network service.
    ///
    /// This initializes the commonware network, registers channels, and starts the background task.
    pub async fn new(
        context: C,
        signer: ed25519::PrivateKey,
        config: P2PConfig,
        logger: slog::Logger,
    ) -> (Self, NetworkReceivers) {
        let public_key = signer.public_key();

        // 1. Create Commonware Config using the local() constructor
        let namespace = config.cluster_id.as_bytes();
        let dialable = Ingress::Socket(config.external_addr);

        // Build bootstrappers from config validators
        let bootstrappers: Vec<_> = config
            .validators
            .iter()
            .filter_map(|v| {
                // Parse ED25519 public key from hex
                let pk_bytes = v.parse_public_key_bytes()?;
                // Use Read trait to parse the public key
                let public_key = ed25519::PublicKey::read(&mut pk_bytes.as_slice()).ok()?;
                // Need an address to bootstrap from
                let addr = v.address?;
                Some((public_key, Ingress::Socket(addr)))
            })
            .collect();

        let cfg = discovery::Config::local(
            signer.clone(),
            namespace,
            config.listen_addr,
            dialable,
            bootstrappers,
            config.max_message_size,
        );

        // 2. Initialize Network Builder
        let (mut network, oracle) = discovery::Network::new(context.clone(), cfg);
        let oracle = Arc::new(Mutex::new(oracle));

        // 3. Register Channels
        // Consensus: High priority, moderate volume
        let (consensus_sender, consensus_recv) = network.register(
            channels::CONSENSUS,
            Quota::per_second(
                std::num::NonZeroU32::new(DEFAULT_CONSENSUS_QUOTA_PER_SECOND).unwrap(),
            ),
            DEFAULT_CONSENSUS_BACKLOG,
        );

        // Transactions: High volume
        let (tx_sender, tx_recv) = network.register(
            channels::TRANSACTIONS,
            Quota::per_second(std::num::NonZeroU32::new(DEFAULT_TX_QUOTA_PER_SECOND).unwrap()),
            DEFAULT_TX_BACKLOG,
        );

        // Sync: Low priority
        let (sync_sender, sync_recv) = network.register(
            channels::BLOCK_SYNC,
            Quota::per_second(std::num::NonZeroU32::new(DEFAULT_SYNC_QUOTA_PER_SECOND).unwrap()),
            DEFAULT_SYNC_BACKLOG,
        );

        // 4. Start Network
        let network_handle = network.start();

        slog::info!(logger, "P2P Network started"; "public_key" => ?public_key);

        let service = Self {
            signer,
            network_handle: Some(network_handle),
            oracle,
            public_key,
            consensus_sender,
            tx_sender,
            sync_sender,
        };

        let receivers = NetworkReceivers {
            consensus: consensus_recv,
            tx: tx_recv,
            sync: sync_recv,
        };

        (service, receivers)
    }

    /// Update the set of authorized validators.
    pub fn update_validators(&self, _validators: Vec<ed25519::PublicKey>) {
        if let Ok(_oracle) = self.oracle.lock() {
            // TODO: Refine this based on exact Oracle API for peer sets.
        }
    }

    /// Broadcast a consensus message (high priority).
    /// Consensus messages are critical and should never be delayed.
    pub async fn broadcast_consensus(&mut self, msg: Vec<u8>, recipients: Vec<ed25519::PublicKey>) {
        let recipients = Self::build_recipients(recipients);
        self.consensus_sender
            .send(recipients, msg.into(), true) // priority = true
            .await
            .ok();
    }

    /// Broadcast a transaction (low priority).
    /// Transactions can be re-gossiped if dropped under load.
    pub async fn broadcast_transaction(
        &mut self,
        msg: Vec<u8>,
        recipients: Vec<ed25519::PublicKey>,
    ) {
        let recipients = Self::build_recipients(recipients);
        self.tx_sender
            .send(recipients, msg.into(), false) // priority = false
            .await
            .ok();
    }

    /// Send a sync request/response (low priority).
    /// Sync messages are large and can retry on failure.
    pub async fn send_sync(&mut self, msg: Vec<u8>, recipients: Vec<ed25519::PublicKey>) {
        let recipients = Self::build_recipients(recipients);
        self.sync_sender
            .send(recipients, msg.into(), false) // priority = false
            .await
            .ok();
    }

    /// Helper to build Recipients enum from a vector of public keys.
    fn build_recipients(recipients: Vec<ed25519::PublicKey>) -> Recipients<ed25519::PublicKey> {
        if recipients.is_empty() {
            Recipients::All
        } else {
            Recipients::Some(recipients)
        }
    }

    pub fn public_key(&self) -> ed25519::PublicKey {
        self.public_key.clone()
    }
}
