use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use alloy::{
    eips::merge::EPOCH_SLOTS,
    primitives::{Address, BlockNumber, U256},
    providers::Provider,
    rpc::types::Header,
    transports::TransportResult,
};
use mk1_chainio::taiko::{
    inbox::TaikoInbox, preconf_whitelist::TaikoPreconfWhitelist, token::TaikoToken,
    wrapper::TaikoWrapper,
};
use mk1_clients::{
    execution::{ExecutionClient, TaikoExecutionClient},
    taiko_preconf::TaikoPreconfClient,
};
use mk1_primitives::{Slot, wei_to_eth};
use tracing::{debug, error};

use crate::{config::RuntimeConfig, driver::DriverError, metrics::DriverMetrics};

#[derive(Debug)]
pub(crate) struct L1State {
    /// Runtime configuration.
    cfg: RuntimeConfig,
    /// L1 execution client connection.
    pub el: ExecutionClient,
    /// The current highest L1 block header.
    pub tip: Header,
    /// The current L1 anchor block header.
    pub anchor: Header,
    /// The current L1 epoch.
    pub epoch: u64,
    /// The L1 head slot, unconditionally set on [`SlotClock`] ticks.
    pub head_slot: Slot,
    /// Taiko ERC20 contract instance
    pub taiko_token: TaikoToken,
    /// Taiko Inbox contract instance
    pub taiko_inbox: TaikoInbox,
    /// Taiko Wrapper contract instance
    pub taiko_wrapper: TaikoWrapper,
    /// `PreconfWhiteList` contract instance
    pub preconf_whitelist: TaikoPreconfWhitelist,
    /// Whether the `PreconfRouter` contract is set in Taiko's `AddressResolver`
    pub is_preconf_router_set: Arc<AtomicBool>,
}

impl L1State {
    /// Create a new L1 state container from the given runtime configuration.
    pub(crate) async fn new(cfg: RuntimeConfig) -> TransportResult<Self> {
        let el = ExecutionClient::new(cfg.l1.el_url.clone(), cfg.l1.el_ws_url.clone()).await?;

        let tip = el.get_header(None).await?;
        let anchor_number = tip.number.saturating_sub(cfg.preconf.anchor_block_lag);
        let anchor = el.get_header(Some(anchor_number)).await?;
        let head_slot = cfg.timestamp_to_l1_slot(tip.timestamp);
        let epoch = tip.number / EPOCH_SLOTS;

        let taiko_token = TaikoToken::new(
            cfg.l1.el_url.clone(),
            cfg.contracts.taiko_token,
            cfg.operator.private_key.clone(),
        );
        let taiko_inbox = TaikoInbox::new(
            cfg.l1.el_url.clone(),
            cfg.contracts.taiko_inbox,
            cfg.operator.private_key.clone(),
        );
        let preconf_whitelist = TaikoPreconfWhitelist::from_address(
            cfg.l1.el_url.clone(),
            cfg.contracts.preconf_whitelist,
        );
        let taiko_wrapper = TaikoWrapper::new(cfg.l1.el_url.clone(), cfg.contracts.taiko_wrapper);

        let is_preconf_router_set = Arc::new(AtomicBool::new(
            // If we fail to fetch: consider it as SET.
            taiko_wrapper.get_preconf_router().await.map_or(true, |a| a != Address::ZERO),
        ));

        Ok(Self {
            cfg,
            el,
            tip,
            anchor,
            epoch,
            head_slot,
            taiko_token,
            taiko_inbox,
            taiko_wrapper,
            preconf_whitelist,
            is_preconf_router_set,
        })
    }

    /// Returns true if the preconf router is set in the `TaikoWrapper` contract.
    pub(crate) fn is_preconf_router_set(&self) -> bool {
        self.is_preconf_router_set.load(Ordering::Relaxed)
    }

    /// Sync the L1 state with the given L1 header.
    pub(crate) async fn sync(&mut self, l1_header: Header) -> Result<(), DriverError> {
        let num = l1_header.number;

        self.tip = l1_header;

        // Only update the cached anchor if it's smaller than the new safe anchor block.
        let new_safe_anchor = num.saturating_sub(self.cfg.preconf.anchor_block_lag);
        if self.anchor.number < new_safe_anchor {
            self.update_anchor(new_safe_anchor).await?;
        }

        // Fetch the sequencer balances (ETH and TKO) in the background
        self.update_token_balances();

        // Check if the preconf router is still set in the TaikoWrapper contract in the background
        self.check_is_preconf_router_set();

        Ok(())
    }

    /// Returns true if the given slot is part of a new L1 epoch,
    /// where "new" means the epoch is different from the latest known epoch.
    pub(crate) const fn is_new_epoch(&self, slot: Slot) -> bool {
        let epoch = slot / EPOCH_SLOTS;

        epoch > self.epoch
    }

    /// Update the cached L1 anchor block.
    pub(crate) async fn update_anchor(&mut self, new: BlockNumber) -> TransportResult<()> {
        let old = self.anchor.number;
        let new_anchor = self.el.get_header(Some(new)).await?;

        self.anchor = new_anchor;
        DriverMetrics::set_l1_anchor_block(self.anchor.number);
        debug!(old, new = self.anchor.number, "Updated L1 anchor block");

        Ok(())
    }

    /// Assert that the local operator has sufficient TKO and ETH balances.
    pub(crate) async fn assert_token_balances(&self) -> Result<(), DriverError> {
        let (eth_balance, taiko_balance) = self.get_token_balances().await?;
        assert_token_balance("ETH", eth_balance, self.cfg.operator.min_eth_balance)?;
        assert_token_balance("TAIKO", taiko_balance, self.cfg.operator.min_taiko_balance)?;

        Ok(())
    }

    /// Returns the ETH and TKO balances of the local operator.
    pub(crate) async fn get_token_balances(&self) -> Result<(U256, U256), DriverError> {
        let operator = self.cfg.operator.private_key.address();
        let eth = self.el.get_balance(operator).await.map_err(|e| {
            DriverError::Custom(format!("Failed to get ETH balance of operator: {}", e))
        })?;
        let tko = self.taiko_token.get_balance_of(operator).await.map_err(|e| {
            DriverError::Custom(format!("Failed to get TKO balance of operator: {}", e))
        })?;

        Ok((eth, tko))
    }

    /// Update the token balances of the local operator in the background.
    pub(crate) fn update_token_balances(&self) {
        let el = self.el.clone();
        let token = self.taiko_token.clone();
        let operator = self.cfg.operator.private_key.address();

        tokio::spawn(async move {
            let (eth, tko) = tokio::join!(el.get_balance(operator), token.get_balance_of(operator));

            match (&eth, &tko) {
                (Ok(eth_wei), Ok(tko_wei)) => {
                    let eth = wei_to_eth(*eth_wei);
                    let tko = wei_to_eth(*tko_wei);

                    DriverMetrics::set_batch_submitter_balance(eth);
                    DriverMetrics::set_batch_submitter_tko_balance(tko);

                    debug!(%eth, %tko, "Updated operator balances");
                }
                _ => error!(?eth, ?tko, "Error while fetching operator balances"),
            }
        });
    }

    /// Approve the Taiko contract to spend the local operator's TKO tokens.
    pub(crate) async fn approve_taiko_spending(&self) -> Result<(), DriverError> {
        // Use a really high value as minimum allowance to avoid approval spam.
        let min_allowance = U256::MAX / U256::from(2);

        self.taiko_token
            .check_and_approve(
                *self.taiko_inbox.address(),
                min_allowance,
                self.cfg.operator.private_key.address(),
            )
            .await
            .map_err(|e| {
                DriverError::Custom(format!(
                    "Failed to check and approve $TKO tokens for taiko inbox: {}",
                    e
                ))
            })
    }

    /// Check if the preconf router is still set in the `TaikoWrapper` contract in the background.
    fn check_is_preconf_router_set(&self) {
        let taiko_wrapper = self.taiko_wrapper.clone();
        let is_preconf_router_set = Arc::clone(&self.is_preconf_router_set);
        tokio::spawn(async move {
            // If we fail to fetch: consider it as SET.
            let set = taiko_wrapper.get_preconf_router().await.map_or(true, |a| a != Address::ZERO);
            is_preconf_router_set.store(set, Ordering::Relaxed);
        });
    }
}

#[derive(Debug)]
pub(crate) struct L2State {
    /// L2 execution client connection.
    pub el: TaikoExecutionClient,
    /// Taiko preconf client connection.
    pub preconf_client: TaikoPreconfClient,
    /// The current L2 head block number.
    pub head_block: BlockNumber,
    /// The last L1 Origin, defined as the last L2 block that
    /// has been posted into an L1 batch.
    pub head_l1_origin: BlockNumber,
}

impl L2State {
    /// Create a new L2 state container from the given runtime configuration.
    pub(crate) async fn new(cfg: &RuntimeConfig) -> Result<Self, DriverError> {
        let el = ExecutionClient::new(cfg.l2.el_url.clone(), cfg.l2.el_ws_url.clone()).await?;

        let preconf_client = TaikoPreconfClient::new(
            cfg.l2.preconf_url.clone(),
            cfg.l2.preconf_ws_url.clone(),
            cfg.l2.jwt_secret,
        );

        let head_block = el.get_block_number().await?;

        // The initial head L1 origin can be fetched safely from the L1 TaikoInbox.
        let head_l1_origin = TaikoInbox::new(
            cfg.l1.el_url.clone(),
            cfg.contracts.taiko_inbox,
            cfg.operator.private_key.clone(),
        )
        .get_head_l1_origin()
        .await?;

        Ok(Self { el, head_block, head_l1_origin, preconf_client })
    }
}

/// Check if the given token balance is sufficient to cover the required amount.
pub(crate) fn assert_token_balance(
    name: &str,
    current: U256,
    required: U256,
) -> Result<(), DriverError> {
    if current < required {
        return Err(DriverError::InsufficientTokenBalance(format!(
            "Insufficient {name} balance: have {}, need at least {}",
            wei_to_eth(current),
            wei_to_eth(required)
        )));
    }

    Ok(())
}
