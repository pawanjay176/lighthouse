use crate::{EthSpec, FullPayload, SignedBeaconBlockElectra, SignedInclusionList};
use derivative::Derivative;
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use superstruct::superstruct;
use tree_hash_derive::TreeHash;

#[superstruct(
    variants(Electra),
    variant_attributes(
        derive(
            Debug,
            Clone,
            Serialize,
            Deserialize,
            Encode,
            Decode,
            TreeHash,
            Derivative,
            arbitrary::Arbitrary
        ),
        derivative(PartialEq, Hash(bound = "E: EthSpec")),
        serde(bound = "E: EthSpec"),
        arbitrary(bound = "E: EthSpec"),
    ),
    map_into(BeaconBlock),
    map_ref_into(BeaconBlockRef),
    map_ref_mut_into(BeaconBlockRefMut)
)]
#[derive(
    Debug, Clone, Serialize, Deserialize, Encode, TreeHash, Derivative, arbitrary::Arbitrary,
)]
#[derivative(PartialEq, Hash(bound = "E: EthSpec"))]
#[serde(untagged)]
#[serde(bound = "E: EthSpec")]
#[arbitrary(bound = "E: EthSpec")]
#[tree_hash(enum_behaviour = "transparent")]
#[ssz(enum_behaviour = "transparent")]
pub struct SignedBeaconBlockAndInclusionList<E: EthSpec> {
    #[superstruct(only(Electra), partial_getter(rename = "message_electra"))]
    pub signed_block: SignedBeaconBlockElectra<E, FullPayload<E>>,
    pub signed_inclusion_list: SignedInclusionList<E>,
}
