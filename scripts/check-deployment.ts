import * as anchor from "@coral-xyz/anchor";
import { PublicKey } from "@solana/web3.js";

async function checkDeployment() {
  console.log("🔍 Checking deployment status...\n");

  // Setup provider
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const programId = new PublicKey("AKnc8CqVVCyBuzzyvNEPQZGYCiEiqRneETDSgm1ZU69N");
  const program = anchor.workspace.StakingRewardsContract;

  console.log("📋 Configuration:");
  console.log(`   Program ID: ${programId.toString()}`);
  console.log(`   Network: ${provider.connection.rpcEndpoint}`);
  console.log(`   Admin: ${provider.wallet.publicKey.toString()}\n`);

  // Find PDAs
  const [programState] = PublicKey.findProgramAddressSync(
    [Buffer.from("program_state")],
    programId
  );
  const [programVault] = PublicKey.findProgramAddressSync(
    [Buffer.from("program_vault")],
    programId
  );

  try {
    // Check if program state exists
    const programStateAccount = await program.account.programState.fetch(programState);

    console.log("✅ Program is already deployed and initialized!");
    console.log("📊 Current State:");
    console.log(`   Admin: ${programStateAccount.admin.toString()}`);
    console.log(`   Token Mint: ${programStateAccount.rewardTokenMint.toString()}`);
    console.log(`   Treasury: ${programStateAccount.protocolTreasury.toString()}`);
    console.log(`   Total Staked: ${programStateAccount.totalStaked.toString()}`);
    console.log(`   Reward Pool: ${programStateAccount.rewardPool.toString()}`);
    console.log(`   Current Epoch: ${programStateAccount.currentEpoch.toString()}`);
    console.log(`   Is Paused: ${programStateAccount.isPaused}`);
    console.log(`   Token Extensions Enabled: ${programStateAccount.useTokenExtensions}\n`);

    // Check if staking tier exists
    const [stakingTier] = PublicKey.findProgramAddressSync(
      [Buffer.from("staking_tier"), Buffer.from([1])],
      programId
    );

    try {
      const stakingTierAccount = await program.account.stakingTier.fetch(stakingTier);
      console.log("✅ Default staking tier exists:");
      console.log(`   Tier ID: ${stakingTierAccount.tierId}`);
      console.log(`   Multiplier: ${stakingTierAccount.multiplier.toString()}`);
      console.log(`   Min Duration: ${stakingTierAccount.minDurationMonths} months`);
      console.log(`   Max Duration: ${stakingTierAccount.maxDurationMonths} months`);
      console.log(`   Is Active: ${stakingTierAccount.isActive}\n`);
    } catch (tierError) {
      console.log("⚠️  Default staking tier not found. You may need to create it.\n");
    }

    console.log("🔗 Explorer Links:");
    console.log(`   Program: https://explorer.solana.com/address/${programId.toString()}?cluster=devnet`);
    console.log(`   Token: https://explorer.solana.com/address/${programStateAccount.rewardTokenMint.toString()}?cluster=devnet`);
    console.log(`   Program State: https://explorer.solana.com/address/${programState.toString()}?cluster=devnet\n`);

    console.log("✅ Contract is ready for use!");

  } catch (error) {
    console.log("❌ Program not deployed or not initialized");
    console.log("   Error:", error.message);
    console.log("\n💡 You need to run the deployment script first:");
    console.log("   yarn deploy");
  }
}

checkDeployment().catch(console.error);