use anchor_lang::prelude::*;

pub mod errors;
pub mod instructions;
pub mod raydium;
pub mod state;

use instructions::*;

// Devnet/test builds keep the devnet program ID; the mainnet build (no
// features) is deployed at its own address, ground with a "Jet" prefix.
#[cfg(any(feature = "devnet", feature = "e2e", test))]
declare_id!("7rXDkm484DDp2YoPkLBBLtGMzuwrxysFGUgPUc4EpDmk");
#[cfg(not(any(feature = "devnet", feature = "e2e", test)))]
declare_id!("JetvDoU56t5vU6vo4RCkYmFUCatnQpLeXNWsuzxA4Cj");

/// Treasury pubkey — used in account constraints for buy/sell/graduate.
/// Hardcoded so the on-chain constraint cannot be bypassed by an untrusted caller.
/// Devnet/test builds use the devnet treasury, whose key is committed in the repo.
#[cfg(any(feature = "devnet", feature = "e2e", test))]
pub const TREASURY_PUBKEY: Pubkey = pubkey!("13DWuEycYuJvGpo2EwPMgaiBDfRKmpoxdXjJ5GKe9RPW");
/// Mainnet treasury: generated on the server for mainnet (its key never left it).
#[cfg(not(any(feature = "devnet", feature = "e2e", test)))]
pub const TREASURY_PUBKEY: Pubkey = pubkey!("FpRb8wNaSg1zcezV3gm7L2rFAv8mNw7HH6Vhuauxkd6K");

/// The only signer allowed to run the legacy `graduate` (pool created off-chain
/// from the treasury). It is the emergency fallback for `graduate_to_raydium`,
/// so on mainnet this MUST be the Squads multisig vault (2-of-3), never a server key.
/// JetForge's Squads vault (2-of-3), verified on mainnet as a system-owned,
/// data-free vault account. On devnet nobody can sign for it, so the fallback
/// stays disabled there.
#[cfg(not(feature = "e2e"))]
pub const GRADUATION_ADMIN: Pubkey = pubkey!("6KUisCkQ7kC5ijJgRgahUjqPtnZoiFPrvgF1ouS621PE");
#[cfg(feature = "e2e")]
pub const GRADUATION_ADMIN: Pubkey = pubkey!("GdKEwQStpJiJWX4CktBhZHkd43FshY82YoBHM3TmNmZB");

#[program]
pub mod token_launch {
    use super::*;

    /// Create a new token with bonding curve
    /// - Initializes mint with 6 decimals
    /// - Creates BondingCurveState PDA
    /// - Creates token vault and reserve vault
    /// - Mints trading and reserve supply
    /// - Creates Metaplex on-chain metadata
    pub fn create_token(
        ctx: Context<CreateToken>,
        name: String,
        symbol: String,
        uri: String,
    ) -> Result<()> {
        instructions::create_token::create_token(ctx, name, symbol, uri)
    }

    /// Buy tokens from the bonding curve
    /// - sol_amount: lamports to spend (including fee)
    /// - min_tokens_out: minimum tokens to receive (slippage protection)
    pub fn buy(
        ctx: Context<Buy>,
        sol_amount: u64,
        min_tokens_out: u64,
    ) -> Result<()> {
        instructions::buy::buy(ctx, sol_amount, min_tokens_out)
    }

    /// Sell tokens back to the bonding curve
    /// - token_amount: number of tokens to sell (with 6 decimals)
    /// - min_sol_out: minimum SOL to receive (slippage protection)
    pub fn sell(
        ctx: Context<Sell>,
        token_amount: u64,
        min_sol_out: u64,
    ) -> Result<()> {
        instructions::sell::sell(ctx, token_amount, min_sol_out)
    }

    /// Graduate a completed token: create the Raydium pool and burn the LP
    /// tokens in one instruction. Permissionless; nothing passes through a
    /// wallet with a private key.
    pub fn graduate_to_raydium(ctx: Context<GraduateToRaydium>) -> Result<()> {
        instructions::graduate_to_raydium::graduate_to_raydium(ctx)
    }

    /// Emergency fallback: graduate by sending pool SOL + tokens to the treasury,
    /// which creates the pool off-chain. Only GRADUATION_ADMIN (Squads) may call it.
    pub fn graduate(ctx: Context<Graduate>) -> Result<()> {
        instructions::graduate::graduate(ctx)
    }

    /// Execute buyback-and-burn using accumulated fee SOL
    /// - Permissionless: anyone can call when buyback_vault >= BUYBACK_THRESHOLD
    /// - Uses accumulated 20% fee SOL to buy tokens from curve, then burns them
    /// - Deflationary: reduces supply and pushes price up
    pub fn execute_buyback(ctx: Context<ExecuteBuyback>) -> Result<()> {
        instructions::execute_buyback::execute_buyback(ctx)
    }

    /// Withdraw accumulated creator fees from the creator vault PDA
    /// - Only callable by the token's creator
    /// - Transfers all available SOL from creator_vault to creator wallet
    pub fn withdraw_creator_fees(ctx: Context<WithdrawCreatorFees>) -> Result<()> {
        instructions::withdraw_creator_fees::withdraw_creator_fees(ctx)
    }
}
