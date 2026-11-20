//! Integration tests for the blocklist guard on the two-step admin rotation
//! (`admin.rs` — issue #600 / `#464`).
//!
//! `claim_admin_role` installs `proposal.new_admin` as the stored admin. Admin
//! calls pass auth checks before any blocklist gate, so a blocklisted address
//! must never be installed: the claim is rejected with `Forbidden` and the
//! current admin is left unchanged.
//!
//! These live under `tests/` rather than next to the contract because the
//! `src/test_*` rotation modules are not `mod`-declared in `lib.rs`, so
//! `cargo test` never compiles them.

#![cfg(test)]

use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    Address, Env,
};
use subscription_vault::{Error, SubscriptionVault, SubscriptionVaultClient};

/// Ledger time every test starts from.
const T0: u64 = 1_000_000;
/// `ADMIN_PROPOSAL_COOLDOWN_SECS` — minimum age before a proposal can be claimed.
const COOLDOWN: u64 = 24 * 60 * 60;

fn setup() -> (Env, SubscriptionVaultClient<'static>, Address) {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(T0);

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    client.init(&token, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));

    (env, client, admin)
}

#[test]
fn claim_is_rejected_when_new_admin_is_blocklisted() {
    let (env, client, admin) = setup();
    let new_admin = Address::generate(&env);

    client.propose_admin(&admin, &new_admin);

    // The proposed admin is blocklisted before the claim.
    client.add_to_blocklist(&admin, &new_admin, &None);
    assert!(client.is_blocklisted(&new_admin));

    env.ledger().set_timestamp(T0 + COOLDOWN);

    let res = client.try_claim_admin_role(&new_admin);
    assert_eq!(res, Err(Ok(Error::Forbidden)));

    // The blocklisted address must not have been installed as admin.
    assert_eq!(client.get_admin().unwrap(), admin);
}

#[test]
fn claim_succeeds_for_non_blocklisted_new_admin() {
    let (env, client, admin) = setup();
    let new_admin = Address::generate(&env);

    client.propose_admin(&admin, &new_admin);
    env.ledger().set_timestamp(T0 + COOLDOWN);

    client.claim_admin_role(&new_admin);
    assert_eq!(client.get_admin().unwrap(), new_admin);
}
