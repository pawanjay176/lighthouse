use crate::PeerId;
use crate::rpc::config::InboundRateLimiterConfig;
use crate::rpc::rate_limiter::RateLimiterItem;
use crate::rpc::rate_limiter::{RPCRateLimiter, RateLimitedErr};
use crate::rpc::self_limiter::timestamp_now;
use crate::rpc::{InboundRequestId, Protocol, RequestType};
use futures::FutureExt;
use libp2p::swarm::ConnectionId;
use logging::crit;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio_util::time::DelayQueue;
use tracing::debug;
use types::{EthSpec, ForkContext};

/// A request that was rate limited or waiting on rate limited requests for the same peer and
/// protocol.
#[derive(Clone)]
pub(super) struct QueuedRequest<E: EthSpec> {
    pub peer_id: PeerId,
    pub connection_id: ConnectionId,
    pub inbound_request_id: InboundRequestId,
    pub request_type: RequestType<E>,
    pub protocol: Protocol,
    pub queued_at: Duration,
}

pub(super) struct ResponseLimiter<E: EthSpec> {
    /// Rate limiter for inbound requests, charged by their expected response count.
    limiter: RPCRateLimiter,
    /// Requests queued for processing. These requests are stored when the limiter rejects them.
    delayed_requests: HashMap<(PeerId, Protocol), VecDeque<QueuedRequest<E>>>,
    /// The delay required to allow a peer's inbound request per protocol.
    next_request: DelayQueue<(PeerId, Protocol)>,
}

impl<E: EthSpec> ResponseLimiter<E> {
    /// Creates a new [`ResponseLimiter`] based on configuration values.
    pub fn new(
        config: InboundRateLimiterConfig,
        fork_context: Arc<ForkContext>,
    ) -> Result<Self, &'static str> {
        Ok(ResponseLimiter {
            limiter: RPCRateLimiter::new_with_config(config.0, fork_context)?,
            delayed_requests: HashMap::new(),
            next_request: DelayQueue::new(),
        })
    }

    /// Checks if the rate limiter allows the request. When not allowed, the request is delayed
    /// until it can be processed.
    pub fn allows(
        &mut self,
        peer_id: PeerId,
        connection_id: ConnectionId,
        inbound_request_id: InboundRequestId,
        request_type: RequestType<E>,
    ) -> bool {
        let protocol = request_type.protocol();

        // First check that there are not already other requests waiting to be processed.
        if let Some(queue) = self.delayed_requests.get_mut(&(peer_id, protocol)) {
            debug!(%peer_id, %protocol, "Inbound request rate limiting since there are already other requests waiting to be processed");
            queue.push_back(QueuedRequest {
                peer_id,
                connection_id,
                inbound_request_id,
                request_type,
                protocol,
                queued_at: timestamp_now(),
            });
            return false;
        }

        if let Err(wait_time) = Self::try_limiter(&mut self.limiter, peer_id, &request_type) {
            self.delayed_requests
                .entry((peer_id, protocol))
                .or_default()
                .push_back(QueuedRequest {
                    peer_id,
                    connection_id,
                    inbound_request_id,
                    request_type,
                    protocol,
                    queued_at: timestamp_now(),
                });
            self.next_request.insert((peer_id, protocol), wait_time);
            return false;
        }

        true
    }

    /// Checks if the limiter allows the request. If the request should be delayed, the duration
    /// to wait is returned.
    fn try_limiter(
        limiter: &mut RPCRateLimiter,
        peer_id: PeerId,
        request_type: &RequestType<E>,
    ) -> Result<(), Duration> {
        match limiter.allows(&peer_id, request_type) {
            Ok(()) => Ok(()),
            Err(e) => match e {
                RateLimitedErr::TooLarge => {
                    // This should never happen with default parameters. Let's just process the request.
                    // Log a crit since this is a config issue.
                    crit!(
                        protocol = %request_type.protocol(),
                        "Inbound request rate limiting error for a batch that will never fit. Processing request anyway. Check configuration parameters."
                    );
                    Ok(())
                }
                RateLimitedErr::TooSoon(wait_time) => {
                    debug!(%peer_id, protocol = %request_type.protocol(), wait_time_ms = wait_time.as_millis(), "Inbound request rate limiting");
                    Err(wait_time)
                }
            },
        }
    }

    /// Informs the limiter that a peer has disconnected. This removes any pending requests and
    /// returns their IDs.
    pub fn peer_disconnected(&mut self, peer_id: PeerId) -> Vec<InboundRequestId> {
        let mut dropped_requests = Vec::new();
        self.delayed_requests
            .retain(|(map_peer_id, _protocol), queue| {
                if map_peer_id == &peer_id {
                    dropped_requests.extend(queue.iter().map(|request| request.inbound_request_id));
                    false
                } else {
                    true
                }
            });
        dropped_requests
    }

    /// When a peer and protocol are allowed to process a next request, this function checks the
    /// queued requests and attempts marking as ready as many as the limiter allows.
    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Vec<QueuedRequest<E>>> {
        let mut requests = vec![];
        while let Poll::Ready(Some(expired)) = self.next_request.poll_expired(cx) {
            let (peer_id, protocol) = expired.into_inner();

            if let Entry::Occupied(mut entry) = self.delayed_requests.entry((peer_id, protocol)) {
                let queue = entry.get_mut();
                // Take delayed requests from the queue, as long as the limiter allows it.
                while let Some(request) = queue.pop_front() {
                    debug_assert_eq!(request.protocol, protocol);
                    match Self::try_limiter(
                        &mut self.limiter,
                        request.peer_id,
                        &request.request_type,
                    ) {
                        Ok(()) => {
                            metrics::observe_duration(
                                &crate::metrics::RESPONSE_IDLING,
                                timestamp_now().saturating_sub(request.queued_at),
                            );
                            requests.push(request)
                        }
                        Err(wait_time) => {
                            // The request was taken from the queue, but the limiter didn't allow it.
                            let request_protocol = request.protocol;
                            queue.push_front(request);
                            self.next_request
                                .insert((peer_id, request_protocol), wait_time);
                            break;
                        }
                    }
                }
                if queue.is_empty() {
                    entry.remove();
                }
            }
        }

        // Prune the rate limiter.
        let _ = self.limiter.poll_unpin(cx);

        if !requests.is_empty() {
            return Poll::Ready(requests);
        }
        Poll::Pending
    }
}
