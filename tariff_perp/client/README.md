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

🛰️ **Devnet SOL/USD Pyth feed**
