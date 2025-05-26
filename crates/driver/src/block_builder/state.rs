use alloy::rpc::types::Header;
use mk1_primitives::taiko::batch::PreconfirmedBlock;
use tokio::sync::watch;

/// The possible states of the block builder.
#[derive(Debug, Clone, Copy)]
pub(crate) enum State {
    /// The builder is idle and waiting for a new block production request.
    Idle,
    /// The builder is busy finalizing a new block.
    Finalizing(u64),
}

/// The struct responsible for managing the state of the block builder.
/// In particular, this is used to avoid race conditions when the driver
/// may write to the outstanding batch while the block builder is finalizing a block.
///
/// Here is a simple example to illustrate the problem:
///
/// |--------- 1 --------- 2 --------- 3 ---------> time
///
/// - at 1) we start finalizing a new L2 block with anchor block 120
/// - at 2) a condition in the driver is met such that the outstanding batch is proposed
/// - at this point, the anchor block gets updated to a more recent value (say 130)
/// - at 3) the block builder ends up finalizing the block with anchor block 120
/// - we now have an inconsistent state where the outstanding batch has anchor block 130, but a
///   block with anchor block 120 has been added to it.
#[derive(Debug)]
pub(crate) struct BuilderState {
    state_tx: watch::Sender<State>,
    state_rx: watch::Receiver<State>,
}

impl BuilderState {
    /// Create a new [`BuilderState`] instance.
    pub(crate) fn new() -> Self {
        let (state_tx, state_rx) = watch::channel(State::Idle);
        Self { state_tx, state_rx }
    }

    /// Get a clone of the sender for the builder state.
    pub(crate) fn sender(&self) -> watch::Sender<State> {
        self.state_tx.clone()
    }

    /// Set the builder state to [`State::Finalizing`].
    pub(crate) fn set_finalizing(&self, block_number: u64) {
        self.state_tx.send(State::Finalizing(block_number)).expect("to send finalizing state");
    }

    /// Check if the builder is currently finalizing a block.
    pub(crate) fn is_finalizing(&self) -> Option<u64> {
        match *self.state_rx.borrow() {
            State::Finalizing(block_number) => Some(block_number),
            State::Idle => None,
        }
    }

    /// Wait for the builder to be idle.
    pub(crate) async fn wait_for_idle(&mut self) {
        if self.is_finalizing().is_some() {
            let _ = self.state_rx.changed().await;
        }
    }
}

/// The response type for the [`L2BlockBuilder`].
#[derive(Debug)]
pub(crate) struct BlockBuilderResponse {
    /// The guard for the builder state. As soon as this is dropped, the builder state will be set
    /// to idle, thereby releasing the lock on the builder.
    guard: watch::Sender<State>,
    preconfirmed_block: PreconfirmedBlock,
    anchor: Header,
}

impl Drop for BlockBuilderResponse {
    /// Set the builder state to [`State::Idle`] when the response is dropped.
    ///
    /// NOTE: sending over the channel will only fail if the receiver has been dropped,
    /// which is when [`L2BlockBuilder`] is dropped.
    fn drop(&mut self) {
        let _ = self.guard.send(State::Idle);
    }
}

impl BlockBuilderResponse {
    /// Create a new [`BlockBuilderResponse`] instance.
    pub(crate) const fn new(
        guard: watch::Sender<State>,
        preconfirmed_block: PreconfirmedBlock,
        anchor: Header,
    ) -> Self {
        Self { guard, preconfirmed_block, anchor }
    }

    /// Consume the response and return the preconfirmed block along with its anchor header,
    /// releasing the lock on the builder.
    pub(crate) fn into_block_with_anchor(mut self) -> (PreconfirmedBlock, Header) {
        (std::mem::take(&mut self.preconfirmed_block), std::mem::take(&mut self.anchor))
    }
}
