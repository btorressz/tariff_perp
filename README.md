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


