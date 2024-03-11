use crate::{EthSpec, FullPayload, SignedBeaconBlockElectra, SignedInclusionList};
use derivative::Derivative;
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

#[derive(
    Debug, Clone, Serialize, Deserialize, Decode, Encode, TreeHash, Derivative, arbitrary::Arbitrary,
)]
#[derivative(PartialEq, Hash(bound = "E: EthSpec"))]
#[serde(bound = "E: EthSpec")]
#[arbitrary(bound = "E: EthSpec")]
pub struct SignedBeaconBlockAndInclusionList<E: EthSpec> {
    // TODO(eip7547): In future forks we'll need superstruct
    pub signed_block: SignedBeaconBlockElectra<E, FullPayload<E>>,
    pub signed_inclusion_list: SignedInclusionList<E>,
}
