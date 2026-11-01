# Bouncer program

The Solana program behind [bouncer.family](https://bouncer.family), a memecoin launchpad with a door policy against bundlers.

**Official $BOUNCER token:** [`6SsWu1LfJtHq7XikR3o1qWobHMxkAWEupXp7qCqKpump`](https://solscan.io/token/6SsWu1LfJtHq7XikR3o1qWobHMxkAWEupXp7qCqKpump) · [DexScreener](https://dexscreener.com/solana/6SsWu1LfJtHq7XikR3o1qWobHMxkAWEupXp7qCqKpump) · [pump.fun](https://pump.fun/coin/6SsWu1LfJtHq7XikR3o1qWobHMxkAWEupXp7qCqKpump). This is the only official token; always check the address before you buy.

**Program ID (mainnet):** [`BNCo74BKMR2jg4FLcvKpwbXJdABMzvYMzNwwaqefqnmu`](https://solscan.io/account/BNCo74BKMR2jg4FLcvKpwbXJdABMzvYMzNwwaqefqnmu)

## Door policy

- **X required.** A wallet can only buy or create coins once it holds a `Permit`. The gatekeeper issues a permit after the wallet's owner logs in with X and links the wallet. One X account links one wallet, and a wallet can only be linked once.
- **3% max per wallet.** A wallet can hold at most 3% of a coin's supply (30M of 1B tokens) from the curve. The cap is its buys minus its own sells, so selling back to the curve from the same wallet frees the limit again. Transferring tokens away does not.
- **Bonding curve.** Every coin trades on a constant-product curve with 1,000,000,000 tokens. The full supply is minted to the curve and the mint authority is removed at creation, so nobody can mint more.
- **Graduation.** When the curve has raised 85 SOL, trading on the curve stops. The migrator withdraws the liquidity and creates a PumpSwap pool at the curve's final price. The LP tokens are burned, so the liquidity is locked forever.
- **Fees.** 1% of every trade goes to the treasury. There is no migration fee; only the network rent for creating the pool (about 0.06 SOL) is held back from the curve's SOL.

All limits are checked by the program itself, not by the website. A transaction that breaks a rule fails on-chain.

## Accounts and roles

| Role | Address | What it can do |
| --- | --- | --- |
| Admin / upgrade authority | [`Azwb1jVDwdMouQnUNH3ZKb1oMWki8TE8zWMsrJ4nypMV`](https://solscan.io/account/Azwb1jVDwdMouQnUNH3ZKb1oMWki8TE8zWMsrJ4nypMV) | Update config, pause, upgrade the program |
| Gatekeeper | [`Ap7RopckKt6EEcu8jJS7ZvEpL2qqg2Ar2JqyHvdJv3Td`](https://solscan.io/account/Ap7RopckKt6EEcu8jJS7ZvEpL2qqg2Ar2JqyHvdJv3Td) | Issue and revoke wallet permits |
| Migrator | [`GX8pL2uDzrJnCnjZ4BLuXm64kTDig9M3eyeRxnksj19a`](https://solscan.io/account/GX8pL2uDzrJnCnjZ4BLuXm64kTDig9M3eyeRxnksj19a) | Move a full curve to PumpSwap |
| Treasury | [`4xCRV6tSwuoL4Wko2Q6onSMUYszwziCjHsWMuHYufo25`](https://solscan.io/account/4xCRV6tSwuoL4Wko2Q6onSMUYszwziCjHsWMuHYufo25) | Receives the 1% trade fee |

The current values live in the on-chain `Config` account (PDA with seed `config`).

## Layout

- `programs/bouncer/src/lib.rs`: instructions, accounts, events and errors
- `programs/bouncer/src/math.rs`: bonding curve math
- `tests/bouncer.ts`: tests against a local validator, including the 3% cap, permits and migration
- `idl/bouncer.json`: the program's IDL

## Build and test

Needs Rust, the Solana CLI and Anchor 1.2.2.

```sh
npm ci
anchor build
anchor test
```
