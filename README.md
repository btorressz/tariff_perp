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

