use crate::{
    period_snapshots::{get_period_snapshot, list_period_snapshots, write_period_snapshot},
    types::{
        BillingPeriodSnapshot, Error, MAX_BILLING_PERIOD_SNAPSHOTS_PER_SUBSCRIPTION,
        SNAPSHOT_FLAG_CLOSED, SNAPSHOT_FLAG_EMPTY, SNAPSHOT_FLAG_INTERVAL_CHARGED,
        SNAPSHOT_FLAG_USAGE_CHARGED,
    },
    SubscriptionVault,
};
use soroban_sdk::{testutils::Ledger, Address, Env};

fn setup() -> (Env, soroban_sdk::Address) {
    let env = Env::default();
    env.ledger().set_timestamp(1_000_000);
    let contract_id = env.register(crate::SubscriptionVault, ());
    (env, contract_id)
}

#[test]
fn test_write_and_get_snapshot() {
    let (env, contract_id) = setup();
    let sub_id = 1;
    let period_index = 0;

        let snapshot = BillingPeriodSnapshot {
            subscription_id: sub_id,
            period_index,
            period_start: 100,
            period_end: 200,
            total_charged: 500,
            total_usage_units: 50,
            status_flags: SNAPSHOT_FLAG_USAGE_CHARGED,
            finalized_at: 200,
        };

    env.as_contract(&contract_id, || {
        assert!(write_period_snapshot(&env, snapshot.clone()).is_ok());
        let fetched = get_period_snapshot(&env, sub_id, period_index).unwrap();
        assert_eq!(fetched, snapshot);
    });
}

#[test]
fn test_list_period_snapshots_returns_latest_n() {
    let (env, contract_id) = setup();
    let sub_id = 2;

    env.as_contract(&contract_id, || {
        for i in 0..5 {
            let snapshot = BillingPeriodSnapshot {
                subscription_id: sub_id,
                period_index: i,
                period_start: 100 + i * 100,
                period_end: 200 + i * 100,
                total_charged: 500,
                total_usage_units: 0,
                status_flags: SNAPSHOT_FLAG_INTERVAL_CHARGED | SNAPSHOT_FLAG_CLOSED,
                finalized_at: 200 + i * 100,
            };
            assert!(write_period_snapshot(&env, snapshot).is_ok());
        }

        // Get latest 3
        let latest = list_period_snapshots(&env, sub_id, 3);
        assert_eq!(latest.len(), 3);
        
        // Should return newest first
        assert_eq!(latest.get(0).unwrap().period_index, 4);
        assert_eq!(latest.get(1).unwrap().period_index, 3);
        assert_eq!(latest.get(2).unwrap().period_index, 2);
    });
}

#[test]
fn test_snapshot_retention_prunes_oldest_periods() {
    let (env, contract_id) = setup();
    let sub_id = 7;

    env.as_contract(&contract_id, || {
        for period_index in 0..=MAX_BILLING_PERIOD_SNAPSHOTS_PER_SUBSCRIPTION {
            let snapshot = BillingPeriodSnapshot {
                subscription_id: sub_id,
                period_index: u64::from(period_index),
                period_start: u64::from(period_index) * 100,
                period_end: u64::from(period_index) * 100 + 50,
                total_charged: 500,
                total_usage_units: 0,
                status_flags: SNAPSHOT_FLAG_INTERVAL_CHARGED | SNAPSHOT_FLAG_CLOSED,
                finalized_at: u64::from(period_index) * 100 + 50,
            };
            assert!(write_period_snapshot(&env, snapshot).is_ok());
        }

        assert!(get_period_snapshot(&env, sub_id, 0).is_none());

        let index: soroban_sdk::Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::BillingPeriodSnapshotIndex(sub_id))
            .unwrap();
        assert_eq!(index.len(), MAX_BILLING_PERIOD_SNAPSHOTS_PER_SUBSCRIPTION);

        let retained = list_period_snapshots(
            &env,
            sub_id,
            MAX_BILLING_PERIOD_SNAPSHOTS_PER_SUBSCRIPTION,
        );
        assert_eq!(retained.len(), MAX_BILLING_PERIOD_SNAPSHOTS_PER_SUBSCRIPTION);
        assert_eq!(
            retained.get(0).unwrap().period_index,
            u64::from(MAX_BILLING_PERIOD_SNAPSHOTS_PER_SUBSCRIPTION)
        );
        assert_eq!(retained.get(retained.len() - 1).unwrap().period_index, 1);
    });
}

#[test]
fn test_overwrite_closed_snapshot_rejected() {
    let (env, contract_id) = setup();
    let sub_id = 3;
    let period_index = 0;

        let mut snapshot = BillingPeriodSnapshot {
            subscription_id: sub_id,
            period_index,
            period_start: 100,
            period_end: 200,
            total_charged: 500,
            total_usage_units: 0,
            status_flags: SNAPSHOT_FLAG_CLOSED | SNAPSHOT_FLAG_INTERVAL_CHARGED,
            finalized_at: 200,
        };

    env.as_contract(&contract_id, || {
        assert!(write_period_snapshot(&env, snapshot.clone()).is_ok());

        // Try to update closed snapshot
        snapshot.total_charged = 1000;
        let result = write_period_snapshot(&env, snapshot);
        assert_eq!(result, Err(Error::InvalidStatusTransition));
    });
}

#[test]
fn test_empty_period_sets_empty_flag() {
    let (env, contract_id) = setup();
    let sub_id = 4;
    let period_index = 0;

        let snapshot = BillingPeriodSnapshot {
            subscription_id: sub_id,
            period_index,
            period_start: 100,
            period_end: 200,
            total_charged: 0,
            total_usage_units: 0,
            status_flags: SNAPSHOT_FLAG_CLOSED,
            finalized_at: 200,
        };

    env.as_contract(&contract_id, || {
        assert!(write_period_snapshot(&env, snapshot).is_ok());

        let fetched = get_period_snapshot(&env, sub_id, period_index).unwrap();
        assert_eq!(fetched.status_flags, SNAPSHOT_FLAG_CLOSED | SNAPSHOT_FLAG_EMPTY);
    });
}

#[test]
fn test_mixed_interval_and_usage_sets_both_flags() {
    let (env, contract_id) = setup();
    let sub_id = 5;
    let period_index = 0;

    env.as_contract(&contract_id, || {
        // 1. Write usage charge
        let usage_snapshot = BillingPeriodSnapshot {
            subscription_id: sub_id,
            period_index,
            period_start: 100,
            period_end: 150,
            total_charged: 200,
            total_usage_units: 20,
            status_flags: SNAPSHOT_FLAG_USAGE_CHARGED,
            finalized_at: 150,
        };
        assert!(write_period_snapshot(&env, usage_snapshot).is_ok());

        // 2. Write interval charge closing the period
        let interval_snapshot = BillingPeriodSnapshot {
            subscription_id: sub_id,
            period_index,
            period_start: 100,
            period_end: 200,
            total_charged: 1000,
            total_usage_units: 0,
            status_flags: SNAPSHOT_FLAG_CLOSED | SNAPSHOT_FLAG_INTERVAL_CHARGED,
            finalized_at: 200,
        };
        assert!(write_period_snapshot(&env, interval_snapshot).is_ok());

        let fetched = get_period_snapshot(&env, sub_id, period_index).unwrap();
        
        // Assert merged state
        assert_eq!(fetched.total_charged, 1200);
        assert_eq!(fetched.total_usage_units, 20);
        assert_eq!(
            fetched.status_flags,
            SNAPSHOT_FLAG_CLOSED | SNAPSHOT_FLAG_INTERVAL_CHARGED | SNAPSHOT_FLAG_USAGE_CHARGED
        );
        assert_eq!(fetched.period_start, 100);
        assert_eq!(fetched.period_end, 200);
        assert_eq!(fetched.finalized_at, 200); // the max finalized_at
    });
}

#[test]
fn test_integrity_checks() {
    let (env, contract_id) = setup();
    
    env.as_contract(&contract_id, || {
        // Invalid boundaries
        let bad_bounds = BillingPeriodSnapshot {
            subscription_id: 6,
            period_index: 0,
            period_start: 200,
            period_end: 100, // end < start
            total_charged: 100,
            total_usage_units: 0,
            status_flags: SNAPSHOT_FLAG_USAGE_CHARGED,
            finalized_at: 150,
        };
        assert_eq!(write_period_snapshot(&env, bad_bounds), Err(Error::InvalidInput));

        // Interval charge requires period_start < period_end
        let bad_interval = BillingPeriodSnapshot {
            subscription_id: 6,
            period_index: 0,
            period_start: 100,
            period_end: 100, // start == end
            total_charged: 100,
            total_usage_units: 0,
            status_flags: SNAPSHOT_FLAG_INTERVAL_CHARGED,
            finalized_at: 100,
        };
        assert_eq!(write_period_snapshot(&env, bad_interval), Err(Error::InvalidInput));
    });
}

/// At exactly MAX_BILLING_PERIOD_SNAPSHOTS_PER_SUBSCRIPTION entries the index
/// is full but no eviction has occurred yet.  The (cap+1)-th write triggers the
/// first eviction and the oldest entry disappears from the index.
#[test]
fn test_snapshot_index_eviction_at_cap_boundary() {
    let (env, contract_id) = setup();
    let sub_id = 7;
    let cap = MAX_BILLING_PERIOD_SNAPSHOTS_PER_SUBSCRIPTION;

    env.as_contract(&contract_id, || {
        // Fill the index to exactly cap entries (period_indices 0 .. cap-1).
        for i in 0..cap {
            let snapshot = BillingPeriodSnapshot {
                subscription_id: sub_id,
                period_index: i as u64,
                period_start: 100 + i as u64 * 100,
                period_end: 200 + i as u64 * 100,
                total_charged: 1,
                total_usage_units: 0,
                status_flags: SNAPSHOT_FLAG_INTERVAL_CHARGED | SNAPSHOT_FLAG_CLOSED,
                finalized_at: 200 + i as u64 * 100,
            };
            assert!(write_period_snapshot(&env, snapshot).is_ok());
        }

        // Index is full; oldest entry (period_index 0) must still be present.
        assert!(
            get_period_snapshot(&env, sub_id, 0).is_some(),
            "oldest snapshot must still exist after exactly cap writes"
        );

        // Write entry cap (the (cap+1)-th, 0-indexed as `cap`).  This should
        // evict period_index 0 and retain period_indices 1..=cap.
        let overflow_snapshot = BillingPeriodSnapshot {
            subscription_id: sub_id,
            period_index: cap as u64,
            period_start: 100 + cap as u64 * 100,
            period_end: 200 + cap as u64 * 100,
            total_charged: 1,
            total_usage_units: 0,
            status_flags: SNAPSHOT_FLAG_INTERVAL_CHARGED | SNAPSHOT_FLAG_CLOSED,
            finalized_at: 200 + cap as u64 * 100,
        };
        assert!(write_period_snapshot(&env, overflow_snapshot).is_ok());

        // The index still holds exactly cap entries.
        let all = list_period_snapshots(&env, sub_id, cap + 1);
        assert_eq!(
            all.len(),
            cap,
            "index must hold exactly cap={} entries after eviction",
            cap
        );

        // Newest entry (period_index == cap) must be present.
        assert!(
            get_period_snapshot(&env, sub_id, cap as u64).is_some(),
            "newest snapshot (period_index={cap}) must be present"
        );

        // Oldest entry (period_index 0) must have been evicted from the index.
        // Note: the snapshot record itself may still exist in persistent storage,
        // but it is no longer reachable via list_period_snapshots.
        let listed = list_period_snapshots(&env, sub_id, cap);
        let found_zero = (0..listed.len()).any(|i| listed.get(i).unwrap().period_index == 0);
        assert!(
            !found_zero,
            "period_index 0 must have been evicted from the rolling index"
        );
    });
}

/// Writing the cap-th entry (1-indexed) must NOT evict: eviction only fires
/// when the post-insert count would exceed the cap.
#[test]
fn test_snapshot_index_no_premature_eviction_at_cap() {
    let (env, contract_id) = setup();
    let sub_id = 8;
    let cap = MAX_BILLING_PERIOD_SNAPSHOTS_PER_SUBSCRIPTION;

    env.as_contract(&contract_id, || {
        // Write exactly cap entries (period_indices 0 .. cap-1).
        for i in 0..cap {
            let snapshot = BillingPeriodSnapshot {
                subscription_id: sub_id,
                period_index: i as u64,
                period_start: 100 + i as u64 * 100,
                period_end: 200 + i as u64 * 100,
                total_charged: 1,
                total_usage_units: 0,
                status_flags: SNAPSHOT_FLAG_INTERVAL_CHARGED | SNAPSHOT_FLAG_CLOSED,
                finalized_at: 200 + i as u64 * 100,
            };
            assert!(write_period_snapshot(&env, snapshot).is_ok());
        }

        // All cap entries must be reachable — no premature eviction.
        let all = list_period_snapshots(&env, sub_id, cap);
        assert_eq!(
            all.len(),
            cap,
            "all {cap} snapshots must be present without eviction"
        );

        // The very first entry must still be in the index.
        let found_zero = (0..all.len()).any(|i| all.get(i).unwrap().period_index == 0);
        assert!(
            found_zero,
            "period_index 0 must not be evicted until cap+1 entries are written"
        );
    });
}