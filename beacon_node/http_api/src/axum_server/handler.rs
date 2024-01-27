use axum::{
    debug_handler,
    extract::State,
    extract::{Path, Query},
    Error, Extension, Json,
};
use std::sync::Arc;

use beacon_chain::{BeaconChain, BeaconChainTypes};
use eth2::types::GenesisData;

use super::error::Error as HandlerError;
use super::ChainState;

/// Returns the chain state otherwise returns an error
fn chain_filter<T: BeaconChainTypes>(
    chain_state: Arc<ChainState<T>>,
) -> Result<Arc<BeaconChain<T>>, HandlerError> {
    if let Some(chain) = &chain_state.chain {
        // Maybe unnecessary clone here?
        Ok(chain.clone())
    } else {
        return Err(HandlerError::Other(
            "beacon chain not available, genesis not completed".to_string(),
        ));
    }
}

// #[debug_handler]
pub async fn get_beacon_genesis<T: BeaconChainTypes>(
    State(chain_state): State<Arc<ChainState<T>>>,
) -> Result<Json<GenesisData>, HandlerError> {
    let chain = chain_filter(chain_state)?;
    let genesis_data = GenesisData {
        genesis_time: chain.genesis_time,
        genesis_validators_root: chain.genesis_validators_root,
        genesis_fork_version: chain.spec.genesis_fork_version,
    };
    Ok(Json(genesis_data))
}
