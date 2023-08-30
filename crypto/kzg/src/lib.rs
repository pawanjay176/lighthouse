mod kzg_commitment;
mod kzg_proof;
mod trusted_setup;

use serde_derive::{Deserialize, Serialize};
use std::fmt::Debug;
use std::str::FromStr;

pub use crate::{kzg_commitment::KzgCommitment, kzg_proof::KzgProof, trusted_setup::TrustedSetup};
pub use c_kzg::{
    Blob, Bytes32, Bytes48, KzgSettings, BYTES_PER_COMMITMENT, BYTES_PER_FIELD_ELEMENT,
    BYTES_PER_PROOF,
};

#[derive(Debug)]
pub enum Error {
    InvalidTrustedSetup(c_kzg::Error),
    InvalidKzgProof(c_kzg::Error),
    InvalidBytes(c_kzg::Error),
    KzgProofComputationFailed(c_kzg::Error),
    InvalidBlob(c_kzg::Error),
    InvalidBytesForBlob(c_kzg::Error),
}

pub enum KzgPresetId {
    Mainnet,
    Minimal,
}

impl FromStr for KzgPresetId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "mainnet" => Ok(KzgPresetId::Mainnet),
            "minimal" => Ok(KzgPresetId::Minimal),
            _ => Err(format!("Unknown eth spec: {}", s)),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, arbitrary::Arbitrary)]
pub struct MainnetKzgPreset;
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, arbitrary::Arbitrary)]
pub struct MinimalKzgPreset;

/// A wrapper over a kzg library that holds the trusted setup parameters.
#[derive(Debug)]
pub struct Kzg {
    trusted_setup: KzgSettings,
}

impl Kzg {
    /// Load the kzg trusted setup parameters from a vec of G1 and G2 points.
    ///
    /// The number of G1 points should be equal to FIELD_ELEMENTS_PER_BLOB
    /// Note: this number changes based on the preset values.
    /// The number of G2 points should be equal to 65.
    pub fn new_from_trusted_setup(trusted_setup: TrustedSetup) -> Result<Self, Error> {
        Ok(Self {
            trusted_setup: c_kzg::KzgSettings::load_trusted_setup(
                trusted_setup.g1_points(),
                trusted_setup.g2_points(),
            )
            .map_err(Error::InvalidTrustedSetup)?,
        })
    }

    pub fn bytes_per_blob(&self) -> usize {
        self.trusted_setup.get_bytes_per_blob()
    }

    pub fn field_elements_per_blob(&self) -> usize {
        self.trusted_setup.get_field_elements_per_blob()
    }

    pub fn blob_from_bytes(&self, bytes: &[u8]) -> Result<Blob, Error> {
        Blob::from_bytes(bytes, &self.trusted_setup).map_err(Error::InvalidBlob)
    }

    /// Compute the kzg proof given a blob and its kzg commitment.
    pub fn compute_blob_kzg_proof(
        &self,
        blob: Blob,
        kzg_commitment: KzgCommitment,
    ) -> Result<KzgProof, Error> {
        c_kzg::KzgProof::compute_blob_kzg_proof(&blob, kzg_commitment.into(), &self.trusted_setup)
            .map_err(Error::KzgProofComputationFailed)
            .map(|proof| KzgProof(proof.to_bytes().into_inner()))
    }

    /// Verify a kzg proof given the blob, kzg commitment and kzg proof.
    pub fn verify_blob_kzg_proof(
        &self,
        blob: Blob,
        kzg_commitment: KzgCommitment,
        kzg_proof: KzgProof,
    ) -> Result<bool, Error> {
        c_kzg::KzgProof::verify_blob_kzg_proof(
            &blob,
            kzg_commitment.into(),
            kzg_proof.into(),
            &self.trusted_setup,
        )
        .map_err(Error::InvalidKzgProof)
    }

    /// Verify a batch of blob commitment proof triplets.
    ///
    /// Note: This method is slightly faster than calling `Self::verify_blob_kzg_proof` in a loop sequentially.
    /// TODO(pawan): test performance against a parallelized rayon impl.
    pub fn verify_blob_kzg_proof_batch(
        &self,
        blobs: &[Blob],
        kzg_commitments: &[KzgCommitment],
        kzg_proofs: &[KzgProof],
    ) -> Result<bool, Error> {
        let commitments_bytes = kzg_commitments
            .iter()
            .map(|comm| Bytes48::from(*comm))
            .collect::<Vec<_>>();

        let proofs_bytes = kzg_proofs
            .iter()
            .map(|proof| Bytes48::from(*proof))
            .collect::<Vec<_>>();

        c_kzg::KzgProof::verify_blob_kzg_proof_batch(
            blobs,
            &commitments_bytes,
            &proofs_bytes,
            &self.trusted_setup,
        )
        .map_err(Error::InvalidKzgProof)
    }

    /// Converts a blob to a kzg commitment.
    pub fn blob_to_kzg_commitment(&self, blob: Blob) -> Result<KzgCommitment, Error> {
        c_kzg::KzgCommitment::blob_to_kzg_commitment(&blob, &self.trusted_setup)
            .map_err(Error::InvalidBlob)
            .map(|commitment| KzgCommitment(commitment.to_bytes().into_inner()))
    }

    /// Computes the kzg proof for a given `blob` and an evaluation point `z`
    pub fn compute_kzg_proof(&self, blob: Blob, z: Bytes32) -> Result<(KzgProof, Bytes32), Error> {
        c_kzg::KzgProof::compute_kzg_proof(&blob, z, &self.trusted_setup)
            .map_err(Error::KzgProofComputationFailed)
            .map(|(proof, y)| (KzgProof(proof.to_bytes().into_inner()), y))
    }

    /// Verifies a `kzg_proof` for a `kzg_commitment` that evaluating a polynomial at `z` results in `y`
    pub fn verify_kzg_proof(
        &self,
        kzg_commitment: KzgCommitment,
        z: Bytes32,
        y: Bytes32,
        kzg_proof: KzgProof,
    ) -> Result<bool, Error> {
        c_kzg::KzgProof::verify_kzg_proof(
            kzg_commitment.into(),
            z,
            y,
            kzg_proof.into(),
            &self.trusted_setup,
        )
        .map_err(Error::InvalidKzgProof)
    }
}
