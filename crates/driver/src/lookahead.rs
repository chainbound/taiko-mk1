use std::{fmt::Debug, marker::PhantomData, ops::Range, time::Instant};

use alloy::eips::merge::EPOCH_SLOTS;
use mk1_chainio::taiko::preconf_whitelist::TaikoPreconfWhitelist;
use mk1_primitives::{
    Epoch, EpochUtils as _, Slot, SlotUtils as _,
    time::{Timestamp, current_timestamp_seconds},
};
use tracing::{debug, error, trace};

use crate::{config::RuntimeConfig, metrics::DriverMetrics, status::DriverStatus};

/// The lookahead is the range of slots that the driver is allowed to include and sequence.
/// It is calculated based on the current and next epoch's sequencer in the
/// [`TaikoPreconfWhitelist`] contract.
#[derive(Debug)]
pub(crate) struct Lookahead {
    cfg: RuntimeConfig,
    preconf_whitelist: TaikoPreconfWhitelist,
    sequencing_window: Window<Sequencing>,
    inclusion_window: Window<Inclusion>,
    end_of_sequencing_mark: Epoch,
}

impl Lookahead {
    /// Create a new [`Lookahead`] instance.
    pub(crate) fn new(cfg: RuntimeConfig) -> Self {
        let preconf_whitelist = TaikoPreconfWhitelist::from_address(
            cfg.l1.el_url.clone(),
            cfg.contracts.preconf_whitelist,
        );

        let offset = cfg.preconf.handover_window_slots;

        Self {
            cfg,
            preconf_whitelist,
            end_of_sequencing_mark: 0,
            inclusion_window: Window::default(),
            sequencing_window: Window::with_offset(offset),
        }
    }

    /// Synchronize the lookahead windows.
    ///
    /// NOTE: this info is as updated as the head slot. Also, given we query both the current and
    /// next epoch, we always have an epoch of buffer.
    pub(crate) async fn sync(&mut self, head_slot: Slot) -> DriverStatus {
        trace!(slot = %head_slot, "Syncing lookahead");

        if head_slot % EPOCH_SLOTS == 0 {
            // Wait for the next epoch to start before updating the lookahead
            return self.driver_status(head_slot);
        }

        let (current_operator, next_operator) = tokio::join!(
            self.preconf_whitelist.get_operator_for_current_epoch(),
            self.preconf_whitelist.get_operator_for_next_epoch()
        );

        let (current_operator, next_operator) = match (current_operator, next_operator) {
            (Ok(current), Ok(next)) => (current, next),
            (current, next) => {
                error!(?current, ?next, "Failed to read the operator lookahead");
                return self.driver_status(head_slot);
            }
        };

        let seq_current_epoch = current_operator == self.cfg.operator.private_key.address();
        let seq_next_epoch = next_operator == self.cfg.operator.private_key.address();

        DriverMetrics::set_current_operator(current_operator, head_slot);
        DriverMetrics::set_next_operator(next_operator, head_slot);
        DriverMetrics::set_operator_at_epoch(current_operator, head_slot.to_epoch());
        DriverMetrics::set_operator_at_epoch(next_operator, head_slot.to_epoch().saturating_add(1));

        self.inclusion_window.calculate(head_slot, seq_current_epoch, seq_next_epoch);

        self.sequencing_window.calculate(self.inclusion_window);

        let slots_left_in_epoch = EPOCH_SLOTS - head_slot % EPOCH_SLOTS;
        DriverMetrics::set_slots_left_in_epoch(slots_left_in_epoch);

        debug!(
            "🔃 Synced lookahead: slot={}, slots_left_in_epoch={}",
            head_slot, slots_left_in_epoch,
        );
        debug!("🔃 current_operator={}, next_operator={}", current_operator, next_operator);

        if !self.sequencing_window.is_empty() || !self.inclusion_window.is_empty() {
            debug!(
                "🔃 sequencing_window={:?}, inclusion_window={:?}",
                self.sequencing_window, self.inclusion_window,
            );
        }

        self.driver_status(head_slot)
    }

    /// Update the end of sequencing mark.
    pub(crate) const fn update_end_of_sequencing_mark(&mut self, epoch: Epoch) {
        self.end_of_sequencing_mark = epoch;
    }

    /// Returns the amount of sequencing slots left in the current sequencing window.
    pub(crate) fn sequencing_slots_left(&self, l1_head_slot: Slot) -> u64 {
        self.sequencing_window.end().saturating_sub(l1_head_slot)
    }

    /// Returns the end of the inclusion window.
    pub(crate) fn inclusion_window_end(&self) -> Slot {
        self.inclusion_window.end()
    }

    /// Returns true if the given L1 slot is in a "new" inclusion window.
    pub(crate) fn is_new_inclusion_window(&self, l1_head_slot: Slot) -> bool {
        self.inclusion_window.is_new(l1_head_slot)
    }

    /// Check if an address is in the whitelist.
    pub(crate) async fn is_whitelisted(&self) -> bool {
        let current_epoch = self.cfg.current_slot().to_epoch();
        let epoch_ts = self.cfg.l1_genesis_timestamp +
            current_epoch * EPOCH_SLOTS * self.cfg.chain.l1_block_time;
        let operator = self.cfg.operator.private_key.address();

        match self.preconf_whitelist.is_whitelisted(operator, epoch_ts).await {
            Ok(is_whitelisted) => is_whitelisted,
            Err(e) => {
                error!(?e, "Failed to check if address is whitelisted");
                false
            }
        }
    }

    /// Computes the driver status based on the current head slot and the
    /// current knowledge of the lookahead (without fetching the new lookahead)
    pub(crate) fn driver_status(&self, head_slot: u64) -> DriverStatus {
        if self.in_sequencing_window(head_slot) && self.in_inclusion_window(head_slot) {
            DriverStatus::Active
        } else if self.in_sequencing_window(head_slot) {
            if self.is_past_handover_buffer() {
                DriverStatus::SequencingOnly
            } else {
                DriverStatus::HandoverWait { start: Instant::now() }
            }
        } else if self.in_inclusion_window(head_slot) {
            DriverStatus::InclusionOnly
        } else {
            DriverStatus::Inactive
        }
    }

    /// Returns true if the given timestamp is the final one of the current sequencing window.
    /// This is only possible if we are the current epoch's sequencer. This information is
    /// used to determine if we should send the `EndOfSequencing` event.
    pub(crate) fn is_last_block_in_sequencing_window(&self, timestamp: Timestamp) -> bool {
        let slot = self.cfg.timestamp_to_l1_slot(timestamp);

        // If we aren't in the last slot of the sequencing window, it cannot be the last block.
        // NOTE: [`Window::end`] returns the non-inclusive end of the window.
        if self.sequencing_window.end().saturating_sub(1) != slot {
            return false;
        }

        // Check that the timestamp is exactly the one of the last block built in this slot
        // e.g. if 1 L1_slot=12s and L2_slot=2s, then last_ts is 10s into the L1 slot.
        let slot_ts = self.cfg.slot_to_timestamp(slot);
        let last_ts = slot_ts + self.cfg.chain.l1_block_time - self.cfg.chain.l2_block_time;

        timestamp == last_ts
    }

    /// Returns true if the given L1 slot is in the inclusion window.
    fn in_inclusion_window(&self, l1_head_slot: u64) -> bool {
        self.inclusion_window.contains(&l1_head_slot)
    }

    /// Returns true if the given L1 slot is in the sequencing window.
    fn in_sequencing_window(&self, l1_head_slot: u64) -> bool {
        self.sequencing_window.contains(&l1_head_slot)
    }

    /// Checks if the given timestamp is past the handover buffer.
    ///
    /// PRECONDITION: assumes we're in the beginning of the sequencing window.
    /// As an example, if the sequencing window is the range 28..60, we're at slot 29.
    fn is_past_handover_buffer(&self) -> bool {
        let timestamp = current_timestamp_seconds();

        // 1. Get the L1 slot of the current timestamp.
        let slot = self.cfg.timestamp_to_l1_slot(timestamp);

        // 2. If the EOS mark has already been received for the current epoch,
        // we can short-circuit and return true.
        if self.end_of_sequencing_mark == slot.to_epoch() {
            return true;
        }

        // 3. Calculate the first slot of the sequencing window.
        let seq_window_first_slot = slot
            .next_multiple_of(EPOCH_SLOTS)
            .saturating_sub(self.cfg.preconf.handover_window_slots);

        // 4. Calculate the timestamp of the beginning of the sequencing window.
        let seq_window_ts = self.cfg.slot_to_timestamp(seq_window_first_slot);

        // 5. Check if the difference between the given timestamp and the slot timestamp
        // is greater than or equal to the handover skip slots.
        timestamp.saturating_sub(seq_window_ts) >=
            self.cfg.preconf.handover_skip_slots * self.cfg.chain.l2_block_time
    }
}

/// The size of the inclusion and sequencing windows ring buffers, expressed in epochs.
const WINDOW_SIZE: usize = 3;

/// A type marker for the inclusion window.
#[derive(Debug, Clone, Copy)]
struct Inclusion;
/// A type marker for the sequencing window.
#[derive(Debug, Clone, Copy)]
struct Sequencing;

/// A trait marker for the inclusion and sequencing windows.
trait WindowTrait {}
impl WindowTrait for Inclusion {}
impl WindowTrait for Sequencing {}

/// A ring buffer for the inclusion and sequencing windows.
///
/// This ring buffer contains [`WINDOW_SIZE`] elements and exposes
/// an interface that matches as close as possible the one of [`std::ops::Range`]
#[derive(Clone, Copy)]
struct Window<T: WindowTrait> {
    /// The inner ring buffer, where each element contains a tuple marking
    /// whether sequencing or inclusion is possible at a certain epoch number.
    inner: [(bool, Epoch); WINDOW_SIZE],
    /// An offset to be applied to the epochs to calculate the range of slots in the window.
    offset: u64,
    /// A phantom data field to mark the type of window.
    _phantom: PhantomData<T>,
}

impl<T: WindowTrait> Debug for Window<T> {
    /// Format the inner buffer like multiple [`std::ops::Range`]s of [`Slot`]
    ///
    /// Example: 0..32,64..96
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let epoch_ranges = self
            .inner
            .iter()
            .filter(|(is_set, _)| *is_set)
            .map(|(_, epoch)| {
                let slot = epoch.to_slot();
                slot.saturating_sub(self.offset)..
                    slot.saturating_add(EPOCH_SLOTS).saturating_sub(self.offset)
            })
            .collect::<Vec<_>>();
        let ranges = merge_adjacent_ranges(epoch_ranges);
        let string = ranges.iter().map(|r| format!("{r:?}")).collect::<Vec<_>>().join(",");
        f.write_str(&string)
    }
}

impl Default for Window<Inclusion> {
    fn default() -> Self {
        Self { inner: [(false, 0); WINDOW_SIZE], offset: 0, _phantom: PhantomData }
    }
}

impl Window<Sequencing> {
    const fn with_offset(offset: u64) -> Self {
        Self { inner: [(false, 0); WINDOW_SIZE], offset, _phantom: PhantomData }
    }
}

impl Window<Inclusion> {
    /// Calculate the inclusion window based on the current and next epoch's sequencer.
    fn calculate(
        &mut self,
        head_slot: Slot,
        sequencer_current_epoch: bool,
        sequencer_next_epoch: bool,
    ) {
        let current_epoch = head_slot.to_epoch();
        let current_epoch_start_slot = current_epoch.to_slot();
        if sequencer_current_epoch {
            self.apply_epoch(head_slot.to_epoch());
        } else if self.contains(&current_epoch_start_slot) {
            // Erase the stale entry in the ring buffer.
            self.reset_at_epoch(current_epoch);
        }

        let next_epoch = head_slot.to_epoch().saturating_add(1);
        let next_epoch_start_slot = next_epoch.to_slot();
        if sequencer_next_epoch {
            self.apply_epoch(head_slot.to_epoch().saturating_add(1));
        } else if self.contains(&next_epoch_start_slot) {
            // Erase the stale entry in the ring buffer.
            self.reset_at_epoch(next_epoch);
        }
    }

    /// Returns whether we are in a "new" inclusion window by performing the following checks:
    /// 1. At slot `head_slot` we're in the inclusion window;
    /// 2. At slot `head_slot.saturating_sub(EPOCH_SLOTS)` we were NOT in the inclusion window.
    fn is_new(&self, head_slot: Slot) -> bool {
        let active_now = self.contains(&head_slot);
        let active_previously = self.contains(&(head_slot.saturating_sub(EPOCH_SLOTS)));
        active_now && !active_previously
    }

    /// Manually erase an entry in the ring buffer.
    ///
    /// Use with caution!
    const fn reset_at_epoch(&mut self, epoch: Epoch) {
        let index = epoch as usize % WINDOW_SIZE;
        self.inner[index] = (false, 0);
    }
}

impl Window<Sequencing> {
    /// Calculate the sequencing window based exclusively on the inclusion window.
    fn calculate(&mut self, inclusion_window: Window<Inclusion>) {
        // NOTE: Since the sequencing window is calculated from an inclusion window,
        // we should reset the ring buffer to avoid stale entries.
        self.inner = [(false, 0); WINDOW_SIZE];

        let epochs =
            inclusion_window.inner.iter().filter(|(is_set, _)| *is_set).map(|(_, epoch)| epoch);
        for epoch in epochs {
            self.apply_epoch(*epoch);
        }
    }
}

impl<T: WindowTrait> Window<T> {
    // Convert the epoch to a range of slots, and subtracting the slot offset.
    fn epoch_to_range_with_offset(&self, epoch: Epoch) -> Range<Slot> {
        let range = epoch.to_slot_range();
        range.start.saturating_sub(self.offset)..range.end.saturating_sub(self.offset)
    }

    /// Apply an epoch to the buffer
    const fn apply_epoch(&mut self, epoch: Epoch) {
        self.inner[epoch as usize % WINDOW_SIZE] = (true, epoch);
    }

    /// Check if the given slot is part of the window, taking into account the offset.
    fn contains(&self, slot: &Slot) -> bool {
        let maybe_found = self
            .inner
            .iter()
            .find(|(_, epoch)| self.epoch_to_range_with_offset(*epoch).contains(slot));
        if let Some((is_set, _)) = maybe_found { *is_set } else { false }
    }

    /// Returns the end of the window, or [`Slot::MIN`] if the window is empty.
    ///
    /// NOTE: like [`std::ops::Range`], the end is excluded.
    fn end(&self) -> Slot {
        let (mut highest, mut set) = (Slot::MIN, false);
        for (is_set, epoch) in &self.inner {
            if *is_set && epoch >= &highest {
                highest = *epoch;
                set = true;
            }
        }
        if highest == Slot::MIN && !set {
            return Slot::MIN;
        }
        highest.to_slot().saturating_add(EPOCH_SLOTS).saturating_sub(self.offset)
    }

    /// Check if the window is empty.
    fn is_empty(&self) -> bool {
        self.inner.iter().all(|(is_set, _)| !*is_set)
    }
}

/// Merge adjacent ranges into a single range.
///
/// ```rs
/// let input = vec![0..32, 32..64, 100..120, 120..140];
/// let merged = merge_adjacent_ranges(input);
///
/// assert_eq!(merged.len(), 2);
/// assert_eq!(merged[0], 0..64);
/// assert_eq!(merged[1], 100..140);
/// ```
fn merge_adjacent_ranges<T: Ord + Eq + Clone>(mut ranges: Vec<Range<T>>) -> Vec<Range<T>> {
    if ranges.is_empty() {
        return ranges;
    }

    // Sort by start just in case
    ranges.sort_by_key(|r| r.start.clone());

    let mut collapsed = vec![];
    let mut current = ranges[0].clone();

    for r in ranges.into_iter().skip(1) {
        if current.end == r.start {
            // Extend the current range
            current.end = r.end;
        } else {
            collapsed.push(current);
            current = r;
        }
    }

    collapsed.push(current);
    collapsed
}

#[cfg(test)]
mod tests {
    use super::*;

    impl<T: WindowTrait> Window<T> {
        fn contains_range(&self, range: &Range<Slot>) -> bool {
            for i in range.clone().step_by(EPOCH_SLOTS as usize) {
                if !self.contains(&i) {
                    return false;
                }
            }
            true
        }
    }

    #[test]
    fn merge_adjacent_ranges_works() {
        #[allow(clippy::single_range_in_vec_init)]
        let input = vec![0..32];
        let merged = merge_adjacent_ranges(input);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0], 0..32);

        let input = vec![0..32, 32..64, 100..120, 120..140];
        let merged = merge_adjacent_ranges(input);

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], 0..64);
        assert_eq!(merged[1], 100..140);
    }

    #[test]
    fn calculate_inclusion_window_current_sequencer_works() {
        let head_slot = 16;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = false;

        let mut window = Window::<Inclusion>::default();
        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);

        let expected = 0..32;

        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);
    }

    #[test]
    fn calculate_inclusion_window_next_precofirmer_works() {
        let head_slot = 16;
        let sequencer_current_epoch = false;
        let sequencer_next_epoch = true;

        let mut window = Window::<Inclusion>::default();
        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);

        let expected = 32..64;

        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);
    }

    #[test]
    fn calculate_inclusion_window_current_and_next_precofirmer_works() {
        let head_slot = 16;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = true;

        let mut window = Window::<Inclusion>::default();
        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);

        let expected = 0..64;

        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);
    }

    #[test]
    fn calculate_sequencing_window_not_precofirmer_works() {
        let head_slot = 16;
        let sequencer_current_epoch = false;
        let sequencer_next_epoch = false;

        let mut window = Window::<Inclusion>::default();
        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);

        assert!(window.is_empty(), "expected: empty, actual: {:?}", window);
    }

    #[test]
    fn calculate_new_inclusion_window_works() {
        let head_slot = 16;
        let sequencer_current_epoch = false;
        let sequencer_next_epoch = true;

        let mut window = Window::<Inclusion>::default();
        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);

        let head_slot = 33;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = false;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        assert!(window.is_new(head_slot));

        let expected = 32..64;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);
    }

    #[test]
    fn window_contains_works() {
        let mut window = Window::<Inclusion>::default();

        let head_slot = 16;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = false;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);

        for i in 0..32 {
            assert!(window.contains(&i), "expected: true, actual: false");
        }
        assert!(!window.contains(&32), "expected: false, actual: true");
    }

    #[test]
    fn sequencing_window_calculation_works() {
        let mut inclusion_window = Window::<Inclusion>::default();
        let mut sequencing_window = Window::<Sequencing>::with_offset(4);

        let head_slot = 16;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = false;

        inclusion_window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        sequencing_window.calculate(inclusion_window);
        let expected = 0..28;

        assert!(
            sequencing_window.contains_range(&expected),
            "expected: {:?}, actual: {:?}",
            expected,
            sequencing_window,
        );

        let head_slot = 48;
        let sequencer_current_epoch = false;
        let sequencer_next_epoch = true;

        inclusion_window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        sequencing_window.calculate(inclusion_window);
        let expected = 0..28;

        assert!(
            sequencing_window.contains_range(&expected),
            "expected: {:?}, actual: {:?}",
            expected,
            sequencing_window,
        );

        let expected = 60..92;
        assert!(
            sequencing_window.contains_range(&expected),
            "expected: {:?}, actual: {:?}",
            expected,
            sequencing_window,
        );
    }

    #[test]
    fn sequencing_window_calculation_reset_works() {
        let mut inclusion_window = Window::<Inclusion>::default();
        let mut sequencing_window = Window::<Sequencing>::with_offset(4);

        let head_slot = 16;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = true;

        inclusion_window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        sequencing_window.calculate(inclusion_window);
        let expected = 0..60;

        assert!(
            sequencing_window.contains_range(&expected),
            "expected: {:?}, actual: {:?}",
            expected,
            sequencing_window,
        );

        let head_slot = 48;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = true;

        inclusion_window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        sequencing_window.calculate(inclusion_window);
        let expected = 0..92;

        assert!(
            sequencing_window.contains_range(&expected),
            "expected: {:?}, actual: {:?}",
            expected,
            sequencing_window,
        );

        // Assume the inclusion window has changed mid-epoch. The sequencing window
        // should reflect that as well.
        let head_slot = 80;
        let sequencer_current_epoch = false;
        let sequencer_next_epoch = true;

        // Now inclusion window is 32..64,96..128
        inclusion_window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        // The sequencing window should be 28..60,96..128 as well
        sequencing_window.calculate(inclusion_window);
        let expected = 28..60;

        assert!(
            sequencing_window.contains_range(&expected),
            "expected: {:?}, actual: {:?}",
            expected,
            sequencing_window,
        );

        let reset_range = 60..92;
        assert!(!sequencing_window.contains_range(&reset_range), "expected: false, actual: true");

        let expected = 92..124;

        assert!(
            sequencing_window.contains_range(&expected),
            "expected: {:?}, actual: {:?}",
            expected,
            sequencing_window,
        );
    }

    #[test]
    fn window_with_gaps_works() {
        let mut window = Window::<Inclusion>::default();

        let head_slot = 80;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = false;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);

        let head_slot = 112;
        let sequencer_current_epoch = false;
        let sequencer_next_epoch = true;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);

        let expected = 64..96;
        assert!(window.inner[2].0);
        let actual = window.inner[2].1.to_slot_range(); // (64 / 32) % 3 == 2
        assert_eq!(expected, actual, "expected: {:?}, actual: {:?}", expected, actual);

        let expected = 128..160;
        assert!(window.inner[1].0);
        let actual = window.inner[1].1.to_slot_range(); // (128 / 32) % 3 == 1
        assert_eq!(expected, actual, "expected: {:?}, actual: {:?}", expected, actual);

        assert!(!window.inner[0].0);
    }

    /// Test whether transitions like
    /// (false, false) -> (false, true) -> (true, true) -> (true, false) -> (false, false)
    /// works correctly.
    #[test]
    fn window_two_epoch_transition_works() {
        let mut window = Window::<Inclusion>::default();

        let head_slot = 48;
        let sequencer_current_epoch = false;
        let sequencer_next_epoch = false;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        let expected = 0..0;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);
        assert!(window.is_empty());

        let head_slot = 80;
        let sequencer_current_epoch = false;
        let sequencer_next_epoch = true;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        let expected = 96..128;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);

        let head_slot = 112;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = true;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        let expected = 96..160;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);

        let head_slot = 144;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = false;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        // The previous epoch remains!
        let expected = 96..160;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);

        let head_slot = 176;
        let sequencer_current_epoch = false;
        let sequencer_next_epoch = false;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        // Here an empty range has been calcualted and applied, so the inclusion window remains the
        // same as the previous iteration.
        let expected = 96..160;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);
        assert!(!window.is_empty());
    }

    /// Test whether the transition
    /// (true, true, true) -> (true, true, false) -> (true, false, false) -> (false, false, false)
    /// works correctly.
    #[test]
    fn window_three_epoch_transition_works() {
        let mut window = Window::<Inclusion>::default();

        let head_slot = 48;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = true;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        let expected = 32..96;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);

        let head_slot = 80;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = true;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        // We also have the previous epoch here.
        let expected = 32..128;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);

        let head_slot = 112;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = false;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        let expected = 32..128;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);

        let head_slot = 144;
        let sequencer_current_epoch = false;
        let sequencer_next_epoch = false;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);

        // Here an empty range has been calculated and applied, so the inclusion window remains the
        // same as the previous iteration.
        let expected = 32..128;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);

        let head_slot = 176;
        let sequencer_current_epoch = false;
        let sequencer_next_epoch = false;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        let expected = 32..128;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);
    }

    #[test]
    fn unexpected_current_sequencer_change_handled() {
        let mut window = Window::<Inclusion>::default();

        let head_slot = 48;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = true;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        let expected = 32..96;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);

        // For some unknown reason, now we're not anymore the sequencer in the next epoch.
        let sequencer_current_epoch = false;
        let sequencer_next_epoch = true;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        let expected = 64..96;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);
    }

    #[test]
    fn unexpected_next_sequencer_change_handled() {
        let mut window = Window::<Inclusion>::default();

        let head_slot = 48;
        let sequencer_current_epoch = true;
        let sequencer_next_epoch = true;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        let expected = 32..96;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);

        // For some unknown reason, now we're not anymore the sequencer in the next epoch.
        let sequencer_next_epoch = false;

        window.calculate(head_slot, sequencer_current_epoch, sequencer_next_epoch);
        let expected = 32..64;
        assert!(window.contains_range(&expected), "expected: {:?}, actual: {:?}", expected, window);
    }
}
