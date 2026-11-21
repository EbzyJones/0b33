# Merchant Balance Ceiling - Flow Diagrams

## Charge Flow with Ceiling Gate

```
User initiates charge
        ↓
charge_subscription() / charge_usage() / charge_one_off()
        ↓
    [ Calculate charge amount ]
        ↓
credit_merchant_balance_for_token(
    merchant,
    token,
    amount,
    kind
)
        ↓
    [ INPUT VALIDATION ]
    if amount < 0:
        return Err(Error::InvalidAmount)
        ↓
    [ BALANCE UPDATE - THE GATE ]
    current_balance ← storage
    new_balance ← safe_add_merchant_balance(current_balance, amount)
    
        ↓
        ├─ Check: amount < 0?
        │   ├─ YES → return Err(Underflow)
        │   └─ NO → continue
        │
        ├─ Check: safe_add(current, amount) → i128::MAX?
        │   ├─ YES → return Err(Overflow)
        │   └─ NO → new_balance ← current + amount
        │
        ├─ Check: new_balance > CEILING?
        │   ├─ YES → return Err(Overflow) ← NEW GATE
        │   └─ NO → continue
        │
        └─ return Ok(new_balance)
        ↓
    [ STATE WRITE - CEI PATTERN ]
    ✓ Set merchant balance in storage
    ✓ Update earnings records
    ✓ Register token
    ✓ Assert accounting invariant
        ↓
    [ EVENTS ]
    ✓ Emit MerchantBalanceSnapshot
        ↓
    OK - Charge applied successfully
        ↓
Return to subscriber charge flow
```

## Decision Tree

```
                        Entry: safe_add_merchant_balance(balance, amount)
                                        ↓
                              Is amount < 0?
                             /            \
                           YES            NO
                            ↓              ↓
                   return Underflow      Compute: new_balance = balance + amount
                                                  ↓
                                        Does new_balance overflow i128?
                                         /                      \
                                       YES                       NO
                                        ↓                        ↓
                                 return Overflow           Is new_balance > CEILING?
                                                            /              \
                                                          YES              NO
                                                           ↓               ↓
                                                    return Overflow   return Ok(new_balance)
                                                         ↑                 ↑
                                                         │                 │
                                             ← NEW GATE ─┘                 └→ SUCCESS
```

## State Transitions

### Successful Charge (Normal Case)

```
Merchant State Before:
├─ Balance: 1,000,000
├─ Accruals.interval: 100,000,000
├─ Status: Active
└─ Vault token balance: $5,000,000

Input: Charge subscription for $5,000

Processing:
1. safe_add_merchant_balance(1,000,000, 5,000) → Ok(1,005,000) ✓
2. Set balance = 1,005,000
3. Update accruals.interval = 100,005,000
4. Emit MerchantBalanceSnapshot

Merchant State After:
├─ Balance: 1,005,000 ✓
├─ Accruals.interval: 100,005,000 ✓
├─ Status: Active
└─ Vault token balance: $5,000,000
```

### Rejected Charge (Near Ceiling)

```
Merchant State Before:
├─ Balance: 4,611,686,018,427,387,000 (just below ceiling)
├─ Accruals.interval: 5,000,000,000,000,000,000
├─ Status: Active
└─ Vault token balance: $5,000,000

Input: Charge subscription for $5,000

Processing:
1. safe_add_merchant_balance(4,611,686,018,427,387,000, 5,000)
2. Check: 5,000 < 0? NO → continue
3. Check: 4,611,686,018,427,387,000 + 5,000 > i128::MAX? NO → new_balance = 4,611,686,018,427,392,000
4. Check: 4,611,686,018,427,392,000 > CEILING (4,611,686,018,427,387,903)? YES → return Err(Overflow)
5. NO STATE MUTATION ✓

Merchant State After (unchanged):
├─ Balance: 4,611,686,018,427,387,000 ✓ (UNCHANGED)
├─ Accruals.interval: 5,000,000,000,000,000,000 ✓ (UNCHANGED)
├─ Status: Active
└─ Vault token balance: $5,000,000 ✓ (UNCHANGED)

Return: Error::Overflow
```

## Error Paths

### Path 1: Negative Amount (Should Never Happen)

```
Input: amount = -5,000 (logic error in charge calculation)

safe_add_merchant_balance(balance, -5,000)
        ↓
Is -5,000 < 0? YES
        ↓
return Err(Error::Underflow)
        ↓
No state mutation
        ↓
Charge fails - operator investigates calculation logic
```

### Path 2: i128 Overflow (Should Never Happen)

```
Input: balance very close to i128::MAX, large charge amount

safe_add_merchant_balance(balance, amount)
        ↓
Is amount < 0? NO → continue
        ↓
safe_add(balance, amount)
        ↓
balance + amount > i128::MAX? YES
        ↓
return Err(Error::Overflow)
        ↓
No state mutation
        ↓
Charge fails - operator investigates data corruption
```

### Path 3: Ceiling Exceeded (Expected at Scale)

```
Input: balance = 4.6e18 - 1, amount = 2 (exceeds CEILING)

safe_add_merchant_balance(balance, amount)
        ↓
Is amount < 0? NO → continue
        ↓
safe_add(balance, amount) → 4.6e18 + 1 (no overflow to i128::MAX)
        ↓
Is new_balance > CEILING? YES ← NEW CHECK
        ↓
return Err(Error::Overflow)
        ↓
No state mutation
        ↓
Charge fails - operator notifies merchant to withdraw funds
```

## Invariant Preservation

### Before Ceiling Implementation

```
                    i128::MAX
                      │
                      │ (unreliable arithmetic range)
                      │
Danger Zone:     Balance > i128::MAX / 2 ← could approach limit
                      │                     through normal ops
                      │
Safe Zone:       Balance <= i128::MAX / 2  ← reliable arithmetic
                      │
                      0
```

### After Ceiling Implementation

```
                    i128::MAX
                      │
                      │
                      │
Protected:       Balance > CEILING ← REJECTED (gated out)
                      │   ↑
                      │   └─── NEW GATE (i128::MAX / 4)
                      │
Safe Zone:       Balance <= CEILING ← ACCEPTED (always in safe range)
                      │
                      0
```

## Reconciliation Example

### Before (Near Overflow)

```
Accruals = 9,223,372,036,854,770,000
Withdrawals = 9,223,372,036,854,760,000
Refunds = 0
Computed = 9,223,372,036,854,770,000 - 9,223,372,036,854,760,000 = 10,000

Stored Balance = ??? (could be wrong due to overflow)

Mismatch = Reconciliation FAILS ✗ (unreliable)
```

### After (With Ceiling)

```
Accruals = 4,500,000,000,000,000,000 (stays below ceiling)
Withdrawals = 4,000,000,000,000,000,000
Refunds = 0
Computed = 4,500,000,000,000,000,000 - 4,000,000,000,000,000,000 = 500,000,000,000,000,000

Stored Balance = 500,000,000,000,000,000

Mismatch = 0 (PERFECT MATCH) ✓ (always reliable)
```

## Storage Layout

```
Subscription Vault Contract
├─ Instance Storage
│  ├─ Admin address
│  ├─ Token address
│  ├─ Emergency stop flag
│  └─ ...
│
└─ Per-Merchant Storage
   ├─ DataKey::MerchantBalance(merchant, token)
   │  └─ i128: current balance
   │     └─ Range: [0, CEILING] ← GATED
   │
   ├─ DataKey::MerchantEarnings(merchant, token)
   │  ├─ accruals.interval
   │  ├─ accruals.usage
   │  ├─ accruals.one_off
   │  ├─ withdrawals
   │  └─ refunds
   │
   └─ ... (other merchant data)
```

## Timeline of a Charge

```
T=0: User initiates charge
     charge_amount = $5,000

T=1: Validate subscription status
     → Active ✓

T=2: Calculate fee
     fee = charge_amount * fee_bips / 10000 ✓

T=3: Load merchant balance
     current_balance = 1,000,000 ✓

T=4: CEILING GATE ← check balance + charge_amount
     new_balance = safe_add_merchant_balance(1,000,000, 5,000)
     → Is 5,000 < 0? NO
     → Is 1,000,005,000 > i128::MAX? NO
     → Is 1,000,005,000 > CEILING? NO ✓
     → return Ok(1,005,000)

T=5: Update balance
     store 1,005,000 ✓

T=6: Update earnings
     accruals.interval += 5,000 ✓

T=7: Transfer fee to treasury
     token.transfer(...) ✓

T=8: Emit events ✓

T=9: Return success ✓

Total: ~10ms (includes token transfer)
Ceiling gate: <1μs
```

## Visual: Balance Over Merchant Lifetime

```
Balance
  │
  │
  │
  │    ╱╱╱ Steady growth from charges
  │   ╱╱╱
  │  ╱╱╱╱ Few withdrawals
  │ ╱╱╱╱
  │     ╲  Large withdrawal
  │      ╲  (merchant pays out)
  │       ╲
  │        ╱╱ Resume growth
CEILING → ─────────────────────── (NEW GATE)
  │       ✗ Charges rejected here
  │       ✗ Merchant must withdraw
  │
  │ Previous: could drift higher causing:
  │ - Reconciliation failures
  │ - Charge failures
  │ - Silent earnings loss
  │
0 │
  └─────────────────────────────────→ Time (months/years)
    Month 1   Month 12   Year 10  Year 30+
```
