use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};
use mpl_core::{
    ID as MPL_CORE_PROGRAM_ID,
    instructions::CreateV2CpiBuilder
};
use std::cmp;

declare_id!("AKnc8CqVVCyBuzzyvNEPQZGYCiEiqRneETDSgm1ZU69N");
const WEEK_IN_SECONDS: i64 = 604800;
const MAX_APY: u64 = 75;
const EARLY_UNSTAKE_PENALTY: u64 = 50;
const REWARD_DISTRIBUTION_REFERRAL: u64 = 30;
const REWARD_DISTRIBUTION_CASHBACK: u64 = 30;
const REWARD_DISTRIBUTION_STAKING: u64 = 40;
const VESTING_PERIOD_SECONDS: i64 = 30 * 24 * 60 * 60;
const MIN_STAKE_AMOUNT: u64 = 1_000_000;
const MAX_STAKE_AMOUNT: u64 = 1_000_000_000_000_000;

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

        emit!(ProgramInitialized {
            admin,
            reward_token_mint,
            protocol_treasury,
            referral_pool,
            cashback_pool,
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

        validate_token_account(
            &ctx.accounts.user_token_account,
            &ctx.accounts.staker.key(),
            &ctx.accounts.program_state.reward_token_mint,
        )?;

        let clock = Clock::get()?;
        let stake_position = &mut ctx.accounts.stake_position;

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

    pub fn unstake_tokens(ctx: Context<UnstakeTokens>, _position_seed: u64) -> Result<()> {
        prevent_reentrancy(&ctx.accounts.program_state)?;

        let stake_position = &mut ctx.accounts.stake_position;
        require!(stake_position.is_active, StakingError::StakeNotActive);

        let clock = Clock::get()?;
        let is_early_unstake = clock.unix_timestamp < stake_position.unlock_timestamp;

        let mut transfer_amount = stake_position.amount;
        let mut penalty_amount = 0u64;

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

            let to_reward_pool = penalty_amount
                .checked_div(2)
                .ok_or(StakingError::CalculationOverflow)?;
            let to_treasury = penalty_amount
                .checked_sub(to_reward_pool)
                .ok_or(StakingError::CalculationOverflow)?;
            ctx.accounts.program_state.reward_pool = ctx
                .accounts
                .program_state
                .reward_pool
                .checked_add(to_reward_pool)
                .ok_or(StakingError::CalculationOverflow)?;
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
        token::transfer(transfer_ctx, transfer_amount)?;

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

    pub fn claim_rewards(
        ctx: Context<ClaimRewards>,
        _position_seed: u64,
        _nft_seed: u64,
    ) -> Result<()> {
        prevent_reentrancy(&ctx.accounts.program_state)?;
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

        let max_weekly_reward = calculate_max_weekly_reward(stake_position.amount)?;
        let final_reward = cmp::min(
            reward_amount,
            max_weekly_reward
                .checked_mul(weeks_elapsed as u64)
                .ok_or(StakingError::CalculationOverflow)?,
        );

        require!(
            ctx.accounts.program_state.reward_pool >= final_reward,
            StakingError::InsufficientRewardPool
        );

        if final_reward > 0 {
            // Create a Metaplex Core NFT representing the reward claim
            let nft_name = format!("Staking Reward #{}", &ctx.accounts.nft_asset.key().to_string()[..8]);
            let nft_uri = format!("https://rewards.example.com/metadata/{}", ctx.accounts.nft_asset.key());

            // Create the Core NFT using CPI
            CreateV2CpiBuilder::new(&ctx.accounts.mpl_core_program)
                .asset(&ctx.accounts.nft_asset)
                .payer(&ctx.accounts.staker)
                .owner(Some(&ctx.accounts.asset_owner))
                .update_authority(Some(&ctx.accounts.staker))
                .system_program(&ctx.accounts.system_program)
                .name(nft_name)
                .uri(nft_uri)
                .invoke()?;

            // Store reward information in PDA
            let reward_nft = &mut ctx.accounts.reward_nft;
            reward_nft.owner = ctx.accounts.staker.key();
            reward_nft.reward_amount = final_reward;
            reward_nft.vest_timestamp = clock.unix_timestamp + VESTING_PERIOD_SECONDS;
            reward_nft.nft_asset = ctx.accounts.nft_asset.key();
            reward_nft.is_active = true;
            reward_nft.bump = ctx.bumps.reward_nft;

            stake_position.accumulated_rewards = stake_position
                .accumulated_rewards
                .checked_add(final_reward)
                .ok_or(StakingError::CalculationOverflow)?;

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

    pub fn vest_reward_nft(ctx: Context<VestRewardNft>, _nft_seed: u64) -> Result<()> {
        prevent_reentrancy(&ctx.accounts.program_state)?;
        let reward_nft = &ctx.accounts.reward_nft;
        require!(reward_nft.is_active, StakingError::NFTAlreadyVested);

        let clock = Clock::get()?;
        require!(
            is_vesting_complete(reward_nft.vest_timestamp, clock.unix_timestamp),
            StakingError::VestingNotComplete
        );

        // Note: With Metaplex Core NFTs, ownership verification is handled by the Core program
        // The NFT can be traded/transferred, and whoever holds it can redeem it
        // This allows for secondary market trading of vesting rewards

        // Transfer the reward tokens to the current NFT holder (user)
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

        // Mark the reward as redeemed (but keep the NFT as proof)
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

    pub fn distribute_weekly_rewards(
        ctx: Context<DistributeWeeklyRewards>,
        total_rewards: u64,
    ) -> Result<()> {
        prevent_reentrancy(&ctx.accounts.program_state)?;
        let clock = Clock::get()?;
        let program_state = &mut ctx.accounts.program_state;

        check_rate_limit(
            program_state.last_epoch_timestamp,
            clock.unix_timestamp,
            WEEK_IN_SECONDS,
        )?;

        validate_reward_distribution()?;

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

        let total_distributed = referral_amount
            .checked_add(cashback_amount)
            .ok_or(StakingError::CalculationOverflow)?
            .checked_add(staking_amount)
            .ok_or(StakingError::CalculationOverflow)?;
        require!(
            total_distributed == total_rewards,
            StakingError::InvalidDistribution
        );

        // Transfer referral amount to referral pool
        if referral_amount > 0 {
            let seeds = &[
                b"program_vault".as_ref(),
                &[ctx.accounts.program_vault.bump],
            ];
            let signer = &[&seeds[..]];

            let transfer_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.program_vault_token_account.to_account_info(),
                    to: ctx.accounts.referral_pool_token_account.to_account_info(),
                    authority: ctx.accounts.program_vault.to_account_info(),
                },
                signer,
            );
            token::transfer(transfer_ctx, referral_amount)?;
        }

        // Transfer cashback amount to cashback pool
        if cashback_amount > 0 {
            let seeds = &[
                b"program_vault".as_ref(),
                &[ctx.accounts.program_vault.bump],
            ];
            let signer = &[&seeds[..]];

            let transfer_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.program_vault_token_account.to_account_info(),
                    to: ctx.accounts.cashback_pool_token_account.to_account_info(),
                    authority: ctx.accounts.program_vault.to_account_info(),
                },
                signer,
            );
            token::transfer(transfer_ctx, cashback_amount)?;
        }

        // Add staking amount to reward pool
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

    pub fn pause_program(ctx: Context<AdminOnly>) -> Result<()> {
        ctx.accounts.program_state.is_paused = true;

        emit!(ProgramPaused {
            admin: ctx.accounts.admin.key(),
        });

        Ok(())
    }

    pub fn unpause_program(ctx: Context<AdminOnly>) -> Result<()> {
        ctx.accounts.program_state.is_paused = false;

        emit!(ProgramUnpaused {
            admin: ctx.accounts.admin.key(),
        });

        Ok(())
    }

    pub fn buyback_and_burn(ctx: Context<BuybackAndBurn>, amount: u64) -> Result<()> {
        require!(amount > 0, StakingError::InvalidAmount);

        emit!(BuybackAndBurnEvent {
            admin: ctx.accounts.admin.key(),
            amount,
        });

        Ok(())
    }

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

    pub fn add_reward_funds(ctx: Context<AddRewardFunds>, amount: u64) -> Result<()> {
        prevent_reentrancy(&ctx.accounts.program_state)?;
        require!(amount > 0, StakingError::InvalidAmount);

        let transfer_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.admin_token_account.to_account_info(),
                to: ctx.accounts.program_vault_token_account.to_account_info(),
                authority: ctx.accounts.admin.to_account_info(),
            },
        );
        token::transfer(transfer_ctx, amount)?;

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
    pub bump: u8,
}

impl ProgramState {
    pub const LEN: usize = 8 + 32 + 32 + 32 + 32 + 32 + 8 + 8 + 8 + 1 + 8 + 1;
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
    pub multiplier: u64,            
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
    pub const LEN: usize = 8 + 32 + 8 + 1 + 8 + 1 + 1 + 8 + 8 + 8 + 8 + 1 + 1;
}

#[account]
#[derive(Debug)]
pub struct RewardNFT {
    pub owner: Pubkey,
    pub reward_amount: u64,
    pub vest_timestamp: i64,
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
    /// CHECK: This will be the Core NFT asset account - validated by Core program
    #[account(mut)]
    pub nft_asset: UncheckedAccount<'info>,
    /// CHECK: This is the authority that will own the NFT
    pub asset_owner: AccountInfo<'info>,
    #[account(
        mut,
        seeds = [b"program_state"],
        bump
    )]
    pub program_state: Account<'info, ProgramState>,
    pub system_program: Program<'info, System>,
    /// CHECK: This is the Metaplex Core program
    #[account(address = MPL_CORE_PROGRAM_ID)]
    pub mpl_core_program: UncheckedAccount<'info>,
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
    /// CHECK: This is the Core NFT asset account
    #[account(
        mut,
        address = reward_nft.nft_asset
    )]
    pub nft_asset: UncheckedAccount<'info>,
    #[account(
        mut,
        seeds = [b"program_state"],
        bump
    )]
    pub program_state: Account<'info, ProgramState>,
    #[account(
        mut,
        constraint = user_token_account.owner == user.key() @ StakingError::Unauthorized,
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
        constraint = referral_pool_token_account.owner == program_state.referral_pool @ StakingError::InvalidTokenAccount,
        constraint = referral_pool_token_account.mint == program_state.reward_token_mint @ StakingError::InvalidTokenAccount
    )]
    pub referral_pool_token_account: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = cashback_pool_token_account.owner == program_state.cashback_pool @ StakingError::InvalidTokenAccount,
        constraint = cashback_pool_token_account.mint == program_state.reward_token_mint @ StakingError::InvalidTokenAccount
    )]
    pub cashback_pool_token_account: Account<'info, TokenAccount>,
    pub admin: Signer<'info>,
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
    #[account(
        mut,
        constraint = program_vault_token_account.owner == program_vault.key() @ StakingError::InvalidTokenAccount,
        constraint = program_vault_token_account.mint == program_state.reward_token_mint @ StakingError::InvalidTokenAccount
    )]
    pub program_vault_token_account: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}
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
    pub nft_asset: Pubkey,
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
    #[msg("NFT not owned")]
    NFTNotOwned,
}
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

pub fn validate_reward_distribution() -> Result<()> {
    let total =
        REWARD_DISTRIBUTION_REFERRAL + REWARD_DISTRIBUTION_CASHBACK + REWARD_DISTRIBUTION_STAKING;

    require!(total == 100, StakingError::InvalidDistribution);
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
    current_time >= last_claim + WEEK_IN_SECONDS
}

pub fn is_vesting_complete(vest_timestamp: i64, current_time: i64) -> bool {
    current_time >= vest_timestamp
}

pub fn calculate_max_weekly_reward(stake_amount: u64) -> Result<u64> {
    stake_amount
        .checked_mul(MAX_APY)
        .ok_or(StakingError::CalculationOverflow)?
        .checked_div(100 * 52)
        .ok_or(StakingError::CalculationOverflow.into())
}

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

pub fn prevent_reentrancy(program_state: &ProgramState) -> Result<()> {
    require!(!program_state.is_paused, StakingError::ProgramPaused);
    Ok(())
}

pub fn check_rate_limit(last_action_time: i64, current_time: i64, min_interval: i64) -> Result<()> {
    require!(
        current_time >= last_action_time + min_interval,
        StakingError::EpochNotReady
    );
    Ok(())
}

pub fn validate_admin(program_state: &ProgramState, signer: &Pubkey) -> Result<()> {
    require!(program_state.admin == *signer, StakingError::Unauthorized);
    Ok(())
}
