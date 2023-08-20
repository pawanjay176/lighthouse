use super::*;
use kzg::BYTES_PER_COMMITMENT;

impl TestRandom for KzgCommitment {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        KzgCommitment(<[u8; BYTES_PER_COMMITMENT] as TestRandom>::random_for_test(rng))
    }
}
