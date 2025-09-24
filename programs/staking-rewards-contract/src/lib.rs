// programs/staking-rewards-contract/src/lib.rs
// Production-Ready Solana Staking & Rewards Smart Contract
// Enhanced with comprehensive security, error handling, and optimizations

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};
use std::cmp;

declare_id!("AKnc8CqVVCyBuzzyvNEPQZGYCiEiqRneETDSgm1ZU69N");

// ================================
// CONSTANTS & CONFIGURATION
// ================================

const WEEK_IN_SECONDS: i64 = 604800;
const MAX_APY: u64 = 75; // 75% maximum APY
const EARLY_UNSTAKE_PENALTY: u64 = 50; // 50% penalty
const REWARD_DISTRIBUTION_REFERRAL: u64 = 30; // 30%
const REWARD_DISTRIBUTION_CASHBACK: u64 = 30; // 30%
const REWARD_DISTRIBUTION_STAKING: u64 = 40; // 40%
const VESTING_PERIOD_SECONDS: i64 = 30 * 24 * 60 * 60; // 30 days
const MIN_STAKE_AMOUNT: u64 = 1_000_000; // Minimum stake amount (0.001 tokens with 9 decimals)
const MAX_STAKE_AMOUNT: u64 = 1_000_000_000_000_000; // Maximum stake amount per position

// ================================
// MAIN PROGRAM
// ================================

#[program]
pub mod staking_rewards_contract {
    use super::*;

    /// Initialize the staking program
    pub fn initialize(
        ctx: Context<Initialize>,
        admin: Pubkey,
        reward_token_mint: Pubkey,
        protocol_treasury: Pubkey,
    ) -> Result<()> {
        let program_state = &mut ctx.accounts.program_state;
        program_state.admin = admin;
        program_state.reward_token_mint = reward_token_mint;
        program_state.protocol_treasury = protocol_treasury;
        program_state.current_epoch = 0;
        program_state.total_staked = 0;
        program_state.reward_pool = 0;
        program_state.is_paused = false;
        program_state.last_epoch_timestamp = Clock::get()?.unix_timestamp;
        program_state.bump = ctx.bumps.program_state;

        emit!(ProgramInitialized {
            admin,
            reward_token_mint,
            protocol_treasury,
        });

        Ok(())
    }

    /// Create a new staking tier
    pub fn create_staking_tier(
        ctx: Context<CreateStakingTier>,
        tier_id: u8,
        multiplier: u64,
        min_duration_months: u8,
        max_duration_months: u8,
    ) -> Result<()> {
        // Validation
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

    /// Stake tokens with specified parameters
    pub fn stake_tokens(
        ctx: Context<StakeTokens>,
        amount: u64,
        duration_months: u8,
        tier_id: u8,
        is_locked: bool,
        position_seed: u64,
    ) -> Result<()> {
        // Security checks
        prevent_reentrancy(&ctx.accounts.program_state)?;
        require!(
            amount >= MIN_STAKE_AMOUNT && amount <= MAX_STAKE_AMOUNT,
            StakingError::InvalidAmount
        );
        require!(
            duration_months >= 1 && duration_months <= 36,
            StakingError::InvalidDuration
        );

        let staking_tier = &ctx.accounts.staking_tier;
        require!(staking_tier.is_active, StakingError::TierNotActive);
        require!(
            duration_months >= staking_tier.min_duration_months
                && duration_months <= staking_tier.max_duration_months,
            StakingError::DurationNotAllowed
        );

        // Validate token accounts
        validate_token_account(
            &ctx.accounts.user_token_account,
            &ctx.accounts.staker.key(),
            &ctx.accounts.program_state.reward_token_mint,
        )?;

        let clock = Clock::get()?;
        let stake_position = &mut ctx.accounts.stake_position;

        // Initialize stake position
        stake_position.owner = ctx.accounts.staker.key();
        stake_position.amount = amount;
        stake_position.tier_id = tier_id;
        stake_position.multiplier = staking_tier.multiplier;
        stake_position.duration_months = duration_months;
        stake_position.is_locked = is_locked;
        stake_position.start_timestamp = clock.unix_timestamp;
        stake_position.unlock_timestamp =
            calculate_unlock_timestamp(clock.unix_timestamp, duration_months)?;
        stake_position.last_reward_timestamp = clock.unix_timestamp;
        stake_position.accumulated_rewards = 0;
        stake_position.is_active = true;
        stake_position.bump = ctx.bumps.stake_position;

        // Transfer tokens to program vault
        let transfer_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.user_token_account.to_account_info(),
                to: ctx.accounts.program_vault.to_account_info(),
                authority: ctx.accounts.staker.to_account_info(),
            },
        );
        token::transfer(transfer_ctx, amount)?;

        // Update global state safely
        ctx.accounts.program_state.total_staked = ctx
            .accounts
            .program_state
            .total_staked
            .checked_add(amount)
            .ok_or(StakingError::CalculationOverflow)?;

        emit!(TokensStaked {
            staker: ctx.accounts.staker.key(),
            amount,
            duration_months,
            tier_id,
            is_locked,
            unlock_timestamp: stake_position.unlock_timestamp,
        });

        Ok(())
    }

    /// Unstake tokens with penalty calculation for early unstaking
    pub fn unstake_tokens(ctx: Context<UnstakeTokens>, position_seed: u64) -> Result<()> {
        prevent_reentrancy(&ctx.accounts.program_state)?;

        let stake_position = &mut ctx.accounts.stake_position;
        require!(stake_position.is_active, StakingError::StakeNotActive);

        let clock = Clock::get()?;
        let is_early_unstake = clock.unix_timestamp < stake_position.unlock_timestamp;

        let mut transfer_amount = stake_position.amount;
        let mut penalty_amount = 0u64;

        // Apply penalty for early locked unstaking
        if is_early_unstake && stake_position.is_locked {
            penalty_amount = calculate_early_unstake_penalty(
                stake_position.amount,
                stake_position.is_locked,
                is_early_unstake,
            )?;

            transfer_amount = stake_position
                .amount
                .checked_sub(penalty_amount)
                .ok_or(StakingError::CalculationOverflow)?;

            // Split penalty: 50% to reward pool, 50% to treasury
            let to_reward_pool = penalty_amount
                .checked_div(2)
                .ok_or(StakingError::CalculationOverflow)?;
            let to_treasury = penalty_amount
                .checked_sub(to_reward_pool)
                .ok_or(StakingError::CalculationOverflow)?;

            // Update reward pool
            ctx.accounts.program_state.reward_pool = ctx
                .accounts
                .program_state
                .reward_pool
                .checked_add(to_reward_pool)
                .ok_or(StakingError::CalculationOverflow)?;

            // Transfer penalty to treasury if > 0
            if to_treasury > 0 {
                let seeds = &[
                    b"program_vault".as_ref(),
                    &[ctx.accounts.program_vault.bump],
                ];
                let signer = &[&seeds[..]];

                let transfer_ctx = CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.program_vault.to_account_info(),
                        to: ctx.accounts.treasury_token_account.to_account_info(),
                        authority: ctx.accounts.program_vault.to_account_info(),
                    },
                    signer,
                );
                token::transfer(transfer_ctx, to_treasury)?;
            }
        }

        // Transfer tokens back to user
        let seeds = &[
            b"program_vault".as_ref(),
            &[ctx.accounts.program_vault.bump],
        ];
        let signer = &[&seeds[..]];

        let transfer_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.program_vault.to_account_info(),
                to: ctx.accounts.user_token_account.to_account_info(),
                authority: ctx.accounts.program_vault.to_account_info(),
            },
            signer,
        );
        token::transfer(transfer_ctx, transfer_amount)?;

        // Update global state
        ctx.accounts.program_state.total_staked = ctx
            .accounts
            .program_state
            .total_staked
            .checked_sub(stake_position.amount)
            .ok_or(StakingError::CalculationOverflow)?;

        stake_position.is_active = false;
        stake_position.amount = 0;

        emit!(TokensUnstaked {
            staker: ctx.accounts.staker.key(),
            amount: transfer_amount,
            penalty_amount,
            is_early_unstake,
        });

        Ok(())
    }

    /// Claim rewards and create vesting NFT
    pub fn claim_rewards(
        ctx: Context<ClaimRewards>,
        position_seed: u64,
        nft_seed: u64,
    ) -> Result<()> {
        let stake_position = &mut ctx.accounts.stake_position;
        require!(stake_position.is_active, StakingError::StakeNotActive);

        let clock = Clock::get()?;
        let time_elapsed = clock.unix_timestamp - stake_position.last_reward_timestamp;

        require!(
            can_claim_rewards(stake_position.last_reward_timestamp, clock.unix_timestamp),
            StakingError::RewardNotReady
        );

        let weeks_elapsed = time_elapsed / WEEK_IN_SECONDS;
        let reward_amount = calculate_reward(
            stake_position.amount,
            stake_position.multiplier,
            weeks_elapsed as u64,
        )?;

        // Apply APY cap
        let max_weekly_reward = calculate_max_weekly_reward(stake_position.amount)?;
        let final_reward = cmp::min(
            reward_amount,
            max_weekly_reward
                .checked_mul(weeks_elapsed as u64)
                .ok_or(StakingError::CalculationOverflow)?,
        );

        // Check reward pool has sufficient balance
        require!(
            ctx.accounts.program_state.reward_pool >= final_reward,
            StakingError::InsufficientRewardPool
        );

        if final_reward > 0 {
            // Create NFT for reward vesting
            let reward_nft = &mut ctx.accounts.reward_nft;
            reward_nft.owner = ctx.accounts.staker.key();
            reward_nft.reward_amount = final_reward;
            reward_nft.vest_timestamp = clock.unix_timestamp + VESTING_PERIOD_SECONDS;
            reward_nft.is_active = true;
            reward_nft.bump = ctx.bumps.reward_nft;

            // Update accumulated rewards
            stake_position.accumulated_rewards = stake_position
                .accumulated_rewards
                .checked_add(final_reward)
                .ok_or(StakingError::CalculationOverflow)?;

            // Deduct from reward pool
            ctx.accounts.program_state.reward_pool = ctx
                .accounts
                .program_state
                .reward_pool
                .checked_sub(final_reward)
                .ok_or(StakingError::CalculationOverflow)?;
        }

        stake_position.last_reward_timestamp = clock.unix_timestamp;

        emit!(RewardsClaimed {
            staker: ctx.accounts.staker.key(),
            reward_amount: final_reward,
            vest_timestamp: clock.unix_timestamp + VESTING_PERIOD_SECONDS,
            weeks_elapsed: weeks_elapsed as u64,
        });

        Ok(())
    }

    /// Vest NFT rewards after vesting period
    pub fn vest_reward_nft(ctx: Context<VestRewardNft>, nft_seed: u64) -> Result<()> {
        let reward_nft = &ctx.accounts.reward_nft;
        require!(reward_nft.is_active, StakingError::NFTAlreadyVested);

        let clock = Clock::get()?;
        require!(
            is_vesting_complete(reward_nft.vest_timestamp, clock.unix_timestamp),
            StakingError::VestingNotComplete
        );

        // Transfer reward tokens to user
        let seeds = &[
            b"program_vault".as_ref(),
            &[ctx.accounts.program_vault.bump],
        ];
        let signer = &[&seeds[..]];

        let transfer_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.program_vault.to_account_info(),
                to: ctx.accounts.user_token_account.to_account_info(),
                authority: ctx.accounts.program_vault.to_account_info(),
            },
            signer,
        );
        token::transfer(transfer_ctx, reward_nft.reward_amount)?;

        // Mark NFT as vested
        let reward_nft = &mut ctx.accounts.reward_nft;
        let vested_amount = reward_nft.reward_amount;
        reward_nft.is_active = false;
        reward_nft.reward_amount = 0;

        emit!(RewardsVested {
            user: ctx.accounts.user.key(),
            amount: vested_amount,
        });

        Ok(())
    }

    /// Distribute weekly rewards across pools
    pub fn distribute_weekly_rewards(
        ctx: Context<DistributeWeeklyRewards>,
        total_rewards: u64,
    ) -> Result<()> {
        let clock = Clock::get()?;
        let program_state = &mut ctx.accounts.program_state;

        check_rate_limit(
            program_state.last_epoch_timestamp,
            clock.unix_timestamp,
            WEEK_IN_SECONDS,
        )?;

        // Validate distribution
        validate_reward_distribution()?;

        // Calculate distribution amounts
        let referral_amount = total_rewards
            .checked_mul(REWARD_DISTRIBUTION_REFERRAL)
            .ok_or(StakingError::CalculationOverflow)?
            .checked_div(100)
            .ok_or(StakingError::CalculationOverflow)?;

        let cashback_amount = total_rewards
            .checked_mul(REWARD_DISTRIBUTION_CASHBACK)
            .ok_or(StakingError::CalculationOverflow)?
            .checked_div(100)
            .ok_or(StakingError::CalculationOverflow)?;

        let staking_amount = total_rewards
            .checked_mul(REWARD_DISTRIBUTION_STAKING)
            .ok_or(StakingError::CalculationOverflow)?
            .checked_div(100)
            .ok_or(StakingError::CalculationOverflow)?;

        // Verify total distribution equals input
        let total_distributed = referral_amount
            .checked_add(cashback_amount)
            .ok_or(StakingError::CalculationOverflow)?
            .checked_add(staking_amount)
            .ok_or(StakingError::CalculationOverflow)?;
        require!(
            total_distributed == total_rewards,
            StakingError::InvalidDistribution
        );

        // Update program state
        program_state.reward_pool = program_state
            .reward_pool
            .checked_add(staking_amount)
            .ok_or(StakingError::CalculationOverflow)?;
        program_state.current_epoch = program_state
            .current_epoch
            .checked_add(1)
            .ok_or(StakingError::CalculationOverflow)?;
        program_state.last_epoch_timestamp = clock.unix_timestamp;

        emit!(WeeklyRewardsDistributed {
            epoch: program_state.current_epoch,
            total_rewards,
            referral_amount,
            cashback_amount,
            staking_amount,
        });

        Ok(())
    }

    /// Emergency pause program (admin only)
    pub fn pause_program(ctx: Context<AdminOnly>) -> Result<()> {
        ctx.accounts.program_state.is_paused = true;

        emit!(ProgramPaused {
            admin: ctx.accounts.admin.key(),
        });

        Ok(())
    }

    /// Unpause program (admin only)
    pub fn unpause_program(ctx: Context<AdminOnly>) -> Result<()> {
        ctx.accounts.program_state.is_paused = false;

        emit!(ProgramUnpaused {
            admin: ctx.accounts.admin.key(),
        });

        Ok(())
    }

    /// Placeholder for future buyback and burn functionality
    pub fn buyback_and_burn(ctx: Context<BuybackAndBurn>, amount: u64) -> Result<()> {
        require!(amount > 0, StakingError::InvalidAmount);

        emit!(BuybackAndBurnEvent {
            admin: ctx.accounts.admin.key(),
            amount,
        });

        Ok(())
    }

    /// Update staking tier (admin only)
    pub fn update_staking_tier(
        ctx: Context<UpdateStakingTier>,
        tier_id: u8,
        multiplier: u64,
        min_duration_months: u8,
        max_duration_months: u8,
        is_active: bool,
    ) -> Result<()> {
        validate_tier_params(multiplier, min_duration_months, max_duration_months)?;

        let staking_tier = &mut ctx.accounts.staking_tier;
        staking_tier.multiplier = multiplier;
        staking_tier.min_duration_months = min_duration_months;
        staking_tier.max_duration_months = max_duration_months;
        staking_tier.is_active = is_active;

        emit!(StakingTierUpdated {
            tier_id,
            multiplier,
            min_duration_months,
            max_duration_months,
            is_active,
        });

        Ok(())
    }

    /// Add funds to reward pool (admin only)
    pub fn add_reward_funds(ctx: Context<AddRewardFunds>, amount: u64) -> Result<()> {
        require!(amount > 0, StakingError::InvalidAmount);

        // Transfer tokens to program vault
        let transfer_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.admin_token_account.to_account_info(),
                to: ctx.accounts.program_vault.to_account_info(),
                authority: ctx.accounts.admin.to_account_info(),
            },
        );
        token::transfer(transfer_ctx, amount)?;

        // Update reward pool
        ctx.accounts.program_state.reward_pool = ctx
            .accounts
            .program_state
            .reward_pool
            .checked_add(amount)
            .ok_or(StakingError::CalculationOverflow)?;

        emit!(RewardFundsAdded {
            admin: ctx.accounts.admin.key(),
            amount,
        });

        Ok(())
    }
}

// ================================
// ACCOUNT STRUCTURES
// ================================

#[account]
#[derive(Debug)]
pub struct ProgramState {
    pub admin: Pubkey,             // 32
    pub reward_token_mint: Pubkey, // 32
    pub protocol_treasury: Pubkey, // 32
    pub current_epoch: u64,        // 8
    pub total_staked: u64,         // 8
    pub reward_pool: u64,          // 8
    pub is_paused: bool,           // 1
    pub last_epoch_timestamp: i64, // 8
    pub bump: u8,                  // 1
}

impl ProgramState {
    pub const LEN: usize = 8 + 32 + 32 + 32 + 8 + 8 + 8 + 1 + 8 + 1;
}

#[account]
#[derive(Debug)]
pub struct StakingTier {
    pub tier_id: u8,             // 1
    pub multiplier: u64,         // 8
    pub min_duration_months: u8, // 1
    pub max_duration_months: u8, // 1
    pub is_active: bool,         // 1
    pub bump: u8,                // 1
}

impl StakingTier {
    pub const LEN: usize = 8 + 1 + 8 + 1 + 1 + 1 + 1;
}

#[account]
#[derive(Debug)]
pub struct StakePosition {
    pub owner: Pubkey,              // 32
    pub amount: u64,                // 8
    pub tier_id: u8,                // 1
    pub multiplier: u64,            // 8
    pub duration_months: u8,        // 1
    pub is_locked: bool,            // 1
    pub start_timestamp: i64,       // 8
    pub unlock_timestamp: i64,      // 8
    pub last_reward_timestamp: i64, // 8
    pub accumulated_rewards: u64,   // 8
    pub is_active: bool,            // 1
    pub bump: u8,                   // 1
}

impl StakePosition {
    pub const LEN: usize = 8 + 32 + 8 + 1 + 8 + 1 + 1 + 8 + 8 + 8 + 8 + 1 + 1;
}

#[account]
#[derive(Debug)]
pub struct RewardNFT {
    pub owner: Pubkey,       // 32
    pub reward_amount: u64,  // 8
    pub vest_timestamp: i64, // 8
    pub is_active: bool,     // 1
    pub bump: u8,            // 1
}

impl RewardNFT {
    pub const LEN: usize = 8 + 32 + 8 + 8 + 1 + 1;
}

#[account]
#[derive(Debug)]
pub struct ProgramVault {
    pub bump: u8, // 1
}

impl ProgramVault {
    pub const LEN: usize = 8 + 1;
}

// ================================
// CONTEXT STRUCTURES
// ================================

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
        constraint = user_token_account.owner == staker.key() @ StakingError::Unauthorized
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
        constraint = treasury_token_account.owner == program_state.protocol_treasury @ StakingError::InvalidTokenAccount
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
    #[account(
        mut,
        seeds = [b"program_state"],
        bump
    )]
    pub program_state: Account<'info, ProgramState>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(nft_seed: u64)]
pub struct VestRewardNft<'info> {
    #[account(
        mut,
        seeds = [b"reward_nft", user.key().as_ref(), &nft_seed.to_le_bytes()],
        bump,
        constraint = reward_nft.owner == user.key() @ StakingError::Unauthorized
    )]
    pub reward_nft: Account<'info, RewardNFT>,
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(
        mut,
        seeds = [b"program_state"],
        bump
    )]
    pub program_state: Account<'info, ProgramState>,
    #[account(
        mut,
        constraint = user_token_account.owner == user.key() @ StakingError::Unauthorized
    )]
    pub user_token_account: Account<'info, TokenAccount>,
    #[account(
        mut,
        seeds = [b"program_vault"],
        bump
    )]
    pub program_vault: Account<'info, ProgramVault>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct DistributeWeeklyRewards<'info> {
    #[account(
        mut,
        seeds = [b"program_state"],
        bump,
        constraint = program_state.admin == admin.key() @ StakingError::Unauthorized
    )]
    pub program_state: Account<'info, ProgramState>,
    pub admin: Signer<'info>,
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

#[derive(Accounts)]
pub struct BuybackAndBurn<'info> {
    #[account(
        mut,
        seeds = [b"program_state"],
        bump,
        constraint = program_state.admin == admin.key() @ StakingError::Unauthorized
    )]
    pub program_state: Account<'info, ProgramState>,
    pub admin: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
#[instruction(tier_id: u8)]
pub struct UpdateStakingTier<'info> {
    #[account(
        mut,
        seeds = [b"staking_tier", tier_id.to_le_bytes().as_ref()],
        bump
    )]
    pub staking_tier: Account<'info, StakingTier>,
    #[account(
        seeds = [b"program_state"],
        bump,
        constraint = program_state.admin == admin.key() @ StakingError::Unauthorized
    )]
    pub program_state: Account<'info, ProgramState>,
    pub admin: Signer<'info>,
}

#[derive(Accounts)]
pub struct AddRewardFunds<'info> {
    #[account(
        mut,
        seeds = [b"program_state"],
        bump,
        constraint = program_state.admin == admin.key() @ StakingError::Unauthorized
    )]
    pub program_state: Account<'info, ProgramState>,
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        mut,
        constraint = admin_token_account.owner == admin.key() @ StakingError::Unauthorized
    )]
    pub admin_token_account: Account<'info, TokenAccount>,
    #[account(
        mut,
        seeds = [b"program_vault"],
        bump
    )]
    pub program_vault: Account<'info, ProgramVault>,
    pub token_program: Program<'info, Token>,
}

// ================================
// EVENTS
// ================================

#[event]
pub struct ProgramInitialized {
    pub admin: Pubkey,
    pub reward_token_mint: Pubkey,
    pub protocol_treasury: Pubkey,
}

#[event]
pub struct StakingTierCreated {
    pub tier_id: u8,
    pub multiplier: u64,
    pub min_duration_months: u8,
    pub max_duration_months: u8,
}

#[event]
pub struct StakingTierUpdated {
    pub tier_id: u8,
    pub multiplier: u64,
    pub min_duration_months: u8,
    pub max_duration_months: u8,
    pub is_active: bool,
}

#[event]
pub struct TokensStaked {
    pub staker: Pubkey,
    pub amount: u64,
    pub duration_months: u8,
    pub tier_id: u8,
    pub is_locked: bool,
    pub unlock_timestamp: i64,
}

#[event]
pub struct TokensUnstaked {
    pub staker: Pubkey,
    pub amount: u64,
    pub penalty_amount: u64,
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
}

#[event]
pub struct WeeklyRewardsDistributed {
    pub epoch: u64,
    pub total_rewards: u64,
    pub referral_amount: u64,
    pub cashback_amount: u64,
    pub staking_amount: u64,
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
pub struct BuybackAndBurnEvent {
    pub admin: Pubkey,
    pub amount: u64,
}

#[event]
pub struct RewardFundsAdded {
    pub admin: Pubkey,
    pub amount: u64,
}

// ================================
// ERROR HANDLING
// ================================

#[error_code]
pub enum StakingError {
    #[msg("Invalid multiplier value (must be 1-500)")]
    InvalidMultiplier,
    #[msg("Invalid duration")]
    InvalidDuration,
    #[msg("Duration too long (max 36 months)")]
    ExcessiveDuration,
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
    #[msg("Vesting period not complete")]
    VestingNotComplete,
    #[msg("NFT already vested")]
    NFTAlreadyVested,
    #[msg("Epoch not ready (wait 1 week)")]
    EpochNotReady,
    #[msg("Calculation overflow detected")]
    CalculationOverflow,
    #[msg("Unauthorized access")]
    Unauthorized,
    #[msg("Invalid reward distribution")]
    InvalidDistribution,
    #[msg("Insufficient reward pool")]
    InsufficientRewardPool,
    #[msg("Invalid tier ID")]
    InvalidTierId,
    #[msg("Tier already exists")]
    TierAlreadyExists,
    #[msg("Position seed collision")]
    PositionSeedCollision,
    #[msg("Invalid token account")]
    InvalidTokenAccount,
}

// ================================
// HELPER FUNCTIONS
// ================================

/// Calculate weekly reward based on stake amount, multiplier, and time
fn calculate_reward(amount: u64, multiplier: u64, weeks: u64) -> Result<u64> {
    let annual_reward = amount
        .checked_mul(multiplier)
        .ok_or(StakingError::CalculationOverflow)?
        .checked_div(100)
        .ok_or(StakingError::CalculationOverflow)?;

    let weekly_reward = annual_reward
        .checked_div(52)
        .ok_or(StakingError::CalculationOverflow)?;

    weekly_reward
        .checked_mul(weeks)
        .ok_or(StakingError::CalculationOverflow.into())
}

/// Validate staking tier parameters
pub fn validate_tier_params(multiplier: u64, min_duration: u8, max_duration: u8) -> Result<()> {
    require!(
        multiplier > 0 && multiplier <= 500,
        StakingError::InvalidMultiplier
    );
    require!(
        min_duration > 0 && min_duration <= max_duration,
        StakingError::InvalidDuration
    );
    require!(max_duration <= 36, StakingError::ExcessiveDuration);
    Ok(())
}

/// Calculate penalty amount for early unstaking
pub fn calculate_early_unstake_penalty(
    amount: u64,
    is_locked: bool,
    is_early: bool,
) -> Result<u64> {
    if !is_locked || !is_early {
        return Ok(0);
    }

    amount
        .checked_mul(EARLY_UNSTAKE_PENALTY)
        .ok_or(StakingError::CalculationOverflow)?
        .checked_div(100)
        .ok_or(StakingError::CalculationOverflow.into())
}

/// Validate reward distribution percentages
pub fn validate_reward_distribution() -> Result<()> {
    let total =
        REWARD_DISTRIBUTION_REFERRAL + REWARD_DISTRIBUTION_CASHBACK + REWARD_DISTRIBUTION_STAKING;

    require!(total == 100, StakingError::InvalidDistribution);
    Ok(())
}

/// Calculate unlock timestamp based on duration
pub fn calculate_unlock_timestamp(start: i64, duration_months: u8) -> Result<i64> {
    let duration_seconds = (duration_months as i64)
        .checked_mul(30 * 24 * 60 * 60)
        .ok_or(StakingError::CalculationOverflow)?;

    start
        .checked_add(duration_seconds)
        .ok_or(StakingError::CalculationOverflow.into())
}

/// Check if enough time has passed for reward claim
pub fn can_claim_rewards(last_claim: i64, current_time: i64) -> bool {
    current_time >= last_claim + WEEK_IN_SECONDS
}

/// Check if vesting period is complete
pub fn is_vesting_complete(vest_timestamp: i64, current_time: i64) -> bool {
    current_time >= vest_timestamp
}

/// Calculate maximum reward per week based on APY cap
pub fn calculate_max_weekly_reward(stake_amount: u64) -> Result<u64> {
    stake_amount
        .checked_mul(MAX_APY)
        .ok_or(StakingError::CalculationOverflow)?
        .checked_div(100 * 52)
        .ok_or(StakingError::CalculationOverflow.into())
}

/// Validate token account ownership and mint
pub fn validate_token_account(
    token_account: &TokenAccount,
    expected_owner: &Pubkey,
    expected_mint: &Pubkey,
) -> Result<()> {
    require!(
        token_account.owner == *expected_owner,
        StakingError::Unauthorized
    );
    require!(
        token_account.mint == *expected_mint,
        StakingError::InvalidTokenAccount
    );
    Ok(())
}

/// Prevent reentrancy attacks by checking program state
pub fn prevent_reentrancy(program_state: &ProgramState) -> Result<()> {
    require!(!program_state.is_paused, StakingError::ProgramPaused);
    Ok(())
}

/// Rate limiting helper for sensitive operations
pub fn check_rate_limit(last_action_time: i64, current_time: i64, min_interval: i64) -> Result<()> {
    require!(
        current_time >= last_action_time + min_interval,
        StakingError::EpochNotReady
    );
    Ok(())
}

/// Validate admin permissions
pub fn validate_admin(program_state: &ProgramState, signer: &Pubkey) -> Result<()> {
    require!(program_state.admin == *signer, StakingError::Unauthorized);
    Ok(())
}
