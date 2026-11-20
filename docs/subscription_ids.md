# Subscription ID Semantics

This document describes how subscription IDs are generated, what guarantees they provide, and how overflow is prevented in the `subscription_vault` contract.

---

## Overview

Every subscription is identified by a `u32` integer assigned at creation time by the internal `_next_id` helper. IDs are **stable**, **unique**, and **monotonically increasing** for the lifetime of the contract.

---

## ID Properties

| Property | Value |
|---|---|
| **Type** | `u32` |
| **First ID** | `0` |
| **Maximum ID** | `u32::MAX` = 4 294 967 295 (type bound; the allocator's overflow backstop) |
| **Allocation step** | `+1` per subscription |
| **Reuse** | Never — cancelled/expired subscriptions keep their ID forever |
| **Enforced ceiling** | `MAX_TOTAL_SUBSCRIPTIONS` = 1 000 000 subscriptions per contract instance |
| **Total capacity** | `MAX_TOTAL_SUBSCRIPTIONS` (1 000 000) — bounded so the contract's persistent storage budget cannot be exhausted |

---

## How IDs Are Allocated

Each call to `create_subscription` internally calls `_next_id`:

```rust
fn _next_id(env: &Env) -> Result<u32, Error> {
    let current: u32 = env.storage().instance().get(&DataKey::NextId).unwrap_or(0u32);

    if current >= MAX_TOTAL_SUBSCRIPTIONS {
        return Err(Error::SubscriptionLimitReached);
    }

    env.storage().instance().set(&DataKey::NextId, &(current + 1));
    Ok(current)
}
```

Steps:
1. Read the current counter (defaults to `0` on first use).
2. **Ceiling guard**: if `current >= MAX_TOTAL_SUBSCRIPTIONS`, return `SubscriptionLimitReached` (counter left unchanged).
3. Write `current + 1` back (safe — step 2 guarantees `current < MAX_TOTAL_SUBSCRIPTIONS ≤ u32::MAX`).
4. Return `current` as the newly allocated ID.

---

## Overflow Prevention

The original implementation used `id + 1` with no guard, which would:
- **Panic** in Rust debug builds at `u32::MAX + 1`.
- **Silently wrap to `0`** in release builds, overwriting the first subscription and breaking all uniqueness guarantees.

The hardened version **guards before incrementing**, so neither panic nor wrap can occur. Instead, callers receive a clean `Error::SubscriptionLimitReached` (code `6001`). The guard also enforces the practical ceiling `MAX_TOTAL_SUBSCRIPTIONS` (1 000 000), well below `u32::MAX`, so the contract cannot mint enough `DataKey::Sub(id)` persistent records to exhaust its storage budget.

---

## Named Limit Constants

```rust
/// Type bound for a subscription ID; the allocator's overflow backstop.
pub const MAX_SUBSCRIPTION_ID: u32 = u32::MAX;

/// Enforced ceiling on the total subscriptions a contract instance will allocate.
/// Once the counter reaches it, the next creation returns `SubscriptionLimitReached`
/// and the counter is left unchanged.
pub const MAX_TOTAL_SUBSCRIPTIONS: u32 = 1_000_000;
```

`MAX_TOTAL_SUBSCRIPTIONS` is the enforced practical cap: it bounds the number of
`DataKey::Sub(id)` persistent records a contract can create, preventing unbounded
growth from exhausting the persistent storage budget. `MAX_SUBSCRIPTION_ID` remains
as the `u32` type bound / overflow backstop.

Using named constants (rather than inline `u32::MAX`) means:
- The limit is self-documenting.
- It is trivially changeable if a different practical cap is desired.
- Tests can reference it directly instead of embedding magic numbers.

---

## Querying the Count

```rust
pub fn get_subscription_count(env: Env) -> u32
```

Returns the current value of the internal counter, which equals the total number of subscriptions ever created (including cancelled and expired ones). This is a read-only, zero-cost call.

```rust
let total = client.get_subscription_count();
println!("Total subscriptions created: {}", total);
```

---

## Storage Layout

| Storage key | Type | Description |
|---|---|---|
| `"next_id"` (Symbol) | `u32` | Next ID to be allocated; absent until first subscription |
| `<subscription_id>` (u32) | `Subscription` | Subscription data keyed by its ID |

Both live in instance storage and share the contract's storage budget.

---

## Error Reference

| Code | Name | When |
|---|---|---|
| `6001` | `SubscriptionLimitReached` | Counter reached `MAX_TOTAL_SUBSCRIPTIONS`; no more IDs available |
| `404` | `NotFound` | Subscription ID does not exist in storage |

---

## Test Coverage

Boundary coverage lives in `tests/id_exhaustion.rs`, which seeds `DataKey::NextId`
via `env.as_contract` so exactly one allocation succeeds before the ceiling:

| Test | Scenario |
|---|---|
| `create_subscription_last_id_succeeds` | Counter at `MAX_TOTAL_SUBSCRIPTIONS - 1` → allocation returns `MAX_TOTAL_SUBSCRIPTIONS - 1`, counter advances to `MAX_TOTAL_SUBSCRIPTIONS` |
| `create_subscription_at_max_returns_limit_reached` | Counter at `MAX_TOTAL_SUBSCRIPTIONS` → `SubscriptionLimitReached` |
| `create_subscription_counter_unchanged_after_failure` | Failed allocation leaves the counter unchanged (no wrap, no increment) |
| `create_subscription_with_token_*` | Same boundary checks via the token-specific path |
| `create_subscription_from_plan_*` | Same boundary checks via the plan path, now also returning `SubscriptionLimitReached` (aligned with the other paths) |
