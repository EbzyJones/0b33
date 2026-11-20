//! Shared ring-buffer idempotency key helpers.
//!
//! Three entrypoints use idempotency keys: `charge_subscription`,
//! `deposit_funds`, and `charge_one_off`.  Each domain is scoped with a
//! unique domain constant so that reusing the same raw 32-byte key across
//! different entrypoints does **not** produce a replay collision.
//!
//! Storage key: `DataKey::IdemKey(subscription_id)` stores `IdemRingBuffer`.
//!
//! ## Replay-protection window
//!
//! Each entry stores the hash **and** the ledger timestamp at insertion.
//! `check_key` ignores entries older than the configured TTL; they are treated
//! as if they were never inserted. `push_key` removes expired entries before
//! persisting a new key.

use crate::types::{DataKey, Error};
use soroban_sdk::{contracttype, BytesN, Env, Symbol, Vec};

/// Number of idempotency slots retained per subscription.
///
/// 64 slots comfortably covers high-frequency billing (e.g. daily charges
/// over two months) while keeping per-subscription storage overhead small.
pub(crate) const IDEM_HISTORY: u32 = 64;

/// Default duration in seconds for which an idempotency entry remains active.
///
/// Set to 7 days: long enough to survive any reasonable retry window for
/// weekly or monthly subscriptions, short enough that a cycling attack
/// would require 64 charges within the TTL window to succeed — a scenario
/// that cannot happen under normal subscription billing frequencies.
pub(crate) const IDEM_TTL_SECS: u64 = 7 * 24 * 60 * 60; // 7 days

/// Maximum allowed idempotency TTL (365 days).
///
/// Prevents misconfiguration that would effectively make the ring buffer
/// permanent and exhaust slots for long-lived subscriptions.
pub(crate) const IDEM_TTL_MAX_SECS: u64 = 365 * 24 * 60 * 60;

/// Ring buffer of recently seen idempotency-key hashes with insertion timestamps.
///
/// Each entry is `(hash, inserted_at_timestamp)`. Entries older than the
/// configured TTL are considered expired and will not match on lookup.
#[contracttype]
#[derive(Clone, Debug)]
pub(crate) struct IdemRingBuffer {
    pub entries: Vec<(BytesN<32>, u64)>,
    pub cursor: u32,
}

/// Return the configured idempotency-key lifetime, defaulting to seven days.
///
/// Reads from persistent storage via `DataKey::IdempotencyTtlSecs` (the same
/// `read_config` path used by `GracePeriod`, `DisputeWindowSecs`, etc.).
pub fn get_ttl_secs(env: &Env) -> u64 {
    crate::admin::read_config::<u64>(env, &DataKey::IdempotencyTtlSecs)
        .unwrap_or(IDEM_TTL_SECS)
}

/// Apply a validated idempotency-TTL value.
///
/// Used by both the direct admin path (`set_idempotency_ttl` in `lib.rs`,
/// which enforces the cooldown before calling here) and the governance bypass
/// path (`ProposalKind::SetIdempotencyTtl` in `governance.rs`, which calls
/// here directly, intentionally skipping the cooldown).
///
/// Emits [`IdemTtlUpdatedEvent`] on success.
pub fn apply_ttl_secs(env: &Env, admin: &soroban_sdk::Address, ttl_secs: u64) -> Result<(), Error> {
    if ttl_secs == 0 || ttl_secs > IDEM_TTL_MAX_SECS {
        return Err(Error::InvalidInput);
    }
    let old_ttl_secs = get_ttl_secs(env);
    crate::admin::write_config(env, &DataKey::IdempotencyTtlSecs, &ttl_secs);
    env.events().publish(
        (Symbol::new(env, "idem_ttl_updated"),),
        crate::types::IdemTtlUpdatedEvent {
            admin: admin.clone(),
            old_ttl_secs,
            new_ttl_secs: ttl_secs,
            timestamp: env.ledger().timestamp(),
            schema_version: crate::types::EVENT_SCHEMA_VERSION,
            contract_id: env.current_contract_address(),
        },
    );
    Ok(())
}

/// Return the raw byte representation of a 32-byte idempotency key.
fn key_bytes(key: &BytesN<32>) -> [u8; 32] {
    let mut out = [0u8; 32];
    let arr = key.to_array();
    out.copy_from_slice(&arr);
    out
}

/// Hash (domain, subscription_id, raw_key) into a 32-byte fingerprint.
///
/// The caller **must** supply the correct `domain` constant for their
/// entrypoint so that two different operations receiving the same raw key
/// produce different fingerprints.
pub fn hash_idem_key(
    env: &Env,
    domain: u32,
    subscription_id: u32,
    raw_key: &BytesN<32>,
) -> BytesN<32> {
    let raw = key_bytes(raw_key);
    let mut buf = [0u8; 40];
    buf[..4].copy_from_slice(&domain.to_be_bytes());
    buf[4..8].copy_from_slice(&subscription_id.to_be_bytes());
    buf[8..40].copy_from_slice(&raw);
    let input = soroban_sdk::Bytes::from_slice(env, &buf);
    env.crypto().sha256(&input).into()
}

/// Load the ring buffer for `subscription_id`.
///
/// Returns an empty buffer when no idempotency key has ever been stored,
/// or when the stored value cannot be deserialized (e.g. old on-chain format
/// from before the timestamp migration).
fn load_buffer(env: &Env, subscription_id: u32) -> IdemRingBuffer {
    env.storage()
        .instance()
        .get(&DataKey::IdemKey(subscription_id))
        .unwrap_or(IdemRingBuffer {
            entries: Vec::new(env),
            cursor: 0,
        })
}

/// Persist the ring buffer for `subscription_id`.
fn save_buffer(env: &Env, subscription_id: u32, buf: &IdemRingBuffer) {
    env.storage()
        .instance()
        .set(&DataKey::IdemKey(subscription_id), buf);
}

/// Check whether `hashed` already exists in the ring buffer **and** is still
/// within the TTL window.
///
/// Returns `true` when the key is a live duplicate (replay).
/// Expired entries are skipped and treated as absent.
pub fn check_key(env: &Env, subscription_id: u32, hashed: &BytesN<32>) -> bool {
    check_key_at(env, subscription_id, hashed, env.ledger().timestamp())
}

/// Testable variant of `check_key` that accepts an explicit `now` timestamp.
pub(crate) fn check_key_at(
    env: &Env,
    subscription_id: u32,
    hashed: &BytesN<32>,
    now: u64,
) -> bool {
    let buf = load_buffer(env, subscription_id);
    let ttl_secs = get_ttl_secs(env);
    for entry in buf.entries.iter() {
        let (stored_hash, inserted_at) = entry;
        // Skip entries that have aged out of the replay-protection window.
        if now.saturating_sub(inserted_at) >= ttl_secs {
            continue;
        }
        if stored_hash == *hashed {
            return true;
        }
    }
    false
}

/// Insert a new idempotency key hash into the ring buffer.
///
/// `now` must be the current ledger timestamp so that the TTL check in
/// `check_key` can determine whether each entry is still active.
///
/// Expired entries are removed before insertion.  If after pruning the buffer
/// is still at `IDEM_HISTORY` capacity (all retained entries are live), this
/// function returns [`Error::IdemBufferFull`] rather than evicting a live key.
/// Silently overwriting a live slot would open a replay window: an attacker
/// who evicted slot N could replay key N as a fresh charge.
///
/// Under normal billing cadences this error cannot occur: filling 64 slots
/// within the 7-day TTL requires a charge every ~2.6 hours.
pub fn push_key(env: &Env, subscription_id: u32, hashed: &BytesN<32>, now: u64) -> Result<(), Error> {
    let mut buf = load_buffer(env, subscription_id);
    let ttl_secs = get_ttl_secs(env);
    let len = buf.entries.len();
    if len > 0 {
        let start = if len == IDEM_HISTORY {
            buf.cursor % len
        } else {
            0
        };
        let mut retained = Vec::new(env);
        for offset in 0..len {
            let idx = (start + offset) % len;
            let entry = buf.entries.get(idx).unwrap();
            if now.saturating_sub(entry.1) < ttl_secs {
                retained.push_back(entry);
            }
        }
        buf.entries = retained;
    }

    // After pruning expired entries, refuse to evict a live slot.
    if buf.entries.len() >= IDEM_HISTORY {
        return Err(Error::IdemBufferFull);
    }

    buf.entries.push_back((hashed.clone(), now));
    buf.cursor = 0;
    save_buffer(env, subscription_id, &buf);
    Ok(())
}
