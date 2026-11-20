#![cfg(test)]

//! Integration coverage for the cursor-paginated query variants added in #599.
//!
//! The unbounded query entrypoints (`list_credit_limits`,
//! `get_token_reconciliation`, `generate_reconciliation_proof`,
//! `get_contract_reconciliation_summary`) each scan an unbounded index or the
//! full `0..NextId` subscription ID space in a single call, which exhausts the
//! per-transaction instruction budget on large contracts. The `*_paginated`
//! variants bound the work per call and return a continuation cursor.
//!
//! These tests exercise:
//! * `list_credit_limits_paginated` — offset cursor, expired-grant skipping.
//! * `get_token_reconciliation_paginated` / `generate_reconciliation_proof_paginated`
//!   — bounded subscription scan, partial-total accumulation.
//! * `get_contract_reconciliation_summary_paginated` — two-level iteration.
//! * Cursor termination and limit validation.

use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token::StellarAssetClient as TokenAdminClient,
    Address, Env, Symbol, Vec,
};
use subscription_vault::{
    Error, SubscriptionVault, SubscriptionVaultClient, MAX_CREDIT_LIMITS_PAGE,
    MAX_PREPAID_SCAN_DEPTH,
};

const MIN_TOPUP: i128 = 1_000_000;
const AMOUNT: i128 = 5_000_000;
const INTERVAL: u64 = 30 * 24 * 60 * 60;
const NOW: u64 = 1_000_000;

struct Fixture {
    env: Env,
    client: SubscriptionVaultClient<'static>,
    token: Address,
    admin: Address,
    merchant: Address,
}

impl Fixture {
    fn new() -> Self {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(NOW);

        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();

        let admin = Address::generate(&env);
        let merchant = Address::generate(&env);

        let contract_id = env.register(SubscriptionVault, ());
        let client = SubscriptionVaultClient::new(&env, &contract_id);
        client.init(&token, &7u32, &admin, &MIN_TOPUP, &(7 * 24 * 60 * 60));

        Self {
            env,
            client,
            token,
            admin,
            merchant,
        }
    }

    fn funded_subscription(&self, subscriber: &Address, deposit: i128) -> u32 {
        let token_admin_client = TokenAdminClient::new(&self.env, &self.token);
        token_admin_client.mint(subscriber, &deposit);

        let sub_id = self.client.create_subscription(
            subscriber,
            &self.merchant,
            &AMOUNT,
            &INTERVAL,
            &false,
            &None::<i128>,
            &None::<u64>,
            &None::<u32>,
            &None::<Symbol>,
            &false,
        );
        self.client.deposit_funds(&sub_id, &deposit, &None);
        sub_id
    }

    fn grant(&self, subscriber: &Address, limit: i128, expires_at: Option<u64>) {
        self.client.set_subscriber_credit_limit(
            &self.admin,
            &self.merchant,
            subscriber,
            &self.token,
            &limit,
            &expires_at,
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// list_credit_limits_paginated
// ─────────────────────────────────────────────────────────────────────────────

/// Walk every page and collect the subscribers of the returned grants.
fn collect_credit_limit_subscribers(f: &Fixture, limit: u32) -> Vec<Address> {
    let mut out = Vec::new(&f.env);
    let mut cursor = None;
    let mut pages = 0u32;
    loop {
        let page = f
            .client
            .list_credit_limits_paginated(&f.merchant, &cursor, &limit);
        for entry in page.entries.iter() {
            out.push_back(entry.subscriber);
        }
        pages += 1;
        assert!(pages < 100, "credit-limit pagination failed to terminate");
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    out
}

#[test]
fn credit_limits_pagination_skips_expired_and_advances_cursor() {
    let f = Fixture::new();

    let s0 = Address::generate(&f.env);
    let s1 = Address::generate(&f.env);
    let s2 = Address::generate(&f.env);
    let s3 = Address::generate(&f.env);
    let s4 = Address::generate(&f.env);

    // Insertion order into the index: s0, s1, s2, s3, s4.
    f.grant(&s0, 1_000, None);
    f.grant(&s1, 1_000, None);
    // s2 is expired: retained in the index but must be skipped.
    f.grant(&s2, 1_000, Some(NOW - 1));
    f.grant(&s3, 1_000, None);
    f.grant(&s4, 1_000, None);

    // Page 1: index positions [0, 2) -> s0, s1.
    let page1 = f.client.list_credit_limits_paginated(&f.merchant, &None, &2);
    assert_eq!(page1.total, 5);
    assert_eq!(page1.entries.len(), 2);
    assert_eq!(page1.entries.get(0).unwrap().subscriber, s0);
    assert_eq!(page1.entries.get(1).unwrap().subscriber, s1);
    assert_eq!(page1.next_cursor, Some(2));

    // Page 2: index positions [2, 4) -> s2 (expired, skipped), s3.
    let page2 = f
        .client
        .list_credit_limits_paginated(&f.merchant, &page1.next_cursor, &2);
    assert_eq!(page2.entries.len(), 1, "expired grant must be skipped");
    assert_eq!(page2.entries.get(0).unwrap().subscriber, s3);
    assert_eq!(page2.next_cursor, Some(4));

    // Page 3: index positions [4, 5) -> s4, terminal page.
    let page3 = f
        .client
        .list_credit_limits_paginated(&f.merchant, &page2.next_cursor, &2);
    assert_eq!(page3.entries.len(), 1);
    assert_eq!(page3.entries.get(0).unwrap().subscriber, s4);
    assert_eq!(page3.next_cursor, None);

    // Full traversal yields exactly the four active grants, in order.
    let all = collect_credit_limit_subscribers(&f, 2);
    let mut expected = Vec::new(&f.env);
    expected.push_back(s0.clone());
    expected.push_back(s1.clone());
    expected.push_back(s3.clone());
    expected.push_back(s4.clone());
    assert_eq!(all, expected);

    // The unbounded form still returns the same active grants.
    let unbounded = f.client.list_credit_limits(&f.merchant);
    assert_eq!(unbounded.len(), 4);
}

#[test]
fn credit_limits_cursor_past_end_is_empty_and_terminal() {
    let f = Fixture::new();
    let subscriber = Address::generate(&f.env);
    f.grant(&subscriber, 1_000, None);

    let page = f
        .client
        .list_credit_limits_paginated(&f.merchant, &Some(9_999), &10);
    assert_eq!(page.entries.len(), 0);
    assert_eq!(page.next_cursor, None);
}

#[test]
fn credit_limits_rejects_out_of_range_limits() {
    let f = Fixture::new();

    assert_eq!(
        f.client
            .try_list_credit_limits_paginated(&f.merchant, &None, &0),
        Err(Ok(Error::InvalidInput))
    );
    assert_eq!(
        f.client.try_list_credit_limits_paginated(
            &f.merchant,
            &None,
            &(MAX_CREDIT_LIMITS_PAGE + 1)
        ),
        Err(Ok(Error::InvalidInput))
    );
    assert!(f
        .client
        .try_list_credit_limits_paginated(&f.merchant, &None, &MAX_CREDIT_LIMITS_PAGE)
        .is_ok());
}

// ─────────────────────────────────────────────────────────────────────────────
// get_token_reconciliation_paginated / generate_reconciliation_proof_paginated
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn token_reconciliation_pages_accumulate_to_full_prepaid_total() {
    let f = Fixture::new();

    let mut expected_total: i128 = 0;
    for _ in 0..3 {
        let subscriber = Address::generate(&f.env);
        f.funded_subscription(&subscriber, 2_000_000);
        expected_total += 2_000_000;
    }

    // scan_limit = 2 forces the 3-id space into two pages.
    let page1 = f
        .client
        .get_token_reconciliation_paginated(&f.token, &None, &2);
    assert_eq!(page1.token, f.token);
    assert_eq!(page1.partial_prepaid, 4_000_000);
    assert_eq!(page1.subscriptions_count, 2);
    assert_eq!(page1.next_cursor, Some(2));
    assert!(page1.has_more);
    assert_eq!(page1.contract_balance, expected_total);

    let page2 = f
        .client
        .get_token_reconciliation_paginated(&f.token, &page1.next_cursor, &2);
    assert_eq!(page2.partial_prepaid, 2_000_000);
    assert_eq!(page2.subscriptions_count, 1);
    assert_eq!(page2.next_cursor, None);
    assert!(!page2.has_more);

    assert_eq!(
        page1.partial_prepaid + page2.partial_prepaid,
        expected_total,
        "accumulated partials must equal the on-chain prepaid total"
    );
}

#[test]
fn token_reconciliation_scan_limit_zero_clamps_to_one() {
    let f = Fixture::new();
    let subscriber = Address::generate(&f.env);
    f.funded_subscription(&subscriber, 2_000_000);

    // scan_limit = 0 must not produce a zero-width window (which would never
    // advance); it clamps to 1, so a single-id space completes in one page.
    let page = f
        .client
        .get_token_reconciliation_paginated(&f.token, &None, &0);
    assert_eq!(page.partial_prepaid, 2_000_000);
    assert_eq!(page.next_cursor, None);
    assert!(!page.has_more);
}

#[test]
fn token_reconciliation_rejects_unaccepted_token() {
    let f = Fixture::new();
    let bogus = Address::generate(&f.env);

    assert_eq!(
        f.client
            .try_get_token_reconciliation_paginated(&bogus, &None, &10),
        Err(Ok(Error::InvalidToken))
    );
}

#[test]
fn reconciliation_proof_pages_accumulate_and_carry_ledger_anchor() {
    let f = Fixture::new();

    let mut expected_total: i128 = 0;
    for _ in 0..3 {
        let subscriber = Address::generate(&f.env);
        f.funded_subscription(&subscriber, 2_000_000);
        expected_total += 2_000_000;
    }

    let page1 = f
        .client
        .generate_reconciliation_proof_paginated(&f.token, &None, &2);
    assert_eq!(page1.token, f.token);
    assert_eq!(page1.timestamp, NOW);
    assert_eq!(page1.partial_prepaid, 4_000_000);
    assert_eq!(page1.subscription_count, 2);
    assert_eq!(page1.next_cursor, Some(2));
    assert!(page1.has_more);
    assert_eq!(page1.contract_balance, expected_total);

    let page2 = f
        .client
        .generate_reconciliation_proof_paginated(&f.token, &page1.next_cursor, &2);
    assert_eq!(page2.partial_prepaid, 2_000_000);
    assert_eq!(page2.next_cursor, None);
    assert!(!page2.has_more);

    assert_eq!(
        page1.partial_prepaid + page2.partial_prepaid,
        expected_total
    );
}

#[test]
fn reconciliation_proof_rejects_unaccepted_token() {
    let f = Fixture::new();
    let bogus = Address::generate(&f.env);

    assert_eq!(
        f.client
            .try_generate_reconciliation_proof_paginated(&bogus, &None, &10),
        Err(Ok(Error::InvalidToken))
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// get_contract_reconciliation_summary_paginated
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_summary_paginated_reports_partial_prepaid_per_token() {
    let f = Fixture::new();

    let mut expected_total: i128 = 0;
    for _ in 0..3 {
        let subscriber = Address::generate(&f.env);
        f.funded_subscription(&subscriber, 2_000_000);
        expected_total += 2_000_000;
    }

    // Only the init token is accepted, so the token window holds one entry.
    let page = f
        .client
        .get_contract_reconciliation_summary_paginated(&0, &10, &None);

    assert_eq!(page.token_summaries.len(), 1);
    let summary = page.token_summaries.get(0).unwrap();
    assert_eq!(summary.token, f.token);
    assert_eq!(summary.partial_prepaid, expected_total);
    assert_eq!(summary.subscriptions_count, 3);

    // A single token fits the default window, so both cursors are exhausted.
    assert_eq!(page.next_token_index, None);
    assert_eq!(page.next_subscription_cursor, None);
}

#[test]
fn contract_summary_paginated_honours_subscription_cursor() {
    let f = Fixture::new();

    for _ in 0..3 {
        let subscriber = Address::generate(&f.env);
        f.funded_subscription(&subscriber, 2_000_000);
    }

    // Resume the shared subscription scan at id 1: only ids 1 and 2 remain.
    let page = f
        .client
        .get_contract_reconciliation_summary_paginated(&0, &10, &Some(1));

    let summary = page.token_summaries.get(0).unwrap();
    assert_eq!(summary.partial_prepaid, 4_000_000);
    assert_eq!(summary.subscriptions_count, 2);
    assert_eq!(page.next_subscription_cursor, None);
}

#[test]
fn contract_summary_paginated_token_cursor_past_end_is_empty() {
    let f = Fixture::new();
    let subscriber = Address::generate(&f.env);
    f.funded_subscription(&subscriber, 2_000_000);

    let page = f
        .client
        .get_contract_reconciliation_summary_paginated(&5, &10, &None);
    assert_eq!(page.token_summaries.len(), 0);
    assert_eq!(page.next_token_index, None);
    assert_eq!(page.next_subscription_cursor, None);
}

#[test]
fn contract_summary_paginated_rejects_zero_limit() {
    let f = Fixture::new();

    assert_eq!(
        f.client
            .try_get_contract_reconciliation_summary_paginated(&0, &0, &None),
        Err(Ok(Error::InvalidInput))
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Budget boundary
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn reconciliation_scan_is_bounded_by_scan_limit() {
    let f = Fixture::new();

    // More subscriptions than the scan window, so the call must stop at the
    // window boundary rather than scanning every id.
    for _ in 0..7 {
        let subscriber = Address::generate(&f.env);
        f.funded_subscription(&subscriber, 1_000_000);
    }

    let scan_window: u32 = 3;
    let page = f
        .client
        .get_token_reconciliation_paginated(&f.token, &None, &scan_window);

    assert!(
        page.subscriptions_count <= scan_window,
        "a bounded page must not scan more than scan_limit ids"
    );
    assert_eq!(page.next_cursor, Some(scan_window));
    assert!(page.has_more);
    assert!(
        scan_window < MAX_PREPAID_SCAN_DEPTH,
        "test window must be below the hard cap"
    );
}
