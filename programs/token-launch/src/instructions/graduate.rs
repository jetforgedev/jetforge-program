use anchor_lang::prelude::*;
use anchor_lang::system_program::{self, Transfer as SolTransfer};
use anchor_spl::{
    associated_token::AssociatedToken,
    token::{burn, transfer, Burn, Mint, Token, TokenAccount, Transfer as TokenTransfer},
};

use crate::{
    errors::TokenLaunchError,
    state::{BondingCurveState, GRADUATION_TREASURY_BPS, GRADUATION_CREATOR_BPS, BPS_DENOMINATOR},
};

#[derive(Accounts)]
pub struct Graduate<'info> {
    /// Emergency fallback only (see `graduate_to_raydium`): must be the Squads vault.
    #[account(mut, address = crate::GRADUATION_ADMIN @ TokenLaunchError::Unauthorized)]
    pub caller: Signer<'info>,

    /// Mint must be mutable so we can burn the unsold curve tokens
    #[account(mut)]
    pub mint: Account<'info, Mint>,

    #[account(
        mut,
        close = treasury,  // reclaims rent (~0.002 SOL) to treasury after graduation; Anchor runs this after instruction body
        seeds = [b"bonding_curve", mint.key().as_ref()],
        bump = bonding_curve.bump,
        has_one = mint,
        constraint = bonding_curve.complete @ TokenLaunchError::GraduationThresholdNotMet,
    )]
    pub bonding_curve: Account<'info, BondingCurveState>,

    /// Bonding curve's main token vault — holds unsold curve tokens (will be BURNED)
    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = bonding_curve,
    )]
    pub token_vault: Account<'info, TokenAccount>,

    /// Reserve vault — holds the 30% (300M) reserve. Only the slice needed for a
    /// price-continuous pool goes to Raydium; the rest is burned.
    /// Security note: `token::mint = mint` validates reserve_vault.mint == mint.key(),
    /// providing equivalent protection to `has_one = mint` for SPL token accounts.
    #[account(
        mut,
        seeds = [b"reserve_vault", mint.key().as_ref()],
        bump,
        token::mint = mint,           // validates reserve_vault.mint == mint.key()
        token::authority = bonding_curve,
    )]
    pub reserve_vault: Account<'info, TokenAccount>,

    /// Treasury — must match the program-hardcoded TREASURY address.
    /// CHECK: Key is validated by the constraint below.
    #[account(
        mut,
        constraint = treasury.key() == crate::TREASURY_PUBKEY @ TokenLaunchError::Unauthorized,
    )]
    pub treasury: UncheckedAccount<'info>,

    /// Treasury token account — receives the pool tokens to seed the Raydium pool.
    /// Must already exist before calling this instruction.
    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = treasury,
    )]
    pub treasury_token_account: Account<'info, TokenAccount>,

    /// Token creator — receives graduation bonus (5% of SOL) + swept creator fees
    /// CHECK: Validated against bonding_curve.creator
    #[account(mut, constraint = creator.key() == bonding_curve.creator @ TokenLaunchError::Unauthorized)]
    pub creator: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,

    // ── Appended (keep at the END so existing account order is unchanged) ──

    /// Creator fee vault — swept to the creator here. `withdraw_creator_fees`
    /// needs the bonding curve account, which this instruction closes, so any
    /// unswept balance would be locked forever (audit H1).
    /// CHECK: validated by PDA seeds [b"creator_vault", mint]
    #[account(mut, seeds = [b"creator_vault", mint.key().as_ref()], bump)]
    pub creator_vault: UncheckedAccount<'info>,

    /// Buyback vault — leftover SOL is swept into the pool liquidity here;
    /// `execute_buyback` can't run after graduation (audit H1).
    /// CHECK: validated by PDA seeds [b"buyback_vault", mint]
    #[account(mut, seeds = [b"buyback_vault", mint.key().as_ref()], bump)]
    pub buyback_vault: UncheckedAccount<'info>,
}

#[event]
pub struct GraduationEvent {
    pub mint: Pubkey,
    pub creator: Pubkey,
    /// SOL going to the Raydium pool (90% of raised SOL + swept buyback SOL)
    pub real_sol_reserves: u64,
    /// Tokens going to the Raydium pool (sized so the pool opens at the
    /// curve's final price)
    pub real_token_reserves: u64,
    /// Tokens burned: unsold curve tokens + reserve tokens not needed for the pool
    pub tokens_burned: u64,
    pub total_volume_sol: u64,
    pub total_trades: u64,
    pub timestamp: i64,
}

pub fn graduate(ctx: Context<Graduate>) -> Result<()> {
    let bonding_curve = &ctx.accounts.bonding_curve;

    // `complete` is set by buy / execute_buyback when the threshold is reached
    // OR the curve sells out (see BondingCurveState::should_graduate).
    require!(
        bonding_curve.complete,
        TokenLaunchError::GraduationThresholdNotMet
    );

    let mint_key = ctx.accounts.mint.key();
    let bump = ctx.accounts.bonding_curve.bump;
    let seeds: &[&[u8]] = &[b"bonding_curve", mint_key.as_ref(), &[bump]];
    let signer_seeds = &[seeds];

    let total_sol      = bonding_curve.real_sol_reserves;
    let unsold_tokens  = ctx.accounts.token_vault.amount;   // curve leftovers — BURNED
    let reserve_tokens = ctx.accounts.reserve_vault.amount; // 30% reserve — pool slice + burn
    let creator        = bonding_curve.creator;
    let total_volume   = bonding_curve.total_volume_sol;
    let total_trades   = bonding_curve.total_trades;

    msg!("[graduate] total_sol={} unsold_tokens={} reserve_tokens={}",
        total_sol, unsold_tokens, reserve_tokens);

    // ── SOL split ────────────────────────────────────────────────────────────────
    //   5%  → creator (graduation reward)
    //   5%  → treasury (platform fee)
    //   90% → treasury (to seed Raydium pool)
    let treasury_cut = total_sol
        .checked_mul(GRADUATION_TREASURY_BPS)
        .ok_or(TokenLaunchError::MathOverflow)?
        .checked_div(BPS_DENOMINATOR)
        .ok_or(TokenLaunchError::MathOverflow)?;
    let creator_bonus = total_sol
        .checked_mul(GRADUATION_CREATOR_BPS)
        .ok_or(TokenLaunchError::MathOverflow)?
        .checked_div(BPS_DENOMINATOR)
        .ok_or(TokenLaunchError::MathOverflow)?;
    let curve_liquidity = total_sol
        .checked_sub(treasury_cut)
        .ok_or(TokenLaunchError::MathOverflow)?
        .checked_sub(creator_bonus)
        .ok_or(TokenLaunchError::MathOverflow)?;

    // ── Step 0: sweep the fee vaults (system CPIs before any direct lamport edits)
    let creator_fees = ctx.accounts.creator_vault.lamports();
    if creator_fees > 0 {
        let cv_seeds: &[&[u8]] = &[b"creator_vault", mint_key.as_ref(), &[ctx.bumps.creator_vault]];
        system_program::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.to_account_info(),
                SolTransfer {
                    from: ctx.accounts.creator_vault.to_account_info(),
                    to: ctx.accounts.creator.to_account_info(),
                },
                &[cv_seeds],
            ),
            creator_fees,
        )?;
    }
    let buyback_leftover = ctx.accounts.buyback_vault.lamports();
    if buyback_leftover > 0 {
        let bb_seeds: &[&[u8]] = &[b"buyback_vault", mint_key.as_ref(), &[ctx.bumps.buyback_vault]];
        system_program::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.to_account_info(),
                SolTransfer {
                    from: ctx.accounts.buyback_vault.to_account_info(),
                    to: ctx.accounts.treasury.to_account_info(),
                },
                &[bb_seeds],
            ),
            buyback_leftover,
        )?;
    }

    // Buyback SOL was meant to support the price → it deepens the pool.
    let liquidity_sol = curve_liquidity
        .checked_add(buyback_leftover)
        .ok_or(TokenLaunchError::MathOverflow)?;

    // ── Pool sizing: open the pool at the curve's final price (audit H4) ─────────
    let pool_tokens = ctx.accounts.bonding_curve
        .pool_tokens_for(liquidity_sol)
        .ok_or(TokenLaunchError::MathOverflow)?
        .min(reserve_tokens);
    let excess_reserve = reserve_tokens
        .checked_sub(pool_tokens)
        .ok_or(TokenLaunchError::MathOverflow)?;

    msg!("[graduate] treasury_cut={} creator_bonus={} creator_fees={} liquidity_sol={} pool_tokens={} excess_reserve={}",
        treasury_cut, creator_bonus, creator_fees, liquidity_sol, pool_tokens, excess_reserve);

    // ── Step 1: pool slice of the reserve → treasury ATA (for the Raydium pool) ──
    if pool_tokens > 0 {
        transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                TokenTransfer {
                    from: ctx.accounts.reserve_vault.to_account_info(),
                    to: ctx.accounts.treasury_token_account.to_account_info(),
                    authority: ctx.accounts.bonding_curve.to_account_info(),
                },
                signer_seeds,
            ),
            pool_tokens,
        )?;
    }

    // ── Step 2: BURN the reserve not needed for the pool ─────────────────────────
    if excess_reserve > 0 {
        burn(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.mint.to_account_info(),
                    from: ctx.accounts.reserve_vault.to_account_info(),
                    authority: ctx.accounts.bonding_curve.to_account_info(),
                },
                signer_seeds,
            ),
            excess_reserve,
        )?;
    }

    // ── Step 3: BURN unsold curve tokens ─────────────────────────────────────────
    if unsold_tokens > 0 {
        burn(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.mint.to_account_info(),
                    from: ctx.accounts.token_vault.to_account_info(),
                    authority: ctx.accounts.bonding_curve.to_account_info(),
                },
                signer_seeds,
            ),
            unsold_tokens,
        )?;
    }

    // ── Step 4: SOL distribution from the curve (direct lamport edits, last) ─────
    {
        let bc_info = ctx.accounts.bonding_curve.to_account_info();
        let mut bc_lamps = bc_info.try_borrow_mut_lamports()?;
        **bc_lamps = bc_lamps
            .checked_sub(total_sol)
            .ok_or(TokenLaunchError::MathOverflow)?;
    }

    // Treasury receives platform fee (5%) + curve liquidity SOL (90%) = 95%
    **ctx.accounts.treasury.to_account_info().try_borrow_mut_lamports()? +=
        treasury_cut.checked_add(curve_liquidity).ok_or(TokenLaunchError::MathOverflow)?;

    // Creator receives graduation bonus (5%)
    **ctx.accounts.creator.to_account_info().try_borrow_mut_lamports()? += creator_bonus;

    // ── Update bonding curve state ───────────────────────────────────────────────
    let bonding_curve = &mut ctx.accounts.bonding_curve;
    bonding_curve.real_sol_reserves = 0;
    bonding_curve.real_token_reserves = 0;

    let tokens_burned = unsold_tokens
        .checked_add(excess_reserve)
        .ok_or(TokenLaunchError::MathOverflow)?;

    emit!(GraduationEvent {
        mint: mint_key,
        creator,
        real_sol_reserves: liquidity_sol,   // SOL → Raydium pool
        real_token_reserves: pool_tokens,   // tokens → Raydium pool (price-continuous)
        tokens_burned,                      // unsold curve + excess reserve, destroyed
        total_volume_sol: total_volume,
        total_trades,
        timestamp: Clock::get()?.unix_timestamp,
    });

    Ok(())
}
