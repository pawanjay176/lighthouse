use axum::extract::RawQuery;
use axum::{debug_handler, extract::Path, extract::State, Error, Extension, Json};
use std::{collections::HashMap, str::FromStr, sync::Arc};

use crate::{state_id::StateId, validator};
use beacon_chain::{BeaconChain, BeaconChainTypes};
use eth2::types::{self as api_types, ValidatorBalanceData, ValidatorBalancesQuery, ValidatorId};
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

pub async fn catch_all() -> &'static str {
    dbg!("yaha aaya");
    "whoaaa"
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

/// GET beacon/states/{state_id}/fork
pub async fn get_beacon_state_fork<T: BeaconChainTypes>(
    State(chain_state): State<Arc<ChainState<T>>>,
    Path(state_id): Path<String>,
) -> Result<Json<ExecutionOptimisticFinalizedResponse<api_types::Fork>>, HandlerError> {
    let chain = chain_filter(chain_state)?;
    let state_id = StateId::from_str(&state_id)?;
    let (fork, execution_optimistic, finalized) =
        state_id.fork_and_execution_optimistic_and_finalized(&chain)?;
    Ok(GenericResponse::from(api_types::Fork::from(fork)))
        .map(|resp| resp.add_execution_optimistic_finalized(execution_optimistic, finalized))
        .map(Json)
}

/// GET beacon/states/{state_id}/finality_checkpoints
pub async fn get_beacon_state_finality_checkpoints<T: BeaconChainTypes>(
    State(chain_state): State<Arc<ChainState<T>>>,
    Path(state_id): Path<String>,
) -> Result<
    Json<ExecutionOptimisticFinalizedResponse<api_types::FinalityCheckpointsData>>,
    HandlerError,
> {
    let chain = chain_filter(chain_state)?;
    let state_id = StateId::from_str(&state_id)?;
    let (data, execution_optimistic, finalized) = state_id
        .map_state_and_execution_optimistic_and_finalized(
            &chain,
            |state, execution_optimistic, finalized| {
                Ok((
                    api_types::FinalityCheckpointsData {
                        previous_justified: state.previous_justified_checkpoint(),
                        current_justified: state.current_justified_checkpoint(),
                        finalized: state.finalized_checkpoint(),
                    },
                    execution_optimistic,
                    finalized,
                ))
            },
        )?;
    Ok(api_types::ExecutionOptimisticFinalizedResponse {
        data,
        execution_optimistic: Some(execution_optimistic),
        finalized: Some(finalized),
    })
    .map(Json)
}

/// GET beacon/states/{state_id}/validator_balances?id
pub async fn get_beacon_state_validator_balances<T: BeaconChainTypes>(
    State(chain_state): State<Arc<ChainState<T>>>,
    Path(state_id): Path<String>,
    RawQuery(query): RawQuery, // Should probably have a cleaner solution for this
) -> Result<Json<ExecutionOptimisticFinalizedResponse<Vec<ValidatorBalanceData>>>, HandlerError> {
    let chain = chain_filter(chain_state)?;
    let state_id = StateId::from_str(&state_id)?;
    let validator_queries = if let Some(query_str) = query {
        let validator_queies: ValidatorBalancesQuery = serde_array_query::from_str(&query_str)
            .map_err(|e| {
                HandlerError::Other(format!(
                    "Failed to parse query string: Query string: {} error: {:?}",
                    query_str, e
                ))
            })?;
        validator_queies.id
    } else {
        None
    };
    crate::validators::get_beacon_state_validator_balances(
        state_id,
        chain,
        validator_queries.as_deref(),
    )
    .map_err(HandlerError::Warp)
    .map(Json)
}
