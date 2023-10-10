mod kzg_commitment;
mod kzg_proof;
mod trusted_setup;

pub use crate::{kzg_commitment::KzgCommitment, kzg_proof::KzgProof, trusted_setup::TrustedSetup};
pub use c_kzg::{
    Bytes32, Bytes48, KzgSettings, BYTES_PER_COMMITMENT, BYTES_PER_FIELD_ELEMENT, BYTES_PER_PROOF,
};

#[derive(Debug)]
pub enum Error {
    CKzgError(c_kzg::Error),
    TrustedSetupMismatch,
}

impl From<c_kzg::Error> for Error {
    fn from(value: c_kzg::Error) -> Self {
        Self::CKzgError(value)
    }
}
