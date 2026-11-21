# Subscription Cancellation and Refunds

## State Transitions

Subscriptions exist in one of the following states: `Active`, `Paused`, `Cancelled`, or `InsufficientBalance`.

When a subscription is cancelled via the `cancel_subscription` endpoint:

- The state transitions directly to `Cancelled`.
- This operation is idempotent: if the subscription is already `Cancelled`, the call succeeds without error and makes no changes.
- Cancellation guarantees that no further charges can be made against the subscription, as the billing engine will reject processing for non-Active states.

## Authorization

Cancellation can be triggered by either:

1. **The Subscriber** (the entity paying for the subscription)
2. **The Merchant** (the entity receiving the recurring payments)

Requiring authorization from either party ensures flexibility and protects both user autonomy and merchant management policies.

## Refund Model: Time-Locked Cancellation Escrow

When a subscriber deposits funds into their `SubscriptionVault` for a specific subscription, those funds are credited to the `prepaid_balance`.

If the subscription is cancelled before these funds are exhausted, the remaining `prepaid_balance` belongs to the subscriber.

Rather than transferring that balance to the subscriber synchronously during the cancellation call, `cancel_subscription` places it into a **time-locked cancellation escrow**. This holds the funds for `CANCELLATION_ESCROW_WINDOW_SECS` (24 hours) so the merchant has a window to dispute a wrongful cancellation, while guaranteeing the subscriber can always recover the funds once the window elapses.

### What `cancel_subscription` does

For a subscription with a non-zero `prepaid_balance`, the call:

1. Transitions the status to `Cancelled` and zeroes `prepaid_balance` in the stored subscription.
2. Writes a `DataKey::CancellationEscrow(subscription_id)` record containing the amount, token, subscriber, merchant, and `released_at = now + CANCELLATION_ESCROW_WINDOW_SECS`.
3. Emits a `cancellation_escrow_opened` event.

The tokens are not moved: they remain in the contract and stay counted in `total_accounted`. The accounting ledger is only decremented when the escrow is later released.

### Why not refund immediately?

1. **Merchant Independence**: If an immediate token transfer fails (e.g. the subscriber's Stellar account loses trustlines or becomes frozen), an automatic refund would fail the entire cancellation transaction. Holding funds in escrow lets a merchant _always_ cancel a problematic subscription without being blocked by external token-transfer constraints.
2. **Dispute Window**: The hold window gives the merchant a bounded opportunity to contest a cancellation before the funds leave the vault.
3. **Reentrancy Protection**: Release is a separate "pull" call, following the recommended pull-over-push pattern for fund distribution.

### Atomicity of the escrow write

The status transition and the escrow write are **transactionally atomic**. In Soroban, a contract invocation executes as a single transaction frame and every storage write performed during it (the `Subscription` record via `write_subscription` and the `CancellationEscrow` record via `persistent().set`) is committed together, or the whole invocation reverts and no write takes effect.

As a consequence there is no "TTL-zero eviction race": a persistent entry cannot be evicted between the status write and the escrow write, because neither is visible to any other transaction until the invocation commits, and both are applied at that same commit point. Either the subscription is marked `Cancelled` **and** the escrow record exists, or neither change is persisted. The escrow can never be silently dropped while the subscription is marked cancelled.

Both writes are also ordered before any external interaction (Checks-Effects-Interactions), and re-entrant calls are additionally blocked by the contract's reentrancy guard.

### Window behaviour

- **During the window** (`now < released_at`): the merchant may call `lodge_escrow_dispute` to convert the escrow into a live `Dispute`. The subscriber cannot yet claim; `claim_cancellation_escrow` and the escrow branch of `withdraw_subscriber_funds` fail with `EscrowNotReleased`.
- **After the window** (`now >= released_at`): the subscriber claims the refund with either `claim_cancellation_escrow` or `withdraw_subscriber_funds`. The vault transfers the escrowed amount, removes the escrow record, decrements `total_accounted`, and emits `cancellation_escrow_released`.

### Getting a Refund

1. `cancel_subscription` transitions the status to `Cancelled` and opens the escrow (see above).
2. Once `CANCELLATION_ESCROW_WINDOW_SECS` has elapsed (and no dispute is open), the subscriber calls `claim_cancellation_escrow(subscriber, subscription_id)` or `withdraw_subscriber_funds(subscription_id, subscriber)`.
3. The vault transfers the escrowed amount (USDC or equivalent token) from the contract balance to the subscriber's address, removes the escrow record, and decrements the accounting ledger.
