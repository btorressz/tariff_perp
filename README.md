# tariff_perp
# 🎯 Tariff Perpetual futures Program

A sophisticated **Solana Anchor-based perpetual futures protocol** featuring dynamic tariff-based pricing, funding rates, liquidation mechanisms, and comprehensive risk management.

## 📋 Table of Contents

- [Overview](#overview)
- [Core Components](#core-components)
- [Key Features](#key-features)
- [Program Architecture](#program-architecture)
- [Account Structures](#account-structures)
- [Instructions](#instructions)
- [Error Handling](#error-handling)
- [Safety & Security](#safety--security)
- [Getting Started](#getting-started)
- [Constants & Configuration](#constants--configuration)

---

## 🌟 Overview

The **Tariff Perpetual Futures** program is a decentralized protocol for trading perpetual futures contracts on Solana. It uses a virtual Automated Market Maker (vAMM) model combined with a novel **dynamic tariff oracle system** to determine index prices and funding rates.

### Key Highlights:

✨ **Dynamic Tariff Oracle** - Country-based addons and basket weights drive index pricing  
🔐 **Collateralized Trading** - USDC-backed margin accounts with real-time monitoring  
📊 **Virtual AMM** - Constant-product formula for position management  
⚡ **Funding Rates** - Automatic funding settlements per configurable periods  
🛡️ **Risk Management** - Position limits, liquidation mechanisms, bad debt insurance  
💰 **Insurance Fund** - Fee routing and bad debt coverage system  
🔍 **Pyth Oracle Integration** - SOL/USD price feed for sanity checks and monotonic updates

---

## 🏗️ Core Components

### 1. **Tariff Oracle** 🎲
Manages dynamic tariff-based index pricing:
- **Baseline Tariff** - Base percentage rate (0-500%)
- **Country Addons** - Signed basis point adjustments per country (up to 16)
- **Basket Weights** - Weighted contributions from country baskets (up to 16)
- **Staleness Tracking** - Time-locked validity windows with update cadences

**Tariff Index Calculation:**
