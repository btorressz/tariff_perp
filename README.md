# 🌎📈 Tariff Perpetual Futures Program (Solana / Anchor)

> ### ⚠️ Note
> **This proof-of-concept was developed in _Solana Playground_ using _Anchor_.**  

---

## ### ***✨ Overview***

This project is a **Tariff Index Oracle + Perpetual Futures market** implemented as a single Solana program. It models a world where **tariff policy shocks** (baseline tariffs + country-specific add-ons) can be turned into a tradable on-chain index, and a perp market can offer **hedging** and **speculation** on “global tariff pressure.”

### ✅ At a high level

- **🧾 TariffOracle (admin-updated)** stores:
  - **baseline tariff** (bps)
  - optional **country add-ons** (bps, signed — can be negative)
  - optional **basket weights** (bps)
  - **validity window** (staleness protection)
  - **update guardrails** (anti-spam + anti-jump)

- **🧠 TariffPerpMarket** references the oracle and runs a perp venue with:
  - **💵 USDC collateral** deposited into a PDA-owned vault (SPL tokens)
  - **📊 vAMM pricing** (constant product)
  - **⏱️ funding** in discrete periods (keeps mark aligned with index)
  - **🧯 partial liquidations** (unwinds risk gradually)
  - **🛡️ insurance fund** (fee routing + bad-debt backstop with a cap)
  - **🛰️ Pyth SOL/USD sanity guard** (blocks risky ops if feed is stale / low-quality)

> ✅ **POC / research-grade demo**  
> ❌ Not a production perp exchange (no orderbook, no advanced liquidation auctions, etc.)

---

## ### ***🧠 Key Concepts***

## ### ***🧾 Tariff Index (BPS)***

The oracle computes a tariff index in basis points:
index_bps = baseline_bps + Σ (weight_bps * addon_bps / 10_000)

- baseline_bps = global baseline tariff

- addon_bps = per-country adjustment (signed i16, can be negative)

- weight_bps = basket weight (u16, typically totals 10,000 across enabled entries)

- only counts entries where both addon + weight exist for same country and are enabled

- clamps to [0, 50,000] bps (0% to 500%)

## ### ***📈 Perp Mark Price (Q64.64)***

Market uses **Q64.64 fixed-point** for prices (`i128`) and a **constant-product vAMM**:

- `mark_price = quote_reserve / base_reserve` (**Q64.64**)
- reserves update while maintaining:  
  `k = base_reserve * quote_reserve` *(integer rounding applies)*

---

## ### ***💵 Collateral + Positions***

- collateral tracked as `u64` **micro-USDC** *(6 decimals)*
- positions tracked as `i128` **micro-base** using:  
  `BASE_Q = 1_000_000` *(1e6)*
- entry & execution prices stored in **Q64.64** (`i128`)

---

## ### ***⏱️ Funding (period-based)***

Funding updates in **discrete periods** *(default hourly; configurable)*:

- compare **mark** vs **index**
- compute funding rate *(capped per period: **±50 bps / period**)*
- update `funding_index`
- users settle funding into collateral on interaction:
  - trade / withdraw / liquidate / close

---

## ### ***🧯 Liquidation (partial)***

Liquidation closes only a fraction per call:

- `LIQUIDATION_FRACTION_BPS = 2_000` → **20%**
- liquidation fee charged only on **closed notional**
- if still under maintenance after partial close → account remains **liquidatable** (repeat calls)

---

## ### ***🛰️ Pyth SOL/USD sanity guard***

Pyth is used as a **risk/sanity gate**, **not** as the tariff index.

Blocks risk operations if:

- feed is stale (`get_price_no_older_than`)
- price is non-positive
- relative confidence too wide:

  `conf_ratio_bps = conf * 10_000 / abs(price)`

  require `conf_ratio_bps <= 100` (**1%**)

  ---

  ## ### ***🏗️ Architecture***

## ### ***🧷 PDAs***

| PDA | Seeds | Purpose |
|---|---|---|
| `TariffOracle` | `["oracle", admin]` | Admin-controlled tariff oracle |
| `TariffPerpMarket` | `["market", oracle, usdc_mint]` | Market state + config |
| `vault_authority` | `["vault_auth", market]` | SPL token authority for collateral vault |
| `insurance_authority` | `["insurance", market]` | SPL token authority for insurance vault |
| `MarginAccount` | `["margin", market, owner]` | User margin + position state |

---

## ### ***🏦 Token Accounts (ATAs)***

### 💰 Vault USDC ATA *(owned by `vault_authority` PDA)*
- Holds user deposits  
- Used for withdrawals + liquidator payouts

### 🛡️ Insurance USDC ATA *(owned by `insurance_authority` PDA)*
- Receives fee routing  
- Covers bad debt *(capped per liquidation)*

---

## ### ***🧱 Data Structures***

## ### ***🧾 `TariffOracle`***

**Fields**
- `admin: Pubkey`
- `baseline_tariff_bps: u16`
- `confidence_bps: u16`
- `valid_until_ts: i64`
- `last_updated_ts: i64`

**Guardrails**
- `min_update_interval_secs: i64`
- `max_jump_bps_per_update: u16`
- `last_baseline_bps: u16`

**Fixed arrays**
- `country_addons: [CountryAddon; 16]` + `addon_len`
- `basket_weights: [BasketWeight; 16]` + `weight_len`

---

## ### ***📊 `TariffPerpMarket`***

**Core**
- `admin, oracle, usdc_mint`
- vault + insurance authority bumps
- `vault_usdc, insurance_vault_usdc`
- `pyth_sol_usd_feed, last_pyth_publish_time`

**vAMM**
- `base_reserve, quote_reserve, invariant_k`

**Funding**
- `funding_index, last_funding_ts, funding_period_secs`

**Fees + margins**
- `initial_margin_bps, maintenance_margin_bps`
- `trade_fee_bps, liquidation_fee_bps`

**Insurance**
- `max_insurance_payout_per_liq_usdc`
- `fee_to_insurance_bps`

**vAMM guardrails**
- `min_trade_base`
- `max_price_impact_bps`
- `spread_bps`

**Switches**
- `reduce_only, paused`

**Market tracking**
- `open_interest_base, net_position_base`

**Risk limits**
- `max_open_interest_base, max_skew_base`

---

## ### ***👤 `MarginAccount`***

- `owner, market`
- `collateral_usdc: u64`
- `position_base: i128`
- `entry_price_q64: i128`
- `last_funding_index: i128`
- `realized_pnl_usdc: i64`
- `publish_time` goes backwards *(monotonic guard stored in market)*


  ---

  ## ### ***🧰 Instruction Set***

## ### ***🧾 Oracle instructions***
- `initialize_oracle(baseline_bps, confidence_bps, valid_secs)`
- `oracle_set_guardrails(min_update_interval_secs, max_jump_bps_per_update)`
- `oracle_set_baseline(new_baseline_bps, valid_secs)`
- `oracle_upsert_country_addon(country_code, addon_bps, enabled)`
- `oracle_upsert_basket_weight(country_code, weight_bps, enabled)`

---

## ### ***🛠️ Market/admin instructions***
- `initialize_market(base_reserve, quote_reserve, margin/fee params, risk limits, pyth_sol_usd_feed)`
- `set_reduce_only(reduce_only)`
- `set_paused(paused)`
- `set_market_config(max_insurance_payout_per_liq_usdc, fee_to_insurance_bps, funding_period_secs, min_trade_base, max_price_impact_bps, spread_bps)`

---

## ### ***💳 User instructions***
- `initialize_margin()`
- `deposit_usdc(amount)` ✅ allowed while paused
- `withdraw_usdc(amount)` ❌ blocked while paused
- `open_position(side, base_amount)` ❌ blocked while paused
- `close_position()` ✅ allowed while paused / reduce-only
- `apply_funding()` ❌ blocked while paused
- `liquidate()` ❌ blocked while paused

---

## ### ***🧪 Events (for demos + debugging)***
- `OracleUpdateEvent`
- `DepositEvent`
- `WithdrawEvent`
- `TradeEvent`
- `FundingAppliedEvent`
- `LiquidationEvent`
- `BadDebtEvent`

---

## ### ***🧮 Units & Math***
- **Collateral:** micro-USDC (`u64`, 6 decimals)
- **Position base:** micro-base (`i128`, `BASE_Q = 1e6`)
- **Prices:** Q64.64 (`i128`)
- **Tariffs:** BPS (`u16` / `i16`, 10,000 bps = 100%)
- ✅ No floats, checked integer math everywhere

  ---


## ### ***✅ What this POC is (and isn’t)***

## ### ***✅ It *is*** ✅

A **clean, research-grade demo** showing how you can combine a **policy-driven index** (tariff regime data) with a **perpetual futures market** on Solana using Anchor — all in a way that’s testable, deterministic, and easy to extend.

### ✅ Specifically, it demonstrates:

- **🧾 Oracle-driven “policy index”**
  - An admin-updated TariffOracle with:
    - baseline tariff (bps)
    - per-country add-ons (signed bps, can be negative)
    - basket weights (trade-weight style weighting)
    - validity windows (staleness protection)
    - update guardrails (anti-spam + anti-jump)
  - A deterministic on-chain formula that produces a single **Tariff Index** number you can reference in markets.
 
    - **📈 vAMM-based perpetuals**
  - A simple, transparent **constant-product virtual AMM** used to price and execute perps.
  - Mark price derived from reserves and updated by trades.
  - Price impact guardrails and spread controls to reduce obvious manipulation.

- **💵 Real token collateral (USDC vault)**
  - Users deposit real SPL token collateral (USDC-like mint) into a PDA-owned vault.
  - Withdrawals are guarded by margin requirements (can’t withdraw if it would break safety).

- **⏱️ Funding + margin mechanics**
  - Funding accrues **periodically** (discrete cadence) to push mark toward index.
  - Funding settles into user collateral on interaction (trade / withdraw / close / liquidate).
  - Initial + maintenance margin checks enforce basic risk safety.
     
- **🧯 Partial liquidation behavior**
  - Liquidations close a fraction per call (instead of nuking the whole position).
  - Fees are charged on **closed notional**, not total notional.
  - Repeat liquidations are possible if the account remains unhealthy.

- **🛡️ Insurance-style backstop (capped)**
  - Fee routing to an insurance vault.
  - Bad debt coverage attempts are capped per liquidation (circuit breaker).
  - Emits events for visibility (trade/funding/liquidation/bad-debt).

- **🛰️ Pyth SOL/USD sanity gating**
  - Uses Pyth as a safety gate to block risk-moving actions if price quality is bad:
    - stale feed
    - non-positive price
    - confidence ratio too wide
    - non-monotonic publish time
  - Pyth is **not** the tariff index itself — it’s a guardrail.

---

## ### ***❌ It *isn’t (yet)*** ❌

A full production perpetual DEX. This POC intentionally avoids major components that production perps require.

### ❌ Missing / simplified components:

- **📚 No orderbook / CLOB**
  - No on-chain matching engine.
  - No maker/taker queue, no limit order depth, no cancel/replace logic.

- **🔨 No auction-based liquidation engine**
  - No liquidation auctions, no partial fill matching, no Dutch auction, no keeper competition mechanisms.
  - Liquidation is direct and simplified.

- **🧠 No multi-collateral cross-margin risk engine**
  - No portfolio margin across multiple markets.
  - No correlated risk offsets, no haircuts per asset class, no dynamic risk weights.

- **🔗 No decentralized tariff publisher**
  - The tariff oracle is admin-updated for POC speed and clarity.
  - No multisig governance, threshold signatures, decentralized feeders, or on-chain verification of sources.

- **🧾 No full exchange accounting stack**
  - No fee tiers, rebates, referral accounting, or detailed trading ledger system.
  - No insurance fund policies like ADL (auto-deleveraging) or socialized loss mechanisms beyond the simple cap.

---




