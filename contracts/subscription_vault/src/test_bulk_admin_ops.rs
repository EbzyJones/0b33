//! Tests for the bulk admin/operator pause & cancel tooling (issue #497).
//!
//! Coverage targets the operational-hygiene requirements:
//! - admin **and** operator are both accepted authorizers; anyone else is rejected;
//! - the batch is partial-failure tolerant (already-paused/cancelled/missing ids
//!   are skipped or reported, never aborting the batch);
//! - empty list is a no-op that consumes no nonce and emits no envelope event;
//! - duplicate ids inside one batch are safe (no double pause / double refund);
//! - batches larger than `BATCH_MAX_SIZE` are rejected wholesale;
//! - the per-batch nonce advances and rejects wrong/replayed values.

use crate::nonce::DOMAIN_OPERATOR_BATCH_CHARGE;
use crate::test_utils::setup::TestEnv;
use crate::types::{BulkSubscriptionResult, Error, SubscriptionStatus, BATCH_MAX_SIZE};
use soroban_sdk::{testutils::Address as _, testutils::Events as _, vec, Address, Vec};

const AMOUNT: i128 = 1_000;
const INTERVAL: u64 = 24 * 60 * 60;
const DEPOSIT: i128 = 5_000_000; // >= init min_topup (1_000_000)

/// Create an `Active`, fully-funded subscription and return its id.
fn funded_sub(te: &TestEnv, subscriber: &Address, merchant: &Address) -> u32 {
    let sub_id = te.client.create_subscription(
        subscriber, merchant, &AMOUNT, &INTERVAL, &false, &None, &None,
        &None::<u32>,
);
    te.stellar_token_client().mint(subscriber, &DEPOSIT);
    te.client.deposit_funds(&sub_id, &DEPOSIT, &None);
    sub_id
}

fn token_balance(te: &TestEnv, who: &Address) -> i128 {
    soroban_sdk::token::Client::new(&te.env, &te.token).balance(who)
}

fn status(te: &TestEnv, sub_id: u32) -> SubscriptionStatus {
    te.client.get_subscription(&sub_id).status
}

// ── Bulk pause: happy paths ─────────────────────────────────────────────────

#[test]
fn admin_bulk_pause_pauses_all_active_subscriptions() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);

    let a = funded_sub(&te, &subscriber, &merchant);
    let b = funded_sub(&te, &subscriber, &merchant);

    let results = te
        .client
        .bulk_pause_subscriptions(&te.admin, &vec![&te.env, a, b], &0u64);

    assert_eq!(results.len(), 2);
    assert!(results.get(0).unwrap().success && results.get(0).unwrap().changed);
    assert!(results.get(1).unwrap().success && results.get(1).unwrap().changed);
    assert_eq!(status(&te, a), SubscriptionStatus::Paused);
    assert_eq!(status(&te, b), SubscriptionStatus::Paused);
}

#[test]
fn operator_is_authorized_for_bulk_pause() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);
    let operator = Address::generate(&te.env);
    te.client.set_operator(&te.admin, &operator);

    let a = funded_sub(&te, &subscriber, &merchant);

    let results = te
        .client
        .bulk_pause_subscriptions(&operator, &vec![&te.env, a], &0u64);

    assert!(results.get(0).unwrap().changed);
    assert_eq!(status(&te, a), SubscriptionStatus::Paused);
}

#[test]
fn unauthorized_caller_rejected_for_bulk_pause() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);
    let stranger = Address::generate(&te.env);

    let a = funded_sub(&te, &subscriber, &merchant);

    let res = te
        .client
        .try_bulk_pause_subscriptions(&stranger, &vec![&te.env, a], &0u64);
    assert_eq!(res, Err(Ok(Error::Unauthorized)));
    // State untouched.
    assert_eq!(status(&te, a), SubscriptionStatus::Active);
}

// ── Bulk pause: edge cases ──────────────────────────────────────────────────

#[test]
fn bulk_pause_empty_vec_emits_no_events_and_no_storage_writes() {
    let te = TestEnv::default();
    let empty: Vec<u32> = Vec::new(&te.env);

    let before_events = te.env.events().all().len();
    let results = te.client.bulk_pause_subscriptions(&te.admin, &empty, &0u64);
    let after_events = te.env.events().all().len();

    assert_eq!(results.len(), 0);
    assert_eq!(
        before_events, after_events,
        "empty bulk_pause must not emit any events"
    );
    assert_eq!(
        te.client
            .get_admin_nonce(&te.admin, &DOMAIN_OPERATOR_BATCH_CHARGE.as_u32()),
        0u64,
        "empty bulk_pause must not consume nonce"
    );
}

#[test]
fn bulk_pause_id_zero_reports_not_found() {
    let te = TestEnv::default();

    // No subscriptions exist, so id 0 is not found.
    let results = te
        .client
        .bulk_pause_subscriptions(&te.admin, &vec![&te.env, 0u32], &0u64);
    assert_eq!(results.len(), 1);
    assert_eq!(
        results.get(0).unwrap(),
        BulkSubscriptionResult {
            subscription_id: 0,
            success: false,
            changed: false,
            error_code: Error::NotFound.to_code(),
        }
    );

    // Nonce was consumed (batch was non-empty).
    assert_eq!(
        te.client
            .get_admin_nonce(&te.admin, &DOMAIN_OPERATOR_BATCH_CHARGE.as_u32()),
        1u64
    );
}

#[test]
fn empty_bulk_pause_is_a_noop_and_consumes_no_nonce() {
    let te = TestEnv::default();

    let before_events = te.env.events().all().len();
    let empty: Vec<u32> = Vec::new(&te.env);
    let results = te.client.bulk_pause_subscriptions(&te.admin, &empty, &0u64);
    assert_eq!(results.len(), 0);

    // No events were emitted — empty batch is a true no-op.
    assert_eq!(
        te.env.events().all().len(),
        before_events,
        "empty bulk_pause must not emit any events"
    );

    // Nonce was NOT consumed — a real batch can still use nonce 0.
    assert_eq!(
        te.client
            .get_admin_nonce(&te.admin, &DOMAIN_OPERATOR_BATCH_CHARGE.as_u32()),
        0u64
    );
}

#[test]
fn bulk_pause_skips_already_paused_without_aborting() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);

    let active = funded_sub(&te, &subscriber, &merchant);
    let already = funded_sub(&te, &subscriber, &merchant);
    // Pre-pause `already` via the merchant (single-id path).
    te.client.pause_subscription(&already, &merchant);

    let missing = 9_999u32; // never created

    let results = te.client.bulk_pause_subscriptions(
        &te.admin,
        &vec![&te.env, active, already, missing],
        &0u64,
    );

    // active -> changed
    assert_eq!(
        results.get(0).unwrap(),
        BulkSubscriptionResult {
            subscription_id: active,
            success: true,
            changed: true,
            error_code: 0,
        }
    );
    // already paused -> skipped (success, not changed)
    assert_eq!(
        results.get(1).unwrap(),
        BulkSubscriptionResult {
            subscription_id: already,
            success: true,
            changed: false,
            error_code: 0,
        }
    );
    // missing -> failed with NotFound
    assert_eq!(
        results.get(2).unwrap(),
        BulkSubscriptionResult {
            subscription_id: missing,
            success: false,
            changed: false,
            error_code: Error::NotFound.to_code(),
        }
    );

    assert_eq!(status(&te, active), SubscriptionStatus::Paused);
}

#[test]
fn bulk_pause_with_duplicate_ids_pauses_once_then_skips() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);
    let a = funded_sub(&te, &subscriber, &merchant);

    let results = te
        .client
        .bulk_pause_subscriptions(&te.admin, &vec![&te.env, a, a, a], &0u64);

    assert!(results.get(0).unwrap().changed);
    assert!(!results.get(1).unwrap().changed && results.get(1).unwrap().success);
    assert!(!results.get(2).unwrap().changed && results.get(2).unwrap().success);
    assert_eq!(status(&te, a), SubscriptionStatus::Paused);
}

#[test]
fn bulk_pause_reports_expired_as_failure() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);

    let now = te.env.ledger().timestamp();
    let sub_id = te.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None,
        &Some(now + 1_000),
        &None::<u32>,
);
    te.stellar_token_client().mint(&subscriber, &DEPOSIT);
    te.client.deposit_funds(&sub_id, &DEPOSIT, &None);

    te.jump(2_000); // past expires_at

    let results = te
        .client
        .bulk_pause_subscriptions(&te.admin, &vec![&te.env, sub_id], &0u64);
    assert_eq!(
        results.get(0).unwrap().error_code,
        Error::SubscriptionExpired.to_code()
    );
    assert!(!results.get(0).unwrap().success);
}

#[test]
fn bulk_pause_rejects_oversized_batch() {
    let te = TestEnv::default();

    let mut ids: Vec<u32> = Vec::new(&te.env);
    for i in 0..(BATCH_MAX_SIZE + 1) {
        ids.push_back(i);
    }

    let res = te
        .client
        .try_bulk_pause_subscriptions(&te.admin, &ids, &0u64);
    assert_eq!(res, Err(Ok(Error::BatchTooLarge)));
    // Oversized batch must not burn the nonce.
    assert_eq!(
        te.client
            .get_admin_nonce(&te.admin, &DOMAIN_OPERATOR_BATCH_CHARGE.as_u32()),
        0u64
    );
}

#[test]
fn bulk_pause_at_max_batch_size_is_accepted() {
    let te = TestEnv::default();

    let mut ids: Vec<u32> = Vec::new(&te.env);
    for i in 0..BATCH_MAX_SIZE {
        ids.push_back(i); // ids don't need to exist; they report NotFound
    }

    // Exactly BATCH_MAX_SIZE is allowed (does not return BatchTooLarge).
    let results = te.client.bulk_pause_subscriptions(&te.admin, &ids, &0u64);
    assert_eq!(results.len(), BATCH_MAX_SIZE);
}

// ── Bulk pause: nonce semantics ─────────────────────────────────────────────

#[test]
fn bulk_pause_advances_nonce() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);
    let a = funded_sub(&te, &subscriber, &merchant);

    te.client
        .bulk_pause_subscriptions(&te.admin, &vec![&te.env, a], &0u64);
    assert_eq!(
        te.client
            .get_admin_nonce(&te.admin, &DOMAIN_OPERATOR_BATCH_CHARGE.as_u32()),
        1u64
    );
}

#[test]
fn bulk_pause_wrong_nonce_rejected() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);
    let a = funded_sub(&te, &subscriber, &merchant);

    let res = te
        .client
        .try_bulk_pause_subscriptions(&te.admin, &vec![&te.env, a], &1u64);
    assert_eq!(res, Err(Ok(Error::NonceAlreadyUsed)));
}

#[test]
fn bulk_pause_replay_rejected() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);
    let a = funded_sub(&te, &subscriber, &merchant);
    let b = funded_sub(&te, &subscriber, &merchant);

    te.client
        .bulk_pause_subscriptions(&te.admin, &vec![&te.env, a], &0u64);
    // Replaying nonce 0 must fail.
    let res = te
        .client
        .try_bulk_pause_subscriptions(&te.admin, &vec![&te.env, b], &0u64);
    assert_eq!(res, Err(Ok(Error::NonceAlreadyUsed)));
}

#[test]
fn admin_and_operator_have_independent_nonce_sequences() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);
    let operator = Address::generate(&te.env);
    te.client.set_operator(&te.admin, &operator);

    let a = funded_sub(&te, &subscriber, &merchant);
    let b = funded_sub(&te, &subscriber, &merchant);

    // Both start at nonce 0 independently (keyed per signer address).
    te.client
        .bulk_pause_subscriptions(&te.admin, &vec![&te.env, a], &0u64);
    te.client
        .bulk_pause_subscriptions(&operator, &vec![&te.env, b], &0u64);

    assert_eq!(
        te.client
            .get_admin_nonce(&te.admin, &DOMAIN_OPERATOR_BATCH_CHARGE.as_u32()),
        1u64
    );
    assert_eq!(te.client.get_operator_nonce(&operator), 1u64);
}

// ── Bulk cancel: happy paths & refunds ──────────────────────────────────────

#[test]
fn admin_bulk_cancel_cancels_and_holds_escrow() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);

    let a = funded_sub(&te, &subscriber, &merchant);
    let b = funded_sub(&te, &subscriber, &merchant);
    // Both deposits are in the contract; subscriber holds nothing right now.
    assert_eq!(token_balance(&te, &subscriber), 0);

    let results = te
        .client
        .bulk_cancel_subscriptions(&te.admin, &vec![&te.env, a, b], &0u64);

    assert!(results.get(0).unwrap().changed && results.get(1).unwrap().changed);
    assert_eq!(status(&te, a), SubscriptionStatus::Cancelled);
    assert_eq!(status(&te, b), SubscriptionStatus::Cancelled);
    // Cancellation holds each prepaid balance in a time-locked escrow (#604)
    // instead of refunding immediately.
    assert_eq!(token_balance(&te, &subscriber), 0);
    assert_eq!(te.client.get_cancellation_escrow(&a).amount, DEPOSIT);
    assert_eq!(te.client.get_cancellation_escrow(&b).amount, DEPOSIT);
}

#[test]
fn operator_is_authorized_for_bulk_cancel() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);
    let operator = Address::generate(&te.env);
    te.client.set_operator(&te.admin, &operator);

    let a = funded_sub(&te, &subscriber, &merchant);
    te.client
        .bulk_cancel_subscriptions(&operator, &vec![&te.env, a], &0u64);
    assert_eq!(status(&te, a), SubscriptionStatus::Cancelled);
}

#[test]
fn unauthorized_caller_rejected_for_bulk_cancel() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);
    let stranger = Address::generate(&te.env);
    let a = funded_sub(&te, &subscriber, &merchant);

    let res = te
        .client
        .try_bulk_cancel_subscriptions(&stranger, &vec![&te.env, a], &0u64);
    assert_eq!(res, Err(Ok(Error::Unauthorized)));
    assert_eq!(status(&te, a), SubscriptionStatus::Active);
}

// ── Bulk cancel: edge cases ─────────────────────────────────────────────────

#[test]
fn bulk_cancel_empty_vec_emits_no_events_and_no_storage_writes() {
    let te = TestEnv::default();
    let empty: Vec<u32> = Vec::new(&te.env);

    let before_events = te.env.events().all().len();
    let results = te
        .client
        .bulk_cancel_subscriptions(&te.admin, &empty, &0u64);
    let after_events = te.env.events().all().len();

    assert_eq!(results.len(), 0);
    assert_eq!(
        before_events, after_events,
        "empty bulk_cancel must not emit any events"
    );
    assert_eq!(
        te.client
            .get_admin_nonce(&te.admin, &DOMAIN_OPERATOR_BATCH_CHARGE.as_u32()),
        0u64,
        "empty bulk_cancel must not consume nonce"
    );
}

#[test]
fn bulk_cancel_id_zero_reports_not_found() {
    let te = TestEnv::default();

    let results = te
        .client
        .bulk_cancel_subscriptions(&te.admin, &vec![&te.env, 0u32], &0u64);
    assert_eq!(results.len(), 1);
    assert_eq!(
        results.get(0).unwrap(),
        BulkSubscriptionResult {
            subscription_id: 0,
            success: false,
            changed: false,
            error_code: Error::NotFound.to_code(),
        }
    );

    assert_eq!(
        te.client
            .get_admin_nonce(&te.admin, &DOMAIN_OPERATOR_BATCH_CHARGE.as_u32()),
        1u64
    );
}

#[test]
fn bulk_cancel_skips_already_cancelled_no_double_escrow() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);

    // `funded_sub` is net-zero on the subscriber's wallet (mint DEPOSIT, then
    // deposit DEPOSIT into the contract).
    let already = funded_sub(&te, &subscriber, &merchant);
    // Cancel once via the merchant single-id path — escrows DEPOSIT already.
    te.client.cancel_subscription(&already, &merchant);
    assert_eq!(token_balance(&te, &subscriber), 0);

    // Creating a second funded sub is net-zero, so the escrow above is untouched.
    let active = funded_sub(&te, &subscriber, &merchant);
    assert_eq!(token_balance(&te, &subscriber), 0);

    let results =
        te.client
            .bulk_cancel_subscriptions(&te.admin, &vec![&te.env, already, active], &0u64);

    // already-cancelled -> skipped, no second escrow
    assert_eq!(
        results.get(0).unwrap(),
        BulkSubscriptionResult {
            subscription_id: already,
            success: true,
            changed: false,
            error_code: 0,
        }
    );
    assert!(results.get(1).unwrap().changed);
    // Funds are held in escrow, not refunded to the wallet.
    assert_eq!(token_balance(&te, &subscriber), 0);
    // `already` was escrowed exactly once; `active` is escrowed here.
    assert_eq!(te.client.get_cancellation_escrow(&already).amount, DEPOSIT);
    assert_eq!(te.client.get_cancellation_escrow(&active).amount, DEPOSIT);
}

#[test]
fn bulk_cancel_duplicate_ids_escrows_once() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);
    let a = funded_sub(&te, &subscriber, &merchant);
    assert_eq!(token_balance(&te, &subscriber), 0);

    let results = te
        .client
        .bulk_cancel_subscriptions(&te.admin, &vec![&te.env, a, a], &0u64);

    assert!(results.get(0).unwrap().changed);
    assert!(!results.get(1).unwrap().changed && results.get(1).unwrap().success);
    // Escrowed exactly once (not doubled) and nothing refunded to the wallet.
    assert_eq!(token_balance(&te, &subscriber), 0);
    assert_eq!(te.client.get_cancellation_escrow(&a).amount, DEPOSIT);
}

#[test]
fn bulk_cancel_mixed_valid_cancelled_missing() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);

    let valid = funded_sub(&te, &subscriber, &merchant);
    let cancelled = funded_sub(&te, &subscriber, &merchant);
    te.client.cancel_subscription(&cancelled, &merchant);
    let missing = 4_242u32;

    let results = te.client.bulk_cancel_subscriptions(
        &te.admin,
        &vec![&te.env, valid, cancelled, missing],
        &0u64,
    );

    assert!(results.get(0).unwrap().changed);
    assert!(!results.get(1).unwrap().changed && results.get(1).unwrap().success);
    assert_eq!(
        results.get(2).unwrap().error_code,
        Error::NotFound.to_code()
    );
    assert!(!results.get(2).unwrap().success);
}

#[test]
fn empty_bulk_cancel_is_a_noop_and_consumes_no_nonce() {
    let te = TestEnv::default();
    let before_events = te.env.events().all().len();
    let empty: Vec<u32> = Vec::new(&te.env);
    let results = te
        .client
        .bulk_cancel_subscriptions(&te.admin, &empty, &0u64);
    assert_eq!(results.len(), 0);

    // No events were emitted — empty batch is a true no-op.
    assert_eq!(
        te.env.events().all().len(),
        before_events,
        "empty bulk_cancel must not emit any events"
    );

    assert_eq!(
        te.client
            .get_admin_nonce(&te.admin, &DOMAIN_OPERATOR_BATCH_CHARGE.as_u32()),
        0u64
    );
}

#[test]
fn bulk_cancel_rejects_oversized_batch() {
    let te = TestEnv::default();
    let mut ids: Vec<u32> = Vec::new(&te.env);
    for i in 0..(BATCH_MAX_SIZE + 1) {
        ids.push_back(i);
    }
    let res = te
        .client
        .try_bulk_cancel_subscriptions(&te.admin, &ids, &0u64);
    assert_eq!(res, Err(Ok(Error::BatchTooLarge)));
}

#[test]
fn bulk_cancel_replay_rejected() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);
    let a = funded_sub(&te, &subscriber, &merchant);
    let b = funded_sub(&te, &subscriber, &merchant);

    te.client
        .bulk_cancel_subscriptions(&te.admin, &vec![&te.env, a], &0u64);
    let res = te
        .client
        .try_bulk_cancel_subscriptions(&te.admin, &vec![&te.env, b], &0u64);
    assert_eq!(res, Err(Ok(Error::NonceAlreadyUsed)));
}

// ── Bulk operations: atomicity and partial failure semantics ────────────────

/// Verify that bulk_pause processes all IDs even when some fail, and returns
/// a result for each ID (partial-failure tolerance, not atomicity).
///
/// Current implementation: bulk operations are **best-effort**, not atomic.
/// A failure mid-batch does not roll back earlier changes. This test documents
/// that behavior and ensures callers can inspect per-ID results.
#[test]
fn bulk_pause_partial_failures_documented() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);

    // Create mix of: active, paused, expired, and missing subscriptions
    let active = funded_sub(&te, &subscriber, &merchant);
    let paused = funded_sub(&te, &subscriber, &merchant);
    te.client.pause_subscription(&paused, &merchant);

    // Create an expired subscription
    let now = te.env.ledger().timestamp();
    let expired_id = te.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None,
        &Some(now + 10), // expires soon
        &None::<u32>,
    );
    te.stellar_token_client().mint(&subscriber, &DEPOSIT);
    te.client.deposit_funds(&expired_id, &DEPOSIT, &None);
    te.jump(100); // past expiration

    let missing = 9_999u32;

    // Bulk pause all four: [active (succeeds), paused (skipped), expired (fails), missing (fails)]
    let results = te.client.bulk_pause_subscriptions(
        &te.admin,
        &vec![&te.env, active, paused, expired_id, missing],
        &0u64,
    );

    assert_eq!(results.len(), 4, "must return result for every ID");

    // Active: changed
    assert!(results.get(0).unwrap().success && results.get(0).unwrap().changed);
    assert_eq!(status(&te, active), SubscriptionStatus::Paused);

    // Already paused: success but not changed
    assert!(results.get(1).unwrap().success && !results.get(1).unwrap().changed);
    assert_eq!(status(&te, paused), SubscriptionStatus::Paused);

    // Expired: failure
    assert!(!results.get(2).unwrap().success);
    assert_eq!(
        results.get(2).unwrap().error_code,
        Error::SubscriptionExpired.to_code()
    );

    // Missing: failure
    assert!(!results.get(3).unwrap().success);
    assert_eq!(
        results.get(3).unwrap().error_code,
        Error::NotFound.to_code()
    );

    // The first subscription was paused despite later failures — this is
    // the documented partial-completion semantic.
}

/// Verify that bulk_cancel processes all IDs with partial failures and does
/// not roll back earlier successful cancellations.
#[test]
fn bulk_cancel_partial_failures_escrows_successful_only() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);

    let sub_a = funded_sub(&te, &subscriber, &merchant);
    let sub_b = funded_sub(&te, &subscriber, &merchant);
    let missing = 7_777u32;

    // Subscriber has deposited 2 * DEPOSIT, but all funds are in the contract
    assert_eq!(token_balance(&te, &subscriber), 0);

    // Bulk cancel: [sub_a (succeeds), missing (fails), sub_b (succeeds)]
    let results = te.client.bulk_cancel_subscriptions(
        &te.admin,
        &vec![&te.env, sub_a, missing, sub_b],
        &0u64,
    );

    assert_eq!(results.len(), 3);

    // sub_a: cancelled and escrowed
    assert!(results.get(0).unwrap().success && results.get(0).unwrap().changed);
    assert_eq!(status(&te, sub_a), SubscriptionStatus::Cancelled);

    // missing: failed
    assert!(!results.get(1).unwrap().success);
    assert_eq!(
        results.get(1).unwrap().error_code,
        Error::NotFound.to_code()
    );

    // sub_b: cancelled and escrowed
    assert!(results.get(2).unwrap().success && results.get(2).unwrap().changed);
    assert_eq!(status(&te, sub_b), SubscriptionStatus::Cancelled);

    // Both successful cancellations were escrowed despite the missing ID
    // failure, and nothing was refunded straight to the wallet (#604).
    assert_eq!(
        token_balance(&te, &subscriber),
        0,
        "successful cancellations escrow funds even when other IDs fail"
    );
    assert_eq!(te.client.get_cancellation_escrow(&sub_a).amount, DEPOSIT);
    assert_eq!(te.client.get_cancellation_escrow(&sub_b).amount, DEPOSIT);
}

/// Verify that if a failure occurs in the middle of a batch, later IDs are
/// still processed (no early abort).
#[test]
fn bulk_operations_do_not_abort_on_first_failure() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);

    let first = funded_sub(&te, &subscriber, &merchant);
    let missing = 1_111u32; // will fail
    let last = funded_sub(&te, &subscriber, &merchant);

    // Bulk pause: [first (succeeds), missing (fails), last (should still be processed)]
    let results = te.client.bulk_pause_subscriptions(
        &te.admin,
        &vec![&te.env, first, missing, last],
        &0u64,
    );

    assert_eq!(results.len(), 3);

    // First: paused
    assert!(results.get(0).unwrap().success);
    assert_eq!(status(&te, first), SubscriptionStatus::Paused);

    // Missing: failed
    assert!(!results.get(1).unwrap().success);

    // Last: paused (confirms no early abort)
    assert!(
        results.get(2).unwrap().success,
        "bulk operation must continue after mid-batch failure"
    );
    assert_eq!(status(&te, last), SubscriptionStatus::Paused);
}

/// Document atomicity semantics: bulk operations are **not** atomic.
/// On partial failure, successful changes persist. This is intentional
/// for operational resilience.
#[test]
fn bulk_operations_are_not_atomic_successful_changes_persist() {
    let te = TestEnv::default();
    let subscriber = Address::generate(&te.env);
    let merchant = Address::generate(&te.env);

    let good = funded_sub(&te, &subscriber, &merchant);
    let bad = 5_555u32; // missing

    // Record initial state
    assert_eq!(status(&te, good), SubscriptionStatus::Active);

    // Bulk pause with one success and one failure
    let results = te
        .client
        .bulk_pause_subscriptions(&te.admin, &vec![&te.env, good, bad], &0u64);

    // good was paused, bad failed
    assert!(results.get(0).unwrap().success);
    assert!(!results.get(1).unwrap().success);

    // The successful pause persists — there is no rollback
    assert_eq!(
        status(&te, good),
        SubscriptionStatus::Paused,
        "successful operations in a bulk batch are not rolled back on partial failure"
    );

    // Document: if all-or-nothing behavior is required, the caller must verify
    // all IDs exist before calling bulk operations, or implement their own
    // rollback logic by inspecting the returned results.
}

// ══════════════════════════════════════════════════════════════════════════════
// Token-removal migration path — issue #583
//
// Tests the two acceptance criteria:
//   AC1 — `remove_accepted_token` emits a `TokenRemovedEvent`.
//   AC2 — `cancel_subscriptions_for_token` lets the admin drain subscriptions
//          denominated in a removed token, refunding each subscriber's balance.
// ══════════════════════════════════════════════════════════════════════════════

mod token_removal_migration {
    use crate::test_utils::setup::TestEnv;
    use crate::types::{Error, SubscriptionStatus, TokenRemovedEvent, BASE_EVENT_SCHEMA_VERSION};
    use soroban_sdk::{
        testutils::{Address as _, Events as _},
        token::StellarAssetClient as TokenAdminClient,
        Address, Env, Symbol,
    };

    const AMOUNT: i128 = 1_000;
    const INTERVAL: u64 = 24 * 60 * 60;
    const DEPOSIT: i128 = 5_000_000;

    /// Register a fresh Stellar asset contract as a second accepted token and
    /// return its address.
    fn register_second_token(te: &TestEnv) -> Address {
        let token2 = te
            .env
            .register_stellar_asset_contract_v2(te.admin.clone())
            .address();
        te.client
            .add_accepted_token(&te.admin, &token2, &6u32);
        token2
    }

    /// Create a `Paused` subscription denominated in `token` with a prepaid
    /// balance.  `Paused` is used because `remove_accepted_token` rejects
    /// removal while any `Active`/`GracePeriod` subs reference the token.
    fn paused_sub_for_token(te: &TestEnv, subscriber: &Address, token: &Address) -> u32 {
        let merchant = Address::generate(&te.env);
        let sub_id = te.client.create_subscription_with_token(
            subscriber, &merchant, token, &AMOUNT, &INTERVAL, &false, &None,
            &None, &None::<u32>, &None,
        );
        // Mint and deposit via the second token's admin client
        TokenAdminClient::new(&te.env, token).mint(subscriber, &DEPOSIT);
        te.client.deposit_funds(&sub_id, &DEPOSIT, &None);
        // Pause so the token can be removed
        te.client.pause_subscription(&sub_id, subscriber);
        sub_id
    }

    fn token_balance(te: &TestEnv, token: &Address, who: &Address) -> i128 {
        soroban_sdk::token::Client::new(&te.env, token)
            .balance(who)
    }

    // ── AC1: TokenRemovedEvent is emitted ────────────────────────────────────

    #[test]
    fn remove_accepted_token_emits_token_removed_event() {
        let te = TestEnv::default();
        let token2 = register_second_token(&te);
        let subscriber = Address::generate(&te.env);
        let _sub_id = paused_sub_for_token(&te, &subscriber, &token2);

        te.client.remove_accepted_token(&te.admin, &token2);

        let events = te.env.events().all();
        let topic = Symbol::new(&te.env, "token_removed");
        let evt = events.iter().rev().find(|e| {
            e.0.len() >= 1 && {
                let sym: Symbol = soroban_sdk::FromVal::from_val(&te.env, &e.0.get(0).unwrap());
                sym == topic
            }
        });
        assert!(evt.is_some(), "token_removed event must be emitted");

        let payload: TokenRemovedEvent =
            soroban_sdk::FromVal::from_val(&te.env, &evt.unwrap().1);
        assert_eq!(payload.token, token2);
        assert_eq!(payload.remaining_subscription_count, 1);
        assert_eq!(payload.admin, te.admin);
        assert_eq!(payload.schema_version, BASE_EVENT_SCHEMA_VERSION);
    }

    #[test]
    fn remove_accepted_token_with_no_subs_emits_zero_count() {
        let te = TestEnv::default();
        let token2 = register_second_token(&te);

        te.client.remove_accepted_token(&te.admin, &token2);

        let events = te.env.events().all();
        let topic = Symbol::new(&te.env, "token_removed");
        let evt = events.iter().rev().find(|e| {
            e.0.len() >= 1 && {
                let sym: Symbol = soroban_sdk::FromVal::from_val(&te.env, &e.0.get(0).unwrap());
                sym == topic
            }
        });
        let payload: TokenRemovedEvent =
            soroban_sdk::FromVal::from_val(&te.env, &evt.unwrap().1);
        assert_eq!(payload.remaining_subscription_count, 0);
    }

    // ── AC2: cancel_subscriptions_for_token migrates balances ────────────────

    #[test]
    fn cancel_subscriptions_for_token_escrows_prepaid_balances() {
        let te = TestEnv::default();
        let token2 = register_second_token(&te);
        let subscriber = Address::generate(&te.env);
        let sub_id = paused_sub_for_token(&te, &subscriber, &token2);

        // Balance is locked inside the vault — subscriber holds nothing.
        assert_eq!(token_balance(&te, &token2, &subscriber), 0);

        let results =
            te.client
                .cancel_subscriptions_for_token(&te.admin, &token2, &0u32, &10u32, &0u64);

        assert_eq!(results.len(), 1);
        let r = results.get(0).unwrap();
        assert!(r.success, "cancel must succeed");
        assert!(r.changed, "subscription must have been changed");

        assert_eq!(
            te.client.get_subscription(&sub_id).status,
            SubscriptionStatus::Cancelled
        );
        // The prepaid balance is held in a time-locked escrow (#604), not
        // refunded straight to the wallet.
        assert_eq!(token_balance(&te, &token2, &subscriber), 0);
        assert_eq!(te.client.get_cancellation_escrow(&sub_id).amount, DEPOSIT);
    }

    #[test]
    fn cancel_subscriptions_for_token_paginates_correctly() {
        let te = TestEnv::default();
        let token2 = register_second_token(&te);
        let sub_a = paused_sub_for_token(&te, &Address::generate(&te.env), &token2);
        let sub_b = paused_sub_for_token(&te, &Address::generate(&te.env), &token2);
        let sub_c = paused_sub_for_token(&te, &Address::generate(&te.env), &token2);

        // Page 1: cancel first two
        let r1 = te.client
            .cancel_subscriptions_for_token(&te.admin, &token2, &0u32, &2u32, &0u64);
        assert_eq!(r1.len(), 2);

        // Page 2: cancel the third (nonce=1 because page 1 consumed nonce=0)
        let r2 = te.client
            .cancel_subscriptions_for_token(&te.admin, &token2, &2u32, &2u32, &1u64);
        assert_eq!(r2.len(), 1);
        assert!(r2.get(0).unwrap().success);

        assert_eq!(
            te.client.get_subscription(&sub_a).status,
            SubscriptionStatus::Cancelled
        );
        assert_eq!(
            te.client.get_subscription(&sub_b).status,
            SubscriptionStatus::Cancelled
        );
        assert_eq!(
            te.client.get_subscription(&sub_c).status,
            SubscriptionStatus::Cancelled
        );
    }

    #[test]
    fn cancel_subscriptions_for_token_zero_limit_rejected() {
        let te = TestEnv::default();
        let token2 = register_second_token(&te);
        let result =
            te.client
                .try_cancel_subscriptions_for_token(&te.admin, &token2, &0u32, &0u32, &0u64);
        assert_eq!(result, Err(Ok(Error::BatchTooLarge)));
    }

    #[test]
    fn cancel_subscriptions_for_token_start_beyond_index_returns_empty() {
        let te = TestEnv::default();
        let token2 = register_second_token(&te);
        // No subscriptions; start=0, limit=10 should return empty successfully.
        let results =
            te.client
                .cancel_subscriptions_for_token(&te.admin, &token2, &0u32, &10u32, &0u64);
        assert_eq!(results.len(), 0);
    }

    // ── Live-subscription guard still blocks removal ──────────────────────────

    #[test]
    fn remove_accepted_token_blocked_while_active_subscription_exists() {
        let te = TestEnv::default();
        let token2 = register_second_token(&te);
        let subscriber = Address::generate(&te.env);
        let merchant = Address::generate(&te.env);

        // Create an Active subscription (not paused) for token2.
        let sub_id = te.client.create_subscription_with_token(
            &subscriber, &merchant, &token2, &AMOUNT, &INTERVAL, &false,
            &None, &None, &None::<u32>, &None,
        );
        TokenAdminClient::new(&te.env, &token2).mint(&subscriber, &DEPOSIT);
        te.client.deposit_funds(&sub_id, &DEPOSIT, &None);
        // sub is now Active with funds.

        let result = te.client.try_remove_accepted_token(&te.admin, &token2);
        assert_eq!(
            result,
            Err(Ok(Error::InvalidStatusTransition)),
            "must not remove token while Active subscription exists"
        );
    }
}

