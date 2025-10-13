use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};
use mpl_core::{
    ID as MPL_CORE_PROGRAM_ID,
    instructions::CreateV2CpiBuilder
};

declare_id!("3Li2pDFFDmzrtw7zJpGDmaYFoRvje8xQ7pvt1vkTzLRg");

// ========== FIXED CONSTANTS ==========
const WEEK_IN_SECONDS: i64 = 604800;
const VESTING_PERIOD_SECONDS: i64 = 365 * 24 * 60 * 60;
const COOLDOWN_PERIOD_SECONDS: i64 = 7 * 24 * 60 * 60;  // 7 days
const WEEKLY_EMISSION_RATE: u64 = 21;
const EMISSION_PRECISION: u64 = 10000;
const REWARD_PENALTY_PERCENT: u64 = 100;
const MIN_STAKE_AMOUNT: u64 = 1_000_000;
const MAX_STAKE_AMOUNT: u64 = 1_000_000_000_000_000;
const MAX_APY_BASIS_POINTS: u64 = 7500;  // 75% APY cap

  

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
        program_state.total_staking_power = 0;
        program_state.reward_pool = 0;
        program_state.is_paused = false;
        program_state.last_epoch_timestamp = Clock::get()?.unix_timestamp;
        program_state.bump = ctx.bumps.program_state;
        program_state.use_token_extensions = false;  // Can be updated later
        program_state.kyc_required = false;  // Can be enabled later
        program_state.approved_kyc_providers = Vec::new();

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
        ensure_not_paused(&ctx.accounts.program_state)?;
        verify_kyc_status(&ctx.accounts.program_state, &ctx.accounts.staker.key())?;
        require!(
            (MIN_STAKE_AMOUNT..=MAX_STAKE_AMOUNT).contains(&amount),
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
        stake_position.cooldown_end = 0;     // No cooldown initially
        stake_position.pending_principal = 0; // No pending principal initially
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

        // Calculate staking power for this position
        let staking_power = calculate_staking_power(amount, stake_position.power_multiplier)?;
        stake_position.staking_power = staking_power;     // Store calculated staking power

        ctx.accounts.program_state.total_staked = ctx
            .accounts.program_state.total_staked
            .checked_add(amount)
            .ok_or(StakingError::CalculationOverflow)?;

        ctx.accounts.program_state.total_staking_power = ctx
            .accounts.program_state.total_staking_power
            .checked_add(staking_power)
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
        ensure_not_paused(&ctx.accounts.program_state)?;

        let stake_position = &mut ctx.accounts.stake_position;
        require!(stake_position.is_active, StakingError::StakeNotActive);
        require!(stake_position.cooldown_end == 0, StakingError::CooldownAlreadyActive);

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

        stake_position.cooldown_end = clock.unix_timestamp + COOLDOWN_PERIOD_SECONDS;
        stake_position.pending_principal = stake_position.amount;
        stake_position.is_active = false;  

        emit!(TokensUnstaked {
            staker: ctx.accounts.staker.key(),
            amount: stake_position.amount,
            reward_penalty,  
            is_early_unstake,
            cooldown_end: stake_position.cooldown_end,
        });

        Ok(())
    }

    pub fn finalize_unstake(ctx: Context<FinalizeUnstake>, _position_seed: u64) -> Result<()> {
        ensure_not_paused(&ctx.accounts.program_state)?;

        let stake_position = &mut ctx.accounts.stake_position;
        require!(!stake_position.is_active, StakingError::StakeStillActive);
        require!(stake_position.cooldown_end > 0, StakingError::NoCooldownActive);
        require!(stake_position.pending_principal > 0, StakingError::NoPendingPrincipal);

        let clock = Clock::get()?;
        require!(
            clock.unix_timestamp >= stake_position.cooldown_end,
            StakingError::CooldownNotComplete
        );

        // Transfer the pending principal
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
        token::transfer(transfer_ctx, stake_position.pending_principal)?;

        // Update program state
        ctx.accounts.program_state.total_staked = ctx
            .accounts.program_state.total_staked
            .checked_sub(stake_position.pending_principal)
            .ok_or(StakingError::CalculationOverflow)?;

        ctx.accounts.program_state.total_staking_power = ctx
            .accounts.program_state.total_staking_power
            .checked_sub(stake_position.staking_power)
            .ok_or(StakingError::CalculationOverflow)?;

        let finalized_amount = stake_position.pending_principal;

        // Clear cooldown state
        stake_position.cooldown_end = 0;
        stake_position.pending_principal = 0;
        stake_position.amount = 0;

        emit!(UnstakeFinalized {
            staker: ctx.accounts.staker.key(),
            amount: finalized_amount,
        });

        Ok(())
    }

    pub fn claim_rewards(
        ctx: Context<ClaimRewards>,
        _position_seed: u64,
        _nft_seed: u64,
    ) -> Result<()> {
        ensure_not_paused(&ctx.accounts.program_state)?;
        verify_kyc_status(&ctx.accounts.program_state, &ctx.accounts.staker.key())?;
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

        let reward_amount = calculate_prorata_reward(
            stake_position.staking_power,
            ctx.accounts.program_state.total_staking_power,
            ctx.accounts.program_state.reward_pool,
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
        ensure_not_paused(&ctx.accounts.program_state)?;
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
            asset_data[0..8] == expected_discriminator,
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

    pub fn replenish_reward_pool(ctx: Context<ReplenishRewardPool>, amount: u64) -> Result<()> {
        require!(amount > 0, StakingError::InvalidAmount);

        // Transfer tokens from admin to program vault
        let transfer_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.admin_token_account.to_account_info(),
                to: ctx.accounts.program_vault_token_account.to_account_info(),
                authority: ctx.accounts.admin.to_account_info(),
            },
        );
        token::transfer(transfer_ctx, amount)?;

        // Increase the reward pool
        ctx.accounts.program_state.reward_pool = ctx
            .accounts.program_state.reward_pool
            .checked_add(amount)
            .ok_or(StakingError::CalculationOverflow)?;

        emit!(RewardPoolReplenished {
            admin: ctx.accounts.admin.key(),
            amount,
            new_pool_balance: ctx.accounts.program_state.reward_pool,
        });

        Ok(())
    }

    // ===== KYC MANAGEMENT FUNCTIONS =====

    pub fn set_kyc_required(ctx: Context<AdminOnly>, required: bool) -> Result<()> {
        ctx.accounts.program_state.kyc_required = required;

        emit!(KycRequirementChanged {
            admin: ctx.accounts.admin.key(),
            required,
        });

        Ok(())
    }

    pub fn add_kyc_provider(ctx: Context<AdminOnly>, provider: Pubkey) -> Result<()> {
        let program_state = &mut ctx.accounts.program_state;

        if !program_state.approved_kyc_providers.contains(&provider) {
            require!(
                program_state.approved_kyc_providers.len() < 10,
                StakingError::TooManyKycProviders
            );
            program_state.approved_kyc_providers.push(provider);
        }

        emit!(KycProviderAdded {
            admin: ctx.accounts.admin.key(),
            provider,
        });

        Ok(())
    }

    pub fn remove_kyc_provider(ctx: Context<AdminOnly>, provider: Pubkey) -> Result<()> {
        let program_state = &mut ctx.accounts.program_state;
        program_state.approved_kyc_providers.retain(|&x| x != provider);

        emit!(KycProviderRemoved {
            admin: ctx.accounts.admin.key(),
            provider,
        });

        Ok(())
    }

    pub fn register_kyc_verification(
        ctx: Context<RegisterKycVerification>,
        user: Pubkey,
        expiry_days: u16,
    ) -> Result<()> {
        let clock = Clock::get()?;
        let kyc_registry = &mut ctx.accounts.kyc_registry;

        kyc_registry.user = user;
        kyc_registry.is_verified = true;
        kyc_registry.verification_timestamp = clock.unix_timestamp;
        kyc_registry.kyc_provider = ctx.accounts.kyc_provider.key();
        kyc_registry.attestation_mint = None; // Can be set if using attestation tokens
        kyc_registry.expiry_timestamp = clock.unix_timestamp + (expiry_days as i64 * 24 * 60 * 60);
        kyc_registry.bump = ctx.bumps.kyc_registry;

        emit!(KycVerificationRegistered {
            user,
            provider: ctx.accounts.kyc_provider.key(),
            expiry_timestamp: kyc_registry.expiry_timestamp,
        });

        Ok(())
    }

    pub fn revoke_kyc_verification(ctx: Context<RevokeKycVerification>) -> Result<()> {
        let kyc_registry = &mut ctx.accounts.kyc_registry;
        kyc_registry.is_verified = false;
        kyc_registry.expiry_timestamp = 0;

        emit!(KycVerificationRevoked {
            user: kyc_registry.user,
            provider: ctx.accounts.kyc_provider.key(),
        });

        Ok(())
    }

    pub fn add_to_whitelist(
        ctx: Context<ManageWhitelist>,
        user: Pubkey,
    ) -> Result<()> {
        let clock = Clock::get()?;
        let whitelist = &mut ctx.accounts.user_whitelist;

        whitelist.user = user;
        whitelist.is_whitelisted = true;
        whitelist.added_timestamp = clock.unix_timestamp;
        whitelist.added_by = ctx.accounts.admin.key();
        whitelist.bump = ctx.bumps.user_whitelist;

        emit!(UserWhitelisted {
            user,
            admin: ctx.accounts.admin.key(),
        });

        Ok(())
    }

    pub fn remove_from_whitelist(ctx: Context<RemoveFromWhitelist>) -> Result<()> {
        let whitelist = &mut ctx.accounts.user_whitelist;
        whitelist.is_whitelisted = false;

        emit!(UserRemovedFromWhitelist {
            user: whitelist.user,
            admin: ctx.accounts.admin.key(),
        });

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
    pub total_staking_power: u64,  // Total staking power across all positions
    pub reward_pool: u64,
    pub is_paused: bool,
    pub last_epoch_timestamp: i64,
    pub use_token_extensions: bool,  // Token Extensions flag for KYC
    pub kyc_required: bool,          // Whether KYC is required for operations
    pub approved_kyc_providers: Vec<Pubkey>,  // List of approved KYC attestation providers
    pub bump: u8,
}

impl ProgramState {
    pub const LEN: usize = 8 + 32 + 32 + 32 + 32 + 32 + 8 + 8 + 8 + 8 + 1 + 8 + 1 + 1 + (4 + 32 * 10) + 1; // Added space for kyc fields
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
    pub power_multiplier: u64,  // Power multiplier based on duration tiers
    pub staking_power: u64,     // Cached staking power for this position
    pub duration_months: u8,
    pub is_locked: bool,
    pub start_timestamp: i64,
    pub unlock_timestamp: i64,
    pub last_reward_timestamp: i64,
    pub accumulated_rewards: u64,
    pub is_active: bool,
    pub cooldown_end: i64,     // Cooldown end timestamp (0 if no cooldown)
    pub pending_principal: u64, // Principal amount pending in cooldown
    pub bump: u8,
}

impl StakePosition {
    pub const LEN: usize = 8 + 32 + 8 + 1 + 8 + 8 + 8 + 1 + 1 + 8 + 8 + 8 + 8 + 1 + 8 + 8 + 1;
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

#[account]
#[derive(Debug)]
pub struct KycRegistry {
    pub user: Pubkey,
    pub is_verified: bool,
    pub verification_timestamp: i64,
    pub kyc_provider: Pubkey,
    pub attestation_mint: Option<Pubkey>,  // Optional attestation token mint
    pub expiry_timestamp: i64,  // KYC expiry timestamp
    pub bump: u8,
}

impl KycRegistry {
    pub const LEN: usize = 8 + 32 + 1 + 8 + 32 + (1 + 32) + 8 + 1;
}

#[account]
#[derive(Debug)]
pub struct UserWhitelist {
    pub user: Pubkey,
    pub is_whitelisted: bool,
    pub added_timestamp: i64,
    pub added_by: Pubkey,
    pub bump: u8,
}

impl UserWhitelist {
    pub const LEN: usize = 8 + 32 + 1 + 8 + 32 + 1;
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
#[instruction(position_seed: u64)]
pub struct FinalizeUnstake<'info> {
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

#[derive(Accounts)]
pub struct ReplenishRewardPool<'info> {
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
        constraint = admin_token_account.owner == admin.key() @ StakingError::Unauthorized,
        constraint = admin_token_account.mint == program_state.reward_token_mint @ StakingError::InvalidTokenAccount
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

#[derive(Accounts)]
#[instruction(user: Pubkey)]
pub struct RegisterKycVerification<'info> {
    #[account(
        init,
        payer = kyc_provider,
        space = KycRegistry::LEN,
        seeds = [b"kyc_registry", user.as_ref()],
        bump
    )]
    pub kyc_registry: Account<'info, KycRegistry>,
    #[account(mut)]
    pub kyc_provider: Signer<'info>,
    #[account(
        seeds = [b"program_state"],
        bump,
        constraint = program_state.approved_kyc_providers.contains(&kyc_provider.key()) @ StakingError::UnauthorizedKycProvider
    )]
    pub program_state: Account<'info, ProgramState>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RevokeKycVerification<'info> {
    #[account(
        mut,
        seeds = [b"kyc_registry", kyc_registry.user.as_ref()],
        bump,
        constraint = kyc_registry.kyc_provider == kyc_provider.key() @ StakingError::Unauthorized
    )]
    pub kyc_registry: Account<'info, KycRegistry>,
    pub kyc_provider: Signer<'info>,
    #[account(
        seeds = [b"program_state"],
        bump
    )]
    pub program_state: Account<'info, ProgramState>,
}

#[derive(Accounts)]
#[instruction(user: Pubkey)]
pub struct ManageWhitelist<'info> {
    #[account(
        init_if_needed,
        payer = admin,
        space = UserWhitelist::LEN,
        seeds = [b"user_whitelist", user.as_ref()],
        bump
    )]
    pub user_whitelist: Account<'info, UserWhitelist>,
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
pub struct RemoveFromWhitelist<'info> {
    #[account(
        mut,
        seeds = [b"user_whitelist", user_whitelist.user.as_ref()],
        bump
    )]
    pub user_whitelist: Account<'info, UserWhitelist>,
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        seeds = [b"program_state"],
        bump,
        constraint = program_state.admin == admin.key() @ StakingError::Unauthorized
    )]
    pub program_state: Account<'info, ProgramState>,
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
    pub cooldown_end: i64,
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

#[event]
pub struct UnstakeFinalized {
    pub staker: Pubkey,
    pub amount: u64,
}

#[event]
pub struct RewardPoolReplenished {
    pub admin: Pubkey,
    pub amount: u64,
    pub new_pool_balance: u64,
}

#[event]
pub struct KycRequirementChanged {
    pub admin: Pubkey,
    pub required: bool,
}

#[event]
pub struct KycProviderAdded {
    pub admin: Pubkey,
    pub provider: Pubkey,
}

#[event]
pub struct KycProviderRemoved {
    pub admin: Pubkey,
    pub provider: Pubkey,
}

#[event]
pub struct KycVerificationRegistered {
    pub user: Pubkey,
    pub provider: Pubkey,
    pub expiry_timestamp: i64,
}

#[event]
pub struct KycVerificationRevoked {
    pub user: Pubkey,
    pub provider: Pubkey,
}

#[event]
pub struct UserWhitelisted {
    pub user: Pubkey,
    pub admin: Pubkey,
}

#[event]
pub struct UserRemovedFromWhitelist {
    pub user: Pubkey,
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
    #[msg("Cooldown already active")]
    CooldownAlreadyActive,
    #[msg("Stake still active")]
    StakeStillActive,
    #[msg("No cooldown active")]
    NoCooldownActive,
    #[msg("No pending principal")]
    NoPendingPrincipal,
    #[msg("Cooldown period not complete")]
    CooldownNotComplete,
    #[msg("KYC verification required")]
    KYCNotVerified,
    #[msg("KYC provider not authorized")]
    UnauthorizedKycProvider,
    #[msg("KYC verification expired")]
    KycVerificationExpired,
    #[msg("Too many KYC providers (max 10)")]
    TooManyKycProviders,
    #[msg("User not whitelisted")]
    UserNotWhitelisted,
}

// ========== HELPER FUNCTIONS ==========

// ✅ Pro-rata reward distribution from pool
fn calculate_prorata_reward(
    user_staking_power: u64,
    total_staking_power: u64,
    reward_pool: u64,
    weeks: u64,
) -> Result<u64> {
    if total_staking_power == 0 || user_staking_power == 0 {
        return Ok(0);
    }

    // Weekly emission from pool: 0.21% = 21/10000
    let weekly_pool_emission = reward_pool
        .checked_mul(WEEKLY_EMISSION_RATE)
        .ok_or(StakingError::CalculationOverflow)?
        .checked_div(EMISSION_PRECISION)
        .ok_or(StakingError::CalculationOverflow)?;

    // User's share based on staking power
    let user_weekly_share = weekly_pool_emission
        .checked_mul(user_staking_power)
        .ok_or(StakingError::CalculationOverflow)?
        .checked_div(total_staking_power)
        .ok_or(StakingError::CalculationOverflow)?;

    // Multiply by weeks
    let total_reward = user_weekly_share
        .checked_mul(weeks)
        .ok_or(StakingError::CalculationOverflow)?;

    // Apply 75% APY cap
    apply_apy_cap(user_staking_power, total_reward, weeks)
}

// Calculate staking power = amount × multiplier / 100
fn calculate_staking_power(amount: u64, power_multiplier: u64) -> Result<u64> {
    amount
        .checked_mul(power_multiplier)
        .ok_or(StakingError::CalculationOverflow)?
        .checked_div(100)
        .ok_or(StakingError::CalculationOverflow.into())
}

// Apply 75% APY cap to prevent over-incentivizing
fn apply_apy_cap(staked_amount: u64, reward_amount: u64, weeks: u64) -> Result<u64> {
    if weeks == 0 {
        return Ok(reward_amount);
    }

    // Calculate annualized rate (basis points)
    // APY = (reward / staked) × (52 / weeks) × 10000
    let annualized_rate = reward_amount
        .checked_mul(52)
        .ok_or(StakingError::CalculationOverflow)?
        .checked_mul(10000)
        .ok_or(StakingError::CalculationOverflow)?
        .checked_div(staked_amount)
        .ok_or(StakingError::CalculationOverflow)?
        .checked_div(weeks)
        .ok_or(StakingError::CalculationOverflow)?;

    if annualized_rate > MAX_APY_BASIS_POINTS {
        // Cap at 75% APY
        let capped_reward = staked_amount
            .checked_mul(MAX_APY_BASIS_POINTS)
            .ok_or(StakingError::CalculationOverflow)?
            .checked_div(10000)
            .ok_or(StakingError::CalculationOverflow)?
            .checked_mul(weeks)
            .ok_or(StakingError::CalculationOverflow)?
            .checked_div(52)
            .ok_or(StakingError::CalculationOverflow)?;

        Ok(capped_reward)
    } else {
        Ok(reward_amount)
    }
}

fn calculate_power_multiplier(_amount: u64, duration_months: u8) -> Result<u64> {
    let multiplier = match duration_months {
        1..=5 => 100,    // 1× for 1-5 months
        6..=11 => 150,   // 1.5× for 6-11 months
        12..=17 => 200,  // 2× for 12-17 months
        18..=23 => 250,  // 2.5× for 18-23 months
        24..=35 => 300,  // 3× for 24-35 months
        36..=u8::MAX => 400,  // 4× for 36+ months
        _ => 100,        // Default to 1× for edge cases
    };

    Ok(multiplier)
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
    current_time >= last_claim + WEEK_IN_SECONDS  
}

pub fn is_vesting_complete(vest_timestamp: i64, current_time: i64) -> bool {
    current_time >= vest_timestamp  
}

pub fn ensure_not_paused(program_state: &ProgramState) -> Result<()> {
    require!(!program_state.is_paused, StakingError::ProgramPaused);
    Ok(())
}

// KYC check placeholder - integrates with Solana Attestation Service
// Comprehensive KYC verification system
pub fn verify_kyc_status(program_state: &ProgramState, user: &Pubkey) -> Result<()> {
    // If KYC is not required, allow all operations
    if !program_state.kyc_required {
        return Ok(());
    }

    // If token extensions are not enabled, only basic whitelist checking
    if !program_state.use_token_extensions {
        // Basic whitelist approach - would need to be checked via CPI call
        // This is a simplified version that assumes whitelist checking is done elsewhere
        msg!("KYC required but token extensions disabled - manual verification needed");
        return Ok(()); // In production, this might require additional verification
    }

    // Full KYC verification with attestation service integration
    // In a production environment, this would:
    // 1. Check for KYC attestation tokens in user's wallet via CPI
    // 2. Verify attestation signatures from approved providers
    // 3. Check expiry timestamps
    // 4. Validate against program's approved provider list

    msg!("KYC verification required for user: {}", user);
    msg!("Approved KYC providers: {:?}", program_state.approved_kyc_providers);

    // This would be implemented with actual attestation token checking:
    // let kyc_registry = get_kyc_registry(user)?;
    // verify_kyc_attestation(&kyc_registry, program_state)?;

    // For production deployment, uncomment this to enforce KYC:
    // require!(false, StakingError::KYCNotVerified);

    Ok(())
}

// Helper function to verify KYC attestation (would be used in production)
pub fn verify_kyc_attestation(
    user: &Pubkey,
    _program_state: &ProgramState,
    kyc_registry_data: Option<&[u8]>,
) -> Result<bool> {
    // If no KYC registry data provided, check if user is whitelisted
    let Some(registry_data) = kyc_registry_data else {
        msg!("No KYC registry found for user: {}", user);
        return Ok(false);
    };

    // Parse KYC registry data (this would be actual deserialization in production)
    // For now, we simulate the structure
    if registry_data.len() < 8 {
        return Ok(false);
    }

    // Simulate KYC verification logic
    let current_timestamp = Clock::get()?.unix_timestamp;

    // In production, this would deserialize the actual KycRegistry struct
    // and verify:
    // 1. is_verified is true
    // 2. expiry_timestamp > current_timestamp
    // 3. kyc_provider is in approved_kyc_providers list
    // 4. Optional: verify attestation token signature

    msg!("KYC verification check for user: {} at timestamp: {}", user, current_timestamp);

    Ok(true) // In production, return actual verification result
}

// Function to check if user is whitelisted (bypass KYC for certain users)
pub fn check_user_whitelist(user: &Pubkey, whitelist_data: Option<&[u8]>) -> Result<bool> {
    let Some(data) = whitelist_data else {
        return Ok(false);
    };

    if data.len() < 8 {
        return Ok(false);
    }

    // In production, deserialize UserWhitelist struct and check is_whitelisted
    msg!("Checking whitelist for user: {}", user);
    Ok(false) // Return actual whitelist status in production
}

// Enhanced verify function that uses all KYC mechanisms
pub fn verify_kyc_comprehensive(
    program_state: &ProgramState,
    user: &Pubkey,
    kyc_registry_data: Option<&[u8]>,
    whitelist_data: Option<&[u8]>,
) -> Result<()> {
    // Skip verification if KYC not required
    if !program_state.kyc_required {
        return Ok(());
    }

    // Check whitelist first (admin override)
    if check_user_whitelist(user, whitelist_data)? {
        msg!("User {} approved via whitelist", user);
        return Ok(());
    }

    // If token extensions enabled, do full attestation verification
    if program_state.use_token_extensions {
        if verify_kyc_attestation(user, program_state, kyc_registry_data)? {
            msg!("User {} approved via KYC attestation", user);
            return Ok(());
        }
    }

    // If we reach here, user failed all verification methods
    msg!("KYC verification failed for user: {}", user);
    Err(StakingError::KYCNotVerified.into())
}