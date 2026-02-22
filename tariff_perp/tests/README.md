# 🧪 Tariff Perp — Playground Tests (`anchor.test.ts`)

This test suite validates the **Tariff Index Oracle + Tariff Perp Market** proof-of-concept in **Solana Playground (Anchor)**.

It is designed to be:

- ✅ Playground-friendly (no `chai`)
- ♻️ Idempotent (safe to run multiple times)
- 🛰️ Pyth-aware (skips risk tests if Pyth is stale/unavailable)

---

## 🎯 High-Level Goals

The test file verifies the full lifecycle of your protocol:

- 🧠 **Oracle** — initialize + update tariff parameters  
- 🏦 **Perp Market** — initialize vAMM + vaults + insurance vault  
- 👤 **Margin Account** — create per-user account  
- 💰 **Collateral Flow** — deposit USDC  
- 📈 **Trading / Funding / Liquidation** — risk-moving logic (when Pyth is usable)

---

## 🧩 Test Breakdown

---

### 1️⃣ 🔧 `before()` — Setup Phase

This runs once before tests begin.

**What it does:**

- 💸 Transfers SOL from admin → `user` + `liquidator`
  - (Avoids flaky `requestAirdrop`)
- 🪙 Creates a fresh **6-decimal USDC mint**
- 👛 Creates Associated Token Accounts (ATAs)
- 🧾 Mints:
  - ✅ 5000 USDC to user
  - ✅ 1000 USDC to liquidator

This guarantees a clean token environment each test run.

### 2️⃣ 🏛️ Initialize Oracle (Idempotent)

Derives the Oracle PDA:

["oracle", admin]


#### 🛑 Important Fix
If the oracle already exists (from a previous Playground run), the test **does not reinitialize it**.

This prevents the classic error: custom program error: 0x0


---
