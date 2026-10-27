//! Raydium CPMM (constant-product) pool creation via CPI.
//!
//! Raydium's `raydium-cp-swap` crate targets a different Anchor version, so the
//! `initialize` instruction is built by hand. Account order and data layout match
//! raydium-cp-swap `instructions/initialize.rs` and the Raydium SDK v2 builder
//! (`makeCreateCpmmPoolInInstruction`), which is what creates pools in production.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
};

/// Raydium CPMM program. `--features devnet` builds against Raydium's devnet deployment.
#[cfg(feature = "devnet")]
pub const CPMM_PROGRAM_ID: Pubkey = pubkey!("DRaycpLY18LhpbydsBWbVJtxpNv9oXPgjRSfpF2bWpYb");
#[cfg(not(feature = "devnet"))]
pub const CPMM_PROGRAM_ID: Pubkey = pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");

/// Fee tier the pool is created in (index 0 = the lowest-fee config, as the
/// off-chain graduation service used). The config address is derived from it.
pub const AMM_CONFIG_INDEX: u16 = 0;

const AMM_CONFIG_SEED: &[u8] = b"amm_config";

/// Anchor discriminator of `initialize`: sha256("global:initialize")[..8].
const INITIALIZE_DISCRIMINATOR: [u8; 8] = [175, 175, 109, 31, 13, 152, 155, 237];

/// The fee-tier config account for `AMM_CONFIG_INDEX`.
pub fn amm_config_address() -> Pubkey {
    Pubkey::find_program_address(
        &[AMM_CONFIG_SEED, &AMM_CONFIG_INDEX.to_be_bytes()],
        &CPMM_PROGRAM_ID,
    )
    .0
}

/// Raydium's pool-creation fee (lamports) from an `AmmConfig` account. Layout
/// (raydium-cp-swap states/config.rs): 8 discriminator, bump u8, disable_create_pool
/// bool, index u16, trade/protocol/fund fee rates u64 x3, then create_pool_fee u64.
/// The caller must have verified the account's address and owner.
pub fn create_pool_fee(amm_config: &AccountInfo) -> Result<u64> {
    const OFFSET: usize = 8 + 1 + 1 + 2 + 8 + 8 + 8;
    let data = amm_config.try_borrow_data()?;
    let bytes: [u8; 8] = data
        .get(OFFSET..OFFSET + 8)
        .and_then(|b| b.try_into().ok())
        .ok_or(error!(crate::errors::TokenLaunchError::RaydiumAccountMismatch))?;
    Ok(u64::from_le_bytes(bytes))
}

/// Accounts for CPMM `initialize`, in Raydium's order. `creator` must sign
/// (here: our migration PDA via `invoke_signed`) and pays the rent of the new
/// pool accounts plus the pool-creation fee in SOL. `pool_state` is a PDA of
/// *this* program and signs too: Raydium accepts any signing pool address, and
/// unlike its own canonical PDA nobody can create a pool there first and block
/// the graduation.
pub struct CpmmInitialize<'a, 'info> {
    pub cpmm_program: &'a AccountInfo<'info>,
    pub creator: &'a AccountInfo<'info>,
    pub amm_config: &'a AccountInfo<'info>,
    pub authority: &'a AccountInfo<'info>,
    pub pool_state: &'a AccountInfo<'info>,
    pub token_0_mint: &'a AccountInfo<'info>,
    pub token_1_mint: &'a AccountInfo<'info>,
    pub lp_mint: &'a AccountInfo<'info>,
    pub creator_token_0: &'a AccountInfo<'info>,
    pub creator_token_1: &'a AccountInfo<'info>,
    pub creator_lp_token: &'a AccountInfo<'info>,
    pub token_0_vault: &'a AccountInfo<'info>,
    pub token_1_vault: &'a AccountInfo<'info>,
    pub create_pool_fee: &'a AccountInfo<'info>,
    pub observation_state: &'a AccountInfo<'info>,
    pub token_program: &'a AccountInfo<'info>,
    pub token_0_program: &'a AccountInfo<'info>,
    pub token_1_program: &'a AccountInfo<'info>,
    pub associated_token_program: &'a AccountInfo<'info>,
    pub system_program: &'a AccountInfo<'info>,
    pub rent: &'a AccountInfo<'info>,
}

pub fn cpmm_initialize(
    a: CpmmInitialize<'_, '_>,
    init_amount_0: u64,
    init_amount_1: u64,
    open_time: u64,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    let metas = vec![
        AccountMeta::new(a.creator.key(), true),
        AccountMeta::new_readonly(a.amm_config.key(), false),
        AccountMeta::new_readonly(a.authority.key(), false),
        AccountMeta::new(a.pool_state.key(), true),
        AccountMeta::new_readonly(a.token_0_mint.key(), false),
        AccountMeta::new_readonly(a.token_1_mint.key(), false),
        AccountMeta::new(a.lp_mint.key(), false),
        AccountMeta::new(a.creator_token_0.key(), false),
        AccountMeta::new(a.creator_token_1.key(), false),
        AccountMeta::new(a.creator_lp_token.key(), false),
        AccountMeta::new(a.token_0_vault.key(), false),
        AccountMeta::new(a.token_1_vault.key(), false),
        AccountMeta::new(a.create_pool_fee.key(), false),
        AccountMeta::new(a.observation_state.key(), false),
        AccountMeta::new_readonly(a.token_program.key(), false),
        AccountMeta::new_readonly(a.token_0_program.key(), false),
        AccountMeta::new_readonly(a.token_1_program.key(), false),
        AccountMeta::new_readonly(a.associated_token_program.key(), false),
        AccountMeta::new_readonly(a.system_program.key(), false),
        AccountMeta::new_readonly(a.rent.key(), false),
    ];

    let mut data = Vec::with_capacity(8 + 24);
    data.extend_from_slice(&INITIALIZE_DISCRIMINATOR);
    data.extend_from_slice(&init_amount_0.to_le_bytes());
    data.extend_from_slice(&init_amount_1.to_le_bytes());
    data.extend_from_slice(&open_time.to_le_bytes());

    let ix = Instruction { program_id: CPMM_PROGRAM_ID, accounts: metas, data };
    invoke_signed(
        &ix,
        &[
            a.creator.clone(),
            a.amm_config.clone(),
            a.authority.clone(),
            a.pool_state.clone(),
            a.token_0_mint.clone(),
            a.token_1_mint.clone(),
            a.lp_mint.clone(),
            a.creator_token_0.clone(),
            a.creator_token_1.clone(),
            a.creator_lp_token.clone(),
            a.token_0_vault.clone(),
            a.token_1_vault.clone(),
            a.create_pool_fee.clone(),
            a.observation_state.clone(),
            a.token_program.clone(),
            a.token_0_program.clone(),
            a.token_1_program.clone(),
            a.associated_token_program.clone(),
            a.system_program.clone(),
            a.rent.clone(),
            a.cpmm_program.clone(),
        ],
        signer_seeds,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::solana_program::hash::hash;

    #[test]
    fn initialize_discriminator_matches_anchor() {
        let h = hash(b"global:initialize").to_bytes();
        assert_eq!(&h[..8], &INITIALIZE_DISCRIMINATOR);
    }
}
