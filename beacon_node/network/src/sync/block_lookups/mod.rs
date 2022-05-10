use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::time::Duration;

use beacon_chain::{BeaconChainTypes, BlockError, ExecutionPayloadError};
use fnv::FnvHashMap;
use lighthouse_network::{PeerAction, PeerId};
use lru_cache::LRUCache;
use slog::{crit, debug, error, trace, warn, Logger};
use smallvec::SmallVec;
use store::{Hash256, SignedBeaconBlock};
use tokio::sync::mpsc;

use crate::beacon_processor::{ChainSegmentProcessId, FailureMode, WorkEvent};
use crate::metrics;

use self::{
    parent_lookup::{ParentLookup, VerifyError},
    single_block_lookup::SingleBlockRequest,
};

use super::BatchProcessResult;
use super::{
    manager::{BlockProcessType, Id},
    network_context::SyncNetworkContext,
};

use super::super::router::timestamp_now;

mod parent_lookup;
mod single_block_lookup;
#[cfg(test)]
mod tests;

const FAILED_CHAINS_CACHE_SIZE: usize = 500;
const SINGLE_BLOCK_LOOKUP_MAX_ATTEMPTS: u8 = 3;

pub(crate) struct BlockLookups<T: BeaconChainTypes> {
    /// A collection of parent block lookups.
    parent_queue: SmallVec<[ParentLookup<T::EthSpec>; 3]>,

    /// A cache of failed chain lookups to prevent duplicate searches.
    failed_chains: LRUCache<Hash256>,

    /// A collection of block hashes being searched for and a flag indicating if a result has been
    /// received or not.
    ///
    /// The flag allows us to determine if the peer returned data or sent us nothing.
    single_block_lookups: FnvHashMap<Id, SingleBlockRequest<SINGLE_BLOCK_LOOKUP_MAX_ATTEMPTS>>,

    /// A multi-threaded, non-blocking processor for applying messages to the beacon chain.
    beacon_processor_send: mpsc::Sender<WorkEvent<T>>,

    /// A hashmap of blocks that are waiting on the execution layer
    ///  to come online for verification.
    waiting_execution: HashMap<Hash256, Box<SignedBeaconBlock<T::EthSpec>>>,

    /// The logger for the import manager.
    log: Logger,
}

impl<T: BeaconChainTypes> BlockLookups<T> {
    pub fn new(beacon_processor_send: mpsc::Sender<WorkEvent<T>>, log: Logger) -> Self {
        Self {
            parent_queue: Default::default(),
            failed_chains: LRUCache::new(FAILED_CHAINS_CACHE_SIZE),
            single_block_lookups: Default::default(),
            beacon_processor_send,
            waiting_execution: Default::default(),
            log,
        }
    }

    /* Lookup requests */

    pub fn search_block(
        &mut self,
        hash: Hash256,
        peer_id: PeerId,
        cx: &mut SyncNetworkContext<T::EthSpec>,
    ) {
        // Do not re-request a block that is already being requested
        if self
            .single_block_lookups
            .values_mut()
            .any(|single_block_request| single_block_request.add_peer(&hash, &peer_id))
        {
            return;
        }

        debug!(
            self.log,
            "Searching for block";
            "peer_id" => %peer_id,
            "block" => %hash
        );

        let mut single_block_request = SingleBlockRequest::new(hash, peer_id);

        // If the block exists in the `waiting_execution` cache, we directly call the
        // `single_block_lookup_response` function with the block to avoid re-requesting
        // the block over the network
        if let Some(block) = self.waiting_execution.remove(&hash) {
            debug!(
                self.log,
                "Single block response already exists in cache, making a dummy request";
                "root" => %hash
            );
            let (peer_id, request) = single_block_request.request_block().unwrap();
            if let Ok(request_id) = cx.single_block_lookup_request(peer_id, request, false) {
                self.single_block_lookups
                    .insert(request_id, single_block_request);

                self.single_block_lookup_response(
                    request_id,
                    peer_id,
                    Some(block),
                    timestamp_now(),
                    cx,
                );
            }
        }
        // Block does not exist in the cache, request it over the network
        else {
            trace!(
                self.log,
                "Making single block lookup request";
                "root" => %hash
            );
            let (peer_id, request) = single_block_request.request_block().unwrap();
            if let Ok(request_id) = cx.single_block_lookup_request(peer_id, request, true) {
                self.single_block_lookups
                    .insert(request_id, single_block_request);
            }
        }
        metrics::set_gauge(
            &metrics::SYNC_SINGLE_BLOCK_LOOKUPS,
            self.single_block_lookups.len() as i64,
        );
        metrics::set_gauge(
            &metrics::SYNC_WAITING_ON_EXECUTION,
            self.waiting_execution.len() as i64,
        );
    }

    pub fn search_parent(
        &mut self,
        block: Box<SignedBeaconBlock<T::EthSpec>>,
        peer_id: PeerId,
        cx: &mut SyncNetworkContext<T::EthSpec>,
    ) {
        let block_root = block.canonical_root();
        let parent_root = block.parent_root();

        // If this block or it's parent is part of a known failed chain, ignore it.
        if self.failed_chains.contains(&parent_root) || self.failed_chains.contains(&block_root) {
            debug!(self.log, "Block is from a past failed chain. Dropping";
                "block_root" => ?block_root, "block_slot" => block.slot());
            return;
        }

        // Make sure this block is not already downloaded, and that neither it or its parent is
        // being searched for.
        if self.parent_queue.iter_mut().any(|parent_req| {
            parent_req.contains_block(&block)
                || parent_req.add_peer(&block_root, &peer_id)
                || parent_req.add_peer(&parent_root, &peer_id)
        }) {
            // we are already searching for this block, ignore it
            return;
        }

        debug!(
            self.log,
            "Searching for parent";
            "block_root" => %block_root,
            "parent_root" => %parent_root,
        );

        // If the incoming block is the `chain_hash` of an existing
        // parent chain, then we simply insert the new block to the
        // tip of the chain and send the chain segment for processing.
        //
        // B1 <- B2 : Existing parent chain with chain_hash set to B2
        //
        // New block B3 with parent as B2.
        //
        // B1 <- B2 <- B3 : New parent chain with chain_hash set to B3.
        if let Some(pos) = self
            .parent_queue
            .iter()
            .position(|request| request.chain_hash() == parent_root)
        {
            let mut parent_lookup = self.parent_queue.remove(pos);
            parent_lookup.insert_block(*block, peer_id);

            debug!(
                self.log,
                "Inserting block into existing parent chain";
                "new_chain_hash" => %parent_lookup.chain_hash(),
                "old_chain_hash" => %parent_root,
            );
            let chain_hash = parent_lookup.chain_hash();
            let blocks = parent_lookup.chain_blocks_clone();
            let process_id = ChainSegmentProcessId::ParentLookup(chain_hash);
            match self
                .beacon_processor_send
                .try_send(WorkEvent::chain_segment(process_id, blocks))
            {
                Ok(_) => {
                    self.parent_queue.push(parent_lookup);
                }
                Err(e) => {
                    error!(
                        self.log,
                        "Failed to send chain segment to processor";
                        "error" => ?e
                    );
                }
            }
        } else {
            let parent_lookup = ParentLookup::new(*block, peer_id);
            self.request_parent(parent_lookup, cx);
        }
    }

    /* Lookup responses */

    pub fn single_block_lookup_response(
        &mut self,
        id: Id,
        peer_id: PeerId,
        block: Option<Box<SignedBeaconBlock<T::EthSpec>>>,
        seen_timestamp: Duration,
        cx: &mut SyncNetworkContext<T::EthSpec>,
    ) {
        let mut request = match self.single_block_lookups.entry(id) {
            Entry::Occupied(req) => req,
            Entry::Vacant(_) => {
                if block.is_some() {
                    crit!(
                        self.log,
                        "Block returned for single block lookup not present"
                    );
                    #[cfg(debug_assertions)]
                    panic!("block returned for single block lookup not present");
                }
                return;
            }
        };

        match request.get_mut().verify_block(block) {
            Ok(Some(block)) => {
                // If this block's parent already exists in a parent_lookup, add the block to
                // the waiting_execution list.
                if self
                    .parent_queue
                    .iter()
                    .any(|request| request.chain_hash() == block.parent_root())
                {
                    let block_hash = block.canonical_root();
                    debug!(
                        self.log,
                        "Single block request is waiting on execution";
                        "block_hash" => %block_hash,
                        "parent_hash" => %block.parent_root(),
                    );
                    self.waiting_execution.insert(block_hash, block.clone());
                }
                // This is the correct block, send it for processing
                if self
                    .send_block_for_processing(
                        block,
                        seen_timestamp,
                        BlockProcessType::SingleBlock { id },
                    )
                    .is_err()
                {
                    // Remove to avoid inconsistencies
                    self.single_block_lookups.remove(&id);
                }
            }
            Ok(None) => {
                // request finished correctly, it will be removed after the block is processed.
            }
            Err(error) => {
                let msg: &str = error.into();
                cx.report_peer(peer_id, PeerAction::LowToleranceError, msg);
                // Remove the request, if it can be retried it will be added with a new id.
                let mut req = request.remove();

                debug!(self.log, "Single block lookup failed";
                        "peer_id" => %peer_id, "error" => msg, "block_root" => %req.hash);
                // try the request again if possible
                if let Ok((peer_id, request)) = req.request_block() {
                    if let Ok(id) = cx.single_block_lookup_request(peer_id, request, true) {
                        self.single_block_lookups.insert(id, req);
                    }
                }
            }
        }

        metrics::set_gauge(
            &metrics::SYNC_SINGLE_BLOCK_LOOKUPS,
            self.single_block_lookups.len() as i64,
        );
        metrics::set_gauge(
            &metrics::SYNC_WAITING_ON_EXECUTION,
            self.waiting_execution.len() as i64,
        );
    }

    pub fn parent_lookup_response(
        &mut self,
        id: Id,
        peer_id: PeerId,
        block: Option<Box<SignedBeaconBlock<T::EthSpec>>>,
        seen_timestamp: Duration,
        cx: &mut SyncNetworkContext<T::EthSpec>,
    ) {
        let mut parent_lookup = if let Some(pos) = self
            .parent_queue
            .iter()
            .position(|request| request.pending_response(id))
        {
            self.parent_queue.remove(pos)
        } else {
            if block.is_some() {
                debug!(self.log, "Response for a parent lookup request that was not found"; "peer_id" => %peer_id);
            }
            return;
        };

        match parent_lookup.verify_block(block, &self.failed_chains) {
            Ok(Some(block)) => {
                // Block is correct, send to the beacon processor.
                let chain_hash = parent_lookup.chain_hash();
                if self
                    .send_block_for_processing(
                        block,
                        seen_timestamp,
                        BlockProcessType::ParentLookup { chain_hash },
                    )
                    .is_ok()
                {
                    self.parent_queue.push(parent_lookup)
                }
            }
            Ok(None) => {
                // Request finished successfully, nothing else to do. It will be removed after the
                // processing result arrives.
                self.parent_queue.push(parent_lookup);
            }
            Err(e) => match e {
                VerifyError::RootMismatch
                | VerifyError::NoBlockReturned
                | VerifyError::ExtraBlocksReturned => {
                    let e = e.into();
                    warn!(self.log, "Peer sent invalid response to parent request.";
                        "peer_id" => %peer_id, "reason" => %e);

                    // We do not tolerate these kinds of errors. We will accept a few but these are signs
                    // of a faulty peer.
                    cx.report_peer(peer_id, PeerAction::LowToleranceError, e);

                    // We try again if possible.
                    self.request_parent(parent_lookup, cx);
                }
                VerifyError::PreviousFailure { parent_root } => {
                    self.failed_chains.insert(parent_lookup.chain_hash());
                    debug!(
                        self.log,
                        "Parent chain ignored due to past failure";
                        "block" => %parent_root,
                    );
                    // Add the root block to failed chains
                    self.failed_chains.insert(parent_lookup.chain_hash());

                    cx.report_peer(
                        peer_id,
                        PeerAction::MidToleranceError,
                        "bbroot_failed_chains",
                    );
                }
            },
        };

        metrics::set_gauge(
            &metrics::SYNC_PARENT_BLOCK_LOOKUPS,
            self.parent_queue.len() as i64,
        );
    }

    /* Error responses */

    #[allow(clippy::needless_collect)] // false positive
    pub fn peer_disconnected(&mut self, peer_id: &PeerId, cx: &mut SyncNetworkContext<T::EthSpec>) {
        /* Check disconnection for single block lookups */
        // better written after https://github.com/rust-lang/rust/issues/59618
        let remove_retry_ids: Vec<Id> = self
            .single_block_lookups
            .iter_mut()
            .filter_map(|(id, req)| {
                if req.check_peer_disconnected(peer_id).is_err() {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();

        for mut req in remove_retry_ids
            .into_iter()
            .map(|id| self.single_block_lookups.remove(&id).unwrap())
            .collect::<Vec<_>>()
        {
            // retry the request
            match req.request_block() {
                Ok((peer_id, block_request)) => {
                    if let Ok(request_id) =
                        cx.single_block_lookup_request(peer_id, block_request, true)
                    {
                        self.single_block_lookups.insert(request_id, req);
                    }
                }
                Err(e) => {
                    trace!(
                        self.log,
                        "Single block request failed on peer disconnection";
                        "block_root" => %req.hash,
                        "peer_id" => %peer_id,
                        "reason" => <&str>::from(e),
                    );
                }
            }
        }

        /* Check disconnection for parent lookups */
        while let Some(pos) = self
            .parent_queue
            .iter_mut()
            .position(|req| req.check_peer_disconnected(peer_id).is_err())
        {
            let parent_lookup = self.parent_queue.remove(pos);
            debug!(self.log, "Parent lookup's peer disconnected"; &parent_lookup);
            self.request_parent(parent_lookup, cx);
        }
    }

    pub fn parent_lookup_failed(
        &mut self,
        id: Id,
        peer_id: PeerId,
        cx: &mut SyncNetworkContext<T::EthSpec>,
    ) {
        if let Some(pos) = self
            .parent_queue
            .iter()
            .position(|request| request.pending_response(id))
        {
            let mut parent_lookup = self.parent_queue.remove(pos);
            parent_lookup.download_failed();
            debug!(self.log, "Parent lookup request failed"; &parent_lookup);
            self.request_parent(parent_lookup, cx);
        } else {
            return debug!(self.log, "RPC failure for a parent lookup request that was not found"; "peer_id" => %peer_id);
        };
        metrics::set_gauge(
            &metrics::SYNC_PARENT_BLOCK_LOOKUPS,
            self.parent_queue.len() as i64,
        );
    }

    pub fn single_block_lookup_failed(&mut self, id: Id, cx: &mut SyncNetworkContext<T::EthSpec>) {
        if let Some(mut request) = self.single_block_lookups.remove(&id) {
            request.register_failure();
            trace!(self.log, "Single block lookup failed"; "block" => %request.hash);
            if let Ok((peer_id, block_request)) = request.request_block() {
                if let Ok(request_id) = cx.single_block_lookup_request(peer_id, block_request, true)
                {
                    self.single_block_lookups.insert(request_id, request);
                }
            }
        }

        metrics::set_gauge(
            &metrics::SYNC_SINGLE_BLOCK_LOOKUPS,
            self.single_block_lookups.len() as i64,
        );
    }

    /* Processing responses */

    pub fn single_block_processed(
        &mut self,
        id: Id,
        result: Result<(), BlockError<T::EthSpec>>,
        cx: &mut SyncNetworkContext<T::EthSpec>,
    ) {
        let mut req = match self.single_block_lookups.remove(&id) {
            Some(req) => req,
            None => {
                #[cfg(debug_assertions)]
                panic!("block processed for single block lookup not present");
                #[cfg(not(debug_assertions))]
                return crit!(
                    self.log,
                    "Block processed for single block lookup not present"
                );
            }
        };

        let root = req.hash;
        let peer_id = match req.processing_peer() {
            Ok(peer) => peer,
            Err(_) => return,
        };

        if let Err(e) = &result {
            trace!(self.log, "Single block processing failed"; "block" => %root, "error" => %e);
        } else {
            trace!(self.log, "Single block processing succeeded"; "block" => %root);
        }

        if let Err(e) = result {
            match e {
                BlockError::BlockIsAlreadyKnown => {
                    // No error here
                }
                BlockError::BeaconChainError(e) => {
                    // Internal error
                    error!(self.log, "Beacon chain error processing single block"; "block_root" => %root, "error" => ?e);
                }
                BlockError::ParentUnknown(block) => {
                    self.search_parent(block, peer_id, cx);
                }
                BlockError::ExecutionPayloadError(e) => match e {
                    ExecutionPayloadError::NoExecutionConnection { block }
                    | ExecutionPayloadError::RequestFailed { err: _, block } => {
                        // These errors indicate an issue with the EL and not the block.
                        warn!(self.log,
                            "Single block lookup failed. Execution layer is stalled";
                            "root" => %root,
                        );

                        // Add this to the existing parent request and send the chain segment
                        // for processing.
                        if let Some(_) = self.waiting_execution.remove(&root) {
                            if let Some(pos) = self
                                .parent_queue
                                .iter()
                                .position(|req| req.chain_hash() == block.parent_root())
                            {
                                debug!(
                                    self.log,
                                    "Single block lookup processed, parent exists in parent queue";
                                    "block_root" => %block.canonical_root(),
                                    "parent_root" => %block.parent_root(),
                                );
                                let mut parent_lookup = self.parent_queue.remove(pos);
                                parent_lookup.insert_block(*block, peer_id);

                                let chain_hash = parent_lookup.chain_hash();
                                let blocks = parent_lookup.chain_blocks_clone();
                                let process_id = ChainSegmentProcessId::ParentLookup(chain_hash);
                                match self
                                    .beacon_processor_send
                                    .try_send(WorkEvent::chain_segment(process_id, blocks))
                                {
                                    Ok(_) => {
                                        self.parent_queue.push(parent_lookup);
                                    }
                                    Err(e) => {
                                        error!(
                                            self.log,
                                            "Failed to send chain segment to processor";
                                            "error" => ?e
                                        );
                                    }
                                }
                            }
                        }
                    }
                    err => {
                        debug!(self.log,
                            "Single block lookup failed. Invalid execution payload";
                            "root" => %root,
                            "peer_id" => %peer_id,
                            "error" => ?err
                        );
                        cx.report_peer(
                            peer_id,
                            PeerAction::LowToleranceError,
                            "single_block_lookup_failed_invalid_payload",
                        );
                        req.register_failure();
                    }
                },
                other => {
                    warn!(self.log, "Peer sent invalid block in single block lookup"; "root" => %root, "error" => ?other, "peer_id" => %peer_id);
                    cx.report_peer(
                        peer_id,
                        PeerAction::MidToleranceError,
                        "single_block_failure",
                    );

                    // Try it again if possible.
                    req.register_failure();
                    if let Ok((peer_id, request)) = req.request_block() {
                        if let Ok(request_id) =
                            cx.single_block_lookup_request(peer_id, request, true)
                        {
                            // insert with the new id
                            self.single_block_lookups.insert(request_id, req);
                        }
                    }
                }
            }
        }

        metrics::set_gauge(
            &metrics::SYNC_SINGLE_BLOCK_LOOKUPS,
            self.single_block_lookups.len() as i64,
        );

        metrics::set_gauge(
            &metrics::SYNC_WAITING_ON_EXECUTION,
            self.waiting_execution.len() as i64,
        );
    }

    pub fn parent_block_processed(
        &mut self,
        chain_hash: Hash256,
        result: Result<(), BlockError<T::EthSpec>>,
        cx: &mut SyncNetworkContext<T::EthSpec>,
    ) {
        let (mut parent_lookup, peer_id) = if let Some((pos, peer)) = self
            .parent_queue
            .iter()
            .enumerate()
            .find_map(|(pos, request)| {
                request
                    .get_processing_peer(chain_hash)
                    .map(|peer| (pos, peer))
            }) {
            (self.parent_queue.remove(pos), peer)
        } else {
            #[cfg(debug_assertions)]
            panic!(
                "Process response for a parent lookup request that was not found. Chain_hash: {}",
                chain_hash
            );
            #[cfg(not(debug_assertions))]
            return crit!(self.log, "Process response for a parent lookup request that was not found"; "chain_hash" => %chain_hash);
        };

        if let Err(e) = &result {
            trace!(self.log, "Parent block processing failed"; &parent_lookup, "error" => %e);
        } else {
            trace!(self.log, "Parent block processing succeeded"; &parent_lookup);
        }

        match result {
            Err(BlockError::ParentUnknown(block)) => {
                // need to keep looking for parents
                // add the block back to the queue and continue the search
                debug!(self.log, "Making recursive parent request"; "block_hash" => %block.canonical_root());
                parent_lookup.add_block(*block);
                self.request_parent(parent_lookup, cx);
            }
            Ok(_) | Err(BlockError::BlockIsAlreadyKnown { .. }) => {
                let chain_hash = parent_lookup.chain_hash();
                let blocks = parent_lookup.chain_blocks();
                let process_id = ChainSegmentProcessId::ParentLookup(chain_hash);

                match self
                    .beacon_processor_send
                    .try_send(WorkEvent::chain_segment(process_id, blocks))
                {
                    Ok(_) => {
                        self.parent_queue.push(parent_lookup);
                    }
                    Err(e) => {
                        error!(
                            self.log,
                            "Failed to send chain segment to processor";
                            "error" => ?e
                        );
                    }
                }
            }
            Err(BlockError::ExecutionPayloadError(e)) => match e {
                ExecutionPayloadError::NoExecutionConnection { block }
                | ExecutionPayloadError::RequestFailed { err: _, block } => {
                    // These errors indicate an issue with the EL and not the block.
                    warn!(self.log,
                        "Parent request failed. Execution layer is stalled";
                    );
                    parent_lookup.add_block(*block);
                    let chain_hash = parent_lookup.chain_hash();
                    let blocks = parent_lookup.chain_blocks_clone();
                    let process_id = ChainSegmentProcessId::ParentLookup(chain_hash);

                    match self
                        .beacon_processor_send
                        .try_send(WorkEvent::chain_segment(process_id, blocks))
                    {
                        Ok(_) => {
                            self.parent_queue.push(parent_lookup);
                        }
                        Err(e) => {
                            error!(
                                self.log,
                                "Failed to send chain segment to processor";
                                "error" => ?e
                            );
                        }
                    }
                }
                err => {
                    debug!(self.log,
                        "Parent request failed. Invalid execution payload";
                        "error" => ?err
                    );

                    // An invalid execution payload is an invalid block
                    self.failed_chains.insert(chain_hash);

                    cx.report_peer(
                        peer_id,
                        PeerAction::LowToleranceError,
                        "parent_request_err_invalid_payload",
                    );
                }
            },
            Err(outcome) => {
                // all else we consider the chain a failure and downvote the peer that sent
                // us the last block
                warn!(
                    self.log, "Invalid parent chain";
                    "score_adjustment" => %PeerAction::MidToleranceError,
                    "outcome" => ?outcome,
                    "last_peer" => %peer_id,
                );

                // Add this chain to cache of failed chains
                self.failed_chains.insert(chain_hash);

                // This currently can be a host of errors. We permit this due to the partial
                // ambiguity.
                cx.report_peer(peer_id, PeerAction::MidToleranceError, "parent_request_err");
            }
        }

        metrics::set_gauge(
            &metrics::SYNC_PARENT_BLOCK_LOOKUPS,
            self.parent_queue.len() as i64,
        );
    }

    pub fn parent_chain_processed(
        &mut self,
        chain_hash: Hash256,
        result: BatchProcessResult,
        cx: &mut SyncNetworkContext<T::EthSpec>,
    ) {
        let parent_lookup = if let Some(pos) = self
            .parent_queue
            .iter()
            .position(|request| request.chain_hash() == chain_hash)
        {
            self.parent_queue.remove(pos)
        } else {
            #[cfg(debug_assertions)]
            panic!(
                "Chain process response for a parent lookup request that was not found. Chain_hash: {}",
                chain_hash
            );
            #[cfg(not(debug_assertions))]
            return crit!(self.log, "Chain process response for a parent lookup request that was not found"; "chain_hash" => %chain_hash);
        };

        debug!(self.log, "Parent chain processed"; "chain_hash" => %chain_hash, "result" => ?result);
        match result {
            BatchProcessResult::Success(_) => {
                // nothing to do.
            }
            BatchProcessResult::Failed {
                imported_blocks: _,
                peer_action,
                mode,
            } => {
                debug!(
                    self.log,
                    "Batch processing failed";
                    "mode" => ?mode,
                );
                if let FailureMode::ExecutionLayer { pause_sync } = mode {
                    debug!(self.log, "Execution layer offline");
                    if pause_sync {
                        self.parent_queue.push(parent_lookup);
                        metrics::set_gauge(
                            &metrics::SYNC_PARENT_BLOCK_LOOKUPS,
                            self.parent_queue.len() as i64,
                        );
                        return;
                    }
                } else {
                    self.failed_chains.insert(parent_lookup.chain_hash());
                    if let Some(peer_action) = peer_action {
                        for &peer_id in parent_lookup.used_peers() {
                            cx.report_peer(peer_id, peer_action, "parent_chain_failure")
                        }
                    }
                }
            }
        }
        metrics::set_gauge(
            &metrics::SYNC_PARENT_BLOCK_LOOKUPS,
            self.parent_queue.len() as i64,
        );
    }

    /* Helper functions */

    fn send_block_for_processing(
        &mut self,
        block: Box<SignedBeaconBlock<T::EthSpec>>,
        duration: Duration,
        process_type: BlockProcessType,
    ) -> Result<(), ()> {
        trace!(self.log, "Sending block for processing"; "block" => %block.canonical_root(), "process" => ?process_type);
        let event = WorkEvent::rpc_beacon_block(block, duration, process_type);
        if let Err(e) = self.beacon_processor_send.try_send(event) {
            error!(
                self.log,
                "Failed to send sync block to processor";
                "error" => ?e
            );
            return Err(());
        }

        Ok(())
    }

    fn request_parent(
        &mut self,
        mut parent_lookup: ParentLookup<T::EthSpec>,
        cx: &mut SyncNetworkContext<T::EthSpec>,
    ) {
        match parent_lookup.request_parent(cx) {
            Err(e) => {
                debug!(self.log, "Failed to request parent"; &parent_lookup, "error" => e.as_static());
                match e {
                    parent_lookup::RequestError::SendFailed(_) => {
                        // Probably shutting down, nothing to do here. Drop the request
                    }
                    parent_lookup::RequestError::ChainTooLong
                    | parent_lookup::RequestError::TooManyAttempts => {
                        self.failed_chains.insert(parent_lookup.chain_hash());
                        // This indicates faulty peers.
                        for &peer_id in parent_lookup.used_peers() {
                            cx.report_peer(peer_id, PeerAction::LowToleranceError, e.as_static())
                        }
                    }
                    parent_lookup::RequestError::NoPeers => {
                        // This happens if the peer disconnects while the block is being
                        // processed. Drop the request without extra penalty
                    }
                }
            }
            Ok(_) => {
                debug!(self.log, "Requesting parent"; &parent_lookup);
                self.parent_queue.push(parent_lookup)
            }
        }

        // We remove and add back again requests so we want this updated regardless of outcome.
        metrics::set_gauge(
            &metrics::SYNC_PARENT_BLOCK_LOOKUPS,
            self.parent_queue.len() as i64,
        );
    }
}
