//! The incremental processing steps (e.g., signatures verified but not the state transition) is
//! represented as a sequence of wrapper-types around the envelope. There is a linear progression of
//! types, starting at a `SignedExecutionPayloadEnvelope` and finishing with an `AvailableExecutedEnvelope` (see
//! diagram below).
//!
//! ```ignore
//! SignedExecutionPayloadEnvelope
//!              |
//!              ▼
//!    GossipVerifiedEnvelope
//!              |
//!              ▼
//!  ExecutionPendingEnvelope
//!              |
//!            await
//!              ▼
//!      ExecutedEnvelope
//!
//! ```

use state_processing::envelope_processing::EnvelopeProcessingError;
use std::collections::HashSet;
use std::sync::Arc;
use store::Error as DBError;
use strum::AsRefStr;
use tracing::instrument;
use types::{
    BeaconState, BeaconStateError, ChainSpec, DataColumnSidecarList, EthSpec, ExecutionBlockHash,
    ExecutionPayloadEnvelope, Hash256, SignedExecutionPayloadBid, SignedExecutionPayloadEnvelope,
    Slot,
};

use crate::data_availability_checker::AvailabilityCheckError;
use crate::{
    BeaconChainError, BeaconChainTypes, BeaconStore, BlockError, CustodyContext,
    ExecutionPayloadError, PayloadVerificationError, PayloadVerificationOutcome,
};

pub mod execution_pending_envelope;
pub mod gossip_verified_envelope;
pub mod import;
mod payload_notifier;

pub use execution_pending_envelope::ExecutionPendingEnvelope;

/// The data column data accompanying a Gloas payload envelope, mirroring `AvailableBlockData`.
///
/// In Gloas, data availability is checked on the payload envelope rather than the block, and
/// the KZG commitments live in the block's `signed_execution_payload_bid`.
#[derive(Debug, Clone)]
pub enum AvailableEnvelopeData<E: EthSpec> {
    /// The bid has no commitments or columns aren't required for this epoch.
    NoData,
    /// The bid commits more than zero blobs, so the full custody column set is required.
    DataColumns(DataColumnSidecarList<E>),
}

impl<E: EthSpec> AvailableEnvelopeData<E> {
    pub fn columns(&self) -> DataColumnSidecarList<E> {
        match self {
            AvailableEnvelopeData::NoData => vec![],
            AvailableEnvelopeData::DataColumns(columns) => columns.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AvailableEnvelope<E: EthSpec> {
    envelope: Arc<SignedExecutionPayloadEnvelope<E>>,
    column_data: AvailableEnvelopeData<E>,
}

impl<E: EthSpec> AvailableEnvelope<E> {
    /// Constructs an `AvailableEnvelope` from an envelope and its data columns.
    ///
    /// This mirrors `AvailableBlock::new`, validating that:
    /// - Columns are not provided when not required (bid commits zero blobs, or the bid's epoch is
    ///   beyond the data availability boundary, per `da_check_required`)
    /// - The full custody column set is present when columns are required
    ///
    /// `da_check_required` determines if the data is within the data availability
    /// boundary.
    ///
    /// Returns `AvailabilityCheckError` if:
    /// - `InvalidAvailableBlockData`: Columns are provided but not required
    /// - `MissingCustodyColumns`: Columns are required but the custody set is incomplete
    ///
    /// Note: This only enforces consistency between bid and data columns commitments.
    /// It does not perform any kzg verification.
    pub fn new(
        envelope: Arc<SignedExecutionPayloadEnvelope<E>>,
        columns: DataColumnSidecarList<E>,
        bid: &SignedExecutionPayloadBid<E>,
        da_check_required: bool,
        custody_context: &CustodyContext<E>,
        spec: &ChainSpec,
    ) -> Result<Self, AvailabilityCheckError> {
        let epoch = bid.message.slot.epoch(E::slots_per_epoch());
        // Gloas is always post-PeerDAS, so columns are required if the bid commits any blobs and
        // the epoch is within the data availability boundary.
        let columns_required = !bid.message.blob_kzg_commitments.is_empty() && da_check_required;

        if !columns_required {
            if !columns.is_empty() {
                return Err(AvailabilityCheckError::InvalidAvailableBlockData);
            } else {
                return Ok(Self {
                    envelope,
                    column_data: AvailableEnvelopeData::NoData,
                });
            }
        }

        let mut column_indices = custody_context
            .sampling_columns_for_epoch(epoch, spec)
            .iter()
            .collect::<HashSet<_>>();

        for column in &columns {
            column_indices.remove(column.index());
        }

        if !column_indices.is_empty() {
            return Err(AvailabilityCheckError::MissingCustodyColumns);
        }

        Ok(Self {
            envelope,
            column_data: AvailableEnvelopeData::DataColumns(columns),
        })
    }

    pub fn envelope(&self) -> &Arc<SignedExecutionPayloadEnvelope<E>> {
        &self.envelope
    }

    pub fn message(&self) -> &ExecutionPayloadEnvelope<E> {
        &self.envelope.message
    }

    pub fn columns(&self) -> DataColumnSidecarList<E> {
        self.column_data.columns()
    }

    #[allow(clippy::type_complexity)]
    pub fn deconstruct(
        self,
    ) -> (
        Arc<SignedExecutionPayloadEnvelope<E>>,
        DataColumnSidecarList<E>,
    ) {
        let AvailableEnvelope {
            envelope,
            column_data,
        } = self;
        (envelope, column_data.columns())
    }
}

/// This snapshot is to be used for verifying a payload envelope.
#[derive(Debug, Clone)]
pub struct EnvelopeProcessingSnapshot<E: EthSpec> {
    /// This state is equivalent to the `self.beacon_block.state_root()` before applying the envelope.
    pub pre_state: BeaconState<E>,
    pub state_root: Hash256,
    pub beacon_block_root: Hash256,
}

/// A payload envelope that has completed all envelope processing checks, verification
/// by an EL client but does not have all requisite columns to get imported into
/// fork choice.
pub struct AvailabilityPendingExecutedEnvelope<E: EthSpec> {
    pub envelope: Arc<SignedExecutionPayloadEnvelope<E>>,
    pub block_root: Hash256,
    pub payload_verification_outcome: PayloadVerificationOutcome,
}

impl<E: EthSpec> AvailabilityPendingExecutedEnvelope<E> {
    pub fn new(
        envelope: Arc<SignedExecutionPayloadEnvelope<E>>,
        block_root: Hash256,
        payload_verification_outcome: PayloadVerificationOutcome,
    ) -> Self {
        Self {
            envelope,
            block_root,
            payload_verification_outcome,
        }
    }
}

/// A payload envelope that has completed all payload processing checks including verification
/// by an EL client **and** has all requisite blob data to be imported into fork choice.
pub struct AvailableExecutedEnvelope<E: EthSpec> {
    pub envelope: AvailableEnvelope<E>,
    pub block_root: Hash256,
    pub payload_verification_outcome: PayloadVerificationOutcome,
}

impl<E: EthSpec> AvailableExecutedEnvelope<E> {
    pub fn new(
        envelope: AvailableEnvelope<E>,
        block_root: Hash256,
        payload_verification_outcome: PayloadVerificationOutcome,
    ) -> Self {
        Self {
            envelope,
            block_root,
            payload_verification_outcome,
        }
    }
}

#[derive(Debug, AsRefStr)]
pub enum EnvelopeError {
    /// The envelope's block root is unknown.
    BlockRootUnknown { block_root: Hash256 },
    /// The signature is invalid.
    BadSignature,
    /// The builder index doesn't match the committed bid
    BuilderIndexMismatch { committed_bid: u64, envelope: u64 },
    /// The envelope slot doesn't match the block
    SlotMismatch { block: Slot, envelope: Slot },
    /// The validator index is unknown
    UnknownValidator { proposer_index: u64 },
    /// The block hash doesn't match the committed bid
    BlockHashMismatch {
        committed_bid: ExecutionBlockHash,
        envelope: ExecutionBlockHash,
    },
    /// The block's proposer_index does not match the locally computed proposer
    IncorrectBlockProposer {
        proposer_index: u64,
        local_shuffling: u64,
    },
    /// The slot belongs to a block that is from a slot prior than
    /// to most recently finalized slot
    PriorToFinalization {
        payload_slot: Slot,
        latest_finalized_slot: Slot,
    },
    /// Some Beacon Chain Error
    BeaconChainError(Box<BeaconChainError>),
    /// Some Beacon State error
    BeaconStateError(BeaconStateError),
    /// Some EnvelopeProcessingError
    EnvelopeProcessingError(EnvelopeProcessingError),
    /// Error verifying the execution payload
    ExecutionPayloadError(ExecutionPayloadError),
    /// Optimistic sync is not supported for Gloas payload envelopes.
    OptimisticSyncNotSupported { block_root: Hash256 },
    /// The envelope's beacon block was not present in fork choice at import time.
    ///
    /// Unlike [`EnvelopeError::BlockRootUnknown`] (raised during gossip verification, where the
    /// block may simply not have arrived yet), this is raised during import where the block is
    /// expected to already be present, so it indicates an internal inconsistency.
    BlockRootNotInForkChoice(Hash256),
    /// An internal error occurred while importing the envelope (e.g. updating fork choice).
    InternalError(String),
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl EnvelopeError {
    pub fn penalize_peer(&self) -> bool {
        match self {
            EnvelopeError::BadSignature
            | EnvelopeError::BuilderIndexMismatch { .. }
            | EnvelopeError::SlotMismatch { .. }
            | EnvelopeError::BlockHashMismatch { .. }
            | EnvelopeError::UnknownValidator { .. }
            | EnvelopeError::IncorrectBlockProposer { .. }
            | EnvelopeError::EnvelopeProcessingError(_) => true,
            EnvelopeError::ExecutionPayloadError(e) => e.penalize_peer(),
            EnvelopeError::BlockRootUnknown { .. }
            | EnvelopeError::PriorToFinalization { .. }
            | EnvelopeError::BeaconChainError(_)
            | EnvelopeError::BeaconStateError(_)
            | EnvelopeError::OptimisticSyncNotSupported { .. }
            | EnvelopeError::BlockRootNotInForkChoice(_)
            | EnvelopeError::InternalError(_) => false,
        }
    }
}

impl From<BeaconChainError> for EnvelopeError {
    fn from(e: BeaconChainError) -> Self {
        EnvelopeError::BeaconChainError(Box::new(e))
    }
}

impl From<ExecutionPayloadError> for EnvelopeError {
    fn from(e: ExecutionPayloadError) -> Self {
        EnvelopeError::ExecutionPayloadError(e)
    }
}

impl From<BeaconStateError> for EnvelopeError {
    fn from(e: BeaconStateError) -> Self {
        EnvelopeError::BeaconStateError(e)
    }
}

impl From<DBError> for EnvelopeError {
    fn from(e: DBError) -> Self {
        EnvelopeError::BeaconChainError(Box::new(BeaconChainError::DBError(e)))
    }
}

impl From<EnvelopeError> for BlockError {
    fn from(e: EnvelopeError) -> Self {
        BlockError::EnvelopeError(Box::new(e))
    }
}

impl From<PayloadVerificationError> for EnvelopeError {
    fn from(e: PayloadVerificationError) -> Self {
        match e {
            PayloadVerificationError::ExecutionPayloadError(e) => {
                EnvelopeError::ExecutionPayloadError(e)
            }
            PayloadVerificationError::BeaconChainError(e) => EnvelopeError::BeaconChainError(e),
        }
    }
}

impl From<EnvelopeProcessingError> for EnvelopeError {
    fn from(e: EnvelopeProcessingError) -> Self {
        match e {
            EnvelopeProcessingError::BadSignature => EnvelopeError::BadSignature,
            EnvelopeProcessingError::BeaconStateError(e) => EnvelopeError::BeaconStateError(e),
            EnvelopeProcessingError::BlockHashMismatch {
                committed_bid,
                envelope,
            } => EnvelopeError::BlockHashMismatch {
                committed_bid,
                envelope,
            },
            e => EnvelopeError::EnvelopeProcessingError(e),
        }
    }
}

#[instrument(skip_all, level = "debug", fields(beacon_block_root = %beacon_block_root))]
/// Load state from store given a known state root and block root.
/// Use this when the proto block has already been looked up from fork choice.
pub(crate) fn load_snapshot_from_state_root<T: BeaconChainTypes>(
    beacon_block_root: Hash256,
    block_state_root: Hash256,
    store: &BeaconStore<T>,
) -> Result<EnvelopeProcessingSnapshot<T::EthSpec>, EnvelopeError> {
    // TODO(EIP-7732): add metrics here

    // We can use `get_hot_state` here rather than `get_advanced_hot_state` because the envelope
    // must be from the same slot as its block (so no advance is required).
    let cache_state = true;
    let state = store
        .get_hot_state(&block_state_root, cache_state)
        .map_err(EnvelopeError::from)?
        .ok_or_else(|| {
            BeaconChainError::DBInconsistent(format!(
                "Missing state for envelope block {block_state_root:?}",
            ))
        })?;

    Ok(EnvelopeProcessingSnapshot {
        pre_state: state,
        state_root: block_state_root,
        beacon_block_root,
    })
}
