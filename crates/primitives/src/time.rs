use std::{
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::stream::Stream;
use tokio::time::{Instant, Interval, MissedTickBehavior};

use crate::Slot;

/// A UNIX timestamp in seconds.
pub type Timestamp = u64;

/// Helper function to convert a timestamp to a L1 slot,
/// using the L1 genesis timestamp and block time.
pub const fn timestamp_to_l1_slot(
    timestamp: Timestamp,
    genesis_timestamp: Timestamp,
    block_time: u64,
) -> Slot {
    (timestamp.saturating_sub(genesis_timestamp)) / block_time
}

/// Helper function to convert a L1 slot to a timestamp, using the L1 genesis timestamp and block
/// time.
pub const fn slot_to_timestamp(
    slot: Slot,
    genesis_timestamp: Timestamp,
    block_time: u64,
) -> Timestamp {
    genesis_timestamp + (slot * block_time)
}

/// Helper function to get the timestamp in seconds of the next slot.
pub fn next_l1_slot_timestamp(genesis_timestamp: Timestamp, block_time: u64) -> Timestamp {
    let current_timestamp = current_timestamp_seconds();
    let next_slot = timestamp_to_l1_slot(current_timestamp, genesis_timestamp, block_time) + 1;
    genesis_timestamp + (next_slot * block_time)
}

/// Get the current UNIX timestamp in seconds.
pub fn current_timestamp_seconds() -> Timestamp {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("Time went backwards").as_secs()
}

/// Get the current UNIX timestamp in milliseconds.
pub fn current_timestamp_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("Time went backwards").as_millis()
}

/// A tick from the slot clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotTick {
    /// A tick from the L1 clock.
    L1(Timestamp),
    /// A tick from the L2 clock.
    L2(Timestamp),
}

/// A clock that emits ticks from two slot intervals. It prioritizes L1 ticks over L2 ticks. L1
/// ticks are unconditionally emitted, while L2 ticks are only emitted if they are not skipped.
#[derive(Debug)]
pub struct SlotClock {
    l1_interval: Interval,
    l2_interval: Interval,
    cache: Option<SlotTick>,
}

impl SlotClock {
    /// Creates a new `SlotClock` that emits ticks from two slot intervals.
    /// Note that the `l1_interval` will be prioritized over `l2_interval`.
    /// `l2_interval` will be skipped if it has missed a tick.
    pub fn new(l1_interval: Duration, l2_interval: Duration) -> Self {
        Self::new_at(Instant::now(), l1_interval, l2_interval)
    }

    /// Creates a new `SlotClock` that emits ticks from two slot intervals starting at `start`.
    /// Note that the `l1_interval` will be prioritized over `l2_interval`.
    ///
    /// Intervals will be skipped if they've missed a tick on the receiver side,
    /// avoiding unnecessary bursts of old values.
    pub fn new_at(start: Instant, l1_interval: Duration, l2_interval: Duration) -> Self {
        let mut l1_interval = tokio::time::interval_at(start, l1_interval);
        l1_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut l2_interval = tokio::time::interval_at(start, l2_interval);
        l2_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        Self { l1_interval, l2_interval, cache: None }
    }

    /// Returns the next tick from the L1 clock.
    pub async fn next_l1_tick(&mut self) -> Instant {
        self.l1_interval.tick().await
    }
}

impl Stream for SlotClock {
    type Item = SlotTick;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(tick) = self.cache.take() {
            return Poll::Ready(Some(tick));
        }

        // Prioritize L1 ticks. If both are ready, cache the L2 tick and make sure we're woken up
        // again, so we can return it in the next poll.
        if Pin::new(&mut self.l1_interval).poll_tick(cx).is_ready() {
            if Pin::new(&mut self.l2_interval).poll_tick(cx).is_ready() {
                // Cache the L2 tick and make sure we're woken up again.
                self.cache = Some(SlotTick::L2(current_timestamp_seconds()));
                cx.waker().wake_by_ref();
            }

            return Poll::Ready(Some(SlotTick::L1(current_timestamp_seconds())));
        } else if Pin::new(&mut self.l2_interval).poll_tick(cx).is_ready() {
            // If only L2 is ready, return it.
            return Poll::Ready(Some(SlotTick::L2(current_timestamp_seconds())));
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures::StreamExt as _;
    use std::time::Duration;

    #[tokio::test]
    async fn test_slot_clock_priority_and_completeness() {
        // Create a SlotClock with short intervals for testing
        let l1_interval = Duration::from_millis(50);
        let l2_interval = Duration::from_millis(30);

        let mut clock = SlotClock::new(l1_interval, l2_interval);

        // Collect ticks for a period that should include multiple L1 and L2 ticks
        let test_duration = Duration::from_millis(200);
        let start = std::time::Instant::now();

        let mut l1_ticks = 0;
        let mut l2_ticks = 0;

        while std::time::Instant::now().duration_since(start) < test_duration {
            if let Some(tick) = clock.next().await {
                match tick {
                    SlotTick::L1(_) => {
                        l1_ticks += 1;
                    }
                    SlotTick::L2(_) => {
                        l2_ticks += 1;
                    }
                }
            }
        }

        // We should have received approximately the expected number of ticks
        // L1: test_duration / l1_interval ≈ 200ms / 50ms = 4 ticks
        // L2: test_duration / l2_interval ≈ 200ms / 30ms = 6-7 ticks

        // Allow for some timing variance in the test environment
        assert!(l1_ticks >= 3, "Expected at least 3 L1 ticks, got {}", l1_ticks);
        assert!(l2_ticks >= 5, "Expected at least 5 L2 ticks, got {}", l2_ticks);

        // Test priority - create a clock where both ticks will fire simultaneously
        let same_interval = Duration::from_millis(50);
        let mut clock = SlotClock::new(same_interval, same_interval);

        // Collect the first two ticks
        let first_tick = clock.next().await.unwrap();
        let second_tick = clock.next().await.unwrap();

        // First tick should be L1, second should be L2
        match (first_tick, second_tick) {
            (SlotTick::L1(_), SlotTick::L2(_)) => {
                // This is the expected case - L1 has priority
            }
            other => {
                panic!("Expected (L1, L2) priority order, got {:?}", other);
            }
        }
    }

    #[tokio::test]
    async fn test_clock_l2_skips() {
        // Arbitrarily high L1 interval
        let l1_interval = Duration::from_millis(100000);
        let l2_interval = Duration::from_millis(1000);

        let start = current_timestamp_seconds();
        let mut clock = SlotClock::new(l1_interval, l2_interval);

        // Discard first L1 tick
        clock.next().await.unwrap();

        let next_tick = clock.next().await.unwrap();
        assert_eq!(next_tick, SlotTick::L2(start));

        let delay_secs = 2;

        // Simulate a delay that would cause multiple L2 ticks to be missed
        tokio::time::sleep(Duration::from_millis(delay_secs * 1000)).await;

        // Get the next tick
        let next_tick = clock.next().await.unwrap();

        assert_eq!(next_tick, SlotTick::L2(start + delay_secs));
    }
}
