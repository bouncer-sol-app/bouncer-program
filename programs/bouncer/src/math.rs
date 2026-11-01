//! Curve constants and pure math, kept separate so it can be unit-tested on any machine.

pub const DECIMALS: u8 = 6;
const ONE_TOKEN: u64 = 1_000_000; // 10^DECIMALS
const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// 1,000,000,000 tokens.
pub const TOTAL_SUPPLY: u64 = 1_000_000_000 * ONE_TOKEN;
/// 3% of the supply per wallet, counted over all buys.
pub const MAX_BUY_PER_WALLET: u64 = TOTAL_SUPPLY / 100 * 3;
/// Tokens that can be bought from the curve. The rest seeds the DEX pool.
pub const CURVE_SELLABLE_TOKENS: u64 = 793_100_000 * ONE_TOKEN;
pub const INITIAL_VIRTUAL_SOL: u64 = 30 * LAMPORTS_PER_SOL;
pub const INITIAL_VIRTUAL_TOKENS: u64 = 1_073_000_000 * ONE_TOKEN;
/// Fees can never be set above 3%.
pub const MAX_FEE_BPS: u16 = 300;
const BPS: u128 = 10_000;

#[derive(Debug, PartialEq, Eq)]
pub struct BuyQuote {
    pub tokens_out: u64,
    /// Lamports that go into the curve.
    pub sol_net: u64,
    /// Lamports that go to the fee recipient.
    pub fee: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SellQuote {
    /// Lamports that leave the curve.
    pub sol_gross: u64,
    /// Lamports the seller receives.
    pub sol_net: u64,
    pub fee: u64,
}

fn ceil_div(a: u128, b: u128) -> Option<u128> {
    if b == 0 {
        return None;
    }
    Some(a.checked_add(b - 1)? / b)
}

/// Quote a buy that spends at most `max_sol_in` lamports, fee included.
/// Rounds in the curve's favour. If the curve has fewer tokens left than the buy would get,
/// the buyer gets the rest and pays only what that costs.
pub fn quote_buy(
    virtual_sol: u64,
    virtual_tokens: u64,
    real_tokens_left: u64,
    max_sol_in: u64,
    fee_bps: u16,
) -> Option<BuyQuote> {
    let vs = virtual_sol as u128;
    let vt = virtual_tokens as u128;
    let k = vs.checked_mul(vt)?;
    let bps = fee_bps as u128;

    let mut net = (max_sol_in as u128).checked_mul(BPS)? / (BPS + bps);
    let new_vt = ceil_div(k, vs.checked_add(net)?)?;
    let mut out = vt.checked_sub(new_vt)?;

    if out > real_tokens_left as u128 {
        out = real_tokens_left as u128;
        let remaining_vt = vt.checked_sub(out)?;
        net = ceil_div(k, remaining_vt)?.checked_sub(vs)?;
    }
    let fee = net.checked_mul(bps)? / BPS;

    Some(BuyQuote {
        tokens_out: u64::try_from(out).ok()?,
        sol_net: u64::try_from(net).ok()?,
        fee: u64::try_from(fee).ok()?,
    })
}

/// Quote selling `token_amount` tokens back to the curve. Rounds in the curve's favour.
pub fn quote_sell(virtual_sol: u64, virtual_tokens: u64, token_amount: u64, fee_bps: u16) -> Option<SellQuote> {
    let vs = virtual_sol as u128;
    let vt = virtual_tokens as u128;
    let k = vs.checked_mul(vt)?;
    let new_vs = ceil_div(k, vt.checked_add(token_amount as u128)?)?;
    let gross = vs.checked_sub(new_vs)?;
    let fee = gross.checked_mul(fee_bps as u128)? / BPS;
    let net = gross - fee;
    Some(SellQuote {
        sol_gross: u64::try_from(gross).ok()?,
        sol_net: u64::try_from(net).ok()?,
        fee: u64::try_from(fee).ok()?,
    })
}

/// Bytes needed for the Token-2022 metadata TLV entry (generous upper bound).
pub fn metadata_space(name: &str, symbol: &str, uri: &str) -> usize {
    // TLV header (4) + update authority (32) + mint (32)
    // + three length-prefixed strings + empty additional-metadata vec (4)
    4 + 32 + 32 + (4 + name.len()) + (4 + symbol.len()) + (4 + uri.len()) + 4
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOL: u64 = LAMPORTS_PER_SOL;

    #[test]
    fn cap_is_thirty_million_tokens() {
        assert_eq!(MAX_BUY_PER_WALLET, 30_000_000 * ONE_TOKEN);
    }

    #[test]
    fn small_buy_gets_tokens_and_pays_fee() {
        let q = quote_buy(INITIAL_VIRTUAL_SOL, INITIAL_VIRTUAL_TOKENS, CURVE_SELLABLE_TOKENS, SOL, 100).unwrap();
        assert!(q.tokens_out > 30_000_000 * ONE_TOKEN && q.tokens_out < 36_000_000 * ONE_TOKEN, "{q:?}");
        assert!(q.sol_net + q.fee <= SOL);
        assert_eq!(q.fee, q.sol_net / 100);
    }

    #[test]
    fn buy_then_sell_never_profits() {
        let q = quote_buy(INITIAL_VIRTUAL_SOL, INITIAL_VIRTUAL_TOKENS, CURVE_SELLABLE_TOKENS, SOL / 2, 100).unwrap();
        let vs = INITIAL_VIRTUAL_SOL + q.sol_net;
        let vt = INITIAL_VIRTUAL_TOKENS - q.tokens_out;
        let s = quote_sell(vs, vt, q.tokens_out, 100).unwrap();
        assert!(s.sol_gross <= q.sol_net, "curve must not lose SOL on a round trip");
        assert!(s.sol_net < SOL / 2);
    }

    #[test]
    fn zero_fee_round_trip_does_not_drain_curve() {
        for amount in [1u64, 999, SOL / 1000, SOL, 7 * SOL] {
            let q = quote_buy(INITIAL_VIRTUAL_SOL, INITIAL_VIRTUAL_TOKENS, CURVE_SELLABLE_TOKENS, amount, 0).unwrap();
            let s = quote_sell(INITIAL_VIRTUAL_SOL + q.sol_net, INITIAL_VIRTUAL_TOKENS - q.tokens_out, q.tokens_out, 0).unwrap();
            assert!(s.sol_gross <= q.sol_net);
        }
    }

    #[test]
    fn last_buy_is_capped_at_remaining_tokens() {
        let left = 1_000 * ONE_TOKEN;
        let q = quote_buy(INITIAL_VIRTUAL_SOL, INITIAL_VIRTUAL_TOKENS, left, 100 * SOL, 100).unwrap();
        assert_eq!(q.tokens_out, left);
        assert!(q.sol_net < SOL, "should only pay for what's left: {q:?}");
    }

    #[test]
    fn migration_point_is_around_50k_at_180_usd() {
        // Raised SOL needed for ~$50k market cap when SOL = $180.
        let raised: u64 = 64_600_000_000; // 64.6 SOL
        let vs = (INITIAL_VIRTUAL_SOL + raised) as u128;
        let vt = (INITIAL_VIRTUAL_SOL as u128 * INITIAL_VIRTUAL_TOKENS as u128) / vs;
        let mc_usd = vs * TOTAL_SUPPLY as u128 / vt * 180 / SOL as u128;
        assert!((48_000..52_000).contains(&(mc_usd as u64)), "mc {mc_usd}");
    }

    #[test]
    fn sell_of_nothing_is_zero() {
        let s = quote_sell(INITIAL_VIRTUAL_SOL, INITIAL_VIRTUAL_TOKENS, 0, 100).unwrap();
        assert_eq!(s.sol_gross, 0);
    }
}
