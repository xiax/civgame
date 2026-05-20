# Plan: Fix Performance Leaks (Market & Job Escrow)

## Problem Statement
The game experiences performance degradation over time due to two primary leaks:
1. **Market Listing Bloat:** `Market::listings` grows indefinitely because manufactured items are never removed when their quantity hits zero.
2. **JobEscrow Entity Leak:** The `chief_job_posting_system` silently drops unclaimed jobs (Stockpile, Farm, etc.) without firing a `JobCompletedEvent`. This leaves `JobEscrow` entities orphaned in the world, bloats the `JobEscrowIndex`, and permanently leaks faction treasury funds.

## Proposed Changes

### 1. Market Listing Pruning (`src/economy/market.rs`)
Modify `try_buy_inner` to remove entries from the `listings` vector when the quantity reaches zero.
- **Impact:** Prevents the $O(N)$ market scan from slowing down as the game progresses.

### 2. Job Completion & Escrow Cleanup (`src/simulation/jobs.rs`)
Refactor the cleanup logic in `chief_job_posting_system` to be explicit rather than using `retain`.
- **Logic:**
    - Identify all jobs that need to be dropped (unclaimed Stockpile/Farm/Calories/Crafting/Build, or satisfied Haul).
    - For each dropped job, emit a `JobCompletedEvent`.
    - If it's a satisfied `Haul` job, set `completed: true`.
    - If it's an unclaimed job being dropped for recalculation, set `completed: false`.
- **Result:** The `job_payout_system` will receive these events, find the corresponding `JobEscrow` via `JobEscrowIndex`, and despawn it. If `completed: false`, the escrow's `on_remove` hook will automatically refund the funds to the faction treasury.

## Verification Plan

### Automated Tests
- **Market Test:** Add a test case to `src/economy/market.rs` that adds an item, buys it out, and asserts that `listings.len()` returns to its original state.
- **Job Escrow Test:** Add a test case to `src/simulation/jobs.rs` (or a integration test) that simulates a chief posting a funded job, waits for the cleanup interval, and verifies that the `JobEscrow` entity is despawned and funds are refunded.

### Manual Verification
- Run a long game and monitor entity counts for `JobEscrow` using the debug inspector.
- Check faction treasuries to ensure they don't hit zero and stay there due to "invisible" escrowed funds.
- Monitor frame times to ensure they remain stable after many market transactions.
