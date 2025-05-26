use std::{fmt, time::Instant};

use derive_more::derive::IsVariant;

/// The status of the driver, indicating its current operational state.
/// Here is a diagram that shows the different states and transitions:
///
/// ```text
///     TIME:  |------------------------------------------------------------------------>
///   EPOCHS:  0------------------------------------1----------------------------------2
///    SLOTS:  | 0 | 1 | 2 | .. | 28 | 29 | 30 | 31 | 32 | 33 | .. | 60 | 61 | 62 | 63 |
///   EVENTS: (1)              (2)       (3)       (4)            (5)                 (6)
///      SEQ:  |--------------------------|xxxxxxxxxxxxxxxxxxxxxxxx|-------------------|
///     INCL:  |------------------------------------|xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx|
/// ```
///
/// Where the events are:
/// 1) The driver starts in the `Inactive` state while someone else is the active sequencer
/// 2) The driver starts its `HandoverWait` period to let the previous sequencer blocks propagate
/// 3) The driver enters its `SequencingOnly` state and starts building blocks
/// 4) The driver enters its inclusion window too, becoming fully `Active`
/// 5) The driver's sequencing window ends, but it can still include batches: `InclusionOnly`
/// 6) The driver's handover window ends, becoming `Inactive` again
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, IsVariant)]
pub(crate) enum DriverStatus {
    /// The driver is inactive and not producing or proposing blocks.
    Inactive,
    /// The driver is in its sequencing window but it's not past the handover wait.
    /// The `start` field is the timestamp when the handover wait started.
    HandoverWait { start: Instant },
    /// The driver is in its sequencing window and it's past the handover wait, but not yet able to
    /// propose blocks. This also corresponds with the previous sequencer's handover window.
    SequencingOnly,
    /// The driver is in its inclusion window but not able to produce new blocks anymore.
    /// This also corresponds with our handover window.
    InclusionOnly,
    /// The driver is able to produce new blocks and propose them.
    Active,
}

impl DriverStatus {
    /// Check if the driver is in a state where it can propose L1 batches.
    pub(crate) const fn can_propose(&self) -> bool {
        matches!(self, Self::InclusionOnly | Self::Active)
    }

    /// Check if the driver is in a state where it can produce L2 blocks.
    pub(crate) const fn can_sequence(&self) -> bool {
        matches!(self, Self::SequencingOnly | Self::Active)
    }
}

impl DriverStatus {
    /// Returns an iterable slice of the enum variants.
    pub(crate) const fn variant_names() -> &'static [&'static str; 5] {
        &["Inactive", "HandoverWait", "SequencingOnly", "InclusionOnly", "Active"]
    }
}

impl fmt::Display for DriverStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inactive => write!(f, "Inactive"),
            Self::SequencingOnly => write!(f, "SequencingOnly"),
            Self::InclusionOnly => write!(f, "InclusionOnly"),
            Self::Active => write!(f, "Active"),
            Self::HandoverWait { start: _ } => {
                write!(f, "HandoverWait")
            }
        }
    }
}

impl fmt::Debug for DriverStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inactive => write!(f, "Inactive"),
            Self::SequencingOnly => write!(f, "SequencingOnly"),
            Self::InclusionOnly => write!(f, "InclusionOnly"),
            Self::Active => write!(f, "Active"),
            Self::HandoverWait { start } => {
                write!(f, "HandoverWait({}ms)", start.elapsed().as_millis())
            }
        }
    }
}
