use axum::{
    http::{header::CONTENT_TYPE, HeaderValue, Method},
    routing::{get, post},
    Router,
};
use beacon_chain::BeaconChainTypes;

mod error;
mod handler;
use super::Context;

use slog::info;
use std::net::IpAddr;
use std::sync::Arc;

use std::future::{Future, IntoFuture};
use std::net::{SocketAddr, TcpListener};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    trace::{DefaultOnRequest, TraceLayer},
};

pub async fn serve<T: BeaconChainTypes>(
    ctx: Arc<Context<T>>,
    shutdown: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<(), String> {
    let server = start_server(ctx, shutdown)?;
    let _ = server.await;

    Ok(())
}

// Custom `on_request` function for logging
fn on_request() -> DefaultOnRequest {
    DefaultOnRequest::new()
}

pub fn routes<T: BeaconChainTypes>(ctx: Arc<Context<T>>) -> Router {
    Router::new()
        .route(
            "/eth/v1/beacon/genesis",
            get(handler::get_beacon_genesis::<T>),
        )
        .route(
            "/eth/v1/beacon/blocks/:block_id/root",
            get(handler::get_beacon_blocks_root::<T>),
        )
        .route(
            "/eth/v1/beacon/states/:state_id/root",
            get(handler::get_beacon_state_root::<T>),
        )
        .route(
            "/eth/v1/beacon/states/:state_id/fork",
            get(handler::get_beacon_state_fork::<T>),
        )
        .route(
            "/eth/v1/beacon/states/:state_id/finality_checkpoints",
            get(handler::get_beacon_state_finality_checkpoints::<T>),
        )
        .route(
            "/eth/v1/beacon/states/:state_id/validator_balances",
            get(handler::get_beacon_state_validator_balances::<T>),
        )
        .route(
            "/eth/v1/beacon/states/:state_id/validators/:validator_id",
            get(handler::get_beacon_state_validators_id::<T>),
        )
        .route(
            "/eth/v1/beacon/blinded_blocks",
            post(handler::post_beacon_blinded_blocks_json::<T>),
        )
        .route(
            "/eth/v2/beacon/blinded_blocks",
            post(handler::post_beacon_blinded_blocks_json_v2::<T>),
        )
        .route(
            "/eth/v1/beacon/blocks",
            post(handler::post_beacon_blocks_json::<T>),
        )
        .route(
            "/eth/v2/beacon/blocks",
            post(handler::post_beacon_blocks_json_v2),
        )
        .route(
            "/eth/v1/beacon/pool/attestations",
            post(handler::post_beacon_pool_attestations::<T>),
        )
        .route(
            "/eth/v1/beacon/pool/sync_committees",
            post(handler::post_beacon_pool_sync_committees::<T>),
        )
        .route("/eth/v1/node/syncing", get(handler::get_node_syncing::<T>))
        .route("/eth/v1/node/version", get(handler::get_node_version))
        .route("/eth/v1/config/spec", get(handler::get_config_spec::<T>))
        .route(
            "/eth/v1/validator/duties/attester/:epoch",
            post(handler::post_validator_duties_attester::<T>),
        )
        .route(
            "/eth/v1/validator/duties/proposer/:epoch",
            get(handler::get_validator_duties_proposer::<T>),
        )
        .route(
            "/eth/v1/validator/duties/sync/:epoch",
            post(handler::post_validator_duties_sync::<T>),
        )
        .route(
            "/eth/v2/validator/blocks/:slot",
            get(handler::get_validator_blocks_v2::<T>),
        )
        .route(
            "/eth/v3/validator/blocks/:slot",
            get(handler::get_validator_blocks_v3::<T>),
        )
        .route(
            "/eth/v1/validator/attestation_data",
            get(handler::get_validator_attestation_data::<T>),
        )
        .route(
            "/eth/v1/validator/aggregate_attestation",
            get(handler::get_validator_aggregate_attestation::<T>),
        )
        .route(
            "/eth/v1/validator/aggregate_and_proofs",
            post(handler::post_validator_aggregate_and_proofs::<T>),
        )
        .route(
            "/eth/v1/validator/beacon_committee_subscriptions",
            post(handler::post_validator_beacon_committee_subscriptions::<T>),
        )
        .route(
            "/eth/v1/validator/sync_committee_subscriptions",
            post(handler::post_validator_sync_committee_subscriptions::<T>),
        )
        .route(
            "/eth/v1/validator/sync_committee_contribution",
            get(handler::get_validator_sync_committee_contribution::<T>),
        )
        .route(
            "/eth/v1/validator/contribution_and_proofs",
            post(handler::post_validator_contribution_and_proofs::<T>),
        )
        .route(
            "/eth/v1/validator/prepare_beacon_proposer",
            post(handler::post_validator_prepare_beacon_proposer::<T>),
        )
        .route("/eth/v1/events", get(handler::get_events::<T>))
        .fallback(handler::catch_all)
        // .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(TraceLayer::new_for_http().on_request(on_request()))
        .with_state(ctx)
}

fn cors_layer(
    allow_origin: Option<String>,
    listen_addr: IpAddr,
    listen_port: u16,
) -> Result<CorsLayer, String> {
    // Configure CORS.
    let origins: AllowOrigin = if let Some(allow_origin) = allow_origin {
        let mut origins: Vec<HeaderValue> = Vec::new();
        for origin in allow_origin.split(",") {
            if origin == "*" {
                return Ok(CorsLayer::new()
                    .allow_methods([Method::GET, Method::POST])
                    .allow_headers([CONTENT_TYPE])
                    .allow_origin(AllowOrigin::any()));
            }
            origins.push(
                origin
                    .parse::<HeaderValue>()
                    .map_err(|e| format!("Invalid origins header: {:?}", e))?,
            );
        }
        origins.into()
    } else {
        let origin = match listen_addr {
            IpAddr::V4(_) => format!("http://{}:{}", listen_addr, listen_port),
            IpAddr::V6(_) => format!("http://[{}]:{}", listen_addr, listen_port),
        };
        vec![origin
            .parse::<HeaderValue>()
            .map_err(|e| format!("Invalid origins header: {:?}", e))?]
        .into()
    };

    let cors_layer = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([CONTENT_TYPE])
        .allow_origin(origins);
    Ok(cors_layer)
}

pub fn start_server<T: BeaconChainTypes>(
    ctx: Arc<Context<T>>,
    shutdown: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<impl Future<Output = Result<(), std::io::Error>> + 'static, String> {
    let config = ctx.config.clone();

    let app = routes(ctx.clone()).layer(cors_layer(
        config.allow_origin,
        config.listen_addr,
        config.listen_port,
    )?);

    let addr = SocketAddr::new(ctx.config.listen_addr, ctx.config.listen_port + 1);
    let listener =
        TcpListener::bind(addr).map_err(|e| format!("Failed to bind to address: {:?}", e))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("Failed to set to non blocking: {:?}", e))?;

    let serve = axum::serve(
        tokio::net::TcpListener::from_std(listener)
            .map_err(|e| format!("Failed to start tcp listener: {:?}", e))?,
        app.into_make_service(),
    );
    let log = ctx.log.clone();

    info!(
        log,
        "Axum http server started";
        "listen_address" => %addr,
    );
    Ok(serve
        .with_graceful_shutdown(async {
            shutdown.await;
        })
        .into_future())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        extract::connect_info::MockConnectInfo,
        http::{self, Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use lighthouse_network::service::api_types;
    use logging::test_logger;
    use serde_json::{json, Value};
    use std::{collections::HashMap, net::SocketAddr};
    use tokio::net::TcpListener;
    use tower::{Service, ServiceExt}; // for `call`, `oneshot`, and `ready`
    use tracing::{info_span, Span};
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    use types::{EthSpec, MainnetEthSpec, SyncCommitteeMessage};

    use super::super::test_utils::InteractiveTester;

    type E = MainnetEthSpec;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_axum_genesis() {
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    // axum logs rejections from built-in extractors with the `axum::rejection`
                    // target, at `TRACE` level. `axum::rejection=trace` enables showing those events
                    "example_tracing_aka_logging=debug,tower_http=debug,axum::rejection=trace"
                        .into()
                }),
            )
            .with(tracing_subscriber::fmt::layer())
            .init();
        let validator_count = 24;
        let spec = E::default_spec();

        let tester = InteractiveTester::<E>::new(Some(spec.clone()), validator_count).await;
        let app = routes(tester.ctx);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/eth/v1/beacon/blocks/head/root")
                    .method("GET")
                    .header("Content-Type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        dbg!(body);
    }

    #[test]
    fn test_query_params() {
        use axum::extract::Query;
        use eth2::types::ValidatorBalancesQuery;
        use http::Uri;
        use serde::Deserialize;
        use std::str::FromStr;

        let query = "topics=head";
        let topics: eth2::types::EventQuery = serde_array_query::from_str(query).unwrap();
        dbg!(&topics);
    }
}
