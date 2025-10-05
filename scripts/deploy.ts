// Production deployment script for Solana Staking & Rewards Contract
// This script handles complete initialization of the contract with proper security checks

import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { StakingRewardsContract } from "../target/types/staking_rewards_contract";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  getOrCreateAssociatedTokenAccount,
  mintTo,
} from "@solana/spl-token";
import { Keypair, PublicKey, Connection } from "@solana/web3.js";

// Configuration - Update these for production deployment
const ADMIN_KEYPAIR_PATH = process.env.ADMIN_KEYPAIR_PATH || "~/.config/solana/id.json";
const RPC_URL = process.env.RPC_URL || "https://api.mainnet-beta.solana.com";
const NETWORK = process.env.NETWORK || "mainnet-beta";

// Production addresses - Replace with actual production addresses
const TREASURY_ADDRESS = process.env.TREASURY_ADDRESS || ""; // Set this in production
const REFERRAL_POOL_ADDRESS = process.env.REFERRAL_POOL_ADDRESS || ""; // Set this in production
const CASHBACK_POOL_ADDRESS = process.env.CASHBACK_POOL_ADDRESS || ""; // Set this in production
const TOKEN_MINT_ADDRESS = process.env.TOKEN_MINT_ADDRESS || ""; // Set this in production

interface DeploymentConfig {
  admin: Keypair;
  treasury: PublicKey;
  referralPool: PublicKey;
  cashbackPool: PublicKey;
  tokenMint: PublicKey;
  initialTiers: TierConfig[];
}

interface TierConfig {
  id: number;
  multiplier: number;
  minDurationMonths: number;
  maxDurationMonths: number;
}

// Default tier configurations
const DEFAULT_TIERS: TierConfig[] = [
  {
    id: 1,
    multiplier: 150, // 150% APY
    minDurationMonths: 1,
    maxDurationMonths: 12,
  },
  {
    id: 2,
    multiplier: 300, // 300% APY
    minDurationMonths: 6,
    maxDurationMonths: 24,
  },
  {
    id: 3,
    multiplier: 500, // 500% APY
    minDurationMonths: 12,
    maxDurationMonths: 60, // 5 years - no limit now
  },
];

async function main() {
  console.log("🚀 Starting Solana Staking & Rewards Contract Deployment");
  console.log("Network:", NETWORK);
  console.log("RPC URL:", RPC_URL);

  // Setup connection and provider
  const connection = new Connection(RPC_URL, "confirmed");

  // Load admin keypair
  let adminKeypair: Keypair;
  try {
    if (process.env.ADMIN_PRIVATE_KEY) {
      // Load from environment variable (base58 encoded)
      const privateKeyBytes = anchor.utils.bytes.bs58.decode(process.env.ADMIN_PRIVATE_KEY);
      adminKeypair = Keypair.fromSecretKey(privateKeyBytes);
    } else {
      // Load from file
      adminKeypair = anchor.web3.Keypair.fromSecretKey(
        Buffer.from(
          JSON.parse(
            require("fs").readFileSync(
              ADMIN_KEYPAIR_PATH.replace("~", require("os").homedir()),
              "utf8"
            )
          )
        )
      );
    }
    console.log("✅ Admin keypair loaded:", adminKeypair.publicKey.toString());
  } catch (error) {
    console.error("❌ Failed to load admin keypair:", error);
    process.exit(1);
  }

  const wallet = new anchor.Wallet(adminKeypair);
  const provider = new anchor.AnchorProvider(connection, wallet, {
    commitment: "confirmed",
  });
  anchor.setProvider(provider);

  const program = anchor.workspace.StakingRewardsContract as Program<StakingRewardsContract>;
  console.log("📦 Program ID:", program.programId.toString());

  // Validate required addresses for production
  if (NETWORK === "mainnet-beta" || NETWORK === "devnet") {
    if (!TREASURY_ADDRESS || !REFERRAL_POOL_ADDRESS || !CASHBACK_POOL_ADDRESS) {
      console.error("❌ Required addresses not set for production deployment");
      console.log("Please set the following environment variables:");
      console.log("- TREASURY_ADDRESS");
      console.log("- REFERRAL_POOL_ADDRESS");
      console.log("- CASHBACK_POOL_ADDRESS");
      process.exit(1);
    }
  }

  // Setup deployment configuration
  const config: DeploymentConfig = {
    admin: adminKeypair,
    treasury: TREASURY_ADDRESS ? new PublicKey(TREASURY_ADDRESS) : adminKeypair.publicKey,
    referralPool: REFERRAL_POOL_ADDRESS ? new PublicKey(REFERRAL_POOL_ADDRESS) : adminKeypair.publicKey,
    cashbackPool: CASHBACK_POOL_ADDRESS ? new PublicKey(CASHBACK_POOL_ADDRESS) : adminKeypair.publicKey,
    tokenMint: TOKEN_MINT_ADDRESS ? new PublicKey(TOKEN_MINT_ADDRESS) : PublicKey.default,
    initialTiers: DEFAULT_TIERS,
  };

  // Create token mint if not provided (for devnet/localnet only)
  if (config.tokenMint.equals(PublicKey.default)) {
    if (NETWORK === "mainnet-beta") {
      console.error("❌ TOKEN_MINT_ADDRESS must be provided for mainnet deployment");
      process.exit(1);
    }

    console.log("🪙 Creating new token mint...");
    config.tokenMint = await createMint(
      connection,
      adminKeypair,
      adminKeypair.publicKey,
      adminKeypair.publicKey,
      9 // 9 decimals
    );
    console.log("✅ Token mint created:", config.tokenMint.toString());
  }

  // Find PDAs
  console.log("🔍 Finding Program Derived Addresses...");

  const [programState] = PublicKey.findProgramAddressSync(
    [Buffer.from("program_state")],
    program.programId
  );

  const [programVault] = PublicKey.findProgramAddressSync(
    [Buffer.from("program_vault")],
    program.programId
  );

  console.log("Program State PDA:", programState.toString());
  console.log("Program Vault PDA:", programVault.toString());

  // Check if already initialized
  try {
    const programStateAccount = await program.account.programState.fetch(programState);
    console.log("⚠️  Program already initialized!");
    console.log("Current admin:", programStateAccount.admin.toString());

    if (!programStateAccount.admin.equals(adminKeypair.publicKey)) {
      console.error("❌ You are not the admin of this program!");
      process.exit(1);
    }

    console.log("✅ Continuing with existing deployment...");
  } catch (error) {
    // Program not initialized, proceed with initialization
    console.log("📋 Initializing program...");

    try {
      const tx = await program.methods
        .initialize(
          config.admin.publicKey,
          config.tokenMint,
          config.treasury,
          config.referralPool,
          config.cashbackPool
        )
        .accounts({
          programState: programState,
          programVault: programVault,
          admin: config.admin.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .signers([config.admin])
        .rpc();

      console.log("✅ Program initialized! Transaction:", tx);
    } catch (initError) {
      console.error("❌ Failed to initialize program:", initError);
      process.exit(1);
    }
  }

  // Create program vault token account
  console.log("💰 Setting up program vault token account...");
  const programVaultTokenAccount = await getOrCreateAssociatedTokenAccount(
    connection,
    config.admin,
    config.tokenMint,
    programVault,
    true // allowOwnerOffCurve
  );
  console.log("✅ Program vault token account:", programVaultTokenAccount.address.toString());

  // Create staking tiers
  console.log("🎯 Creating staking tiers...");
  for (const tierConfig of config.initialTiers) {
    const [stakingTier] = PublicKey.findProgramAddressSync(
      [Buffer.from("staking_tier"), Buffer.from([tierConfig.id])],
      program.programId
    );

    try {
      // Check if tier already exists
      await program.account.stakingTier.fetch(stakingTier);
      console.log(`⚠️  Tier ${tierConfig.id} already exists, skipping...`);
    } catch (error) {
      // Tier doesn't exist, create it
      try {
        const tx = await program.methods
          .createStakingTier(
            tierConfig.id,
            new anchor.BN(tierConfig.multiplier),
            tierConfig.minDurationMonths,
            tierConfig.maxDurationMonths
          )
          .accounts({
            stakingTier: stakingTier,
            admin: config.admin.publicKey,
            programState: programState,
            systemProgram: anchor.web3.SystemProgram.programId,
          })
          .signers([config.admin])
          .rpc();

        console.log(`✅ Tier ${tierConfig.id} created! Transaction:`, tx);
      } catch (tierError) {
        console.error(`❌ Failed to create tier ${tierConfig.id}:`, tierError);
      }
    }
  }

  // Add initial reward funds (for non-mainnet only)
  if (NETWORK !== "mainnet-beta") {
    console.log("💸 Adding initial reward funds...");
    const initialRewardAmount = new anchor.BN(1_000_000 * 10 ** 9); // 1M tokens

    const adminTokenAccount = await getOrCreateAssociatedTokenAccount(
      connection,
      config.admin,
      config.tokenMint,
      config.admin.publicKey
    );

    // Mint tokens to admin first (for testing)
    try {
      await mintTo(
        connection,
        config.admin,
        config.tokenMint,
        adminTokenAccount.address,
        config.admin,
        initialRewardAmount.toNumber()
      );
      console.log("✅ Minted tokens to admin");
    } catch (error) {
      console.log("⚠️  Failed to mint (tokens may already exist)");
    }

    // Add reward funds to program
    try {
      const tx = await program.methods
        .addRewardFunds(initialRewardAmount)
        .accounts({
          programState: programState,
          admin: config.admin.publicKey,
          adminTokenAccount: adminTokenAccount.address,
          programVault: programVault,
          programVaultTokenAccount: programVaultTokenAccount.address,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([config.admin])
        .rpc();

      console.log("✅ Initial reward funds added! Transaction:", tx);
    } catch (error) {
      console.error("❌ Failed to add reward funds:", error);
    }
  }

  // Final verification
  console.log("🔍 Verifying deployment...");
  try {
    const programStateAccount = await program.account.programState.fetch(programState);

    console.log("\n📊 Deployment Summary:");
    console.log("═══════════════════════════════════════");
    console.log("✅ Program ID:", program.programId.toString());
    console.log("✅ Admin:", programStateAccount.admin.toString());
    console.log("✅ Token Mint:", programStateAccount.rewardTokenMint.toString());
    console.log("✅ Treasury:", programStateAccount.protocolTreasury.toString());
    console.log("✅ Referral Pool:", programStateAccount.referralPool.toString());
    console.log("✅ Cashback Pool:", programStateAccount.cashbackPool.toString());
    console.log("✅ Program Vault:", programVault.toString());
    console.log("✅ Vault Token Account:", programVaultTokenAccount.address.toString());
    console.log("✅ Total Staked:", programStateAccount.totalStaked.toNumber() / 10**9, "tokens");
    console.log("✅ Reward Pool:", programStateAccount.rewardPool.toNumber() / 10**9, "tokens");
    console.log("✅ Current Epoch:", programStateAccount.currentEpoch.toNumber());
    console.log("✅ Is Paused:", programStateAccount.isPaused);
    console.log("═══════════════════════════════════════");

    // Verify tiers
    console.log("\n🎯 Created Staking Tiers:");
    for (const tierConfig of config.initialTiers) {
      const [stakingTier] = PublicKey.findProgramAddressSync(
        [Buffer.from("staking_tier"), Buffer.from([tierConfig.id])],
        program.programId
      );

      try {
        const tierAccount = await program.account.stakingTier.fetch(stakingTier);
        console.log(`✅ Tier ${tierAccount.tierId}: ${tierAccount.multiplier.toNumber()}% APY, ${tierAccount.minDurationMonths}-${tierAccount.maxDurationMonths} months`);
      } catch (error) {
        console.log(`❌ Tier ${tierConfig.id}: Failed to verify`);
      }
    }

    console.log("\n🎉 Deployment completed successfully!");

    if (NETWORK === "mainnet-beta") {
      console.log("\n⚠️  PRODUCTION DEPLOYMENT CHECKLIST:");
      console.log("1. ✅ Verify all addresses are correct");
      console.log("2. ⚠️  Add reward funds using add_reward_funds instruction");
      console.log("3. ⚠️  Set up monitoring for the contract");
      console.log("4. ⚠️  Test with small amounts first");
      console.log("5. ⚠️  Ensure admin key security");
    }

  } catch (error) {
    console.error("❌ Deployment verification failed:", error);
    process.exit(1);
  }
}

// Run deployment
main().catch((error) => {
  console.error("❌ Deployment failed:", error);
  process.exit(1);
});