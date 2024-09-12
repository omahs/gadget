#![allow(clippy::module_name_repetitions)]

use crate::logger::Logger;

/// An event watcher for the Tangle network.
#[derive(Debug, Clone)]
pub struct TangleEventsWatcher {
    pub logger: Logger,
}

/// A Type alias for the Tangle configuration [`subxt::PolkadotConfig`].
pub type TangleConfig = subxt::PolkadotConfig;

// /// A Type alias for the Tangle configuration [`subxt::PolkadotConfig`].
// pub type TangleConfig = subxt::SubstrateConfig;

#[async_trait::async_trait]
impl super::substrate::SubstrateEventWatcher<TangleConfig> for TangleEventsWatcher {
    const TAG: &'static str = "tangle";
    const PALLET_NAME: &'static str = "Services";
    fn logger(&self) -> &Logger {
        &self.logger
    }
}
