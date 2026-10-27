# JetForge — on-chain program

The Solana program behind **[JetForge](https://jetforge.io)**, a fair-launch token launchpad:
launch an SPL token in one transaction, trade it on a bonding curve, and graduate it to
Raydium automatically.

This repository publishes the program's source so anyone can read exactly what the
deployed program does. It contains only the on-chain program; the website and backend
are not part of it.

| | |
|---|---|
| **Program (mainnet)** | [`JetvDoU56t5vU6vo4RCkYmFUCatnQpLeXNWsuzxA4Cj`](https://solscan.io/account/JetvDoU56t5vU6vo4RCkYmFUCatnQpLeXNWsuzxA4Cj) |
| **Upgrade authority** | [`6KUisCkQ7kC5ijJgRgahUjqPtnZoiFPrvgF1ouS621PE`](https://solscan.io/account/6KUisCkQ7kC5ijJgRgahUjqPtnZoiFPrvgF1ouS621PE) — a 2-of-3 Squads multisig |
| **Platform treasury** | [`FpRb8wNaSg1zcezV3gm7L2rFAv8mNw7HH6Vhuauxkd6K`](https://solscan.io/account/FpRb8wNaSg1zcezV3gm7L2rFAv8mNw7HH6Vhuauxkd6K) |
| **Framework** | Anchor 0.30.1 (Rust) |
| **Website / API** | [jetforge.io](https://jetforge.io) · [API docs](https://jetforge.io/docs/api) |

## How it works

**Launch.** `create_token` creates the mint, its Metaplex metadata and its bonding curve in
one transaction. Mint authority and freeze authority are revoked and the metadata is
immutable, so supply can never grow, holders can never be frozen, and the name, symbol and
image can never change. There is no presale and no free team allocation: every token,
including any the creator buys, comes from the bonding curve at the same price as everyone
else's. (The jetforge.io launch form lets the creator add a first buy of up to 5% of the
supply in the same transaction; that limit is set by the website, not by the program.)

**Supply.** 1,000,000,000 tokens (6 decimals): 700,000,000 are sold on the bonding curve,
300,000,000 are reserved for the Raydium pool.

**Bonding curve.** Constant product (`x · y = k`) over virtual reserves that start at
30 SOL and 947,058,823.529412 tokens. Everyone buys and sells against the same formula.

**Fees.** 1% on every buy and sell, split on-chain in the same instruction:

| Recipient | Share |
|---|---|
| Token creator (per-token vault, withdrawable any time via `withdraw_creator_fees`) | 40% |
| Platform treasury | 40% |
| Per-token buyback-and-burn vault | 20% |

**Buyback and burn.** `execute_buyback` is permissionless once a token's buyback vault holds
0.1 SOL: it buys the token back from the curve and burns it. Keeping each burn small makes
it unprofitable to sandwich.

**Graduation.** When the curve has raised 85 SOL, `graduate_to_raydium` migrates the token
to a Raydium CPMM pool in **a single transaction**:

- 90% of the raised SOL (plus any leftover buyback SOL) and the matching tokens seed the
  pool, sized so it opens at the curve's final price; excess reserve tokens are burned;
- 5% of the raised SOL goes to the creator and 5% to the treasury;
- the LP tokens are burned in the same transaction, so the liquidity is locked forever;
- the pool address is a program-derived address, so nobody can front-run the canonical pool.

Graduation funds never pass through a wallet with a private key: the transaction either
completes entirely or does nothing.

`graduate` is a legacy fallback, callable only by the multisig above.

## Instructions

| Instruction | Who can call it |
|---|---|
| `create_token` | anyone |
| `buy` / `sell` | anyone |
| `graduate_to_raydium` | anyone, once the curve is complete |
| `execute_buyback` | anyone, once the buyback vault holds ≥ 0.1 SOL |
| `withdraw_creator_fees` | the token's creator |
| `graduate` (legacy fallback) | the 2-of-3 multisig only |

## Building

The mainnet program is built with no cargo features:

```bash
cargo-build-sbf --manifest-path programs/token-launch/Cargo.toml
```

Toolchain used for the deployed build: Solana CLI / `cargo-build-sbf` 3.1.11 (Anza), stable Rust.

Cargo features select test builds and change the program ID, treasury and Raydium program:

| Feature | Use |
|---|---|
| *(none)* | mainnet |
| `devnet` | devnet build (devnet program ID, treasury and Raydium CPMM) |
| `e2e` | local end-to-end tests only |
| `devnet-low-threshold` | devnet only: graduation at 3 SOL (refuses to compile without `devnet`) |

## Verifying the deployed program

The deployed program at `JetvDoU56t5vU6vo4RCkYmFUCatnQpLeXNWsuzxA4Cj` is 465,352 bytes with
SHA-256 `764acd9e6709a9522520c756174f22e31c3787c52918343d9fe597eba5aa23ad`
(the on-chain account is padded with zeros up to 575,000 bytes for future upgrades).

```bash
solana program dump JetvDoU56t5vU6vo4RCkYmFUCatnQpLeXNWsuzxA4Cj onchain.so --url mainnet-beta
head -c 465352 onchain.so | sha256sum
```

## Security

Please report vulnerabilities privately; see [SECURITY.md](SECURITY.md).

The program has not been formally audited by a third party. Token trading is risky:
only trade what you can afford to lose.

## License

Copyright © 2026 JetForge. All rights reserved. The source is published so the deployed
program can be read and verified; see [LICENSE](LICENSE). It is not open source.

---

[jetforge.io](https://jetforge.io) · [X @jetforgeDev](https://x.com/jetforgeDev) · [API docs](https://jetforge.io/docs/api)
