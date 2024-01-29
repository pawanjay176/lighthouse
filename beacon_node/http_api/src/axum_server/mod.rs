use axum::{
    routing::{get, post},
    Error, Router,
};
use beacon_chain::BeaconChainTypes;

mod error;
mod handler;
use super::Context;

use slog::info;
use std::sync::Arc;

use std::future::{Future, IntoFuture};
use std::net::{SocketAddr, TcpListener};
use tower_http::trace::{DefaultOnRequest, TraceLayer};

pub async fn serve<T: BeaconChainTypes>(
    ctx: Arc<Context<T>>,
    shutdown: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<(), Error> {
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
        .route("/beacon/genesis", get(handler::get_beacon_genesis::<T>))
        .route(
            "/beacon/states/:state_id/root",
            get(handler::get_beacon_state_root::<T>),
        )
        .route(
            "/beacon/states/:state_id/fork",
            get(handler::get_beacon_state_fork::<T>),
        )
        .route(
            "/beacon/states/:state_id/finality_checkpoints",
            get(handler::get_beacon_state_finality_checkpoints::<T>),
        )
        .route(
            "/beacon/states/:state_id/validator_balances",
            get(handler::get_beacon_state_validator_balances::<T>),
        )
        .route(
            "/beacon/blinded_blocks",
            post(handler::post_beacon_blinded_blocks_json::<T>),
        )
        .route(
            "/beacon/blinded_blocks",
            post(handler::post_beacon_blinded_blocks_json_v2::<T>),
        )
        .fallback(handler::catch_all)
        // .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(TraceLayer::new_for_http().on_request(on_request()))
        .with_state(ctx)
}

pub fn start_server<T: BeaconChainTypes>(
    ctx: Arc<Context<T>>,
    shutdown: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<impl Future<Output = Result<(), std::io::Error>> + 'static, Error> {
    let app = routes(ctx.clone());

    let addr = SocketAddr::new(ctx.config.listen_addr, ctx.config.listen_port + 1);
    let listener = TcpListener::bind(addr).unwrap();
    listener.set_nonblocking(true).unwrap();

    let serve = axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), app);
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
    use logging::test_logger;
    use serde_json::{json, Value};
    use std::{collections::HashMap, net::SocketAddr};
    use tokio::net::TcpListener;
    use tower::{Service, ServiceExt}; // for `call`, `oneshot`, and `ready`
    use tracing::{info_span, Span};
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    use types::{EthSpec, MainnetEthSpec};

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
        let body = tester
            .harness
            .get_head_block()
            .as_block()
            .clone_as_blinded();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/beacon/blinded_blocks")
                    .method("POST")
                    .header("Content-Type", "application/json")
                    .body(serde_json::to_string(&body).unwrap())
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

        #[derive(Deserialize, Default)]
        struct Temp {
            #[serde(default)]
            id: Vec<u64>,
        }

        let uri: Uri = "http://example.com/path/?id=0,1".parse().unwrap();
        let result: Query<Vec<(String, String)>> = Query::try_from_uri(&uri).unwrap();
        dbg!(&result);
    }
}
