# Fix: `Task::Sleep` orphan desync panic — structural redesign (dedicated `sleep_task_system`)

## Context

`debug_assert!` in `ActionQueue::dispatch` crashed:

```
ActionQueue::dispatch desync — pushing Sleep { bed: None } while current is
still Sleep { bed: None } and queue is empty   (typed_task.rs:652, from htn.rs:3619)
```

**Root cause.** `Task::Sleep` is the only typed task with no dedicated executor. Its
lifecycle is split: `htn_dispatch_system` plans/routes/flips state; `needs.rs::
tick_needs_system` is the *sole* retirement, only when `ai.state==Sleeping` +
bucket-active. Any system that resets `state→Idle` while leaving `aq.current==Sleep`
strands the task (no retirement path). `goal_dispatch_system`'s Sleep preserve-arm
(tasks.rs:756-757) keeps `current==Sleep` alive, so the orphan persists; next
`htn_dispatch_system` falls through all guards and re-dispatches Sleep → desync.
Concrete resetter: `combat_system` retaliation (combat.rs:437-446/473-478) Idle-resets
a sleeping victim + emits `CombatRetaliationStartedEvent`, but the block is gated by
`if target_combat.0.is_none()` (combat.rs:436/472) — skipped for 2nd+ attacker.

**Outcome.** Remove the fragility *class*: give `Task::Sleep` a real executor keyed on
the typed task; fix the combat defect at its source.

## Design

### 1. New `sleep_task_system` (`src/simulation/sleep.rs`, NEW)

Sequential executor modeled on `drink::drink_task_system`. Query: `(Entity, &mut
PersonAI, &mut ActionQueue, &mut Needs, &Transform, &BucketSlot, &LodLevel)` +
`Res<BedMap>`, `Res<SimClock>`, `Res<Time>`. Branch order:

1. `if *lod == LodLevel::Dormant { continue; }`
2. `if aq.current_task_kind() != TaskKind::Sleep as u16 { continue; }` ← source-of-truth
3. at_rest = chebyshev(cur_tile, ai.dest_tile) ≤ 1
4. arrival→Sleeping: at_rest && state ∈ {Working, Idle} → `state = Sleeping`
5. orphan self-heal: !at_rest && state ∉ {Seeking, Routing} → `aq.cancel_chain(&mut ai)`; continue
6. recovery+retire (state==Sleeping only; rate gated on `clock.is_active`, steps 4-5 NOT):
   - `dt = time.delta_secs() * clock.scale_factor()`
   - `on_bed = bed_map.0.contains_key(&(tx,ty))`
   - `needs.sleep -= SLEEP_RECOVER_RATE * (on_bed?2:1) * dt` (no con_scale; parity)
   - `needs.willpower += WILLPOWER_SLEEP_RECOVER * (on_bed?2:1) * dt`
   - `if needs.sleep < SLEEP_WAKE_THRESHOLD { aq.finish_task(&mut ai); }`

### 2. Trim `needs.rs`

- `pub` SLEEP_RECOVER_RATE (128), WILLPOWER_SLEEP_RECOVER (140); add
  `pub const SLEEP_WAKE_THRESHOLD: f32 = 10.0;`. SLEEP_RATE stays private.
- Delete recovery+finish body (185-207); keep `if state != Sleeping { sleep += ... }`.
- Leave willpower-drain skip (261-272) untouched (still correct; executor sets Sleeping).

### 3. Trim `htn_dispatch_system`

Remove guard-4 (htn.rs:3439-3442). Keep guard-3 (3430) and guard-5 (3445-3451).

### 4. combat.rs (432-456, 468-488)

Keep `target_combat.0=Some(attacker)` + non-sleeper Idle+event inside `is_none()`.
Add mutually-exclusive branch: Person victim damaged && `state==Sleeping` → `state=Idle`
+ send `CombatRetaliationStartedEvent` regardless of `target_combat.0` (idempotent).
`combat_retaliation_cleanup_system` unchanged.

### 5. Scheduling (mod.rs ~642, Sequential)

`sleep::sleep_task_system` `.after(movement_system)`,
`.after(combat_retaliation_cleanup_system)`, `.before(cosleep_observation_system)`.

## `AiState::Sleeping` readers preserved (executor still sets Sleeping at rest)

reproduction.rs:172,195,317-318; needs.rs:261; goals.rs:756; movement.rs:698,808;
ui/inspector.rs:1028. No rendering reader.

## Files

sleep.rs (NEW), mod.rs, needs.rs, htn.rs, combat.rs, src/simulation/CLAUDE.md.

## Tests (`cargo test --bin civgame`)

1. `sleep_orphan_state_reset_recovers_without_panic` — tired agent in player faction
   at (20,20), tick_n(60) → Sleep+Seeking; force state=Idle (keep current==Sleep);
   tick_n(5) → no panic, re-coheres, queued_len stayed 0, reaches Sleeping.
2. `sleep_normal_flow_unchanged` — at-home tired agent → Sleeping → drains → retires.
3. `sleeping_agent_still_cosleeps` — CoSleepTracker.bond_strength still accrues.
4. (if feasible) multi-attacker sleeper → chain cancelled.
5. Regression: existing `sleep_goal_dispatches_typed_sleep_task` + combat tests.

## Docs (`src/simulation/CLAUDE.md` only)

Sleep/Needs: dedicated `sleep_task_system` owns lifecycle; tick_needs only the
increase. ActionQueue Rules/Consumers: add sleep_task_system + cancel_chain idiom.
External preempts: combat cancels Sleeping victim every hit.

## Verification

`cargo test --bin civgame` green; `cargo run` (combat on sleeper, no panic);
`cargo check` clean; CLAUDE.md updated.

## Status — shipped

All 737 tests pass (incl. new `sleep_orphan_state_reset_recovers_without_panic`,
`sleep_executor_drives_recovery_and_sleeping_state`, and the existing
`cosleep_*`/`sleep_goal_dispatches_typed_sleep_task` regressions). Refinement vs.
plan: the htn guard is a single `current==Sleep → return` (subsumes old guards
3/4/5) — `htn_dispatch_system` (ParallelB) runs before `sleep_task_system`
(Sequential), so the dispatcher must never act on a live Sleep task; the executor
owns reconciliation. CLAUDE.md updated.
