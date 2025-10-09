import * as anchor from "@coral-xyz/anchor";
import {
  PublicKey,
  Keypair,
  SystemProgram
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  mintTo,
  getOrCreateAssociatedTokenAccount,
  getAccount
} from "@solana/spl-token";
import fs from "fs";
import path from "path";
import dotenv from "dotenv";

// Load environment variables
dotenv.config({ path: path.join(__dirname, "../config.env") });

async function main() {
  console.log("🚀 Deploying Solana Staking Contract to Testnet...\n");

  // Setup provider
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const programId = new PublicKey("9zbbGQ1crgrG9dj7UXCTbx9JkXm522fg7AprTwSBHoa6");
  
  console.log("📋 Configuration:");
  console.log(`   Program ID: ${programId.toString()}`);
  console.log(`   Network: ${provider.connection.rpcEndpoint}`);
  console.log(`   Admin: ${provider.wallet.publicKey.toString()}\n`);

  // Load wallet keypair
  const keypairPath = process.env.ANCHOR_WALLET || "/Users/soloking/.config/solana/testnet-keypair.json";
  const keypairData = JSON.parse(fs.readFileSync(keypairPath, "utf8"));
  const walletKeypair = Keypair.fromSecretKey(new Uint8Array(keypairData));

  const admin = walletKeypair.publicKey;
  const treasury = Keypair.generate();
  const referralPool = Keypair.generate();
  const cashbackPool = Keypair.generate();

  console.log("🔑 Keypairs:");
  console.log(`   Admin: ${admin.toString()}`);
  console.log(`   Treasury: ${treasury.toString()}`);
  console.log(`   Referral Pool: ${referralPool.publicKey.toString()}`);
  console.log(`   Cashback Pool: ${cashbackPool.publicKey.toString()}\n`);

  try {
    // Step 1: Find PDAs first
    const [programState] = PublicKey.findProgramAddressSync(
      [Buffer.from("program_state")],
      programId
    );
    const [programVault] = PublicKey.findProgramAddressSync(
      [Buffer.from("program_vault")],
      programId
    );
    const [stakingTier] = PublicKey.findProgramAddressSync(
      [Buffer.from("staking_tier"), new anchor.BN(1).toArrayLike(Buffer, "le", 1)],
      programId
    );

    console.log("📍 Program Addresses:");
    console.log(`   Program State: ${programState.toString()}`);
    console.log(`   Program Vault: ${programVault.toString()}`);
    console.log(`   Staking Tier: ${stakingTier.toString()}\n`);

    // Step 2: Check if program is already deployed
    console.log("2️⃣ Checking existing deployment...");
    const program = anchor.workspace.StakingRewardsContract;

    let tokenMint;
    try {
      const existingState = await program.account.programState.fetch(programState);
      tokenMint = existingState.rewardTokenMint;
      console.log(`   ✅ Using existing token mint: ${tokenMint.toString()}\n`);
    } catch (error) {
      console.log("   No existing deployment found, creating new token mint...");
      tokenMint = await createMint(
        provider.connection,
        walletKeypair,
        admin,
        null,
        9 // 9 decimals
      );
      console.log(`   ✅ New token mint created: ${tokenMint.toString()}\n`);
    }

    // Step 3: Create program vault token account
    console.log("3️⃣ Creating program vault token account...");
    const programVaultAta = await getOrCreateAssociatedTokenAccount(
      provider.connection,
      walletKeypair,
      tokenMint,
      programVault,
      true // allowOwnerOffCurve for PDA
    );
    console.log(`   ✅ Program Vault ATA: ${programVaultAta.address.toString()}\n`);

    // Step 4: Mint initial tokens
    console.log("4️⃣ Minting initial tokens...");
    await mintTo(
      provider.connection,
      walletKeypair,
      tokenMint,
      programVaultAta.address,
      admin,
      1000000 * 10 ** 9 // 1,000,000 tokens
    );
    console.log(`   ✅ Minted 1,000,000 tokens\n`);

    // Step 5: Create referral and cashback pool token accounts
    console.log("5️⃣ Creating referral and cashback pool token accounts...");
    const referralPoolAta = await getOrCreateAssociatedTokenAccount(
      provider.connection,
      walletKeypair,
      tokenMint,
      referralPool.publicKey,
      false
    );
    const cashbackPoolAta = await getOrCreateAssociatedTokenAccount(
      provider.connection,
      walletKeypair,
      tokenMint,
      cashbackPool.publicKey,
      false
    );
    console.log(`   ✅ Referral Pool ATA: ${referralPoolAta.address.toString()}`);
    console.log(`   ✅ Cashback Pool ATA: ${cashbackPoolAta.address.toString()}\n`);

    // Step 6: Initialize the program (if not already initialized)
    console.log("6️⃣ Checking if program is already initialized...");

    let programStateAccount;
    try {
      programStateAccount = await program.account.programState.fetch(programState);
      console.log(`   ✅ Program already initialized with admin: ${programStateAccount.admin.toString()}`);
      console.log(`   ✅ Using existing token mint: ${programStateAccount.rewardTokenMint.toString()}\n`);

      // Update tokenMint to use the existing one
      tokenMint = programStateAccount.rewardTokenMint;
    } catch (error) {
      console.log("   Program not initialized yet, initializing now...");
      await program.methods
        .initialize(
          admin,
          tokenMint,
          treasury.publicKey,
          referralPool.publicKey,
          cashbackPool.publicKey
        )
        .accounts({
          programState: programState,
          programVault: programVault,
          admin: admin,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .signers([walletKeypair])
        .rpc();

      console.log(`   ✅ Program initialized successfully\n`);

      // Fetch the newly created state
      programStateAccount = await program.account.programState.fetch(programState);
    }

    // Step 7: Create default staking tier (if not already created)
    console.log("7️⃣ Checking default staking tier...");
    try {
      const stakingTierAccount = await program.account.stakingTier.fetch(stakingTier);
      console.log(`   ✅ Staking tier already exists with multiplier: ${stakingTierAccount.multiplier.toString()}\n`);
    } catch (error) {
      console.log("   Creating default staking tier...");
      await program.methods
        .createStakingTier(
          1, // tier_id
          new anchor.BN(150), // 150% multiplier
          1, // min_duration_months
          60 // max_duration_months (5 years for long-term staking)
        )
        .accounts({
          stakingTier: stakingTier,
          admin: admin,
          programState: programState,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .signers([walletKeypair])
        .rpc();
      console.log(`   ✅ Default staking tier created\n`);
    }

    // Step 8: Enable Token Extensions (KYC + pause capability)
    console.log("8️⃣ Checking Token Extensions status...");
    const currentState = await program.account.programState.fetch(programState);
    if (currentState.useTokenExtensions) {
      console.log(`   ✅ Token Extensions already enabled\n`);
    } else {
      console.log("   Enabling Token Extensions for KYC + pause functionality...");
      await program.methods
        .enableTokenExtensions()
        .accounts({
          programState: programState,
          admin: admin,
        })
        .signers([walletKeypair])
        .rpc();
      console.log(`   ✅ Token Extensions enabled for future KYC + pause features\n`);
    }

    // Step 9: Save deployment info
    const deploymentInfo = {
      network: "testnet",
      programId: programId.toString(),
      admin: admin.toString(),
      treasury: treasury.publicKey.toString(),
      referralPool: referralPool.publicKey.toString(),
      cashbackPool: cashbackPool.publicKey.toString(),
      tokenMint: tokenMint.toString(),
      programState: programState.toString(),
      programVault: programVault.toString(),
      stakingTier: stakingTier.toString(),
      programVaultTokenAccount: programVaultAta.address.toString(),
      referralPoolTokenAccount: referralPoolAta.address.toString(),
      cashbackPoolTokenAccount: cashbackPoolAta.address.toString(),
      deploymentDate: new Date().toISOString()
    };

    const deploymentPath = path.join(__dirname, "../deployments/testnet-deployment.json");
    fs.writeFileSync(deploymentPath, JSON.stringify(deploymentInfo, null, 2));

    console.log("💾 Deployment info saved to: deployments/testnet-deployment.json\n");

    // Step 10: Verify deployment
    console.log("🔟 Verifying deployment...");
    const vaultBalance = await getAccount(provider.connection, programVaultAta.address);

    console.log("📊 Deployment Summary:");
    console.log(`   Token Mint: ${tokenMint.toString()}`);
    console.log(`   Program State: ${programState.toString()}`);
    console.log(`   Program Vault: ${programVault.toString()}`);
    console.log(`   Staking Tier: ${stakingTier.toString()}`);
    console.log(`   Vault Balance: ${vaultBalance.amount.toString()}\n`);

    console.log("🎉 DEPLOYMENT SUCCESSFUL!");
    console.log("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    console.log(`📋 Contract Address: ${programId.toString()}`);
    console.log(`🪙 Token Mint: ${tokenMint.toString()}`);
    console.log(`🏛️  Program State: ${programState.toString()}`);
    console.log(`💰 Program Vault: ${programVault.toString()}`);
    console.log("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    console.log("🔗 Testnet Explorer Links:");
    console.log(`   Program: https://explorer.solana.com/address/${programId.toString()}?cluster=testnet`);
    console.log(`   Token: https://explorer.solana.com/address/${tokenMint.toString()}?cluster=testnet`);
    console.log(`   Program State: https://explorer.solana.com/address/${programState.toString()}?cluster=testnet\n`);

    console.log("✅ Contract is ready for testing on Solana Testnet!");

  } catch (error) {
    console.error("❌ Deployment failed:", error);
    process.exit(1);
  }
}

main().catch((error) => {
  console.error("❌ Script failed:", error);
  process.exit(1);
});
