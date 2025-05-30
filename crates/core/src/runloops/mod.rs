use solana_message::{v0::LoadedAddresses, SimpleAddressLoader};
use solana_sdk::transaction::MessageHash;
use solana_transaction::sanitized::SanitizedTransaction;
use solana_transaction_status::{InnerInstruction, InnerInstructions, TransactionStatusMeta};
use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::Arc,
    thread::{sleep, JoinHandle},
    time::{Duration, Instant},
};
use surfpool_subgraph::SurfpoolSubgraphPlugin;
use tokio::sync::RwLock;

use crate::{
    rpc::{
        self, accounts_data::AccountsData, accounts_scan::AccountsScan, admin::AdminRpc,
        bank_data::BankData, full::Full, minimal::Minimal, surfnet_cheatcodes::SvmTricksRpc,
        SurfpoolMiddleware,
    },
    surfnet::{GeyserEvent, SurfnetSvm},
    PluginManagerCommand,
};
use agave_geyser_plugin_interface::geyser_plugin_interface::{
    GeyserPlugin, ReplicaTransactionInfoV2, ReplicaTransactionInfoVersions,
};
use crossbeam::select;
use crossbeam_channel::{unbounded, Receiver, Sender};
use ipc_channel::{
    ipc::{IpcOneShotServer, IpcReceiver},
    router::RouterProxy,
};
use jsonrpc_core::MetaIoHandler;
use jsonrpc_http_server::{DomainsValidation, ServerBuilder};
use surfpool_types::{
    BlockProductionMode, ClockCommand, ClockEvent, SchemaDataSourcingEvent, SubgraphPluginConfig,
};
use surfpool_types::{SimnetCommand, SimnetEvent, SubgraphCommand, SurfpoolConfig};

const BLOCKHASH_SLOT_TTL: u64 = 75;

pub async fn start_local_surfnet_runloop(
    svm_locker: Arc<RwLock<SurfnetSvm>>,
    config: SurfpoolConfig,
    subgraph_commands_tx: Sender<SubgraphCommand>,
    simnet_commands_tx: Sender<SimnetCommand>,
    simnet_commands_rx: Receiver<SimnetCommand>,
    geyser_events_rx: Receiver<GeyserEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(simnet) = config.simnets.first() else {
        return Ok(());
    };
    let block_production_mode = simnet.block_production_mode.clone();

    let mut surfnet_svm = svm_locker.write().await;
    surfnet_svm.airdrop_pubkeys(simnet.airdrop_token_amount, &simnet.airdrop_addresses);
    let _ = surfnet_svm.connect(&simnet.remote_rpc_url).await?;
    let simnet_events_tx_cc = surfnet_svm.simnet_events_tx.clone();
    drop(surfnet_svm);

    let (plugin_manager_commands_rx, _rpc_handle) =
        start_rpc_server_runloop(&config, &simnet_commands_tx, svm_locker.clone()).await?;

    let simnet_config = simnet.clone();

    if !config.plugin_config_path.is_empty() {
        match start_geyser_runloop(
            plugin_manager_commands_rx,
            subgraph_commands_tx.clone(),
            simnet_events_tx_cc.clone(),
            geyser_events_rx,
        ) {
            Ok(_) => {}
            Err(e) => {
                let _ = simnet_events_tx_cc
                    .send(SimnetEvent::error(format!("Geyser plugin failed: {e}")));
            }
        };
    }

    let (clock_event_rx, clock_command_tx) = start_clock_runloop(simnet_config.slot_time);

    let _ = simnet_events_tx_cc.send(SimnetEvent::Ready);

    start_block_production_runloop(
        clock_event_rx,
        clock_command_tx,
        simnet_commands_rx,
        svm_locker,
        block_production_mode,
    )
    .await
}

pub async fn start_block_production_runloop(
    clock_event_rx: Receiver<ClockEvent>,
    clock_command_tx: Sender<ClockCommand>,
    simnet_commands_rx: Receiver<SimnetCommand>,
    svm_locker: Arc<RwLock<SurfnetSvm>>,
    mut block_production_mode: BlockProductionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let mut do_produce_block = false;

        select! {
            recv(clock_event_rx) -> msg => if let Ok(event) = msg {
                match event {
                    ClockEvent::Tick => {
                        if block_production_mode.eq(&BlockProductionMode::Clock) {
                            do_produce_block = true;
                        }
                    }
                    ClockEvent::ExpireBlockHash => {
                        do_produce_block = true;
                    }
                }
            },
            recv(simnet_commands_rx) -> msg => if let Ok(event) = msg {
                match event {
                    SimnetCommand::SlotForward(_key) => {
                        block_production_mode = BlockProductionMode::Manual;
                        do_produce_block = true;
                    }
                    SimnetCommand::SlotBackward(_key) => {

                    }
                    SimnetCommand::UpdateClock(update) => {
                        let _ = clock_command_tx.send(update);
                        continue
                    }
                    SimnetCommand::UpdateBlockProductionMode(update) => {
                        block_production_mode = update;
                        continue
                    }
                    SimnetCommand::TransactionReceived(_key, transaction, status_tx, skip_preflight) => {
                        let mut svm_writer = svm_locker.write().await;
                        svm_writer.process_transaction(transaction, status_tx ,skip_preflight).await?;
                    }
                    SimnetCommand::Terminate(_) => {
                        std::process::exit(0)
                    }
                }
            },
        }

        {
            if do_produce_block {
                let mut svm_writer = svm_locker.write().await;
                svm_writer.confirm_transactions()?;
                svm_writer.finalize_transactions()?;
                svm_writer.new_blockhash();
            }
        }
    }
}

pub fn start_clock_runloop(mut slot_time: u64) -> (Receiver<ClockEvent>, Sender<ClockCommand>) {
    let (clock_event_tx, clock_event_rx) = unbounded::<ClockEvent>();
    let (clock_command_tx, clock_command_rx) = unbounded::<ClockCommand>();

    let _handle = hiro_system_kit::thread_named("clock").spawn(move || {
        let mut enabled = true;
        let mut block_hash_timeout = Instant::now();

        loop {
            match clock_command_rx.try_recv() {
                Ok(ClockCommand::Pause) => {
                    enabled = false;
                }
                Ok(ClockCommand::Resume) => {
                    enabled = true;
                }
                Ok(ClockCommand::Toggle) => {
                    enabled = !enabled;
                }
                Ok(ClockCommand::UpdateSlotInterval(updated_slot_time)) => {
                    slot_time = updated_slot_time;
                }
                Err(_e) => {}
            }
            sleep(Duration::from_millis(slot_time));
            if enabled {
                let _ = clock_event_tx.send(ClockEvent::Tick);
                // Todo: the block expiration is not completely accurate.
                if block_hash_timeout.elapsed()
                    > Duration::from_millis(BLOCKHASH_SLOT_TTL * slot_time)
                {
                    let _ = clock_event_tx.send(ClockEvent::ExpireBlockHash);
                    block_hash_timeout = Instant::now();
                }
            }
        }
    });

    (clock_event_rx, clock_command_tx)
}

fn start_geyser_runloop(
    plugin_manager_commands_rx: Receiver<PluginManagerCommand>,
    subgraph_commands_tx: Sender<SubgraphCommand>,
    simnet_events_tx: Sender<SimnetEvent>,
    geyser_events_rx: Receiver<GeyserEvent>,
) -> Result<JoinHandle<Result<(), String>>, String> {
    let handle = hiro_system_kit::thread_named("Geyser Plugins Handler").spawn(move || {
        let mut plugin_manager = vec![];

        let ipc_router = RouterProxy::new();
        // Note:
        // At the moment, surfpool-subgraph is the only plugin that we're mounting.
        // Please open an issue http://github.com/txtx/surfpool/issues/new if this is a feature you need!
        //
        // Proof of concept:
        //
        // let geyser_plugin_config_file = PathBuf::from("../../surfpool_subgraph_plugin.json");
        // let contents = "{\"name\": \"surfpool-subgraph\", \"libpath\": \"target/release/libsurfpool_subgraph.dylib\"}";
        // let result: serde_json::Value = json5::from_str(&contents).unwrap();
        // let libpath = result["libpath"]
        //     .as_str()
        //     .unwrap();
        // let mut libpath = PathBuf::from(libpath);
        // if libpath.is_relative() {
        //     let config_dir = geyser_plugin_config_file.parent().ok_or_else(|| {
        //         GeyserPluginManagerError::CannotOpenConfigFile(format!(
        //             "Failed to resolve parent of {geyser_plugin_config_file:?}",
        //         ))
        //     }).unwrap();
        //     libpath = config_dir.join(libpath);
        // }
        // let plugin_name = result["name"].as_str().map(|s| s.to_owned()).unwrap_or(format!("surfpool-subgraph"));
        // let (plugin, lib) = unsafe {
        //     let lib = match Library::new(&surfpool_subgraph_path) {
        //         Ok(lib) => lib,
        //         Err(e) => {
        //             let _ = simnet_events_tx_copy.send(SimnetEvent::ErrorLog(Local::now(), format!("Unable to load plugin {}: {}", plugin_name, e.to_string())));
        //             continue;
        //         }
        //     };
        //     let constructor: Symbol<PluginConstructor> = lib
        //         .get(b"_create_plugin")
        //         .map_err(|e| format!("{}", e.to_string()))?;
        //     let plugin_raw = constructor();
        //     (Box::from_raw(plugin_raw), lib)
        // };

        let err = loop {
            select! {
                recv(plugin_manager_commands_rx) -> msg => {
                    match msg {
                        Ok(event) => {
                            match event {
                                PluginManagerCommand::LoadConfig(uuid, config, notifier) => {
                                    let _ = subgraph_commands_tx.send(SubgraphCommand::CreateSubgraph(uuid, config.data.clone(), notifier));
                                    let mut plugin = SurfpoolSubgraphPlugin::default();

                                    let (server, ipc_token) = IpcOneShotServer::<IpcReceiver<SchemaDataSourcingEvent>>::new().expect("Failed to create IPC one-shot server.");
                                    let subgraph_plugin_config = SubgraphPluginConfig {
                                        uuid,
                                        ipc_token,
                                        subgraph_request: config.data.clone()
                                    };

                                    let config_file = match serde_json::to_string(&subgraph_plugin_config) {
                                        Ok(c) => c,
                                        Err(e) => {
                                            let _ = simnet_events_tx.send(SimnetEvent::error(format!("Failed to serialize subgraph plugin config: {:?}", e)));
                                            continue;
                                        }
                                    };

                                    if let Err(e) = plugin.on_load(&config_file, false) {
                                        let _ = simnet_events_tx.send(SimnetEvent::error(format!("Failed to load Geyser plugin: {:?}", e)));
                                    };
                                    if let Ok((_, rx)) = server.accept() {
                                        let subgraph_rx = ipc_router.route_ipc_receiver_to_new_crossbeam_receiver::<SchemaDataSourcingEvent>(rx);
                                        let _ = subgraph_commands_tx.send(SubgraphCommand::ObserveSubgraph(subgraph_rx));
                                    };
                                    let plugin: Box<dyn GeyserPlugin> = Box::new(plugin);
                                    plugin_manager.push(plugin);
                                    let _ = simnet_events_tx.send(SimnetEvent::PluginLoaded("surfpool-subgraph".into()));
                                }
                            }
                        },
                        Err(e) => {
                            break format!("Failed to read plugin manager command: {:?}", e);
                        },
                    }
                },
                recv(geyser_events_rx) -> msg => match msg {
                    Err(e) => {
                        break format!("Failed to read new transaction to send to Geyser plugin: {e}");
                    },
                    Ok(GeyserEvent::NewTransaction(transaction, transaction_metadata, slot)) => {
                        let mut inner_instructions = vec![];
                        for (i,inner) in transaction_metadata.inner_instructions.iter().enumerate() {
                            inner_instructions.push(
                                InnerInstructions {
                                    index: i as u8,
                                    instructions: inner.iter().map(|i| InnerInstruction {
                                        instruction: i.instruction.clone(),
                                        stack_height: Some(i.stack_height as u32)
                                    }).collect()
                                }
                            )
                        }

                        let transaction_status_meta = TransactionStatusMeta {
                            status: Ok(()),
                            fee: 0,
                            pre_balances: vec![],
                            post_balances: vec![],
                            inner_instructions: Some(inner_instructions),
                            log_messages: Some(transaction_metadata.logs.clone()),
                            pre_token_balances: None,
                            post_token_balances: None,
                            rewards: None,
                            loaded_addresses: LoadedAddresses {
                                writable: vec![],
                                readonly: vec![],
                            },
                            return_data: Some(transaction_metadata.return_data.clone()),
                            compute_units_consumed: Some(transaction_metadata.compute_units_consumed),
                        };

                        let transaction = match SanitizedTransaction::try_create(transaction, MessageHash::Compute, None, SimpleAddressLoader::Disabled, &HashSet::new()) {
                        Ok(tx) => tx,
                            Err(e) => {
                                let _ = simnet_events_tx.send(SimnetEvent::error(format!("Failed to notify Geyser plugin of new transaction: failed to serialize transaction: {:?}", e)));
                                continue;
                            }
                        };

                        let transaction_replica = ReplicaTransactionInfoV2 {
                            signature: &transaction_metadata.signature,
                            is_vote: false,
                            transaction: &transaction,
                            transaction_status_meta: &transaction_status_meta,
                            index: 0
                        };
                        for plugin in plugin_manager.iter() {
                            if let Err(e) = plugin.notify_transaction(ReplicaTransactionInfoVersions::V0_0_2(&transaction_replica), slot) {
                                let _ = simnet_events_tx.send(SimnetEvent::error(format!("Failed to notify Geyser plugin of new transaction: {:?}", e)));
                            };
                        }
                    }
                }
            }
        };
        Err(err)
    }).map_err(|e| format!("Failed to spawn Geyser Plugins Handler thread: {:?}", e))?;
    Ok(handle)
}

async fn start_rpc_server_runloop(
    config: &SurfpoolConfig,
    simnet_commands_tx: &Sender<SimnetCommand>,
    svm_locker: Arc<RwLock<SurfnetSvm>>,
) -> Result<(Receiver<PluginManagerCommand>, JoinHandle<()>), String> {
    let (plugin_manager_commands_tx, plugin_manager_commands_rx) = unbounded();
    let simnet_events_tx = svm_locker.read().await.simnet_events_tx.clone();

    let middleware = SurfpoolMiddleware::new(
        svm_locker,
        &simnet_commands_tx,
        &plugin_manager_commands_tx,
        &config.rpc,
    );
    let server_bind: SocketAddr = config
        .rpc
        .get_socket_address()
        .parse::<SocketAddr>()
        .map_err(|e| e.to_string())?;

    let mut io = MetaIoHandler::with_middleware(middleware);
    io.extend_with(rpc::minimal::SurfpoolMinimalRpc.to_delegate());
    io.extend_with(rpc::full::SurfpoolFullRpc.to_delegate());
    io.extend_with(rpc::accounts_data::SurfpoolAccountsDataRpc.to_delegate());
    io.extend_with(rpc::accounts_scan::SurfpoolAccountsScanRpc.to_delegate());
    io.extend_with(rpc::bank_data::SurfpoolBankDataRpc.to_delegate());
    io.extend_with(rpc::surfnet_cheatcodes::SurfnetCheatcodesRpc.to_delegate());
    io.extend_with(rpc::admin::SurfpoolAdminRpc.to_delegate());

    if !config.plugin_config_path.is_empty() {
        io.extend_with(rpc::admin::SurfpoolAdminRpc.to_delegate());
    }

    let _ = std::net::TcpListener::bind(server_bind)
        .map_err(|e| format!("Failed to start RPC server: {}", e))?;

    let _handle = hiro_system_kit::thread_named("RPC Handler")
        .spawn(move || {
            let server = match ServerBuilder::new(io)
                .cors(DomainsValidation::Disabled)
                .start_http(&server_bind)
            {
                Ok(server) => server,
                Err(e) => {
                    let _ = simnet_events_tx.send(SimnetEvent::Aborted(format!(
                        "Failed to start RPC server: {:?}",
                        e
                    )));
                    return;
                }
            };

            server.wait();
            let _ = simnet_events_tx.send(SimnetEvent::Shutdown);
        })
        .map_err(|e| format!("Failed to spawn RPC Handler thread: {:?}", e))?;
    Ok((plugin_manager_commands_rx, _handle))
}
