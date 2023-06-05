pub mod control;
pub mod events;
pub mod test_tools;

use std::borrow::Cow;
use std::ops::{ControlFlow, Deref};

use clarity::Address;
use ethbridge_events::{event_codecs, EventKind};
use namada::core::hints;
use namada::core::types::ethereum_structs;
use namada::eth_bridge::oracle::config::Config;
#[cfg(not(test))]
use namada::ledger::eth_bridge::eth_syncing_status_timeout;
use namada::ledger::eth_bridge::SyncStatus;
#[cfg(not(test))]
use namada::types::control_flow::time::Instant;
use namada::types::control_flow::time::{Duration, SleepStrategy};
use namada::types::ethereum_events::EthereumEvent;
use num256::Uint256;
use thiserror::Error;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::Sender as BoundedSender;
use tokio::task::LocalSet;
#[cfg(not(test))]
use web30::client::Web3;

use self::events::PendingEvent;
#[cfg(test)]
use self::test_tools::mock_web3_client::Web3;
use super::abortable::AbortableSpawner;
use crate::node::ledger::oracle::control::Command;

/// The default amount of time the oracle will wait between processing blocks
const DEFAULT_BACKOFF: Duration = Duration::from_millis(500);
const DEFAULT_CEILING: Duration = Duration::from_secs(30);

#[derive(Error, Debug)]
pub enum Error {
    #[error("Ethereum node has fallen out of sync")]
    FallenBehind,
    #[error(
        "Couldn't check for events ({0} from {1}) with the RPC endpoint: {2}"
    )]
    CheckEvents(String, Address, String),
    #[error("Could not send all bridge events ({0} from {1}) to the shell")]
    Channel(String, Address),
    #[error(
        "Need more confirmations for oracle to continue processing blocks."
    )]
    MoreConfirmations,
    #[error("The Ethereum oracle timed out")]
    Timeout,
}

/// A client that can talk to geth and parse
/// and relay events relevant to Namada to the
/// ledger process
pub struct Oracle {
    /// The client that talks to the Ethereum fullnode
    client: Web3,
    /// A channel for sending processed and confirmed
    /// events to the ledger process
    sender: BoundedSender<EthereumEvent>,
    /// The most recently processed block is recorded here.
    last_processed_block: last_processed_block::Sender,
    /// How long the oracle should wait between checking blocks
    backoff: Duration,
    /// How long the oracle should allow the fullnode to be unresponsive
    #[cfg_attr(test, allow(dead_code))]
    ceiling: Duration,
    /// A channel for controlling and configuring the oracle.
    control: control::Receiver,
}

impl Deref for Oracle {
    type Target = Web3;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl Oracle {
    /// Construct a new [`Oracle`]. Note that it can not do anything until it
    /// has been sent a configuration via the passed in `control` channel.
    pub fn new(
        url: &str,
        sender: BoundedSender<EthereumEvent>,
        last_processed_block: last_processed_block::Sender,
        backoff: Duration,
        ceiling: Duration,
        control: control::Receiver,
    ) -> Self {
        Self {
            client: Web3::new(url, std::time::Duration::from_secs(30)),
            sender,
            backoff,
            ceiling,
            last_processed_block,
            control,
        }
    }

    /// Check if the fullnode we are connected
    /// to is syncing or is up to date with the
    /// Ethereum (an return the block height).
    ///
    /// Note that the syncing call may return false
    /// inaccurately. In that case, we must check if the block
    /// number is 0 or not.
    #[cfg(not(test))]
    async fn syncing(&self) -> Result<SyncStatus, Error> {
        let deadline = Instant::now() + self.ceiling;
        match eth_syncing_status_timeout(&self.client, self.backoff, deadline)
            .await
            .map_err(|_| Error::Timeout)?
        {
            s @ SyncStatus::Syncing => Ok(s),
            SyncStatus::AtHeight(height) => {
                match &*self.last_processed_block.borrow() {
                    Some(last) if <&Uint256>::from(last) < &height => {
                        Ok(SyncStatus::AtHeight(height))
                    }
                    None => Ok(SyncStatus::AtHeight(height)),
                    _ => Err(Error::FallenBehind),
                }
            }
        }
    }

    /// Send a series of [`EthereumEvent`]s to the Namada
    /// ledger. Returns a boolean indicating that all sent
    /// successfully. If false is returned, the receiver
    /// has hung up.
    ///
    /// N.B. this will block if the internal channel buffer
    /// is full.
    async fn send(&self, events: Vec<EthereumEvent>) -> bool {
        if self.sender.is_closed() {
            return false;
        }
        for event in events.into_iter() {
            if self.sender.send(event).await.is_err() {
                return false;
            }
        }
        true
    }

    /// Check if a new config has been sent from teh Shell.
    fn update_config(&mut self) -> Option<Config> {
        match self.control.try_recv() {
            Ok(Command::UpdateConfig(config)) => Some(config),
            Err(TryRecvError::Disconnected) => panic!(
                "The Ethereum oracle command channel has unexpectedly hung up."
            ),
            _ => None,
        }
    }
}

/// Block until an initial configuration is received via the command channel.
/// Returns the initial config once received, or `None` if the command channel
/// is closed.
async fn await_initial_configuration(
    receiver: &mut control::Receiver,
) -> Option<Config> {
    match receiver.recv().await {
        Some(Command::UpdateConfig(config)) => Some(config),
        _ => None,
    }
}

/// Set up an Oracle and run the process where the Oracle
/// processes and forwards Ethereum events to the ledger
pub fn run_oracle(
    url: impl AsRef<str>,
    sender: BoundedSender<EthereumEvent>,
    control: control::Receiver,
    last_processed_block: last_processed_block::Sender,
    spawner: &mut AbortableSpawner,
) -> tokio::task::JoinHandle<()> {
    let url = url.as_ref().to_owned();
    // we have to run the oracle in a [`LocalSet`] due to the web30
    // crate
    let blocking_handle = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async move {
            LocalSet::new()
                .run_until(async move {
                    tracing::info!(?url, "Ethereum event oracle is starting");

                    let oracle = Oracle::new(
                        &url,
                        sender,
                        last_processed_block,
                        DEFAULT_BACKOFF,
                        DEFAULT_CEILING,
                        control,
                    );
                    run_oracle_aux(oracle).await;

                    tracing::info!(
                        ?url,
                        "Ethereum event oracle is no longer running"
                    );
                })
                .await
        });
    });
    spawner
        .spawn_abortable("Ethereum Oracle", move |aborter| async move {
            blocking_handle.await.unwrap();
            drop(aborter);
        })
        .with_no_cleanup()
}

/// Given an oracle, watch for new Ethereum events, processing
/// them into Namada native types.
///
/// It also checks that once the specified number of confirmations
/// is reached, an event is forwarded to the ledger process
async fn run_oracle_aux(mut oracle: Oracle) {
    tracing::info!("Oracle is awaiting initial configuration");
    let mut config =
        match await_initial_configuration(&mut oracle.control).await {
            Some(config) => {
                tracing::info!(
                    "Oracle received initial configuration - {:?}",
                    config
                );
                config
            }
            None => {
                tracing::debug!(
                    "Oracle control channel was closed before the oracle \
                     could be configured"
                );
                return;
            }
        };

    let mut next_block_to_process = config.start_block.clone();

    loop {
        tracing::info!(
            ?next_block_to_process,
            "Checking Ethereum block for bridge events"
        );
        let res = SleepStrategy::Constant(oracle.backoff).run(|| async {
            tokio::select! {
                result = process(&oracle, &config, next_block_to_process.clone()) => {
                    match result {
                        Ok(()) => {
                            ControlFlow::Break(Ok(()))
                        },
                        Err(
                            reason @ (
                                Error::Timeout
                                | Error::Channel(_, _)
                                | Error::CheckEvents(_, _, _)
                            )
                        ) => {
                            // the oracle is unresponsive, we don't want the test to end
                            if cfg!(test) && matches!(&reason, Error::CheckEvents(_, _, _)) {
                                return ControlFlow::Continue(());
                            }
                            tracing::error!(
                                %reason,
                                block = ?next_block_to_process,
                                "The Ethereum oracle has disconnected"
                            );
                            ControlFlow::Break(Err(()))
                        }
                        Err(error) => {
                            // this is a recoverable error, hence the debug log,
                            // to avoid spamming info logs
                            tracing::debug!(
                                %error,
                                block = ?next_block_to_process,
                                "Error while trying to process Ethereum block"
                            );
                            ControlFlow::Continue(())
                        }
                    }
                },
                _ = oracle.sender.closed() => {
                    tracing::info!(
                        "Ethereum oracle can not send events to the ledger; the \
                        receiver has hung up. Shutting down"
                    );
                    ControlFlow::Break(Err(()))
                }
            }
        })
        .await;

        if hints::unlikely(res.is_err()) {
            break;
        }

        oracle
            .last_processed_block
            .send_replace(Some(next_block_to_process.clone()));
        // check if a new config has been sent.
        if let Some(new_config) = oracle.update_config() {
            config = new_config;
        }
        next_block_to_process += 1.into();
    }
}

/// Checks if the given block has any events relating to the bridge, and if so,
/// sends them to the oracle's `sender` channel
async fn process(
    oracle: &Oracle,
    config: &Config,
    block_to_process: ethereum_structs::BlockHeight,
) -> Result<(), Error> {
    let mut queue: Vec<PendingEvent> = vec![];
    let pending = &mut queue;
    // update the latest block height

    let latest_block = match oracle.syncing().await? {
        SyncStatus::AtHeight(height) => height,
        SyncStatus::Syncing => return Err(Error::FallenBehind),
    }
    .into();
    let minimum_latest_block =
        block_to_process.clone() + config.min_confirmations.into();
    if minimum_latest_block > latest_block {
        tracing::debug!(
            ?block_to_process,
            ?latest_block,
            ?minimum_latest_block,
            "Waiting for enough Ethereum blocks to be synced"
        );
        return Err(Error::MoreConfirmations);
    }
    tracing::debug!(
        ?block_to_process,
        ?latest_block,
        "Got latest Ethereum block height"
    );
    // check for events in Ethereum blocks that have reached the minimum number
    // of confirmations
    for codec in event_codecs() {
        let sig = match codec.event_signature() {
            Cow::Borrowed(s) => s,
            _ => unreachable!(
                "All Ethereum events should have a static ABI signature"
            ),
        };
        let addr: Address = match codec.kind() {
            EventKind::Bridge => config.bridge_contract.0.into(),
            EventKind::Governance => config.governance_contract.0.into(),
        };
        tracing::debug!(
            ?block_to_process,
            ?addr,
            ?sig,
            "Checking for bridge events"
        );
        // fetch the events for matching the given signature
        let mut events = {
            let logs = match oracle
                .check_for_events(
                    block_to_process.clone().into(),
                    Some(block_to_process.clone().into()),
                    vec![addr],
                    vec![sig],
                )
                .await
            {
                Ok(logs) => logs,
                Err(error) => {
                    return Err(Error::CheckEvents(
                        sig.into(),
                        addr,
                        error.to_string(),
                    ));
                }
            };
            if !logs.is_empty() {
                tracing::info!(
                    ?block_to_process,
                    ?addr,
                    ?sig,
                    n_events = logs.len(),
                    "Found bridge events in Ethereum block"
                )
            }
            logs.into_iter()
                .map(Web30LogExt::into_ethabi)
                .filter_map(|log| {
                    match PendingEvent::decode(
                        codec,
                        block_to_process.clone().into(),
                        &log,
                        u64::from(config.min_confirmations).into(),
                    ) {
                        Ok(event) => Some(event),
                        Err(error) => {
                            tracing::error!(
                                ?error,
                                ?block_to_process,
                                ?addr,
                                ?sig,
                                "Couldn't decode event: {:#?}",
                                log
                            );
                            None
                        }
                    }
                })
                .collect()
        };
        pending.append(&mut events);
        if !pending.is_empty() {
            tracing::info!(
                ?block_to_process,
                ?addr,
                ?sig,
                pending = pending.len(),
                "There are Ethereum events pending"
            );
        }
        let confirmed = process_queue(&latest_block, pending);
        if !confirmed.is_empty() {
            tracing::info!(
                ?block_to_process,
                ?addr,
                ?sig,
                pending = pending.len(),
                confirmed = confirmed.len(),
                min_confirmations = ?config.min_confirmations,
                "Some events that have reached the minimum number of \
                 confirmations and will be sent onwards"
            );
        }
        if !oracle.send(confirmed).await {
            return Err(Error::Channel(sig.into(), addr));
        }
    }
    Ok(())
}

/// Check which events in the queue have reached their
/// required number of confirmations and remove them
/// from the queue of pending events
fn process_queue(
    latest_block: &Uint256,
    pending: &mut Vec<PendingEvent>,
) -> Vec<EthereumEvent> {
    let mut pending_tmp: Vec<PendingEvent> = Vec::with_capacity(pending.len());
    std::mem::swap(&mut pending_tmp, pending);
    let mut confirmed = vec![];
    for item in pending_tmp.into_iter() {
        if item.is_confirmed(latest_block) {
            confirmed.push(item.event);
        } else {
            pending.push(item);
        }
    }
    confirmed
}

/// Extra methods for [`web30::types::Log`] instances.
trait Web30LogExt {
    /// Convert a [`web30`] event log to the corresponding
    /// [`ethabi`] type.
    fn into_ethabi(self) -> ethabi::RawLog;
}

impl Web30LogExt for web30::types::Log {
    fn into_ethabi(self) -> ethabi::RawLog {
        let topics = self
            .topics
            .iter()
            .filter_map(|topic| {
                (topic.len() == 32)
                    .then(|| ethabi::Hash::from_slice(topic.as_slice()))
            })
            .collect();
        let data = self.data.0;
        ethabi::RawLog { topics, data }
    }
}

pub mod last_processed_block {
    //! Functionality to do with publishing which blocks we have processed.
    use namada::core::types::ethereum_structs;
    use tokio::sync::watch;

    pub type Sender = watch::Sender<Option<ethereum_structs::BlockHeight>>;
    pub type Receiver = watch::Receiver<Option<ethereum_structs::BlockHeight>>;

    /// Construct a [`tokio::sync::watch`] channel to publish the most recently
    /// processed block. Until the live oracle processes its first block, this
    /// will be `None`.
    pub fn channel() -> (Sender, Receiver) {
        watch::channel(None)
    }
}

#[cfg(test)]
mod test_oracle {
    use std::num::NonZeroU64;

    use ethbridge_bridge_events::{
        TransferToErcFilter, TransferToNamadaFilter,
    };
    use namada::eth_bridge::ethers::abi::AbiEncode;
    use namada::eth_bridge::ethers::types::H160;
    use namada::eth_bridge::structs::Erc20Transfer;
    use namada::types::address::testing::gen_established_address;
    use namada::types::ethereum_events::{EthAddress, TransferToEthereum};
    use tokio::sync::oneshot::channel;
    use tokio::time::timeout;

    use super::*;
    use crate::node::ledger::ethereum_oracle::test_tools::mock_web3_client::{
        event_signature, TestCmd, Web3, Web3Controller,
    };

    /// The data returned from setting up a test
    struct TestPackage {
        oracle: Oracle,
        controller: Web3Controller,
        eth_recv: tokio::sync::mpsc::Receiver<EthereumEvent>,
        control_sender: control::Sender,
        blocks_processed_recv: tokio::sync::mpsc::UnboundedReceiver<Uint256>,
    }

    /// Helper function that starts running the oracle in a new thread, and
    /// initializes it with a simple default configuration that is appropriate
    /// for tests.
    async fn start_with_default_config(
        oracle: Oracle,
        control_sender: &mut control::Sender,
        config: Config,
    ) -> tokio::task::JoinHandle<()> {
        let handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async move {
                LocalSet::new()
                    .run_until(async move {
                        run_oracle_aux(oracle).await;
                    })
                    .await
            });
        });
        control_sender
            .try_send(control::Command::UpdateConfig(config))
            .unwrap();
        handle
    }

    /// Set up an oracle with a mock web3 client that we can control
    fn setup() -> TestPackage {
        let (blocks_processed_recv, client) = Web3::setup();
        let (eth_sender, eth_receiver) = tokio::sync::mpsc::channel(1000);
        let (last_processed_block_sender, _) = last_processed_block::channel();
        let (control_sender, control_receiver) = control::channel();
        let controller = client.controller();
        TestPackage {
            oracle: Oracle {
                client,
                sender: eth_sender,
                last_processed_block: last_processed_block_sender,
                // backoff should be short for tests so that they run faster
                backoff: Duration::from_millis(5),
                ceiling: DEFAULT_CEILING,
                control: control_receiver,
            },
            controller,
            eth_recv: eth_receiver,
            control_sender,
            blocks_processed_recv,
        }
    }

    /// Test that if the fullnode stops, the oracle
    /// shuts down, even if the web3 client is unresponsive
    #[tokio::test]
    async fn test_shutdown() {
        let TestPackage {
            oracle,
            eth_recv,
            controller,
            mut control_sender,
            ..
        } = setup();
        let oracle = start_with_default_config(
            oracle,
            &mut control_sender,
            Config::default(),
        )
        .await;
        controller.apply_cmd(TestCmd::Unresponsive);
        drop(eth_recv);
        oracle.await.expect("Test failed");
    }

    /// Test that if no logs are received from the web3
    /// client, no events are sent out
    #[tokio::test]
    async fn test_no_logs_no_op() {
        let TestPackage {
            oracle,
            mut eth_recv,
            controller,
            blocks_processed_recv: _processed,
            mut control_sender,
        } = setup();
        let oracle = start_with_default_config(
            oracle,
            &mut control_sender,
            Config::default(),
        )
        .await;
        controller.apply_cmd(TestCmd::NewHeight(Uint256::from(150u32)));

        let mut time = std::time::Duration::from_secs(1);
        while time > std::time::Duration::from_millis(10) {
            assert!(eth_recv.try_recv().is_err());
            time -= std::time::Duration::from_millis(10);
        }
        drop(eth_recv);
        oracle.await.expect("Test failed");
    }

    /// Test that if a new block height doesn't increase,
    /// no events are sent out even if there are
    /// some in the logs.
    #[tokio::test]
    async fn test_cant_get_new_height() {
        let TestPackage {
            oracle,
            mut eth_recv,
            controller,
            blocks_processed_recv: _processed,
            mut control_sender,
        } = setup();
        let min_confirmations = 100;
        let config = Config {
            min_confirmations: NonZeroU64::try_from(min_confirmations)
                .expect("Test wasn't set up correctly"),
            ..Config::default()
        };
        let oracle =
            start_with_default_config(oracle, &mut control_sender, config)
                .await;
        // Increase height above the configured minimum confirmations
        controller.apply_cmd(TestCmd::NewHeight(min_confirmations.into()));

        let new_event = TransferToNamadaFilter {
            nonce: 0.into(),
            transfers: vec![],
            valid_map: vec![],
            confirmations: 100.into(),
        }
        .encode();
        let (sender, _) = channel();
        controller.apply_cmd(TestCmd::NewEvent {
            event_type: event_signature::<TransferToNamadaFilter>(),
            data: new_event,
            height: 101,
            seen: sender,
        });
        // since height is not updating, we should not receive events
        let mut time = std::time::Duration::from_secs(1);
        while time > std::time::Duration::from_millis(10) {
            assert!(eth_recv.try_recv().is_err());
            time -= std::time::Duration::from_millis(10);
        }
        drop(eth_recv);
        oracle.await.expect("Test failed");
    }

    /// Test that the oracle waits until new logs
    /// are received before sending them on.
    #[tokio::test]
    async fn test_wait_on_new_logs() {
        let TestPackage {
            oracle,
            eth_recv,
            controller,
            blocks_processed_recv: _processed,
            mut control_sender,
        } = setup();
        let min_confirmations = 100;
        let config = Config {
            min_confirmations: NonZeroU64::try_from(min_confirmations)
                .expect("Test wasn't set up correctly"),
            ..Config::default()
        };
        let oracle =
            start_with_default_config(oracle, &mut control_sender, config)
                .await;
        // Increase height above the configured minimum confirmations
        controller.apply_cmd(TestCmd::NewHeight(min_confirmations.into()));

        // set the oracle to be unresponsive
        controller.apply_cmd(TestCmd::Unresponsive);
        // send a new event to the oracle
        let new_event = TransferToNamadaFilter {
            nonce: 0.into(),
            transfers: vec![],
            valid_map: vec![],
            confirmations: 100.into(),
        }
        .encode();
        let (sender, mut seen) = channel();
        controller.apply_cmd(TestCmd::NewEvent {
            event_type: event_signature::<TransferToNamadaFilter>(),
            data: new_event,
            height: 150,
            seen: sender,
        });
        // set the height high enough to emit the event
        controller.apply_cmd(TestCmd::NewHeight(Uint256::from(251u32)));

        // the event should not be emitted even though the height is large
        // enough
        let mut time = std::time::Duration::from_secs(1);
        while time > std::time::Duration::from_millis(10) {
            assert!(seen.try_recv().is_err());
            time -= std::time::Duration::from_millis(10);
        }
        // check that when web3 becomes responsive, oracle sends event
        controller.apply_cmd(TestCmd::Normal);
        seen.await.expect("Test failed");
        drop(eth_recv);
        oracle.await.expect("Test failed");
    }

    /// Test that events are only sent when they
    /// reach the required number of confirmations
    #[tokio::test]
    async fn test_finality_gadget() {
        let TestPackage {
            oracle,
            mut eth_recv,
            controller,
            blocks_processed_recv: _processed,
            mut control_sender,
        } = setup();
        let min_confirmations = 100;
        let config = Config {
            min_confirmations: NonZeroU64::try_from(min_confirmations)
                .expect("Test wasn't set up correctly"),
            ..Config::default()
        };
        let oracle =
            start_with_default_config(oracle, &mut control_sender, config)
                .await;
        // Increase height above the configured minimum confirmations
        controller.apply_cmd(TestCmd::NewHeight(min_confirmations.into()));

        // confirmed after 100 blocks
        let first_event = TransferToNamadaFilter {
            nonce: 0.into(),
            transfers: vec![],
            valid_map: vec![],
            confirmations: 100.into(),
        }
        .encode();

        // confirmed after 125 blocks
        let gas_payer = gen_established_address();
        let second_event = TransferToErcFilter {
            transfers: vec![Erc20Transfer {
                amount: 0.into(),
                from: H160([0; 20]),
                sender: gas_payer.to_string(),
                to: H160([1; 20]),
                fee: 0.into(),
                fee_from: gas_payer.to_string(),
            }],
            valid_map: vec![true],
            relayer_address: gas_payer.to_string(),
            nonce: 0.into(),
        }
        .encode();

        // send in the events to the logs
        let (sender, seen_second) = channel();
        controller.apply_cmd(TestCmd::NewEvent {
            event_type: event_signature::<TransferToErcFilter>(),
            data: second_event,
            height: 125,
            seen: sender,
        });
        let (sender, _recv) = channel();
        controller.apply_cmd(TestCmd::NewEvent {
            event_type: event_signature::<TransferToNamadaFilter>(),
            data: first_event,
            height: 100,
            seen: sender,
        });

        // increase block height so first event is confirmed but second is
        // not.
        controller.apply_cmd(TestCmd::NewHeight(Uint256::from(200u32)));
        // check the correct event is received
        let event = eth_recv.recv().await.expect("Test failed");
        if let EthereumEvent::TransfersToNamada {
            nonce,
            transfers,
            valid_transfers_map: valid_map,
        } = event
        {
            assert_eq!(nonce, 0.into());
            assert!(transfers.is_empty());
            assert!(valid_map.is_empty());
        } else {
            panic!("Test failed, {:?}", event);
        }

        // check no other events are received
        let mut time = std::time::Duration::from_secs(1);
        while time > std::time::Duration::from_millis(10) {
            assert!(eth_recv.try_recv().is_err());
            time -= std::time::Duration::from_millis(10);
        }

        // increase block height so second event is emitted
        controller.apply_cmd(TestCmd::NewHeight(Uint256::from(225u32)));
        // wait until event is emitted
        seen_second.await.expect("Test failed");
        // increase block height so second event is confirmed
        controller.apply_cmd(TestCmd::NewHeight(Uint256::from(250u32)));
        // check correct event is received
        let event = eth_recv.recv().await.expect("Test failed");
        if let EthereumEvent::TransfersToEthereum { mut transfers, .. } = event
        {
            assert_eq!(transfers.len(), 1);
            let transfer = transfers.remove(0);
            assert_eq!(
                transfer,
                TransferToEthereum {
                    amount: Default::default(),
                    asset: EthAddress([0; 20]),
                    sender: gas_payer.clone(),
                    receiver: EthAddress([1; 20]),
                    gas_amount: Default::default(),
                    gas_payer: gas_payer.clone(),
                }
            );
        } else {
            panic!("Test failed");
        }

        drop(eth_recv);
        oracle.await.expect("Test failed");
    }

    /// Test that Ethereum blocks are processed in sequence up to the latest
    /// block that has reached the minimum number of confirmations
    #[tokio::test]
    async fn test_blocks_checked_sequence() {
        let TestPackage {
            oracle,
            eth_recv,
            controller,
            mut blocks_processed_recv,
            mut control_sender,
        } = setup();
        let config = Config::default();
        let oracle = start_with_default_config(
            oracle,
            &mut control_sender,
            config.clone(),
        )
        .await;

        // set the height of the chain such that there are some blocks deep
        // enough to be considered confirmed by the oracle
        let confirmed_block_height = 9; // all blocks up to and including this block have enough confirmations
        let synced_block_height =
            u64::from(config.min_confirmations) + confirmed_block_height;
        for height in 0..synced_block_height + 1 {
            controller.apply_cmd(TestCmd::NewHeight(Uint256::from(height)));
        }
        // check that the oracle indeed processes the confirmed blocks
        for height in 0u64..confirmed_block_height + 1 {
            let block_processed = timeout(
                std::time::Duration::from_secs(3),
                blocks_processed_recv.recv(),
            )
            .await
            .expect("Timed out waiting for block to be checked")
            .unwrap();
            assert_eq!(block_processed, Uint256::from(height));
        }

        // check that the oracle hasn't yet checked any further blocks
        // TODO: check this in a deterministic way rather than just waiting a
        // bit
        assert!(
            timeout(
                std::time::Duration::from_secs(1),
                blocks_processed_recv.recv()
            )
            .await
            .is_err()
        );

        // increase the height of the chain by one, and check that the oracle
        // processed the next confirmed block
        let synced_block_height = synced_block_height + 1;
        controller
            .apply_cmd(TestCmd::NewHeight(Uint256::from(synced_block_height)));

        let block_processed = timeout(
            std::time::Duration::from_secs(3),
            blocks_processed_recv.recv(),
        )
        .await
        .expect("Timed out waiting for block to be checked")
        .unwrap();
        assert_eq!(block_processed, Uint256::from(confirmed_block_height + 1));

        drop(eth_recv);
        oracle.await.expect("Test failed");
    }

    /// Test that if the Ethereum RPC endpoint returns a latest block that is
    /// more than one block later than the previous latest block we received, we
    /// still check all the blocks in between
    #[tokio::test]
    async fn test_all_blocks_checked() {
        let TestPackage {
            oracle,
            eth_recv,
            controller,
            mut blocks_processed_recv,
            mut control_sender,
        } = setup();
        let config = Config::default();
        let oracle = start_with_default_config(
            oracle,
            &mut control_sender,
            config.clone(),
        )
        .await;

        let confirmed_block_height = 9; // all blocks up to and including this block have enough confirmations
        let synced_block_height =
            u64::from(config.min_confirmations) + confirmed_block_height;
        controller
            .apply_cmd(TestCmd::NewHeight(Uint256::from(synced_block_height)));

        // check that the oracle has indeed processed the first `n` blocks, even
        // though the first latest block that the oracle received was not 0
        for height in 0u64..confirmed_block_height + 1 {
            let block_processed = timeout(
                std::time::Duration::from_secs(3),
                blocks_processed_recv.recv(),
            )
            .await
            .expect("Timed out waiting for block to be checked")
            .unwrap();
            assert_eq!(block_processed, Uint256::from(height));
        }

        // the next time the oracle checks, the latest block will have increased
        // by more than one
        let difference = 10;
        let synced_block_height = synced_block_height + difference;
        controller
            .apply_cmd(TestCmd::NewHeight(Uint256::from(synced_block_height)));

        // check that the oracle still checks the blocks inbetween
        for height in (confirmed_block_height + 1)
            ..(confirmed_block_height + difference + 1)
        {
            let block_processed = timeout(
                std::time::Duration::from_secs(3),
                blocks_processed_recv.recv(),
            )
            .await
            .expect("Timed out waiting for block to be checked")
            .unwrap();
            assert_eq!(block_processed, Uint256::from(height));
        }

        drop(eth_recv);
        oracle.await.expect("Test failed");
    }
}