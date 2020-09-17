use csv::Writer;
use eth2_libp2p::types::GossipKind;
use eth2_libp2p::Enr;
use eth2_libp2p::PubsubMessage;
use eth2_libp2p::Service as LibP2PService;
use eth2_libp2p::{BehaviourEvent, Libp2pEvent, NetworkConfig, NetworkGlobals};
use libp2p::gossipsub::{GossipsubConfigBuilder, GossipsubMessage, MessageId, ValidationMode};
use libp2p::PeerId;
use serde::Serialize;
use sha2::{Digest, Sha256};
use slog::{info, o, Drain};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::time::Duration;
use tempdir::TempDir;
use types::{
    AggregateSignature, Attestation, EnrForkId, Epoch, Hash256, MainnetEthSpec,
    SignedAggregateAndProof, Slot, SubnetId,
};

const DHT_FILE_NAME: &str = "dht";
const AGGREGATES_FILE_NAME: &str = "aggregates.csv";
const ATTESTATIONS_FILE_NAME: &str = "attestations.csv";

pub const GOSSIP_MAX_SIZE: usize = 1_048_576;
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
    let path = TempDir::new("libp2p_crawler").unwrap();
    let mut config = NetworkConfig::default();

    config.libp2p_port = port; // tcp port
    config.discovery_port = port; // udp port
    config.enr_tcp_port = Some(port);
    config.enr_udp_port = Some(port);
    config.enr_address = Some("127.0.0.1".parse().unwrap());
    config.boot_nodes_enr.append(&mut boot_nodes);
    config.network_dir = path.into_path();
    config.target_peers = 500;

    let gossip_message_id = |message: &GossipsubMessage| {
        MessageId::from(base64::encode_config(
            &Sha256::digest(&message.data),
            base64::URL_SAFE_NO_PAD,
        ))
    };
    let gs_config = GossipsubConfigBuilder::new()
        .max_transmit_size(GOSSIP_MAX_SIZE)
        .heartbeat_interval(Duration::from_millis(700))
        .mesh_n(48)
        .mesh_n_low(32)
        .mesh_n_high(64)
        .gossip_lazy(32)
        .fanout_ttl(Duration::from_secs(60))
        .history_length(6)
        .history_gossip(3)
        .validate_messages() // require validation before propagation
        .validation_mode(ValidationMode::Permissive)
        // prevent duplicates for 550 heartbeats(700millis * 550) = 385 secs
        .duplicate_cache_time(Duration::from_secs(385))
        .message_id_fn(gossip_message_id)
        .build()
        .expect("valid gossipsub configuration");

    config.gs_config = gs_config;

    // The default topics that we will initially subscribe to
    let mut topics = vec![GossipKind::BeaconAggregateAndProof];
    // Subscribe to all attestation subnets
    let subnet_topics: Vec<GossipKind> =
        (0..64).map(|i| GossipKind::Attestation(i.into())).collect();
    topics.extend(subnet_topics);
    config.topics = topics;

    config
}

#[tokio::main]
async fn main() {
    let enr_strs = vec![
        "enr:-Ku4QLglCMIYAgHd51uFUqejD9DWGovHOseHQy7Od1SeZnHnQ3fSpE4_nbfVs8lsy8uF07ae7IgrOOUFU0NFvZp5D4wBh2F0dG5ldHOIAAAAAAAAAACEZXRoMpAYrkzLAAAAAf__________gmlkgnY0gmlwhBLf22SJc2VjcDI1NmsxoQJxCnE6v_x2ekgY_uoE1rtwzvGy40mq9eD66XfHPBWgIIN1ZHCCD6A",
        "enr:-Ku4QOdk3u7rXI5YvqwmEbApW_OLlRkq_yzmmhdlrJMcfviacLWwSm-tr1BOvamuRQqfc6lnMeec4E4ddOhd3KqCB98Bh2F0dG5ldHOIAAAAAAAAAACEZXRoMpAYrkzLAAAAAf__________gmlkgnY0gmlwhBLf22SJc2VjcDI1NmsxoQKH3lxnglLqrA7L6sl5r7XFnckr3XCnlZMaBTYSdE8SHIN1ZHCCG1g",
        "enr:-Ku4QOVrqhlmsh9m2MGSnvVz8XPfjwHWBuOcgVQvWwBhN0-NI0XVhSerujBBwIeLpc-OES0C9iAzJhiCgRZ0xH13DgEBh2F0dG5ldHOIAAAAAAAAAACEZXRoMpAYrkzLAAAAAf__________gmlkgnY0gmlwhBLf22SJc2VjcDI1NmsxoQLEq16KLm1vPjUKYGkHq296D60i7y209NYPUpwZPXDVgYN1ZHCCF3A",
        "enr:-LK4QC3FCb7-JTNRiWAezECk_QUJc9c2IkJA1-EAmqAA5wmdbPWsAeRpnMXKRJqOYG0TE99ycB1nOb9y26mjb_UoHS4Bh2F0dG5ldHOIAAAAAAAAAACEZXRoMpDnp11aAAAAAf__________gmlkgnY0gmlwhDMPYfCJc2VjcDI1NmsxoQOmDQryZJApMwIT-dQAbxjvxLbPzyKn9GFk5dqam4MDTYN0Y3CCIyiDdWRwgiMo",
        "enr:-LK4QLvxLzt346gAPkTxohygiJvjd97lGcFeE5yXgZKtsMfEOveLE_FO2slJoHNzNF7vhwfwjt4X2vqzwGiR9gcrmDMBh2F0dG5ldHOIAAAAAAAAAACEZXRoMpDnp11aAAAAAf__________gmlkgnY0gmlwhDMPRgeJc2VjcDI1NmsxoQPjXTGx3HkaCG2neFxJmaTn5eCgbra3LY1twCeXPHChL4N0Y3CCIyiDdWRwgiMo",
        "enr:-Ku4QFVactU18ogiqPPasKs3jhUm5ISszUrUMK2c6SUPbGtANXVJ2wFapsKwVEVnVKxZ7Gsr9yEc4PYF-a14ahPa1q0Bh2F0dG5ldHOIAAAAAAAAAACEZXRoMpAYrkzLAAAAAf__________gmlkgnY0gmlwhGQbAHyJc2VjcDI1NmsxoQILF-Ya2i5yowVkQtlnZLjG0kqC4qtwmSk8ha7tKLuME4N1ZHCCIyg",
        "enr:-KG4QFuKQ9eeXDTf8J4tBxFvs3QeMrr72mvS7qJgL9ieO6k9Rq5QuGqtGK4VlXMNHfe34Khhw427r7peSoIbGcN91fUDhGV0aDKQD8XYjwAAAAH__________4JpZIJ2NIJpcIQDhMExiXNlY3AyNTZrMaEDESplmV9c2k73v0DjxVXJ6__2bWyP-tK28_80lf7dUhqDdGNwgiMog3VkcIIjKA",
        "enr:-LK4QCGFeQXjpQkgOfLHsbTjD65IOtSqV7Qo-Qdqv6SrL8lqFY7INPMMGP5uGKkVDcJkeXimSeNeypaZV3MHkcJgr9QCh2F0dG5ldHOIAAAAAAAAAACEZXRoMpDnp11aAAAAAf__________gmlkgnY0gmlwhA37LMaJc2VjcDI1NmsxoQJ7k0mKtTd_kdEq251flOjD1HKpqgMmIETDoD-Msy_O-4N0Y3CCIyiDdWRwgiMo",
        "enr:-LK4QNifGuaUmm3zfqC8SHSjvJP9JICHj4DYz2aAMXfJssgaRBnTanMRRz_eoIIaz5gX31JHT28Ce_El8krAWnDmh2MCh2F0dG5ldHOIAAAAAAAAAACEZXRoMpDnp11aAAAAAf__________gmlkgnY0gmlwhDQlA5CJc2VjcDI1NmsxoQOYiWqrQtQksTEtS3qY6idxJE5wkm0t9wKqpzv2gCR21oN0Y3CCIyiDdWRwgiMo",
        "enr:-LK4QBwf3yQV4A2H8piP7HI584BsXJYJqlH4v2kr25pEajFwTTsnF0-mC-nVLhbE_tV3Dfm1OSGHfY3TIJDhhk0vQwABh2F0dG5ldHOI__________-EZXRoMpDnp11aAAAAAf__________gmlkgnY0gmlwhAN7IWiJc2VjcDI1NmsxoQN7SVjDI903lJ9olSB8a_Fp7zajPhh5FgEGD-lSOxonZYN0Y3CCI4yDdWRwgiOM",
        "enr:-LK4QA5FEn7IcW83DyYmYgKEC5MNlfkXDyuH60EX4_GyapIbQJaPkkWaTgbU5mKIg8xd8Ek7Z7lRkPbh0U7E85DcLtoBh2F0dG5ldHOI__________-EZXRoMpDnp11aAAAAAf__________gmlkgnY0gmlwhBKcVIyJc2VjcDI1NmsxoQIKJAFKbLs9vR-4H4He8HvNxm03YIjORGmJIJoFJ3lPO4N0Y3CCI4yDdWRwgiOM",
        "enr:-LK4QM2RJb5_1Wd1sMdLcdcRv7i397hCwXMEPyqRj1Wbn6HZGM0ioncwNnMDV163-0cNmTJLXuALbQoNufR6rX18LI8Bh2F0dG5ldHOI__________-EZXRoMpDnp11aAAAAAf__________gmlkgnY0gmlwhDZd9S6Jc2VjcDI1NmsxoQPqwn1FZZKe3afNhwgqn3uQDNDOh5-Pr8qgVQMkSFahWYN0Y3CCI4yDdWRwgiOM",
        "enr:-LK4QIolrZmrkGhK9_Q5qX44rFM6D6z7pXL_ilHRQ3rNunDqZQEvhDGART--MbLaMZxSZtOKpd9sP520edm3ZUVcwcIBh2F0dG5ldHOI__________-EZXRoMpDnp11aAAAAAf__________gmlkgnY0gmlwhBLEvqqJc2VjcDI1NmsxoQKzNXbQu165tGZvK6sWqu44Fk9k_s93AmUzqIfbCyQyz4N0Y3CCI4yDdWRwgiOM",
        "enr:-LK4QGvceQZPuO44DTEsb_HqvkiMl85Fva7qvg0s8pJ0lkU3J_pvDrrYsmOkp-e8Zgq8m5Ewimd4Xhe4ZBnLanY7d-ABh2F0dG5ldHOI__________-EZXRoMpDnp11aAAAAAf__________gmlkgnY0gmlwhAN8wRKJc2VjcDI1NmsxoQJAGkv3ZK5DJLP8B07BkMSOp13LDYQEHloP65F4We9vSYN0Y3CCI4yDdWRwgiOM",
        "enr:-LK4QFMUor7tPnQfx0CO8lBv1IicmvrlITSl7wMmf-SvBI9eGoOpSrn1TRG2WSxmEA7JKxkgqa_wZsCmqw_NUVEYf0EBh2F0dG5ldHOI__________-EZXRoMpDnp11aAAAAAf__________gmlkgnY0gmlwhBKdpCiJc2VjcDI1NmsxoQK7ayo4eVvgc_EzENnncZT5_KFhVEvC4jbu1w529m2j_YN0Y3CCI4yDdWRwgiOM",
        "enr:-LK4QKWk9yZo258PQouLshTOEEGWVHH7GhKwpYmB5tmKE4eHeSfman0PZvM2Rpp54RWgoOagAsOfKoXgZSbiCYzERWABh2F0dG5ldHOIAAAAAAAAAACEZXRoMpAAAAAAAAAAAAAAAAAAAAAAgmlkgnY0gmlwhDQlA5CJc2VjcDI1NmsxoQOYiWqrQtQksTEtS3qY6idxJE5wkm0t9wKqpzv2gCR21oN0Y3CCIyiDdWRwgiMo",
        "enr:-LK4QEnIS-PIxxLCadJdnp83VXuJqgKvC9ZTIWaJpWqdKlUFCiup2sHxWihF9EYGlMrQLs0mq_2IyarhNq38eoaOHUoBh2F0dG5ldHOIAAAAAAAAAACEZXRoMpAAAAAAAAAAAAAAAAAAAAAAgmlkgnY0gmlwhA37LMaJc2VjcDI1NmsxoQJ7k0mKtTd_kdEq251flOjD1HKpqgMmIETDoD-Msy_O-4N0Y3CCIyiDdWRwgiMo",
        "enr:-KG4QIOJRu0BBlcXJcn3lI34Ub1aBLYipbnDaxBnr2uf2q6nE1TWnKY5OAajg3eG6mHheQSfRhXLuy-a8V5rqXKSoUEChGV0aDKQGK5MywAAAAH__________4JpZIJ2NIJpcIQKAAFhiXNlY3AyNTZrMaEDESplmV9c2k73v0DjxVXJ6__2bWyP-tK28_80lf7dUhqDdGNwgiMog3VkcIIjKA",
        "enr:-Ku4QLglCMIYAgHd51uFUqejD9DWGovHOseHQy7Od1SeZnHnQ3fSpE4_nbfVs8lsy8uF07ae7IgrOOUFU0NFvZp5D4wBh2F0dG5ldHOIAAAAAAAAAACEZXRoMpAYrkzLAAAAAf__________gmlkgnY0gmlwhBLf22SJc2VjcDI1NmsxoQJxCnE6v_x2ekgY_uoE1rtwzvGy40mq9eD66XfHPBWgIIN1ZHCCD6A",
        "enr:-Ku4QOzU2MY51tYFcoByfULugCu2mepfqAbB0DajbRzg8xlILLfi5Iv_Wx-ARn8SiFoZZb3yp2x05cnUDYSoDYZupjIBh2F0dG5ldHOIAAAAAAAAAACEZXRoMpAYrkzLAAAAAf__________gmlkgnY0gmlwhBLf22SJc2VjcDI1NmsxoQLEq16KLm1vPjUKYGkHq296D60i7y209NYPUpwZPXDVgYN1ZHCCD6A",
        "enr:-Ku4QOYFmi2BW_YPDew_CKdfMvsrcRY1ARA-ImtcqFl-lgoxOFbxte4PU44-1M3uRNSRM-6rVa8USGohmWwtgwalEt8Bh2F0dG5ldHOIAAAAAAAAAACEZXRoMpAYrkzLAAAAAf__________gmlkgnY0gmlwhBLf22SJc2VjcDI1NmsxoQKH3lxnglLqrA7L6sl5r7XFnckr3XCnlZMaBTYSdE8SHIN1ZHCCD6A",
        "enr:-LK4QC3FCb7-JTNRiWAezECk_QUJc9c2IkJA1-EAmqAA5wmdbPWsAeRpnMXKRJqOYG0TE99ycB1nOb9y26mjb_UoHS4Bh2F0dG5ldHOIAAAAAAAAAACEZXRoMpDnp11aAAAAAf__________gmlkgnY0gmlwhDMPYfCJc2VjcDI1NmsxoQOmDQryZJApMwIT-dQAbxjvxLbPzyKn9GFk5dqam4MDTYN0Y3CCIyiDdWRwgiMo",
        "enr:-LK4QLvxLzt346gAPkTxohygiJvjd97lGcFeE5yXgZKtsMfEOveLE_FO2slJoHNzNF7vhwfwjt4X2vqzwGiR9gcrmDMBh2F0dG5ldHOIAAAAAAAAAACEZXRoMpDnp11aAAAAAf__________gmlkgnY0gmlwhDMPRgeJc2VjcDI1NmsxoQPjXTGx3HkaCG2neFxJmaTn5eCgbra3LY1twCeXPHChL4N0Y3CCIyiDdWRwgiMo",
        "enr:-Ku4QFVactU18ogiqPPasKs3jhUm5ISszUrUMK2c6SUPbGtANXVJ2wFapsKwVEVnVKxZ7Gsr9yEc4PYF-a14ahPa1q0Bh2F0dG5ldHOIAAAAAAAAAACEZXRoMpAYrkzLAAAAAf__________gmlkgnY0gmlwhGQbAHyJc2VjcDI1NmsxoQILF-Ya2i5yowVkQtlnZLjG0kqC4qtwmSk8ha7tKLuME4N1ZHCCIyg",
    ];
    let bootnodes: Vec<Enr> = enr_strs.iter().map(|e| e.parse().unwrap()).collect();
    let config = build_config(9000, bootnodes);
    let log = build_log(slog::Level::Info, true);
    let (_signal, exit) = exit_future::signal();
    let (shutdown_tx, _) = futures::channel::mpsc::channel(1);
    let executor = environment::TaskExecutor::new(
        tokio::runtime::Handle::current(),
        exit,
        log.clone(),
        shutdown_tx,
    );
    // let fork_digest = "0xe7a75d5a";
    let mut enr_fork_id = EnrForkId::default();
    enr_fork_id.fork_digest = [231, 167, 93, 90];
    enr_fork_id.next_fork_version = [0, 0, 0, 1];

    let (globals, mut libp2p_service): (_, LibP2PService<E>) =
        LibP2PService::new(executor, &config, enr_fork_id, &log)
            .await
            .expect("should build libp2p instance");

    // csv datafiles
    let mut wtr_attestation = csv::Writer::from_path(ATTESTATIONS_FILE_NAME).unwrap();
    let mut wtr_aggregate = csv::Writer::from_path(AGGREGATES_FILE_NAME).unwrap();

    // Advertise all enr subnets as listening
    for subnet_id in 0..64 {
        libp2p_service
            .swarm
            .update_enr_subnet(SubnetId::new(subnet_id), true);
    }

    // Add persisted enrs back to the dht
    let enrs = load_dht().unwrap_or(vec![]);
    info!(log, "Loading dht"; "num" => enrs.len());
    for enr in enrs {
        libp2p_service.swarm.add_enr(enr);
    }

    let mut rx = block_until_ctrl_c();
    let mut clients = HashMap::new();

    let fut = async {
        loop {
            tokio::select! {
                _ = (&mut rx) => {
                    info!(log, "Persisting dht");
                    persist_dht(libp2p_service.swarm.enr_entries());
                    return;
                }
                libp2p_event = libp2p_service.next_event() => {
                    match libp2p_event {
                    Libp2pEvent::Behaviour(BehaviourEvent::PubsubMessage {
                        source, message, message_source,..
                    }) => {
                        let source_client = get_client(&source, globals.clone(), &mut clients);
                        let message_client = if let Some(ms) = &message_source {
                            get_client(&ms,globals.clone(), &mut clients)
                        }
                        else {
                            None
                        };
                        match message {
                        PubsubMessage::Attestation(msg) => write_csv(
                            &mut wtr_attestation,
                            AttestationCSV::new(msg.1,  source, message_source,source_client, message_client),
                        ),
                        PubsubMessage::AggregateAndProofAttestation(msg) => {
                            write_csv(&mut wtr_aggregate, AggregateCSV::new(*msg, source, message_source, source_client, message_client))
                        }

                        _ => {}
                    }
                    },
                    Libp2pEvent::Behaviour(BehaviourEvent::PeerConnected(peer_id)) => {
                        info!(
                            log,
                            "Connected to peer";
                            "peer_id" => %peer_id,
                            "peer_count" => globals.connected_peers()
                        );
                    }
                    Libp2pEvent::Behaviour(BehaviourEvent::PeerDialed(peer_id)) => {
                        info!(
                            log,
                            "Connected to peer";
                            "peer_id" => %peer_id,
                            "peer_count" => globals.connected_peers()
                        );
                    }
                    Libp2pEvent::Behaviour(BehaviourEvent::PeerDisconnected(peer_id)) => {
                        info!(
                            log,
                            "Disconnected from peer";
                            "peer_id" => %peer_id,
                            "peer_count" => globals.connected_peers()
                        );
                    }
                    _ => {}
                }
            }
            }
        }
    };
    fut.await;
}

fn get_client(
    peer_id: &PeerId,
    globals: std::sync::Arc<NetworkGlobals<E>>,
    local_db: &mut HashMap<PeerId, String>,
) -> Option<String> {
    if let Some(client) = local_db.get(peer_id) {
        return Some(client.clone());
    } else if let Some(info) = globals.peers.read().peer_info(peer_id) {
        local_db.insert(peer_id.clone(), info.client.kind.to_string());
        return Some(info.client.kind.to_string());
    } else {
        None
    }
}

/// Block the current thread until Ctrl+C is received.
pub fn block_until_ctrl_c() -> tokio::sync::oneshot::Receiver<()> {
    let (ctrlc_send, ctrlc_oneshot) = tokio::sync::oneshot::channel();
    let ctrlc_send_c = std::cell::RefCell::new(Some(ctrlc_send));
    ctrlc::set_handler(move || {
        if let Some(ctrlc_send) = ctrlc_send_c.try_borrow_mut().unwrap().take() {
            ctrlc_send.send(()).expect("Error sending ctrl-c message");
        }
    })
    .unwrap();

    ctrlc_oneshot
}

#[derive(Debug, Serialize)]
struct AttestationCSV {
    timestamp: i64,
    propagation_source: String,
    message_source: Option<String>,
    propagation_client: Option<String>,
    message_client: Option<String>,
    aggregation_bits: String,
    slot: Slot,
    index: u64,
    beacon_block_root: Hash256,
    source_epoch: Epoch,
    source_root: Hash256,
    target_epoch: Epoch,
    target_root: Hash256,
    signature: AggregateSignature,
}

impl AttestationCSV {
    pub fn new(
        data: Attestation<E>,
        propagation_source: PeerId,
        message_source: Option<PeerId>,
        propagation_client: Option<String>,
        message_client: Option<String>,
    ) -> Self {
        let timestamp = chrono::prelude::Utc::now().timestamp_millis();
        Self {
            timestamp,
            propagation_source: propagation_source.to_string(),
            message_source: message_source.map(|p| p.to_string()),
            propagation_client,
            message_client,
            aggregation_bits: hex::encode(data.aggregation_bits.into_bytes()),
            slot: data.data.slot,
            index: data.data.index,
            beacon_block_root: data.data.beacon_block_root,
            source_epoch: data.data.source.epoch,
            source_root: data.data.source.root,
            target_epoch: data.data.target.epoch,
            target_root: data.data.target.root,
            signature: data.signature,
        }
    }
}

#[derive(Debug, Serialize)]
struct AggregateCSV {
    timestamp: i64,
    propagation_source: String,
    message_source: Option<String>,
    propagation_client: Option<String>,
    message_client: Option<String>,
    aggregation_bits: String,
    aggregator_index: u64,
    slot: Slot,
    index: u64,
    beacon_block_root: Hash256,
    source_epoch: Epoch,
    source_root: Hash256,
    target_epoch: Epoch,
    target_root: Hash256,
    aggregate_signature: AggregateSignature,
}

impl AggregateCSV {
    pub fn new(
        data: SignedAggregateAndProof<E>,
        propagation_source: PeerId,
        message_source: Option<PeerId>,
        propagation_client: Option<String>,
        message_client: Option<String>,
    ) -> Self {
        let timestamp = chrono::prelude::Utc::now().timestamp_millis();
        Self {
            timestamp,
            propagation_source: propagation_source.to_string(),
            message_source: message_source.map(|p| p.to_string()),
            propagation_client,
            message_client,
            aggregation_bits: hex::encode(data.message.aggregate.aggregation_bits.into_bytes()),
            aggregator_index: data.message.aggregator_index,
            slot: data.message.aggregate.data.slot,
            index: data.message.aggregate.data.index,
            beacon_block_root: data.message.aggregate.data.beacon_block_root,
            source_epoch: data.message.aggregate.data.source.epoch,
            source_root: data.message.aggregate.data.source.root,
            target_epoch: data.message.aggregate.data.target.epoch,
            target_root: data.message.aggregate.data.target.root,
            aggregate_signature: data.message.aggregate.signature,
        }
    }
}

fn write_csv<S: Serialize>(writer: &mut Writer<File>, data: S) {
    writer.serialize(data).unwrap();
    writer.flush().unwrap();
}

pub fn persist_dht(enrs: Vec<Enr>) {
    let bytes = rlp::encode_list(&enrs);
    let mut file = std::fs::File::create(DHT_FILE_NAME).unwrap();
    file.write_all(&bytes).unwrap();
}

pub fn load_dht() -> Result<Vec<Enr>, String> {
    let mut file = std::fs::File::open(DHT_FILE_NAME).map_err(|e| format!("No such file {}", e))?;
    let mut buf = vec![];
    file.read_to_end(&mut buf)
        .map_err(|e| format!("Cant read {}", e))?;
    let rlp = rlp::Rlp::new(buf.as_slice());
    let enrs: Vec<Enr> = rlp
        .as_list()
        .map_err(|e| format!("Cant decode rlp {}", e))?;
    Ok(enrs)
}
