# Fix Fractional Work Stalls

## Context

Workers reaching valid adjacent targets enter `AiState::Working`, but the work-progress accumulator in `movement_system` truncates the slowdown-scaled delta to `u8`:

```rust
let progress = (base * factor) as u8;             // truncates fractions to 0
ai.work_progress = ai.work_progress.saturating_add(progress);
```

At fixed-update 20 Hz, `base = sim_dt * 20.0 ≈ 1.0` per tick. Any `factor < 1.0` from `sickness_work_factor()` or `Energy::energy_factor()` produces a sub-1 product, casts to 0, and the worker accumulates nothing. Designed behavior (per `simulation/CLAUDE.md` § Energy and § Healing pipeline) is that tired/sick workers work **slower**, not never.

Two write sites: `src/simulation/movement.rs:236-237` (working-in-place while `dist > 2.0`) and `src/simulation/movement.rs:829-830` (arrival path).

## Approach

Add a fractional sub-tick remainder field on `PersonAI`. `movement_system` accumulates `base * factor` into it; whole units roll into the existing `u8 work_progress`, the fraction persists across ticks. Average rate is preserved exactly (`factor = 0.45` → +1 progress every ~2.22 ticks).

Rejected alternatives: `round`/`ceil` (still stalls or removes the slowdown), widening to `u16` (~40-site refactor for no behavioral gain), `f32` work_progress (equality-comparison hazard against many thresholds).

## Scope correction vs. earlier draft

Do **not** broadcast a `reset_work_progress()` helper across every executor. Most `ai.work_progress = 0` sites in executors are *per-cycle* resets inside a still-running task (e.g. `gather.rs` per-stone-level, `production.rs` per-craft-cycle, `farm.rs` per-tile prep, `draftwork.rs` per-tile plow, `nomad_pack_labor.rs` per-structure). Carrying the fractional remainder across those resets is *correct* — dropping it slightly extra-penalises tired workers at every cycle boundary, an unintended behavior change.

Route fractional zero-out through the four canonical task-boundary helpers in `typed_task.rs` only — they already own task entry/exit semantics (CLAUDE.md `simulation/CLAUDE.md:120,193`).

## Implementation

### 1. `src/simulation/person.rs` — new field + accumulator

After `work_progress: u8` (line ~221):

```rust
/// Sub-tick remainder of slowdown-scaled work accrual. `movement_system`
/// accumulates `base * factor` here; whole units roll into `work_progress`
/// and the fraction persists across ticks. Zeroed at task entry/exit by
/// `ActionQueue::{begin_working, finish_task, cancel_chain}`.
pub(in crate::simulation) work_progress_fraction: f32,
```

In `PersonAI::default()` (line ~283): `work_progress_fraction: 0.0,`.

In `impl PersonAI`:

```rust
#[inline]
pub fn add_work_progress(&mut self, amount: f32) {
    if !(amount > 0.0) {
        return;
    }
    let total = self.work_progress_fraction + amount;
    let whole = total.floor();
    self.work_progress_fraction = total - whole;
    let whole_u8 = whole.min(u8::MAX as f32) as u8;
    self.work_progress = self.work_progress.saturating_add(whole_u8);
}
```

### 2. `src/simulation/movement.rs` — swap the two accumulation sites

Lines 236-237 and 829-830, replace:

```rust
let progress = (base * factor) as u8;
ai.work_progress = ai.work_progress.saturating_add(progress);
```

with:

```rust
ai.add_work_progress(base * factor);
```

### 3. `src/simulation/typed_task.rs` — zero the fraction at boundaries

Append `ai.work_progress_fraction = 0.0;` next to the existing `ai.work_progress = 0;` write in:

- `finish_task` (line ~862)
- `cancel_chain` (line ~871)
- `begin_working` (line ~901)

`cancel()` doesn't take `&mut ai` and its callers (internal `dispatch` / `advance` overflow) don't run on a worker that just accrued fraction — leave it asymmetric and document with a one-line comment on `cancel()`: "fraction is zeroed by `cancel_chain`/`finish_task`/`begin_working`; raw `cancel()` doesn't touch ai".

### 4. `src/simulation/test_fixture.rs` — struct-literal field

Line 13726 builds `PersonAI { … }` explicitly. Add `work_progress_fraction: 0.0,`. Other test sites use `..Default::default()` and need no change.

### 5. `src/simulation/CLAUDE.md` — terse doc update

Update the "Canonical exit helpers" line under § ActionQueue and typed Task variants:

> `aq.finish_task(&mut ai)` (success — `state = Idle` + `work_progress = 0` + `work_progress_fraction = 0.0` + advance), `aq.cancel_chain(&mut ai)` (abort — same + cancel).

Add a one-liner near the Energy / Healing coverage noting that the fractional accumulator exists because slowdown factors < 1.0 would otherwise truncate to zero progress per tick.

## Out of scope

- `+= 1` increments in `tasks.rs` (Play), `corpse.rs` (Butcher), `teaching.rs` (Read/Teach), `construction.rs` (Deconstruct bed) bypass the slowdown factor entirely. Whether they should slow down is a design question, not a bug.
- `CraftOrder.work_progress` (separate field on the order entity, `crafting.rs:703`) — different mechanism.
- Widening `work_progress` to `u16` — rejected for churn vs. value.

## Verification

Unit test on `PersonAI::add_work_progress`:

- `add_work_progress(0.4)` × 2 → `work_progress == 0`, `fraction ≈ 0.8`.
- Third `add_work_progress(0.4)` → `work_progress == 1`, `fraction ≈ 0.2`.
- `add_work_progress(300.0)` once → `work_progress == 255` (saturated), `fraction ≈ 0.0`.
- `add_work_progress(-1.0)` and `add_work_progress(0.0)` → no-op.

Behavioural regression via `test_fixture.rs`:

- Worker adjacent to a `PrepareField` target, `Energy { current: 50.0, .. }` (`energy_factor() ≈ 0.6`). `tick_n(30)` → `ai.work_progress` strictly increases (today: stays 0 forever).
- Mirror with `Sickness { severity: 200 }` (`sickness_work_factor ≈ 0.5`).

Focused existing tests:

- `cargo test --bin civgame prepare_field_completion_uses_job_board_progress_and_releases_claim`
- `cargo test --bin civgame gather_wood_goal_dispatches_gather_then_deposit_chain`
- `cargo test --bin civgame build_claimed_blueprint_goal_dispatches_construct_task`

Full suite: `cargo test --bin civgame`.

In-game smoke check (`cargo run`): find a worker with `energy < 90` or live `Sickness`; inspector `work_progress` should tick upward at a visibly slower rate instead of stalling.

## Critical files

- `src/simulation/person.rs` — new field, default, accumulator method.
- `src/simulation/movement.rs:236, 829` — two call-site swaps.
- `src/simulation/typed_task.rs:862, 871, 901` — fraction-zero at boundaries.
- `src/simulation/test_fixture.rs:13726` — struct-literal field add.
- `src/simulation/CLAUDE.md` — one-line doc update.
