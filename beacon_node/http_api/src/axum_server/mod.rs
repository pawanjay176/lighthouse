use axum::{
    http::{StatusCode, Uri},
    routing::get,
    Error, Extension, Json, Router,
};
use beacon_chain::{BeaconChain, BeaconChainTypes};

mod handler;
mod error;
use super::Context;

use slog::info;
use std::sync::Arc;

use std::future::{Future, IntoFuture};
use std::net::{SocketAddr, TcpListener};

pub async fn serve<T: BeaconChainTypes>(
    ctx: Arc<Context<T>>,
    shutdown: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<(), Error> {
    let server = start_server(ctx, shutdown)?;
    let _ = server.await;

    Ok(())
}


/// The beacon chain state to share across all handlers
#[derive(Clone)]
pub(crate) struct ChainState<T: BeaconChainTypes> {
    chain: Option<Arc<BeaconChain<T>>>,
}

pub fn start_server<T: BeaconChainTypes>(
    ctx: Arc<Context<T>>,
    shutdown: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<impl Future<Output = Result<(), std::io::Error>> + 'static, Error> {
    let chain_state = Arc::new(ChainState {
        chain: ctx.chain.clone(),
    });

    let mut routes = Router::new().route("/beacon/genesis", get(handler::get_beacon_genesis));
    let app = routes.with_state(chain_state);

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
