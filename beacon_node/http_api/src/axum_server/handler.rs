use axum::extract::{Query, RawQuery};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::{extract::Path, extract::State, Json};
use beacon_chain::attestation_verification::Error as AttnError;
use beacon_chain::validator_monitor::timestamp_now;
use lighthouse_network::{NetworkGlobals, PubsubMessage};
use network::NetworkMessage;
use slog::{debug, error};
use slot_clock::SlotClock;
use std::{str::FromStr, sync::Arc};
use tokio::sync::mpsc::UnboundedSender;
use types::{
    Attestation, ConfigAndPreset, Epoch, SignedBlindedBeaconBlock, SyncCommitteeMessage, SyncDuty,
};

use crate::state_id::StateId;
use crate::{attester_duties, proposer_duties, sync_committees};
use crate::{publish_blocks, publish_pubsub_message, Context, ProvenancedBlock};
use beacon_chain::{BeaconChain, BeaconChainTypes};
use eth2::types::{
    self as api_types, BroadcastValidation, PublishBlockRequest, SyncingData, ValidatorBalanceData,
    ValidatorBalancesQuery, ValidatorIndexData,
};
use eth2::types::{ExecutionOptimisticFinalizedResponse, GenericResponse, GenesisData, RootData};

use super::error::Error as HandlerError;

/// Returns the `BeaconChain` otherwise returns an error
fn chain_filter<T: BeaconChainTypes>(
    ctx: &Context<T>,
) -> Result<Arc<BeaconChain<T>>, HandlerError> {
    if let Some(chain) = &ctx.chain {
        Ok(chain.clone())
    } else {
        return Err(HandlerError::Other(
            "beacon chain not available, genesis not completed".to_string(),
        ));
    }
}

/// Returns the `Network` channel sender otherwise returns an error
fn network_tx<T: BeaconChainTypes>(
    ctx: &Context<T>,
) -> Result<UnboundedSender<NetworkMessage<T::EthSpec>>, HandlerError> {
    if let Some(network_tx) = &ctx.network_senders {
        Ok(network_tx.network_send())
    } else {
        return Err(HandlerError::Other(
            "The networking stack has not yet started (network_tx).".to_string(),
        ));
    }
}

/// Returns the network globals otherwise returns an error
fn network_globals<T: BeaconChainTypes>(
    ctx: &Context<T>,
) -> Result<Arc<NetworkGlobals<T::EthSpec>>, HandlerError> {
    if let Some(globals) = &ctx.network_globals {
        Ok(globals.clone())
    } else {
        return Err(HandlerError::Other(
            "The networking stack has not yet started (network_globals).".to_string(),
        ));
    }
}

pub async fn catch_all() -> &'static str {
    dbg!("yaha aaya");
    "whoaaa"
}

/// GET beacon/genesis
pub async fn get_beacon_genesis<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
) -> Result<Json<GenericResponse<GenesisData>>, HandlerError> {
    let chain = chain_filter(&ctx)?;
    let genesis_data = GenesisData {
        genesis_time: chain.genesis_time,
        genesis_validators_root: chain.genesis_validators_root,
        genesis_fork_version: chain.spec.genesis_fork_version,
    };
    Ok(Json(GenericResponse::from(genesis_data)))
}

/// GET beacon/states/{state_id}/root
pub async fn get_beacon_state_root<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
    Path(state_id): Path<String>,
) -> Result<Json<ExecutionOptimisticFinalizedResponse<RootData>>, HandlerError> {
    let chain = chain_filter(&ctx)?;
    let state_id = StateId::from_str(&state_id)?;
    let (root, execution_optimistic, finalized) = state_id.root(&chain)?;
    Ok(GenericResponse::from(api_types::RootData::from(root)))
        .map(|resp| resp.add_execution_optimistic_finalized(execution_optimistic, finalized))
        .map(Json)
}

/// GET beacon/states/{state_id}/fork
pub async fn get_beacon_state_fork<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
    Path(state_id): Path<String>,
) -> Result<Json<ExecutionOptimisticFinalizedResponse<api_types::Fork>>, HandlerError> {
    let chain = chain_filter(&ctx)?;
    let state_id = StateId::from_str(&state_id)?;
    let (fork, execution_optimistic, finalized) =
        state_id.fork_and_execution_optimistic_and_finalized(&chain)?;
    Ok(GenericResponse::from(api_types::Fork::from(fork)))
        .map(|resp| resp.add_execution_optimistic_finalized(execution_optimistic, finalized))
        .map(Json)
}

/// GET beacon/states/{state_id}/finality_checkpoints
pub async fn get_beacon_state_finality_checkpoints<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
    Path(state_id): Path<String>,
) -> Result<
    Json<ExecutionOptimisticFinalizedResponse<api_types::FinalityCheckpointsData>>,
    HandlerError,
> {
    let chain = chain_filter(&ctx)?;
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
    State(ctx): State<Arc<Context<T>>>,
    Path(state_id): Path<String>,
    RawQuery(query): RawQuery, // Should probably have a cleaner solution for this
) -> Result<Json<ExecutionOptimisticFinalizedResponse<Vec<ValidatorBalanceData>>>, HandlerError> {
    let chain = chain_filter(&ctx)?;
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

/// TODO: investigate merging ssz and json handlers
/// beacon/blinded_blocks
pub async fn post_beacon_blinded_blocks_json<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
    _header_map: HeaderMap,
    Json(block_contents): Json<Arc<SignedBlindedBeaconBlock<T::EthSpec>>>,
) -> Result<Response, HandlerError> {
    let chain = chain_filter(&ctx)?;
    let network_tx = network_tx(&ctx)?;
    let log = ctx.log.clone();
    let _warp_response = publish_blocks::publish_blinded_block(
        block_contents,
        chain,
        &network_tx,
        log,
        BroadcastValidation::default(),
        ctx.config.duplicate_block_status_code,
    )
    .await?;
    Ok(Response::new(().into()))
}

/// v2/beacon/blinded_blocks
pub async fn post_beacon_blinded_blocks_json_v2<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
    _header_map: HeaderMap,
    Query(validation_level): Query<api_types::BroadcastValidationQuery>,
    Json(block_contents): Json<Arc<SignedBlindedBeaconBlock<T::EthSpec>>>,
) -> Result<Response, HandlerError> {
    let chain = chain_filter(&ctx)?;
    let network_tx = network_tx(&ctx)?;
    let log = ctx.log.clone();
    let _warp_response = publish_blocks::publish_blinded_block(
        block_contents,
        chain,
        &network_tx,
        log,
        validation_level.broadcast_validation,
        ctx.config.duplicate_block_status_code,
    )
    .await?;
    Ok(Response::new(().into()))
}

/// TODO: investigate merging ssz and json handlers
/// beacon/blinded_blocks
pub async fn post_beacon_blocks_json<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
    _header_map: HeaderMap,
    Json(block_contents): Json<PublishBlockRequest<T::EthSpec>>,
) -> Result<Response, HandlerError> {
    let chain = chain_filter(&ctx)?;
    let network_tx = network_tx(&ctx)?;
    let log = ctx.log.clone();
    let _warp_response = publish_blocks::publish_block(
        None,
        ProvenancedBlock::local(block_contents),
        chain,
        &network_tx,
        log,
        BroadcastValidation::default(),
        ctx.config.duplicate_block_status_code,
    )
    .await?;
    Ok(Response::new(().into()))
}

/// TODO: investigate merging ssz and json handlers
/// beacon/blinded_blocks
pub async fn post_beacon_blocks_json_v2<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
    _header_map: HeaderMap,
    Query(validation_level): Query<api_types::BroadcastValidationQuery>,
    Json(block_contents): Json<PublishBlockRequest<T::EthSpec>>,
) -> Result<Response, HandlerError> {
    let chain = chain_filter(&ctx)?;
    let network_tx = network_tx(&ctx)?;
    let log = ctx.log.clone();
    let _warp_response = publish_blocks::publish_block(
        None,
        ProvenancedBlock::local(block_contents),
        chain,
        &network_tx,
        log,
        validation_level.broadcast_validation,
        ctx.config.duplicate_block_status_code,
    )
    .await?;
    Ok(Response::new(().into()))
}

/// POST beacon/pool/attestations
pub async fn post_beacon_pool_attestations<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
    _header_map: HeaderMap,
    Json(attestations): Json<Vec<Attestation<T::EthSpec>>>,
) -> Result<(), HandlerError> {
    let chain = chain_filter(&ctx)?;
    let network_tx = network_tx(&ctx)?;
    let log = ctx.log.clone();

    let seen_timestamp = timestamp_now();
    let mut failures = Vec::new();
    let mut num_already_known = 0;

    for (index, attestation) in attestations.as_slice().iter().enumerate() {
        let attestation = match chain.verify_unaggregated_attestation_for_gossip(attestation, None)
        {
            Ok(attestation) => attestation,
            Err(AttnError::PriorAttestationKnown { .. }) => {
                num_already_known += 1;

                // Skip to the next attestation since an attestation for this
                // validator is already known in this epoch.
                //
                // There's little value for the network in validating a second
                // attestation for another validator since it is either:
                //
                // 1. A duplicate.
                // 2. Slashable.
                // 3. Invalid.
                //
                // We are likely to get duplicates in the case where a VC is using
                // fallback BNs. If the first BN actually publishes some/all of a
                // batch of attestations but fails to respond in a timely fashion,
                // the VC is likely to try publishing the attestations on another
                // BN. That second BN may have already seen the attestations from
                // the first BN and therefore indicate that the attestations are
                // "already seen". An attestation that has already been seen has
                // been published on the network so there's no actual error from
                // the perspective of the user.
                //
                // It's better to prevent slashable attestations from ever
                // appearing on the network than trying to slash validators,
                // especially those validators connected to the local API.
                //
                // There might be *some* value in determining that this attestation
                // is invalid, but since a valid attestation already it exists it
                // appears that this validator is capable of producing valid
                // attestations and there's no immediate cause for concern.
                continue;
            }
            Err(e) => {
                error!(log,
                    "Failure verifying attestation for gossip";
                    "error" => ?e,
                    "request_index" => index,
                    "committee_index" => attestation.data.index,
                    "attestation_slot" => attestation.data.slot,
                );
                failures.push(api_types::Failure::new(
                    index,
                    format!("Verification: {:?}", e),
                ));
                // skip to the next attestation so we do not publish this one to gossip
                continue;
            }
        };

        // Notify the validator monitor.
        chain
            .validator_monitor
            .read()
            .register_api_unaggregated_attestation(
                seen_timestamp,
                attestation.indexed_attestation(),
                &chain.slot_clock,
            );

        publish_pubsub_message(
            &network_tx,
            PubsubMessage::Attestation(Box::new((
                attestation.subnet_id(),
                attestation.attestation().clone(),
            ))),
        )?;

        let committee_index = attestation.attestation().data.index;
        let slot = attestation.attestation().data.slot;

        if let Err(e) = chain.apply_attestation_to_fork_choice(&attestation) {
            error!(log,
                "Failure applying verified attestation to fork choice";
                "error" => ?e,
                "request_index" => index,
                "committee_index" => committee_index,
                "slot" => slot,
            );
            failures.push(api_types::Failure::new(
                index,
                format!("Fork choice: {:?}", e),
            ));
        };

        if let Err(e) = chain.add_to_naive_aggregation_pool(&attestation) {
            error!(log,
                "Failure adding verified attestation to the naive aggregation pool";
                "error" => ?e,
                "request_index" => index,
                "committee_index" => committee_index,
                "slot" => slot,
            );
            failures.push(api_types::Failure::new(
                index,
                format!("Naive aggregation pool: {:?}", e),
            ));
        }
    }

    if num_already_known > 0 {
        debug!(
            log,
            "Some unagg attestations already known";
            "count" => num_already_known
        );
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(warp_utils::reject::indexed_bad_request(
            "error processing attestations".to_string(),
            failures,
        ))
        .map_err(HandlerError::Warp)
    }
}

/// POST beacon/pool/sync_committees
pub async fn post_beacon_pool_sync_committees<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
    _header_map: HeaderMap,
    Json(signatures): Json<Vec<SyncCommitteeMessage>>,
) -> Result<(), HandlerError> {
    let chain = chain_filter(&ctx)?;
    let network_tx = network_tx(&ctx)?;
    let log = ctx.log.clone();

    sync_committees::process_sync_committee_signatures(signatures, network_tx, &chain, log)?;
    Ok(())
}

/// GET node/syncing
pub async fn get_node_syncing<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
) -> Result<Json<GenericResponse<SyncingData>>, HandlerError> {
    let chain = chain_filter(&ctx)?;
    let network_globals = network_globals(&ctx)?;

    let el_offline = if let Some(el) = &chain.execution_layer {
        el.is_offline_or_erroring().await
    } else {
        true
    };

    let head_slot = chain.canonical_head.cached_head().head_slot();
    let current_slot = chain.slot_clock.now_or_genesis().ok_or_else(|| {
        warp_utils::reject::custom_server_error("Unable to read slot clock".into())
    })?;

    // Taking advantage of saturating subtraction on slot.
    let sync_distance = current_slot - head_slot;

    let is_optimistic = chain
        .is_optimistic_or_invalid_head()
        .map_err(warp_utils::reject::beacon_chain_error)?;

    let syncing_data = api_types::SyncingData {
        is_syncing: network_globals.sync_state.read().is_syncing(),
        is_optimistic: Some(is_optimistic),
        el_offline: Some(el_offline),
        head_slot,
        sync_distance,
    };

    Ok(api_types::GenericResponse::from(syncing_data)).map(Json)
}

/// GET node/syncing
pub async fn get_config_spec<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
) -> Result<Json<GenericResponse<ConfigAndPreset>>, HandlerError> {
    let chain = chain_filter(&ctx)?;
    let spec_fork_name = ctx.config.spec_fork_name;
    let config_and_preset =
        ConfigAndPreset::from_chain_spec::<T::EthSpec>(&chain.spec, spec_fork_name);
    Ok(api_types::GenericResponse::from(config_and_preset)).map(Json)
}

/// POST validator/duties/attester/{epoch}
pub async fn post_validator_duties_attester<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
    Path(epoch): Path<Epoch>,
    Json(indices): Json<ValidatorIndexData>,
) -> Result<Json<api_types::DutiesResponse<Vec<api_types::AttesterData>>>, HandlerError> {
    let chain = chain_filter(&ctx)?;
    attester_duties::attester_duties(epoch, &indices.0, &chain)
        .map_err(HandlerError::Warp)
        .map(Json)
}

/// POST validator/duties/proposer/{epoch}
pub async fn post_validator_duties_proposer<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
    Path(epoch): Path<Epoch>,
) -> Result<Json<api_types::DutiesResponse<Vec<api_types::ProposerData>>>, HandlerError> {
    let chain = chain_filter(&ctx)?;
    let log = ctx.log.clone();
    proposer_duties::proposer_duties(epoch, &chain, &log)
        .map_err(HandlerError::Warp)
        .map(Json)
}

/// POST validator/duties/sync/{epoch}
pub async fn post_validator_duties_sync<T: BeaconChainTypes>(
    State(ctx): State<Arc<Context<T>>>,
    Path(epoch): Path<Epoch>,
    Json(indices): Json<ValidatorIndexData>,
) -> Result<Json<api_types::ExecutionOptimisticResponse<Vec<SyncDuty>>>, HandlerError> {
    let chain = chain_filter(&ctx)?;
    sync_committees::sync_committee_duties(epoch, &indices.0, &chain)
        .map_err(HandlerError::Warp)
        .map(Json)
}
