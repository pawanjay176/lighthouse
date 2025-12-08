//! A PeerDAS data column. May or may not contain all cells. This is not necessarily implementable
//! for a beacon spec container, more as more data might be required, such as the partial message
//! group id alongside a partial message.

use crate::beacon_block_body::KzgCommitments;
use crate::data::partial_data_column_sidecar::VerifiablePartialDataColumn;
use crate::data_column_sidecar::{Cell, DataColumn};
use crate::{
    ColumnIndex, DataColumnSidecar, EthSpec, Hash256, SignedBeaconBlock, SignedBeaconBlockHeader,
    Slot,
};
use kzg::{KzgCommitment, KzgProof};
use ssz_types::{FixedVector, VariableList};
use std::borrow::Cow;
use std::sync::Arc;
use crate::partial_data_column_sidecar::{DanglingPartialDataColumn, PartialDataColumnSidecar};

#[derive(Clone)]
pub struct DasColumn<E: EthSpec> {
    block_root: Hash256,
    slot: Slot,
    index: ColumnIndex,
    column: VariableList<Option<Cell<E>>, E::MaxBlobCommitmentsPerBlock>,
    kzg_proofs: VariableList<KzgProof, E::MaxBlobCommitmentsPerBlock>,
    proof_components: Option<Arc<ProofComponents<E>>>,
}

pub struct ProofComponents<E: EthSpec> {
    /// All the KZG commitments and proofs associated with the block, used for verifying sample cells.
    pub kzg_commitments: KzgCommitments<E>,
    pub signed_block_header: SignedBeaconBlockHeader,
    /// An inclusion proof, proving the inclusion of `blob_kzg_commitments` in `BeaconBlockBody`.
    pub kzg_commitments_inclusion_proof: FixedVector<Hash256, E::KzgCommitmentsInclusionProofDepth>,
}

impl<E: EthSpec> DasColumn<E> {
    fn slot(&self) -> Slot {
        self.slot
    }
    fn index(&self) -> ColumnIndex {
        self.index
    }

    fn column(&self) -> &VariableList<Option<Cell<E>>, E::MaxBlobCommitmentsPerBlock> {
        &self.column
    }

    fn proof_components(&self) -> &Option<Arc<ProofComponents<E>>> {
        &self.proof_components
    }

    fn to_partial(&self) -> DanglingPartialDataColumn<E> {
        DanglingPartialDataColumn {
            block_root: self.block_root,
            index: self.index,
            // todo
            sidecar: PartialDataColumnSidecar {
                cells_present_bitmap: (),
                column: (),
                kzg_proofs: (),
            },
        }
    }
}

fn kzg_proofs(&self) -> &VariableList<KzgProof, E::MaxBlobCommitmentsPerBlock>;
    fn kzg_commitments(&self) -> &KzgCommitments<E>;
    fn block_root(&self) -> Hash256;
    fn signed_block_header(&self) -> Option<&SignedBeaconBlockHeader>;
    fn into_partial(self) -> VerifiablePartialDataColumn<E>;

    /// Convert this column into a full data column (e.g. for gossip). Note that this is potentially
    /// expensive.
    fn as_full(
        &self,
        header: Option<&SignedBeaconBlock<E>>,
    ) -> Option<Cow<'_, DataColumnSidecar<E>>>;

    fn iter(&self) -> impl Iterator<Item = Option<CellWithMetadata<'_, E>>>;

    fn compare<C: DasColumn<E>>(&self, rhs: &C) -> ColumnComparison {
        if self.slot() != rhs.slot()
            || self.index() != rhs.index()
            || self.block_root() != rhs.block_root()
        {
            return ColumnComparison::DifferentColumns;
        }

        if self.cell_count_total() != rhs.cell_count_total() {
            return ColumnComparison::DataConflict;
        }

        let mut missing_in_rhs = vec![];
        let mut missing_in_lhs = vec![];
        for (index, (lhs, rhs)) in self.iter().zip(rhs.iter()).enumerate() {
            match (lhs, rhs) {
                (None, None) => {}
                (Some(_), None) => missing_in_rhs.push(index),
                (None, Some(_)) => missing_in_lhs.push(index),
                (Some(lhs), Some(rhs)) => {
                    if lhs != rhs {
                        return ColumnComparison::DataConflict;
                    }
                }
            }
        }
        if missing_in_rhs.is_empty() && missing_in_lhs.is_empty() {
            return ColumnComparison::Equal;
        }

        ColumnComparison::MissingCells {
            missing_in_lhs,
            missing_in_rhs,
        }
    }
}

#[derive(Debug)]
pub enum ColumnComparison {
    DifferentColumns,
    DataConflict,
    MissingCells {
        missing_in_lhs: Vec<usize>,
        missing_in_rhs: Vec<usize>,
    },
    Equal,
}

impl<E: EthSpec> PartialEq<DataColumnSidecar<E>> for VerifiablePartialDataColumn<E> {
    fn eq(&self, other: &DataColumnSidecar<E>) -> bool {
        // Slight optimisation: Can only be the same if `self` is fully present
        self.column.sidecar.is_complete()
            && self.slot() == other.slot()
            && self.index() == other.index()
            && self.block_root() == other.block_root()
            && self.kzg_commitments() == other.kzg_commitments()
            && self.column() == other.column()
            && self.kzg_proofs() == other.kzg_proofs()
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CellWithMetadata<'a, E: EthSpec> {
    pub cell: &'a Cell<E>,
    pub proof: &'a KzgProof,
    pub commitment: &'a KzgCommitment,
}
