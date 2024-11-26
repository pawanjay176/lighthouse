use axum::{body::to_bytes, body::Body, Extension, Router};
use beacon_chain::test_utils::{BeaconChainHarness, EphemeralHarnessType};
use eth2::types::{GenericResponse, GenesisData};
use http_api::{
    axum_server::handler::{get_beacon_genesis, get_beacon_genesis_without_task_spawner},
    axum_server::task_spawner::{create_task_spawner, create_trace_layer},
    test_utils::create_api_server as create_warp_server,
    Context, Error as ApiError,
};

use hyper::Request;
use lighthouse_network::NetworkGlobals;
use slog::{o, Discard, Logger};
use std::future::Future;
use std::time::Duration;
use std::{sync::Arc, time::Instant};
use task_executor::test_utils::TestRuntime;
use tower::ServiceExt;
use types::MainnetEthSpec;
use warp::{Filter, Rejection, Reply};

type E = MainnetEthSpec;
type T = EphemeralHarnessType<E>;

const MAX_BODY_SIZE: usize = 1024 * 1024;
const BEACON_GENESIS_V1_ENDPOINT: &str = "/eth/v1/beacon/genesis";

async fn setup_test_context() -> Arc<Context<T>> {
    let harness = BeaconChainHarness::builder(E::default())
        .default_spec()
        .deterministic_keypairs(8)
        .fresh_ephemeral_store()
        .build();

    let log = Logger::root(Discard, o!());
    let network_globals = Arc::new(NetworkGlobals::new_test_globals(vec![], &log));

    let mut config = http_api::Config::default();
    config.enabled = true;

    Arc::new(Context {
        chain: Some(harness.chain.clone()),
        config,
        network_globals: Some(network_globals),
        network_senders: None,
        beacon_processor_send: None,
        beacon_processor_reprocess_send: None,
        eth1_service: None,
        log,
        sse_logging_components: None,
    })
}

async fn setup_warp_routes(
    ctx: Arc<Context<T>>,
) -> Result<impl Filter<Extract = impl Reply, Error = Rejection> + Clone, ApiError> {
    let runtime = TestRuntime::default();

    let api_server = create_warp_server(
        ctx.chain.clone().expect("Chain should exist"),
        &runtime,
        ctx.log.clone(),
    )
    .await;

    let routes = warp::path!("eth" / "v1" / "beacon" / "genesis")
        .and(warp::get())
        .and_then(move || {
            let chain = api_server.ctx.chain.clone().expect("Chain should exist");
            async move {
                let genesis_data = GenesisData {
                    genesis_time: chain.genesis_time,
                    genesis_validators_root: chain.genesis_validators_root,
                    genesis_fork_version: chain.spec.genesis_fork_version,
                };
                Ok::<_, Rejection>(warp::reply::json(&GenericResponse::from(genesis_data)))
            }
        });

    Ok(routes)
}

// Setup function for endpoint without task spawner
fn setup_axum_genesis_endpoint_without_task_spawner(ctx: Arc<Context<T>>) -> Router {
    Router::new()
        .route(
            BEACON_GENESIS_V1_ENDPOINT,
            axum::routing::get(get_beacon_genesis_without_task_spawner),
        )
        .with_state(ctx)
}

// Setup function for endpoint with initialized task spawner
fn setup_axum_genesis_endpoint_with_task_spawner_initialize_first(ctx: Arc<Context<T>>) -> Router {
    let spawner = create_task_spawner(&ctx);

    Router::new()
        .route(
            BEACON_GENESIS_V1_ENDPOINT,
            axum::routing::get(get_beacon_genesis_without_task_spawner),
        )
        .layer(Extension(spawner))
        .layer(create_trace_layer())
        .with_state(ctx)
}

// Setup function for endpoint with task spawner for every request
fn setup_axum_genesis_endpoint_with_task_spawner_for_every_endpoint(
    ctx: Arc<Context<T>>,
) -> Router {
    Router::new()
        .route(
            BEACON_GENESIS_V1_ENDPOINT,
            axum::routing::get(get_beacon_genesis),
        )
        .with_state(ctx)
}

// Helper function to create requests
// For axum endpoint
fn make_request() -> Request<Body> {
    Request::builder()
        .uri(BEACON_GENESIS_V1_ENDPOINT)
        .method("GET")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn test_endpoint_response_verification() {
    println!("Starting response verification...\n");

    // Setup endpoints
    let ctx = setup_test_context().await;
    let warp_filter = setup_warp_routes(ctx.clone())
        .await
        .expect("should create warp routes");
    let axum_app_no_spawner = setup_axum_genesis_endpoint_without_task_spawner(ctx.clone());
    let axum_app_spawner_initialize_first =
        setup_axum_genesis_endpoint_with_task_spawner_initialize_first(ctx.clone());
    let axum_app_spawner_every =
        setup_axum_genesis_endpoint_with_task_spawner_for_every_endpoint(ctx);

    // Get responses
    let warp_response = warp::test::request()
        .method("GET")
        .path(BEACON_GENESIS_V1_ENDPOINT)
        .reply(&warp_filter)
        .await;

    let axum_response_no_spawner = axum_app_no_spawner
        .clone()
        .oneshot(make_request())
        .await
        .unwrap();

    let axum_response_spawner_init = axum_app_spawner_initialize_first
        .clone()
        .oneshot(make_request())
        .await
        .unwrap();

    let axum_response_spawner_every = axum_app_spawner_every
        .clone()
        .oneshot(make_request())
        .await
        .unwrap();

    // Get warp data as baseline
    let warp_body: GenericResponse<GenesisData> =
        serde_json::from_slice(warp_response.body()).unwrap();
    let warp_status = warp_response.status().as_u16();

    // Handle no-spawner response
    let axum_no_spawner_status = axum_response_no_spawner.status().as_u16();
    let axum_body_no_spawner_bytes = to_bytes(axum_response_no_spawner.into_body(), MAX_BODY_SIZE)
        .await
        .unwrap();
    let axum_body_no_spawner: GenericResponse<GenesisData> =
        serde_json::from_slice(&axum_body_no_spawner_bytes).unwrap();

    // Handle spawner initialized first response
    let axum_spawner_init_status = axum_response_spawner_init.status().as_u16();
    let axum_body_spawner_init_bytes =
        to_bytes(axum_response_spawner_init.into_body(), MAX_BODY_SIZE)
            .await
            .unwrap();
    let axum_body_spawner_init: GenericResponse<GenesisData> =
        serde_json::from_slice(&axum_body_spawner_init_bytes).unwrap();

    // Handle spawner every request response
    let axum_spawner_every_status = axum_response_spawner_every.status().as_u16();
    let axum_body_spawner_every_bytes =
        to_bytes(axum_response_spawner_every.into_body(), MAX_BODY_SIZE)
            .await
            .unwrap();
    let axum_body_spawner_every: GenericResponse<GenesisData> =
        serde_json::from_slice(&axum_body_spawner_every_bytes).unwrap();

    println!("\nVerifying response consistency across implementations:");
    println!("----------------------------------------");

    // Verify against no spawner implementation
    assert_eq!(
        warp_body.data.genesis_time, axum_body_no_spawner.data.genesis_time,
        "Genesis time mismatch between Warp and Axum without task spawner"
    );
    assert_eq!(
        warp_body.data.genesis_validators_root, axum_body_no_spawner.data.genesis_validators_root,
        "Validators root mismatch between Warp and Axum without task spawner"
    );
    assert_eq!(
        warp_body.data.genesis_fork_version, axum_body_no_spawner.data.genesis_fork_version,
        "Fork version mismatch between Warp and Axum without task spawner"
    );
    assert_eq!(
        warp_status, axum_no_spawner_status,
        "Status code mismatch between Warp and Axum without task spawner"
    );
    println!("✓ All checks passed for Axum without task spawner");

    // Verify against spawner initialized first implementation
    assert_eq!(
        warp_body.data.genesis_time, axum_body_spawner_init.data.genesis_time,
        "Genesis time mismatch between Warp and Axum with initialized task spawner"
    );
    assert_eq!(
        warp_body.data.genesis_validators_root, axum_body_spawner_init.data.genesis_validators_root,
        "Validators root mismatch between Warp and Axum with initialized task spawner"
    );
    assert_eq!(
        warp_body.data.genesis_fork_version, axum_body_spawner_init.data.genesis_fork_version,
        "Fork version mismatch between Warp and Axum with initialized task spawner"
    );
    assert_eq!(
        warp_status, axum_spawner_init_status,
        "Status code mismatch between Warp and Axum with initialized task spawner"
    );
    println!("✓ All checks passed for Axum with initialized task spawner");

    // Verify against spawner every request implementation
    assert_eq!(
        warp_body.data.genesis_time, axum_body_spawner_every.data.genesis_time,
        "Genesis time mismatch between Warp and Axum with task spawner every request"
    );
    assert_eq!(
        warp_body.data.genesis_validators_root,
        axum_body_spawner_every.data.genesis_validators_root,
        "Validators root mismatch between Warp and Axum with task spawner every request"
    );
    assert_eq!(
        warp_body.data.genesis_fork_version, axum_body_spawner_every.data.genesis_fork_version,
        "Fork version mismatch between Warp and Axum with task spawner every request"
    );
    assert_eq!(
        warp_status, axum_spawner_every_status,
        "Status code mismatch between Warp and Axum with task spawner every request"
    );
    println!("✓ All checks passed for Axum with task spawner every request");

    println!("----------------------------------------");
    println!("✓ Response verification passed successfully!\n");
}

#[tokio::test]
async fn test_endpoint_performance() {
    println!("Starting performance benchmarking...\n");

    const NUM_REQUESTS: u32 = 100;
    const WARMUP_REQUESTS: u32 = 5;

    // Setup endpoints
    let ctx = setup_test_context().await;
    let warp_filter = setup_warp_routes(ctx.clone())
        .await
        .expect("should create warp routes");
    let axum_app_no_spawner = setup_axum_genesis_endpoint_without_task_spawner(ctx.clone());
    let axum_app_spawner_init =
        setup_axum_genesis_endpoint_with_task_spawner_initialize_first(ctx.clone());
    let axum_app_spawner_every =
        setup_axum_genesis_endpoint_with_task_spawner_for_every_endpoint(ctx);

    // Warmup
    println!("\nPerforming warmup requests...");
    for _ in 0..WARMUP_REQUESTS {
        warp::test::request()
            .method("GET")
            .path(BEACON_GENESIS_V1_ENDPOINT)
            .reply(&warp_filter)
            .await;
        axum_app_no_spawner
            .clone()
            .oneshot(make_request())
            .await
            .unwrap();
        axum_app_spawner_init
            .clone()
            .oneshot(make_request())
            .await
            .unwrap();
        axum_app_spawner_every
            .clone()
            .oneshot(make_request())
            .await
            .unwrap();
    }

    // Benchmark each implementation
    let warp_duration = helper_test_performance("Warp", NUM_REQUESTS, || async {
        warp::test::request()
            .method("GET")
            .path(BEACON_GENESIS_V1_ENDPOINT)
            .reply(&warp_filter)
            .await;
    })
    .await;

    let axum_no_spawner_duration =
        helper_test_performance("Axum without task spawner", NUM_REQUESTS, || async {
            axum_app_no_spawner
                .clone()
                .oneshot(make_request())
                .await
                .unwrap();
        })
        .await;

    let axum_spawner_init_duration = helper_test_performance(
        "Axum with initialized task spawner",
        NUM_REQUESTS,
        || async {
            axum_app_spawner_init
                .clone()
                .oneshot(make_request())
                .await
                .unwrap();
        },
    )
    .await;

    let axum_spawner_every_duration = helper_test_performance(
        "Axum with task spawner every request",
        NUM_REQUESTS,
        || async {
            axum_app_spawner_every
                .clone()
                .oneshot(make_request())
                .await
                .unwrap();
        },
    )
    .await;

    // Print results
    println!(
        "\nPerformance Comparison ({} requests per implementation):",
        NUM_REQUESTS
    );
    println!("----------------------------------------");
    println!("Framework & Configuration         | Total Time | Avg Time/Request | vs. Warp");
    println!("----------------------------------------");
    println!(
        "Warp                             | {:>9?} | {:>14?} |  baseline",
        warp_duration,
        warp_duration / NUM_REQUESTS
    );
    println!(
        "Axum (no spawner)                | {:>9?} | {:>14?} |  {:.2}x slower",
        axum_no_spawner_duration,
        axum_no_spawner_duration / NUM_REQUESTS,
        axum_no_spawner_duration.as_secs_f64() / warp_duration.as_secs_f64()
    );
    println!(
        "Axum (initialized task spawner)  | {:>9?} | {:>14?} |  {:.2}x slower",
        axum_spawner_init_duration,
        axum_spawner_init_duration / NUM_REQUESTS,
        axum_spawner_init_duration.as_secs_f64() / warp_duration.as_secs_f64()
    );
    println!(
        "Axum (spawner every request)     | {:>9?} | {:>14?} |  {:.2}x slower",
        axum_spawner_every_duration,
        axum_spawner_every_duration / NUM_REQUESTS,
        axum_spawner_every_duration.as_secs_f64() / warp_duration.as_secs_f64()
    );
    println!("----------------------------------------");
}

async fn helper_test_performance<F, Fut>(name: &str, num_requests: u32, test_fn: F) -> Duration
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()>,
{
    println!("\nTesting {} endpoint...", name);
    let start = Instant::now();
    for i in 1..=num_requests {
        test_fn().await;
        if i % 20 == 0 {
            print!(".");
        }
    }
    let duration = start.elapsed();
    println!(" Done!");
    duration
}
