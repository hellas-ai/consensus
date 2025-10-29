use anyhow::Result;

use crate::{
    consensus_manager::events::ViewProgressEvent,
    consensus_manager::{
        traits::{ApplicationStateMachine, CryptoProvider, NetworkSender},
        view_manager::ViewProgressManager,
    },
};

#[allow(unused)]
pub struct ConsensusStateMachine<
    Network,
    Crypto,
    Application,
    const N: usize,
    const F: usize,
    const M_SIZE: usize,
> where
    Network: NetworkSender<N, F, M_SIZE>,
    Crypto: CryptoProvider,
    Application: ApplicationStateMachine,
{
    view_manager: ViewProgressManager<N, F, M_SIZE>,
    network: Network,
    crypto: Crypto,
    application: Application,
    /// Event subscribers
    event_callbacks: Vec<Box<dyn Fn(ViewProgressEvent<N, F, M_SIZE>) -> Result<()>>>,
    logger: slog::Logger,
}

impl<Network, Crypto, Application, const N: usize, const F: usize, const M_SIZE: usize>
    ConsensusStateMachine<Network, Crypto, Application, N, F, M_SIZE>
where
    Network: NetworkSender<N, F, M_SIZE>,
    Crypto: CryptoProvider,
    Application: ApplicationStateMachine,
{
    pub fn new(
        view_manager: ViewProgressManager<N, F, M_SIZE>,
        network: Network,
        crypto: Crypto,
        application: Application,
        logger: slog::Logger,
    ) -> Result<Self> {
        Ok(Self {
            view_manager,
            network,
            crypto,
            application,
            event_callbacks: Vec::new(),
            logger,
        })
    }
}
