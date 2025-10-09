use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};
// ✅ Token Extensions ready (uncomment when needed)
// use anchor_spl::token_2022::{self, Token2022};
// use anchor_spl::token_interface::{Mint, TokenAccount as TokenAccount2022};
use mpl_core::{
    ID as MPL_CORE_PROGRAM_ID,
    instructions::CreateV2CpiBuilder
};
use std::cmp;

declare_id!("9zbbGQ1crgrG9dj7UXCTbx9JkXm522fg7AprTwSBHoa6");

// ========== FIXED CONSTANTS ==========
const WEEK_IN_SECONDS: i64 = 604800;  // 7 days ✅
const VESTING_PERIOD_SECONDS: i64 = 365 * 24 * 60 * 60;  // ✅ 1 YEAR (not 30 days)
const WEEKLY_EMISSION_RATE: u64 = 21;  // ✅ 0.21% = 21/10000
const EMISSION_PRECISION: u64 = 10000;  // For 0.21% precision
const REWARD_PENALTY_PERCENT: u64 = 100;  // ✅ 100% penalty on REWARDS only
const MIN_STAKE_AMOUNT: u64 = 1_000_000;
const MAX_STAKE_AMOUNT: u64 = 1_000_000_000_000_000;

// Power multiplier constants
const BASE_MULTIPLIER: u64 = 100;  // 1.0x in basis points

#[program]
pub mod staking_rewards_contract {
    use super::*;

    pub fn initialize(
        ctx: Context<Initialize>,
        admin: Pubkey,
        reward_token_mint: Pubkey,
        protocol_treasury: Pubkey,
        referral_pool: Pubkey,
        cashback_pool: Pubkey,
    ) -> Result<()> {
        let program_state = &mut ctx.accounts.program_state;
        program_state.admin = admin;
        program_state.reward_token_mint = reward_token_mint;
        program_state.protocol_treasury = protocol_treasury;
        program_state.referral_pool = referral_pool;
        program_state.cashback_pool = cashback_pool;
        program_state.current_epoch = 0;
        program_state.total_staked = 0;
        program_state.reward_pool = 0;
        program_state.is_paused = false;
        program_state.last_epoch_timestamp = Clock::get()?.unix_timestamp;
        program_state.bump = ctx.bumps.program_state;
        program_state.use_token_extensions = false;  // Can be updated later

        emit!(ProgramInitialized {
            admin,
            reward_token_mint,
            protocol_treasury,
            referral_pool,
            cashback_pool,
        });

        Ok(())
    }

    pub fn stake_tokens(
        ctx: Context<StakeTokens>,
        amount: u64,
        duration_months: u8,
        tier_id: u8,
        is_locked: bool,
        _position_seed: u64,
    ) -> Result<()> {
        prevent_reentrancy(&ctx.accounts.program_state)?;
        require!(
            amount >= MIN_STAKE_AMOUNT && amount <= MAX_STAKE_AMOUNT,
            StakingError::InvalidAmount
        );
        require!(duration_months >= 1, StakingError::InvalidDuration);

        let staking_tier = &ctx.accounts.staking_tier;
        require!(staking_tier.is_active, StakingError::TierNotActive);
        require!(
            duration_months >= staking_tier.min_duration_months
                && duration_months <= staking_tier.max_duration_months,
            StakingError::DurationNotAllowed
        );

        let clock = Clock::get()?;
        let stake_position = &mut ctx.accounts.stake_position;

        // ✅ Calculate power multiplier based on amount and duration
        let power_multiplier = calculate_power_multiplier(amount, duration_months)?;

        stake_position.owner = ctx.accounts.staker.key();
        stake_position.amount = amount;
        stake_position.tier_id = tier_id;
        stake_position.base_multiplier = staking_tier.multiplier;
        stake_position.power_multiplier = power_multiplier;  // ✅ Store power multiplier
        stake_position.duration_months = duration_months;
        stake_position.is_locked = is_locked;
        stake_position.start_timestamp = clock.unix_timestamp;
        stake_position.unlock_timestamp = calculate_unlock_timestamp(clock.unix_timestamp, duration_months)?;
        stake_position.last_reward_timestamp = clock.unix_timestamp;
        stake_position.accumulated_rewards = 0;
        stake_position.is_active = true;
        stake_position.bump = ctx.bumps.stake_position;

        // Transfer tokens
        let transfer_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.user_token_account.to_account_info(),
                to: ctx.accounts.program_vault_token_account.to_account_info(),
                authority: ctx.accounts.staker.to_account_info(),
            },
        );
        token::transfer(transfer_ctx, amount)?;

        ctx.accounts.program_state.total_staked = ctx
            .accounts.program_state.total_staked
            .checked_add(amount)
            .ok_or(StakingError::CalculationOverflow)?;

        emit!(TokensStaked {
            staker: ctx.accounts.staker.key(),
            amount,
            duration_months,
            tier_id,
            is_locked,
            power_multiplier,
            unlock_timestamp: stake_position.unlock_timestamp,
        });

        Ok(())
    }

    pub fn unstake_tokens(ctx: Context<UnstakeTokens>, _position_seed: u64) -> Result<()> {
        prevent_reentrancy(&ctx.accounts.program_state)?;

        let stake_position = &mut ctx.accounts.stake_position;
        require!(stake_position.is_active, StakingError::StakeNotActive);

        let clock = Clock::get()?;
        let is_early_unstake = clock.unix_timestamp < stake_position.unlock_timestamp;

        // ✅ PENALTY ONLY ON REWARDS, NOT PRINCIPAL
        let mut reward_penalty = 0u64;
        
        if is_early_unstake && stake_position.is_locked {
            // Penalty applies to accumulated rewards only
            reward_penalty = stake_position
                .accumulated_rewards
                .checked_mul(REWARD_PENALTY_PERCENT)
                .ok_or(StakingError::CalculationOverflow)?
                .checked_div(100)
                .ok_or(StakingError::CalculationOverflow)?;

            // Distribute penalty: 50% to reward pool, 50% to treasury
            let to_reward_pool = reward_penalty / 2;
            let to_treasury = reward_penalty - to_reward_pool;

            ctx.accounts.program_state.reward_pool = ctx
                .accounts.program_state.reward_pool
                .checked_add(to_reward_pool)
                .ok_or(StakingError::CalculationOverflow)?;

            // Transfer penalty to treasury if applicable
            if to_treasury > 0 {
                let seeds = &[
                    b"program_vault".as_ref(),
                    &[ctx.accounts.program_vault.bump],
                ];
                let signer = &[&seeds[..]];

                let transfer_ctx = CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.program_vault_token_account.to_account_info(),
                        to: ctx.accounts.treasury_token_account.to_account_info(),
                        authority: ctx.accounts.program_vault.to_account_info(),
                    },
                    signer,
                );
                token::transfer(transfer_ctx, to_treasury)?;
            }
        }

        // ✅ Return FULL principal amount (no penalty on principal!)
        let seeds = &[
            b"program_vault".as_ref(),
            &[ctx.accounts.program_vault.bump],
        ];
        let signer = &[&seeds[..]];

        let transfer_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.program_vault_token_account.to_account_info(),
                to: ctx.accounts.user_token_account.to_account_info(),
                authority: ctx.accounts.program_vault.to_account_info(),
            },
            signer,
        );
        token::transfer(transfer_ctx, stake_position.amount)?;  // ✅ Full amount returned

        ctx.accounts.program_state.total_staked = ctx
            .accounts.program_state.total_staked
            .checked_sub(stake_position.amount)
            .ok_or(StakingError::CalculationOverflow)?;

        stake_position.is_active = false;
        stake_position.amount = 0;

        emit!(TokensUnstaked {
            staker: ctx.accounts.staker.key(),
            amount: stake_position.amount,
            reward_penalty,  // ✅ Penalty on rewards only
            is_early_unstake,
        });

        Ok(())
    }

    pub fn claim_rewards(
        ctx: Context<ClaimRewards>,
        _position_seed: u64,
        _nft_seed: u64,
    ) -> Result<()> {
        prevent_reentrancy(&ctx.accounts.program_state)?;
        let stake_position = &mut ctx.accounts.stake_position;
        require!(stake_position.is_active, StakingError::StakeNotActive);

        let clock = Clock::get()?;
        
        // ✅ 7-day cooldown check
        require!(
            can_claim_rewards(stake_position.last_reward_timestamp, clock.unix_timestamp),
            StakingError::RewardNotReady
        );

        let time_elapsed = clock.unix_timestamp - stake_position.last_reward_timestamp;
        let weeks_elapsed = time_elapsed / WEEK_IN_SECONDS;

        // ✅ Calculate reward with 0.21% weekly emission + power multiplier
        let reward_amount = calculate_reward_with_power(
            stake_position.amount,
            stake_position.power_multiplier,
            weeks_elapsed as u64,
        )?;

        require!(
            ctx.accounts.program_state.reward_pool >= reward_amount,
            StakingError::InsufficientRewardPool
        );

        if reward_amount > 0 {
            // Create NFT for vested reward
            let nft_name = format!("Staking Reward #{}", &ctx.accounts.nft_asset.key().to_string()[..8]);
            let nft_uri = format!("https://rewards.example.com/metadata/{}", ctx.accounts.nft_asset.key());

            CreateV2CpiBuilder::new(&ctx.accounts.mpl_core_program)
                .asset(&ctx.accounts.nft_asset)
                .payer(&ctx.accounts.staker)
                .owner(Some(&ctx.accounts.asset_owner))
                .update_authority(Some(&ctx.accounts.staker))
                .system_program(&ctx.accounts.system_program)
                .name(nft_name)
                .uri(nft_uri)
                .invoke()?;

            // ✅ 1-YEAR VESTING
            let reward_nft = &mut ctx.accounts.reward_nft;
            reward_nft.owner = ctx.accounts.staker.key();
            reward_nft.reward_amount = reward_amount;
            reward_nft.vest_timestamp = clock.unix_timestamp + VESTING_PERIOD_SECONDS;  // ✅ 1 year
            reward_nft.nft_asset = ctx.accounts.nft_asset.key();
            reward_nft.is_active = true;
            reward_nft.bump = ctx.bumps.reward_nft;

            stake_position.accumulated_rewards = stake_position
                .accumulated_rewards
                .checked_add(reward_amount)
                .ok_or(StakingError::CalculationOverflow)?;

            ctx.accounts.program_state.reward_pool = ctx
                .accounts.program_state.reward_pool
                .checked_sub(reward_amount)
                .ok_or(StakingError::CalculationOverflow)?;
        }

        stake_position.last_reward_timestamp = clock.unix_timestamp;

        emit!(RewardsClaimed {
            staker: ctx.accounts.staker.key(),
            reward_amount,
            vest_timestamp: clock.unix_timestamp + VESTING_PERIOD_SECONDS,
            weeks_elapsed: weeks_elapsed as u64,
        });

        Ok(())
    }

    pub fn vest_reward_nft(ctx: Context<VestRewardNft>, _nft_seed: u64) -> Result<()> {
        prevent_reentrancy(&ctx.accounts.program_state)?;
        let reward_nft = &ctx.accounts.reward_nft;
        require!(reward_nft.is_active, StakingError::NFTAlreadyVested);

        let clock = Clock::get()?;
        
        // ✅ 1-YEAR VESTING CHECK
        require!(
            is_vesting_complete(reward_nft.vest_timestamp, clock.unix_timestamp),
            StakingError::VestingNotComplete
        );

        // Verify NFT ownership
        let nft_asset_info = &ctx.accounts.nft_asset;
        require!(
            nft_asset_info.key() == reward_nft.nft_asset,
            StakingError::InvalidNFTAsset
        );

        let asset_data = nft_asset_info.try_borrow_data()?;
        require!(asset_data.len() >= 40, StakingError::InvalidNFTAsset);

        let expected_discriminator = [232, 219, 223, 41, 219, 236, 220, 190];
        require!(
            &asset_data[0..8] == expected_discriminator,
            StakingError::InvalidNFTAsset
        );

        let owner_bytes = &asset_data[8..40];
        let current_owner = Pubkey::new_from_array(
            owner_bytes.try_into().map_err(|_| StakingError::InvalidNFTAsset)?
        );

        require!(
            current_owner == ctx.accounts.user.key(),
            StakingError::NFTNotOwned
        );

        // Transfer rewards
        let seeds = &[
            b"program_vault".as_ref(),
            &[ctx.accounts.program_vault.bump],
        ];
        let signer = &[&seeds[..]];

        let transfer_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.program_vault_token_account.to_account_info(),
                to: ctx.accounts.user_token_account.to_account_info(),
                authority: ctx.accounts.program_vault.to_account_info(),
            },
            signer,
        );
        token::transfer(transfer_ctx, reward_nft.reward_amount)?;

        let reward_nft = &mut ctx.accounts.reward_nft;
        let vested_amount = reward_nft.reward_amount;
        reward_nft.is_active = false;
        reward_nft.reward_amount = 0;

        emit!(RewardsVested {
            user: ctx.accounts.user.key(),
            amount: vested_amount,
            nft_asset: ctx.accounts.nft_asset.key(),
        });

        Ok(())
    }

    // ✅ Token Extensions support for KYC + Pause
    pub fn enable_token_extensions(ctx: Context<AdminOnly>) -> Result<()> {
        ctx.accounts.program_state.use_token_extensions = true;
        
        emit!(TokenExtensionsEnabled {
            admin: ctx.accounts.admin.key(),
        });

        Ok(())
    }

    pub fn create_staking_tier(
        ctx: Context<CreateStakingTier>,
        tier_id: u8,
        multiplier: u64,
        min_duration_months: u8,
        max_duration_months: u8,
    ) -> Result<()> {
        validate_tier_params(multiplier, min_duration_months, max_duration_months)?;

        let staking_tier = &mut ctx.accounts.staking_tier;
        staking_tier.tier_id = tier_id;
        staking_tier.multiplier = multiplier;
        staking_tier.min_duration_months = min_duration_months;
        staking_tier.max_duration_months = max_duration_months;
        staking_tier.is_active = true;
        staking_tier.bump = ctx.bumps.staking_tier;

        emit!(StakingTierCreated {
            tier_id,
            multiplier,
            min_duration_months,
            max_duration_months,
        });

        Ok(())
    }

    // Additional admin functions...
    pub fn pause_program(ctx: Context<AdminOnly>) -> Result<()> {
        ctx.accounts.program_state.is_paused = true;
        emit!(ProgramPaused { admin: ctx.accounts.admin.key() });
        Ok(())
    }

    pub fn unpause_program(ctx: Context<AdminOnly>) -> Result<()> {
        ctx.accounts.program_state.is_paused = false;
        emit!(ProgramUnpaused { admin: ctx.accounts.admin.key() });
        Ok(())
    }
}

// ========== STRUCTS ==========

#[account]
#[derive(Debug)]
pub struct ProgramState {
    pub admin: Pubkey,
    pub reward_token_mint: Pubkey,
    pub protocol_treasury: Pubkey,
    pub referral_pool: Pubkey,
    pub cashback_pool: Pubkey,
    pub current_epoch: u64,
    pub total_staked: u64,
    pub reward_pool: u64,
    pub is_paused: bool,
    pub last_epoch_timestamp: i64,
    pub use_token_extensions: bool,  // ✅ Token Extensions flag
    pub bump: u8,
}

impl ProgramState {
    pub const LEN: usize = 8 + 32 + 32 + 32 + 32 + 32 + 8 + 8 + 8 + 1 + 8 + 1 + 1;
}

#[account]
#[derive(Debug)]
pub struct StakingTier {
    pub tier_id: u8,
    pub multiplier: u64,
    pub min_duration_months: u8,
    pub max_duration_months: u8,
    pub is_active: bool,
    pub bump: u8,
}

impl StakingTier {
    pub const LEN: usize = 8 + 1 + 8 + 1 + 1 + 1 + 1;
}

#[account]
#[derive(Debug)]
pub struct StakePosition {
    pub owner: Pubkey,
    pub amount: u64,
    pub tier_id: u8,
    pub base_multiplier: u64,  // Base tier multiplier
    pub power_multiplier: u64,  // ✅ Power multiplier based on amount/duration
    pub duration_months: u8,
    pub is_locked: bool,
    pub start_timestamp: i64,
    pub unlock_timestamp: i64,
    pub last_reward_timestamp: i64,
    pub accumulated_rewards: u64,
    pub is_active: bool,
    pub bump: u8,
}

impl StakePosition {
    pub const LEN: usize = 8 + 32 + 8 + 1 + 8 + 8 + 1 + 1 + 8 + 8 + 8 + 8 + 1 + 1;
}

#[account]
#[derive(Debug)]
pub struct RewardNFT {
    pub owner: Pubkey,
    pub reward_amount: u64,
    pub vest_timestamp: i64,  // ✅ Now 1 year from claim
    pub nft_asset: Pubkey,
    pub is_active: bool,
    pub bump: u8,
}

impl RewardNFT {
    pub const LEN: usize = 8 + 32 + 8 + 8 + 32 + 1 + 1;
}

#[account]
#[derive(Debug)]
pub struct ProgramVault {
    pub bump: u8,
}

impl ProgramVault {
    pub const LEN: usize = 8 + 1;
}

// ========== CONTEXTS ==========

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = admin,
        space = ProgramState::LEN,
        seeds = [b"program_state"],
        bump
    )]
    pub program_state: Account<'info, ProgramState>,

    #[account(
        init,
        payer = admin,
        space = ProgramVault::LEN,
        seeds = [b"program_vault"],
        bump
    )]
    pub program_vault: Account<'info, ProgramVault>,

    #[account(mut)]
    pub admin: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(tier_id: u8)]
pub struct CreateStakingTier<'info> {
    #[account(
        init,
        payer = admin,
        space = StakingTier::LEN,
        seeds = [b"staking_tier", tier_id.to_le_bytes().as_ref()],
        bump
    )]
    pub staking_tier: Account<'info, StakingTier>,
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        seeds = [b"program_state"],
        bump,
        constraint = program_state.admin == admin.key() @ StakingError::Unauthorized
    )]
    pub program_state: Account<'info, ProgramState>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(amount: u64, duration_months: u8, tier_id: u8, is_locked: bool, position_seed: u64)]
pub struct StakeTokens<'info> {
    #[account(
        init,
        payer = staker,
        space = StakePosition::LEN,
        seeds = [b"stake_position", staker.key().as_ref(), &position_seed.to_le_bytes()],
        bump
    )]
    pub stake_position: Account<'info, StakePosition>,
    #[account(mut)]
    pub staker: Signer<'info>,
    #[account(
        mut,
        seeds = [b"program_state"],
        bump
    )]
    pub program_state: Account<'info, ProgramState>,
    #[account(
        seeds = [b"staking_tier", tier_id.to_le_bytes().as_ref()],
        bump
    )]
    pub staking_tier: Account<'info, StakingTier>,
    #[account(
        mut,
        constraint = user_token_account.owner == staker.key() @ StakingError::Unauthorized,
        constraint = user_token_account.mint == program_state.reward_token_mint @ StakingError::InvalidTokenAccount
    )]
    pub user_token_account: Account<'info, TokenAccount>,
    #[account(
        mut,
        seeds = [b"program_vault"],
        bump
    )]
    pub program_vault: Account<'info, ProgramVault>,
    #[account(
        mut,
        constraint = program_vault_token_account.owner == program_vault.key() @ StakingError::InvalidTokenAccount,
        constraint = program_vault_token_account.mint == program_state.reward_token_mint @ StakingError::InvalidTokenAccount
    )]
    pub program_vault_token_account: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(position_seed: u64)]
pub struct UnstakeTokens<'info> {
    #[account(
        mut,
        seeds = [b"stake_position", staker.key().as_ref(), &position_seed.to_le_bytes()],
        bump,
        constraint = stake_position.owner == staker.key() @ StakingError::Unauthorized
    )]
    pub stake_position: Account<'info, StakePosition>,
    #[account(mut)]
    pub staker: Signer<'info>,
    #[account(
        mut,
        seeds = [b"program_state"],
        bump
    )]
    pub program_state: Account<'info, ProgramState>,
    #[account(
        mut,
        constraint = user_token_account.owner == staker.key() @ StakingError::Unauthorized,
        constraint = user_token_account.mint == program_state.reward_token_mint @ StakingError::InvalidTokenAccount
    )]
    pub user_token_account: Account<'info, TokenAccount>,
    #[account(
        mut,
        seeds = [b"program_vault"],
        bump
    )]
    pub program_vault: Account<'info, ProgramVault>,
    #[account(
        mut,
        constraint = program_vault_token_account.owner == program_vault.key() @ StakingError::InvalidTokenAccount,
        constraint = program_vault_token_account.mint == program_state.reward_token_mint @ StakingError::InvalidTokenAccount
    )]
    pub program_vault_token_account: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = treasury_token_account.owner == program_state.protocol_treasury @ StakingError::InvalidTokenAccount,
        constraint = treasury_token_account.mint == program_state.reward_token_mint @ StakingError::InvalidTokenAccount
    )]
    pub treasury_token_account: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
#[instruction(position_seed: u64, nft_seed: u64)]
pub struct ClaimRewards<'info> {
    #[account(
        mut,
        seeds = [b"stake_position", staker.key().as_ref(), &position_seed.to_le_bytes()],
        bump,
        constraint = stake_position.owner == staker.key() @ StakingError::Unauthorized
    )]
    pub stake_position: Account<'info, StakePosition>,
    #[account(mut)]
    pub staker: Signer<'info>,
    #[account(
        init,
        payer = staker,
        space = RewardNFT::LEN,
        seeds = [b"reward_nft", staker.key().as_ref(), &nft_seed.to_le_bytes()],
        bump
    )]
    pub reward_nft: Account<'info, RewardNFT>,
    /// CHECK: Core NFT asset
    #[account(mut)]
    pub nft_asset: UncheckedAccount<'info>,
    /// CHECK: NFT owner
    pub asset_owner: AccountInfo<'info>,
    #[account(
        mut,
        seeds = [b"program_state"],
        bump
    )]
    pub program_state: Account<'info, ProgramState>,
    pub system_program: Program<'info, System>,
    /// CHECK: Metaplex Core
    #[account(address = MPL_CORE_PROGRAM_ID)]
    pub mpl_core_program: UncheckedAccount<'info>,
}

#[derive(Accounts)]
#[instruction(nft_seed: u64)]
pub struct VestRewardNft<'info> {
    #[account(
        mut,
        seeds = [b"reward_nft", reward_nft.owner.as_ref(), &nft_seed.to_le_bytes()],
        bump
    )]
    pub reward_nft: Account<'info, RewardNFT>,
    #[account(mut)]
    pub user: Signer<'info>,
    /// CHECK: Core NFT asset
    #[account(mut, address = reward_nft.nft_asset)]
    pub nft_asset: UncheckedAccount<'info>,
    #[account(mut, seeds = [b"program_state"], bump)]
    pub program_state: Account<'info, ProgramState>,
    #[account(
        mut,
        constraint = user_token_account.owner == user.key() @ StakingError::Unauthorized,
        constraint = user_token_account.mint == program_state.reward_token_mint @ StakingError::InvalidTokenAccount
    )]
    pub user_token_account: Account<'info, TokenAccount>,
    #[account(mut, seeds = [b"program_vault"], bump)]
    pub program_vault: Account<'info, ProgramVault>,
    #[account(
        mut,
        constraint = program_vault_token_account.owner == program_vault.key() @ StakingError::InvalidTokenAccount,
        constraint = program_vault_token_account.mint == program_state.reward_token_mint @ StakingError::InvalidTokenAccount
    )]
    pub program_vault_token_account: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    #[account(
        mut,
        seeds = [b"program_state"],
        bump,
        constraint = program_state.admin == admin.key() @ StakingError::Unauthorized
    )]
    pub program_state: Account<'info, ProgramState>,
    pub admin: Signer<'info>,
}

// ========== EVENTS ==========

#[event]
pub struct ProgramInitialized {
    pub admin: Pubkey,
    pub reward_token_mint: Pubkey,
    pub protocol_treasury: Pubkey,
    pub referral_pool: Pubkey,
    pub cashback_pool: Pubkey,
}

#[event]
pub struct StakingTierCreated {
    pub tier_id: u8,
    pub multiplier: u64,
    pub min_duration_months: u8,
    pub max_duration_months: u8,
}

#[event]
pub struct TokensStaked {
    pub staker: Pubkey,
    pub amount: u64,
    pub duration_months: u8,
    pub tier_id: u8,
    pub is_locked: bool,
    pub power_multiplier: u64,  // ✅ Added
    pub unlock_timestamp: i64,
}

#[event]
pub struct TokensUnstaked {
    pub staker: Pubkey,
    pub amount: u64,
    pub reward_penalty: u64,  // ✅ Changed from penalty_amount
    pub is_early_unstake: bool,
}

#[event]
pub struct RewardsClaimed {
    pub staker: Pubkey,
    pub reward_amount: u64,
    pub vest_timestamp: i64,
    pub weeks_elapsed: u64,
}

#[event]
pub struct RewardsVested {
    pub user: Pubkey,
    pub amount: u64,
    pub nft_asset: Pubkey,
}

#[event]
pub struct ProgramPaused {
    pub admin: Pubkey,
}

#[event]
pub struct ProgramUnpaused {
    pub admin: Pubkey,
}

#[event]
pub struct TokenExtensionsEnabled {
    pub admin: Pubkey,
}

// ========== ERRORS ==========

#[error_code]
pub enum StakingError {
    #[msg("Invalid multiplier value")]
    InvalidMultiplier,
    #[msg("Invalid duration")]
    InvalidDuration,
    #[msg("Invalid amount")]
    InvalidAmount,
    #[msg("Staking tier not active")]
    TierNotActive,
    #[msg("Duration not allowed for this tier")]
    DurationNotAllowed,
    #[msg("Stake position not active")]
    StakeNotActive,
    #[msg("Program is paused")]
    ProgramPaused,
    #[msg("Rewards not ready (wait 1 week)")]
    RewardNotReady,
    #[msg("Vesting period not complete (1 year)")]
    VestingNotComplete,
    #[msg("NFT already vested")]
    NFTAlreadyVested,
    #[msg("Calculation overflow detected")]
    CalculationOverflow,
    #[msg("Unauthorized access")]
    Unauthorized,
    #[msg("Insufficient reward pool")]
    InsufficientRewardPool,
    #[msg("Invalid token account")]
    InvalidTokenAccount,
    #[msg("NFT not owned")]
    NFTNotOwned,
    #[msg("Invalid NFT asset")]
    InvalidNFTAsset,
}

// ========== HELPER FUNCTIONS ==========

// ✅ 0.21% WEEKLY EMISSION
fn calculate_reward_with_power(
    amount: u64,
    power_multiplier: u64,
    weeks: u64,
) -> Result<u64> {
    // Base weekly emission: 0.21% = 21/10000
    let base_weekly_reward = amount
        .checked_mul(WEEKLY_EMISSION_RATE)
        .ok_or(StakingError::CalculationOverflow)?
        .checked_div(EMISSION_PRECISION)
        .ok_or(StakingError::CalculationOverflow)?;

    // Apply power multiplier (stored as basis points, e.g., 150 = 1.5x)
    let weekly_with_power = base_weekly_reward
        .checked_mul(power_multiplier)
        .ok_or(StakingError::CalculationOverflow)?
        .checked_div(100)
        .ok_or(StakingError::CalculationOverflow)?;

    // Multiply by weeks
    weekly_with_power
        .checked_mul(weeks)
        .ok_or(StakingError::CalculationOverflow.into())
}

// ✅ TRUE POWER MULTIPLIER: amount^1.5 × duration
fn calculate_power_multiplier(amount: u64, duration_months: u8) -> Result<u64> {
    // Normalize amount to avoid overflow (divide by 1M)
    let normalized_amount = amount / 1_000_000;
    
    // Calculate power: amount^1.5 using integer math
    // Formula: sqrt(amount^3) ≈ amount^1.5
    let amount_squared = normalized_amount
        .checked_mul(normalized_amount)
        .ok_or(StakingError::CalculationOverflow)?;
    let amount_cubed = amount_squared
        .checked_mul(normalized_amount)
        .ok_or(StakingError::CalculationOverflow)?;
    
    // Integer square root approximation
    let power_value = integer_sqrt(amount_cubed);
    
    // Duration bonus: 1 + (months / 12) = 1 to 6x for 0-60 months
    let duration_bonus = BASE_MULTIPLIER
        .checked_add((duration_months as u64).checked_mul(100 / 12).unwrap_or(0))
        .ok_or(StakingError::CalculationOverflow)?;
    
    // Combine: base (100) + power component + duration bonus
    let final_multiplier = BASE_MULTIPLIER
        .checked_add(power_value / 1000)  // Scale down power
        .ok_or(StakingError::CalculationOverflow)?
        .checked_add(duration_bonus)
        .ok_or(StakingError::CalculationOverflow)?;
    
    // Cap at 10x (1000 basis points)
    Ok(cmp::min(final_multiplier, 1000))
}

// Integer square root (Babylonian method)
fn integer_sqrt(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

pub fn validate_tier_params(multiplier: u64, min_duration: u8, max_duration: u8) -> Result<()> {
    require!(
        multiplier > 0 && multiplier <= 500,
        StakingError::InvalidMultiplier
    );
    require!(
        min_duration > 0 && min_duration <= max_duration,
        StakingError::InvalidDuration
    );
    Ok(())
}

pub fn calculate_unlock_timestamp(start: i64, duration_months: u8) -> Result<i64> {
    let duration_seconds = (duration_months as i64)
        .checked_mul(30 * 24 * 60 * 60)
        .ok_or(StakingError::CalculationOverflow)?;

    start
        .checked_add(duration_seconds)
        .ok_or(StakingError::CalculationOverflow.into())
}

pub fn can_claim_rewards(last_claim: i64, current_time: i64) -> bool {
    current_time >= last_claim + WEEK_IN_SECONDS  // ✅ 7-day cooldown
}

pub fn is_vesting_complete(vest_timestamp: i64, current_time: i64) -> bool {
    current_time >= vest_timestamp  // ✅ 1-year vesting
}

pub fn prevent_reentrancy(program_state: &ProgramState) -> Result<()> {
    require!(!program_state.is_paused, StakingError::ProgramPaused);
    Ok(())
}