use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

// Pyth crates (Solana Playground compatible for pyth-sdk 0.8.x)
use pyth_sdk::{Price, PriceFeed};
use pyth_sdk_solana::load_price_feed_from_account_info;

declare_id!("G4g4DNdnxqTa8iAsiznzPKAwRk8JF56YsLZdNg2B7eBU");

/// ------------------------------
/// Constants + Fixed-Point Helpers
/// ------------------------------

/// Q64.64 fixed-point scale.
const Q64: i128 = 1i128 << 64;

/// Basis points divisor (1e4).
const BPS_DENOM_I128: i128 = 10_000;
const BPS_DENOM_U64: u64 = 10_000;

/// Base units scale (micro-base).
const BASE_Q: i128 = 1_000_000; // 1e6

/// Tariff index clamp [0, 50_000] bps (0% to 500%).
const TARIFF_BPS_MIN: i128 = 0;
const TARIFF_BPS_MAX: i128 = 50_000;

/// Fixed-size limits for oracle arrays.
const MAX_COUNTRY_ADDONS: usize = 16;
const MAX_BASKET_WEIGHTS: usize = 16;

/// Liquidation fraction (partial liquidation): 20%
const LIQUIDATION_FRACTION_BPS: i128 = 2_000;

/// Funding: cap per funding period at +/- 50 bps (0.50%) (in Q64.64 units).
const MAX_FUNDING_RATE_BPS_PER_PERIOD: i128 = 50;

/// Pyth staleness bound.
const PYTH_MAX_STALENESS_SECS_U64: u64 = 60;
const PYTH_MAX_STALENESS_SECS_I64: i64 = 60;

/// Pyth relative confidence cap in BPS (e.g., 100 = 1%).
const PYTH_MAX_CONF_BPS: u64 = 100;

/// ------------------------------
/// Program
/// ------------------------------
#[program]
pub mod tariff_perp {
    use super::*;

    // --------------------------
    // ORACLE INSTRUCTIONS
    // --------------------------

    /// Initialize the Tariff Oracle PDA.
    /// PDA seeds: [b"oracle", admin]
    pub fn initialize_oracle(
        ctx: Context<InitializeOracle>,
        baseline_tariff_bps: u16,
        confidence_bps: u16,
        valid_secs: i64,
    ) -> Result<()> {
        require!(confidence_bps <= 10_000, TariffError::InvalidBps);
        let now = Clock::get()?.unix_timestamp;

        let oracle = &mut ctx.accounts.oracle;
        oracle.admin = ctx.accounts.admin.key();
        oracle.baseline_tariff_bps = baseline_tariff_bps;
        oracle.last_baseline_bps = baseline_tariff_bps;
        oracle.confidence_bps = confidence_bps;

        oracle.min_update_interval_secs = 0; // configurable later
        oracle.max_jump_bps_per_update = 50_000; // effectively "no limit" until configured

        oracle.last_updated_ts = now;
        oracle.valid_until_ts = now
            .checked_add(valid_secs)
            .ok_or(TariffError::MathOverflow)?;

        oracle.addon_len = 0;
        oracle.weight_len = 0;
        oracle._pad0 = [0u8; 6];

        oracle.country_addons = [CountryAddon::default(); MAX_COUNTRY_ADDONS];
        oracle.basket_weights = [BasketWeight::default(); MAX_BASKET_WEIGHTS];

        Ok(())
    }

    /// Admin config: set anti-spam + anti-jump guardrails.
    pub fn oracle_set_guardrails(
        ctx: Context<OracleSetGuardrails>,
        min_update_interval_secs: i64,
        max_jump_bps_per_update: u16,
    ) -> Result<()> {
        require!(min_update_interval_secs >= 0, TariffError::InvalidConfig);
        require!(max_jump_bps_per_update <= 50_000, TariffError::InvalidBps);

        let oracle = &mut ctx.accounts.oracle;
        require_keys_eq!(oracle.admin, ctx.accounts.admin.key(), TariffError::Unauthorized);

        oracle.min_update_interval_secs = min_update_interval_secs;
        oracle.max_jump_bps_per_update = max_jump_bps_per_update;

        Ok(())
    }

    /// Admin baseline update with jump + cadence checks.
    /// Also refreshes valid_until using valid_secs.
    pub fn oracle_set_baseline(
        ctx: Context<OracleSetBaseline>,
        new_baseline_bps: u16,
        valid_secs: i64,
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let oracle = &mut ctx.accounts.oracle;

        require_keys_eq!(oracle.admin, ctx.accounts.admin.key(), TariffError::Unauthorized);
        oracle.require_can_update(now)?;

        let old = oracle.baseline_tariff_bps;
        let diff = if new_baseline_bps >= old {
            new_baseline_bps - old
        } else {
            old - new_baseline_bps
        };

        require!(
            diff <= oracle.max_jump_bps_per_update,
            TariffError::BaselineJumpTooLarge
        );

        oracle.last_baseline_bps = old;
        oracle.baseline_tariff_bps = new_baseline_bps;

        oracle.valid_until_ts = now
            .checked_add(valid_secs)
            .ok_or(TariffError::MathOverflow)?;
        oracle.last_updated_ts = now;

        emit!(OracleUpdateEvent {
            oracle: oracle.key(),
            admin: oracle.admin,
            baseline_bps: oracle.baseline_tariff_bps,
            valid_until_ts: oracle.valid_until_ts,
            ts: now,
        });

        Ok(())
    }

    /// Upsert a country addon entry (signed bps, can be negative).
    /// Only admin can update.
    pub fn oracle_upsert_country_addon(
        ctx: Context<OracleUpsertAddon>,
        country_code: [u8; 2],
        addon_bps: i16,
        enabled: bool,
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let oracle = &mut ctx.accounts.oracle;

        require_keys_eq!(oracle.admin, ctx.accounts.admin.key(), TariffError::Unauthorized);
        oracle.require_can_update(now)?;

        let mut found: Option<usize> = None;
        let len = oracle.addon_len as usize;
        for i in 0..len {
            if oracle.country_addons[i].country_code == country_code {
                found = Some(i);
                break;
            }
        }

        match found {
            Some(i) => {
                oracle.country_addons[i].addon_bps = addon_bps;
                oracle.country_addons[i].enabled = enabled;
            }
            None => {
                require!(len < MAX_COUNTRY_ADDONS, TariffError::ArrayFull);
                oracle.country_addons[len] = CountryAddon {
                    country_code,
                    addon_bps,
                    enabled,
                    _pad: [0u8; 3],
                };
                oracle.addon_len = (len as u8)
                    .checked_add(1)
                    .ok_or(TariffError::MathOverflow)?;
            }
        }

        oracle.last_updated_ts = now;
        Ok(())
    }

    /// Upsert a basket weight entry (weight bps).
    /// Only admin can update.
    pub fn oracle_upsert_basket_weight(
        ctx: Context<OracleUpsertWeight>,
        country_code: [u8; 2],
        weight_bps: u16,
        enabled: bool,
    ) -> Result<()> {
        require!(weight_bps <= 10_000, TariffError::InvalidBps);
        let now = Clock::get()?.unix_timestamp;

        let oracle = &mut ctx.accounts.oracle;
        require_keys_eq!(oracle.admin, ctx.accounts.admin.key(), TariffError::Unauthorized);
        oracle.require_can_update(now)?;

        let mut found: Option<usize> = None;
        let len = oracle.weight_len as usize;
        for i in 0..len {
            if oracle.basket_weights[i].country_code == country_code {
                found = Some(i);
                break;
            }
        }

        match found {
            Some(i) => {
                oracle.basket_weights[i].weight_bps = weight_bps;
                oracle.basket_weights[i].enabled = enabled;
            }
            None => {
                require!(len < MAX_BASKET_WEIGHTS, TariffError::ArrayFull);
                oracle.basket_weights[len] = BasketWeight {
                    country_code,
                    weight_bps,
                    enabled,
                    _pad: [0u8; 3],
                };
                oracle.weight_len = (len as u8)
                    .checked_add(1)
                    .ok_or(TariffError::MathOverflow)?;
            }
        }

        oracle.last_updated_ts = now;
        Ok(())
    }

    // --------------------------
    // MARKET INSTRUCTIONS
    // --------------------------

    /// Initialize the Perp Market PDA and its vault authority PDAs + ATAs.
    /// PDA seeds:
    /// - market: [b"market", oracle, usdc_mint]
    /// - vault_auth: [b"vault_auth", market]
    /// - insurance_auth: [b"insurance", market]
    pub fn initialize_market(
        ctx: Context<InitializeMarket>,
        base_reserve: i128,
        quote_reserve: i128,
        initial_margin_bps: u16,
        maintenance_margin_bps: u16,
        trade_fee_bps: u16,
        liquidation_fee_bps: u16,
        max_open_interest_base: i128,
        max_skew_base: i128,
        pyth_sol_usd_feed: Pubkey,
    ) -> Result<()> {
        require!(initial_margin_bps <= 10_000, TariffError::InvalidBps);
        require!(maintenance_margin_bps <= 10_000, TariffError::InvalidBps);
        require!(
            maintenance_margin_bps <= initial_margin_bps,
            TariffError::BadMarginParams
        );
        require!(trade_fee_bps <= 10_000, TariffError::InvalidBps);
        require!(liquidation_fee_bps <= 10_000, TariffError::InvalidBps);

        require!(base_reserve > 0, TariffError::InvalidReserve);
        require!(quote_reserve > 0, TariffError::InvalidReserve);

        let k = base_reserve
            .checked_mul(quote_reserve)
            .ok_or(TariffError::MathOverflow)?;

        let now = Clock::get()?.unix_timestamp;

        let market = &mut ctx.accounts.market;
        market.admin = ctx.accounts.admin.key();
        market.oracle = ctx.accounts.oracle.key();
        market.usdc_mint = ctx.accounts.usdc_mint.key();

        market.vault_authority_bump = ctx.bumps.vault_authority;
        market.insurance_authority_bump = ctx.bumps.insurance_authority;
        market._pad0 = [0u8; 6];

        market.vault_usdc = ctx.accounts.vault_usdc.key();
        market.insurance_vault_usdc = ctx.accounts.insurance_vault_usdc.key();

        market.pyth_sol_usd_feed = pyth_sol_usd_feed;
        market.last_pyth_publish_time = 0;

        // vAMM
        market.base_reserve = base_reserve;
        market.quote_reserve = quote_reserve;
        market.invariant_k = k;

        // funding
        market.funding_index = 0;
        market.last_funding_ts = now;
        market.funding_period_secs = 3600; // default hourly

        // margin + fees
        market.initial_margin_bps = initial_margin_bps;
        market.maintenance_margin_bps = maintenance_margin_bps;
        market.trade_fee_bps = trade_fee_bps;
        market.liquidation_fee_bps = liquidation_fee_bps;

        // insurance config defaults
        market.max_insurance_payout_per_liq_usdc = 1_000_000_000u64; // 1,000 USDC cap by default
        market.fee_to_insurance_bps = 10_000; // 100% of fees to insurance by default

        // vAMM guardrails defaults
        market.min_trade_base = 10i128
            .checked_mul(BASE_Q)
            .ok_or(TariffError::MathOverflow)?;
        market.max_price_impact_bps = 500; // 5%
        market.spread_bps = 10; // 0.10%

        // switches
        market.reduce_only = false;
        market.paused = false;
        market._pad1 = [0u8; 2];

        // market-level position tracking
        market.open_interest_base = 0;
        market.net_position_base = 0;

        // risk limits
        market.max_open_interest_base = max_open_interest_base;
        market.max_skew_base = max_skew_base;

        Ok(())
    }

    /// Admin: set reduce-only mode.
    pub fn set_reduce_only(ctx: Context<SetMarketFlags>, reduce_only: bool) -> Result<()> {
        let market = &mut ctx.accounts.market;
        require_keys_eq!(market.admin, ctx.accounts.admin.key(), TariffError::Unauthorized);
        market.reduce_only = reduce_only;
        Ok(())
    }

    /// Admin: set paused mode.
    pub fn set_paused(ctx: Context<SetMarketFlags>, paused: bool) -> Result<()> {
        let market = &mut ctx.accounts.market;
        require_keys_eq!(market.admin, ctx.accounts.admin.key(), TariffError::Unauthorized);
        market.paused = paused;
        Ok(())
    }

    /// Admin: set market config (insurance circuit breaker + fee routing + funding cadence + vAMM guards).
    pub fn set_market_config(
        ctx: Context<SetMarketFlags>,
        max_insurance_payout_per_liq_usdc: u64,
        fee_to_insurance_bps: u16,
        funding_period_secs: i64,
        min_trade_base: i128,
        max_price_impact_bps: u16,
        spread_bps: u16,
    ) -> Result<()> {
        let market = &mut ctx.accounts.market;
        require_keys_eq!(market.admin, ctx.accounts.admin.key(), TariffError::Unauthorized);

        require!(fee_to_insurance_bps <= 10_000, TariffError::InvalidBps);
        require!(funding_period_secs > 0, TariffError::InvalidConfig);
        require!(min_trade_base > 0, TariffError::InvalidConfig);
        require!(max_price_impact_bps <= 10_000, TariffError::InvalidBps);
        require!(spread_bps <= 10_000, TariffError::InvalidBps);

        market.max_insurance_payout_per_liq_usdc = max_insurance_payout_per_liq_usdc;
        market.fee_to_insurance_bps = fee_to_insurance_bps;
        market.funding_period_secs = funding_period_secs;
        market.min_trade_base = min_trade_base;
        market.max_price_impact_bps = max_price_impact_bps;
        market.spread_bps = spread_bps;

        Ok(())
    }

    // --------------------------
    // USER INSTRUCTIONS
    // --------------------------

    /// Initialize a user margin PDA.
    /// PDA seeds: [b"margin", market, owner]
    pub fn initialize_margin(ctx: Context<InitializeMargin>) -> Result<()> {
        let margin = &mut ctx.accounts.margin;
        margin.owner = ctx.accounts.owner.key();
        margin.market = ctx.accounts.market.key();
        margin.collateral_usdc = 0;
        margin.position_base = 0;
        margin.entry_price_q64 = 0;
        margin.last_funding_index = ctx.accounts.market.funding_index;
        margin.realized_pnl_usdc = 0;
        margin.open_notional_q64 = 0;
        Ok(())
    }

    /// Deposit USDC collateral into the market vault.
    /// Allowed even when paused (QoL safety).
    pub fn deposit_usdc(ctx: Context<DepositUsdc>, amount: u64) -> Result<()> {
        require!(amount > 0, TariffError::InvalidAmount);
        let now = Clock::get()?.unix_timestamp;

        // user mint check
        require_keys_eq!(
            ctx.accounts.user_usdc.mint,
            ctx.accounts.market.usdc_mint,
            TariffError::InvalidMint
        );

        // cheap vault checks
        require_keys_eq!(
            ctx.accounts.vault_usdc.mint,
            ctx.accounts.market.usdc_mint,
            TariffError::InvalidMint
        );
        require_keys_eq!(
            ctx.accounts.vault_usdc.owner,
            ctx.accounts.vault_authority.key(),
            TariffError::InvalidTokenOwner
        );

        // transfer user -> vault
        let cpi_accounts = Transfer {
            from: ctx.accounts.user_usdc.to_account_info(),
            to: ctx.accounts.vault_usdc.to_account_info(),
            authority: ctx.accounts.owner.to_account_info(),
        };
        token::transfer(
            CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts),
            amount,
        )?;

        // update margin
        let margin = &mut ctx.accounts.margin;
        margin.collateral_usdc = margin
            .collateral_usdc
            .checked_add(amount)
            .ok_or(TariffError::MathOverflow)?;

        emit!(DepositEvent {
            market: ctx.accounts.market.key(),
            user: ctx.accounts.owner.key(),
            amount_usdc: amount,
            ts: now,
        });

        Ok(())
    }

    /// Withdraw USDC collateral from the market vault.
    /// Blocked when paused.
    pub fn withdraw_usdc(ctx: Context<WithdrawUsdc>, amount: u64) -> Result<()> {
        require!(amount > 0, TariffError::InvalidAmount);
        let now = Clock::get()?.unix_timestamp;

        let market = &mut ctx.accounts.market;
        require!(!market.paused, TariffError::Paused);

        // oracle validity
        ctx.accounts.oracle.require_valid(now)?;

        // Pyth sanity + monotonic update
        require_keys_eq!(
            ctx.accounts.pyth_feed.key(),
            market.pyth_sol_usd_feed,
            TariffError::InvalidPythFeed
        );
        pyth_sanity_check_update_market(market, &ctx.accounts.pyth_feed, now)?;

        // mint checks
        require_keys_eq!(ctx.accounts.user_usdc.mint, market.usdc_mint, TariffError::InvalidMint);
        require_keys_eq!(ctx.accounts.vault_usdc.mint, market.usdc_mint, TariffError::InvalidMint);
        require_keys_eq!(
            ctx.accounts.vault_usdc.owner,
            ctx.accounts.vault_authority.key(),
            TariffError::InvalidTokenOwner
        );

        // settle funding first
        settle_funding(market, &mut ctx.accounts.margin)?;

        // enough tracked collateral
        require!(
            ctx.accounts.margin.collateral_usdc >= amount,
            TariffError::InsufficientCollateral
        );

        // simulate withdrawal & margin check
        let new_collateral = ctx
            .accounts
            .margin
            .collateral_usdc
            .checked_sub(amount)
            .ok_or(TariffError::MathOverflow)?;

        let (equity_before_i128, notional_i128) =
            compute_equity_and_notional_usdc(market, &ctx.accounts.margin)?;
        let equity_sim = equity_before_i128
            .checked_sub(amount as i128)
            .ok_or(TariffError::MathOverflow)?;

        let req = notional_i128
            .checked_mul(market.initial_margin_bps as i128)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(BPS_DENOM_I128)
            .ok_or(TariffError::MathOverflow)?;

        require!(equity_sim >= req, TariffError::MarginTooLow);

        // apply state update
        ctx.accounts.margin.collateral_usdc = new_collateral;

        // transfer vault -> user (PDA signer)
        let market_key = market.key();
        let vault_seeds: &[&[u8]] = &[
            b"vault_auth",
            market_key.as_ref(),
            &[market.vault_authority_bump],
        ];
        let signer_seeds: &[&[&[u8]]] = &[vault_seeds];

        let cpi_accounts = Transfer {
            from: ctx.accounts.vault_usdc.to_account_info(),
            to: ctx.accounts.user_usdc.to_account_info(),
            authority: ctx.accounts.vault_authority.to_account_info(),
        };
        token::transfer(
            CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer_seeds),
            amount,
        )?;

        emit!(WithdrawEvent {
            market: market_key,
            user: ctx.accounts.owner.key(),
            amount_usdc: amount,
            ts: now,
        });

        Ok(())
    }

    /// Open/increase/decrease a position via vAMM swap.
    /// Blocked when paused. Enforces min trade, price impact cap, spread, reduce-only, and market-level risk.
    pub fn open_position(ctx: Context<OpenPosition>, side: u8, base_amount: i128) -> Result<()> {
        require!(side == 0 || side == 1, TariffError::InvalidSide);
        require!(base_amount > 0, TariffError::InvalidAmount);

        let now = Clock::get()?.unix_timestamp;

        let market = &mut ctx.accounts.market;
        require!(!market.paused, TariffError::Paused);

        // oracle validity
        ctx.accounts.oracle.require_valid(now)?;

        // pyth sanity + monotonic
        require_keys_eq!(ctx.accounts.pyth_feed.key(), market.pyth_sol_usd_feed, TariffError::InvalidPythFeed);
        pyth_sanity_check_update_market(market, &ctx.accounts.pyth_feed, now)?;

        // mint & ownership checks (cheap safety)
        require_keys_eq!(ctx.accounts.vault_usdc.mint, market.usdc_mint, TariffError::InvalidMint);
        require_keys_eq!(ctx.accounts.insurance_vault_usdc.mint, market.usdc_mint, TariffError::InvalidMint);
        require_keys_eq!(ctx.accounts.vault_usdc.owner, ctx.accounts.vault_authority.key(), TariffError::InvalidTokenOwner);
        require_keys_eq!(
            ctx.accounts.insurance_vault_usdc.owner,
            ctx.accounts.insurance_authority.key(),
            TariffError::InvalidTokenOwner
        );

        // settle funding
        settle_funding(market, &mut ctx.accounts.margin)?;

        // min trade
        require!(base_amount >= market.min_trade_base, TariffError::TradeTooSmall);

        // signed delta
        let signed_delta = if side == 0 {
            base_amount
        } else {
            base_amount.checked_neg().ok_or(TariffError::MathOverflow)?
        };

        let old_pos = ctx.accounts.margin.position_base;
        let new_pos = old_pos.checked_add(signed_delta).ok_or(TariffError::MathOverflow)?;

        // reduce-only enforcement (strict: no flips, no opens)
        if market.reduce_only {
            require!(old_pos != 0, TariffError::ReduceOnly);
            require!(new_pos == 0 || same_sign_nonzero(old_pos, new_pos), TariffError::ReduceOnly);
            require!(i128_abs(new_pos)? < i128_abs(old_pos)?, TariffError::ReduceOnly);
        }

        // market-level risk check (pre-check)
        check_market_risk_after_change(market, old_pos, new_pos)?;

        // vAMM manipulation guardrails: impact cap + spread
        let mark_before_q64 = mark_price_q64_from_reserves(market.base_reserve, market.quote_reserve)?;

        // simulate swap without mutating market yet
        let (exec_price_q64_raw, _quote_delta_raw, new_base_reserve, new_quote_reserve) =
            vamm_swap(market, signed_delta)?;

        let mark_after_q64 = mark_price_q64_from_reserves(new_base_reserve, new_quote_reserve)?;

        // price impact bps = abs(mark_after - mark_before) / mark_before * 10_000
        let diff = mark_after_q64.checked_sub(mark_before_q64).ok_or(TariffError::MathOverflow)?;
        let diff_abs = i128_abs(diff)?;
        let impact_bps_i128 = diff_abs
            .checked_mul(BPS_DENOM_I128)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(mark_before_q64)
            .ok_or(TariffError::MathOverflow)?;
        require!(
            impact_bps_i128 <= market.max_price_impact_bps as i128,
            TariffError::PriceImpactTooHigh
        );

        // spread-adjusted execution price
        let exec_price_q64 = apply_spread_q64(exec_price_q64_raw, market.spread_bps, side)?;

        // compute notional (spread-adjusted): abs(delta_base) * exec_price / Q64 / BASE_Q
        let base_abs = i128_abs(signed_delta)?;
        let notional_usdc_i128 = base_times_price_to_usdc_micro(base_abs, exec_price_q64)?;
        let notional_abs = i128_abs(notional_usdc_i128)?;

        // trade fee on notional
        let fee_usdc_i128 = notional_abs
            .checked_mul(market.trade_fee_bps as i128)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(BPS_DENOM_I128)
            .ok_or(TariffError::MathOverflow)?;

        let fee_u64 = i128_to_u64(fee_usdc_i128)?;

        // fee must be payable (POC)
        require!(ctx.accounts.margin.collateral_usdc >= fee_u64, TariffError::InsufficientCollateral);

        // commit vAMM reserve changes
        market.base_reserve = new_base_reserve;
        market.quote_reserve = new_quote_reserve;

        // apply position change and entry/PnL at spread-adjusted exec price
        update_position_and_entry(&mut ctx.accounts.margin, exec_price_q64, signed_delta)?;

        // update market-level tracking (OI + net position)
        apply_position_change_to_market(market, old_pos, new_pos)?;

        // route fee_to_insurance portion to insurance vault
        let insurance_fee_u64 = fee_u64
            .checked_mul(market.fee_to_insurance_bps as u64)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(BPS_DENOM_U64)
            .ok_or(TariffError::MathOverflow)?;

        // deduct fee from tracked collateral (always full fee)
        ctx.accounts.margin.collateral_usdc = ctx
            .accounts
            .margin
            .collateral_usdc
            .checked_sub(fee_u64)
            .ok_or(TariffError::MathOverflow)?;

        // transfer insurance portion vault -> insurance vault (PDA signer: vault_auth)
        if insurance_fee_u64 > 0 {
            let market_key = market.key();
            let vault_seeds: &[&[u8]] = &[
                b"vault_auth",
                market_key.as_ref(),
                &[market.vault_authority_bump],
            ];
            let signer_seeds: &[&[&[u8]]] = &[vault_seeds];

            let cpi_accounts = Transfer {
                from: ctx.accounts.vault_usdc.to_account_info(),
                to: ctx.accounts.insurance_vault_usdc.to_account_info(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            };
            token::transfer(
                CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer_seeds),
                insurance_fee_u64,
            )?;
        }

        // post-trade margin check at mark after trade
        let (equity_i128, notional_i128_at_mark) = compute_equity_and_notional_usdc(market, &ctx.accounts.margin)?;
        let req = notional_i128_at_mark
            .checked_mul(market.initial_margin_bps as i128)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(BPS_DENOM_I128)
            .ok_or(TariffError::MathOverflow)?;
        require!(equity_i128 >= req, TariffError::MarginTooLow);

        emit!(TradeEvent {
            market: market.key(),
            user: ctx.accounts.owner.key(),
            side,
            base_delta: signed_delta,
            exec_price_q64,
            fee_usdc: fee_u64,
            mark_before_q64,
            mark_after_q64,
            ts: now,
        });

        Ok(())
    }

    /// Helper: close the user's position to zero using vAMM swap.
    /// Allowed even when paused and reduce_only is true.
    pub fn close_position(ctx: Context<ClosePosition>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;

        let market = &mut ctx.accounts.market;

        // oracle validity
        ctx.accounts.oracle.require_valid(now)?;

        // pyth sanity + monotonic
        require_keys_eq!(ctx.accounts.pyth_feed.key(), market.pyth_sol_usd_feed, TariffError::InvalidPythFeed);
        pyth_sanity_check_update_market(market, &ctx.accounts.pyth_feed, now)?;

        // cheap safety checks
        require_keys_eq!(ctx.accounts.vault_usdc.mint, market.usdc_mint, TariffError::InvalidMint);
        require_keys_eq!(ctx.accounts.insurance_vault_usdc.mint, market.usdc_mint, TariffError::InvalidMint);
        require_keys_eq!(ctx.accounts.vault_usdc.owner, ctx.accounts.vault_authority.key(), TariffError::InvalidTokenOwner);
        require_keys_eq!(
            ctx.accounts.insurance_vault_usdc.owner,
            ctx.accounts.insurance_authority.key(),
            TariffError::InvalidTokenOwner
        );

        // settle funding
        settle_funding(market, &mut ctx.accounts.margin)?;

        let old_pos = ctx.accounts.margin.position_base;
        require!(old_pos != 0, TariffError::NoPosition);

        // delta to close
        let signed_delta = old_pos.checked_neg().ok_or(TariffError::MathOverflow)?;
        let new_pos = 0i128;

        // market-level risk check (closing reduces risk, should pass; still checked)
        check_market_risk_after_change(market, old_pos, new_pos)?;

        let mark_before_q64 = mark_price_q64_from_reserves(market.base_reserve, market.quote_reserve)?;

        // simulate swap
        let (exec_price_q64_raw, _quote_delta_raw, new_base_reserve, new_quote_reserve) =
            vamm_swap(market, signed_delta)?;

        let mark_after_q64 = mark_price_q64_from_reserves(new_base_reserve, new_quote_reserve)?;

        // apply spread
        // side inferred: delta > 0 => long (buy base), delta < 0 => short (sell base)
        let inferred_side: u8 = if signed_delta > 0 { 0 } else { 1 };
        let exec_price_q64 = apply_spread_q64(exec_price_q64_raw, market.spread_bps, inferred_side)?;

        // notional and fee
        let base_abs = i128_abs(signed_delta)?;
        let notional_usdc_i128 = base_times_price_to_usdc_micro(base_abs, exec_price_q64)?;
        let notional_abs = i128_abs(notional_usdc_i128)?;

        let fee_usdc_i128 = notional_abs
            .checked_mul(market.trade_fee_bps as i128)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(BPS_DENOM_I128)
            .ok_or(TariffError::MathOverflow)?;
        let fee_u64 = i128_to_u64(fee_usdc_i128)?;

        require!(ctx.accounts.margin.collateral_usdc >= fee_u64, TariffError::InsufficientCollateral);

        // commit reserves
        market.base_reserve = new_base_reserve;
        market.quote_reserve = new_quote_reserve;

        // update position
        update_position_and_entry(&mut ctx.accounts.margin, exec_price_q64, signed_delta)?;
        apply_position_change_to_market(market, old_pos, new_pos)?;

        // fee routing
        let insurance_fee_u64 = fee_u64
            .checked_mul(market.fee_to_insurance_bps as u64)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(BPS_DENOM_U64)
            .ok_or(TariffError::MathOverflow)?;

        ctx.accounts.margin.collateral_usdc = ctx
            .accounts
            .margin
            .collateral_usdc
            .checked_sub(fee_u64)
            .ok_or(TariffError::MathOverflow)?;

        if insurance_fee_u64 > 0 {
            let market_key = market.key();
            let vault_seeds: &[&[u8]] = &[
                b"vault_auth",
                market_key.as_ref(),
                &[market.vault_authority_bump],
            ];
            let signer_seeds: &[&[&[u8]]] = &[vault_seeds];

            let cpi_accounts = Transfer {
                from: ctx.accounts.vault_usdc.to_account_info(),
                to: ctx.accounts.insurance_vault_usdc.to_account_info(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            };
            token::transfer(
                CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer_seeds),
                insurance_fee_u64,
            )?;
        }

        emit!(TradeEvent {
            market: market.key(),
            user: ctx.accounts.owner.key(),
            side: inferred_side,
            base_delta: signed_delta,
            exec_price_q64,
            fee_usdc: fee_u64,
            mark_before_q64,
            mark_after_q64,
            ts: now,
        });

        Ok(())
    }

    /// Apply funding to the market (callable by anyone).
    /// Blocked when paused. Applies only in discrete funding periods.
    pub fn apply_funding(ctx: Context<ApplyFunding>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;

        let market = &mut ctx.accounts.market;
        require!(!market.paused, TariffError::Paused);

        // oracle validity
        ctx.accounts.oracle.require_valid(now)?;

        // pyth sanity + monotonic
        require_keys_eq!(ctx.accounts.pyth_feed.key(), market.pyth_sol_usd_feed, TariffError::InvalidPythFeed);
        pyth_sanity_check_update_market(market, &ctx.accounts.pyth_feed, now)?;

        let dt = now
            .checked_sub(market.last_funding_ts)
            .ok_or(TariffError::MathOverflow)?;
        require!(dt >= 0, TariffError::MathOverflow);

        let period = market.funding_period_secs;
        require!(period > 0, TariffError::InvalidConfig);

        let periods = dt.checked_div(period).ok_or(TariffError::MathOverflow)?;
        if periods <= 0 {
            return Ok(());
        }

        let periods_i128 = periods as i128;

        let mark_q64 = market.mark_price_q64()?;
        let index_q64 = market.index_price_q64(&ctx.accounts.oracle)?;

        let diff_q64 = mark_q64.checked_sub(index_q64).ok_or(TariffError::MathOverflow)?;

        // raw rate: diff / divisor
        let raw_rate_q64 = diff_q64.checked_div(8).ok_or(TariffError::MathOverflow)?;

        // cap per period: +/- 50 bps
        let cap_q64 = bps_i128_to_q64(MAX_FUNDING_RATE_BPS_PER_PERIOD)?;
        let neg_cap = cap_q64.checked_neg().ok_or(TariffError::MathOverflow)?;
        let rate_q64 = clamp_i128(raw_rate_q64, neg_cap, cap_q64)?;

        // funding_index increments per period (not per second)
        let delta_index = rate_q64
            .checked_mul(periods_i128)
            .ok_or(TariffError::MathOverflow)?;

        market.funding_index = market
            .funding_index
            .checked_add(delta_index)
            .ok_or(TariffError::MathOverflow)?;

        // advance last_funding_ts by whole periods
        let advance = periods
            .checked_mul(period)
            .ok_or(TariffError::MathOverflow)?;
        market.last_funding_ts = market
            .last_funding_ts
            .checked_add(advance)
            .ok_or(TariffError::MathOverflow)?;

        emit!(FundingAppliedEvent {
            market: market.key(),
            dt: advance,
            rate_q64,
            funding_index: market.funding_index,
            mark_q64,
            index_q64,
            ts: now,
        });

        Ok(())
    }

    /// Liquidate a margin account if equity < maintenance requirement.
    /// Blocked when paused. Partial liquidation with LIQUIDATION_FRACTION_BPS.
    pub fn liquidate(ctx: Context<Liquidate>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;

        let market = &mut ctx.accounts.market;
        require!(!market.paused, TariffError::Paused);

        // oracle validity
        ctx.accounts.oracle.require_valid(now)?;

        // pyth sanity + monotonic
        require_keys_eq!(ctx.accounts.pyth_feed.key(), market.pyth_sol_usd_feed, TariffError::InvalidPythFeed);
        pyth_sanity_check_update_market(market, &ctx.accounts.pyth_feed, now)?;

        // cheap safety checks
        require_keys_eq!(ctx.accounts.vault_usdc.mint, market.usdc_mint, TariffError::InvalidMint);
        require_keys_eq!(ctx.accounts.insurance_vault_usdc.mint, market.usdc_mint, TariffError::InvalidMint);
        require_keys_eq!(ctx.accounts.vault_usdc.owner, ctx.accounts.vault_authority.key(), TariffError::InvalidTokenOwner);
        require_keys_eq!(
            ctx.accounts.insurance_vault_usdc.owner,
            ctx.accounts.insurance_authority.key(),
            TariffError::InvalidTokenOwner
        );
        require_keys_eq!(ctx.accounts.liquidator_usdc.mint, market.usdc_mint, TariffError::InvalidMint);

        // settle funding first
        settle_funding(market, &mut ctx.accounts.user_margin)?;

        // check eligibility
        let (equity_before, notional_before) =
            compute_equity_and_notional_usdc(market, &ctx.accounts.user_margin)?;

        let maint_req = notional_before
            .checked_mul(market.maintenance_margin_bps as i128)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(BPS_DENOM_I128)
            .ok_or(TariffError::MathOverflow)?;

        require!(equity_before < maint_req, TariffError::NotLiquidatable);

        let pos = ctx.accounts.user_margin.position_base;
        require!(pos != 0, TariffError::NoPosition);

        // partial close size
        let abs_pos = i128_abs(pos)?;
        let mut close_abs = abs_pos
            .checked_mul(LIQUIDATION_FRACTION_BPS)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(BPS_DENOM_I128)
            .ok_or(TariffError::MathOverflow)?;
        if close_abs < 1 {
            close_abs = 1;
        }
        if close_abs > abs_pos {
            close_abs = abs_pos;
        }

        // signed close delta reduces exposure
        let signed_close_delta = if pos > 0 {
            close_abs.checked_neg().ok_or(TariffError::MathOverflow)?
        } else {
            close_abs
        };

        // compute new user position
        let old_pos = pos;
        let new_pos = old_pos.checked_add(signed_close_delta).ok_or(TariffError::MathOverflow)?;

        // market-level risk check (reduces risk; should pass, but checked)
        check_market_risk_after_change(market, old_pos, new_pos)?;

        // vAMM swap (raw exec price; no extra liquidation spread applied)
        let (exec_price_q64, _quote_delta, new_base_reserve, new_quote_reserve) =
            vamm_swap(market, signed_close_delta)?;

        // commit reserves
        market.base_reserve = new_base_reserve;
        market.quote_reserve = new_quote_reserve;

        // realize pnl / update entry
        update_position_and_entry(&mut ctx.accounts.user_margin, exec_price_q64, signed_close_delta)?;

        // update market-level tracking
        apply_position_change_to_market(market, old_pos, new_pos)?;

        // closed notional (micro-USDC) based on exec_price
        let closed_notional_i128 = base_times_price_to_usdc_micro(close_abs, exec_price_q64)?;
        let closed_notional_abs = i128_abs(closed_notional_i128)?;
        let closed_notional_u64 = i128_to_u64(closed_notional_abs)?;

        // liquidation fee proportional to closed notional
        let liq_fee_i128 = closed_notional_abs
            .checked_mul(market.liquidation_fee_bps as i128)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(BPS_DENOM_I128)
            .ok_or(TariffError::MathOverflow)?;
        let liq_fee_u64 = i128_to_u64(liq_fee_i128)?;

        // fee paid from user's remaining collateral (clamped)
        let user_collat = ctx.accounts.user_margin.collateral_usdc;
        let fee_from_user = std::cmp::min(user_collat, liq_fee_u64);

        if fee_from_user > 0 {
            ctx.accounts.user_margin.collateral_usdc = ctx
                .accounts
                .user_margin
                .collateral_usdc
                .checked_sub(fee_from_user)
                .ok_or(TariffError::MathOverflow)?;
        }

        // split fee: half to liquidator, half to insurance (ensure insurance always gets portion)
        let half = fee_from_user / 2;
        let other_half = fee_from_user
            .checked_sub(half)
            .ok_or(TariffError::MathOverflow)?;

        // vault signer seeds
        let market_key = market.key();
        let vault_seeds: &[&[u8]] = &[
            b"vault_auth",
            market_key.as_ref(),
            &[market.vault_authority_bump],
        ];
        let vault_signer: &[&[&[u8]]] = &[vault_seeds];

        // vault -> liquidator
        if half > 0 {
            let cpi_accounts = Transfer {
                from: ctx.accounts.vault_usdc.to_account_info(),
                to: ctx.accounts.liquidator_usdc.to_account_info(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            };
            token::transfer(
                CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, vault_signer),
                half,
            )?;
        }

        // vault -> insurance vault (penalty portion)
        if other_half > 0 {
            let cpi_accounts = Transfer {
                from: ctx.accounts.vault_usdc.to_account_info(),
                to: ctx.accounts.insurance_vault_usdc.to_account_info(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            };
            token::transfer(
                CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, vault_signer),
                other_half,
            )?;
        }

        // recompute equity after partial liquidation + fee
        let equity_after_before_cover = compute_equity_and_notional_usdc(market, &ctx.accounts.user_margin)?.0;

        // bad debt coverage with cap + insurance balance
        if equity_after_before_cover < 0 {
            let debt_i128 = equity_after_before_cover.checked_neg().ok_or(TariffError::MathOverflow)?;
            let debt_u64 = i128_to_u64(debt_i128)?;

            let insurance_bal = ctx.accounts.insurance_vault_usdc.amount;
            let cap = market.max_insurance_payout_per_liq_usdc;
            let cover = std::cmp::min(std::cmp::min(insurance_bal, debt_u64), cap);

            if cover > 0 {
                let ins_seeds: &[&[u8]] = &[
                    b"insurance",
                    market_key.as_ref(),
                    &[market.insurance_authority_bump],
                ];
                let ins_signer: &[&[&[u8]]] = &[ins_seeds];

                let cpi_accounts = Transfer {
                    from: ctx.accounts.insurance_vault_usdc.to_account_info(),
                    to: ctx.accounts.vault_usdc.to_account_info(),
                    authority: ctx.accounts.insurance_authority.to_account_info(),
                };
                token::transfer(
                    CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, ins_signer),
                    cover,
                )?;
            }

            // tracked collateral can't go negative; clamp to 0
            ctx.accounts.user_margin.collateral_usdc = 0;

            if cover < debt_u64 {
                emit!(BadDebtEvent {
                    market: market_key,
                    user: ctx.accounts.user_margin.owner,
                    debt_usdc: debt_u64,
                    covered_usdc: cover,
                    ts: now,
                });
            }
        }

        let equity_after = compute_equity_and_notional_usdc(market, &ctx.accounts.user_margin)?.0;

        emit!(LiquidationEvent {
            market: market_key,
            user: ctx.accounts.user_margin.owner,
            liquidator: ctx.accounts.liquidator.key(),
            closed_base: close_abs,
            closed_notional_usdc: closed_notional_u64,
            fee_usdc: fee_from_user,
            equity_before,
            equity_after,
            ts: now,
        });

        Ok(())
    }
}

/// ------------------------------
/// Accounts + Instructions Context
/// ------------------------------

#[derive(Accounts)]
pub struct InitializeOracle<'info> {
    /// Admin who can update this oracle.
    #[account(mut)]
    pub admin: Signer<'info>,

    /// Oracle PDA: [b"oracle", admin]
    #[account(
        init,
        payer = admin,
        space = 8 + TariffOracle::SPACE,
        seeds = [b"oracle", admin.key().as_ref()],
        bump
    )]
    pub oracle: Account<'info, TariffOracle>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct OracleSetGuardrails<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [b"oracle", oracle.admin.as_ref()],
        bump
    )]
    pub oracle: Account<'info, TariffOracle>,
}

#[derive(Accounts)]
pub struct OracleSetBaseline<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [b"oracle", oracle.admin.as_ref()],
        bump
    )]
    pub oracle: Account<'info, TariffOracle>,
}

#[derive(Accounts)]
pub struct OracleUpsertAddon<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [b"oracle", oracle.admin.as_ref()],
        bump
    )]
    pub oracle: Account<'info, TariffOracle>,
}

#[derive(Accounts)]
pub struct OracleUpsertWeight<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [b"oracle", oracle.admin.as_ref()],
        bump
    )]
    pub oracle: Account<'info, TariffOracle>,
}

#[derive(Accounts)]
pub struct InitializeMarket<'info> {
    /// Market admin
    #[account(mut)]
    pub admin: Signer<'info>,

    /// Existing oracle
    pub oracle: Account<'info, TariffOracle>,

    pub usdc_mint: Account<'info, Mint>,

    /// Market PDA: [b"market", oracle, usdc_mint]
    #[account(
        init,
        payer = admin,
        space = 8 + TariffPerpMarket::SPACE,
        seeds = [b"market", oracle.key().as_ref(), usdc_mint.key().as_ref()],
        bump
    )]
    pub market: Account<'info, TariffPerpMarket>,

    /// Vault authority PDA: [b"vault_auth", market]
    /// CHECK: PDA signer only
    #[account(
        seeds = [b"vault_auth", market.key().as_ref()],
        bump
    )]
    pub vault_authority: UncheckedAccount<'info>,

    /// Insurance authority PDA: [b"insurance", market]
    /// CHECK: PDA signer only
    #[account(
        seeds = [b"insurance", market.key().as_ref()],
        bump
    )]
    pub insurance_authority: UncheckedAccount<'info>,

    /// USDC vault ATA owned by vault_authority PDA.
    #[account(
        init_if_needed,
        payer = admin,
        associated_token::mint = usdc_mint,
        associated_token::authority = vault_authority
    )]
    pub vault_usdc: Account<'info, TokenAccount>,

    /// Insurance vault ATA owned by insurance_authority PDA.
    #[account(
        init_if_needed,
        payer = admin,
        associated_token::mint = usdc_mint,
        associated_token::authority = insurance_authority
    )]
    pub insurance_vault_usdc: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct SetMarketFlags<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(mut)]
    pub market: Account<'info, TariffPerpMarket>,
}

#[derive(Accounts)]
pub struct InitializeMargin<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    pub market: Account<'info, TariffPerpMarket>,

    /// Margin PDA: [b"margin", market, owner]
    #[account(
        init,
        payer = owner,
        space = 8 + MarginAccount::SPACE,
        seeds = [b"margin", market.key().as_ref(), owner.key().as_ref()],
        bump
    )]
    pub margin: Account<'info, MarginAccount>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DepositUsdc<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    pub market: Account<'info, TariffPerpMarket>,

    #[account(
        mut,
        seeds = [b"margin", market.key().as_ref(), owner.key().as_ref()],
        bump,
        constraint = margin.owner == owner.key() @ TariffError::Unauthorized,
        constraint = margin.market == market.key() @ TariffError::InvalidMarket
    )]
    pub margin: Account<'info, MarginAccount>,

    #[account(mut)]
    pub user_usdc: Account<'info, TokenAccount>,

    /// CHECK: PDA signer only
    #[account(
        seeds = [b"vault_auth", market.key().as_ref()],
        bump = market.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(mut)]
    pub vault_usdc: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct WithdrawUsdc<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    pub oracle: Account<'info, TariffOracle>,

    #[account(
        mut,
        constraint = market.oracle == oracle.key() @ TariffError::InvalidOracle
    )]
    pub market: Account<'info, TariffPerpMarket>,

    #[account(
        mut,
        seeds = [b"margin", market.key().as_ref(), owner.key().as_ref()],
        bump,
        constraint = margin.owner == owner.key() @ TariffError::Unauthorized,
        constraint = margin.market == market.key() @ TariffError::InvalidMarket
    )]
    pub margin: Account<'info, MarginAccount>,

    /// CHECK: Pyth SOL/USD feed account (must match market.pyth_sol_usd_feed)
    pub pyth_feed: UncheckedAccount<'info>,

    /// CHECK: PDA signer only
    #[account(
        seeds = [b"vault_auth", market.key().as_ref()],
        bump = market.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(mut)]
    pub vault_usdc: Account<'info, TokenAccount>,

    #[account(mut)]
    pub user_usdc: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct OpenPosition<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    pub oracle: Account<'info, TariffOracle>,

    #[account(
        mut,
        constraint = market.oracle == oracle.key() @ TariffError::InvalidOracle
    )]
    pub market: Account<'info, TariffPerpMarket>,

    #[account(
        mut,
        seeds = [b"margin", market.key().as_ref(), owner.key().as_ref()],
        bump,
        constraint = margin.owner == owner.key() @ TariffError::Unauthorized,
        constraint = margin.market == market.key() @ TariffError::InvalidMarket
    )]
    pub margin: Account<'info, MarginAccount>,

    /// CHECK: Pyth SOL/USD feed account
    pub pyth_feed: UncheckedAccount<'info>,

    /// CHECK: PDA signer only
    #[account(
        seeds = [b"vault_auth", market.key().as_ref()],
        bump = market.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,

    /// CHECK: PDA signer only
    #[account(
        seeds = [b"insurance", market.key().as_ref()],
        bump = market.insurance_authority_bump
    )]
    pub insurance_authority: UncheckedAccount<'info>,

    #[account(mut)]
    pub vault_usdc: Account<'info, TokenAccount>,

    #[account(mut)]
    pub insurance_vault_usdc: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct ClosePosition<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    pub oracle: Account<'info, TariffOracle>,

    #[account(
        mut,
        constraint = market.oracle == oracle.key() @ TariffError::InvalidOracle
    )]
    pub market: Account<'info, TariffPerpMarket>,

    #[account(
        mut,
        seeds = [b"margin", market.key().as_ref(), owner.key().as_ref()],
        bump,
        constraint = margin.owner == owner.key() @ TariffError::Unauthorized,
        constraint = margin.market == market.key() @ TariffError::InvalidMarket
    )]
    pub margin: Account<'info, MarginAccount>,

    /// CHECK: Pyth SOL/USD feed account
    pub pyth_feed: UncheckedAccount<'info>,

    /// CHECK: PDA signer only
    #[account(
        seeds = [b"vault_auth", market.key().as_ref()],
        bump = market.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,

    /// CHECK: PDA signer only
    #[account(
        seeds = [b"insurance", market.key().as_ref()],
        bump = market.insurance_authority_bump
    )]
    pub insurance_authority: UncheckedAccount<'info>,

    #[account(mut)]
    pub vault_usdc: Account<'info, TokenAccount>,

    #[account(mut)]
    pub insurance_vault_usdc: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct ApplyFunding<'info> {
    pub oracle: Account<'info, TariffOracle>,

    #[account(
        mut,
        constraint = market.oracle == oracle.key() @ TariffError::InvalidOracle
    )]
    pub market: Account<'info, TariffPerpMarket>,

    /// CHECK: Pyth SOL/USD feed account
    pub pyth_feed: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct Liquidate<'info> {
    #[account(mut)]
    pub liquidator: Signer<'info>,

    pub oracle: Account<'info, TariffOracle>,

    #[account(
        mut,
        constraint = market.oracle == oracle.key() @ TariffError::InvalidOracle
    )]
    pub market: Account<'info, TariffPerpMarket>,

    /// Margin account for user being liquidated
    #[account(
        mut,
        constraint = user_margin.market == market.key() @ TariffError::InvalidMarket
    )]
    pub user_margin: Account<'info, MarginAccount>,

    /// CHECK: Pyth SOL/USD feed account
    pub pyth_feed: UncheckedAccount<'info>,

    /// CHECK: PDA signer only
    #[account(
        seeds = [b"vault_auth", market.key().as_ref()],
        bump = market.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,

    /// CHECK: PDA signer only
    #[account(
        seeds = [b"insurance", market.key().as_ref()],
        bump = market.insurance_authority_bump
    )]
    pub insurance_authority: UncheckedAccount<'info>,

    #[account(mut)]
    pub vault_usdc: Account<'info, TokenAccount>,

    #[account(mut)]
    pub insurance_vault_usdc: Account<'info, TokenAccount>,

    #[account(mut)]
    pub liquidator_usdc: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

/// ------------------------------
/// State
/// ------------------------------

#[account]
pub struct TariffOracle {
    pub admin: Pubkey,
    pub baseline_tariff_bps: u16,
    pub valid_until_ts: i64,
    pub last_updated_ts: i64,
    pub confidence_bps: u16,

    // guardrails
    pub min_update_interval_secs: i64,
    pub max_jump_bps_per_update: u16,
    pub last_baseline_bps: u16,

    pub country_addons: [CountryAddon; MAX_COUNTRY_ADDONS],
    pub basket_weights: [BasketWeight; MAX_BASKET_WEIGHTS],
    pub addon_len: u8,
    pub weight_len: u8,

    pub _pad0: [u8; 6],
}

impl TariffOracle {
    pub const SPACE: usize = 32 // admin
        + 2 // baseline_tariff_bps
        + 8 // valid_until_ts
        + 8 // last_updated_ts
        + 2 // confidence_bps
        + 8 // min_update_interval_secs
        + 2 // max_jump_bps_per_update
        + 2 // last_baseline_bps
        + (CountryAddon::SPACE * MAX_COUNTRY_ADDONS)
        + (BasketWeight::SPACE * MAX_BASKET_WEIGHTS)
        + 1 // addon_len
        + 1 // weight_len
        + 6; // pad

    pub fn require_valid(&self, now: i64) -> Result<()> {
        require!(now <= self.valid_until_ts, TariffError::OracleStale);
        Ok(())
    }

    pub fn require_can_update(&self, now: i64) -> Result<()> {
        let dt = now
            .checked_sub(self.last_updated_ts)
            .ok_or(TariffError::MathOverflow)?;
        require!(dt >= 0, TariffError::MathOverflow);
        require!(
            dt >= self.min_update_interval_secs,
            TariffError::OracleUpdateTooSoon
        );
        Ok(())
    }

    /// Compute tariff_index_bps:
    /// index_bps = baseline + Σ (weight_bps * addon_bps / 10_000) for matching enabled country codes.
    /// Clamp [0, 50_000] bps.
    pub fn tariff_index_bps_i128(&self) -> Result<i128> {
        let mut idx: i128 = self.baseline_tariff_bps as i128;

        let addon_len = self.addon_len as usize;
        let weight_len = self.weight_len as usize;

        for ai in 0..addon_len {
            let a = self.country_addons[ai];
            if !a.enabled {
                continue;
            }
            for wi in 0..weight_len {
                let w = self.basket_weights[wi];
                if !w.enabled {
                    continue;
                }
                if w.country_code != a.country_code {
                    continue;
                }

                let weight = w.weight_bps as i128;
                let addon = a.addon_bps as i128; // signed
                let contrib = weight
                    .checked_mul(addon)
                    .ok_or(TariffError::MathOverflow)?
                    .checked_div(BPS_DENOM_I128)
                    .ok_or(TariffError::MathOverflow)?;
                idx = idx.checked_add(contrib).ok_or(TariffError::MathOverflow)?;
            }
        }

        idx = clamp_i128(idx, TARIFF_BPS_MIN, TARIFF_BPS_MAX)?;
        Ok(idx)
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Copy, Clone)]
pub struct CountryAddon {
    pub country_code: [u8; 2],
    pub addon_bps: i16, // signed bps (can be negative)
    pub enabled: bool,
    pub _pad: [u8; 3],
}
impl CountryAddon {
    pub const SPACE: usize = 2 + 2 + 1 + 3;
}
impl Default for CountryAddon {
    fn default() -> Self {
        Self {
            country_code: [0u8; 2],
            addon_bps: 0,
            enabled: false,
            _pad: [0u8; 3],
        }
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Copy, Clone)]
pub struct BasketWeight {
    pub country_code: [u8; 2],
    pub weight_bps: u16,
    pub enabled: bool,
    pub _pad: [u8; 3],
}
impl BasketWeight {
    pub const SPACE: usize = 2 + 2 + 1 + 3;
}
impl Default for BasketWeight {
    fn default() -> Self {
        Self {
            country_code: [0u8; 2],
            weight_bps: 0,
            enabled: false,
            _pad: [0u8; 3],
        }
    }
}

#[account]
pub struct TariffPerpMarket {
    pub admin: Pubkey,
    pub oracle: Pubkey,
    pub usdc_mint: Pubkey,

    pub vault_authority_bump: u8,
    pub insurance_authority_bump: u8,
    pub _pad0: [u8; 6],

    pub vault_usdc: Pubkey,
    pub insurance_vault_usdc: Pubkey,

    pub pyth_sol_usd_feed: Pubkey,
    pub last_pyth_publish_time: i64,

    // vAMM
    pub base_reserve: i128,
    pub quote_reserve: i128,
    pub invariant_k: i128,

    // funding (per-period)
    pub funding_index: i128,
    pub last_funding_ts: i64,
    pub funding_period_secs: i64,

    // margins + fees
    pub initial_margin_bps: u16,
    pub maintenance_margin_bps: u16,
    pub trade_fee_bps: u16,
    pub liquidation_fee_bps: u16,

    // insurance config
    pub max_insurance_payout_per_liq_usdc: u64,
    pub fee_to_insurance_bps: u16,

    // vAMM guardrails
    pub min_trade_base: i128,
    pub max_price_impact_bps: u16,
    pub spread_bps: u16,

    // switches
    pub reduce_only: bool,
    pub paused: bool,
    pub _pad1: [u8; 2],

    // market-level tracking
    pub open_interest_base: i128,
    pub net_position_base: i128,

    // risk limits
    pub max_open_interest_base: i128,
    pub max_skew_base: i128,
}

impl TariffPerpMarket {
    // slightly generous sizing via explicit fields + pads in struct
    pub const SPACE: usize = 32 + 32 + 32
        + 1 + 1 + 6
        + 32 + 32
        + 32 + 8
        + 16 + 16 + 16
        + 16 + 8 + 8
        + 2 + 2 + 2 + 2
        + 8 + 2
        + 16 + 2 + 2
        + 1 + 1 + 2
        + 16 + 16
        + 16 + 16;

    /// mark = quote_reserve / base_reserve in Q64.64
    pub fn mark_price_q64(&self) -> Result<i128> {
        mark_price_q64_from_reserves(self.base_reserve, self.quote_reserve)
    }

    /// index price derived from tariff_index_bps where 10_000 bps = 1.0
    pub fn index_price_q64(&self, oracle: &TariffOracle) -> Result<i128> {
        let bps_i128 = oracle.tariff_index_bps_i128()?;
        let num = bps_i128.checked_mul(Q64).ok_or(TariffError::MathOverflow)?;
        num.checked_div(BPS_DENOM_I128).ok_or(TariffError::MathOverflow.into())
    }
}

#[account]
pub struct MarginAccount {
    pub owner: Pubkey,
    pub market: Pubkey,
    pub collateral_usdc: u64,     // micro-USDC
    pub position_base: i128,      // signed, BASE_Q
    pub entry_price_q64: i128,    // Q64.64
    pub last_funding_index: i128, // last observed market funding_index
    pub realized_pnl_usdc: i64,   // micro-USDC signed
    pub open_notional_q64: i128,  // optional bookkeeping
}
impl MarginAccount {
    pub const SPACE: usize = 32 + 32 + 8 + 16 + 16 + 16 + 8 + 16;
}

/// ------------------------------
/// Events
/// ------------------------------

#[event]
pub struct OracleUpdateEvent {
    pub oracle: Pubkey,
    pub admin: Pubkey,
    pub baseline_bps: u16,
    pub valid_until_ts: i64,
    pub ts: i64,
}

#[event]
pub struct FundingAppliedEvent {
    pub market: Pubkey,
    pub dt: i64,
    pub rate_q64: i128,
    pub funding_index: i128,
    pub mark_q64: i128,
    pub index_q64: i128,
    pub ts: i64,
}

#[event]
pub struct LiquidationEvent {
    pub market: Pubkey,
    pub user: Pubkey,
    pub liquidator: Pubkey,
    pub closed_base: i128,
    pub closed_notional_usdc: u64,
    pub fee_usdc: u64,
    pub equity_before: i128,
    pub equity_after: i128,
    pub ts: i64,
}

#[event]
pub struct TradeEvent {
    pub market: Pubkey,
    pub user: Pubkey,
    pub side: u8,
    pub base_delta: i128,
    pub exec_price_q64: i128,
    pub fee_usdc: u64,
    pub mark_before_q64: i128,
    pub mark_after_q64: i128,
    pub ts: i64,
}

#[event]
pub struct DepositEvent {
    pub market: Pubkey,
    pub user: Pubkey,
    pub amount_usdc: u64,
    pub ts: i64,
}

#[event]
pub struct WithdrawEvent {
    pub market: Pubkey,
    pub user: Pubkey,
    pub amount_usdc: u64,
    pub ts: i64,
}

#[event]
pub struct BadDebtEvent {
    pub market: Pubkey,
    pub user: Pubkey,
    pub debt_usdc: u64,
    pub covered_usdc: u64,
    pub ts: i64,
}

/// ------------------------------
/// Helpers: math + accounting
/// ------------------------------

fn clamp_i128(x: i128, lo: i128, hi: i128) -> Result<i128> {
    require!(lo <= hi, TariffError::MathOverflow);
    Ok(if x < lo { lo } else if x > hi { hi } else { x })
}

fn i128_abs(x: i128) -> Result<i128> {
    if x == i128::MIN {
        return err!(TariffError::MathOverflow);
    }
    Ok(if x < 0 { -x } else { x })
}

fn i128_to_u64(x: i128) -> Result<u64> {
    require!(x >= 0, TariffError::MathOverflow);
    require!(x <= u64::MAX as i128, TariffError::MathOverflow);
    Ok(x as u64)
}

fn i128_to_i64(x: i128) -> Result<i64> {
    require!(x >= i64::MIN as i128, TariffError::MathOverflow);
    require!(x <= i64::MAX as i128, TariffError::MathOverflow);
    Ok(x as i64)
}

fn bps_i128_to_q64(bps: i128) -> Result<i128> {
    let num = bps.checked_mul(Q64).ok_or(TariffError::MathOverflow)?;
    num.checked_div(BPS_DENOM_I128).ok_or(TariffError::MathOverflow.into())
}

fn mark_price_q64_from_reserves(base: i128, quote: i128) -> Result<i128> {
    require!(base > 0, TariffError::InvalidReserve);
    require!(quote > 0, TariffError::InvalidReserve);
    let num = quote.checked_mul(Q64).ok_or(TariffError::MathOverflow)?;
    num.checked_div(base).ok_or(TariffError::MathOverflow.into())
}

/// spread applied to execution price:
/// - long: price * (1 + spread)
/// - short: price * (1 - spread)
fn apply_spread_q64(exec_price_q64: i128, spread_bps: u16, side: u8) -> Result<i128> {
    require!(spread_bps <= 10_000, TariffError::InvalidBps);
    let spread_i128 = spread_bps as i128;

    let mult_bps = if side == 0 {
        BPS_DENOM_I128.checked_add(spread_i128).ok_or(TariffError::MathOverflow)?
    } else {
        BPS_DENOM_I128.checked_sub(spread_i128).ok_or(TariffError::MathOverflow)?
    };

    let num = exec_price_q64.checked_mul(mult_bps).ok_or(TariffError::MathOverflow)?;
    num.checked_div(BPS_DENOM_I128).ok_or(TariffError::MathOverflow.into())
}

/// Convert abs(base) * price_q64 -> micro-USDC (i128):
/// quote_micro = (base_abs * price_q64 / Q64) / BASE_Q
fn base_times_price_to_usdc_micro(base_abs: i128, price_q64: i128) -> Result<i128> {
    let num = base_abs.checked_mul(price_q64).ok_or(TariffError::MathOverflow)?;
    let q = num.checked_div(Q64).ok_or(TariffError::MathOverflow)?;
    q.checked_div(BASE_Q).ok_or(TariffError::MathOverflow.into())
}

/// Unrealized PnL (micro-USDC, signed) using mark vs entry:
/// pnl = position_base/BASE_Q * (mark - entry)
fn unrealized_pnl_usdc_micro(margin: &MarginAccount, mark_q64: i128) -> Result<i128> {
    let pos = margin.position_base;
    if pos == 0 {
        return Ok(0);
    }
    let diff_q64 = mark_q64
        .checked_sub(margin.entry_price_q64)
        .ok_or(TariffError::MathOverflow)?;

    let num = pos.checked_mul(diff_q64).ok_or(TariffError::MathOverflow)?;
    let q = num.checked_div(Q64).ok_or(TariffError::MathOverflow)?;
    q.checked_div(BASE_Q).ok_or(TariffError::MathOverflow.into())
}

/// Compute equity (micro-USDC, i128) and notional (micro-USDC, i128) at current mark.
fn compute_equity_and_notional_usdc(
    market: &TariffPerpMarket,
    margin: &MarginAccount,
) -> Result<(i128, i128)> {
    let collat_i128 = margin.collateral_usdc as i128;
    let mark_q64 = market.mark_price_q64()?;

    let pnl = unrealized_pnl_usdc_micro(margin, mark_q64)?;
    let equity = collat_i128.checked_add(pnl).ok_or(TariffError::MathOverflow)?;

    let pos_abs = i128_abs(margin.position_base)?;
    let notional = base_times_price_to_usdc_micro(pos_abs, mark_q64)?;
    Ok((equity, notional))
}

/// Funding settlement:
/// funding_micro = (pos * (market.funding_index - user.last_funding_index) / Q64) / BASE_Q
fn settle_funding(market: &TariffPerpMarket, margin: &mut MarginAccount) -> Result<()> {
    let delta = market
        .funding_index
        .checked_sub(margin.last_funding_index)
        .ok_or(TariffError::MathOverflow)?;

    if delta == 0 || margin.position_base == 0 {
        margin.last_funding_index = market.funding_index;
        return Ok(());
    }

    let num = margin.position_base.checked_mul(delta).ok_or(TariffError::MathOverflow)?;
    let q = num.checked_div(Q64).ok_or(TariffError::MathOverflow)?;
    let funding_usdc = q.checked_div(BASE_Q).ok_or(TariffError::MathOverflow)?; // signed i128

    if funding_usdc < 0 {
        let debit = funding_usdc.checked_neg().ok_or(TariffError::MathOverflow)?;
        let debit_u64 = i128_to_u64(debit)?;
        if margin.collateral_usdc >= debit_u64 {
            margin.collateral_usdc = margin
                .collateral_usdc
                .checked_sub(debit_u64)
                .ok_or(TariffError::MathOverflow)?;
        } else {
            margin.collateral_usdc = 0;
        }
    } else {
        let credit_u64 = i128_to_u64(funding_usdc)?;
        margin.collateral_usdc = margin
            .collateral_usdc
            .checked_add(credit_u64)
            .ok_or(TariffError::MathOverflow)?;
    }

    margin.last_funding_index = market.funding_index;
    Ok(())
}

/// vAMM constant product swap:
/// - signed_delta_base > 0 => user buys base (long): base_reserve decreases, quote_reserve increases
/// - signed_delta_base < 0 => user sells base (short): base_reserve increases, quote_reserve decreases
///
/// Returns (exec_price_q64, quote_delta_micro_signed, new_base, new_quote)
fn vamm_swap(
    market: &TariffPerpMarket,
    signed_delta_base: i128,
) -> Result<(i128, i128, i128, i128)> {
    require!(signed_delta_base != 0, TariffError::InvalidAmount);

    let base = market.base_reserve;
    let quote = market.quote_reserve;
    let k = market.invariant_k;

    require!(base > 0 && quote > 0 && k > 0, TariffError::InvalidReserve);

    if signed_delta_base > 0 {
        // buy base => base decreases
        let delta = signed_delta_base;
        require!(delta < base, TariffError::InsufficientLiquidity);

        let new_base = base.checked_sub(delta).ok_or(TariffError::MathOverflow)?;
        require!(new_base > 0, TariffError::InsufficientLiquidity);

        let new_quote = k.checked_div(new_base).ok_or(TariffError::MathOverflow)?;
        require!(new_quote > quote, TariffError::MathOverflow);

        let quote_in = new_quote.checked_sub(quote).ok_or(TariffError::MathOverflow)?;
        let price_q64 = quote_in
            .checked_mul(Q64)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(delta)
            .ok_or(TariffError::MathOverflow)?;
        Ok((price_q64, quote_in, new_base, new_quote))
    } else {
        // sell base => base increases
        let delta_abs = signed_delta_base.checked_neg().ok_or(TariffError::MathOverflow)?;

        let new_base = base.checked_add(delta_abs).ok_or(TariffError::MathOverflow)?;
        require!(new_base > 0, TariffError::InvalidReserve);

        let new_quote = k.checked_div(new_base).ok_or(TariffError::MathOverflow)?;
        require!(new_quote < quote, TariffError::InsufficientLiquidity);

        let quote_out = quote.checked_sub(new_quote).ok_or(TariffError::MathOverflow)?;
        let price_q64 = quote_out
            .checked_mul(Q64)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(delta_abs)
            .ok_or(TariffError::MathOverflow)?;

        let quote_delta_signed = quote_out.checked_neg().ok_or(TariffError::MathOverflow)?;
        Ok((price_q64, quote_delta_signed, new_base, new_quote))
    }
}

/// PnL for closing some signed base at exec:
/// pnl_micro = (signed_base * (exec - entry) / Q64) / BASE_Q
fn pnl_on_close_usdc_micro(signed_base: i128, entry_q64: i128, exec_q64: i128) -> Result<i128> {
    let diff = exec_q64.checked_sub(entry_q64).ok_or(TariffError::MathOverflow)?;
    let num = signed_base.checked_mul(diff).ok_or(TariffError::MathOverflow)?;
    let q = num.checked_div(Q64).ok_or(TariffError::MathOverflow)?;
    q.checked_div(BASE_Q).ok_or(TariffError::MathOverflow.into())
}

fn apply_realized_pnl(margin: &mut MarginAccount, pnl_usdc_micro: i128) -> Result<()> {
    let pnl_i64 = i128_to_i64(pnl_usdc_micro)?;
    margin.realized_pnl_usdc = margin
        .realized_pnl_usdc
        .checked_add(pnl_i64)
        .ok_or(TariffError::MathOverflow)?;

    if pnl_usdc_micro < 0 {
        let debit = pnl_usdc_micro.checked_neg().ok_or(TariffError::MathOverflow)?;
        let debit_u64 = i128_to_u64(debit)?;
        if margin.collateral_usdc >= debit_u64 {
            margin.collateral_usdc = margin
                .collateral_usdc
                .checked_sub(debit_u64)
                .ok_or(TariffError::MathOverflow)?;
        } else {
            margin.collateral_usdc = 0;
        }
    } else {
        let credit_u64 = i128_to_u64(pnl_usdc_micro)?;
        margin.collateral_usdc = margin
            .collateral_usdc
            .checked_add(credit_u64)
            .ok_or(TariffError::MathOverflow)?;
    }
    Ok(())
}

/// Update position + entry price:
/// - If adding same direction: weighted avg entry.
/// - If reducing: realize pnl on closed portion.
/// - If closing fully: reset entry.
fn update_position_and_entry(
    margin: &mut MarginAccount,
    exec_price_q64: i128,
    signed_delta: i128,
) -> Result<()> {
    let old_pos = margin.position_base;
    let new_pos = old_pos.checked_add(signed_delta).ok_or(TariffError::MathOverflow)?;

    if old_pos == 0 {
        margin.position_base = new_pos;
        margin.entry_price_q64 = exec_price_q64;
        margin.open_notional_q64 = 0;
        return Ok(());
    }

    if new_pos == 0 {
        let pnl = pnl_on_close_usdc_micro(old_pos, margin.entry_price_q64, exec_price_q64)?;
        apply_realized_pnl(margin, pnl)?;
        margin.position_base = 0;
        margin.entry_price_q64 = 0;
        margin.open_notional_q64 = 0;
        return Ok(());
    }

    // same sign and abs increased => weighted avg
    let old_abs = i128_abs(old_pos)?;
    let new_abs = i128_abs(new_pos)?;
    let delta_abs = i128_abs(signed_delta)?;

    if same_sign_nonzero(old_pos, new_pos) && new_abs > old_abs {
        let num1 = old_abs
            .checked_mul(margin.entry_price_q64)
            .ok_or(TariffError::MathOverflow)?;
        let num2 = delta_abs
            .checked_mul(exec_price_q64)
            .ok_or(TariffError::MathOverflow)?;
        let num = num1.checked_add(num2).ok_or(TariffError::MathOverflow)?;
        let avg = num.checked_div(new_abs).ok_or(TariffError::MathOverflow)?;
        margin.position_base = new_pos;
        margin.entry_price_q64 = avg;
        return Ok(());
    }

    // reduction/flip (delta opposite sign)
    if (old_pos > 0 && signed_delta < 0) || (old_pos < 0 && signed_delta > 0) {
        let closed_abs = if delta_abs > old_abs { old_abs } else { delta_abs };
        let closed_signed = if old_pos > 0 {
            closed_abs
        } else {
            closed_abs.checked_neg().ok_or(TariffError::MathOverflow)?
        };
        let pnl = pnl_on_close_usdc_micro(closed_signed, margin.entry_price_q64, exec_price_q64)?;
        apply_realized_pnl(margin, pnl)?;

        margin.position_base = new_pos;

        // flip -> remainder uses exec entry
        if delta_abs > old_abs {
            margin.entry_price_q64 = exec_price_q64;
        }
        return Ok(());
    }

    // fallback
    margin.position_base = new_pos;
    Ok(())
}

fn same_sign_nonzero(a: i128, b: i128) -> bool {
    (a > 0 && b > 0) || (a < 0 && b < 0)
}

/// Market-level risk pre-check with proposed user change.
fn check_market_risk_after_change(market: &TariffPerpMarket, old_pos: i128, new_pos: i128) -> Result<()> {
    let abs_old = i128_abs(old_pos)?;
    let abs_new = i128_abs(new_pos)?;

    let delta_oi = abs_new.checked_sub(abs_old).ok_or(TariffError::MathOverflow)?;
    let new_open_interest = market
        .open_interest_base
        .checked_add(delta_oi)
        .ok_or(TariffError::MathOverflow)?;
    require!(new_open_interest >= 0, TariffError::MathOverflow);
    require!(new_open_interest <= market.max_open_interest_base, TariffError::RiskLimit);

    let delta_net = new_pos.checked_sub(old_pos).ok_or(TariffError::MathOverflow)?;
    let new_net = market
        .net_position_base
        .checked_add(delta_net)
        .ok_or(TariffError::MathOverflow)?;
    let abs_net = i128_abs(new_net)?;
    require!(abs_net <= market.max_skew_base, TariffError::RiskLimit);

    Ok(())
}

/// Apply market-level tracking updates after position change succeeds.
fn apply_position_change_to_market(market: &mut TariffPerpMarket, old_pos: i128, new_pos: i128) -> Result<()> {
    let abs_old = i128_abs(old_pos)?;
    let abs_new = i128_abs(new_pos)?;

    let delta_oi = abs_new.checked_sub(abs_old).ok_or(TariffError::MathOverflow)?;
    market.open_interest_base = market
        .open_interest_base
        .checked_add(delta_oi)
        .ok_or(TariffError::MathOverflow)?;

    let delta_net = new_pos.checked_sub(old_pos).ok_or(TariffError::MathOverflow)?;
    market.net_position_base = market
        .net_position_base
        .checked_add(delta_net)
        .ok_or(TariffError::MathOverflow)?;

    // enforce invariants
    require!(market.open_interest_base >= 0, TariffError::MathOverflow);
    require!(
        market.open_interest_base <= market.max_open_interest_base,
        TariffError::RiskLimit
    );
    let abs_net = i128_abs(market.net_position_base)?;
    require!(abs_net <= market.max_skew_base, TariffError::RiskLimit);

    Ok(())
}

/// ------------------------------
/// Pyth Sanity Guard (SOL/USD)
///
/// Uses pyth_sdk 0.8.x APIs:
/// - load_price_feed_from_account_info
/// - get_price_no_older_than(now, age_secs_u64) -> Option<Price>
///
/// Enforces:
/// - staleness (no older than bound + defensive checks)
/// - price.price > 0
/// - relative confidence ratio (conf/price) <= PYTH_MAX_CONF_BPS
/// - monotonic publish_time (market.last_pyth_publish_time)
/// - updates market.last_pyth_publish_time when passing
/// ------------------------------
fn pyth_sanity_check_update_market(
    market: &mut TariffPerpMarket,
    pyth_feed: &UncheckedAccount,
    now: i64,
) -> Result<Price> {
    let feed: PriceFeed = load_price_feed_from_account_info(&pyth_feed.to_account_info())
        .map_err(|_| error!(TariffError::PythLoadFailed))?;

    let price: Price = feed
        .get_price_no_older_than(now, PYTH_MAX_STALENESS_SECS_U64)
        .ok_or(TariffError::PythStale)?;

    // defensive staleness checks
    let age = now.checked_sub(price.publish_time).ok_or(TariffError::MathOverflow)?;
    require!(age >= 0, TariffError::MathOverflow);
    require!(age <= PYTH_MAX_STALENESS_SECS_I64, TariffError::PythStale);

    // monotonic publish time
    require!(
        price.publish_time >= market.last_pyth_publish_time,
        TariffError::PythNonMonotonic
    );

    // positive price
    require!(price.price > 0, TariffError::PythBadPrice);

    // abs_price as u64
    require!(price.price <= u64::MAX as i64, TariffError::MathOverflow);
    let abs_price_u64: u64 = price.price as u64;
    require!(abs_price_u64 > 0, TariffError::PythBadPrice);

    // conf_ratio_bps = conf * 10_000 / abs_price
    let conf_u128: u128 = price.conf as u128;
    let num = conf_u128
        .checked_mul(BPS_DENOM_U64 as u128)
        .ok_or(TariffError::MathOverflow)?;
    let denom = abs_price_u64 as u128;
    let ratio_bps_u128 = num.checked_div(denom).ok_or(TariffError::MathOverflow)?;
    require!(ratio_bps_u128 <= PYTH_MAX_CONF_BPS as u128, TariffError::PythConfidenceTooWide);

    // update
    market.last_pyth_publish_time = price.publish_time;

    Ok(price)
}

/// ------------------------------
/// Errors
/// ------------------------------
#[error_code]
pub enum TariffError {
    #[msg("Unauthorized")]
    Unauthorized,
    #[msg("Math overflow/underflow")]
    MathOverflow,
    #[msg("Invalid amount")]
    InvalidAmount,
    #[msg("Invalid side")]
    InvalidSide,
    #[msg("Invalid bps")]
    InvalidBps,
    #[msg("Invalid configuration")]
    InvalidConfig,

    #[msg("Oracle is stale/expired")]
    OracleStale,
    #[msg("Oracle update too soon (min update interval not satisfied)")]
    OracleUpdateTooSoon,
    #[msg("Baseline jump too large for this update")]
    BaselineJumpTooLarge,

    #[msg("Array is full")]
    ArrayFull,

    #[msg("Invalid reserve")]
    InvalidReserve,
    #[msg("Insufficient liquidity")]
    InsufficientLiquidity,

    #[msg("Invalid mint")]
    InvalidMint,
    #[msg("Invalid token account owner (authority mismatch)")]
    InvalidTokenOwner,

    #[msg("Invalid market")]
    InvalidMarket,
    #[msg("Invalid oracle")]
    InvalidOracle,

    #[msg("Insufficient collateral")]
    InsufficientCollateral,
    #[msg("Margin too low")]
    MarginTooLow,
    #[msg("Bad margin parameters")]
    BadMarginParams,

    #[msg("Risk limit exceeded")]
    RiskLimit,

    #[msg("Not liquidatable")]
    NotLiquidatable,
    #[msg("No position")]
    NoPosition,

    #[msg("Reduce-only mode: trade not allowed")]
    ReduceOnly,
    #[msg("Market is paused")]
    Paused,
    #[msg("Trade too small")]
    TradeTooSmall,
    #[msg("Price impact too high")]
    PriceImpactTooHigh,

    #[msg("Invalid Pyth feed account")]
    InvalidPythFeed,
    #[msg("Failed to load Pyth price feed")]
    PythLoadFailed,
    #[msg("Pyth returned no price within staleness bound")]
    PythStale,
    #[msg("Pyth price is non-positive")]
    PythBadPrice,
    #[msg("Pyth confidence ratio too wide")]
    PythConfidenceTooWide,
    #[msg("Pyth publish_time moved backwards")]
    PythNonMonotonic,
}
