//! A timer service for the beacon node.
//!
//! This service allows task execution on the beacon node for various functionality.

use beacon_chain::{BeaconChain, BeaconChainTypes};
use futures::stream::StreamExt;
use slog::info;
use slot_clock::SlotClock;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{interval_at, Instant};

/// Spawns a timer service which periodically executes tasks for the beacon chain
/// TODO: We might not need a `Handle` to the runtime since this function should be
/// called from the context of a runtime and we can simply spawn using task::spawn.
/// Check for issues without the Handle.
pub fn spawn<T: BeaconChainTypes>(
    beacon_chain: Arc<BeaconChain<T>>,
    milliseconds_per_slot: u64,
    log: slog::Logger,
) -> Result<tokio::sync::oneshot::Sender<()>, &'static str> {
    let (exit_signal, exit) = tokio::sync::oneshot::channel();

    let start_instant = Instant::now()
        + beacon_chain
            .slot_clock
            .duration_to_next_slot()
            .ok_or_else(|| "slot_notifier unable to determine time to next slot")?;

    // Warning: `interval_at` panics if `milliseconds_per_slot` = 0.
    let mut interval = interval_at(start_instant, Duration::from_millis(milliseconds_per_slot));
    let timer_future = async move {
        while interval.next().await.is_some() {
            beacon_chain.per_slot_task();
        }
    };

    let log_1 = log.clone();
    let exit_future = async move {
        let _ = exit.await.ok();
        info!(log_1, "Timer service shutdown");
    };

    let future = futures::future::select(Box::pin(timer_future), Box::pin(exit_future));
    tokio::spawn(future);
    info!(log, "Timer service started");

    Ok(exit_signal)
}
