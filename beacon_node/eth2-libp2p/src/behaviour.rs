use crate::config::*;
use crate::discovery::Discovery;
use crate::rpc::{RPCEvent, RPCMessage, RPC};
use crate::{error, NetworkConfig};
use crate::{Topic, TopicHash};
use crate::{BEACON_ATTESTATION_TOPIC, BEACON_BLOCK_TOPIC};
use enr::Enr;
use futures::prelude::*;
use libp2p::{
    core::identity::Keypair,
    discv5::Discv5Event,
    gossipsub::{Gossipsub, GossipsubEvent},
    identify::{Identify, IdentifyEvent},
    ping::{Ping, PingConfig, PingEvent},
    swarm::{NetworkBehaviourAction, NetworkBehaviourEventProcess},
    tokio_io::{AsyncRead, AsyncWrite},
    NetworkBehaviour, PeerId,
};
use slog::{debug, o};
use std::num::NonZeroU32;
use std::time::Duration;

const MAX_IDENTIFY_ADDRESSES: usize = 20;

/// Builds the network behaviour that manages the core protocols of eth2.
/// This core behaviour is managed by `Behaviour` which adds peer management to all core
/// behaviours.
#[derive(NetworkBehaviour)]
#[behaviour(out_event = "BehaviourEvent", poll_method = "poll")]
pub struct Behaviour<TSubstream: AsyncRead + AsyncWrite> {
    /// The routing pub-sub mechanism for eth2.
    gossipsub: Gossipsub<TSubstream>,
    /// The Eth2 RPC specified in the wire-0 protocol.
    eth2_rpc: RPC<TSubstream>,
    /// Keep regular connection to peers and disconnect if absent.
    // TODO: Remove Libp2p ping in favour of discv5 ping.
    ping: Ping<TSubstream>,
    // TODO: Using id for initial interop. This will be removed by mainnet.
    /// Provides IP addresses and peer information.
    identify: Identify<TSubstream>,
    /// Discovery behaviour.
    discovery: Discovery<TSubstream>,
    #[behaviour(ignore)]
    /// The events generated by this behaviour to be consumed in the swarm poll.
    events: Vec<BehaviourEvent>,
    /// Logger for behaviour actions.
    #[behaviour(ignore)]
    log: slog::Logger,
}

impl<TSubstream: AsyncRead + AsyncWrite> Behaviour<TSubstream> {
    pub fn new(
        local_key: &Keypair,
        net_conf: &NetworkConfig,
        log: &slog::Logger,
    ) -> error::Result<Self> {
        let local_peer_id = local_key.public().clone().into_peer_id();
        let behaviour_log = log.new(o!());

        let ping_config = PingConfig::new()
            .with_timeout(Duration::from_secs(30))
            .with_interval(Duration::from_secs(20))
            .with_max_failures(NonZeroU32::new(2).expect("2 != 0"))
            .with_keep_alive(false);

        let identify = Identify::new(
            "lighthouse/libp2p".into(),
            version::version(),
            local_key.public(),
        );

        Ok(Behaviour {
            eth2_rpc: RPC::new(log.clone()),
            gossipsub: Gossipsub::new(local_peer_id.clone(), net_conf.gs_config.clone()),
            discovery: Discovery::new(local_key, net_conf, log)?,
            ping: Ping::new(ping_config),
            identify,
            events: Vec::new(),
            log: behaviour_log,
        })
    }

    pub fn discovery(&self) -> &Discovery<TSubstream> {
        &self.discovery
    }

    pub fn gs(&self) -> &Gossipsub<TSubstream> {
        &self.gossipsub
    }
}

// Implement the NetworkBehaviourEventProcess trait so that we can derive NetworkBehaviour for Behaviour
impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<GossipsubEvent>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, event: GossipsubEvent) {
        match event {
            GossipsubEvent::Message(propagation_source, gs_msg) => {
                let id = gs_msg.id();
                let msg = PubsubMessage::from_topics(&gs_msg.topics, gs_msg.data);

                // Note: We are keeping track here of the peer that sent us the message, not the
                // peer that originally published the message.
                self.events.push(BehaviourEvent::GossipMessage {
                    id,
                    source: propagation_source,
                    topics: gs_msg.topics,
                    message: msg,
                });
            }
            GossipsubEvent::Subscribed { peer_id, topic } => {
                self.events
                    .push(BehaviourEvent::PeerSubscribed(peer_id, topic));
            }
            GossipsubEvent::Unsubscribed { .. } => {}
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<RPCMessage>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, event: RPCMessage) {
        match event {
            RPCMessage::PeerDialed(peer_id) => {
                self.events.push(BehaviourEvent::PeerDialed(peer_id))
            }
            RPCMessage::PeerDisconnected(peer_id) => {
                self.events.push(BehaviourEvent::PeerDisconnected(peer_id))
            }
            RPCMessage::RPC(peer_id, rpc_event) => {
                self.events.push(BehaviourEvent::RPC(peer_id, rpc_event))
            }
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<PingEvent>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, _event: PingEvent) {
        // not interested in ping responses at the moment.
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> Behaviour<TSubstream> {
    /// Consumes the events list when polled.
    fn poll<TBehaviourIn>(
        &mut self,
    ) -> Async<NetworkBehaviourAction<TBehaviourIn, BehaviourEvent>> {
        if !self.events.is_empty() {
            return Async::Ready(NetworkBehaviourAction::GenerateEvent(self.events.remove(0)));
        }

        Async::NotReady
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<IdentifyEvent>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, event: IdentifyEvent) {
        match event {
            IdentifyEvent::Received {
                peer_id,
                mut info,
                observed_addr,
            } => {
                if info.listen_addrs.len() > MAX_IDENTIFY_ADDRESSES {
                    debug!(
                        self.log,
                        "More than 20 addresses have been identified, truncating"
                    );
                    info.listen_addrs.truncate(MAX_IDENTIFY_ADDRESSES);
                }
                debug!(self.log, "Identified Peer"; "Peer" => format!("{}", peer_id),
                "protocol_version" => info.protocol_version,
                "agent_version" => info.agent_version,
                "listening_ addresses" => format!("{:?}", info.listen_addrs),
                "observed_address" => format!("{:?}", observed_addr),
                "protocols" => format!("{:?}", info.protocols)
                );
            }
            IdentifyEvent::Sent { .. } => {}
            IdentifyEvent::Error { .. } => {}
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<Discv5Event>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, _event: Discv5Event) {
        // discv5 has no events to inject
    }
}

/// Implements the combined behaviour for the libp2p service.
impl<TSubstream: AsyncRead + AsyncWrite> Behaviour<TSubstream> {
    /* Pubsub behaviour functions */

    /// Subscribes to a gossipsub topic.
    pub fn subscribe(&mut self, topic: Topic) -> bool {
        self.gossipsub.subscribe(topic)
    }

    /// Unsubscribe from a gossipsub topic.
    pub fn unsubscribe(&mut self, topic: Topic) -> bool {
        self.gossipsub.unsubscribe(topic)
    }

    /// Publishes a message on the pubsub (gossipsub) behaviour.
    pub fn publish(&mut self, topics: &[Topic], message: PubsubMessage) {
        let message_data = message.into_data();
        for topic in topics {
            self.gossipsub.publish(topic, message_data.clone());
        }
    }

    /// Forwards a message that is waiting in gossipsub's mcache. Messages are only propagated
    /// once validated by the beacon chain.
    pub fn propagate_message(&mut self, propagation_source: &PeerId, message_id: String) {
        self.gossipsub
            .propagate_message(&message_id, propagation_source);
    }

    /* Eth2 RPC behaviour functions */

    /// Sends an RPC Request/Response via the RPC protocol.
    pub fn send_rpc(&mut self, peer_id: PeerId, rpc_event: RPCEvent) {
        self.eth2_rpc.send_rpc(peer_id, rpc_event);
    }

    /* Discovery / Peer management functions */
    /// Return the list of currently connected peers.
    pub fn connected_peers(&self) -> usize {
        self.discovery.connected_peers()
    }

    /// Notify discovery that the peer has been banned.
    pub fn peer_banned(&mut self, peer_id: PeerId) {
        self.discovery.peer_banned(peer_id);
    }

    /// Informs the discovery behaviour if a new IP/Port is set at the application layer
    pub fn update_local_enr_socket(&mut self, socket: std::net::SocketAddr, is_tcp: bool) {
        self.discovery.update_local_enr(socket, is_tcp);
    }

    /// Returns an iterator over all enr entries in the DHT.
    pub fn enr_entries(&mut self) -> impl Iterator<Item = &Enr> {
        self.discovery.enr_entries()
    }

    /// Add an ENR to the routing table of the discovery mechanism.
    pub fn add_enr(&mut self, enr: Enr) {
        self.discovery.add_enr(enr);
    }
}

/// The types of events than can be obtained from polling the behaviour.
pub enum BehaviourEvent {
    /// A received RPC event and the peer that it was received from.
    RPC(PeerId, RPCEvent),
    /// We have completed an initial connection to a new peer.
    PeerDialed(PeerId),
    /// A peer has disconnected.
    PeerDisconnected(PeerId),
    /// A gossipsub message has been received.
    GossipMessage {
        /// The gossipsub message id. Used when propagating blocks after validation.
        id: String,
        /// The peer from which we received this message, not the peer that published it.
        source: PeerId,
        /// The topics that this message was sent on.
        topics: Vec<TopicHash>,
        /// The message itself.
        message: PubsubMessage,
    },
    /// Subscribed to peer for given topic
    PeerSubscribed(PeerId, TopicHash),
}

/// Messages that are passed to and from the pubsub (Gossipsub) behaviour. These are encoded and
/// decoded upstream.
#[derive(Debug, Clone, PartialEq)]
pub enum PubsubMessage {
    /// Gossipsub message providing notification of a new block.
    Block(Vec<u8>),
    /// Gossipsub message providing notification of a new attestation.
    Attestation(Vec<u8>),
    /// Gossipsub message providing notification of a voluntary exit.
    VoluntaryExit(Vec<u8>),
    /// Gossipsub message providing notification of a new proposer slashing.
    ProposerSlashing(Vec<u8>),
    /// Gossipsub message providing notification of a new attester slashing.
    AttesterSlashing(Vec<u8>),
    /// Gossipsub message from an unknown topic.
    Unknown(Vec<u8>),
}

impl PubsubMessage {
    /* Note: This is assuming we are not hashing topics. If we choose to hash topics, these will
     * need to be modified.
     *
     * Also note that a message can be associated with many topics. As soon as one of the topics is
     * known we match. If none of the topics are known we return an unknown state.
     */
    fn from_topics(topics: &[TopicHash], data: Vec<u8>) -> Self {
        for topic in topics {
            // compare the prefix and postfix, then match on the topic
            let topic_parts: Vec<&str> = topic.as_str().split('/').collect();
            if topic_parts.len() == 4
                && topic_parts[1] == TOPIC_PREFIX
                && topic_parts[3] == TOPIC_ENCODING_POSTFIX
            {
                match topic_parts[2] {
                    BEACON_BLOCK_TOPIC => return PubsubMessage::Block(data),
                    BEACON_ATTESTATION_TOPIC => return PubsubMessage::Attestation(data),
                    VOLUNTARY_EXIT_TOPIC => return PubsubMessage::VoluntaryExit(data),
                    PROPOSER_SLASHING_TOPIC => return PubsubMessage::ProposerSlashing(data),
                    ATTESTER_SLASHING_TOPIC => return PubsubMessage::AttesterSlashing(data),
                    _ => {}
                }
            }
        }
        PubsubMessage::Unknown(data)
    }

    fn into_data(self) -> Vec<u8> {
        match self {
            PubsubMessage::Block(data)
            | PubsubMessage::Attestation(data)
            | PubsubMessage::VoluntaryExit(data)
            | PubsubMessage::ProposerSlashing(data)
            | PubsubMessage::AttesterSlashing(data)
            | PubsubMessage::Unknown(data) => data,
        }
    }
}
