//! The `SyncManager` facilities the block syncing logic of lighthouse. The current networking
//! specification provides two methods from which to obtain blocks from peers. The `BlocksByRange`
//! request and the `BlocksByRoot` request. The former is used to obtain a large number of
//! blocks and the latter allows for searching for blocks given a block-hash.
//!
//! These two RPC methods are designed for two type of syncing.
//! - Long range (batch) sync, when a client is out of date and needs to the latest head.
//! - Parent lookup - when a peer provides us a block whose parent is unknown to us.
//!
//! Both of these syncing strategies are built into the `SyncManager`.
//!
//! Currently the long-range (batch) syncing method functions by opportunistically downloading
//! batches blocks from all peers who know about a chain that we do not. When a new peer connects
//! which has a later head that is greater than `SLOT_IMPORT_TOLERANCE` from our current head slot,
//! the manager's state becomes `Syncing` and begins a batch syncing process with this peer. If
//! further peers connect, this process is run in parallel with those peers, until our head is
//! within `SLOT_IMPORT_TOLERANCE` of all connected peers.
//!
//! ## Batch Syncing
//!
//! See `RangeSync` for further details.
//!
//! ## Parent Lookup
//!
//! When a block with an unknown parent is received and we are in `Regular` sync mode, the block is
//! queued for lookup. A round-robin approach is used to request the parent from the known list of
//! fully sync'd peers. If `PARENT_FAIL_TOLERANCE` attempts at requesting the block fails, we
//! drop the propagated block and downvote the peer that sent it to us.
//!
//! Block Lookup
//!
//! To keep the logic maintained to the syncing thread (and manage the request_ids), when a block
//! needs to be searched for (i.e if an attestation references an unknown block) this manager can
//! search for the block and subsequently search for parents if needed.

use super::backfill_sync::{BackFillSync, ProcessResult, SyncStart};
use super::block_lookups::BlockLookups;
use super::network_context::SyncNetworkContext;
use super::peer_sync_info::{remote_sync_type, PeerSyncType};
use super::range_sync::{ChainState, RangeSync, RangeSyncType, EPOCHS_PER_BATCH};
use crate::beacon_processor::{ChainSegmentProcessId, FailureMode, WorkEvent as BeaconWorkEvent};
use crate::service::NetworkMessage;
use crate::status::ToStatusMessage;
use beacon_chain::parking_lot::RwLock;
use beacon_chain::{BeaconChain, BeaconChainTypes, BlockError};
use futures::future::OptionFuture;
use lighthouse_network::rpc::methods::MAX_REQUEST_BLOCKS;
use lighthouse_network::types::{NetworkGlobals, SyncState};
use lighthouse_network::SyncInfo;
use lighthouse_network::{PeerAction, PeerId};
use slog::{crit, debug, error, info, trace, Logger};
use std::boxed::Box;
use std::ops::Sub;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use types::{EthSpec, Hash256, SignedBeaconBlock, Slot};

/// The number of slots ahead of us that is allowed before requesting a long-range (batch)  Sync
/// from a peer. If a peer is within this tolerance (forwards or backwards), it is treated as a
/// fully sync'd peer.
///
/// This means that we consider ourselves synced (and hence subscribe to all subnets and block
/// gossip if no peers are further than this range ahead of us that we have not already downloaded
/// blocks for.
pub const SLOT_IMPORT_TOLERANCE: usize = 32;

pub type Id = u32;

/// Id of rpc requests sent by sync to the network.
#[derive(Debug, Hash, PartialEq, Eq, Clone, Copy)]
pub enum RequestId {
    /// Request searching for a block given a hash.
    SingleBlock { id: Id },
    /// Request searching for a block's parent. The id is the chain
    ParentLookup { id: Id },
    /// Request was from the backfill sync algorithm.
    BackFillSync { id: Id },
    /// The request was from a chain in the range sync algorithm.
    RangeSync { id: Id },
}

#[derive(Debug)]
/// A message than can be sent to the sync manager thread.
pub enum SyncMessage<T: EthSpec> {
    /// A useful peer has been discovered.
    AddPeer(PeerId, SyncInfo),

    /// A block has been received from the RPC.
    RpcBlock {
        request_id: RequestId,
        peer_id: PeerId,
        beacon_block: Option<Box<SignedBeaconBlock<T>>>,
        seen_timestamp: Duration,
    },

    /// A block with an unknown parent has been received.
    UnknownBlock(PeerId, Box<SignedBeaconBlock<T>>),

    /// A peer has sent an object that references a block that is unknown. This triggers the
    /// manager to attempt to find the block matching the unknown hash.
    UnknownBlockHash(PeerId, Hash256),

    /// A peer has disconnected.
    Disconnect(PeerId),

    /// An RPC Error has occurred on a request.
    RpcError {
        peer_id: PeerId,
        request_id: RequestId,
    },

    /// A batch has been processed by the block processor thread.
    BatchProcessed {
        sync_type: ChainSegmentProcessId,
        result: BatchProcessResult,
    },

    /// Block processed
    BlockProcessed {
        process_type: BlockProcessType,
        result: Result<(), BlockError<T>>,
    },
}

/// The type of processing specified for a received block.
#[derive(Debug, Clone)]
pub enum BlockProcessType {
    SingleBlock { id: Id },
    ParentLookup { chain_hash: Hash256 },
}

/// The result of processing multiple blocks (a chain segment).
#[derive(Debug)]
pub enum BatchProcessResult {
    /// The batch was completed successfully. It carries whether the sent batch contained blocks.
    Success(bool),
    /// The batch processing failed. It carries whether the processing imported any block.
    Failed {
        imported_blocks: bool,
        peer_action: Option<PeerAction>,
        mode: FailureMode,
    },
}

/// The state of the execution layer connection for block verification.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionState {
    Online,
    Offline,
}

/// A wrapper struct containing a shared lock to the state of the execution layer.
///
/// This struct is passed around to the different sync types which allows them
/// to change the state when the block processing fails due to execution layer
/// failures.
///
/// It also allows them to communicate that the execution layer has gone offline to
/// the sync manager. The sync manager will consequently setup an `ExecutionLayerNotifier`
/// to notify sync when it comes back online.
#[derive(Clone)]
pub struct ExecutionStatusHandler {
    /// Current state of the execution layer.
    ///
    /// A value of `None` indicates that execution isn't enabled.
    state: Arc<RwLock<Option<ExecutionState>>>,
    /// Sends to the sync manager receiver whenever the
    /// execution status is changed to `Offline`.
    sync_manager_tx: tokio::sync::mpsc::Sender<()>,
    log: Logger,
}

impl ExecutionStatusHandler {
    pub fn new(
        state: Option<ExecutionState>,
        log: Logger,
    ) -> (Self, tokio::sync::mpsc::Receiver<()>) {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        (
            Self {
                state: Arc::new(RwLock::new(state)),
                sync_manager_tx: tx,
                log,
            },
            rx,
        )
    }

    /// Returns the current execution status.
    ///
    /// `None` indicates that the execution layer isn't enabled.
    pub fn status(&self) -> Option<ExecutionState> {
        (*self.state.read()).clone()
    }

    /// Sets the `ExecutionState` to offline after sending a message
    /// over the mpsc channel indicating that the execution status has changed
    /// to offline.
    pub fn offline(&self) {
        // Prevent duplicate sends by only updating the state if
        // the current state is online **and**
        // if the send to the sync manager is successful.
        if let Some(ExecutionState::Online) = self.status() {
            if let Err(e) = self.sync_manager_tx.try_send(()) {
                crit!(
                    self.log,
                    "Failed to send message to the sync manager";
                    "error" => ?e
                );
            } else {
                *self.state.write() = Some(ExecutionState::Offline);
            }
        }
    }

    /// Sets the `ExecutionState` to online.
    pub fn online(&self) {
        *self.state.write() = Some(ExecutionState::Online);
    }
}

/// The primary object for handling and driving all the current syncing logic. It maintains the
/// current state of the syncing process, the number of useful peers, downloaded blocks and
/// controls the logic behind both the long-range (batch) sync and the on-going potential parent
/// look-up of blocks.
pub struct SyncManager<T: BeaconChainTypes> {
    /// A reference to the underlying beacon chain.
    chain: Arc<BeaconChain<T>>,

    /// A reference to the network globals and peer-db.
    network_globals: Arc<NetworkGlobals<T::EthSpec>>,

    /// A receiving channel sent by the message processor thread.
    input_channel: mpsc::UnboundedReceiver<SyncMessage<T::EthSpec>>,

    /// A network context to contact the network service.
    network: SyncNetworkContext<T::EthSpec>,

    /// The object handling long-range batch load-balanced syncing.
    range_sync: RangeSync<T>,

    /// Backfill syncing.
    backfill_sync: BackFillSync<T>,

    block_lookups: BlockLookups<T>,

    /// An optional notifier that receives a notification from the execution layer.
    /// The notifier gets initialized when `RangeSync` or BlockLookup stalls because
    ///  of execution layer going offline. The execution layer signals that it is back online
    /// by sending over the channel.
    execution_notifier: Pin<Box<OptionFuture<tokio::sync::oneshot::Receiver<()>>>>,

    /// The state of the execution layer.
    /// `None` implies that the chain is not merge enabled.
    execution_status_handler: ExecutionStatusHandler,

    /// Listens to changes in the execution status from `RangeSync` and
    /// `BlockLookups` and sets up the execution notifier if required.
    execution_status_listener: tokio::sync::mpsc::Receiver<()>,

    /// The logger for the import manager.
    log: Logger,
}

/// Spawns a new `SyncManager` thread which has a weak reference to underlying beacon
/// chain. This allows the chain to be
/// dropped during the syncing process which will gracefully end the `SyncManager`.
pub fn spawn<T: BeaconChainTypes>(
    executor: task_executor::TaskExecutor,
    beacon_chain: Arc<BeaconChain<T>>,
    network_globals: Arc<NetworkGlobals<T::EthSpec>>,
    network_send: mpsc::UnboundedSender<NetworkMessage<T::EthSpec>>,
    beacon_processor_send: mpsc::Sender<BeaconWorkEvent<T>>,
    log: slog::Logger,
) -> mpsc::UnboundedSender<SyncMessage<T::EthSpec>> {
    assert!(
        MAX_REQUEST_BLOCKS >= T::EthSpec::slots_per_epoch() * EPOCHS_PER_BATCH,
        "Max blocks that can be requested in a single batch greater than max allowed blocks in a single request"
    );
    // generate the message channel
    let (sync_send, sync_recv) = mpsc::unbounded_channel::<SyncMessage<T::EthSpec>>();

    let execution_state = if beacon_chain.execution_layer.is_some() {
        // We optimistically assume that the execution layer is online if it is enabled
        Some(ExecutionState::Online)
    } else {
        None
    };

    let (execution_status_handler, rx) = ExecutionStatusHandler::new(execution_state, log.clone());

    // create an instance of the SyncManager
    let mut sync_manager = SyncManager {
        chain: beacon_chain.clone(),
        network_globals: network_globals.clone(),
        input_channel: sync_recv,
        network: SyncNetworkContext::new(network_send, network_globals.clone(), log.clone()),
        range_sync: RangeSync::new(
            beacon_chain.clone(),
            execution_status_handler.clone(),
            beacon_processor_send.clone(),
            log.clone(),
        ),
        backfill_sync: BackFillSync::new(
            beacon_chain,
            network_globals,
            beacon_processor_send.clone(),
            log.clone(),
        ),
        block_lookups: BlockLookups::new(
            beacon_processor_send,
            execution_status_handler.clone(),
            log.clone(),
        ),
        execution_notifier: Box::pin(None.into()),
        execution_status_handler,
        execution_status_listener: rx,
        log: log.clone(),
    };

    // spawn the sync manager thread
    debug!(log, "Sync Manager started");
    executor.spawn(async move { Box::pin(sync_manager.main()).await }, "sync");
    sync_send
}

impl<T: BeaconChainTypes> SyncManager<T> {
    /* Input Handling Functions */

    /// A peer has connected which has blocks that are unknown to us.
    ///
    /// This function handles the logic associated with the connection of a new peer. If the peer
    /// is sufficiently ahead of our current head, a range-sync (batch) sync is started and
    /// batches of blocks are queued to download from the peer. Batched blocks begin at our latest
    /// finalized head.
    ///
    /// If the peer is within the `SLOT_IMPORT_TOLERANCE`, then it's head is sufficiently close to
    /// ours that we consider it fully sync'd with respect to our current chain.
    fn add_peer(&mut self, peer_id: PeerId, remote: SyncInfo) {
        // ensure the beacon chain still exists
        let local = match self.chain.status_message() {
            Ok(status) => SyncInfo {
                head_slot: status.head_slot,
                head_root: status.head_root,
                finalized_epoch: status.finalized_epoch,
                finalized_root: status.finalized_root,
            },
            Err(e) => {
                return error!(self.log, "Failed to get peer sync info";
                    "msg" => "likely due to head lock contention", "err" => ?e)
            }
        };

        let sync_type = remote_sync_type(&local, &remote, &self.chain);

        // update the state of the peer.
        let should_add = self.update_peer_sync_state(&peer_id, &local, &remote, &sync_type);

        if matches!(sync_type, PeerSyncType::Advanced) && should_add {
            self.range_sync
                .add_peer(&mut self.network, local, peer_id, remote);
        }

        self.update_sync_state();
    }

    /// Handles RPC errors related to requests that were emitted from the sync manager.
    fn inject_error(&mut self, peer_id: PeerId, request_id: RequestId) {
        trace!(self.log, "Sync manager received a failed RPC");
        match request_id {
            RequestId::SingleBlock { id } => {
                self.block_lookups
                    .single_block_lookup_failed(id, &mut self.network);
            }
            RequestId::ParentLookup { id } => {
                self.block_lookups
                    .parent_lookup_failed(id, peer_id, &mut self.network);
            }
            RequestId::BackFillSync { id } => {
                if let Some(batch_id) = self.network.backfill_sync_response(id, true) {
                    match self
                        .backfill_sync
                        .inject_error(&mut self.network, batch_id, &peer_id, id)
                    {
                        Ok(_) => {}
                        Err(_) => self.update_sync_state(),
                    }
                }
            }
            RequestId::RangeSync { id } => {
                if let Some((chain_id, batch_id)) = self.network.range_sync_response(id, true) {
                    self.range_sync.inject_error(
                        &mut self.network,
                        peer_id,
                        batch_id,
                        chain_id,
                        id,
                    );
                    self.update_sync_state();
                }
            }
        }
    }

    fn peer_disconnect(&mut self, peer_id: &PeerId) {
        self.range_sync.peer_disconnect(&mut self.network, peer_id);
        self.block_lookups
            .peer_disconnected(peer_id, &mut self.network);
        // Regardless of the outcome, we update the sync status.
        let _ = self
            .backfill_sync
            .peer_disconnected(peer_id, &mut self.network);
        self.update_sync_state()
    }

    /// Updates the syncing state of a peer.
    /// Return whether the peer should be used for range syncing or not, according to its
    /// connection status.
    fn update_peer_sync_state(
        &mut self,
        peer_id: &PeerId,
        local_sync_info: &SyncInfo,
        remote_sync_info: &SyncInfo,
        sync_type: &PeerSyncType,
    ) -> bool {
        // NOTE: here we are gracefully handling two race conditions: Receiving the status message
        // of a peer that is 1) disconnected 2) not in the PeerDB.

        let new_state = sync_type.as_sync_status(remote_sync_info);
        let rpr = new_state.as_str();
        // Drop the write lock
        let update_sync_status = self
            .network_globals
            .peers
            .write()
            .update_sync_status(peer_id, new_state.clone());
        if let Some(was_updated) = update_sync_status {
            let is_connected = self.network_globals.peers.read().is_connected(peer_id);
            if was_updated {
                debug!(self.log, "Peer transitioned sync state"; "peer_id" => %peer_id, "new_state" => rpr,
                    "our_head_slot" => local_sync_info.head_slot, "out_finalized_epoch" => local_sync_info.finalized_epoch,
                    "their_head_slot" => remote_sync_info.head_slot, "their_finalized_epoch" => remote_sync_info.finalized_epoch,
                    "is_connected" => is_connected);

                // A peer has transitioned its sync state. If the new state is "synced" we
                // inform the backfill sync that a new synced peer has joined us.
                if new_state.is_synced() {
                    self.backfill_sync.fully_synced_peer_joined();
                }
            }
            is_connected
        } else {
            error!(self.log, "Status'd peer is unknown"; "peer_id" => %peer_id);
            false
        }
    }

    /// Updates the global sync state, optionally instigating or pausing a backfill sync as well as
    /// logging any changes.
    ///
    /// The logic for which sync should be running is as follows:
    /// - If there is a range-sync running (or required) pause any backfill and let range-sync
    /// complete.
    /// - If there is no current range sync, check for any requirement to backfill and either
    /// start/resume a backfill sync if required. The global state will be BackFillSync if a
    /// backfill sync is running.
    /// - If there is no range sync and no required backfill and we have synced up to the currently
    /// known peers, we consider ourselves synced.
    fn update_sync_state(&mut self) {
        let new_state: SyncState = match self.range_sync.state() {
            Err(e) => {
                crit!(self.log, "Error getting range sync state"; "error" => %e);
                return;
            }
            Ok(state) => match state {
                ChainState::Idle => {
                    // No range sync, so we decide if we are stalled or synced.
                    // For this we check if there is at least one advanced peer. An advanced peer
                    // with Idle range is possible since a peer's status is updated periodically.
                    // If we synced a peer between status messages, most likely the peer has
                    // advanced and will produce a head chain on re-status. Otherwise it will shift
                    // to being synced
                    let mut sync_state = {
                        let head = self.chain.best_slot().unwrap_or_else(|_| Slot::new(0));
                        let current_slot = self.chain.slot().unwrap_or_else(|_| Slot::new(0));

                        let peers = self.network_globals.peers.read();
                        if current_slot >= head
                            && current_slot.sub(head) <= (SLOT_IMPORT_TOLERANCE as u64)
                            && head > 0
                        {
                            SyncState::Synced
                        } else if peers.advanced_peers().next().is_some() {
                            SyncState::SyncTransition
                        } else if peers.synced_peers().next().is_none() {
                            SyncState::Stalled
                        // Another condition that if execution_layer.is_syncing == true, then sync
                        // is stalled
                        } else {
                            // There are no peers that require syncing and we have at least one synced
                            // peer
                            SyncState::Synced
                        }
                    };

                    // If we would otherwise be synced, first check if we need to perform or
                    // complete a backfill sync.
                    if matches!(sync_state, SyncState::Synced) {
                        // Determine if we need to start/resume/restart a backfill sync.
                        match self.backfill_sync.start(&mut self.network) {
                            Ok(SyncStart::Syncing {
                                completed,
                                remaining,
                            }) => {
                                sync_state = SyncState::BackFillSyncing {
                                    completed,
                                    remaining,
                                };
                            }
                            Ok(SyncStart::NotSyncing) => {} // Ignore updating the state if the backfill sync state didn't start.
                            Err(e) => {
                                error!(self.log, "Backfill sync failed to start"; "error" => ?e);
                            }
                        }
                    }

                    // Return the sync state if backfilling is not required.
                    sync_state
                }
                ChainState::Range {
                    range_type: RangeSyncType::Finalized,
                    from: start_slot,
                    to: target_slot,
                } => {
                    // If there is a backfill sync in progress pause it.
                    self.backfill_sync.pause();

                    SyncState::SyncingFinalized {
                        start_slot,
                        target_slot,
                    }
                }
                ChainState::Range {
                    range_type: RangeSyncType::Head,
                    from: start_slot,
                    to: target_slot,
                } => {
                    // If there is a backfill sync in progress pause it.
                    self.backfill_sync.pause();

                    SyncState::SyncingHead {
                        start_slot,
                        target_slot,
                    }
                }
            },
        };

        let old_state = self.network_globals.set_sync_state(new_state);
        let new_state = self.network_globals.sync_state.read();
        if !new_state.eq(&old_state) {
            info!(self.log, "Sync state updated"; "old_state" => %old_state, "new_state" => %new_state);
            // If we have become synced - Subscribe to all the core subnet topics
            // We don't need to subscribe if the old state is a state that would have already
            // invoked this call.
            if new_state.is_synced()
                && !matches!(
                    old_state,
                    SyncState::Synced { .. } | SyncState::BackFillSyncing { .. }
                )
            {
                self.network.subscribe_core_topics();
            }
        }
    }

    /// The main driving future for the sync manager.
    async fn main(&mut self) {
        loop {
            tokio::select! {
                Some(_) = self.execution_status_listener.recv() => {
                    debug!(self.log, "Execution status changed to offline";);
                    if let Some(execution_layer) = self.chain.execution_layer.as_ref() {
                        let receiver = execution_layer.is_online_notifier().await;
                        if let Some(recv) = receiver {
                            self.execution_notifier = Box::pin(Some(recv.0).into());
                        } else {
                            crit!(
                                self.log,
                                "Requesting for duplicate execution layer notifier in range sync";
                            );
                            return;
                        }
                    }
                }
                Some(_) = &mut self.execution_notifier => {
                    debug!(self.log, "Execution layer back online"; "action" => "resuming sync");
                    self.execution_status_handler.online();
                    // Reset the notifier
                    self.execution_notifier = Box::pin(None.into());
                    self.update_sync_state();
                }
                // process any inbound messages
                Some(sync_message) = self.input_channel.recv() => {
                    match sync_message {
                        SyncMessage::AddPeer(peer_id, info) => {
                            self.add_peer(peer_id, info);
                        }
                        SyncMessage::RpcBlock {
                            request_id,
                            peer_id,
                            beacon_block,
                            seen_timestamp,
                        } => {
                            self.rpc_block_received(request_id, peer_id, beacon_block, seen_timestamp);
                        }
                        SyncMessage::UnknownBlock(peer_id, block) => {
                            // If we are not synced or within SLOT_IMPORT_TOLERANCE of the block, ignore
                            if !self.network_globals.sync_state.read().is_synced() {
                                let head_slot = self
                                    .chain
                                    .head_info()
                                    .map(|info| info.slot)
                                    .unwrap_or_else(|_| Slot::from(0u64));
                                let unknown_block_slot = block.slot();

                                // if the block is far in the future, ignore it. If its within the slot tolerance of
                                // our current head, regardless of the syncing state, fetch it.
                                if (head_slot >= unknown_block_slot
                                    && head_slot.sub(unknown_block_slot).as_usize()
                                        > SLOT_IMPORT_TOLERANCE)
                                    || (head_slot < unknown_block_slot
                                        && unknown_block_slot.sub(head_slot).as_usize()
                                            > SLOT_IMPORT_TOLERANCE)
                                {
                                    continue;
                                }
                            }
                            if self.network_globals.peers.read().is_connected(&peer_id) {
                                self.block_lookups
                                    .search_parent(block, peer_id, &mut self.network);
                            }
                        }
                        SyncMessage::UnknownBlockHash(peer_id, block_hash) => {
                            // If we are not synced, ignore this block.
                            if self.network_globals.sync_state.read().is_synced()
                                && self.network_globals.peers.read().is_connected(&peer_id)
                            {
                                self.block_lookups
                                    .search_block(block_hash, peer_id, &mut self.network);
                            }
                        }
                        SyncMessage::Disconnect(peer_id) => {
                            self.peer_disconnect(&peer_id);
                        }
                        SyncMessage::RpcError {
                            peer_id,
                            request_id,
                        } => self.inject_error(peer_id, request_id),
                        SyncMessage::BlockProcessed {
                            process_type,
                            result,
                        } => {
                            let _block_lookup_status = match process_type {
                            BlockProcessType::SingleBlock { id } => self
                                .block_lookups
                                .single_block_processed(id, result, &mut self.network),
                            BlockProcessType::ParentLookup { chain_hash } => self
                                .block_lookups
                                .parent_block_processed(chain_hash, result, &mut self.network),
                            };
                        }
                        SyncMessage::BatchProcessed { sync_type, result } => match sync_type {
                            ChainSegmentProcessId::RangeBatchId(chain_id, epoch) => {
                                self.range_sync.handle_block_process_result(
                                    &mut self.network,
                                    chain_id,
                                    epoch,
                                    result,
                                );
                                self.update_sync_state();
                            }
                            ChainSegmentProcessId::BackSyncBatchId(epoch) => {
                                match self.backfill_sync.on_batch_process_result(
                                    &mut self.network,
                                    epoch,
                                    &result,
                                ) {
                                    Ok(ProcessResult::Successful) => {}
                                    Ok(ProcessResult::SyncCompleted) => self.update_sync_state(),
                                    Err(error) => {
                                        error!(self.log, "Backfill sync failed"; "error" => ?error);
                                        // Update the global status
                                        self.update_sync_state();
                                    }
                                }
                            }
                            ChainSegmentProcessId::ParentLookup(chain_hash) => {
                                self
                                    .block_lookups
                                    .parent_chain_processed(chain_hash, result, &mut self.network);
                            }


                        },
                    }
                }
            }
        }
    }

    fn rpc_block_received(
        &mut self,
        request_id: RequestId,
        peer_id: PeerId,
        beacon_block: Option<Box<SignedBeaconBlock<T::EthSpec>>>,
        seen_timestamp: Duration,
    ) {
        match request_id {
            RequestId::SingleBlock { id } => self.block_lookups.single_block_lookup_response(
                id,
                peer_id,
                beacon_block,
                seen_timestamp,
                &mut self.network,
            ),
            RequestId::ParentLookup { id } => self.block_lookups.parent_lookup_response(
                id,
                peer_id,
                beacon_block,
                seen_timestamp,
                &mut self.network,
            ),
            RequestId::BackFillSync { id } => {
                if let Some(batch_id) = self
                    .network
                    .backfill_sync_response(id, beacon_block.is_none())
                {
                    match self.backfill_sync.on_block_response(
                        &mut self.network,
                        batch_id,
                        &peer_id,
                        id,
                        beacon_block.map(|b| *b),
                    ) {
                        Ok(ProcessResult::SyncCompleted) => self.update_sync_state(),
                        Ok(ProcessResult::Successful) => {}
                        Err(_error) => {
                            // The backfill sync has failed, errors are reported
                            // within.
                            self.update_sync_state();
                        }
                    }
                }
            }
            RequestId::RangeSync { id } => {
                if let Some((chain_id, batch_id)) =
                    self.network.range_sync_response(id, beacon_block.is_none())
                {
                    self.range_sync.blocks_by_range_response(
                        &mut self.network,
                        peer_id,
                        chain_id,
                        batch_id,
                        id,
                        beacon_block.map(|b| *b),
                    );
                    self.update_sync_state();
                }
            }
        }
    }
}
