use super::SlotClock;
use parking_lot::RwLock;
use std::ops::Add;
use std::sync::Arc;
use std::time::Duration;
use types::Slot;

/// Determines the present slot based upon a manually-incremented UNIX timestamp.
pub struct ManualSlotClock {
    genesis_slot: Slot,
    /// Duration from UNIX epoch to genesis.
    genesis_duration: Duration,
    /// Duration from UNIX epoch to right now.
    current_time: Arc<RwLock<Duration>>,
    /// The length of each slot.
    slot_duration: Duration,
    slot_duration_change: Option<(Slot, Duration)>,
}

impl Clone for ManualSlotClock {
    fn clone(&self) -> Self {
        ManualSlotClock {
            genesis_slot: self.genesis_slot,
            genesis_duration: self.genesis_duration,
            current_time: Arc::clone(&self.current_time),
            slot_duration: self.slot_duration,
            slot_duration_change: self.slot_duration_change,
        }
    }
}

impl ManualSlotClock {
    pub fn set_slot(&self, slot: u64) {
        *self.current_time.write() = self.start_of(Slot::new(slot)).expect("valid slot");
    }

    pub fn set_current_time(&self, duration: Duration) {
        *self.current_time.write() = duration;
    }

    pub fn advance_time(&self, duration: Duration) {
        let current_time = *self.current_time.read();
        *self.current_time.write() = current_time.add(duration);
    }

    pub fn advance_slot(&self) {
        self.set_slot(self.now().unwrap().as_u64() + 1)
    }

    pub fn genesis_duration(&self) -> &Duration {
        &self.genesis_duration
    }

    /// Returns the duration from `now` until the start of `slot`.
    ///
    /// Will return `None` if `now` is later than the start of `slot`.
    pub fn duration_to_slot(&self, slot: Slot, now: Duration) -> Option<Duration> {
        self.start_of(slot)?.checked_sub(now)
    }

    /// Returns the duration between `now` and the start of the next slot.
    pub fn duration_to_next_slot_from(&self, now: Duration) -> Option<Duration> {
        if now < self.genesis_duration {
            self.genesis_duration.checked_sub(now)
        } else {
            self.duration_to_slot(self.slot_of(now)? + 1, now)
        }
    }

    /// Returns the duration between `now` and the start of the next epoch.
    pub fn duration_to_next_epoch_from(
        &self,
        now: Duration,
        slots_per_epoch: u64,
    ) -> Option<Duration> {
        if now < self.genesis_duration {
            self.genesis_duration.checked_sub(now)
        } else {
            let next_epoch_start_slot =
                (self.slot_of(now)?.epoch(slots_per_epoch) + 1).start_slot(slots_per_epoch);

            self.duration_to_slot(next_epoch_start_slot, now)
        }
    }
}

impl SlotClock for ManualSlotClock {
    fn new(genesis_slot: Slot, genesis_duration: Duration, slot_duration: Duration) -> Self {
        if slot_duration.as_millis() == 0 {
            panic!("ManualSlotClock cannot have a < 1ms slot duration");
        }

        Self {
            genesis_slot,
            current_time: Arc::new(RwLock::new(genesis_duration)),
            genesis_duration,
            slot_duration,
            slot_duration_change: None,
        }
    }

    fn now(&self) -> Option<Slot> {
        self.slot_of(*self.current_time.read())
    }

    fn is_prior_to_genesis(&self) -> Option<bool> {
        Some(*self.current_time.read() < self.genesis_duration)
    }

    fn now_duration(&self) -> Option<Duration> {
        Some(*self.current_time.read())
    }

    fn slot_of(&self, now: Duration) -> Option<Slot> {
        let genesis = self.genesis_duration;

        if now >= genesis {
            let since_genesis = now
                .checked_sub(genesis)
                .expect("Control flow ensures now is greater than or equal to genesis");
            let (elapsed, base_slot, duration) = match self.slot_duration_change {
                Some((fork_slot, new_duration)) if self.start_of(fork_slot)? <= now => (
                    now.checked_sub(self.start_of(fork_slot)?)?,
                    fork_slot,
                    new_duration,
                ),
                _ => (since_genesis, self.genesis_slot, self.slot_duration),
            };
            let offset = u64::try_from(elapsed.as_millis() / duration.as_millis()).ok()?;
            Some(Slot::new(base_slot.as_u64().checked_add(offset)?))
        } else {
            None
        }
    }

    fn duration_to_next_slot(&self) -> Option<Duration> {
        self.duration_to_next_slot_from(*self.current_time.read())
    }

    fn duration_to_next_epoch(&self, slots_per_epoch: u64) -> Option<Duration> {
        self.duration_to_next_epoch_from(*self.current_time.read(), slots_per_epoch)
    }

    fn slot_duration(&self) -> Duration {
        self.now()
            .map(|slot| self.slot_duration_at(slot))
            .unwrap_or(self.slot_duration)
    }

    fn genesis_slot_duration(&self) -> Duration {
        self.slot_duration
    }

    fn slot_duration_at(&self, slot: Slot) -> Duration {
        match self.slot_duration_change {
            Some((fork_slot, duration)) if slot >= fork_slot => duration,
            _ => self.slot_duration,
        }
    }

    fn with_slot_duration_change(mut self, fork_slot: Slot, duration: Duration) -> Self {
        assert!(
            duration.as_millis() > 0,
            "slot duration must be at least 1ms"
        );
        self.slot_duration_change = Some((fork_slot, duration));
        self
    }

    fn slot_duration_change(&self) -> Option<(Slot, Duration)> {
        self.slot_duration_change
    }

    fn duration_to_slot(&self, slot: Slot) -> Option<Duration> {
        self.duration_to_slot(slot, *self.current_time.read())
    }

    /// Returns the duration between UNIX epoch and the start of `slot`.
    fn start_of(&self, slot: Slot) -> Option<Duration> {
        let slot_number = slot.as_u64().checked_sub(self.genesis_slot.as_u64())?;
        let elapsed = if let Some((fork_slot, new_duration)) = self.slot_duration_change {
            let fork_offset = fork_slot.as_u64().checked_sub(self.genesis_slot.as_u64())?;
            let old_slots = slot_number.min(fork_offset);
            let new_slots = slot_number.saturating_sub(fork_offset);
            self.slot_duration
                .checked_mul(u32::try_from(old_slots).ok()?)?
                .checked_add(new_duration.checked_mul(u32::try_from(new_slots).ok()?)?)?
        } else {
            self.slot_duration
                .checked_mul(u32::try_from(slot_number).ok()?)?
        };
        self.genesis_duration.checked_add(elapsed)
    }

    fn genesis_slot(&self) -> Slot {
        self.genesis_slot
    }

    fn genesis_duration(&self) -> Duration {
        self.genesis_duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_change_and_frozen_history() {
        let clock = ManualSlotClock::new(
            Slot::new(0),
            Duration::from_secs(100),
            Duration::from_secs(12),
        )
        .with_slot_duration_change(Slot::new(32), Duration::from_secs(8));
        assert_eq!(
            clock.start_of(Slot::new(31)),
            Some(Duration::from_secs(472))
        );
        assert_eq!(
            clock.start_of(Slot::new(32)),
            Some(Duration::from_secs(484))
        );
        assert_eq!(
            clock.start_of(Slot::new(34)),
            Some(Duration::from_secs(500))
        );
        assert_eq!(clock.slot_of(Duration::from_secs(483)), Some(Slot::new(31)));
        assert_eq!(clock.slot_of(Duration::from_secs(484)), Some(Slot::new(32)));
        assert_eq!(clock.slot_of(Duration::from_secs(499)), Some(Slot::new(33)));
        clock.set_current_time(Duration::from_secs(483));
        assert_eq!(clock.duration_to_next_slot(), Some(Duration::from_secs(1)));
        clock.set_current_time(Duration::from_secs(484));
        assert_eq!(clock.duration_to_next_slot(), Some(Duration::from_secs(8)));
        clock.set_current_time(Duration::from_secs(500));
        let frozen = clock.freeze_at(Duration::from_secs(499));
        assert_eq!(frozen.now(), Some(Slot::new(33)));
        assert_eq!(
            frozen.start_of(Slot::new(31)),
            clock.start_of(Slot::new(31))
        );
        assert_eq!(frozen.slot_duration(), Duration::from_secs(8));
        assert_eq!(
            frozen.millis_from_current_slot_start(),
            Some(Duration::from_secs(7))
        );
    }

    #[test]
    fn test_slot_now() {
        let clock = ManualSlotClock::new(
            Slot::new(10),
            Duration::from_secs(0),
            Duration::from_secs(1),
        );
        assert_eq!(clock.now(), Some(Slot::new(10)));
        clock.set_slot(123);
        assert_eq!(clock.now(), Some(Slot::new(123)));
    }

    #[test]
    fn test_is_prior_to_genesis() {
        let genesis_secs = 1;

        let clock = ManualSlotClock::new(
            Slot::new(0),
            Duration::from_secs(genesis_secs),
            Duration::from_secs(1),
        );

        *clock.current_time.write() = Duration::from_secs(genesis_secs - 1);
        assert!(clock.is_prior_to_genesis().unwrap(), "prior to genesis");

        *clock.current_time.write() = Duration::from_secs(genesis_secs);
        assert!(!clock.is_prior_to_genesis().unwrap(), "at genesis");

        *clock.current_time.write() = Duration::from_secs(genesis_secs + 1);
        assert!(!clock.is_prior_to_genesis().unwrap(), "after genesis");
    }

    #[test]
    fn start_of() {
        // Genesis slot and genesis duration 0.
        let clock =
            ManualSlotClock::new(Slot::new(0), Duration::from_secs(0), Duration::from_secs(1));
        assert_eq!(clock.start_of(Slot::new(0)), Some(Duration::from_secs(0)));
        assert_eq!(clock.start_of(Slot::new(1)), Some(Duration::from_secs(1)));
        assert_eq!(clock.start_of(Slot::new(2)), Some(Duration::from_secs(2)));

        // Genesis slot 1 and genesis duration 10.
        let clock = ManualSlotClock::new(
            Slot::new(0),
            Duration::from_secs(10),
            Duration::from_secs(1),
        );
        assert_eq!(clock.start_of(Slot::new(0)), Some(Duration::from_secs(10)));
        assert_eq!(clock.start_of(Slot::new(1)), Some(Duration::from_secs(11)));
        assert_eq!(clock.start_of(Slot::new(2)), Some(Duration::from_secs(12)));

        // Genesis slot 1 and genesis duration 0.
        let clock =
            ManualSlotClock::new(Slot::new(1), Duration::from_secs(0), Duration::from_secs(1));
        assert_eq!(clock.start_of(Slot::new(0)), None);
        assert_eq!(clock.start_of(Slot::new(1)), Some(Duration::from_secs(0)));
        assert_eq!(clock.start_of(Slot::new(2)), Some(Duration::from_secs(1)));

        // Genesis slot 1 and genesis duration 10.
        let clock = ManualSlotClock::new(
            Slot::new(1),
            Duration::from_secs(10),
            Duration::from_secs(1),
        );
        assert_eq!(clock.start_of(Slot::new(0)), None);
        assert_eq!(clock.start_of(Slot::new(1)), Some(Duration::from_secs(10)));
        assert_eq!(clock.start_of(Slot::new(2)), Some(Duration::from_secs(11)));
    }

    #[test]
    fn test_duration_to_next_slot() {
        let slot_duration = Duration::from_secs(1);

        // Genesis time is now.
        let clock = ManualSlotClock::new(Slot::new(0), Duration::from_secs(0), slot_duration);
        *clock.current_time.write() = Duration::from_secs(0);
        assert_eq!(clock.duration_to_next_slot(), Some(Duration::from_secs(1)));

        // Genesis time is in the future.
        let clock = ManualSlotClock::new(Slot::new(0), Duration::from_secs(10), slot_duration);
        *clock.current_time.write() = Duration::from_secs(0);
        assert_eq!(clock.duration_to_next_slot(), Some(Duration::from_secs(10)));

        // Genesis time is in the past.
        let clock = ManualSlotClock::new(Slot::new(0), Duration::from_secs(0), slot_duration);
        *clock.current_time.write() = Duration::from_secs(10);
        assert_eq!(clock.duration_to_next_slot(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn test_duration_to_next_epoch() {
        let slot_duration = Duration::from_secs(1);
        let slots_per_epoch = 32;

        // Genesis time is now.
        let clock = ManualSlotClock::new(Slot::new(0), Duration::from_secs(0), slot_duration);
        *clock.current_time.write() = Duration::from_secs(0);
        assert_eq!(
            clock.duration_to_next_epoch(slots_per_epoch),
            Some(Duration::from_secs(32))
        );

        // Genesis time is in the future.
        let clock = ManualSlotClock::new(Slot::new(0), Duration::from_secs(10), slot_duration);
        *clock.current_time.write() = Duration::from_secs(0);
        assert_eq!(
            clock.duration_to_next_epoch(slots_per_epoch),
            Some(Duration::from_secs(10))
        );

        // Genesis time is in the past.
        let clock = ManualSlotClock::new(Slot::new(0), Duration::from_secs(0), slot_duration);
        *clock.current_time.write() = Duration::from_secs(10);
        assert_eq!(
            clock.duration_to_next_epoch(slots_per_epoch),
            Some(Duration::from_secs(22))
        );

        // Genesis time is in the past.
        let clock = ManualSlotClock::new(
            Slot::new(0),
            Duration::from_secs(0),
            Duration::from_secs(12),
        );
        *clock.current_time.write() = Duration::from_secs(72_333);
        assert!(clock.duration_to_next_epoch(slots_per_epoch).is_some(),);
    }

    #[test]
    fn test_tolerance() {
        let clock = ManualSlotClock::new(
            Slot::new(0),
            Duration::from_secs(10),
            Duration::from_secs(1),
        );

        // Set clock to the 0'th slot.
        *clock.current_time.write() = Duration::from_secs(10);
        assert_eq!(
            clock
                .now_with_future_tolerance(Duration::from_secs(0))
                .unwrap(),
            Slot::new(0),
            "future tolerance of zero should return current slot"
        );
        assert_eq!(
            clock
                .now_with_past_tolerance(Duration::from_secs(0))
                .unwrap(),
            Slot::new(0),
            "past tolerance of zero should return current slot"
        );
        assert_eq!(
            clock
                .now_with_future_tolerance(Duration::from_millis(10))
                .unwrap(),
            Slot::new(0),
            "insignificant future tolerance should return current slot"
        );
        assert_eq!(
            clock
                .now_with_past_tolerance(Duration::from_millis(10))
                .unwrap(),
            Slot::new(0),
            "past tolerance that precedes genesis should return genesis slot"
        );

        // Set clock to part-way through the 1st slot.
        *clock.current_time.write() = Duration::from_millis(11_200);
        assert_eq!(
            clock
                .now_with_future_tolerance(Duration::from_secs(0))
                .unwrap(),
            Slot::new(1),
            "future tolerance of zero should return current slot"
        );
        assert_eq!(
            clock
                .now_with_past_tolerance(Duration::from_secs(0))
                .unwrap(),
            Slot::new(1),
            "past tolerance of zero should return current slot"
        );
        assert_eq!(
            clock
                .now_with_future_tolerance(Duration::from_millis(800))
                .unwrap(),
            Slot::new(2),
            "significant future tolerance should return next slot"
        );
        assert_eq!(
            clock
                .now_with_past_tolerance(Duration::from_millis(201))
                .unwrap(),
            Slot::new(0),
            "significant past tolerance should return previous slot"
        );
    }
}
