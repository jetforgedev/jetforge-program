//! Atomic graduation: curve → Raydium CPMM pool → LP burned, in one instruction.
//!
//! The pool's SOL and tokens never pass through a wallet with a private key (the
//! off-chain flow parked ~90% of the raised SOL and the LP tokens on the hot
//! treasury key between transactions — audit H3). A per-mint migration PDA (no
//! data, owned by the system program) acts as Raydium's pool "creator": it holds
//! the pool slice, pays the pool accounts' rent and Raydium's creation fee, signs
//! via seeds, and burns the LP tokens it receives. If any step fails, the whole
//! transaction reverts and everything stays in the curve.
//!
//! Permissionless like `graduate` was: the outcome is fully determined on-chain.

use anchor_lang::prelude::*;
use anchor_lang::system_program::{self, Transfer as SolTransfer};
use anchor_spl::{
    associated_token::{self, get_associated_token_address, AssociatedToken, Create as CreateAta},
    token::{
        burn, close_account, spl_token::native_mint, sync_native, transfer, Burn, CloseAccount,
        Mint, SyncNative, Token, TokenAccount, Transfer as TokenTransfer,
    },
};

use crate::{
    errors::TokenLaunchError,
    instructions::graduate::GraduationEvent,
    raydium::{self, CpmmInitialize, CPMM_PROGRAM_ID},
    state::{
        BondingCurveState, BPS_DENOMINATOR, GRADUATION_CREATOR_BPS, GRADUATION_TREASURY_BPS,
        MIGRATION_SEED, POOL_RENT_BUDGET, POOL_STATE_SEED,
    },
};

#[derive(Accounts)]
pub struct GraduateToRaydium<'info> {
    /// Anyone may trigger graduation; pays the two temporary token accounts' rent
    /// (refunded at the end).
    #[account(mut)]
    pub caller: Signer<'info>,

    #[account(mut)]
    pub mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        close = treasury,
        seeds = [b"bonding_curve", mint.key().as_ref()],
        bump = bonding_curve.bump,
        has_one = mint,
        constraint = bonding_curve.complete @ TokenLaunchError::GraduationThresholdNotMet,
    )]
    pub bonding_curve: Box<Account<'info, BondingCurveState>>,

    /// Unsold curve tokens — burned.
    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = bonding_curve,
    )]
    pub token_vault: Box<Account<'info, TokenAccount>>,

    /// 300M reserve — the price-continuous pool slice goes to Raydium, the rest is burned.
    #[account(
        mut,
        seeds = [b"reserve_vault", mint.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = bonding_curve,
    )]
    pub reserve_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: must be the program's hardcoded treasury.
    #[account(mut, constraint = treasury.key() == crate::TREASURY_PUBKEY @ TokenLaunchError::Unauthorized)]
    pub treasury: UncheckedAccount<'info>,

    /// CHECK: validated against bonding_curve.creator.
    #[account(mut, constraint = creator.key() == bonding_curve.creator @ TokenLaunchError::Unauthorized)]
    pub creator: UncheckedAccount<'info>,

    /// CHECK: validated by PDA seeds [b"creator_vault", mint]
    #[account(mut, seeds = [b"creator_vault", mint.key().as_ref()], bump)]
    pub creator_vault: UncheckedAccount<'info>,

    /// CHECK: validated by PDA seeds [b"buyback_vault", mint]
    #[account(mut, seeds = [b"buyback_vault", mint.key().as_ref()], bump)]
    pub buyback_vault: UncheckedAccount<'info>,

    /// Raydium pool "creator". Data-free and system-owned, so it can pay rent and
    /// the pool fee; only this program can sign for it.
    /// CHECK: validated by PDA seeds [b"migration", mint]
    #[account(mut, seeds = [MIGRATION_SEED, mint.key().as_ref()], bump)]
    pub migration_authority: UncheckedAccount<'info>,

    // The two temporary token accounts are created in the handler (idempotent ATA
    // CPI) rather than with `init_if_needed`: that codegen pushed this struct's
    // validation past the 4 KB BPF stack limit. Addresses are checked there.
    /// CHECK: ATA(mint, migration_authority); created + verified in the handler.
    #[account(mut)]
    pub migration_token_account: UncheckedAccount<'info>,
    /// CHECK: ATA(wSOL, migration_authority); created + verified in the handler.
    #[account(mut)]
    pub migration_wsol_account: UncheckedAccount<'info>,
    /// CHECK: the native (wSOL) mint.
    #[account(address = native_mint::ID)]
    pub wsol_mint: UncheckedAccount<'info>,

    // ── Raydium CPMM ────────────────────────────────────────────────────────────
    /// CHECK: must be Raydium's CPMM program.
    #[account(address = CPMM_PROGRAM_ID @ TokenLaunchError::RaydiumAccountMismatch)]
    pub cpmm_program: UncheckedAccount<'info>,
    /// CHECK: verified in the handler (derived from AMM_CONFIG_INDEX, owned by CPMM).
    pub amm_config: UncheckedAccount<'info>,
    /// CHECK: Raydium's vault/LP authority PDA; Raydium verifies its seeds.
    pub raydium_authority: UncheckedAccount<'info>,
    /// The pool account: a PDA of this program (signs the pool creation), so no
    /// one can create a pool there first and block graduation. Created by Raydium.
    /// CHECK: validated by PDA seeds [b"raydium_pool", mint]
    #[account(mut, seeds = [POOL_STATE_SEED, mint.key().as_ref()], bump)]
    pub pool_state: UncheckedAccount<'info>,
    /// CHECK: created by Raydium (seeds verified there).
    #[account(mut)]
    pub lp_mint: UncheckedAccount<'info>,
    /// CHECK: the migration PDA's LP token account; created by Raydium, burned here.
    #[account(mut)]
    pub migration_lp_account: UncheckedAccount<'info>,
    /// CHECK: pool vault for token_0 (lower mint address); seeds verified by Raydium.
    #[account(mut)]
    pub pool_vault_0: UncheckedAccount<'info>,
    /// CHECK: pool vault for token_1; seeds verified by Raydium.
    #[account(mut)]
    pub pool_vault_1: UncheckedAccount<'info>,
    /// CHECK: Raydium's pool-fee receiver; address verified by Raydium.
    #[account(mut)]
    pub create_pool_fee: UncheckedAccount<'info>,
    /// CHECK: created by Raydium (seeds verified there).
    #[account(mut)]
    pub observation_state: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

/// Emitted with `GraduationEvent` when the pool is created on-chain.
#[event]
pub struct RaydiumPoolCreatedEvent {
    pub mint: Pubkey,
    pub pool_state: Pubkey,
    pub lp_mint: Pubkey,
    pub liquidity_sol: u64,
    pub pool_tokens: u64,
    pub lp_burned: u64,
    pub timestamp: i64,
}

pub fn graduate_to_raydium(ctx: Context<GraduateToRaydium>) -> Result<()> {
    let mint_key = ctx.accounts.mint.key();
    let wsol_key = native_mint::ID;

    // ── Raydium accounts we must not take on trust ────────────────────────────
    let amm_config = raydium::amm_config_address();
    require_keys_eq!(ctx.accounts.amm_config.key(), amm_config, TokenLaunchError::RaydiumAccountMismatch);
    require_keys_eq!(*ctx.accounts.amm_config.owner, CPMM_PROGRAM_ID, TokenLaunchError::RaydiumAccountMismatch);
    // Raydium orders the pair by address: token_0 < token_1.
    let token_is_0 = mint_key < wsol_key;

    // Temporary token accounts of the migration PDA (caller pays rent; refunded below).
    let migration_key = ctx.accounts.migration_authority.key();
    require_keys_eq!(
        ctx.accounts.migration_token_account.key(),
        get_associated_token_address(&migration_key, &mint_key),
        TokenLaunchError::InvalidMint
    );
    require_keys_eq!(
        ctx.accounts.migration_wsol_account.key(),
        get_associated_token_address(&migration_key, &wsol_key),
        TokenLaunchError::InvalidMint
    );
    for (account, token_mint) in [
        (ctx.accounts.migration_token_account.to_account_info(), ctx.accounts.mint.to_account_info()),
        (ctx.accounts.migration_wsol_account.to_account_info(), ctx.accounts.wsol_mint.to_account_info()),
    ] {
        associated_token::create_idempotent(CpiContext::new(
            ctx.accounts.associated_token_program.to_account_info(),
            CreateAta {
                payer: ctx.accounts.caller.to_account_info(),
                associated_token: account,
                authority: ctx.accounts.migration_authority.to_account_info(),
                mint: token_mint,
                system_program: ctx.accounts.system_program.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
            },
        ))?;
    }

    let curve_bump = ctx.accounts.bonding_curve.bump;
    let curve_seeds: &[&[u8]] = &[b"bonding_curve", mint_key.as_ref(), &[curve_bump]];
    let migration_seeds: &[&[u8]] = &[MIGRATION_SEED, mint_key.as_ref(), &[ctx.bumps.migration_authority]];
    let pool_seeds: &[&[u8]] = &[POOL_STATE_SEED, mint_key.as_ref(), &[ctx.bumps.pool_state]];

    let total_sol = ctx.accounts.bonding_curve.real_sol_reserves;
    let unsold_tokens = ctx.accounts.token_vault.amount;
    let reserve_tokens = ctx.accounts.reserve_vault.amount;
    let creator = ctx.accounts.bonding_curve.creator;
    let total_volume = ctx.accounts.bonding_curve.total_volume_sol;
    let total_trades = ctx.accounts.bonding_curve.total_trades;

    // ── SOL split (same as `graduate`): 5% creator, 5% treasury, 90% pool ─────
    let treasury_cut = total_sol
        .checked_mul(GRADUATION_TREASURY_BPS).ok_or(TokenLaunchError::MathOverflow)?
        .checked_div(BPS_DENOMINATOR).ok_or(TokenLaunchError::MathOverflow)?;
    let creator_bonus = total_sol
        .checked_mul(GRADUATION_CREATOR_BPS).ok_or(TokenLaunchError::MathOverflow)?
        .checked_div(BPS_DENOMINATOR).ok_or(TokenLaunchError::MathOverflow)?;
    let curve_90 = total_sol
        .checked_sub(treasury_cut).ok_or(TokenLaunchError::MathOverflow)?
        .checked_sub(creator_bonus).ok_or(TokenLaunchError::MathOverflow)?;
    // Pool rent + Raydium's creation fee come out of the platform's cut, never the
    // creator's reward; unspent budget returns to the treasury below. The fee is
    // read from the (verified) fee-tier config, so a Raydium fee change can't
    // break graduation. Only if the cut is too small (graduations far below 85 SOL,
    // which a mainnet curve can't reach) does the shortfall come from the pool SOL.
    let create_pool_fee = raydium::create_pool_fee(&ctx.accounts.amm_config.to_account_info())?;
    let pool_budget = create_pool_fee.checked_add(POOL_RENT_BUDGET).ok_or(TokenLaunchError::MathOverflow)?;
    let budget_from_treasury = pool_budget.min(treasury_cut);
    let budget_from_pool = pool_budget - budget_from_treasury;
    let curve_liquidity = curve_90.checked_sub(budget_from_pool).ok_or(TokenLaunchError::MathOverflow)?;

    // ── 1. Sweep fee vaults: creator fees → creator; buyback SOL → pool ───────
    let creator_fees = ctx.accounts.creator_vault.lamports();
    if creator_fees > 0 {
        let cv: &[&[u8]] = &[b"creator_vault", mint_key.as_ref(), &[ctx.bumps.creator_vault]];
        system_program::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.to_account_info(),
                SolTransfer { from: ctx.accounts.creator_vault.to_account_info(), to: ctx.accounts.creator.to_account_info() },
                &[cv],
            ),
            creator_fees,
        )?;
    }
    let buyback_leftover = ctx.accounts.buyback_vault.lamports();
    if buyback_leftover > 0 {
        let bb: &[&[u8]] = &[b"buyback_vault", mint_key.as_ref(), &[ctx.bumps.buyback_vault]];
        system_program::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.to_account_info(),
                SolTransfer { from: ctx.accounts.buyback_vault.to_account_info(), to: ctx.accounts.migration_authority.to_account_info() },
                &[bb],
            ),
            buyback_leftover,
        )?;
    }
    let liquidity_sol = curve_liquidity.checked_add(buyback_leftover).ok_or(TokenLaunchError::MathOverflow)?;

    // ── 2. Tokens: pool slice → migration PDA; burn the rest (audit H4) ───────
    let pool_tokens = ctx.accounts.bonding_curve
        .pool_tokens_for(liquidity_sol).ok_or(TokenLaunchError::MathOverflow)?
        .min(reserve_tokens);
    let excess_reserve = reserve_tokens.checked_sub(pool_tokens).ok_or(TokenLaunchError::MathOverflow)?;
    require!(pool_tokens > 0 && liquidity_sol > 0, TokenLaunchError::InvalidAmount);

    transfer(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            TokenTransfer {
                from: ctx.accounts.reserve_vault.to_account_info(),
                to: ctx.accounts.migration_token_account.to_account_info(),
                authority: ctx.accounts.bonding_curve.to_account_info(),
            },
            &[curve_seeds],
        ),
        pool_tokens,
    )?;
    if excess_reserve > 0 {
        burn(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Burn { mint: ctx.accounts.mint.to_account_info(), from: ctx.accounts.reserve_vault.to_account_info(), authority: ctx.accounts.bonding_curve.to_account_info() },
                &[curve_seeds],
            ),
            excess_reserve,
        )?;
    }
    if unsold_tokens > 0 {
        burn(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Burn { mint: ctx.accounts.mint.to_account_info(), from: ctx.accounts.token_vault.to_account_info(), authority: ctx.accounts.bonding_curve.to_account_info() },
                &[curve_seeds],
            ),
            unsold_tokens,
        )?;
    }

    // ── 3. SOL out of the curve (program-owned, so a direct lamport move) ─────
    let curve_ai = ctx.accounts.bonding_curve.to_account_info();
    move_lamports(&curve_ai, &ctx.accounts.creator.to_account_info(), creator_bonus)?;
    move_lamports(&curve_ai, &ctx.accounts.treasury.to_account_info(), treasury_cut - budget_from_treasury)?;
    move_lamports(
        &curve_ai,
        &ctx.accounts.migration_authority.to_account_info(),
        curve_liquidity.checked_add(pool_budget).ok_or(TokenLaunchError::MathOverflow)?,
    )?;

    // ── 4. Wrap the pool SOL ───────────────────────────────────────────────────
    // The runtime only syncs the accounts passed to a CPI; balances changed
    // directly above (curve, creator, treasury) would otherwise look unbalanced
    // ("sum of account balances ... do not match"). So this first CPI also carries
    // them as extra accounts; the System program ignores accounts past [from, to].
    let mut wrap_ix = anchor_lang::solana_program::system_instruction::transfer(
        &ctx.accounts.migration_authority.key(),
        &ctx.accounts.migration_wsol_account.key(),
        liquidity_sol,
    );
    wrap_ix.accounts.extend([
        AccountMeta::new(ctx.accounts.bonding_curve.key(), false),
        AccountMeta::new(ctx.accounts.creator.key(), false),
        AccountMeta::new(ctx.accounts.treasury.key(), false),
    ]);
    anchor_lang::solana_program::program::invoke_signed(
        &wrap_ix,
        &[
            ctx.accounts.migration_authority.to_account_info(),
            ctx.accounts.migration_wsol_account.to_account_info(),
            ctx.accounts.bonding_curve.to_account_info(),
            ctx.accounts.creator.to_account_info(),
            ctx.accounts.treasury.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
        ],
        &[migration_seeds],
    )?;
    sync_native(CpiContext::new(
        ctx.accounts.token_program.to_account_info(),
        SyncNative { account: ctx.accounts.migration_wsol_account.to_account_info() },
    ))?;

    // ── 5. Create the Raydium pool (migration PDA signs as creator) ───────────
    let mint_ai = ctx.accounts.mint.to_account_info();
    let wsol_ai = ctx.accounts.wsol_mint.to_account_info();
    let token_acc_ai = ctx.accounts.migration_token_account.to_account_info();
    let wsol_acc_ai = ctx.accounts.migration_wsol_account.to_account_info();
    let token_program_ai = ctx.accounts.token_program.to_account_info();
    let (token_0_mint, token_1_mint, creator_token_0, creator_token_1, amount_0, amount_1) = if token_is_0 {
        (&mint_ai, &wsol_ai, &token_acc_ai, &wsol_acc_ai, pool_tokens, liquidity_sol)
    } else {
        (&wsol_ai, &mint_ai, &wsol_acc_ai, &token_acc_ai, liquidity_sol, pool_tokens)
    };
    raydium::cpmm_initialize(
        CpmmInitialize {
            cpmm_program: &ctx.accounts.cpmm_program.to_account_info(),
            creator: &ctx.accounts.migration_authority.to_account_info(),
            amm_config: &ctx.accounts.amm_config.to_account_info(),
            authority: &ctx.accounts.raydium_authority.to_account_info(),
            pool_state: &ctx.accounts.pool_state.to_account_info(),
            token_0_mint,
            token_1_mint,
            lp_mint: &ctx.accounts.lp_mint.to_account_info(),
            creator_token_0,
            creator_token_1,
            creator_lp_token: &ctx.accounts.migration_lp_account.to_account_info(),
            token_0_vault: &ctx.accounts.pool_vault_0.to_account_info(),
            token_1_vault: &ctx.accounts.pool_vault_1.to_account_info(),
            create_pool_fee: &ctx.accounts.create_pool_fee.to_account_info(),
            observation_state: &ctx.accounts.observation_state.to_account_info(),
            token_program: &token_program_ai,
            token_0_program: &token_program_ai,
            token_1_program: &token_program_ai,
            associated_token_program: &ctx.accounts.associated_token_program.to_account_info(),
            system_program: &ctx.accounts.system_program.to_account_info(),
            rent: &ctx.accounts.rent.to_account_info(),
        },
        amount_0,
        amount_1,
        0, // open now
        &[migration_seeds, pool_seeds],
    )?;

    // ── 6. Burn every LP token → liquidity locked forever ─────────────────────
    let lp_amount = {
        let lp = ctx.accounts.migration_lp_account.to_account_info();
        require_keys_eq!(*lp.owner, ctx.accounts.token_program.key(), TokenLaunchError::RaydiumAccountMismatch);
        let account = TokenAccount::try_deserialize(&mut &lp.try_borrow_data()?[..])?;
        require_keys_eq!(account.owner, ctx.accounts.migration_authority.key(), TokenLaunchError::RaydiumAccountMismatch);
        require_keys_eq!(account.mint, ctx.accounts.lp_mint.key(), TokenLaunchError::RaydiumAccountMismatch);
        account.amount
    };
    require!(lp_amount > 0, TokenLaunchError::InvalidAmount);
    burn(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Burn {
                mint: ctx.accounts.lp_mint.to_account_info(),
                from: ctx.accounts.migration_lp_account.to_account_info(),
                authority: ctx.accounts.migration_authority.to_account_info(),
            },
            &[migration_seeds],
        ),
        lp_amount,
    )?;

    // ── 7. Clean up: close the emptied token accounts, return unspent SOL ─────
    close_account(CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        CloseAccount {
            account: ctx.accounts.migration_lp_account.to_account_info(),
            destination: ctx.accounts.treasury.to_account_info(),
            authority: ctx.accounts.migration_authority.to_account_info(),
        },
        &[migration_seeds],
    ))?;
    // Raydium took exactly `pool_tokens`; anything else in the account was sent
    // there by someone else, and a non-empty token account can't be closed. Burn it
    // so a stray deposit can never block graduation.
    let stray = {
        let info = ctx.accounts.migration_token_account.to_account_info();
        let data = info.try_borrow_data()?;
        TokenAccount::try_deserialize(&mut &data[..])?.amount
    };
    if stray > 0 {
        burn(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.mint.to_account_info(),
                    from: ctx.accounts.migration_token_account.to_account_info(),
                    authority: ctx.accounts.migration_authority.to_account_info(),
                },
                &[migration_seeds],
            ),
            stray,
        )?;
    }
    // Native (wSOL) accounts close with any balance; the lamports go to the caller
    // who paid their rent.
    for account in [
        ctx.accounts.migration_token_account.to_account_info(),
        ctx.accounts.migration_wsol_account.to_account_info(),
    ] {
        close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount { account, destination: ctx.accounts.caller.to_account_info(), authority: ctx.accounts.migration_authority.to_account_info() },
            &[migration_seeds],
        ))?;
    }
    let unspent = ctx.accounts.migration_authority.lamports();
    if unspent > 0 {
        system_program::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.to_account_info(),
                SolTransfer { from: ctx.accounts.migration_authority.to_account_info(), to: ctx.accounts.treasury.to_account_info() },
                &[migration_seeds],
            ),
            unspent,
        )?;
    }

    let bonding_curve = &mut ctx.accounts.bonding_curve;
    bonding_curve.real_sol_reserves = 0;
    bonding_curve.real_token_reserves = 0;

    let now = Clock::get()?.unix_timestamp;
    emit!(GraduationEvent {
        mint: mint_key,
        creator,
        real_sol_reserves: liquidity_sol,
        real_token_reserves: pool_tokens,
        tokens_burned: unsold_tokens.checked_add(excess_reserve).ok_or(TokenLaunchError::MathOverflow)?,
        total_volume_sol: total_volume,
        total_trades,
        timestamp: now,
    });
    emit!(RaydiumPoolCreatedEvent {
        mint: mint_key,
        pool_state: ctx.accounts.pool_state.key(),
        lp_mint: ctx.accounts.lp_mint.key(),
        liquidity_sol,
        pool_tokens,
        lp_burned: lp_amount,
        timestamp: now,
    });

    Ok(())
}

/// Move lamports out of an account this program owns (the curve). The runtime
/// only lets the owner debit; any writable account can be credited.
fn move_lamports(from: &AccountInfo, to: &AccountInfo, amount: u64) -> Result<()> {
    let from_after = from.lamports().checked_sub(amount).ok_or(TokenLaunchError::MathOverflow)?;
    let to_after = to.lamports().checked_add(amount).ok_or(TokenLaunchError::MathOverflow)?;
    **from.try_borrow_mut_lamports()? = from_after;
    **to.try_borrow_mut_lamports()? = to_after;
    Ok(())
}
