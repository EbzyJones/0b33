use soroban_sdk::{token, Address, Env};

use crate::types::{DataKey, Error};

/// Asserts the on-chain accounting invariant for `token`:
///
/// ```text
/// contract_token_balance >= TotalAccounted(token)
/// ```
///
/// `TotalAccounted(token)` is the running total of every unit the vault has
/// taken into custody: it rises on deposits and falls whenever funds leave the
/// vault (merchant withdrawals, merchant refunds, subscriber withdrawals,
/// dispute payouts). A charge moves value between internal buckets
/// (subscriber prepaid balance -> merchant liability) and therefore leaves it
/// untouched.
///
/// Any balance the vault holds *above* `TotalAccounted` is unaccounted surplus
/// (for example tokens minted straight to the contract) and is admin-recoverable,
/// so `balance` may legitimately exceed `TotalAccounted` — but it must never
/// fall below it. A shortfall means the vault no longer holds enough tokens to
/// cover the balances it claims to owe, i.e. a silent accounting discrepancy.
///
/// This check runs unconditionally on the charge and withdrawal critical paths
/// (see `merchant.rs`); callers propagate the error with `?`, so a violation
/// reverts the surrounding transaction (fail-closed) rather than letting the
/// drift accumulate.
pub fn assert_token_balance_invariant(env: &Env, token: &Address) -> Result<(), Error> {
    // Read the anchor directly instead of going through
    // `accounting::get_total_accounted`, so this guard never rejects a token
    // that has since been removed from the accepted-token list.
    let key = DataKey::TotalAccounted(token.clone());
    let accounted: i128 = env.storage().instance().get(&key).unwrap_or(0i128);

    if accounted < 0 {
        return Err(Error::AccountingInvariantViolation);
    }

    let contract_balance = token::Client::new(env, token).balance(&env.current_contract_address());
    if contract_balance < accounted {
        return Err(Error::AccountingInvariantViolation);
    }

    Ok(())
}
