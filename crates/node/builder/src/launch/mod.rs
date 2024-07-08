//! Abstraction for launching a node.

use std::{future::Future, ops::Deref};

use futures::{future::Either, stream, stream_select, StreamExt};
use reth_auto_seal_consensus::AutoSealClient;
use reth_beacon_consensus::{
    hooks::{EngineHooks, PruneHook, StaticFileHook},
    BeaconConsensusEngine,
};
use reth_engine_util::EngineMessageStreamExt;
use reth_exex::ExExManagerHandle;
use reth_network::{FetchClient, NetworkEvents};
use reth_node_api::{FullNodeComponentsExt, FullNodeTypes};
use reth_node_core::{
    dirs::{ChainPath, DataDirPath},
    exit::NodeExitFuture,
};
use reth_node_events::{cl::ConsensusLayerHealthEvents, node};
use reth_primitives::format_ether;
use reth_provider::providers::BlockchainProvider;
use reth_tasks::TaskExecutor;
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::TransactionPool;
use tokio::sync::mpsc::unbounded_channel;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::{
    builder::{NodeAddOns, NodeTypesAdapter},
    common::InitializedComponents,
    components::{NodeComponents, NodeComponentsBuilder},
    hooks::NodeHooks,
    node::FullNode,
    rpc::{RethRpcServerHandles, RpcAdapter, RpcRegistry},
    EngineAdapter, InitializedComponentsExt, NodeAdapter, NodeAdapterExt,
    NodeBuilderWithComponents, NodeHandle, StageExtComponentsBuild,
};

pub mod common;
pub use common::LaunchContext;
mod exex;
pub use exex::ExExLauncher;

/// A general purpose trait that launches a new node of any kind.
///
/// Acts as a node factory.
///
/// This is essentially the launch logic for a node.
///
/// See also [`DefaultNodeLauncher`] and [`NodeBuilderWithComponents::launch_with`]
pub trait LaunchNode<Target> {
    /// The node type that is created.
    type Node;

    /// Create and return a new node asynchronously.
    fn launch_node(self, target: Target) -> impl Future<Output = eyre::Result<Self::Node>> + Send;
}

impl<F, Target, Fut, Node> LaunchNode<Target> for F
where
    F: FnOnce(Target) -> Fut + Send,
    Fut: Future<Output = eyre::Result<Node>> + Send,
{
    type Node = Node;

    fn launch_node(self, target: Target) -> impl Future<Output = eyre::Result<Self::Node>> + Send {
        self(target)
    }
}

/// The default launcher for a node.
#[derive(Debug)]
pub struct DefaultNodeLauncher {
    /// The task executor for the node.
    pub ctx: LaunchContext,
}

impl DefaultNodeLauncher {
    /// Create a new instance of the default node launcher.
    pub const fn new(task_executor: TaskExecutor, data_dir: ChainPath<DataDirPath>) -> Self {
        Self { ctx: LaunchContext::new(task_executor, data_dir) }
    }
}

impl<T, CB> LaunchNode<NodeBuilderWithComponents<T, CB>> for DefaultNodeLauncher
where
    T: FullNodeTypes<Provider = BlockchainProvider<<T as FullNodeTypes>::DB>>,
    CB: NodeComponentsBuilder<T>,
{
    type Node = NodeHandle<
        NodeAdapterExt<
            NodeAdapter<T, <CB as NodeComponentsBuilder<T>>::Components>,
            BlockchainProvider<<T as FullNodeTypes>::DB>,
            Either<AutoSealClient, FetchClient>,
        >,
    >;

    async fn launch_node(
        self,
        target: NodeBuilderWithComponents<T, CB>,
    ) -> eyre::Result<Self::Node> {
        let Self { ctx } = self;
        let NodeBuilderWithComponents {
            adapter: NodeTypesAdapter { database },
            components_builder,
            add_ons: NodeAddOns { hooks, exexs: installed_exex },
            config,
        } = target;
        let NodeHooks { on_components_initialized, on_node_started, .. } = hooks;

        // setup the launch context
        let ctx = ctx
            .with_configured_globals()
            // load the toml config
            .with_loaded_toml_config(config).await?
            // add resolved peers
            .with_resolved_peers().await?
            // attach the database
            .attach(database.clone())
            // ensure certain settings take effect
            .with_adjusted_configs()
            // Create the provider factory
            .with_provider_factory().await?
            .inspect(|_| {
                info!(target: "reth::cli", "Database opened");
            })
            .with_prometheus().await?
            .inspect(|this| {
                debug!(target: "reth::cli", chain=%this.chain_id(), genesis=?this.genesis_hash(), "Initializing genesis");
            })
            .with_genesis()?
            .inspect(|this| {
                info!(target: "reth::cli", "\n{}", this.chain_spec().display_hardforks());
            })
            .with_metrics()
            // passing FullNodeTypes as type parameter here so that we can build
            // later the components.
            .with_blockchain_db::<T>().await?
            .with_components(components_builder, on_components_initialized).await?;

        // spawn exexs
        let exex_manager_handle = ExExLauncher::new(
            ctx.head(),
            ctx.node().clone(),
            installed_exex,
            ctx.configs().clone(),
        )
        .launch()
        .await;

        // create pipeline
        let network_client = ctx.node().network().fetch_client().await?;
        let (consensus_engine_tx, consensus_engine_rx) = unbounded_channel();

        let node_config = ctx.node_config();
        let consensus_engine_stream = UnboundedReceiverStream::from(consensus_engine_rx)
            .maybe_skip_fcu(node_config.debug.skip_fcu)
            .maybe_skip_new_payload(node_config.debug.skip_new_payload)
            // Store messages _after_ skipping so that `replay-engine` command
            // would replay only the messages that were observed by the engine
            // during this run.
            .maybe_store_messages(node_config.debug.engine_api_store.clone());

        let max_block = ctx.max_block(network_client.clone()).await?;
        let mut hooks = EngineHooks::new();

        let static_file_producer = ctx.static_file_producer();
        let static_file_producer_events = static_file_producer.lock().events();
        hooks.add(StaticFileHook::new(
            static_file_producer.clone(),
            Box::new(ctx.task_executor().clone()),
        ));
        info!(target: "reth::cli", "StaticFileProducer initialized");

        // Configure the pipeline
        let pipeline_exex_handle =
            exex_manager_handle.clone().unwrap_or_else(ExExManagerHandle::empty);
        let (pipeline, client) = if ctx.is_dev() {
            info!(target: "reth::cli", "Starting Reth in dev mode");

            for (idx, (address, alloc)) in ctx.chain_spec().genesis.alloc.iter().enumerate() {
                info!(target: "reth::cli", "Allocated Genesis Account: {:02}. {} ({} ETH)", idx, address.to_string(), format_ether(alloc.balance));
            }

            // install auto-seal
            let mining_mode =
                ctx.dev_mining_mode(ctx.node().pool().pending_transactions_listener());
            info!(target: "reth::cli", mode=%mining_mode, "configuring dev mining mode");

            let (_, client, mut task) = reth_auto_seal_consensus::AutoSealBuilder::new(
                ctx.chain_spec(),
                ctx.blockchain_db().clone(),
                ctx.pool().clone(),
                consensus_engine_tx.clone(),
                mining_mode,
                ctx.block_executor().clone(),
            )
            .build();

            let pipeline = crate::setup::build_networked_pipeline(
                &ctx.toml_config().stages,
                client.clone(),
                ctx.consensus(),
                ctx.provider_factory().clone(),
                ctx.task_executor(),
                ctx.sync_metrics_tx(),
                ctx.prune_config(),
                max_block,
                static_file_producer,
                ctx.node().block_executor().clone(),
                pipeline_exex_handle,
            )
            .await?;

            let pipeline_events = pipeline.events();
            task.set_pipeline_events(pipeline_events);
            debug!(target: "reth::cli", "Spawning auto mine task");
            ctx.task_executor().spawn(Box::pin(task));

            (pipeline, Either::Left(client))
        } else {
            let pipeline = crate::setup::build_networked_pipeline(
                &ctx.toml_config().stages,
                network_client.clone(),
                ctx.consensus(),
                ctx.provider_factory().clone(),
                ctx.task_executor(),
                ctx.sync_metrics_tx(),
                ctx.prune_config(),
                max_block,
                static_file_producer,
                ctx.node().block_executor().clone(),
                pipeline_exex_handle,
            )
            .await?;

            (pipeline, Either::Right(network_client.clone()))
        };

        let pipeline_events = pipeline.events();

        let initial_target = ctx.node_config().debug.tip;

        let mut pruner_builder =
            ctx.pruner_builder().max_reorg_depth(ctx.tree_config().max_reorg_depth() as usize);
        if let Some(exex_manager_handle) = &exex_manager_handle {
            pruner_builder =
                pruner_builder.finished_exex_height(exex_manager_handle.finished_height());
        }

        let pruner = pruner_builder.build(ctx.provider_factory().clone());

        let pruner_events = pruner.events();
        info!(target: "reth::cli", prune_config=?ctx.prune_config().unwrap_or_default(), "Pruner initialized");
        hooks.add(PruneHook::new(pruner, Box::new(ctx.task_executor().clone())));

        // Configure the consensus engine
        let (beacon_consensus_engine, beacon_engine_handle) = BeaconConsensusEngine::with_channel(
            client,
            pipeline,
            ctx.blockchain_db().clone(),
            Box::new(ctx.task_executor().clone()),
            Box::new(ctx.node().network().clone()),
            max_block,
            ctx.node().payload_builder().clone(),
            initial_target,
            reth_beacon_consensus::MIN_BLOCKS_FOR_PIPELINE_RUN,
            consensus_engine_tx,
            Box::pin(consensus_engine_stream),
            hooks,
        )?;
        info!(target: "reth::cli", "Consensus engine initialized");

        // should move into a new `ConsensusBuilder` trait, like for `RpcBuilder`
        let engine = EngineAdapter::new(beacon_consensus_engine, beacon_engine_handle);
        ctx.right().engine(engine);

        let events = stream_select!(
            ctx.node().network().event_listener().map(Into::into),
            beacon_engine_handle.event_listener().map(Into::into),
            pipeline_events.map(Into::into),
            if ctx.node_config().debug.tip.is_none() && !ctx.is_dev() {
                Either::Left(
                    ConsensusLayerHealthEvents::new(Box::new(ctx.blockchain_db().clone()))
                        .map(Into::into),
                )
            } else {
                Either::Right(stream::empty())
            },
            pruner_events.map(Into::into),
            static_file_producer_events.map(Into::into),
        );
        ctx.task_executor().spawn_critical(
            "events task",
            node::handle_events(
                Some(ctx.node().network().clone()),
                Some(ctx.head().number),
                events,
                database.clone(),
            ),
        );

        let consensus_engine_shutdown_rx =
            ctx.right().consensus_engine_shutdown().expect("should be launched");
        // temp here until building other components moved into `OnComponentsInitializedHook`s, will
        // be called in `LaunchContextWith::with_components -> NodeAdapterExt`
        let node = ctx.right().build().await;

        let full_node = FullNode {
            evm_config: node.evm_config().clone(),
            block_executor: node.block_executor().clone(),
            pool: node.pool().clone(),
            network: node.network().clone(),
            provider: node.provider().clone(),
            payload_builder: node.payload_builder().clone(),
            task_executor: node.task_executor().clone(),
            rpc_server_handles: node.rpc().rpc_server_handles(),
            rpc_registry: node.rpc().rpc_registry(),
            config: ctx.node_config().clone(),
            data_dir: ctx.data_dir().clone(),
        };
        // Notify on node started
        on_node_started.on_event(full_node.clone())?;

        let handle = NodeHandle {
            node_exit_future: NodeExitFuture::new(
                async { Ok(consensus_engine_shutdown_rx.await??) },
                full_node.config.debug.terminate,
            ),
            node: full_node,
        };

        Ok(handle)
    }
}
