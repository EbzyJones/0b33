//! ID-space exhaustion boundary tests.
//!
//! Seeds `DataKey::NextId` to `MAX_TOTAL_SUBSCRIPTIONS - 1` via `env.as_contract`
//! so that exactly one more allocation succeeds, then asserts that every
//! subsequent call returns the correct limit error without wrapping the counter.
//!
//! # Errors under test
//!
//! | Entry-point | Error at `MAX_TOTAL_SUBSCRIPTIONS` |
//! |---|---|
//! | `create_subscription` | `SubscriptionLimitReached` (#6001) |
//! | `create_subscription_with_token` | `SubscriptionLimitReached` (#6001) |
//! | `create_subscription_from_plan` | `SubscriptionLimitReached` (#6001) |
//!
//! All three creation paths enforce the global ceiling `MAX_TOTAL_SUBSCRIPTIONS`
//! and return `SubscriptionLimitReached` once the counter reaches it; the
//! counter is left unchanged on the failing call.

#![cfg(test)]

use soroban_sdk::{testutils::Address as _, Address, Env, Symbol};
use subscription_vault::{
    DataKey, Error, SubscriptionVault, SubscriptionVaultClient, MAX_TOTAL_SUBSCRIPTIONS,
};

// ── shared constants ──────────────────────────────────────────────────────────

const AMOUNT: i128 = 10_000_000;
const INTERVAL: u64 = 30 * 24 * 60 * 60; // 30 days

// ── helpers ───────────────────────────────────────────────────────────────────

/// Minimal contract setup: register, init with a real SAC token, return client + token address.
fn setup() -> (Env, SubscriptionVaultClient<'static>, Address) {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    let vault_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &vault_id);
    client.init(
        &token,
        &6u32,
        &admin,
        &1_000_000i128,
        &(7 * 24 * 60 * 60u64),
    );

    (env, client, token)
}

/// Seed `DataKey::NextId` to `value` directly in contract storage.
fn seed_next_id(env: &Env, contract: &Address, value: u32) {
    env.as_contract(contract, || {
        env.storage().persistent().set(&DataKey::NextId, &value);
    });
}

/// Read `DataKey::NextId` from contract storage.
fn read_next_id(env: &Env, contract: &Address) -> u32 {
    env.as_contract(contract, || {
        env.storage()
            .persistent()
            .get(&DataKey::NextId)
            .unwrap_or(0u32)
    })
}

// ── create_subscription ───────────────────────────────────────────────────────

/// At `MAX_TOTAL_SUBSCRIPTIONS - 1` exactly one allocation succeeds and returns
/// id `MAX_TOTAL_SUBSCRIPTIONS - 1`.
#[test]
fn create_subscription_last_id_succeeds() {
    let (env, client, _) = setup();
    seed_next_id(&env, &client.address, MAX_TOTAL_SUBSCRIPTIONS - 1);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
        &None::<u64>,
        &None::<u32>,
        &None::<Symbol>,
    );

    assert_eq!(
        id,
        MAX_TOTAL_SUBSCRIPTIONS - 1,
        "last valid id must be MAX_TOTAL_SUBSCRIPTIONS - 1"
    );
    assert_eq!(
        read_next_id(&env, &client.address),
        MAX_TOTAL_SUBSCRIPTIONS,
        "counter must advance to MAX_TOTAL_SUBSCRIPTIONS after final allocation"
    );
}

/// After the counter reaches `MAX_TOTAL_SUBSCRIPTIONS`, `create_subscription`
/// returns `SubscriptionLimitReached`.
#[test]
fn create_subscription_at_max_returns_limit_reached() {
    let (env, client, _) = setup();
    seed_next_id(&env, &client.address, MAX_TOTAL_SUBSCRIPTIONS);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let err = client
        .try_create_subscription(
            &subscriber,
            &merchant,
            &AMOUNT,
            &INTERVAL,
            &false,
            &None::<i128>,
            &None::<u64>,
            &None::<u32>,
            &None::<Symbol>,
        )
        .expect_err("must fail when counter is at MAX_TOTAL_SUBSCRIPTIONS");

    assert_eq!(err, Ok(Error::SubscriptionLimitReached));
}

/// Counter must not change after a failed allocation (no wrap, no increment).
#[test]
fn create_subscription_counter_unchanged_after_failure() {
    let (env, client, _) = setup();
    seed_next_id(&env, &client.address, MAX_TOTAL_SUBSCRIPTIONS);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let _ = client.try_create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
        &None::<u64>,
        &None::<u32>,
        &None::<Symbol>,
    );

    assert_eq!(
        read_next_id(&env, &client.address),
        MAX_TOTAL_SUBSCRIPTIONS,
        "counter must stay at MAX_TOTAL_SUBSCRIPTIONS after failed allocation"
    );
}

// ── create_subscription_with_token ───────────────────────────────────────────

/// At `MAX_TOTAL_SUBSCRIPTIONS - 1` exactly one allocation succeeds via the token-specific path.
#[test]
fn create_subscription_with_token_last_id_succeeds() {
    let (env, client, token) = setup();
    seed_next_id(&env, &client.address, MAX_TOTAL_SUBSCRIPTIONS - 1);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let id = client.create_subscription_with_token(
        &subscriber,
        &merchant,
        &token,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
        &None::<u64>,
        &None::<u32>,
        &None::<Symbol>,
    );

    assert_eq!(id, MAX_TOTAL_SUBSCRIPTIONS - 1);
    assert_eq!(
        read_next_id(&env, &client.address),
        MAX_TOTAL_SUBSCRIPTIONS
    );
}

/// After the counter reaches `MAX_TOTAL_SUBSCRIPTIONS`, `create_subscription_with_token`
/// returns `SubscriptionLimitReached`.
#[test]
fn create_subscription_with_token_at_max_returns_limit_reached() {
    let (env, client, token) = setup();
    seed_next_id(&env, &client.address, MAX_TOTAL_SUBSCRIPTIONS);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let err = client
        .try_create_subscription_with_token(
            &subscriber,
            &merchant,
            &token,
            &AMOUNT,
            &INTERVAL,
            &false,
            &None::<i128>,
            &None::<u64>,
            &None::<u32>,
            &None::<Symbol>,
        )
        .expect_err("must fail when counter is at MAX_TOTAL_SUBSCRIPTIONS");

    assert_eq!(err, Ok(Error::SubscriptionLimitReached));
}

/// Counter must not change after a failed `create_subscription_with_token`.
#[test]
fn create_subscription_with_token_counter_unchanged_after_failure() {
    let (env, client, token) = setup();
    seed_next_id(&env, &client.address, MAX_TOTAL_SUBSCRIPTIONS);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let _ = client.try_create_subscription_with_token(
        &subscriber,
        &merchant,
        &token,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
        &None::<u64>,
        &None::<u32>,
        &None::<Symbol>,
    );

    assert_eq!(
        read_next_id(&env, &client.address),
        MAX_TOTAL_SUBSCRIPTIONS
    );
}

// ── create_subscription_from_plan ────────────────────────────────────────────

/// At `MAX_TOTAL_SUBSCRIPTIONS - 1` exactly one allocation succeeds via the plan path.
#[test]
fn create_subscription_from_plan_last_id_succeeds() {
    let (env, client, _) = setup();

    let merchant = Address::generate(&env);
    let plan_id = client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);

    seed_next_id(&env, &client.address, MAX_TOTAL_SUBSCRIPTIONS - 1);

    let subscriber = Address::generate(&env);
    let id = client.create_subscription_from_plan(&subscriber, &plan_id, &None::<Symbol>);

    assert_eq!(id, MAX_TOTAL_SUBSCRIPTIONS - 1);
    assert_eq!(
        read_next_id(&env, &client.address),
        MAX_TOTAL_SUBSCRIPTIONS
    );
}

/// After the counter reaches `MAX_TOTAL_SUBSCRIPTIONS`, `create_subscription_from_plan`
/// returns `SubscriptionLimitReached` (#6001), the same as the other creation paths,
/// because `do_create_subscription_from_plan` now enforces the `MAX_TOTAL_SUBSCRIPTIONS`
/// ceiling before allocating an ID.
#[test]
fn create_subscription_from_plan_at_max_returns_limit_reached() {
    let (env, client, _) = setup();

    let merchant = Address::generate(&env);
    let plan_id = client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);

    seed_next_id(&env, &client.address, MAX_TOTAL_SUBSCRIPTIONS);

    let subscriber = Address::generate(&env);
    let err = client
        .try_create_subscription_from_plan(&subscriber, &plan_id, &None::<Symbol>)
        .expect_err("must fail when counter is at MAX_TOTAL_SUBSCRIPTIONS");

    assert_eq!(err, Ok(Error::SubscriptionLimitReached));
}

/// Counter must not change after a failed `create_subscription_from_plan`.
#[test]
fn create_subscription_from_plan_counter_unchanged_after_failure() {
    let (env, client, _) = setup();

    let merchant = Address::generate(&env);
    let plan_id = client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);

    seed_next_id(&env, &client.address, MAX_TOTAL_SUBSCRIPTIONS);

    let subscriber = Address::generate(&env);
    let _ = client.try_create_subscription_from_plan(&subscriber, &plan_id, &None::<Symbol>);

    assert_eq!(
        read_next_id(&env, &client.address),
        MAX_TOTAL_SUBSCRIPTIONS
    );
}
