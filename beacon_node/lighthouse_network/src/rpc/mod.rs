//! The Ethereum 2.0 Wire Protocol
//!
//! This protocol is a purpose built Ethereum 2.0 libp2p protocol. It's role is to facilitate
//! direct peer-to-peer communication primarily for sending/receiving chain information for
//! syncing.

use futures::future::FutureExt;
use handler::RPCHandler;
use libp2p::core::transport::PortUse;
use libp2p::swarm::{
    handler::ConnectionHandler, CloseConnection, ConnectionId, NetworkBehaviour, NotifyHandler,
    ToSwarm,
};
use libp2p::swarm::{ConnectionClosed, FromSwarm, SubstreamProtocol, THandlerInEvent};
use libp2p::PeerId;
use parking_lot::Mutex;
use rate_limiter::{RPCRateLimiter as RateLimiter, RateLimitedErr};
use slog::{crit, debug, o};
use std::marker::PhantomData;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::time::DelayQueue;
use types::{EthSpec, ForkContext};

pub(crate) use handler::{HandlerErr, HandlerEvent};
pub(crate) use methods::{
    MetaData, MetaDataV1, MetaDataV2, MetaDataV3, Ping, RPCCodedResponse, RPCResponse,
};
pub(crate) use protocol::InboundRequest;

use crate::rpc::active_requests_limiter::ActiveRequestsLimiter;
use crate::rpc::rate_limiter::RateLimiterItem;
pub use handler::SubstreamId;
pub use methods::{
    BlocksByRangeRequest, BlocksByRootRequest, GoodbyeReason, LightClientBootstrapRequest,
    RPCResponseErrorCode, ResponseTermination, StatusMessage,
};
pub(crate) use outbound::OutboundRequest;
pub use protocol::{max_rpc_size, Protocol, RPCError};

use self::config::{InboundRateLimiterConfig, OutboundRateLimiterConfig};
use self::protocol::RPCProtocol;
use self::self_limiter::SelfRateLimiter;

mod active_requests_limiter;
pub(crate) mod codec;
pub mod config;
mod handler;
pub mod methods;
mod outbound;
mod protocol;
mod rate_limiter;
mod self_limiter;

/// Composite trait for a request id.
pub trait ReqId: Send + 'static + std::fmt::Debug + Copy + Clone {}
impl<T> ReqId for T where T: Send + 'static + std::fmt::Debug + Copy + Clone {}

/// RPC events sent from Lighthouse.
#[derive(Debug, Clone)]
pub enum RPCSend<Id, E: EthSpec> {
    /// A request sent from Lighthouse.
    ///
    /// The `Id` is given by the application making the request. These
    /// go over *outbound* connections.
    Request(Id, OutboundRequest<E>),
    /// A response sent from Lighthouse.
    ///
    /// The `SubstreamId` must correspond to the RPC-given ID of the original request received from the
    /// peer. The second parameter is a single chunk of a response. These go over *inbound*
    /// connections.
    Response(SubstreamId, RPCCodedResponse<E>),
    /// Lighthouse has requested to terminate the connection with a goodbye message.
    Shutdown(Id, GoodbyeReason),
}

/// RPC events received from outside Lighthouse.
#[derive(Debug, Clone)]
pub enum RPCReceived<Id, E: EthSpec> {
    /// A request received from the outside.
    ///
    /// The `SubstreamId` is given by the `RPCHandler` as it identifies this request with the
    /// *inbound* substream over which it is managed.
    Request(SubstreamId, InboundRequest<E>),
    /// A response received from the outside.
    ///
    /// The `Id` corresponds to the application given ID of the original request sent to the
    /// peer. The second parameter is a single chunk of a response. These go over *outbound*
    /// connections.
    Response(Id, RPCResponse<E>),
    /// Marks a request as completed
    EndOfStream(Id, ResponseTermination),
}

impl<E: EthSpec, Id: std::fmt::Debug> std::fmt::Display for RPCSend<Id, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RPCSend::Request(id, req) => write!(f, "RPC Request(id: {:?}, {})", id, req),
            RPCSend::Response(id, res) => write!(f, "RPC Response(id: {:?}, {})", id, res),
            RPCSend::Shutdown(_id, reason) => write!(f, "Sending Goodbye: {}", reason),
        }
    }
}

/// Messages sent to the user from the RPC protocol.
#[derive(Debug)]
pub struct RPCMessage<Id, E: EthSpec> {
    /// The peer that sent the message.
    pub peer_id: PeerId,
    /// Handler managing this message.
    pub conn_id: ConnectionId,
    /// The message that was sent.
    pub event: HandlerEvent<Id, E>,
}

type BehaviourAction<Id, E> = ToSwarm<RPCMessage<Id, E>, RPCSend<Id, E>>;

pub struct NetworkParams {
    pub max_chunk_size: usize,
    pub ttfb_timeout: Duration,
    pub resp_timeout: Duration,
}

/// Implements the libp2p `NetworkBehaviour` trait and therefore manages network-level
/// logic.
pub struct RPC<Id: ReqId, E: EthSpec> {
    /// Rate limiter for our responses. This is shared with RPCHandlers.
    response_limiter: Option<Arc<Mutex<RateLimiter>>>,
    /// Rate limiter for our own requests.
    outbound_request_limiter: Option<SelfRateLimiter<Id, E>>,
    /// Limiter for inbound requests, which restricts more than two requests from running
    /// simultaneously on the same protocol per peer.
    active_inbound_requests_limiter: ActiveRequestsLimiter,
    /// Queue of events to be processed.
    events: Vec<BehaviourAction<Id, E>>,
    fork_context: Arc<ForkContext>,
    enable_light_client_server: bool,
    /// Slog logger for RPC behaviour.
    log: slog::Logger,
    /// Networking constant values
    network_params: NetworkParams,

    /// Rate limiter for our responses and the PeerId that this handler interacts with.
    /// The PeerId is necessary since the rate limiter manages rate limiting per peer.
    response_limiter_new: RateLimiter,

    /// Responses queued for sending. These responses are stored when the response limiter rejects them.
    delayed_responses: DelayQueue<QueuedResponse<E>>,
}

#[derive(Clone)]
struct QueuedResponse<E: EthSpec> {
    response: RPCCodedResponse<E>,
    substream_id: SubstreamId,
    connection_id: ConnectionId,
    peer_id: PeerId,
}

impl<Id: ReqId, E: EthSpec> RPC<Id, E> {
    pub fn new(
        fork_context: Arc<ForkContext>,
        enable_light_client_server: bool,
        inbound_rate_limiter_config: Option<InboundRateLimiterConfig>,
        outbound_rate_limiter_config: Option<OutboundRateLimiterConfig>,
        log: slog::Logger,
        network_params: NetworkParams,
    ) -> Self {
        let log = log.new(o!("service" => "libp2p_rpc"));

        let response_limiter = inbound_rate_limiter_config.clone().map(|config| {
            debug!(log, "Using response rate limiting params"; "config" => ?config);
            Arc::new(Mutex::new(
                RateLimiter::new_with_config(config.0)
                    .expect("Inbound limiter configuration parameters are valid"),
            ))
        });

        let outbound_request_limiter = outbound_rate_limiter_config.map(|config| {
            SelfRateLimiter::new(config, log.clone()).expect("Configuration parameters are valid")
        });
        let response_limiter_new =
            RateLimiter::new_with_config(inbound_rate_limiter_config.as_ref().unwrap().0.clone())
                .expect("Inbound limiter configuration parameters are valid");
        RPC {
            response_limiter,
            outbound_request_limiter,
            active_inbound_requests_limiter: ActiveRequestsLimiter::new(),
            events: Vec::new(),
            fork_context,
            enable_light_client_server,
            log,
            network_params,
            response_limiter_new,
            delayed_responses: Default::default(),
        }
    }

    /// Checks if the response limiter allows the response. If the response should be delayed, the
    /// duration to wait is returned.
    fn try_response_limiter(
        &mut self,
        peer_id: &PeerId,
        response: &RPCCodedResponse<E>,
    ) -> Result<(), Duration> {
        match self.response_limiter_new.allows(peer_id, response) {
            Ok(()) => Ok(()),
            Err(e) => match e {
                RateLimitedErr::TooLarge => {
                    // This should never happen with default parameters. Let's just send the response.
                    // Log a crit since this is a config issue.
                    crit!(
                       self.log,
                        "Response rate limiting error for a batch that will never fit. Sending response anyway. Check configuration parameters.";
                        "protocol" => %response.protocol()
                    );
                    Ok(())
                }
                RateLimitedErr::TooSoon(wait_time) => {
                    debug!(self.log, "Response rate limiting"; "protocol" => %response.protocol(), "wait_time_ms" => wait_time.as_millis(), "peer_id" => %peer_id);
                    Err(wait_time)
                }
            },
        }
    }

    /// Sends an RPC response.
    ///
    /// The peer must be connected for this to succeed.
    pub fn send_response(
        &mut self,
        peer_id: PeerId,
        id: (ConnectionId, SubstreamId),
        event: RPCCodedResponse<E>,
    ) {
        self.active_inbound_requests_limiter
            .remove_request(peer_id, &id.0, &id.1);
        match self.try_response_limiter(&peer_id, &event) {
            Ok(()) => self.send_response_inner(peer_id, id, event),
            Err(wait_time) => {
                self.delayed_responses.insert_at(
                    QueuedResponse {
                        connection_id: id.0,
                        substream_id: id.1,
                        peer_id,
                        response: event,
                    },
                    Instant::now() + wait_time,
                );
            }
        }
    }

    fn send_response_inner(
        &mut self,
        peer_id: PeerId,
        id: (ConnectionId, SubstreamId),
        event: RPCCodedResponse<E>,
    ) {
        self.events.push(ToSwarm::NotifyHandler {
            peer_id,
            handler: NotifyHandler::One(id.0),
            event: RPCSend::Response(id.1, event),
        })
    }

    /// Submits an RPC request.
    ///
    /// The peer must be connected for this to succeed.
    pub fn send_request(&mut self, peer_id: PeerId, request_id: Id, req: OutboundRequest<E>) {
        let event = if let Some(self_limiter) = self.outbound_request_limiter.as_mut() {
            match self_limiter.allows(peer_id, request_id, req) {
                Ok(event) => event,
                Err(_e) => {
                    // Request is logged and queued internally in the self rate limiter.
                    return;
                }
            }
        } else {
            ToSwarm::NotifyHandler {
                peer_id,
                handler: NotifyHandler::Any,
                event: RPCSend::Request(request_id, req),
            }
        };

        self.events.push(event);
    }

    /// Lighthouse wishes to disconnect from this peer by sending a Goodbye message. This
    /// gracefully terminates the RPC behaviour with a goodbye message.
    pub fn shutdown(&mut self, peer_id: PeerId, id: Id, reason: GoodbyeReason) {
        self.events.push(ToSwarm::NotifyHandler {
            peer_id,
            handler: NotifyHandler::Any,
            event: RPCSend::Shutdown(id, reason),
        });
    }

    fn is_request_size_too_large(&self, request: &InboundRequest<E>) -> bool {
        match request.protocol() {
            Protocol::Status
            | Protocol::Goodbye
            | Protocol::Ping
            | Protocol::MetaData
            | Protocol::LightClientBootstrap
            | Protocol::LightClientOptimisticUpdate
            | Protocol::LightClientFinalityUpdate
            // The RuntimeVariable ssz list ensures that we don't get more requests than the max specified in the config.
            | Protocol::BlocksByRoot
            | Protocol::BlobsByRoot
            | Protocol::DataColumnsByRoot => false,
            Protocol::BlocksByRange => request.max_responses() > self.fork_context.spec.max_request_blocks(self.fork_context.current_fork()) as u64,
            Protocol::BlobsByRange => request.max_responses() > self.fork_context.spec.max_request_blob_sidecars,
            Protocol::DataColumnsByRange => request.max_responses() > self.fork_context.spec.max_request_data_column_sidecars,
        }
    }
}

impl<Id, E> NetworkBehaviour for RPC<Id, E>
where
    E: EthSpec,
    Id: ReqId,
{
    type ConnectionHandler = RPCHandler<Id, E>;
    type ToSwarm = RPCMessage<Id, E>;

    fn handle_established_inbound_connection(
        &mut self,
        connection_id: ConnectionId,
        peer_id: PeerId,
        _local_addr: &libp2p::Multiaddr,
        _remote_addr: &libp2p::Multiaddr,
    ) -> Result<libp2p::swarm::THandler<Self>, libp2p::swarm::ConnectionDenied> {
        let protocol = SubstreamProtocol::new(
            RPCProtocol {
                fork_context: self.fork_context.clone(),
                max_rpc_size: max_rpc_size(&self.fork_context, self.network_params.max_chunk_size),
                enable_light_client_server: self.enable_light_client_server,
                phantom: PhantomData,
                ttfb_timeout: self.network_params.ttfb_timeout,
            },
            (),
        );
        let log = self
            .log
            .new(slog::o!("peer_id" => peer_id.to_string(), "connection_id" => connection_id.to_string()));
        let handler = RPCHandler::new(
            protocol,
            self.fork_context.clone(),
            &log,
            self.network_params.resp_timeout,
            self.response_limiter
                .as_ref()
                .map(|response_limiter| (peer_id, response_limiter.clone())),
        );

        Ok(handler)
    }

    fn handle_established_outbound_connection(
        &mut self,
        connection_id: ConnectionId,
        peer_id: PeerId,
        _addr: &libp2p::Multiaddr,
        _role_override: libp2p::core::Endpoint,
        _port_use: PortUse,
    ) -> Result<libp2p::swarm::THandler<Self>, libp2p::swarm::ConnectionDenied> {
        let protocol = SubstreamProtocol::new(
            RPCProtocol {
                fork_context: self.fork_context.clone(),
                max_rpc_size: max_rpc_size(&self.fork_context, self.network_params.max_chunk_size),
                enable_light_client_server: self.enable_light_client_server,
                phantom: PhantomData,
                ttfb_timeout: self.network_params.ttfb_timeout,
            },
            (),
        );

        let log = self
            .log
            .new(slog::o!("peer_id" => peer_id.to_string(), "connection_id" => connection_id.to_string()));

        let handler = RPCHandler::new(
            protocol,
            self.fork_context.clone(),
            &log,
            self.network_params.resp_timeout,
            self.response_limiter
                .as_ref()
                .map(|response_limiter| (peer_id, response_limiter.clone())),
        );

        Ok(handler)
    }

    fn on_swarm_event(&mut self, event: FromSwarm) {
        // NOTE: FromSwarm is a non exhaustive enum so updates should be based on release notes more
        // than compiler feedback
        // The self rate limiter holds on to requests and attempts to process them within our rate
        // limits. If a peer disconnects whilst we are self-rate limiting, we want to terminate any
        // pending requests and return an error response to the application.

        if let FromSwarm::ConnectionClosed(ConnectionClosed {
            peer_id,
            remaining_established,
            connection_id,
            ..
        }) = event
        {
            // If there are still connections remaining, do nothing.
            if remaining_established > 0 {
                return;
            }
            // Get a list of pending requests from the self rate limiter
            if let Some(limiter) = self.outbound_request_limiter.as_mut() {
                for (id, proto) in limiter.peer_disconnected(peer_id) {
                    let error_msg = ToSwarm::GenerateEvent(RPCMessage {
                        peer_id,
                        conn_id: connection_id,
                        event: HandlerEvent::Err(HandlerErr::Outbound {
                            id,
                            proto,
                            error: RPCError::Disconnected,
                        }),
                    });
                    self.events.push(error_msg);
                }
            }

            // Replace the pending Requests to the disconnected peer
            // with reports of failed requests.
            self.events.iter_mut().for_each(|event| match &event {
                ToSwarm::NotifyHandler {
                    peer_id: p,
                    event: RPCSend::Request(request_id, req),
                    ..
                } if *p == peer_id => {
                    *event = ToSwarm::GenerateEvent(RPCMessage {
                        peer_id,
                        conn_id: connection_id,
                        event: HandlerEvent::Err(HandlerErr::Outbound {
                            id: *request_id,
                            proto: req.versioned_protocol().protocol(),
                            error: RPCError::Disconnected,
                        }),
                    });
                }
                _ => {}
            });
        }
    }

    fn on_connection_handler_event(
        &mut self,
        peer_id: PeerId,
        conn_id: ConnectionId,
        event: <Self::ConnectionHandler as ConnectionHandler>::ToBehaviour,
    ) {
        match event {
            HandlerEvent::Ok(RPCReceived::Request(ref id, ref req)) => {
                if !self.active_inbound_requests_limiter.allows(
                    peer_id,
                    req.versioned_protocol().protocol(),
                    &conn_id,
                    id,
                ) {
                    // There is already an active request with the same protocol. Send an error code to the peer.
                    debug!(self.log, "There is an active request with the same protocol"; "peer_id" => peer_id.to_string(), "request" => %req, "protocol" => %req.versioned_protocol().protocol());
                    self.send_response(
                        peer_id,
                        (conn_id, *id),
                        RPCCodedResponse::Error(
                            RPCResponseErrorCode::RateLimited,
                            "Rate limited. There is an active request with the same protocol"
                                .into(),
                            req.versioned_protocol().protocol(),
                        ),
                    );
                    return;
                }

                if self.is_request_size_too_large(req) {
                    // The request requires responses greater than the number defined in the spec.
                    debug!(self.log, "Request too large to process"; "request" => %req, "protocol" => %req.versioned_protocol().protocol());
                    // Send an error code to the peer.
                    // The handler upon receiving the error code will send it back to the behaviour
                    self.send_response(
                        peer_id,
                        (conn_id, *id),
                        RPCCodedResponse::Error(
                            RPCResponseErrorCode::InvalidRequest,
                            "The request requires responses greater than the number defined in the spec.".into(),
                            req.versioned_protocol().protocol(),
                        ),
                    );
                } else {
                    // Send the event to the user
                    self.events.push(ToSwarm::GenerateEvent(RPCMessage {
                        peer_id,
                        conn_id,
                        event,
                    }))
                }
            }
            HandlerEvent::Close(_) => {
                // Handle the close event here.
                self.active_inbound_requests_limiter.remove_peer(&peer_id);
                self.events.push(ToSwarm::CloseConnection {
                    peer_id,
                    connection: CloseConnection::All,
                });
            }
            _ => {
                self.events.push(ToSwarm::GenerateEvent(RPCMessage {
                    peer_id,
                    conn_id,
                    event,
                }));
            }
        }
    }

    fn poll(&mut self, cx: &mut Context) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        // let the rate limiter prune.
        if let Some(response_limiter) = self.response_limiter.as_ref() {
            let _ = response_limiter.lock().poll_unpin(cx);
        }

        if let Some(self_limiter) = self.outbound_request_limiter.as_mut() {
            if let Poll::Ready(event) = self_limiter.poll_ready(cx) {
                self.events.push(event)
            }
        }

        match self.delayed_responses.poll_expired(cx) {
            Poll::Ready(Some(queued_response)) => {
                let QueuedResponse {
                    peer_id,
                    connection_id,
                    substream_id,
                    response,
                } = queued_response.into_inner();
                debug!(
                    self.log,
                    "Sending delayed response";
                    "peer_id" => %peer_id
                );
                self.send_response_inner(peer_id, (connection_id, substream_id), response);
            }
            // `Poll::Ready(None)` means that there are no more entries in the delay queue and we
            // will continue to get this result until something else is added into the queue.
            Poll::Ready(None) | Poll::Pending => (),
        }

        if !self.events.is_empty() {
            return Poll::Ready(self.events.remove(0));
        }

        Poll::Pending
    }
}

impl<Id, E> slog::KV for RPCMessage<Id, E>
where
    E: EthSpec,
    Id: ReqId,
{
    fn serialize(
        &self,
        _record: &slog::Record,
        serializer: &mut dyn slog::Serializer,
    ) -> slog::Result {
        serializer.emit_arguments("peer_id", &format_args!("{}", self.peer_id))?;
        match &self.event {
            HandlerEvent::Ok(received) => {
                let (msg_kind, protocol) = match received {
                    RPCReceived::Request(_, req) => {
                        ("request", req.versioned_protocol().protocol())
                    }
                    RPCReceived::Response(_, res) => ("response", res.protocol()),
                    RPCReceived::EndOfStream(_, end) => (
                        "end_of_stream",
                        match end {
                            ResponseTermination::BlocksByRange => Protocol::BlocksByRange,
                            ResponseTermination::BlocksByRoot => Protocol::BlocksByRoot,
                            ResponseTermination::BlobsByRange => Protocol::BlobsByRange,
                            ResponseTermination::BlobsByRoot => Protocol::BlobsByRoot,
                            ResponseTermination::DataColumnsByRoot => Protocol::DataColumnsByRoot,
                            ResponseTermination::DataColumnsByRange => Protocol::DataColumnsByRange,
                        },
                    ),
                };
                serializer.emit_str("msg_kind", msg_kind)?;
                serializer.emit_arguments("protocol", &format_args!("{}", protocol))?;
            }
            HandlerEvent::Err(error) => {
                let (msg_kind, protocol) = match &error {
                    HandlerErr::Inbound { proto, .. } => ("inbound_err", *proto),
                    HandlerErr::Outbound { proto, .. } => ("outbound_err", *proto),
                };
                serializer.emit_str("msg_kind", msg_kind)?;
                serializer.emit_arguments("protocol", &format_args!("{}", protocol))?;
            }
            HandlerEvent::Close(err) => {
                serializer.emit_arguments("handler_close", &format_args!("{}", err))?;
            }
        };

        slog::Result::Ok(())
    }
}
