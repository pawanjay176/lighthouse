use env_logger::{Builder, Env};
use eth2_libp2p::Enr;
use eth2_libp2p::Service as LibP2PService;
use eth2_libp2p::{BehaviourEvent, Libp2pEvent, NetworkConfig, Response};
use slog::info;
use slog::{o, Drain};
use task_executor::TaskExecutor;

use tempdir::TempDir;
use types::{BeaconBlock, EthSpec, Signature, SignedBeaconBlock};
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
    let config = build_config(9000, bootnodes);
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
    // let fork_digest = "0xe7a75d5a";
    // Set enr_fork_id to be the medalla fork id
    let enr_fork_id = EnrForkId::default();

    // Create a libp2p service
    let (globals, mut receiver): (_, LibP2PService<E>) =
        LibP2PService::new(executor, &config, enr_fork_id, &log)
            .await
            .expect("should build libp2p instance");

    println!("{}", globals.local_peer_id());

    // BlocksByRange Response
    let spec = E::default_spec();
    let empty_block = BeaconBlock::empty(&spec);
    let empty_signed = SignedBeaconBlock {
        message: empty_block,
        signature: Signature::empty(),
    };
    let rpc_response = Response::BlocksByRange(Some(Box::new(empty_signed)));

    // keep count of the number of messages received
    // build the receiver future
    let receiver_future = async {
        loop {
            match receiver.next_event().await {
                Libp2pEvent::Behaviour(BehaviourEvent::RequestReceived {
                    peer_id,
                    id,
                    request,
                }) => {
                    // send the response
                    info!(log, "Receiver got request"; "req" => ?request);
                    for i in 1..=5 {
                        info!(log, "Sending response"; "i" => i);
                        receiver.swarm.send_successful_response(
                            peer_id.clone(),
                            id,
                            rpc_response.clone(),
                        );
                        tokio::time::delay_for(std::time::Duration::from_millis(100)).await;
                    }
                    // send the stream termination
                    info!(log, "Sending stream termination");
                    receiver.swarm.send_successful_response(
                        peer_id,
                        id,
                        Response::BlocksByRange(None),
                    );
                }
                _ => {} // Ignore other events
            }
        }
    };
    receiver_future.await;
}
