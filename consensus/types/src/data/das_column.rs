//! A PeerDAS data column. May or may not contain all cells. This is not necessarily implementable
//! for a beacon spec container, more as more data might be required, such as the partial message
//! group id alongside a partial message.

use crate::beacon_block_body::KzgCommitments;
use crate::data_column_sidecar::Cell;
use crate::partial_data_column_sidecar::{
    CellBitmap, DanglingPartialDataColumn, PartialDataColumnSidecar, VerifiablePartialDataColumn,
};
use crate::{
    BeaconBlockHeader, ColumnIndex, DataColumnSidecar, EthSpec, Hash256, SignedBeaconBlock,
    SignedBeaconBlockHeader, Slot,
};
use bls::Signature;
use kzg::{KzgCommitment, KzgProof};
use ssz_types::{FixedVector, VariableList};
use std::borrow::Cow;
use std::sync::Arc;
use tree_hash::TreeHash;

#[derive(Clone)]
pub struct DasColumn<E: EthSpec> {
    block_root: Hash256,
    slot: Slot,
    index: ColumnIndex,
    column: VariableList<Option<Arc<(Cell<E>, KzgProof)>>, E::MaxBlobCommitmentsPerBlock>,
    /// All the KZG commitments and proofs associated with the block, used for verifying sample cells.
    kzg_commitments: KzgCommitments<E>,
    signed_block_header: SignedBeaconBlockHeader,
    /// An inclusion proof, proving the inclusion of `blob_kzg_commitments` in `BeaconBlockBody`.
    kzg_commitments_inclusion_proof: FixedVector<Hash256, E::KzgCommitmentsInclusionProofDepth>,
}

impl<E: EthSpec> DasColumn<E> {
    pub fn slot(&self) -> Slot {
        self.slot
    }

    pub fn index(&self) -> ColumnIndex {
        self.index
    }

    pub fn block_root(&self) -> Hash256 {
        self.block_root
    }

    pub fn column(
        &self,
    ) -> &VariableList<Option<Arc<(Cell<E>, KzgProof)>>, E::MaxBlobCommitmentsPerBlock> {
        &self.column
    }

    pub fn kzg_commitments(&self) -> &KzgCommitments<E> {
        &self.kzg_commitments
    }

    pub fn signed_block_header(&self) -> &SignedBeaconBlockHeader {
        &self.signed_block_header
    }

    /// Returns the total number of cell slots (including None/missing cells)
    pub fn cell_count_total(&self) -> usize {
        self.column.len()
    }

    /// Returns the number of present cells (Some values)
    pub fn cell_count_present(&self) -> usize {
        self.column.iter().filter(|cell| cell.is_some()).count()
    }

    /// Returns an iterator over indices of present cells
    pub fn cells_present(&self) -> impl Iterator<Item = usize> + '_ {
        self.column
            .iter()
            .enumerate()
            .filter_map(|(idx, cell)| cell.as_ref().map(|_| idx))
    }

    /// Returns true if all cells are present (no None values)
    pub fn is_complete(&self) -> bool {
        self.column.iter().all(|cell| cell.is_some())
    }

    /// Iterator over cells with metadata
    /// Returns Some(CellWithMetadata) for present cells, None for missing cells
    pub fn iter(&self) -> impl Iterator<Item = Option<CellWithMetadata<'_, E>>> + '_ {
        let commitments = self.kzg_commitments();

        self.column.iter().enumerate().map(move |(idx, data)| {
            data.as_deref().and_then(|(cell, proof)| {
                commitments.get(idx).map(|commitment| CellWithMetadata {
                    cell,
                    proof,
                    commitment,
                })
            })
        })
    }

    pub fn compare(&self, rhs: &DasColumn<E>) -> ColumnComparison {
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

// Conversions FROM external types TO DasColumn

impl<E: EthSpec> From<DataColumnSidecar<E>> for DasColumn<E> {
    fn from(sidecar: DataColumnSidecar<E>) -> Self {
        // Wrap each cell in Some() to create full-length vector
        let column = sidecar
            .column
            .iter()
            .map(|cell| Some(cell.clone()))
            .collect::<Vec<_>>();

        let column = VariableList::new(column).expect("Column length within bounds");

        // Create ProofComponents from sidecar fields
        let proof_components = Some(Arc::new(ProofComponents {
            kzg_commitments: sidecar.kzg_commitments.clone(),
            signed_block_header: sidecar.signed_block_header.clone(),
            kzg_commitments_inclusion_proof: sidecar.kzg_commitments_inclusion_proof.clone(),
        }));

        // Extract block_root from signed_block_header
        let block_root = sidecar.signed_block_header.message.tree_hash_root();
        let slot = sidecar.signed_block_header.message.slot;

        DasColumn {
            block_root,
            slot,
            index: sidecar.index,
            column,
            kzg_proofs: sidecar.kzg_proofs,
            proof_components,
        }
    }
}

impl<E: EthSpec> DasColumn<E> {
    /// Create a DasColumn from a DanglingPartialDataColumn with a provided slot
    pub fn from_dangling_partial(partial: DanglingPartialDataColumn<E>, slot: Slot) -> Self {
        let sidecar = &partial.sidecar;
        let bitmap = &sidecar.cells_present_bitmap;

        // Create full-length vector initialized with None
        let mut column_vec = vec![None; bitmap.len()];
        let mut kzg_proofs_vec = vec![KzgProof::empty(); bitmap.len()];

        // Iterate bitmap to find present cell positions
        let mut packed_idx = 0;
        for (idx, present) in bitmap.iter().enumerate() {
            if present {
                if let Some(cell) = sidecar.column.get(packed_idx) {
                    column_vec[idx] = Some(cell.clone());
                }
                if let Some(proof) = sidecar.kzg_proofs.get(packed_idx) {
                    kzg_proofs_vec[idx] = *proof;
                }
                packed_idx += 1;
            }
        }

        let column = VariableList::new(column_vec).expect("Column length within bounds");
        let kzg_proofs = VariableList::new(kzg_proofs_vec).expect("Proofs length within bounds");

        DasColumn {
            block_root: partial.block_root,
            slot,
            index: partial.index,
            column,
            kzg_proofs,
            proof_components: None, // No commitments for dangling partial
        }
    }

    /// Convert DasColumn to DanglingPartialDataColumn
    /// Returns error if no cells are present
    pub fn to_partial(&self) -> Result<DanglingPartialDataColumn<E>, PartialConversionError> {
        // Create bitmap with length = column.len()
        let mut bitmap = CellBitmap::<E>::with_capacity(self.column.len())
            .map_err(|_| PartialConversionError::BitmapCreation)?;

        // Create empty packed vectors for cells and proofs
        let mut packed_column = Vec::new();
        let mut packed_proofs = Vec::new();

        // Iterate column and pack present cells
        for (idx, cell_opt) in self.column.iter().enumerate() {
            match cell_opt {
                Some(cell) => {
                    bitmap
                        .set(idx, true)
                        .map_err(|_| PartialConversionError::BitmapSet)?;
                    packed_column.push(cell.clone());
                    packed_proofs.push(
                        *self
                            .kzg_proofs
                            .get(idx)
                            .ok_or(PartialConversionError::KzgProofMissing { index: idx })?,
                    );
                }
                None => {
                    bitmap
                        .set(idx, false)
                        .map_err(|_| PartialConversionError::BitmapSet)?;
                }
            }
        }

        // Validate at least one cell present
        if packed_column.is_empty() {
            return Err(PartialConversionError::NoPresentCells);
        }

        let column = VariableList::new(packed_column).expect("Column length within bounds");
        let kzg_proofs = VariableList::new(packed_proofs).expect("Proofs length within bounds");

        Ok(DanglingPartialDataColumn {
            block_root: self.block_root,
            index: self.index,
            sidecar: PartialDataColumnSidecar {
                cells_present_bitmap: bitmap,
                column,
                kzg_proofs,
            },
        })
    }

    /// Convert DasColumn to DataColumnSidecar if all cells are present and proof_components available
    pub fn to_full(&self) -> Result<DataColumnSidecar<E>, FullConversionError> {
        // Validate all cells are present
        if !self.is_complete() {
            let present = self.cell_count_present();
            let total = self.cell_count_total();
            return Err(FullConversionError::IncompleteCells { present, total });
        }

        // Validate proof_components exist
        let proof_components = self
            .proof_components
            .as_ref()
            .ok_or(FullConversionError::MissingProofComponents)?;

        // Unwrap all Option<Cell> to create dense VariableList<Cell>
        let column = self
            .column
            .iter()
            .map(|cell_opt| {
                cell_opt
                    .clone()
                    .expect("is_complete() check ensures all cells are Some")
            })
            .collect::<Vec<_>>();

        let column = VariableList::new(column).expect("Column length within bounds");

        Ok(DataColumnSidecar {
            index: self.index,
            column,
            kzg_commitments: proof_components.kzg_commitments.clone(),
            kzg_proofs: self.kzg_proofs.clone(),
            signed_block_header: proof_components.signed_block_header.clone(),
            kzg_commitments_inclusion_proof: proof_components
                .kzg_commitments_inclusion_proof
                .clone(),
        })
    }

    /// Try to convert to full DataColumnSidecar, optionally using block to construct proof_components
    pub fn as_full<'a>(
        &'a self,
        block: Option<&SignedBeaconBlock<E>>,
    ) -> Option<Cow<'a, DataColumnSidecar<E>>> {
        // First try direct conversion
        if let Ok(full) = self.to_full() {
            return Some(Cow::Owned(full));
        }

        // If missing proof_components but have block, try to construct
        if self.is_complete() && self.proof_components.is_none() {
            if let Some(block) = block {
                // Try to construct proof_components from block
                if let Ok((signed_block_header, kzg_commitments_inclusion_proof)) =
                    block.signed_block_header_and_kzg_commitments_proof()
                {
                    if let Ok(kzg_commitments) =
                        block.message().body().blob_kzg_commitments().cloned()
                    {
                        // Unwrap all cells
                        let column = self
                            .column
                            .iter()
                            .map(|cell_opt| {
                                cell_opt
                                    .clone()
                                    .expect("is_complete() check ensures all cells are Some")
                            })
                            .collect::<Vec<_>>();

                        let column =
                            VariableList::new(column).expect("Column length within bounds");

                        return Some(Cow::Owned(DataColumnSidecar {
                            index: self.index,
                            column,
                            kzg_commitments,
                            kzg_proofs: self.kzg_proofs.clone(),
                            signed_block_header,
                            kzg_commitments_inclusion_proof,
                        }));
                    }
                }
            }
        }

        None
    }

    /// Merge another DasColumn into this one
    /// Returns Ok(true) if any cells were added, Ok(false) if no changes
    /// Returns Err if columns are incompatible or have conflicting data
    pub fn merge(&mut self, other: &DasColumn<E>) -> Result<bool, MergeError> {
        // Validate compatibility
        if self.slot != other.slot {
            return Err(MergeError::SlotMismatch {
                self_slot: self.slot,
                other_slot: other.slot,
            });
        }
        if self.index != other.index {
            return Err(MergeError::IndexMismatch {
                self_index: self.index,
                other_index: other.index,
            });
        }
        if self.block_root != other.block_root {
            return Err(MergeError::BlockRootMismatch);
        }
        if self.column.len() != other.column.len() {
            return Err(MergeError::LengthMismatch {
                self_len: self.column.len(),
                other_len: other.column.len(),
            });
        }

        let mut did_merge = false;

        // Iterate both columns in parallel
        for (idx, (self_cell, other_cell)) in
            self.column.iter_mut().zip(other.column.iter()).enumerate()
        {
            match (self_cell.as_ref(), other_cell.as_ref()) {
                (None, Some(cell)) => {
                    // Copy cell from other
                    *self_cell = Some(cell.clone());
                    // Copy proof
                    if let Some(proof) = other.kzg_proofs.get(idx) {
                        if let Some(self_proof) = self.kzg_proofs.get_mut(idx) {
                            *self_proof = *proof;
                        }
                    }
                    did_merge = true;
                }
                (Some(self_cell), Some(other_cell)) => {
                    // Verify cells match
                    if self_cell != other_cell {
                        return Err(MergeError::DataConflict { index: idx });
                    }
                    // Also verify proofs match
                    if let (Some(self_proof), Some(other_proof)) =
                        (self.kzg_proofs.get(idx), other.kzg_proofs.get(idx))
                    {
                        if self_proof != other_proof {
                            return Err(MergeError::DataConflict { index: idx });
                        }
                    }
                }
                (Some(_), None) | (None, None) => {
                    // No action needed
                }
            }
        }

        // Copy proof_components if self doesn't have it
        if self.proof_components.is_none() && other.proof_components.is_some() {
            self.proof_components = other.proof_components.clone();
            did_merge = true;
        }

        Ok(did_merge)
    }
}

#[derive(Debug)]
pub enum PartialConversionError {
    NoPresentCells,
    BitmapCreation,
    BitmapSet,
    KzgProofMissing { index: usize },
}

#[derive(Debug)]
pub enum FullConversionError {
    IncompleteCells { present: usize, total: usize },
    MissingProofComponents,
}

#[derive(Debug)]
pub enum MergeError {
    SlotMismatch {
        self_slot: Slot,
        other_slot: Slot,
    },
    IndexMismatch {
        self_index: ColumnIndex,
        other_index: ColumnIndex,
    },
    BlockRootMismatch,
    LengthMismatch {
        self_len: usize,
        other_len: usize,
    },
    DataConflict {
        index: usize,
    },
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
            && self.slot == other.slot()
            && self.column.index == other.index
            && self.column.block_root == other.block_root()
            && self.kzg_commitments == other.kzg_commitments
            && self.column.sidecar.column == other.column
            && self.column.sidecar.kzg_proofs == other.kzg_proofs
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CellWithMetadata<'a, E: EthSpec> {
    pub cell: &'a Cell<E>,
    pub proof: &'a KzgProof,
    pub commitment: &'a KzgCommitment,
}
