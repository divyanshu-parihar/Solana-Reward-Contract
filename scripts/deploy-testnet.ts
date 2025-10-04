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

async function main() {
  console.log("🚀 Deploying Solana Staking Contract to Testnet...\n");

  // Setup provider
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const programId = new PublicKey("AKnc8CqVVCyBuzzyvNEPQZGYCiEiqRneETDSgm1ZU69N");
  
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
    // Step 1: Create reward token mint
    console.log("1️⃣ Creating reward token mint...");
    const tokenMint = await createMint(
      provider.connection,
      walletKeypair,
      admin,
      null,
      9 // 9 decimals
    );
    console.log(`   ✅ Token Mint: ${tokenMint.toString()}\n`);

    // Step 2: Find PDAs
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

    // Step 3: Create program vault token account
    console.log("2️⃣ Creating program vault token account...");
    const programVaultAta = await getOrCreateAssociatedTokenAccount(
      provider.connection,
      walletKeypair,
      tokenMint,
      programVault,
      true // allowOwnerOffCurve for PDA
    );
    console.log(`   ✅ Program Vault ATA: ${programVaultAta.address.toString()}\n`);

    // Step 4: Mint initial tokens
    console.log("3️⃣ Minting initial tokens...");
    await mintTo(
      provider.connection,
      walletKeypair,
      tokenMint,
      programVaultAta.address,
      admin,
      1000000 * 10 ** 9 // 1,000,000 tokens
    );
    console.log(`   ✅ Minted 1,000,000 tokens\n`);

    // Step 4: Create referral and cashback pool token accounts
    console.log("4️⃣ Creating referral and cashback pool token accounts...");
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

    // Step 5: Initialize the program
    console.log("5️⃣ Initializing the program...");
    const program = anchor.workspace.StakingRewardsContract;

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

    // Step 6: Save deployment info
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

    // Step 7: Verify deployment
    console.log("6️⃣ Verifying deployment...");
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
