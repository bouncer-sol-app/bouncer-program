import anchor from "@anchor-lang/core";
import type { Program, Wallet } from "@anchor-lang/core";
import chai from "chai";
const { BN } = anchor;
const assert: Chai.AssertStatic = chai.assert;
import {
  Keypair,
  LAMPORTS_PER_SOL,
  PublicKey,
  SystemProgram,
  Transaction,
} from "@solana/web3.js";
import {
  TOKEN_2022_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
  getAssociatedTokenAddressSync,
  getAccount,
  createAssociatedTokenAccountIdempotentInstruction,
  createTransferCheckedInstruction,
} from "@solana/spl-token";
import type { Bouncer } from "../target/types/bouncer";

const provider = anchor.AnchorProvider.env();
anchor.setProvider(provider);
const program = anchor.workspace.Bouncer as Program<Bouncer>;
const connection = provider.connection;
const admin = provider.wallet as Wallet;

const UPGRADEABLE_LOADER = new PublicKey("BPFLoaderUpgradeab1e11111111111111111111111");
const MAX_BUY = new BN("30000000000000"); // 30M tokens with 6 decimals
const MIGRATION_LAMPORTS = new BN(5 * LAMPORTS_PER_SOL);

const pda = (seeds: (Buffer | Uint8Array)[]) => PublicKey.findProgramAddressSync(seeds, program.programId)[0];
const configPda = pda([Buffer.from("config")]);
const permitPda = (w: PublicKey) => pda([Buffer.from("permit"), w.toBuffer()]);
const curvePda = (m: PublicKey) => pda([Buffer.from("curve"), m.toBuffer()]);
const positionPda = (m: PublicKey, w: PublicKey) => pda([Buffer.from("position"), m.toBuffer(), w.toBuffer()]);
const ata = (m: PublicKey, owner: PublicKey) => getAssociatedTokenAddressSync(m, owner, true, TOKEN_2022_PROGRAM_ID);

async function fund(to: PublicKey, sol: number) {
  const tx = new Transaction().add(
    SystemProgram.transfer({ fromPubkey: admin.publicKey, toPubkey: to, lamports: sol * LAMPORTS_PER_SOL }),
  );
  await provider.sendAndConfirm(tx);
}

async function expectError(p: Promise<unknown>, name: string) {
  try {
    await p;
  } catch (e) {
    const msg = String(e) + JSON.stringify((e as { logs?: string[] }).logs ?? []);
    assert.include(msg, name, `expected ${name}, got: ${msg.slice(0, 400)}`);
    return;
  }
  assert.fail(`expected ${name} but the transaction succeeded`);
}

describe("bouncer", () => {
  const gatekeeper = Keypair.generate();
  const migrator = Keypair.generate();
  const feeRecipient = Keypair.generate();
  const creator = Keypair.generate();
  const alice = Keypair.generate();
  const bob = Keypair.generate();
  const stranger = Keypair.generate();
  const crowd = Array.from({ length: 14 }, () => Keypair.generate());
  const mint = Keypair.generate();

  const buy = (who: Keypair, sol: number, minOut = 0) =>
    program.methods
      .buy(new BN(Math.round(sol * LAMPORTS_PER_SOL)), new BN(minOut))
      .accountsPartial({
        buyer: who.publicKey,
        config: configPda,
        feeRecipient: feeRecipient.publicKey,
        permit: permitPda(who.publicKey),
        curve: curvePda(mint.publicKey),
        mint: mint.publicKey,
        vault: ata(mint.publicKey, curvePda(mint.publicKey)),
        buyerTokenAccount: ata(mint.publicKey, who.publicKey),
        position: positionPda(mint.publicKey, who.publicKey),
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .signers([who])
      .rpc();

  const register = (wallet: PublicKey, signer = gatekeeper) =>
    program.methods
      .registerWallet(wallet)
      .accountsPartial({ gatekeeper: signer.publicKey, config: configPda, permit: permitPda(wallet) })
      .signers([signer])
      .rpc();

  before(async () => {
    for (const k of [gatekeeper, migrator, feeRecipient, creator, alice, bob, stranger, ...crowd]) await fund(k.publicKey, 3);
  });

  it("only the upgrade authority can initialize", async () => {
    const programData = PublicKey.findProgramAddressSync([program.programId.toBuffer()], UPGRADEABLE_LOADER)[0];
    const params = {
      gatekeeper: gatekeeper.publicKey,
      migrator: migrator.publicKey,
      feeRecipient: feeRecipient.publicKey,
      feeBps: 100,
      migrationLamports: MIGRATION_LAMPORTS,
    };
    await expectError(
      program.methods
        .initialize(params)
        .accountsPartial({ admin: stranger.publicKey, config: configPda, program: program.programId, programData })
        .signers([stranger])
        .rpc(),
      "Unauthorized",
    );
    await program.methods
      .initialize(params)
      .accountsPartial({ admin: admin.publicKey, config: configPda, program: program.programId, programData })
      .rpc();
    const cfg = await program.account.config.fetch(configPda);
    assert.equal(cfg.feeBps, 100);
  });

  it("only the gatekeeper can register wallets", async () => {
    await expectError(register(stranger.publicKey, stranger), "Unauthorized");
    for (const k of [creator, alice, bob, ...crowd]) await register(k.publicKey);
  });

  it("a wallet without a permit cannot create a coin", async () => {
    const m = Keypair.generate();
    await expectError(
      program.methods
        .createToken("Nope", "NOPE", "")
        .accountsPartial({
          creator: stranger.publicKey,
          config: configPda,
          permit: permitPda(stranger.publicKey),
          mint: m.publicKey,
          curve: curvePda(m.publicKey),
          vault: ata(m.publicKey, curvePda(m.publicKey)),
          tokenProgram: TOKEN_2022_PROGRAM_ID,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .signers([stranger, m])
        .rpc(),
      "AccountNotInitialized",
    );
  });

  it("creates a coin with the full supply on the curve", async () => {
    await program.methods
      .createToken("Fair Coin", "FAIR", "https://example.com/fair.json")
      .accountsPartial({
        creator: creator.publicKey,
        config: configPda,
        permit: permitPda(creator.publicKey),
        mint: mint.publicKey,
        curve: curvePda(mint.publicKey),
        vault: ata(mint.publicKey, curvePda(mint.publicKey)),
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .signers([creator, mint])
      .rpc();
    const vault = await getAccount(connection, ata(mint.publicKey, curvePda(mint.publicKey)), undefined, TOKEN_2022_PROGRAM_ID);
    assert.equal(vault.amount.toString(), "1000000000000000");
  });

  it("a wallet without a permit cannot buy", async () => {
    await expectError(buy(stranger, 0.1), "AccountNotInitialized");
  });

  const sell = (who: Keypair, amount: string | number, withPosition = true) =>
    program.methods
      .sell(new BN(amount.toString()), new BN(0))
      .accountsPartial({
        seller: who.publicKey,
        config: configPda,
        feeRecipient: feeRecipient.publicKey,
        curve: curvePda(mint.publicKey),
        mint: mint.publicKey,
        vault: ata(mint.publicKey, curvePda(mint.publicKey)),
        sellerTokenAccount: ata(mint.publicKey, who.publicKey),
        position: withPosition ? positionPda(mint.publicKey, who.publicKey) : null,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .signers([who])
      .rpc();

  const balance = async (owner: PublicKey) =>
    (await getAccount(connection, ata(mint.publicKey, owner), undefined, TOKEN_2022_PROGRAM_ID)).amount.toString();

  it("selling from the linked wallet frees the limit, so it can buy again", async () => {
    await buy(alice, 0.5);
    const pos = await program.account.position.fetch(positionPda(mint.publicKey, alice.publicKey));
    assert.isTrue(pos.holding.gt(new BN(0)));

    // Over the cap in one go
    await expectError(buy(alice, 1.5), "WalletCapExceeded");

    // Sell everything from the same wallet: the limit is free again
    await sell(alice, await balance(alice.publicKey));
    const after = await program.account.position.fetch(positionPda(mint.publicKey, alice.publicKey));
    assert.equal(after.holding.toString(), "0");
    assert.equal(after.bought.toString(), pos.bought.toString(), "lifetime total stays");

    // ...and it can buy again
    await buy(alice, 0.5);
  });

  it("tokens sent to another wallet and sold there still count against the limit", async () => {
    await buy(bob, 0.8);
    const pos = await program.account.position.fetch(positionPda(mint.publicKey, bob.publicKey));
    const amount = BigInt(await balance(bob.publicKey));

    // Bob sends everything to a fresh wallet
    const tx = new Transaction().add(
      createAssociatedTokenAccountIdempotentInstruction(
        bob.publicKey, ata(mint.publicKey, stranger.publicKey), stranger.publicKey, mint.publicKey, TOKEN_2022_PROGRAM_ID,
      ),
      createTransferCheckedInstruction(
        ata(mint.publicKey, bob.publicKey), mint.publicKey, ata(mint.publicKey, stranger.publicKey),
        bob.publicKey, amount, 6, [], TOKEN_2022_PROGRAM_ID,
      ),
    );
    await provider.sendAndConfirm(tx, [bob]);

    // The fresh wallet (no permit, no position) can sell
    await sell(stranger, amount.toString(), false);

    // Bob's limit is unchanged, so he can't top up past 3%
    const after = await program.account.position.fetch(positionPda(mint.publicKey, bob.publicKey));
    assert.equal(after.holding.toString(), pos.holding.toString());
    await expectError(buy(bob, 0.5), "WalletCapExceeded");
  });

  it("respects slippage", async () => {
    await expectError(buy(crowd[0], 0.1, Number.MAX_SAFE_INTEGER), "SlippageExceeded");
  });

  it("completes the curve at the migration threshold and stops trading", async () => {
    for (const k of crowd) {
      const c = await program.account.curve.fetch(curvePda(mint.publicKey));
      if (c.complete) break;
      await buy(k, 0.45);
    }
    const curve = await program.account.curve.fetch(curvePda(mint.publicKey));
    assert.isTrue(curve.complete, "curve should be complete");
    await expectError(buy(crowd[crowd.length - 1], 0.1), "CurveComplete");
  });

  it("only the migrator (or admin) can withdraw for migration", async () => {
    const accounts = (who: PublicKey) => ({
      migrator: who,
      config: configPda,
      curve: curvePda(mint.publicKey),
      mint: mint.publicKey,
      vault: ata(mint.publicKey, curvePda(mint.publicKey)),
      migratorTokenAccount: ata(mint.publicKey, who),
      tokenProgram: TOKEN_2022_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    });
    await expectError(
      program.methods.withdrawForMigration().accountsPartial(accounts(stranger.publicKey)).signers([stranger]).rpc(),
      "Unauthorized",
    );
    const before = await connection.getBalance(migrator.publicKey);
    await program.methods.withdrawForMigration().accountsPartial(accounts(migrator.publicKey)).signers([migrator]).rpc();
    const afterBal = await connection.getBalance(migrator.publicKey);
    assert.isTrue(afterBal > before + 4 * LAMPORTS_PER_SOL, "migrator should receive the raised SOL");
    const curve = await program.account.curve.fetch(curvePda(mint.publicKey));
    assert.isTrue(curve.migrated);
  });
});
