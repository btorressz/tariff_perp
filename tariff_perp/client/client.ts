// 
// client.ts (Solana Playground) — Tariff Index Oracle + Tariff Perp POC
//
// FIXES:
// - No pg.wallet.payer dependency (Playground sometimes doesn’t expose it).
// - Uses ONLY `pg.connection`, `pg.wallet`, and `pg.program`.
// - Still creates a USDC-like mint + ATAs, initializes oracle/market/margin, deposits,
//   and then *best-effort* tries open/apply/close (may be blocked by Pyth guard).
//
// NOTE:
// - This uses `@solana/spl-token` helpers that accept a `Signer` payer. In Playground,
//   the wallet adapter can’t be used as a Signer. So we create a temporary `Keypair` payer,
//   fund it from your Playground wallet, and use it to create the mint/ATAs + mintTo.

import { BN } from "@coral-xyz/anchor";
import {
  PublicKey,
  SystemProgram,
  Keypair,
  Transaction,
  LAMPORTS_PER_SOL,
} from "@solana/web3.js";
import {
  createMint,
  getOrCreateAssociatedTokenAccount,
  mintTo,
  TOKEN_PROGRAM_ID,
  getAssociatedTokenAddressSync,
  ASSOCIATED_TOKEN_PROGRAM_ID,
} from "@solana/spl-token";

// -------------------------
// Basics
// -------------------------
console.log("My address:", pg.wallet.publicKey.toString());
const bal = await pg.connection.getBalance(pg.wallet.publicKey);
console.log(`My balance: ${bal / LAMPORTS_PER_SOL} SOL`);

const program = pg.program;
const provider = program.provider;

// Keep devnet SOL/USD Pyth feed pubkey (your requirement)
const PYTH_SOL_USD_DEVNET = new PublicKey(
  "J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix"
);

// -------------------------
// Helpers
// -------------------------
async function accountExists(pk: PublicKey): Promise<boolean> {
  const info = await pg.connection.getAccountInfo(pk);
  return info !== null;
}

function isPythGuardError(e: any): boolean {
  const msg = `${e?.message ?? e}`.toLowerCase();
  const code = e?.error?.errorCode?.code;
  return (
    code === "PythStale" ||
    code === "PythLoadFailed" ||
    code === "PythNoPrice" ||
    code === "PythBadPrice" ||
    code === "PythConfidenceTooWide" ||
    code === "PythNonMonotonic" ||
    msg.includes("pyth")
  );
}

async function fundKeypair(kp: Keypair, sol: number) {
  const lamports = Math.floor(sol * LAMPORTS_PER_SOL);
  const tx = new Transaction().add(
    SystemProgram.transfer({
      fromPubkey: pg.wallet.publicKey,
      toPubkey: kp.publicKey,
      lamports,
    })
  );
  await provider.sendAndConfirm(tx, [], { commitment: "confirmed" });
}

// -------------------------
// PDA helpers (must match lib.rs)
// -------------------------
const oraclePda = (adminPk: PublicKey) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("oracle"), adminPk.toBuffer()],
    program.programId
  )[0];

const marketPda = (oracle: PublicKey, usdcMint: PublicKey) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("market"), oracle.toBuffer(), usdcMint.toBuffer()],
    program.programId
  )[0];

const vaultAuthPda = (market: PublicKey) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("vault_auth"), market.toBuffer()],
    program.programId
  )[0];

const insuranceAuthPda = (market: PublicKey) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("insurance"), market.toBuffer()],
    program.programId
  )[0];

const marginPda = (market: PublicKey, owner: PublicKey) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("margin"), market.toBuffer(), owner.toBuffer()],
    program.programId
  )[0];

// -------------------------
// Config (POC)
// -------------------------
const user = Keypair.generate();
const splPayer = Keypair.generate(); // pays for mint/ATA creation + is mint authority

const US: number[] = ["U".charCodeAt(0), "S".charCodeAt(0)];

const BASE_RESERVE = new BN("1000000000000");
const QUOTE_RESERVE = new BN("1000000000000");

const DEPOSIT_USDC = new BN(1_000_000_000); // 1000 USDC (micro-USDC)
const OPEN_BASE = new BN(20_000_000); // 20 base (BASE_Q=1e6)

// -------------------------
// Step 0: fund keypairs (user + splPayer)
// -------------------------
await fundKeypair(user, 0.5);
console.log("Funded user:", user.publicKey.toString());

await fundKeypair(splPayer, 0.5);
console.log("Funded splPayer:", splPayer.publicKey.toString());

// -------------------------
// Step 1: Create USDC mint + user ATA, mint USDC to user
// -------------------------
const usdcMint = await createMint(
  pg.connection,
  splPayer,              // payer for tx fees
  splPayer.publicKey,    // mint authority
  null,                  // freeze authority
  6                      // decimals
);
console.log("USDC mint:", usdcMint.toString());

const userUsdcAta = (
  await getOrCreateAssociatedTokenAccount(
    pg.connection,
    splPayer,
    usdcMint,
    user.publicKey
  )
).address;

await mintTo(
  pg.connection,
  splPayer,
  usdcMint,
  userUsdcAta,
  splPayer.publicKey, // authority must match mint authority
  5_000_000_000       // 5000 USDC
);
console.log("Minted 5000 USDC to user ATA:", userUsdcAta.toString());

// -------------------------
// Step 2: Oracle init + guardrails + baseline + upserts
// -------------------------
const adminPk = pg.wallet.publicKey;
const oracle = oraclePda(adminPk);

if (!(await accountExists(oracle))) {
  await program.methods
    .initializeOracle(1000, 100, new BN(3600))
    .accounts({
      admin: adminPk,
      oracle,
      systemProgram: SystemProgram.programId,
    })
    .rpc();
  console.log("Initialized oracle:", oracle.toString());
} else {
  console.log("Oracle already exists:", oracle.toString());
}

// These exist in your updated lib.rs:
await program.methods
  .oracleSetGuardrails(new BN(0), 5000)
  .accounts({ admin: adminPk, oracle })
  .rpc();
console.log("Set oracle guardrails");

await program.methods
  .oracleSetBaseline(1100, new BN(3600))
  .accounts({ admin: adminPk, oracle })
  .rpc();
console.log("Set oracle baseline to 1100 bps");

await program.methods
  .oracleUpsertCountryAddon(US, -50, true)
  .accounts({ admin: adminPk, oracle })
  .rpc();
await program.methods
  .oracleUpsertBasketWeight(US, 5000, true)
  .accounts({ admin: adminPk, oracle })
  .rpc();
console.log("Upserted US addon/weight");

// -------------------------
// Step 3: Market init (creates vault ATAs via init_if_needed)
// -------------------------
const market = marketPda(oracle, usdcMint);
const vaultAuth = vaultAuthPda(market);
const insuranceAuth = insuranceAuthPda(market);

const vaultUsdc = getAssociatedTokenAddressSync(usdcMint, vaultAuth, true);
const insuranceVaultUsdc = getAssociatedTokenAddressSync(usdcMint, insuranceAuth, true);

if (!(await accountExists(market))) {
  await program.methods
    .initializeMarket(
      BASE_RESERVE,
      QUOTE_RESERVE,
      1000, // initial margin 10%
      500,  // maintenance 5%
      10,   // trade fee 0.10%
      50,   // liq fee 0.50%
      new BN("5000000000000"),
      new BN("2000000000000"),
      PYTH_SOL_USD_DEVNET
    )
    .accounts({
      admin: adminPk,
      oracle,
      usdcMint,
      market,
      vaultAuthority: vaultAuth,
      insuranceAuthority: insuranceAuth,
      vaultUsdc,
      insuranceVaultUsdc,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
      rent: web3.SYSVAR_RENT_PUBKEY,
    })
    .rpc();
  console.log("Initialized market:", market.toString());
} else {
  console.log("Market already exists:", market.toString());
}

// Updated lib.rs has set_market_config:
await program.methods
  .setMarketConfig(
    new BN(200_000_000), // max_insurance_payout_per_liq_usdc
    10_000,              // fee_to_insurance_bps (100%)
    new BN(3600),        // funding_period_secs
    new BN(10_000_000),  // min_trade_base (10 base)
    500,                 // max_price_impact_bps (5%)
    10                   // spread_bps (0.10%)
  )
  .accounts({ admin: adminPk, market })
  .rpc();
console.log("Set market config");

// -------------------------
// Step 4: Margin init (user)
// -------------------------
const userMargin = marginPda(market, user.publicKey);

if (!(await accountExists(userMargin))) {
  await program.methods
    .initializeMargin()
    .accounts({
      owner: user.publicKey,
      market,
      margin: userMargin,
      systemProgram: SystemProgram.programId,
    })
    .signers([user])
    .rpc();
  console.log("Initialized margin:", userMargin.toString());
} else {
  console.log("Margin already exists:", userMargin.toString());
}

// -------------------------
// Step 5: Deposit
// -------------------------
await program.methods
  .depositUsdc(DEPOSIT_USDC)
  .accounts({
    owner: user.publicKey,
    market,
    margin: userMargin,
    userUsdc: userUsdcAta,
    vaultAuthority: vaultAuth,
    vaultUsdc,
    tokenProgram: TOKEN_PROGRAM_ID,
  })
  .signers([user])
  .rpc();
console.log("Deposited USDC:", DEPOSIT_USDC.toString());

// -------------------------
// Step 6: Best-effort open/apply/close (Pyth may block on Playground cluster)
// -------------------------
try {
  await program.methods
    .openPosition(0, OPEN_BASE)
    .accounts({
      owner: user.publicKey,
      oracle,
      market,
      margin: userMargin,
      pythFeed: PYTH_SOL_USD_DEVNET,
      vaultAuthority: vaultAuth,
      insuranceAuthority: insuranceAuth,
      vaultUsdc,
      insuranceVaultUsdc,
      tokenProgram: TOKEN_PROGRAM_ID,
    })
    .signers([user])
    .rpc();
  console.log("Opened long position:", OPEN_BASE.toString());
} catch (e) {
  if (isPythGuardError(e)) {
    console.log("open_position blocked by Pyth sanity guard on this cluster.");
  } else {
    throw e;
  }
}

try {
  await program.methods
    .applyFunding()
    .accounts({
      oracle,
      market,
      pythFeed: PYTH_SOL_USD_DEVNET,
    })
    .rpc();
  console.log("Applied funding");
} catch (e) {
  if (isPythGuardError(e)) {
    console.log("apply_funding blocked by Pyth sanity guard.");
  } else {
    throw e;
  }
}

try {
  await program.methods
    .closePosition()
    .accounts({
      owner: user.publicKey,
      oracle,
      market,
      margin: userMargin,
      pythFeed: PYTH_SOL_USD_DEVNET,
      vaultAuthority: vaultAuth,
      insuranceAuthority: insuranceAuth,
      vaultUsdc,
      insuranceVaultUsdc,
      tokenProgram: TOKEN_PROGRAM_ID,
    })
    .signers([user])
    .rpc();
  console.log("Closed position");
} catch (e) {
  if (isPythGuardError(e)) {
    console.log("close_position blocked by Pyth sanity guard.");
  } else {
    throw e;
  }
}

console.log("Done. finished.");
