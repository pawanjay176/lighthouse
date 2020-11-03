use env_logger::{Builder, Env};
use eth2_libp2p::rpc::methods::*;
use eth2_libp2p::Service as LibP2PService;
use eth2_libp2p::{BehaviourEvent, Libp2pEvent, NetworkConfig, Request, Response};
use eth2_libp2p::{Enr, Multiaddr};
use slog::info;
use slog::{o, Drain};
use task_executor::TaskExecutor;

use tempdir::TempDir;
use types::{EnrForkId, MainnetEthSpec};

type E = MainnetEthSpec;

pub fn build_log(level: slog::Level, enabled: bool) -> slog::Logger {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();

    if enabled {
        slog::Logger::root(drain.filter_level(level).fuse(), o!())
    } else {
        slog::Logger::root(drain.filter(|_| false).fuse(), o!())
    }
}
pub fn build_config(port: u16, mut boot_nodes: Vec<Enr>) -> NetworkConfig {
    let mut config = NetworkConfig::default();
    let path = TempDir::new(&format!("libp2p_test{}", port)).unwrap();

    config.libp2p_port = port; // tcp port
    config.discovery_port = port; // udp port
    config.enr_tcp_port = Some(port);
    config.enr_udp_port = Some(port);
    config.enr_address = Some("127.0.0.1".parse().unwrap());
    config.boot_nodes_enr.append(&mut boot_nodes);
    config.network_dir = path.into_path();
    config.disable_discovery = true;
    config
}

#[tokio::main]
async fn main() {
    Builder::from_env(Env::default()).init();
    let bootnodes = vec![];
    let config = build_config(9001, bootnodes);
    let log = build_log(slog::Level::Debug, false);

    // Tokio stuff
    let (_signal, exit) = exit_future::signal();
    let (shutdown_tx, _) = futures::channel::mpsc::channel(1);
    let executor = TaskExecutor::new(
        tokio::runtime::Handle::current(),
        exit,
        log.clone(),
        shutdown_tx,
    );
    let multiaddr: Multiaddr = "/ip4/127.0.0.1/tcp/9000".parse().unwrap();

    // let fork_digest = "0xe7a75d5a";
    // Set enr_fork_id to be the medalla fork id
    let enr_fork_id = EnrForkId::default();

    // Create a libp2p service
    let (_, mut sender): (_, LibP2PService<E>) =
        LibP2PService::new(executor, &config, enr_fork_id, &log)
            .await
            .expect("should build libp2p instance");

    match libp2p::Swarm::dial_addr(&mut sender.swarm, multiaddr.clone()) {
        Ok(()) => info!(log, "Sender dialed receiver"; "address" => format!("{:?}", multiaddr)),
        Err(_) => info!(log, "Dialing failed"),
    };

    // BlocksByRange Request
    let rpc_request = Request::BlocksByRange(BlocksByRangeRequest {
        start_slot: 0,
        count: 5,
        step: 1,
    });

    let mut receiver_peer_id = None;
    // build the sender future
    let sender_future = async {
        loop {
            match sender.next_event().await {
                Libp2pEvent::Behaviour(BehaviourEvent::PeerDialed(peer_id)) => {
                    info!(log, "Sending RPC");
                    receiver_peer_id = Some(peer_id.clone());
                    sender.swarm.send_request(
                        peer_id.clone(),
                        RequestId::Sync(1),
                        rpc_request.clone(),
                    );
                }
                Libp2pEvent::Behaviour(BehaviourEvent::ResponseReceived {
                    peer_id,
                    id: RequestId::Sync(1),
                    response,
                }) => {
                    match response {
                        Response::BlocksByRange(Some(_)) => {
                            info!(log, "Chunk received");
                        }
                        Response::BlocksByRange(None) => {
                            // should be exactly 10 messages before terminating
                            info!(log, "Completed receiving range response");
                            // end the test
                            // return;
                            // sender.swarm.send_request(
                            //     peer_id.clone(),
                            //     RequestId::Sync(10),
                            //     rpc_request.clone(),
                            // );
                        }
                        _ => panic!("Invalid RPC received"),
                    }
                }
                _ => {} // Ignore other behaviour events
            }
        }
    };
    sender_future.await;
}
