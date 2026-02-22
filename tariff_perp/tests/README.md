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
