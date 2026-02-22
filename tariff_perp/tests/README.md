# 🧪 Tariff Perp – Playground Test (anchor.test.ts)

This test file validates the **Tariff Oracle + Tariff Perp Market** proof-of-concept inside **Solana Playground** using Anchor.

It is intentionally designed to be:

- ✅ Playground-friendly (no `chai`, uses Node `assert`)
- ♻️ Idempotent (safe to re-run without PDA init failures)
- 🛰️ Pyth-aware (skips risk tests if SOL/USD feed is stale)

---

## 🔧 What This Test Covers

### 1️⃣ Setup (before hook)
- 💸 Funds `user` and `liquidator` from the admin wallet (no flaky airdrops).
- 🪙 Creates a fresh 6-decimal USDC mint.
- 👛 Creates ATAs for user + liquidator.
- 🧾 Mints:
  - 5000 USDC to user
  - 1000 USDC to liquidator

---

### 2️⃣ Oracle Initialization (Safe + Idempotent)
- Derives Oracle PDA: `["oracle", admin]`
- If it already exists → does NOT reinitialize (prevents `custom program error: 0x0`)
- Applies:
  - Baseline tariff
  - Country addon (US example)
  - Basket weight
- Confirms admin matches expected wallet

---

### 3️⃣ Market + Margin Initialization
- Derives PDAs:
  - Market
  - Vault authority
  - Insurance authority
  - Margin account
- Calls `initializeMarket(...)`
- Creates user margin account
- Verifies margin owner

---

### 4️⃣ Deposit + Open Position (Pyth-Guarded)
- Deposits USDC into vault.
- Optionally sets test market config.
- Attempts `openPosition(...)`.

🛰️ Uses required **devnet SOL/USD Pyth feed**.

If Pyth is stale/unavailable:
- ❄️ Test logs message
- ⏭ Risk-moving tests are skipped
- ✅ Suite does not fail

---

### 5️⃣ Funding + Admin Toggles + Close
(Only runs if Pyth usable)

- Applies funding
- Enables `reduce_only`
- Enables `paused`
- Closes position
- Confirms position is zero
- Restores market flags

---

### 6️⃣ Optional Liquidation
(Only runs if Pyth usable)

- Opens position
- Liquidator attempts liquidation
- Either:
  - Position reduced (liquidated), or
  - Account healthy → "Not liquidatable — OK"

Both outcomes are valid.

---

## 🛰️ Why Some Tests Skip

Solana Playground clusters often lack fresh Pyth updates.

Since this program enforces:
- Staleness bounds
- Confidence checks
- Price validity

The test detects Pyth guard errors and skips risk tests instead of failing.

---

---
