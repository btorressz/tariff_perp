# 🧪 Tariff Perp — Playground Tests (`anchor.test.ts`)

This test suite validates the **Tariff Index Oracle + Tariff Perp Market** proof-of-concept in **Solana Playground (Anchor)**.

It is designed to be:

- ✅ Playground-friendly (no `chai`)
- ♻️ Idempotent (safe to run multiple times)
- 🛰️ Pyth-aware (skips risk tests if Pyth is stale/unavailable)

---
