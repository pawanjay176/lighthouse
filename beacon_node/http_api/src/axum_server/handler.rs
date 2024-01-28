use axum::{
    debug_handler,
    extract::State,
    extract::{Path, Query},
    Error, Extension, Json,
};
use std::{str::FromStr, sync::Arc};

use crate::state_id::StateId;
use beacon_chain::{BeaconChain, BeaconChainTypes};
use eth2::types as api_types;
use eth2::types::{ExecutionOptimisticFinalizedResponse, GenericResponse, GenesisData, RootData};

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

/// GET beacon/genesis
pub async fn get_beacon_genesis<T: BeaconChainTypes>(
    State(chain_state): State<Arc<ChainState<T>>>,
) -> Result<Json<GenericResponse<GenesisData>>, HandlerError> {
    let chain = chain_filter(chain_state)?;
    let genesis_data = GenesisData {
        genesis_time: chain.genesis_time,
        genesis_validators_root: chain.genesis_validators_root,
        genesis_fork_version: chain.spec.genesis_fork_version,
    };
    Ok(Json(GenericResponse::from(genesis_data)))
}

/// GET beacon/states/{state_id}/root
// #[debug_handler]
pub async fn get_beacon_state_root<T: BeaconChainTypes>(
    State(chain_state): State<Arc<ChainState<T>>>,
    Path(state_id): Path<String>,
) -> Result<Json<ExecutionOptimisticFinalizedResponse<RootData>>, HandlerError> {
    let chain = chain_filter(chain_state)?;
    let state_id = StateId::from_str(&state_id)?;
    let (root, execution_optimistic, finalized) = state_id.root(&chain)?;
    Ok(GenericResponse::from(api_types::RootData::from(root)))
        .map(|resp| resp.add_execution_optimistic_finalized(execution_optimistic, finalized))
        .map(Json)
}
