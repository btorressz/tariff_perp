use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

// pyth crates (Solana Playground compatible for pyth-sdk 0.8.x)
use pyth_sdk::Price;
use pyth_sdk::PriceFeed;
use pyth_sdk_solana::load_price_feed_from_account_info;

declare_id!("G4g4DNdnxqTa8iAsiznzPKAwRk8JF56YsLZdNg2B7eBU");

/// ------------------------------
/// Constants + Fixed-Point Helpers
/// ------------------------------
/// Q64.64 fixed-point scale.
const Q64: i128 = 1i128 << 64;

/// Basis points divisor (1e4).
const BPS_DENOM: i128 = 10_000;

/// We represent "base units" (perp position size) in BASE_Q (micro-base).
/// This is purely internal for this POC and must be consistent everywhere.
const BASE_Q: i128 = 1_000_000; // 1e6

/// USDC decimals assumed 6 => "micro-USDC" (1e-6 USDC).
#[allow(dead_code)]
const USDC_Q: i128 = 1_000_000; // 1e6 (documents units)

/// Tariff index clamp [0, 50_000] bps (0% to 500%).
const TARIFF_BPS_MIN: i128 = 0;
const TARIFF_BPS_MAX: i128 = 50_000;

/// Funding settings (POC).
const FUNDING_DIVISOR_Q64: i128 = 8; // diff/8 per second (scaled below)
const MAX_FUNDING_Q64: i128 = Q64 / 100; // 1% per second (POC cap)

/// Pyth sanity guard.
/// NOTE: pyth_sdk::PriceFeed::get_price_no_older_than expects age_secs as u64.
const PYTH_MAX_STALENESS_SECS_I64: i64 = 60;
const PYTH_MAX_STALENESS_SECS_U64: u64 = 60;

/// Confidence bound in Pyth integer units (same units as price).
/// For SOL/USD (often expo=-8), 1_000_000 ~= $0.01 confidence.
const PYTH_MAX_CONF: u64 = 1_000_000;

/// Fixed-size limits for oracle arrays.
const MAX_COUNTRY_ADDONS: usize = 16;
const MAX_BASKET_WEIGHTS: usize = 16;

/// ------------------------------
/// Program
/// ------------------------------
#[program]
pub mod tariff_perp {
    use super::*;

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
        oracle.confidence_bps = confidence_bps;
        oracle.last_updated_ts = now;
        oracle.valid_until_ts = now
            .checked_add(valid_secs)
            .ok_or(TariffError::MathOverflow)?;

        oracle.addon_len = 0;
        oracle.weight_len = 0;
        oracle.country_addons = [CountryAddon::default(); MAX_COUNTRY_ADDONS];
        oracle.basket_weights = [BasketWeight::default(); MAX_BASKET_WEIGHTS];

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
        let oracle = &mut ctx.accounts.oracle;
        require_keys_eq!(oracle.admin, ctx.accounts.admin.key(), TariffError::Unauthorized);

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

        oracle.last_updated_ts = Clock::get()?.unix_timestamp;
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
        let oracle = &mut ctx.accounts.oracle;
        require_keys_eq!(oracle.admin, ctx.accounts.admin.key(), TariffError::Unauthorized);

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

        oracle.last_updated_ts = Clock::get()?.unix_timestamp;
        Ok(())
    }

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
        require!(trade_fee_bps <= 2_000, TariffError::InvalidBps);
        require!(liquidation_fee_bps <= 5_000, TariffError::InvalidBps);

        require!(base_reserve > 0, TariffError::InvalidReserve);
        require!(quote_reserve > 0, TariffError::InvalidReserve);

        let k = base_reserve
            .checked_mul(quote_reserve)
            .ok_or(TariffError::MathOverflow)?;

        let market = &mut ctx.accounts.market;
        market.admin = ctx.accounts.admin.key();
        market.oracle = ctx.accounts.oracle.key();
        market.usdc_mint = ctx.accounts.usdc_mint.key();

        market.vault_authority_bump = ctx.bumps.vault_authority;
        market.vault_usdc = ctx.accounts.vault_usdc.key();

        market.insurance_authority_bump = ctx.bumps.insurance_authority;
        market.insurance_vault_usdc = ctx.accounts.insurance_vault_usdc.key();

        market.pyth_sol_usd_feed = pyth_sol_usd_feed;

        market.base_reserve = base_reserve;
        market.quote_reserve = quote_reserve;
        market.invariant_k = k;

        market.funding_index = 0;
        market.last_funding_ts = Clock::get()?.unix_timestamp;

        market.initial_margin_bps = initial_margin_bps;
        market.maintenance_margin_bps = maintenance_margin_bps;

        market.trade_fee_bps = trade_fee_bps;
        market.liquidation_fee_bps = liquidation_fee_bps;

        market.max_open_interest_base = max_open_interest_base;
        market.max_skew_base = max_skew_base;
        market.open_interest_base = 0;

        Ok(())
    }

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

    /// Deposit USDC collateral into the market vault, tracked on-chain in the user's MarginAccount.
    pub fn deposit_usdc(ctx: Context<DepositUsdc>, amount: u64) -> Result<()> {
        require!(amount > 0, TariffError::InvalidAmount);

        require_keys_eq!(
            ctx.accounts.user_usdc.mint,
            ctx.accounts.market.usdc_mint,
            TariffError::InvalidMint
        );

        let cpi_accounts = Transfer {
            from: ctx.accounts.user_usdc.to_account_info(),
            to: ctx.accounts.vault_usdc.to_account_info(),
            authority: ctx.accounts.owner.to_account_info(),
        };
        token::transfer(
            CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts),
            amount,
        )?;

        let margin = &mut ctx.accounts.margin;
        margin.collateral_usdc = margin
            .collateral_usdc
            .checked_add(amount)
            .ok_or(TariffError::MathOverflow)?;
        Ok(())
    }

    /// Withdraw USDC collateral from the market vault.
    pub fn withdraw_usdc(ctx: Context<WithdrawUsdc>, amount: u64) -> Result<()> {
        require!(amount > 0, TariffError::InvalidAmount);
        let now = Clock::get()?.unix_timestamp;

        ctx.accounts.oracle.require_valid(now)?;

        require_keys_eq!(
            ctx.accounts.pyth_feed.key(),
            ctx.accounts.market.pyth_sol_usd_feed,
            TariffError::InvalidPythFeed
        );
        pyth_sanity_check(&ctx.accounts.pyth_feed, now)?;

        require_keys_eq!(
            ctx.accounts.user_usdc.mint,
            ctx.accounts.market.usdc_mint,
            TariffError::InvalidMint
        );

        settle_funding(&ctx.accounts.market, &mut ctx.accounts.margin)?;

        require!(
            ctx.accounts.margin.collateral_usdc >= amount,
            TariffError::InsufficientCollateral
        );

        let new_collateral = ctx
            .accounts
            .margin
            .collateral_usdc
            .checked_sub(amount)
            .ok_or(TariffError::MathOverflow)?;

        let (equity_usdc_i128, notional_usdc_i128) =
            compute_equity_and_notional_usdc(&ctx.accounts.market, &ctx.accounts.margin)?;

        let equity_sim = equity_usdc_i128
            .checked_sub(amount as i128)
            .ok_or(TariffError::MathOverflow)?;

        let req = notional_usdc_i128
            .checked_mul(ctx.accounts.market.initial_margin_bps as i128)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(BPS_DENOM)
            .ok_or(TariffError::MathOverflow)?;

        require!(equity_sim >= req, TariffError::MarginTooLow);

        ctx.accounts.margin.collateral_usdc = new_collateral;

        let market_key = ctx.accounts.market.key();
        let vault_seeds: &[&[u8]] = &[
            b"vault_auth",
            market_key.as_ref(),
            &[ctx.accounts.market.vault_authority_bump],
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

        Ok(())
    }

    /// Open/increase/decrease a position via vAMM swap.
    pub fn open_position(ctx: Context<OpenPosition>, side: u8, base_amount: i128) -> Result<()> {
        require!(base_amount > 0, TariffError::InvalidAmount);
        require!(side == 0 || side == 1, TariffError::InvalidSide);

        let now = Clock::get()?.unix_timestamp;

        ctx.accounts.oracle.require_valid(now)?;

        require_keys_eq!(
            ctx.accounts.pyth_feed.key(),
            ctx.accounts.market.pyth_sol_usd_feed,
            TariffError::InvalidPythFeed
        );
        pyth_sanity_check(&ctx.accounts.pyth_feed, now)?;

        settle_funding(&ctx.accounts.market, &mut ctx.accounts.margin)?;

        let signed_delta = if side == 0 {
            base_amount
        } else {
            base_amount.checked_neg().ok_or(TariffError::MathOverflow)?
        };

        enforce_risk_limits(&ctx.accounts.market, &ctx.accounts.margin, signed_delta)?;

        let (exec_price_q64, quote_delta_usdc_i128, new_base_reserve, new_quote_reserve) =
            vamm_swap(&ctx.accounts.market, signed_delta)?;

        let market = &mut ctx.accounts.market;
        market.base_reserve = new_base_reserve;
        market.quote_reserve = new_quote_reserve;

        let notional_abs = i128_abs(quote_delta_usdc_i128)?;
        let fee_usdc_i128 = notional_abs
            .checked_mul(market.trade_fee_bps as i128)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(BPS_DENOM)
            .ok_or(TariffError::MathOverflow)?;

        let fee_u64 = i128_to_u64(fee_usdc_i128)?;
        require!(
            ctx.accounts.margin.collateral_usdc >= fee_u64,
            TariffError::InsufficientCollateral
        );

        ctx.accounts.margin.collateral_usdc = ctx
            .accounts
            .margin
            .collateral_usdc
            .checked_sub(fee_u64)
            .ok_or(TariffError::MathOverflow)?;

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
            fee_u64,
        )?;

        update_position_and_entry(&mut ctx.accounts.margin, exec_price_q64, signed_delta)?;

        let (equity_usdc_i128, notional_usdc_i128) =
            compute_equity_and_notional_usdc(market, &ctx.accounts.margin)?;

        let req = notional_usdc_i128
            .checked_mul(market.initial_margin_bps as i128)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(BPS_DENOM)
            .ok_or(TariffError::MathOverflow)?;

        require!(equity_usdc_i128 >= req, TariffError::MarginTooLow);

        Ok(())
    }

    /// Apply funding to the market (callable by anyone).
    pub fn apply_funding(ctx: Context<ApplyFunding>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;

        ctx.accounts.oracle.require_valid(now)?;

        require_keys_eq!(
            ctx.accounts.pyth_feed.key(),
            ctx.accounts.market.pyth_sol_usd_feed,
            TariffError::InvalidPythFeed
        );
        pyth_sanity_check(&ctx.accounts.pyth_feed, now)?;

        let market = &mut ctx.accounts.market;

        let dt = now
            .checked_sub(market.last_funding_ts)
            .ok_or(TariffError::MathOverflow)?;
        require!(dt >= 0, TariffError::MathOverflow);
        if dt == 0 {
            return Ok(());
        }

        let mark_q64 = market.mark_price_q64()?;
        let index_q64 = market.index_price_q64(&ctx.accounts.oracle)?;

        let diff_q64 = mark_q64
            .checked_sub(index_q64)
            .ok_or(TariffError::MathOverflow)?;

        let mut rate_q64 = diff_q64
            .checked_div(FUNDING_DIVISOR_Q64)
            .ok_or(TariffError::MathOverflow)?;
        let neg_max = MAX_FUNDING_Q64.checked_neg().ok_or(TariffError::MathOverflow)?;
        rate_q64 = clamp_i128(rate_q64, neg_max, MAX_FUNDING_Q64)?;

        let dt_i128 = dt as i128;
        let delta_index = rate_q64
            .checked_mul(dt_i128)
            .ok_or(TariffError::MathOverflow)?;

        market.funding_index = market
            .funding_index
            .checked_add(delta_index)
            .ok_or(TariffError::MathOverflow)?;

        market.last_funding_ts = now;
        Ok(())
    }

    /// Liquidate a margin account if equity < maintenance requirement.
    pub fn liquidate(ctx: Context<Liquidate>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;

        ctx.accounts.oracle.require_valid(now)?;

        require_keys_eq!(
            ctx.accounts.pyth_feed.key(),
            ctx.accounts.market.pyth_sol_usd_feed,
            TariffError::InvalidPythFeed
        );
        pyth_sanity_check(&ctx.accounts.pyth_feed, now)?;

        settle_funding(&ctx.accounts.market, &mut ctx.accounts.user_margin)?;

        let market = &mut ctx.accounts.market;

        let (equity_usdc_i128, notional_usdc_i128) =
            compute_equity_and_notional_usdc(market, &ctx.accounts.user_margin)?;

        let maint_req = notional_usdc_i128
            .checked_mul(market.maintenance_margin_bps as i128)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(BPS_DENOM)
            .ok_or(TariffError::MathOverflow)?;

        require!(equity_usdc_i128 < maint_req, TariffError::NotLiquidatable);

        let pos = ctx.accounts.user_margin.position_base;
        require!(pos != 0, TariffError::NoPosition);

        let close_delta = pos.checked_neg().ok_or(TariffError::MathOverflow)?;

        let (exec_price_q64, _quote_delta, new_base_reserve, new_quote_reserve) =
            vamm_swap(market, close_delta)?;
        market.base_reserve = new_base_reserve;
        market.quote_reserve = new_quote_reserve;

        realize_full_close(&mut ctx.accounts.user_margin, exec_price_q64)?;

        let liq_fee_i128 = notional_usdc_i128
            .checked_mul(market.liquidation_fee_bps as i128)
            .ok_or(TariffError::MathOverflow)?
            .checked_div(BPS_DENOM)
            .ok_or(TariffError::MathOverflow)?;
        let liq_fee_u64 = i128_to_u64(liq_fee_i128)?;

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

        let half = fee_from_user / 2;
        let other_half = fee_from_user
            .checked_sub(half)
            .ok_or(TariffError::MathOverflow)?;

        let market_key = market.key();
        let vault_seeds: &[&[u8]] = &[
            b"vault_auth",
            market_key.as_ref(),
            &[market.vault_authority_bump],
        ];
        let vault_signer: &[&[&[u8]]] = &[vault_seeds];

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

        let equity_after = compute_equity_and_notional_usdc(market, &ctx.accounts.user_margin)?.0;
        if equity_after < 0 {
            let debt = equity_after.checked_neg().ok_or(TariffError::MathOverflow)?;
            let debt_u64 = i128_to_u64(debt)?;

            let insurance_bal = ctx.accounts.insurance_vault_usdc.amount;
            let cover = std::cmp::min(insurance_bal, debt_u64);

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
pub struct OracleUpsertAddon<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    /// Oracle PDA
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

    /// Oracle PDA
    #[account(
        mut,
        seeds = [b"oracle", oracle.admin.as_ref()],
        bump
    )]
    pub oracle: Account<'info, TariffOracle>,
}

#[derive(Accounts)]
pub struct InitializeMarket<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

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

    #[account(
        mut,
        constraint = vault_usdc.key() == market.vault_usdc @ TariffError::InvalidVault
    )]
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

    /// CHECK: Pyth SOL/USD feed account
    pub pyth_feed: UncheckedAccount<'info>,

    /// CHECK: PDA signer only
    #[account(
        seeds = [b"vault_auth", market.key().as_ref()],
        bump = market.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        mut,
        constraint = vault_usdc.key() == market.vault_usdc @ TariffError::InvalidVault
    )]
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

    #[account(
        mut,
        constraint = vault_usdc.key() == market.vault_usdc @ TariffError::InvalidVault
    )]
    pub vault_usdc: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = insurance_vault_usdc.key() == market.insurance_vault_usdc @ TariffError::InvalidVault
    )]
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

    #[account(
        mut,
        constraint = vault_usdc.key() == market.vault_usdc @ TariffError::InvalidVault
    )]
    pub vault_usdc: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = insurance_vault_usdc.key() == market.insurance_vault_usdc @ TariffError::InvalidVault
    )]
    pub insurance_vault_usdc: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = liquidator_usdc.mint == market.usdc_mint @ TariffError::InvalidMint
    )]
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

    pub country_addons: [CountryAddon; MAX_COUNTRY_ADDONS],
    pub basket_weights: [BasketWeight; MAX_BASKET_WEIGHTS],
    pub addon_len: u8,
    pub weight_len: u8,
}

impl TariffOracle {
    pub const SPACE: usize = 32 + 2 + 8 + 8 + 2
        + (CountryAddon::SPACE * MAX_COUNTRY_ADDONS)
        + (BasketWeight::SPACE * MAX_BASKET_WEIGHTS)
        + 1
        + 1;

    pub fn require_valid(&self, now: i64) -> Result<()> {
        require!(now <= self.valid_until_ts, TariffError::OracleStale);
        Ok(())
    }

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
                let addon = a.addon_bps as i128;
                let contrib = weight
                    .checked_mul(addon)
                    .ok_or(TariffError::MathOverflow)?
                    .checked_div(BPS_DENOM)
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
    pub addon_bps: i16,
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

    pub base_reserve: i128,
    pub quote_reserve: i128,
    pub invariant_k: i128,

    pub funding_index: i128,
    pub last_funding_ts: i64,

    pub initial_margin_bps: u16,
    pub maintenance_margin_bps: u16,
    pub trade_fee_bps: u16,
    pub liquidation_fee_bps: u16,

    pub max_open_interest_base: i128,
    pub max_skew_base: i128,
    pub open_interest_base: i128,
}

impl TariffPerpMarket {
    pub const SPACE: usize = 32 + 32 + 32
        + 1 + 1 + 6
        + 32 + 32
        + 32
        + 16 + 16 + 16
        + 16 + 8
        + 2 + 2 + 2 + 2
        + 16 + 16 + 16;

    pub fn mark_price_q64(&self) -> Result<i128> {
        require!(self.base_reserve > 0, TariffError::InvalidReserve);
        let num = self
            .quote_reserve
            .checked_mul(Q64)
            .ok_or(TariffError::MathOverflow)?;
        num.checked_div(self.base_reserve)
            .ok_or(TariffError::MathOverflow.into())
    }

    pub fn index_price_q64(&self, oracle: &TariffOracle) -> Result<i128> {
        let bps_i128 = oracle.tariff_index_bps_i128()?;
        let num = bps_i128.checked_mul(Q64).ok_or(TariffError::MathOverflow)?;
        num.checked_div(BPS_DENOM)
            .ok_or(TariffError::MathOverflow.into())
    }
}

#[account]
pub struct MarginAccount {
    pub owner: Pubkey,
    pub market: Pubkey,
    pub collateral_usdc: u64,
    pub position_base: i128,
    pub entry_price_q64: i128,
    pub last_funding_index: i128,
    pub realized_pnl_usdc: i64,
    pub open_notional_q64: i128,
}
impl MarginAccount {
    pub const SPACE: usize = 32 + 32 + 8 + 16 + 16 + 16 + 8 + 16;
}

/// ------------------------------
/// Events
/// ------------------------------
#[event]
pub struct BadDebtEvent {
    pub market: Pubkey,
    pub user: Pubkey,
    pub debt_usdc: u64,
    pub covered_usdc: u64,
    pub ts: i64,
}

/// ------------------------------
/// Helpers
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

fn base_times_price_to_usdc_micro(base_abs: i128, price_q64: i128) -> Result<i128> {
    let num = base_abs
        .checked_mul(price_q64)
        .ok_or(TariffError::MathOverflow)?;
    let q = num.checked_div(Q64).ok_or(TariffError::MathOverflow)?;
    q.checked_div(BASE_Q).ok_or(TariffError::MathOverflow.into())
}

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
    let funding_usdc = q.checked_div(BASE_Q).ok_or(TariffError::MathOverflow)?;

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

    if (old_pos > 0 && new_pos > 0 && i128_abs(new_pos)? > i128_abs(old_pos)?)
        || (old_pos < 0 && new_pos < 0 && i128_abs(new_pos)? > i128_abs(old_pos)?)
    {
        let old_abs = i128_abs(old_pos)?;
        let delta_abs = i128_abs(signed_delta)?;
        let new_abs = i128_abs(new_pos)?;

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

    if (old_pos > 0 && signed_delta < 0) || (old_pos < 0 && signed_delta > 0) {
        let old_abs = i128_abs(old_pos)?;
        let delta_abs = i128_abs(signed_delta)?;
        let closed_abs = if delta_abs > old_abs { old_abs } else { delta_abs };

        let closed_signed = if old_pos > 0 {
            closed_abs
        } else {
            closed_abs.checked_neg().ok_or(TariffError::MathOverflow)?
        };

        let pnl = pnl_on_close_usdc_micro(closed_signed, margin.entry_price_q64, exec_price_q64)?;
        apply_realized_pnl(margin, pnl)?;

        margin.position_base = new_pos;

        if delta_abs > old_abs {
            margin.entry_price_q64 = exec_price_q64;
        }
        return Ok(());
    }

    margin.position_base = new_pos;
    Ok(())
}

fn realize_full_close(margin: &mut MarginAccount, exec_q64: i128) -> Result<()> {
    let pos = margin.position_base;
    if pos == 0 {
        return Ok(());
    }
    let pnl = pnl_on_close_usdc_micro(pos, margin.entry_price_q64, exec_q64)?;
    apply_realized_pnl(margin, pnl)?;
    margin.position_base = 0;
    margin.entry_price_q64 = 0;
    margin.open_notional_q64 = 0;
    Ok(())
}

fn enforce_risk_limits(
    market: &TariffPerpMarket,
    margin: &MarginAccount,
    signed_delta: i128,
) -> Result<()> {
    let new_pos = margin
        .position_base
        .checked_add(signed_delta)
        .ok_or(TariffError::MathOverflow)?;
    let abs_new = i128_abs(new_pos)?;
    require!(abs_new <= market.max_open_interest_base, TariffError::RiskLimit);
    require!(abs_new <= market.max_skew_base, TariffError::RiskLimit);
    Ok(())
}

/// ------------------------------
/// Pyth Sanity Guard (SOL/USD)
///
/// Fixes:
/// - get_price_no_older_than expects `u64` age secs => use PYTH_MAX_STALENESS_SECS_U64.
/// - pyth_sdk::Price has no `status` field => we can't check Trading/OK from this type.
///   We *do* enforce staleness + confidence, which are available.
/// ------------------------------
fn pyth_sanity_check(pyth_feed: &UncheckedAccount, now: i64) -> Result<()> {
    let feed: PriceFeed = load_price_feed_from_account_info(&pyth_feed.to_account_info())
        .map_err(|_| error!(TariffError::PythLoadFailed))?;

    let price: Price = feed
        .get_price_no_older_than(now, PYTH_MAX_STALENESS_SECS_U64)
        .ok_or(TariffError::PythStale)?;

    // Extra explicit staleness check (defensive)
    let age = now
        .checked_sub(price.publish_time)
        .ok_or(TariffError::MathOverflow)?;
    require!(age >= 0, TariffError::MathOverflow);
    require!(age <= PYTH_MAX_STALENESS_SECS_I64, TariffError::PythStale);

    // Confidence bound
    require!(
        price.conf <= PYTH_MAX_CONF,
        TariffError::PythConfidenceTooWide
    );

    Ok(())
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
    #[msg("Oracle is stale/expired")]
    OracleStale,
    #[msg("Array is full")]
    ArrayFull,
    #[msg("Invalid reserve")]
    InvalidReserve,
    #[msg("Insufficient liquidity")]
    InsufficientLiquidity,
    #[msg("Invalid mint")]
    InvalidMint,
    #[msg("Invalid vault account")]
    InvalidVault,
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
    #[msg("Invalid Pyth feed account")]
    InvalidPythFeed,
    #[msg("Failed to load Pyth price feed")]
    PythLoadFailed,
    #[msg("Pyth returned no price within staleness bound")]
    PythStale,
    #[msg("Pyth confidence interval too wide")]
    PythConfidenceTooWide,
}