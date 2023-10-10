use crate::{Blob, EthSpec, Hash256};
pub use kzg::{
    Bytes32, Bytes48, Error as KzgError, KzgCommitment, KzgProof, KzgSettings, TrustedSetup,
};
use std::marker::PhantomData;

/// A wrapper over a kzg library that holds the trusted setup parameters.
#[derive(Debug)]
pub struct Kzg<E: EthSpec> {
    trusted_setup: KzgSettings,
    _phantom: PhantomData<E>,
}

impl<E: EthSpec> Kzg<E> {
    /// Load the kzg trusted setup parameters from a vec of G1 and G2 points.
    ///
    /// The number of G1 points should be equal to FIELD_ELEMENTS_PER_BLOB
    /// Note: this number changes based on the preset values.
    /// The number of G2 points should be equal to 65.
    pub fn new_from_trusted_setup(trusted_setup: TrustedSetup) -> Result<Self, KzgError> {
        let trusted_setup = KzgSettings::load_trusted_setup(
            trusted_setup.g1_points().as_slice(),
            trusted_setup.g2_points().as_slice(),
        )
        .map_err(KzgError::CKzgError)?;

        if trusted_setup.field_elements_per_blob() != E::field_elements_per_blob() {
            return Err(KzgError::TrustedSetupMismatch);
        }
        if trusted_setup.bytes_per_blob() != E::bytes_per_blob() {
            return Err(KzgError::TrustedSetupMismatch);
        }
        Ok(Self {
            trusted_setup,
            _phantom: PhantomData,
        })
    }

    /// Compute the kzg proof given a blob and its kzg commitment.
    pub fn compute_blob_kzg_proof(
        &self,
        blob: &Blob<E>,
        kzg_commitment: KzgCommitment,
    ) -> Result<KzgProof, KzgError> {
        self.trusted_setup
            .compute_blob_kzg_proof(blob, &kzg_commitment.into())
            .map_err(KzgError::CKzgError)
            .map(|proof| KzgProof(proof.to_bytes().into_inner()))
    }

    /// Verify a kzg proof given the blob, kzg commitment and kzg proof.
    pub fn verify_blob_kzg_proof(
        &self,
        blob: &Blob<E>,
        kzg_commitment: KzgCommitment,
        kzg_proof: KzgProof,
    ) -> Result<bool, KzgError> {
        self.trusted_setup
            .verify_blob_kzg_proof(blob, &kzg_commitment.into(), &kzg_proof.into())
            .map_err(KzgError::CKzgError)
    }

    /// Verify a batch of blob commitment proof triplets.
    ///
    /// Note: This method is slightly faster than calling `Self::verify_blob_kzg_proof` in a loop sequentially.
    /// TODO(pawan): test performance against a parallelized rayon impl.
    pub fn verify_blob_kzg_proof_batch(
        &self,
        blobs: &[Blob<E>],
        kzg_commitments: &[KzgCommitment],
        kzg_proofs: &[KzgProof],
    ) -> Result<bool, KzgError> {
        let commitments_bytes = kzg_commitments
            .iter()
            .map(|comm| Bytes48::from(*comm))
            .collect::<Vec<_>>();

        let proofs_bytes = kzg_proofs
            .iter()
            .map(|proof| Bytes48::from(*proof))
            .collect::<Vec<_>>();

        let blobs = blobs.iter().map(|blob| blob.as_ref()).collect::<Vec<_>>();

        self.trusted_setup
            .verify_blob_kzg_proof_batch(blobs.as_ref(), &commitments_bytes, &proofs_bytes)
            .map_err(KzgError::CKzgError)
    }

    /// Converts a blob to a kzg commitment.
    pub fn blob_to_kzg_commitment(&self, blob: &Blob<E>) -> Result<KzgCommitment, KzgError> {
        self.trusted_setup
            .blob_to_kzg_commitment(blob)
            .map_err(KzgError::CKzgError)
            .map(|commitment| KzgCommitment(commitment.to_bytes().into_inner()))
    }

    /// Computes the kzg proof for a given `blob` and an evaluation point `z`
    pub fn compute_kzg_proof(
        &self,
        blob: &Blob<E>,
        z: Hash256,
    ) -> Result<(KzgProof, Hash256), KzgError> {
        self.trusted_setup
            .compute_kzg_proof(blob, &z.0.into())
            .map_err(KzgError::CKzgError)
            .map(|(proof, y)| (KzgProof(proof.to_bytes().into_inner()), y))
            .map(|(proof, z)| (proof, Hash256::from_slice(&z.to_vec())))
    }

    /// Verifies a `kzg_proof` for a `kzg_commitment` that evaluating a polynomial at `z` results in `y`
    pub fn verify_kzg_proof(
        &self,
        kzg_commitment: KzgCommitment,
        z: Hash256,
        y: Hash256,
        kzg_proof: KzgProof,
    ) -> Result<bool, KzgError> {
        self.trusted_setup
            .verify_kzg_proof(
                &kzg_commitment.into(),
                &z.0.into(),
                &y.0.into(),
                &kzg_proof.into(),
            )
            .map_err(KzgError::CKzgError)
    }
}
