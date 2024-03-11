use crate::test_utils::TestRandom;
use crate::{Address, EthSpec};
use bls::Signature;
use derivative::Derivative;
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use ssz_types::VariableList;
use test_random_derive::TestRandom;
use tree_hash_derive::TreeHash;

#[derive(
    arbitrary::Arbitrary,
    Debug,
    Clone,
    PartialEq,
    Serialize,
    Deserialize,
    Encode,
    Decode,
    TreeHash,
    TestRandom,
    Derivative,
)]
#[derivative(Hash(bound = "E: EthSpec"))]
#[serde(bound = "E: EthSpec")]
#[arbitrary(bound = "E: EthSpec")]
pub struct SignedInclusionListSummary<E: EthSpec> {
    pub summary: VariableList<Address, E::MaxTransactionsPerInclusionList>,
    pub signature: Signature,
}

impl<E: EthSpec> Default for SignedInclusionListSummary<E> {
    fn default() -> Self {
        Self {
            summary: VariableList::default(),
            signature: Signature::empty(),
        }
    }
}
