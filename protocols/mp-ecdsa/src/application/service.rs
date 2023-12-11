#![allow(clippy::all)]
// Copyright 2022 Webb Technologies Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Service and ServiceFactory implementation. Specialized wrapper over substrate service.

pub use crate::eth::{db_config_dir, EthConfiguration};
use crate::eth::{
    new_frontier_partial, spawn_frontier_tasks, BackendType, EthApi, FrontierBackend,
    FrontierBlockImport, FrontierPartialComponents, RpcConfig,
};
use dkg_gadget::debug_logger::DebugLogger;
use futures::{channel::mpsc, FutureExt};
use parity_scale_codec::Encode;
use sc_client_api::{Backend, BlockBackend};
use sc_consensus::BasicQueue;
use sc_consensus_aura::ImportQueueParams;
use sc_consensus_grandpa::SharedVoterState;
pub use sc_executor::NativeElseWasmExecutor;
use sc_network::{NetworkSigner, NetworkStateInfo};
use sc_service::{error::Error as ServiceError, ChainType, Configuration, TaskManager};
use sc_telemetry::{Telemetry, TelemetryWorker};
use sc_transaction_pool_api::OffchainTransactionPoolFactory;
use sp_api::ProvideRuntimeApi;
use sp_consensus_aura::sr25519::AuthorityPair as AuraPair;
use sp_core::{Pair, U256};
use sp_runtime::{generic::Era, SaturatedConversion};

use crate::keystore::ECDSAKeyStore;
use crate::network::NetworkGossipEngineBuilder;
use crate::util::DebugLogger;
use crate::MpEcdsaProtocolConfig;
use std::{path::Path, sync::Arc, time::Duration};
use substrate_frame_rpc_system::AccountNonceApi;
use tangle_testnet_runtime::{self, opaque::Block, RuntimeApi, TransactionConverter};

pub const KEYGEN_PROTOCOL_CHANNEL: &str = "/webb-tools/ecdsa/keygen/1";
pub const SIGNING_PROTOCOL_CHANNEL: &str = "/webb-tools/ecdsa/signing/1";

pub fn fetch_nonce(client: &FullClient, account: sp_core::sr25519::Pair) -> u32 {
    let best_hash = client.chain_info().best_hash;
    client
        .runtime_api()
        .account_nonce(best_hash, account.public().into())
        .expect("Fetching account nonce works; qed")
}

/// The minimum period of blocks on which justifications will be
/// imported and generated.
const GRANDPA_JUSTIFICATION_PERIOD: u32 = 512;

// Our native executor instance.
pub struct ExecutorDispatch;

impl sc_executor::NativeExecutionDispatch for ExecutorDispatch {
    /// Only enable the benchmarking host functions when we actually want to benchmark.
    #[cfg(feature = "runtime-benchmarks")]
    type ExtendHostFunctions = (
        frame_benchmarking::benchmarking::HostFunctions,
        primitives_ext::ext::HostFunctions,
    );
    /// Otherwise we only use the default Substrate host functions.
    #[cfg(not(feature = "runtime-benchmarks"))]
    type ExtendHostFunctions = primitives_ext::ext::HostFunctions;

    fn dispatch(method: &str, data: &[u8]) -> Option<Vec<u8>> {
        tangle_testnet_runtime::api::dispatch(method, data)
    }

    fn native_version() -> sc_executor::NativeVersion {
        tangle_testnet_runtime::native_version()
    }
}

pub(crate) type FullClient =
    sc_service::TFullClient<Block, RuntimeApi, NativeElseWasmExecutor<ExecutorDispatch>>;
pub(crate) type FullBackend = sc_service::TFullBackend<Block>;
type FullSelectChain = sc_consensus::LongestChain<FullBackend, Block>;

type GrandpaLinkHalf<Client> = sc_consensus_grandpa::LinkHalf<Block, Client, FullSelectChain>;
type BoxBlockImport = sc_consensus::BoxBlockImport<Block>;

/// Create a transaction using the given `call`
///
/// The transaction will be signed by `sender`. If `nonce` is `None` it will be fetched from the
/// state of the best block.
///
/// Note: Should only be used for tests.
pub fn create_extrinsic(
    client: &FullClient,
    sender: sp_core::sr25519::Pair,
    function: impl Into<tangle_testnet_runtime::RuntimeCall>,
    nonce: Option<u32>,
) -> tangle_testnet_runtime::UncheckedExtrinsic {
    let function = function.into();
    let genesis_hash = client
        .block_hash(0)
        .ok()
        .flatten()
        .expect("Genesis block exists; qed");
    let best_hash = client.chain_info().best_hash;
    let best_block = client.chain_info().best_number;
    let nonce = nonce.unwrap_or_else(|| fetch_nonce(client, sender.clone()));

    let period = tangle_testnet_runtime::BlockHashCount::get()
        .checked_next_power_of_two()
        .map(|c| c / 2)
        .unwrap_or(2) as u64;
    let tip = 0;
    let extra: tangle_testnet_runtime::SignedExtra = (
        frame_system::CheckNonZeroSender::<tangle_testnet_runtime::Runtime>::new(),
        frame_system::CheckSpecVersion::<tangle_testnet_runtime::Runtime>::new(),
        frame_system::CheckTxVersion::<tangle_testnet_runtime::Runtime>::new(),
        frame_system::CheckGenesis::<tangle_testnet_runtime::Runtime>::new(),
        frame_system::CheckEra::<tangle_testnet_runtime::Runtime>::from(Era::Mortal(
            period,
            best_block.saturated_into(),
        )),
        frame_system::CheckNonce::<tangle_testnet_runtime::Runtime>::from(nonce),
        frame_system::CheckWeight::<tangle_testnet_runtime::Runtime>::new(),
        pallet_transaction_payment::ChargeTransactionPayment::<tangle_testnet_runtime::Runtime>::from(tip),
    );

    let raw_payload = tangle_testnet_runtime::SignedPayload::from_raw(
        function.clone(),
        extra.clone(),
        (
            (),
            tangle_testnet_runtime::VERSION.spec_version,
            tangle_testnet_runtime::VERSION.transaction_version,
            genesis_hash,
            best_hash,
            (),
            (),
            (),
        ),
    );
    let signature = raw_payload.using_encoded(|e| sender.sign(e));

    tangle_testnet_runtime::UncheckedExtrinsic::new_signed(
        function,
        sp_runtime::AccountId32::from(sender.public()).into(),
        tangle_testnet_runtime::Signature::Sr25519(signature),
        extra,
    )
}

pub fn new_partial(
    config: &Configuration,
    eth_config: &EthConfiguration,
) -> Result<
    sc_service::PartialComponents<
        FullClient,
        FullBackend,
        FullSelectChain,
        sc_consensus::DefaultImportQueue<Block>,
        sc_transaction_pool::FullPool<Block, FullClient>,
        (
            Option<Telemetry>,
            BoxBlockImport,
            GrandpaLinkHalf<FullClient>,
            FrontierBackend,
            Arc<fc_rpc::OverrideHandle<Block>>,
        ),
    >,
    ServiceError,
> {
    println!("    ++++++++++++++++++++++++
   +++++++++++++++++++++++++++
   +++++++++++++++++++++++++++
   +++        ++++++      +++         @%%%%%%%%%%%                                     %%%
   ++++++      ++++      +++++        %%%%%%%%%%%%                                     %%%@
   ++++++++++++++++++++++++++            %%%%      %%%%@     %%% %%@       @%%%%%%%   %%%@    %%%%@
	  ++++++++                       %%%%    @%%%%%%%@   %%%%%%%%%   @%%%%%%%%%   %%%@  %%%%%%%%%
	  ++++++++                       %%%%    %%%%%%%%%   %%%% @%%%@  %%%%  %%%%   %%%@  %%%%%%%%%%
   ++++++++++++++++++++++++++            %%%%    %%%%%%%%%   %%%   %%%%  %%%   @%%%   %%%@ @%%%%%  %%%%%
   ++++++      ++++      ++++++          %%%%    %%%%%%%%%   %%%   %%%%  %%%%%%%%%%   %%%@  %%%%%%%%%@
   +++        ++++++        +++          %%%%    %%%%%%%%%   %%%   %%%@   %%%%%%%%%   %%%    %%%%%%%@
   ++++      +++++++++      +++                                           %%%%  %%%%
   ++++++++++++++++++++++++++++                                           %%%%%%%%%
     +++++++++++++++++++++++                                                 %%%%% \n");
    let telemetry = config
        .telemetry_endpoints
        .clone()
        .filter(|x| !x.is_empty())
        .map(|endpoints| -> Result<_, sc_telemetry::Error> {
            let worker = TelemetryWorker::new(16)?;
            let telemetry = worker.handle().new_telemetry(endpoints);
            Ok((worker, telemetry))
        })
        .transpose()?;

    let executor = sc_service::new_native_or_wasm_executor(config);

    let (client, backend, keystore_container, task_manager) =
        sc_service::new_full_parts::<Block, RuntimeApi, _>(
            config,
            telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
            executor,
        )?;
    let client = Arc::new(client);

    let telemetry = telemetry.map(|(worker, telemetry)| {
        task_manager
            .spawn_handle()
            .spawn("telemetry", None, worker.run());
        telemetry
    });

    let select_chain = sc_consensus::LongestChain::new(backend.clone());

    let transaction_pool = sc_transaction_pool::BasicPool::new_full(
        config.transaction_pool.clone(),
        config.role.is_authority().into(),
        config.prometheus_registry(),
        task_manager.spawn_essential_handle(),
        client.clone(),
    );

    let (grandpa_block_import, grandpa_link) = sc_consensus_grandpa::block_import(
        client.clone(),
        GRANDPA_JUSTIFICATION_PERIOD,
        &client,
        select_chain.clone(),
        telemetry.as_ref().map(|x| x.handle()),
    )?;

    let overrides = crate::rpc::overrides_handle(client.clone());
    let frontier_backend = match eth_config.frontier_backend_type {
        BackendType::KeyValue => FrontierBackend::KeyValue(fc_db::kv::Backend::open(
            Arc::clone(&client),
            &config.database,
            &db_config_dir(config),
        )?),
        BackendType::Sql => {
            let db_path = db_config_dir(config).join("sql");
            std::fs::create_dir_all(&db_path).expect("failed creating sql db directory");
            let backend = futures::executor::block_on(fc_db::sql::Backend::new(
                fc_db::sql::BackendConfig::Sqlite(fc_db::sql::SqliteBackendConfig {
                    path: Path::new("sqlite:///")
                        .join(db_path)
                        .join("frontier.db3")
                        .to_str()
                        .unwrap(),
                    create_if_missing: true,
                    thread_count: eth_config.frontier_sql_backend_thread_count,
                    cache_size: eth_config.frontier_sql_backend_cache_size,
                }),
                eth_config.frontier_sql_backend_pool_size,
                std::num::NonZeroU32::new(eth_config.frontier_sql_backend_num_ops_timeout),
                overrides.clone(),
            ))
            .unwrap_or_else(|err| panic!("failed creating sql backend: {:?}", err));
            FrontierBackend::Sql(backend)
        }
    };

    let frontier_block_import =
        FrontierBlockImport::new(grandpa_block_import.clone(), client.clone());

    let slot_duration = sc_consensus_aura::slot_duration(&*client)?;
    let target_gas_price = eth_config.target_gas_price;
    let create_inherent_data_providers = move |_, ()| async move {
        let timestamp = sp_timestamp::InherentDataProvider::from_system_time();
        let slot =
            sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
                *timestamp,
                slot_duration,
            );
        let dynamic_fee = fp_dynamic_fee::InherentDataProvider(U256::from(target_gas_price));
        Ok((slot, timestamp, dynamic_fee))
    };

    let import_queue =
        sc_consensus_aura::import_queue::<AuraPair, _, _, _, _, _>(ImportQueueParams {
            block_import: frontier_block_import.clone(),
            justification_import: Some(Box::new(grandpa_block_import.clone())),
            client: client.clone(),
            create_inherent_data_providers,
            spawner: &task_manager.spawn_essential_handle(),
            registry: config.prometheus_registry(),
            check_for_equivocation: Default::default(),
            telemetry: telemetry.as_ref().map(|x| x.handle()),
            compatibility_mode: Default::default(),
        })?;

    Ok(sc_service::PartialComponents {
        client,
        backend,
        keystore_container,
        task_manager,
        select_chain,
        import_queue,
        transaction_pool,
        other: (
            telemetry,
            Box::new(frontier_block_import),
            grandpa_link,
            frontier_backend,
            overrides,
        ),
    })
}
pub struct RunFullParams {
    pub config: Configuration,
    pub eth_config: EthConfiguration,
    pub rpc_config: RpcConfig,
    pub debug_output: Option<std::path::PathBuf>,
    pub auto_insert_keys: bool,
}
/// Builds a new service for a full client.
pub async fn new_full(
    RunFullParams {
        mut config,
        eth_config,
        rpc_config,
        debug_output,
        auto_insert_keys,
    }: RunFullParams,
) -> Result<TaskManager, ServiceError> {
    let sc_service::PartialComponents {
        client,
        backend,
        mut task_manager,
        import_queue,
        keystore_container,
        select_chain,
        transaction_pool,
        other: (mut telemetry, block_import, grandpa_link, frontier_backend, overrides),
    } = new_partial(&config, &eth_config)?;

    if config.role.is_authority() {
        if auto_insert_keys {
            crate::utils::insert_controller_account_keys_into_keystore(
                &config,
                Some(keystore_container.keystore()),
            );
        } else {
            crate::utils::insert_dev_controller_account_keys_into_keystore(
                &config,
                Some(keystore_container.keystore()),
            );
        }

        // finally check if keys are inserted correctly
        if config.chain_spec.chain_type() != ChainType::Development {
            if crate::utils::ensure_all_keys_exist_in_keystore(keystore_container.keystore())
                .is_err()
            {
                println!("
			++++++++++++++++++++++++++++++++++++++++++++++++
				Validator keys not found, validator keys are essential to run a validator on
				Tangle Network, refer to https://docs.webb.tools/docs/ecosystem-roles/validator/required-keys/ on
				how to generate and insert keys. OR start the node with --auto-insert-keys to automatically generate the keys.
			++++++++++++++++++++++++++++++++++++++++++++++++
			\n");
                panic!("Keys not detected!")
            }
        }
    }

    let FrontierPartialComponents {
        filter_pool,
        fee_history_cache,
        fee_history_cache_limit,
    } = new_frontier_partial(&eth_config)?;

    let mut net_config = sc_network::config::FullNetworkConfiguration::new(&config.network);

    let grandpa_protocol_name = sc_consensus_grandpa::protocol_standard_name(
        &client
            .block_hash(0)
            .ok()
            .flatten()
            .expect("Genesis block exists; qed"),
        &config.chain_spec,
    );

    net_config.add_notification_protocol(sc_consensus_grandpa::grandpa_peers_set_config(
        grandpa_protocol_name.clone(),
    ));

    net_config.add_notification_protocol(ecdsa_peers_set_config(KEYGEN_PROTOCOL_CHANNEL.into()));

    net_config.add_notification_protocol(ecdsa_peers_set_config(SIGNING_PROTOCOL_CHANNEL.into()));

    let warp_sync = Arc::new(sc_consensus_grandpa::warp_proof::NetworkProvider::new(
        backend.clone(),
        grandpa_link.shared_authority_set().clone(),
        Vec::default(),
    ));

    let (network, system_rpc_tx, tx_handler_controller, network_starter, sync_service) =
        sc_service::build_network(sc_service::BuildNetworkParams {
            config: &config,
            net_config,
            client: client.clone(),
            transaction_pool: transaction_pool.clone(),
            spawn_handle: task_manager.spawn_handle(),
            import_queue,
            block_announce_validator_builder: None,
            warp_sync_params: Some(sc_service::WarpSyncParams::WithProvider(warp_sync)),
        })?;

    let role = config.role.clone();
    let force_authoring = config.force_authoring;
    let _backoff_authoring_blocks: Option<()> = None;
    let name = config.network.node_name.clone();
    let enable_grandpa = !config.disable_grandpa;
    let prometheus_registry = config.prometheus_registry().cloned();

    if config.offchain_worker.enabled {
        task_manager.spawn_handle().spawn(
            "offchain-workers-runner",
            "offchain-work",
            sc_offchain::OffchainWorkers::new(sc_offchain::OffchainWorkerOptions {
                runtime_api_provider: client.clone(),
                keystore: Some(keystore_container.keystore()),
                offchain_db: backend.offchain_storage(),
                transaction_pool: Some(OffchainTransactionPoolFactory::new(
                    transaction_pool.clone(),
                )),
                network_provider: network.clone(),
                is_validator: role.is_authority(),
                enable_http_requests: true,
                custom_extensions: move |_| vec![],
            })
            .run(client.clone(), task_manager.spawn_handle())
            .boxed(),
        );
    }

    // Channel for the rpc handler to communicate with the authorship task.
    let (command_sink, _commands_stream) = mpsc::channel(1000);

    // Sinks for pubsub notifications.
    // Everytime a new subscription is created, a new mpsc channel is added to the sink pool.
    // The MappingSyncWorker sends through the channel on block import and the subscription emits a
    // notification to the subscriber on receiving a message through this channel. This way we avoid
    // race conditions when using native substrate block import notification stream.
    let pubsub_notification_sinks: fc_mapping_sync::EthereumBlockNotificationSinks<
        fc_mapping_sync::EthereumBlockNotification<Block>,
    > = Default::default();
    let pubsub_notification_sinks = Arc::new(pubsub_notification_sinks);

    // for ethereum-compatibility rpc.
    config.rpc_id_provider = Some(Box::new(fc_rpc::EthereumSubIdProvider));

    let slot_duration = sc_consensus_aura::slot_duration(&*client)?;
    let target_gas_price = eth_config.target_gas_price;
    let pending_create_inherent_data_providers = move |_, ()| async move {
        let current = sp_timestamp::InherentDataProvider::from_system_time();
        let next_slot = current.timestamp().as_millis() + slot_duration.as_millis();
        let timestamp = sp_timestamp::InherentDataProvider::new(next_slot.into());
        let slot =
            sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
                *timestamp,
                slot_duration,
            );
        let dynamic_fee = fp_dynamic_fee::InherentDataProvider(U256::from(target_gas_price));
        Ok((slot, timestamp, dynamic_fee))
    };

    let ethapi_cmd = rpc_config.ethapi.clone();
    let tracing_requesters =
        if ethapi_cmd.contains(&EthApi::Debug) || ethapi_cmd.contains(&EthApi::Trace) {
            crate::rpc::tracing::spawn_tracing_tasks(
                &task_manager,
                client.clone(),
                backend.clone(),
                frontier_backend.clone(),
                overrides.clone(),
                &rpc_config,
                prometheus_registry.clone(),
            )
        } else {
            crate::rpc::tracing::RpcRequesters {
                debug: None,
                trace: None,
            }
        };
    let eth_rpc_params = crate::rpc::EthDeps {
        client: client.clone(),
        pool: transaction_pool.clone(),
        graph: transaction_pool.pool().clone(),
        converter: Some(TransactionConverter),
        is_authority: config.role.is_authority(),
        enable_dev_signer: eth_config.enable_dev_signer,
        network: network.clone(),
        sync: sync_service.clone(),
        frontier_backend: match frontier_backend.clone() {
            fc_db::Backend::KeyValue(b) => Arc::new(b),
            fc_db::Backend::Sql(b) => Arc::new(b),
        },
        overrides: overrides.clone(),
        block_data_cache: Arc::new(fc_rpc::EthBlockDataCacheTask::new(
            task_manager.spawn_handle(),
            overrides.clone(),
            eth_config.eth_log_block_cache,
            eth_config.eth_statuses_cache,
            prometheus_registry.clone(),
        )),
        filter_pool: filter_pool.clone(),
        max_past_logs: eth_config.max_past_logs,
        fee_history_cache: fee_history_cache.clone(),
        fee_history_cache_limit,
        execute_gas_limit_multiplier: eth_config.execute_gas_limit_multiplier,
        forced_parent_hashes: None,
        tracing_config: Some(crate::rpc::eth::TracingConfig {
            tracing_requesters: tracing_requesters.clone(),
            trace_filter_max_count: rpc_config.ethapi_trace_max_count,
        }),
        pending_create_inherent_data_providers,
    };

    let rpc_builder = {
        let client = client.clone();
        let pool = transaction_pool.clone();
        let pubsub_notification_sinks = pubsub_notification_sinks.clone();
        Box::new(move |deny_unsafe, subscription_task_executor| {
            let deps = crate::rpc::FullDeps {
                client: client.clone(),
                pool: pool.clone(),
                deny_unsafe,
                command_sink: Some(command_sink.clone()),
                eth: eth_rpc_params.clone(),
            };
            if ethapi_cmd.contains(&EthApi::Debug) || ethapi_cmd.contains(&EthApi::Trace) {
                crate::rpc::create_full(
                    deps,
                    subscription_task_executor,
                    pubsub_notification_sinks.clone(),
                )
                .map_err(Into::into)
            } else {
                crate::rpc::create_full(
                    deps,
                    subscription_task_executor,
                    pubsub_notification_sinks.clone(),
                )
                .map_err(Into::into)
            }
        })
    };

    spawn_frontier_tasks(
        &task_manager,
        client.clone(),
        backend.clone(),
        frontier_backend,
        filter_pool,
        overrides,
        fee_history_cache,
        fee_history_cache_limit,
        sync_service.clone(),
        pubsub_notification_sinks,
    )
    .await;

    if role.is_authority() {
        // setup debug logging
        let local_peer_id = network.local_peer_id();

        /*
        let dkg_params = dkg_gadget::DKGParams {
            client: client.clone(),
            backend: backend.clone(),
            key_store: Some(keystore_container.keystore()),
            network: network.clone(),
            sync_service: sync_service.clone(),
            prometheus_registry: prometheus_registry.clone(),
            local_keystore: Some(keystore_container.local_keystore()),
            _block: std::marker::PhantomData::<Block>,
            debug_logger,
        };*/

        let ecdsa_config = MpEcdsaProtocolConfig { account_id: role };

        let logger = DebugLogger {
            peer_id: local_peer_id.to_string(),
        };

        let local_key_store = ECDSAKeyStore::in_memory();

        let task = async move {
            let (_, gossip_network_keygen) = NetworkGossipEngineBuilder::new(
                KEYGEN_PROTOCOL_CHANNEL.into(),
                local_key_store.clone(),
            )
            .build(network.clone(), sync_service.clone(), None, logger.clone())
            .expect("Failed to build Keygen network");
            let (_, gossip_network_signing) = NetworkGossipEngineBuilder::new(
                SIGNING_PROTOCOL_CHANNEL.into(),
                local_key_store.clone(),
            )
            .build(network.clone(), sync_service.clone(), None, logger.clone())
            .expect("Failed to build Signing network");
            // We assume, for now, that we will handle both keygen and signing ecdsa jobs
            // TODO: only run keygen and/or signing depending on the role
            if let Err(err) = crate::run(
                ecdsa_config,
                client.clone(),
                logger.clone(),
                local_key_store,
                gossip_network_keygen,
                gossip_network_signing,
            )
            .await
            {
                logger.error(format!("Error running mp-ecdsa protocol: {:?}", err));
            }
        };

        // Start the ECDSA protocol
        task_manager
            .spawn_essential_handle()
            .spawn_blocking("ecdsa-protocol", None, task);
    }
    let params = sc_service::SpawnTasksParams {
        network: network.clone(),
        client: client.clone(),
        keystore: keystore_container.keystore(),
        task_manager: &mut task_manager,
        transaction_pool: transaction_pool.clone(),
        rpc_builder,
        backend: backend.clone(),
        system_rpc_tx,
        tx_handler_controller,
        sync_service: sync_service.clone(),
        config,
        telemetry: telemetry.as_mut(),
    };
    let _rpc_handlers = sc_service::spawn_tasks(params)?;

    if role.is_authority() {
        let proposer_factory = sc_basic_authorship::ProposerFactory::new(
            task_manager.spawn_handle(),
            client.clone(),
            transaction_pool.clone(),
            prometheus_registry.as_ref(),
            telemetry.as_ref().map(|x| x.handle()),
        );

        let slot_duration = sc_consensus_aura::slot_duration(&*client)?;
        let target_gas_price = eth_config.target_gas_price;
        let create_inherent_data_providers = move |_, ()| async move {
            let timestamp = sp_timestamp::InherentDataProvider::from_system_time();
            let slot = sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
                *timestamp,
                slot_duration,
            );
            let dynamic_fee = fp_dynamic_fee::InherentDataProvider(U256::from(target_gas_price));
            Ok((slot, timestamp, dynamic_fee))
        };

        let aura = sc_consensus_aura::start_aura::<AuraPair, _, _, _, _, _, _, _, _, _, _>(
            sc_consensus_aura::StartAuraParams {
                slot_duration,
                client,
                select_chain,
                block_import,
                proposer_factory,
                sync_oracle: sync_service.clone(),
                justification_sync_link: sync_service.clone(),
                create_inherent_data_providers,
                force_authoring,
                backoff_authoring_blocks: Option::<()>::None,
                keystore: keystore_container.keystore(),
                block_proposal_slot_portion: sc_consensus_aura::SlotProportion::new(2f32 / 3f32),
                max_block_proposal_slot_portion: None,
                telemetry: telemetry.as_ref().map(|x| x.handle()),
                compatibility_mode: sc_consensus_aura::CompatibilityMode::None,
            },
        )?;

        // the AURA authoring task is considered essential, i.e. if it
        // fails we take down the service with it.
        task_manager
            .spawn_essential_handle()
            .spawn_blocking("aura", Some("block-authoring"), aura);
    }

    // if the node isn't actively participating in consensus then it doesn't
    // need a keystore, regardless of which protocol we use below.
    let keystore = if role.is_authority() {
        Some(keystore_container.keystore())
    } else {
        None
    };

    let grandpa_config = sc_consensus_grandpa::Config {
        // FIXME #1578 make this available through chainspec
        gossip_duration: Duration::from_millis(333),
        justification_generation_period: GRANDPA_JUSTIFICATION_PERIOD,
        name: Some(name),
        observer_enabled: false,
        keystore,
        local_role: role,
        telemetry: telemetry.as_ref().map(|x| x.handle()),
        protocol_name: grandpa_protocol_name,
    };

    if enable_grandpa {
        // start the full GRANDPA voter
        // NOTE: non-authorities could run the GRANDPA observer protocol, but at
        // this point the full voter should provide better guarantees of block
        // and vote data availability than the observer. The observer has not
        // been tested extensively yet and having most nodes in a network run it
        // could lead to finality stalls.
        let grandpa_config = sc_consensus_grandpa::GrandpaParams {
            config: grandpa_config,
            link: grandpa_link,
            network,
            sync: Arc::new(sync_service),
            voting_rule: sc_consensus_grandpa::VotingRulesBuilder::default().build(),
            prometheus_registry,
            shared_voter_state: SharedVoterState::empty(),
            telemetry: telemetry.as_ref().map(|x| x.handle()),
            offchain_tx_pool_factory: OffchainTransactionPoolFactory::new(transaction_pool),
        };

        // the GRANDPA voter task is considered infallible, i.e.
        // if it fails we take down the service with it.
        task_manager.spawn_essential_handle().spawn_blocking(
            "grandpa-voter",
            None,
            sc_consensus_grandpa::run_grandpa_voter(grandpa_config)?,
        );
    }

    network_starter.start_network();
    Ok(task_manager)
}

pub fn new_chain_ops(
    config: &mut Configuration,
    eth_config: &EthConfiguration,
) -> Result<
    (
        Arc<FullClient>,
        Arc<FullBackend>,
        BasicQueue<Block>,
        TaskManager,
        FrontierBackend,
    ),
    ServiceError,
> {
    config.keystore = sc_service::config::KeystoreConfig::InMemory;
    let sc_service::PartialComponents {
        client,
        backend,
        import_queue,
        task_manager,
        other,
        ..
    } = new_partial(config, eth_config)?;
    Ok((client, backend, import_queue, task_manager, other.3))
}

/// Returns the configuration value to put in
/// [`sc_network::config::NetworkConfiguration::extra_sets`].
pub fn ecdsa_peers_set_config(
    protocol_name: ProtocolName,
) -> sc_network::config::NonDefaultSetConfig {
    NetworkGossipEngineBuilder::set_config(protocol_name)
}
