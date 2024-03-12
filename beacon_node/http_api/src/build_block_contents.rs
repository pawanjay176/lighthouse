use beacon_chain::{BeaconBlockResponse, BeaconBlockResponseWrapper, BlockProductionError};
use eth2::types::{BlockContentsDeneb, BlockContentsElectra, FullBlockContents, ProduceBlockV3Response};
use types::{EthSpec, ForkName};
type Error = warp::reject::Rejection;

pub fn build_block_contents<E: EthSpec>(
    fork_name: ForkName,
    block_response: BeaconBlockResponseWrapper<E>,
) -> Result<ProduceBlockV3Response<E>, Error> {
    match block_response {
        BeaconBlockResponseWrapper::Blinded(block) => {
            Ok(ProduceBlockV3Response::Blinded(block.block))
        }
        BeaconBlockResponseWrapper::Full(block) => match fork_name {
            ForkName::Base | ForkName::Altair | ForkName::Merge | ForkName::Capella => Ok(
                ProduceBlockV3Response::Full(FullBlockContents::Block(block.block)),
            ),
            ForkName::Deneb => {
                let BeaconBlockResponse {
                    block,
                    state: _,
                    blob_items,
                    inclusion_list: _,
                    execution_payload_value: _,
                    consensus_block_value: _,
                } = block;

                let Some((kzg_proofs, blobs)) = blob_items else {
                    return Err(warp_utils::reject::block_production_error(
                        BlockProductionError::MissingBlobs,
                    ));
                };

                Ok(ProduceBlockV3Response::Full(
                    FullBlockContents::BlockContentsDeneb(BlockContentsDeneb {
                        block,
                        kzg_proofs,
                        blobs,
                    }),
                ))
            },
            ForkName::Electra => {
                let BeaconBlockResponse {
                    block,
                    state: _,
                    blob_items,
                    inclusion_list,
                    execution_payload_value: _,
                    consensus_block_value: _,
                } = block;

                let Some((kzg_proofs, blobs)) = blob_items else {
                    return Err(warp_utils::reject::block_production_error(
                        BlockProductionError::MissingBlobs,
                    ));
                };

                let Some(inclusion_list) = inclusion_list else {
                    return Err(warp_utils::reject::block_production_error(
                        BlockProductionError::MissingInclusionList,
                    ));
                };

                Ok(ProduceBlockV3Response::Full(
                    FullBlockContents::BlockContentsElectra(BlockContentsElectra {
                        block,
                        kzg_proofs,
                        blobs,
                        inclusion_list
                    }),
                ))
            },
        },
    }
}
