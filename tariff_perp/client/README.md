# 🚀 Tariff Perp – Playground Client (client.ts)

This `client.ts` file is a **Playground-friendly demo script** for your  
**Tariff Index Oracle + Tariff Perp** proof-of-concept.

It walks through the full protocol lifecycle from scratch using only:

- ✅ `pg.wallet`
- ✅ `pg.connection`
- ✅ `pg.program`

No `pg.wallet.payer` dependency. No external setup required.

---

## 🧠 What This Script Does

### 1️⃣ Setup
- Prints your Playground wallet address + SOL balance.
- Creates:
  - 👤 A test `user`
  - 🪙 A temporary `splPayer` (used to create mint + ATAs)
- Funds both keypairs from your Playground wallet.

---

### 2️⃣ Create USDC (Test Token)
- 🪙 Creates a 6-decimal USDC-like mint.
- 👛 Creates the user’s USDC ATA.
- 💰 Mints 5000 USDC to the user.

This gives the user real SPL collateral to trade with.

---

### 3️⃣ Initialize Tariff Oracle
- Derives Oracle PDA.
- Initializes it if not already created.
- Sets:
  - 🔒 Guardrails
  - 📊 Baseline tariff (1100 bps)
  - 🇺🇸 Country addon + basket weight

Idempotent — safe to re-run.

---

### 4️⃣ Initialize Market
- Derives:
  - 🏦 Market PDA
  - 🔐 Vault authority PDA
  - 🛡 Insurance authority PDA
- Calls `initializeMarket(...)`
- Applies `setMarketConfig(...)`

The market references:

🛰️ **Devnet SOL/USD Pyth feed** : J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix


---

### 5️⃣ Initialize Margin Account
- Derives user margin PDA.
- Creates it if missing.

Each user gets a separate on-chain margin account.

---

### 6️⃣ Deposit USDC
- Deposits 1000 USDC into the market vault.
- Collateral now tracked on-chain.

---

### 7️⃣ Best-Effort Trading (Pyth-Guarded)
Attempts:

- 📈 `openPosition`
- ⏱ `applyFunding`
- 🔒 `closePosition`

If the Playground cluster has stale Pyth data:

- ❄️ Operations are blocked by sanity guards
- 🟡 Script logs the message
- ✅ Script continues without failing

---

## 🛰️ Why Trading May Be Blocked

Your program enforces Pyth sanity:

- ⏳ Staleness checks
- 📏 Confidence checks
- 📉 Price validity

Playground clusters often lack fresh devnet Pyth updates, so  
risk-moving instructions may be rejected — this is expected.

---

## ✅ What This Client Demonstrates

- Oracle initialization
- Market creation
- Vault + insurance wiring
- Margin account lifecycle
- USDC collateral flow
- vAMM trade flow (when Pyth allows)

---

## 🎯 Purpose

This is a **single-file, reproducible POC demo script** that:

- Works inside Solana Playground
- Requires no external infra
- Is safe to re-run
- Fully exercises your Tariff Perp architecture

---

**Tariff Oracle + Perp Market POC running end-to-end in Playground. 🚀**
