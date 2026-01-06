//! A PeerDAS data column. May or may not contain all cells. This is not necessarily implementable
//! for a beacon spec container, more as more data might be required, such as the partial message
//! group id alongside a partial message.

use crate::beacon_block_body::KzgCommitments;
use crate::data_column_sidecar::Cell;
use crate::partial_data_column_sidecar::{
    CellBitmap, DanglingPartialDataColumn, PartialDataColumnSidecar, VerifiablePartialDataColumn,
};
use crate::{ColumnIndex, DataColumnSidecar, EthSpec, Hash256, SignedBeaconBlockHeader, Slot};
use kzg::{KzgCommitment, KzgProof};
use safe_arith::{ArithError, SafeArith};
use ssz_types::{FixedVector, VariableList};
use tree_hash::TreeHash;

type OptionCellAndProof<E> = Option<(Cell<E>, KzgProof)>;

#[derive(Clone)]
pub struct DasColumn<E: EthSpec> {
    block_root: Hash256,
    slot: Slot,
    index: ColumnIndex,
    column: VariableList<OptionCellAndProof<E>, E::MaxBlobCommitmentsPerBlock>,
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

    pub fn column(&self) -> &VariableList<OptionCellAndProof<E>, E::MaxBlobCommitmentsPerBlock> {
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
            data.as_ref().and_then(|(cell, proof)| {
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

impl<E: EthSpec> From<DataColumnSidecar<E>> for DasColumn<E> {
    fn from(sidecar: DataColumnSidecar<E>) -> Self {
        // Pair each cell with its proof
        let column = sidecar
            .column
            .iter()
            .zip(sidecar.kzg_proofs.iter())
            .map(|(cell, proof)| Some((cell.clone(), *proof)))
            .collect::<Vec<_>>();

        let column = VariableList::new(column).expect("Column length within bounds");

        // Extract block_root from signed_block_header
        let block_root = sidecar.signed_block_header.message.tree_hash_root();
        let slot = sidecar.signed_block_header.message.slot;

        DasColumn {
            block_root,
            slot,
            index: sidecar.index,
            column,
            kzg_commitments: sidecar.kzg_commitments,
            signed_block_header: sidecar.signed_block_header,
            kzg_commitments_inclusion_proof: sidecar.kzg_commitments_inclusion_proof,
        }
    }
}

impl<E: EthSpec> DasColumn<E> {
    /// Create a DasColumn from a DanglingPartialDataColumn with a provided slot and commitments
    pub fn from_dangling_partial(
        partial: DanglingPartialDataColumn<E>,
        slot: Slot,
        kzg_commitments: KzgCommitments<E>,
        signed_block_header: SignedBeaconBlockHeader,
        kzg_commitments_inclusion_proof: FixedVector<Hash256, E::KzgCommitmentsInclusionProofDepth>,
    ) -> Result<Self, ConversionError> {
        let sidecar = &partial.sidecar;
        let bitmap = &sidecar.cells_present_bitmap;

        // Iterate bitmap to find present cell positions and pair cells with proofs
        let mut packed_idx = 0;
        let column_vec = bitmap
            .iter()
            .map(|present| {
                present
                    .then(|| {
                        if let (Some(cell), Some(proof)) = (
                            sidecar.column.get(packed_idx),
                            sidecar.kzg_proofs.get(packed_idx),
                        ) {
                            packed_idx = packed_idx
                                .safe_add(1)
                                .map_err(ConversionError::ArithError)?;
                            Ok((cell.clone(), *proof))
                        } else {
                            Err(ConversionError::InconsistentPartialDataColumn)
                        }
                    })
                    .transpose()
            })
            .collect::<Result<Vec<_>, _>>()?;

        let column = VariableList::new(column_vec)
            .map_err(|_| ConversionError::UnexpectedOutOfSpecBounds)?;

        Ok(DasColumn {
            block_root: partial.block_root,
            slot,
            index: partial.index,
            column,
            kzg_commitments,
            signed_block_header,
            kzg_commitments_inclusion_proof,
        })
    }

    /// Convert DasColumn to DanglingPartialDataColumn
    /// Returns error if no cells are present
    pub fn to_partial(&self) -> Result<DanglingPartialDataColumn<E>, ConversionError> {
        // Create bitmap with length = column.len()
        let mut bitmap = CellBitmap::<E>::with_capacity(self.column.len())
            .map_err(|_| ConversionError::UnexpectedOutOfSpecBounds)?;

        // Create empty packed vectors for cells and proofs
        let mut packed_column = Vec::new();
        let mut packed_proofs = Vec::new();

        // Iterate column and pack present cells, unpacking cell/proof pairs
        for (idx, cell_data) in self.column.iter().enumerate() {
            match cell_data {
                Some(cell_proof) => {
                    bitmap
                        .set(idx, true)
                        .map_err(|_| ConversionError::BitmapSet)?;
                    let (cell, proof) = cell_proof;
                    packed_column.push(cell.clone());
                    packed_proofs.push(*proof);
                }
                None => {
                    bitmap
                        .set(idx, false)
                        .map_err(|_| ConversionError::BitmapSet)?;
                }
            }
        }

        // Validate at least one cell present
        if packed_column.is_empty() {
            return Err(ConversionError::NoPresentCells);
        }

        let column = VariableList::new(packed_column)
            .map_err(|_| ConversionError::UnexpectedOutOfSpecBounds)?;
        let kzg_proofs = VariableList::new(packed_proofs)
            .map_err(|_| ConversionError::UnexpectedOutOfSpecBounds)?;

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

    /// Convert DasColumn to DataColumnSidecar if all cells are present
    pub fn to_full(&self) -> Result<DataColumnSidecar<E>, ConversionError> {
        // Unpack all cell/proof pairs to create separate dense lists
        let mut cells = Vec::new();
        let mut proofs = Vec::new();

        for cell_data in self.column.iter() {
            let Some((cell, proof)) = cell_data else {
                return Err(ConversionError::IncompleteCells);
            };
            cells.push(cell.clone());
            proofs.push(*proof);
        }

        let column =
            VariableList::new(cells).map_err(|_| ConversionError::UnexpectedOutOfSpecBounds)?;
        let kzg_proofs =
            VariableList::new(proofs).map_err(|_| ConversionError::UnexpectedOutOfSpecBounds)?;

        Ok(DataColumnSidecar {
            index: self.index,
            column,
            kzg_commitments: self.kzg_commitments.clone(),
            kzg_proofs,
            signed_block_header: self.signed_block_header.clone(),
            kzg_commitments_inclusion_proof: self.kzg_commitments_inclusion_proof.clone(),
        })
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
                (None, Some(cell_proof)) => {
                    // Copy cell/proof pair from other
                    *self_cell = Some(cell_proof.clone());
                    did_merge = true;
                }
                (Some(self_data), Some(other_data)) => {
                    // Verify cell/proof pairs match
                    if self_data != other_data {
                        return Err(MergeError::DataConflict { index: idx });
                    }
                }
                (Some(_), None) | (None, None) => {
                    // No action needed
                }
            }
        }

        Ok(did_merge)
    }
}

#[derive(Debug)]
pub enum ConversionError {
    NoPresentCells,
    UnexpectedOutOfSpecBounds,
    BitmapSet,
    InconsistentPartialDataColumn,
    ArithError(ArithError),
    KzgProofMissing { index: usize },
    IncompleteCells,
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
