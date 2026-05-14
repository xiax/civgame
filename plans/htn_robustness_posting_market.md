# HTN Robustness via Closing the Posting-Bypass

## Context

Two visible symptoms drove this: (1) 4+ workers swarm one resource tile — first harvests, others arrive empty and idle; (2) workers stand in a wheat field with `AcquireFood` active because the planner can't see the wheat under their feet.

Investigation showed these aren't bugs — they're **design gaps in the boundary between coordinated and autonomous work**. Critically, the codebase already has the right architecture for diverse economic models (Subsistence/Mixed/Market with free-agent workers):

- `JobPosting.poster_class: { Chief | Bureaucrat | HouseholdHead | Individual }` and `reward: f32` already exist.
- Households self-post paid craft contracts (`post_craft_contract_from_treasury`); agents self-post Torch contracts (`esteem_driven_posting_system`).
- Worker scoring already includes wages: paid postings score `U_bid = E(R) − C_action − C_opportunity` (`jobs.rs:2121–2140`).
- Per-resource policy gates exist: `ResourceControlPolicy { chief_allocates_labor, private_actors_allowed }`.
- `Farm`/`Haul`/`Stockpile{non-Wood/Stone}` already require a `JobClaim` — they go through the posting market.

**The brittleness comes from a small but load-bearing bypass:** when a worker has no `JobClaim`, `goal_update_system` falls back to autonomous `GatherFood`/`GatherWood`/`GatherStone`/`Craft`/`Build` (goals.rs:554–622). These bypass the posting layer entirely — no exclusive claim, no slot quota, no wage signal, no per-economy policy gate. Multiple workers run independent HTN against the same shared world state, hence the swarming.

The personal-needs path (`Survive`/`Sleep`/`Defend`) should stay autonomous in all economy modes — a hungry free agent grabbing the nearest berry isn't coordinated work. But that path also has its own brittleness: it consults `CurrentVision` (cleared per ~20-tick bucket) → `SharedKnowledge` clusters; never inspects the live `PlantMap` at the agent's tile.

## Strategy

Two parallel tracks:

**Track A — Personal-needs reflex fixes** (small, low-risk, ships fast). Make `Survive`-driven foraging robust without touching coordination architecture.

**Track B — Close the posting bypass** (medium, architectural). Route productive-work fallback through the existing posting market with self-posts gated by `ResourceControlPolicy`. Honors all three economy modes by construction.

Track A ships first; Track B builds on it.

## Track A — Personal-needs reflex (Phases 1–3) — shipped

- Phase 1: `plants::nearest_mature_plant_under_agent` wired into food/good dispatchers.
- Phase 2: `gather::retarget_neighbor` (chebyshev≤2, 40-tick cooldown) swaps cluster claim and rewrites `aq.current` on stale arrival.
- Phase 3: `ResourceCluster::pick_least_pressured_rep(from, penalty)` plumbed into `KnowledgeMap::nearest` and `nearest_in_tier_set`.

## Track B — Close the posting bypass (Phases 4–7)

### Phase 4 — Make `GatherClaims` cluster-aware exclusive — partially shipped
Per-tile `GatherClaim` remains; cluster-saturation skip (`cluster_is_saturated`, `MAX_PARALLEL_GATHERERS_PER_CLUSTER = 3`) and pressure-aware rep selection ship. `try_claim_cluster_slot` strict-mutex semantics still deferred.

### Phase 5 — Self-posting for autonomous productive fallback — shipped
`post_stockpile_self`, `worker_self_post_stockpile_system` (every `TICKS_PER_DAY`), `SELF_POST_MARGIN = 0.1`. Policy gates honored (Subsistence skips, Mixed/Market participate).

### Phase 6 — Precondition re-validation at task execution — deferred
Before doing work on the first tick after walk-arrival, re-check the load-bearing precondition. If invalid: push `MethodOutcome::FailedTarget`, `aq.cancel()`, release cluster slot.
- `src/simulation/gather.rs` — extend stage check to a generic "is the resource still here?" guard.
- `src/simulation/production.rs` — `withdraw_material_task_system` re-checks `FactionStorage.totals` minus other-agent reservations.
- `src/simulation/construction.rs` — `HaulToBlueprint` arrival re-checks `bp.slot_satisfied(rid)`.
- Test: `gather::tests::stale_target_aborts_on_arrival`, `production::tests::storage_emptied_between_dispatch_and_arrival_aborts`.

### Phase 7 — Posting slot enforcement at claim time + cluster-slot clamp — deferred
Hard skip claims when `posting.claimants.len() >= posting_target_workers(p)`. For Stockpile postings, clamp `target_workers` by `SharedKnowledge::cluster_slot_estimate(faction, kind, near, radius)` (floor 1).
- `src/simulation/jobs.rs:2073` (claim path) and `posting_target_workers` callers near 920–975.
- Test: `jobs::tests::posting_caps_enforce_at_claim`, `jobs::tests::cluster_slot_clamp_prevents_overstaffing`.

## Track C — Plumbing (Phases 8–9, optional/deferred)

### Phase 8 — Dynamic `ActionQueue`
Replace `[Task; 4]` ring with `Vec<Task>` (capacity 4 in `ActionQueue::idle`). `enqueue` becomes infallible. Audit ~30 caller sites.
- `src/simulation/typed_task.rs:456–600` and callers.
- `debug_assert!(self.queued.len() <= 8)` to catch runaway chains.

### Phase 9 — Auto-armed stale-reset + fallback methods for single-method goals
Replace the manual allowlist (`tasks.rs:621–870`) with derivation from `MethodRegistry`. Add low-utility (0.05) fallbacks for orphaned goals (`Sleep` → `SleepInPlaceMethod`, `ReturnSurplus` → `DropAtFeetMethod`, `EquipHuntingSpear` → `WaitMethod`).
- `src/simulation/htn.rs` — methods declare `produces_task_kinds() -> &'static [TaskKind]`.
- Behind `const USE_DERIVED_ARMS: bool = true` for one release as kill-switch.

## Cross-cutting

- **No new crates.** Use `Vec::with_capacity(4)` over `SmallVec`.
- **All three economy modes must work.** Verify each phase with Subsistence + Mixed + Market spawn presets. Subsistence behavior must be invariant under Track B (chief still posts; no self-posting fallback fires). Market behavior should show worker-driven self-posts.
- **CLAUDE.md updates per phase** — top-level for 4/5/8/9, `src/simulation/CLAUDE.md` for 1–3 + 6–7.
- **Test command:** `cargo test --bin civgame`. 411-test suite is the regression invariant.
- **Manual verification:** `cargo run`, with each economy preset:
  - Phase 1: drop founder onto grain field, observe immediate harvest.
  - Phases 4+5 in Market: 6 idle workers gather wood; expect distinct cluster reps + self-posts visible in inspector.
  - Phase 5 in Subsistence: 6 idle workers; expect them to wait for chief reposts (no self-post lines).

## Critical files

- `src/simulation/htn.rs` — dispatcher context build, method registry
- `src/simulation/goals.rs:554–622` — autonomous fallback (the bypass we're closing)
- `src/simulation/gather.rs` — gather_system arrival branch
- `src/simulation/gather_claims.rs` — claim mutex (becoming cluster-slot)
- `src/simulation/shared_knowledge.rs` — cluster rep selection, slot estimate
- `src/simulation/jobs.rs` — posting market (chief + self-post extension)
- `src/economy/policy.rs` — `ResourceControlPolicy`, `LandPolicy` (gates for self-posting)
- `src/simulation/typed_task.rs` — ActionQueue
- `src/simulation/tasks.rs` — goal_dispatch_system stale-reset

## Verification

After each phase: `cargo check && cargo test --bin civgame`. After Phases 1, 3, 5, 7: run `cargo run` for ~5 in-game days under each economy preset. Inspector hover on idle agents should show: goal, method, last `MethodHistory` outcome, posting authorship (Chief/HouseholdHead/Individual), wage. The plan succeeds when:
- No worker stands in a wheat field with active `AcquireFood`/`Survive` (Track A).
- In Market mode, idle workers visibly self-post Stockpile jobs and the posting board shows author entities (Track B).
- In Subsistence mode, the 411-test suite is bit-for-bit identical and behavior unchanged (regression invariant).
