use super::*;
use kzg::{KzgProof, BYTES_PER_PROOF};

impl TestRandom for KzgProof {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        let mut bytes = [0; BYTES_PER_PROOF];
        rng.fill_bytes(&mut bytes);
        Self(bytes)
    }
}
