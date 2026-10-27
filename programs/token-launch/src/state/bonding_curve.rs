use anchor_lang::prelude::*;
use crate::errors::TokenLaunchError;

/// Initial virtual SOL reserves (30 SOL in lamports)
pub const INITIAL_VIRTUAL_SOL: u64 = 30_000_000_000;

/// Initial virtual token reserves.
///
/// Chosen so that selling the full 700M curve supply raises EXACTLY the 85 SOL
/// graduation threshold: with 30 SOL virtual, 30·vT0 = 115·(vT0 − 700M) →
/// vT0 = 115·700M/85 ≈ 947.06M. (The previous value, 1,073,000,191, is
/// pump.fun's — it pairs with a 793.1M curve; with 700M it capped the raise at
/// 56.30 SOL, so graduation was unreachable — audit C1.)
pub const INITIAL_VIRTUAL_TOKENS: u64 = 947_058_823_529_412;

/// Total token supply (1 billion tokens with 6 decimals)
pub const TOTAL_SUPPLY: u64 = 1_000_000_000_000_000;

/// Tokens available for trading on the bonding curve (70% of total supply)
/// 700M tokens purchasable by users — unsold ones are BURNED at graduation
pub const REAL_TOKEN_RESERVES_INIT: u64 = 700_000_000_000_000;

/// Tokens locked in reserve vault for DEX liquidity at graduation (30% of total supply)
/// These 300M tokens are NEVER for sale — they ALWAYS seed the Raydium CPMM pool
pub const RESERVE_TOKEN_AMOUNT: u64 = 300_000_000_000_000;

/// Graduation threshold in lamports.
/// MAINNET-READY: 85 SOL graduation threshold.
/// Matches pump.fun economics — tokens graduate after raising 85 SOL on the bonding curve.
#[cfg(not(feature = "devnet-low-threshold"))]
pub const GRADUATION_THRESHOLD: u64 = 85_000_000_000;
/// Temporary devnet test build: graduate at 3 SOL so the keeper/indexer path can
/// be exercised end-to-end without 85 devnet SOL. Never in a mainnet build.
#[cfg(feature = "devnet-low-threshold")]
pub const GRADUATION_THRESHOLD: u64 = 3_000_000_000;
#[cfg(all(feature = "devnet-low-threshold", not(feature = "devnet")))]
compile_error!("devnet-low-threshold is a devnet-only test build: enable the `devnet` feature too");

/// Fee in basis points (1%)
pub const FEE_BPS: u64 = 100;

/// Total basis points
pub const BPS_DENOMINATOR: u64 = 10_000;

/// Creator fee share of total fee (40%)
pub const CREATOR_FEE_SHARE: u64 = 40;

/// Treasury fee share of total fee (40%)
pub const TREASURY_FEE_SHARE: u64 = 40;

/// Buyback-and-burn fee share of total fee (20%)
pub const BUYBACK_FEE_SHARE: u64 = 20;

/// Minimum SOL accumulated before a buyback-and-burn is triggered.
/// Current value: 100_000_000 lamports = 0.1 SOL.
/// NOTE: revisit before mainnet — a higher threshold (e.g. 1 SOL) reduces the
/// number of tiny, gas-inefficient buyback transactions.
pub const BUYBACK_THRESHOLD: u64 = 100_000_000;

/// Fee share denominator
pub const FEE_SHARE_DENOMINATOR: u64 = 100;

/// Treasury share of total raised SOL at graduation (5%)
pub const GRADUATION_TREASURY_BPS: u64 = 500;

/// Creator reward share of total raised SOL at graduation (5%)
pub const GRADUATION_CREATOR_BPS: u64 = 500;

/// Seeds of the per-mint migration authority: a data-free, system-owned PDA that
/// creates the Raydium pool as its "creator" (pays rent + pool fee, signs via seeds).
pub const MIGRATION_SEED: &[u8] = b"migration";

/// Seeds of the Raydium pool account created at graduation (a PDA of this program,
/// so nobody else can create a pool at that address first).
pub const POOL_STATE_SEED: &[u8] = b"raydium_pool";

/// SOL for the new pool accounts' rent (pool state, LP mint, two vaults,
/// observation, LP token account: about 0.045 SOL), on top of Raydium's
/// creation fee, which is read from its fee-tier config at graduation time.
/// Unspent SOL returns to the treasury in the same instruction.
pub const POOL_RENT_BUDGET: u64 = 100_000_000;

#[account]
pub struct BondingCurveState {
    /// The token mint address
    pub mint: Pubkey,
    /// Creator's public key
    pub creator: Pubkey,
    /// Virtual SOL reserves (includes initial 30 SOL offset)
    pub virtual_sol_reserves: u64,
    /// Virtual token reserves
    pub virtual_token_reserves: u64,
    /// Actual SOL collected from trades
    pub real_sol_reserves: u64,
    /// Actual tokens available for sale
    pub real_token_reserves: u64,
    /// Total token supply
    pub token_total_supply: u64,
    /// Whether curve has graduated to a DEX
    pub complete: bool,
    /// Unix timestamp of creation
    pub created_at: i64,
    /// Total SOL volume traded
    pub total_volume_sol: u64,
    /// Total number of trades
    pub total_trades: u64,
    /// PDA bump
    pub bump: u8,
}

/// Ceiling division for u128 (None on divide-by-zero / overflow).
pub fn ceil_div(a: u128, b: u128) -> Option<u128> {
    if b == 0 {
        return None;
    }
    a.checked_add(b - 1)?.checked_div(b)
}

impl BondingCurveState {
    /// Account discriminator (8) + fields
    pub const SPACE: usize = 8 + 32 + 32 + 8 + 8 + 8 + 8 + 8 + 1 + 8 + 8 + 8 + 1;

    /// Quote a buy of `sol_in_after_fee` (net of the 1% fee).
    ///
    /// Returns `(tokens_out, sol_used)`. Normally `sol_used == sol_in_after_fee`.
    /// If the curve can't supply that many tokens, it PARTIALLY FILLS: all
    /// remaining curve tokens are sold and `sol_used` is only the SOL needed for
    /// them — the caller charges just that (audit M1: the final buy no longer
    /// reverts, and selling out the curve triggers graduation).
    ///
    /// Rounding always favors the protocol (post-trade token reserve and the SOL
    /// needed are rounded UP), so the constant product k never decreases.
    pub fn quote_buy(&self, sol_in_after_fee: u64) -> Option<(u64, u64)> {
        let vs = self.virtual_sol_reserves as u128;
        let vt = self.virtual_token_reserves as u128;
        let k = vs.checked_mul(vt)?;

        let new_vs = vs.checked_add(sol_in_after_fee as u128)?;
        let new_vt = ceil_div(k, new_vs)?;
        let full_out = vt.checked_sub(new_vt)?;

        let remaining = self.real_token_reserves as u128;
        if full_out <= remaining {
            return Some((u64::try_from(full_out).ok()?, sol_in_after_fee));
        }

        // Partial fill: buy exactly the remaining curve tokens.
        let end_vt = vt.checked_sub(remaining)?;
        let end_vs = ceil_div(k, end_vt)?;
        let sol_needed = end_vs.checked_sub(vs)?.min(sol_in_after_fee as u128);
        Some((u64::try_from(remaining).ok()?, u64::try_from(sol_needed).ok()?))
    }

    /// Tokens out for `sol_in_after_fee`, only if the curve can fill it fully.
    pub fn get_tokens_for_sol(&self, sol_in_after_fee: u64) -> Option<u64> {
        match self.quote_buy(sol_in_after_fee)? {
            (tokens, used) if used == sol_in_after_fee => Some(tokens),
            _ => None,
        }
    }

    /// Calculate SOL out for a given token input (before fee deduction)
    /// Uses constant product formula: k = virtual_sol * virtual_token.
    /// The post-trade SOL reserve is rounded UP so the seller never receives
    /// an extra lamport (k never decreases).
    pub fn get_sol_for_tokens(&self, token_in: u64) -> Option<u64> {
        let k = (self.virtual_sol_reserves as u128)
            .checked_mul(self.virtual_token_reserves as u128)?;

        let new_virtual_token = (self.virtual_token_reserves as u128)
            .checked_add(token_in as u128)?;

        let new_virtual_sol = ceil_div(k, new_virtual_token)?;

        let sol_out = (self.virtual_sol_reserves as u128)
            .checked_sub(new_virtual_sol)?;

        // Cap at real SOL reserves — can never extract more than was deposited.
        // NOTE(audit #11): in the rare case the raw curve output exceeds real
        // reserves, the capped value is what apply_sell later subtracts from the
        // virtual reserves too, so virtual reserves diverge slightly from the
        // strict x*y=k invariant. This is a safety cap (prevents draining more
        // than deposited), not a fund-loss path; kept intentionally.
        let sol_out_capped = sol_out.min(self.real_sol_reserves as u128);

        u64::try_from(sol_out_capped).ok()
    }

    /// Current price of token in lamports per token (6 decimals)
    pub fn current_price(&self) -> Option<u64> {
        let price = (self.virtual_sol_reserves as u128)
            .checked_mul(1_000_000)? // scale by token decimals
            .checked_div(self.virtual_token_reserves as u128)?;
        u64::try_from(price).ok()
    }

    /// Market cap in lamports
    pub fn market_cap(&self) -> Option<u64> {
        let price = self.current_price()?;
        let market_cap = (price as u128)
            .checked_mul(self.token_total_supply as u128)?
            .checked_div(1_000_000)?; // divide by token decimals
        u64::try_from(market_cap).ok()
    }

    /// Apply buy: update reserves after purchase
    /// Returns error if arithmetic would overflow/underflow — amounts must be
    /// pre-validated by get_tokens_for_sol() before calling this.
    pub fn apply_buy(&mut self, sol_in_after_fee: u64, tokens_out: u64) -> Result<()> {
        self.virtual_sol_reserves = self.virtual_sol_reserves
            .checked_add(sol_in_after_fee)
            .ok_or(error!(TokenLaunchError::MathOverflow))?;
        self.virtual_token_reserves = self.virtual_token_reserves
            .checked_sub(tokens_out)
            .ok_or(error!(TokenLaunchError::MathOverflow))?;
        self.real_sol_reserves = self.real_sol_reserves
            .checked_add(sol_in_after_fee)
            .ok_or(error!(TokenLaunchError::MathOverflow))?;
        self.real_token_reserves = self.real_token_reserves
            .checked_sub(tokens_out)
            .ok_or(error!(TokenLaunchError::MathOverflow))?;
        Ok(())
    }

    /// Apply sell: update reserves after sale
    /// Returns error if arithmetic would overflow/underflow — amounts must be
    /// pre-validated by get_sol_for_tokens() before calling this.
    pub fn apply_sell(&mut self, tokens_in: u64, sol_out_before_fee: u64) -> Result<()> {
        self.virtual_token_reserves = self.virtual_token_reserves
            .checked_add(tokens_in)
            .ok_or(error!(TokenLaunchError::MathOverflow))?;
        self.virtual_sol_reserves = self.virtual_sol_reserves
            .checked_sub(sol_out_before_fee)
            .ok_or(error!(TokenLaunchError::MathOverflow))?;
        self.real_token_reserves = self.real_token_reserves
            .checked_add(tokens_in)
            .ok_or(error!(TokenLaunchError::MathOverflow))?;
        self.real_sol_reserves = self.real_sol_reserves
            .checked_sub(sol_out_before_fee)
            .ok_or(error!(TokenLaunchError::MathOverflow))?;
        Ok(())
    }

    /// Graduation: threshold reached OR the curve supply is sold out. With the
    /// constants above both happen together (selling 700M raises exactly 85 SOL);
    /// the sell-out condition guards against rounding leaving real_sol a few
    /// lamports short of the threshold.
    pub fn should_graduate(&self) -> bool {
        self.real_sol_reserves >= GRADUATION_THRESHOLD || self.real_token_reserves == 0
    }

    /// Reserve tokens to pair with `liquidity_sol` so the DEX pool opens at the
    /// curve's final price (vS / vT): tokens = liquidity_sol · vT / vS.
    /// (Audit H4: sending the full 300M reserve opened the pool ~45% below the
    /// curve's last price, an instant loss for holders.)
    pub fn pool_tokens_for(&self, liquidity_sol: u64) -> Option<u64> {
        let t = (liquidity_sol as u128)
            .checked_mul(self.virtual_token_reserves as u128)?
            .checked_div(self.virtual_sol_reserves as u128)?;
        u64::try_from(t).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOL: u64 = 1_000_000_000;

    fn fresh() -> BondingCurveState {
        BondingCurveState {
            mint: Pubkey::default(),
            creator: Pubkey::default(),
            virtual_sol_reserves: INITIAL_VIRTUAL_SOL,
            virtual_token_reserves: INITIAL_VIRTUAL_TOKENS,
            real_sol_reserves: 0,
            real_token_reserves: REAL_TOKEN_RESERVES_INIT,
            token_total_supply: TOTAL_SUPPLY,
            complete: false,
            created_at: 0,
            total_volume_sol: 0,
            total_trades: 0,
            bump: 0,
        }
    }

    /// Net buy as the program does it: fee off the top, partial fill at the end.
    fn buy(s: &mut BondingCurveState, gross: u64) -> Option<(u64, u64)> {
        let fee = gross * FEE_BPS / BPS_DENOMINATOR;
        let (tokens, used) = s.quote_buy(gross - fee)?;
        if tokens == 0 {
            return None;
        }
        s.apply_buy(used, tokens).ok()?;
        Some((tokens, used))
    }

    // ── C1: graduation must be reachable ────────────────────────────────────
    #[test]
    fn selling_out_the_curve_raises_the_graduation_threshold() {
        let k = INITIAL_VIRTUAL_SOL as u128 * INITIAL_VIRTUAL_TOKENS as u128;
        let end_vt = (INITIAL_VIRTUAL_TOKENS - REAL_TOKEN_RESERVES_INIT) as u128;
        let raise = ceil_div(k, end_vt).unwrap() - INITIAL_VIRTUAL_SOL as u128;
        assert!(raise >= GRADUATION_THRESHOLD as u128, "sell-out raises {raise}, below threshold");
        assert!(raise <= GRADUATION_THRESHOLD as u128 + 1_000, "sell-out overshoots threshold: {raise}");
    }

    #[test]
    fn buying_to_sell_out_completes_the_curve() {
        let mut s = fresh();
        let mut steps = 0;
        while !s.should_graduate() {
            buy(&mut s, 3 * SOL).expect("buy must not revert before graduation");
            steps += 1;
            assert!(steps < 1_000);
        }
        assert_eq!(s.real_token_reserves, 0);
        assert!(s.real_sol_reserves >= GRADUATION_THRESHOLD);
        // Invariant: real SOL == virtual SOL − initial virtual SOL
        assert_eq!(s.real_sol_reserves, s.virtual_sol_reserves - INITIAL_VIRTUAL_SOL);
    }

    // ── M1: final buy partially fills instead of reverting ──────────────────
    #[test]
    fn oversized_final_buy_partially_fills_and_charges_only_what_is_needed() {
        let mut s = fresh();
        buy(&mut s, 80 * SOL).unwrap(); // close to sell-out
        let remaining = s.real_token_reserves;
        let (tokens, used) = s.quote_buy(50 * SOL).unwrap(); // far more than needed
        assert_eq!(tokens, remaining);
        assert!(used < 50 * SOL);
        s.apply_buy(used, tokens).unwrap();
        assert!(s.should_graduate());
    }

    // ── L1: rounding never favors the trader (k never decreases) ────────────
    #[test]
    fn constant_product_never_decreases() {
        let mut s = fresh();
        let mut k = s.virtual_sol_reserves as u128 * s.virtual_token_reserves as u128;
        for i in 1..200u64 {
            let amt = (i * 7_919_311) % (2 * SOL) + 1_000;
            if i % 3 == 0 {
                let t = (amt as u128 * 1_000) as u64;
                if let Some(out) = s.get_sol_for_tokens(t) {
                    if out > 0 && t <= TOTAL_SUPPLY {
                        s.apply_sell(t, out).unwrap();
                    }
                }
            } else if buy(&mut s, amt).is_none() {
                break;
            }
            let nk = s.virtual_sol_reserves as u128 * s.virtual_token_reserves as u128;
            assert!(nk >= k, "k decreased at step {i}");
            k = nk;
        }
    }

    // ── H4: pool opens at the curve's final price ───────────────────────────
    #[test]
    fn pool_is_sized_for_price_continuity_and_fits_in_reserve() {
        let mut s = fresh();
        while !s.should_graduate() {
            buy(&mut s, 5 * SOL).unwrap();
        }
        let liquidity = s.real_sol_reserves - s.real_sol_reserves * 500 / 10_000 * 2;
        let pool_tokens = s.pool_tokens_for(liquidity).unwrap();
        assert!(pool_tokens <= RESERVE_TOKEN_AMOUNT, "pool needs more than the reserve");
        // pool price (liquidity / pool_tokens) vs curve end price (vS / vT): within 0.01%
        let lhs = liquidity as u128 * s.virtual_token_reserves as u128;
        let rhs = pool_tokens as u128 * s.virtual_sol_reserves as u128;
        let diff = if lhs > rhs { lhs - rhs } else { rhs - lhs };
        assert!(diff * 10_000 <= rhs, "pool price differs from curve end price by >0.01%");
    }
}
