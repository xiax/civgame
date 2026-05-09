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

## Track A — Personal-needs reflex (Phases 1–3)

### Phase 1 — Live `PlantMap` fast path in food/good dispatchers
Before consulting vision/SharedKnowledge, probe `PlantMap` for a mature plant of the right kind within chebyshev radius 2. If hit and unclaimed, dispatch directly.
- `src/simulation/plants.rs` — add `nearest_mature_plant_under_agent(plant_map, plant_q, kind_filter, from, radius)`.
- `src/simulation/htn.rs:3624–3650, 4689–4723` (and StockpileFood / HarvestGrainForCraftOrder branches) — call probe before `current_vision.nearest_gather_target`.
- Test: `htn::tests::agent_on_wheat_tile_dispatches_gather` via `TestSim`.

### Phase 2 — Neighbor-scan fallback on empty arrival
When `gather_system` finds the target plant despawned/immature, scan chebyshev≤2 for a same-kind replacement before `finish_gather`. Atomically swap claim, rewrite `aq.current = Task::Gather { new_tile }`. One re-target per chain via `ai.last_retarget_tick` cooldown (40 ticks).
- `src/simulation/gather.rs:343–388` and the "completed stone tile" / "invalid target" exits.
- Test: `gather::tests::empty_arrival_retargets_adjacent_grain`.

### Phase 3 — Cluster-rep spread via claim pressure at pick time
For the personal `Survive` foraging path, use the LRU 4-rep slots already on `ResourceCluster`. Pick the least-pressured rep, not the closest. (Productive forage paths land in Track B.)
- `src/simulation/shared_knowledge.rs` — add `ResourceCluster::pick_least_pressured_rep(from, claim_penalty)` and `KnowledgeMap::nearest_with_pressure`.
- `src/simulation/htn.rs:3624–3650` (AcquireFood) — pass `|t| gather_claims.pressure(t, now, actor) * 4`.

## Track B — Close the posting bypass (Phases 4–7)

### Phase 4 — Make `GatherClaims` cluster-aware exclusive
Make claims a real mutex *at the cluster-slot level*, not per-tile. Loser's dispatch backs out cleanly; cluster scoring filters out fully-claimed clusters.
- `src/simulation/gather_claims.rs` — add `try_claim_cluster_slot(cluster_id, claimant, expires) -> bool`. Each cluster holds slot count = `cluster.estimated_count.min(MAX_PARALLEL_GATHERERS)`.
- Existing per-tile `GatherClaim` collapses to a derived view; `pressure()` becomes "how many slots taken in this cluster."
- Update all dispatchers' claim-stake sites (htn.rs:4202–4208 + sisters) to use cluster-slot semantics.
- Test: `gather_claims::tests::cluster_slot_exclusion`.

### Phase 5 — Self-posting for autonomous productive fallback
When `goal_update_system` would fall through to autonomous `GatherWood`/`GatherStone`/`GatherFood` (for storage, not personal hunger), instead **scan the faction posting board** for a fitting Stockpile posting. If none exists *and* the faction's `ResourceControlPolicy` allows private posters (`private_actors_allowed=true` for that resource), self-post one at default wage from the agent's own currency (or a chief-treasury-funded posting if `chief_allocates_labor=true` and no chief posting exists yet). Then claim normally.
- `src/simulation/goals.rs:554–622` — replace direct goal assignment with a `try_acquire_or_post_stockpile_job(agent, faction, resource_id, policy)` helper.
- `src/simulation/jobs.rs` — extend `post_craft_contract` style API to `post_stockpile_self(faction_id, resource_id, target_qty, reward, author, poster_class)`. Default wage formula: `reward = market_price(resource_id) * target_qty * SELF_POST_MARGIN` (margin small, e.g. 0.1) so self-posters don't undercut market.
- Subsistence factions (`chief_allocates_labor=true, private_actors_allowed=false`): no self-posting, no autonomous fallback — workers wait for chief postings or run personal-needs only. (Fine: chief reposts every `TICKS_PER_DAY`.)
- Mixed factions: chief posts staples; private actors self-post non-staples.
- Market factions: any worker can self-post any resource; chief posts almost nothing.
- Test: `jobs::tests::market_faction_worker_self_posts_stockpile_when_idle`, `goals::tests::subsistence_faction_no_autonomous_gather`.

### Phase 6 — Precondition re-validation at task execution
Before doing work on the first tick after walk-arrival, re-check the load-bearing precondition. If invalid: push `MethodOutcome::FailedTarget`, `aq.cancel()`, release cluster slot.
- `src/simulation/gather.rs` — extend stage check to a generic "is the resource still here?" guard.
- `src/simulation/production.rs` — `withdraw_material_task_system` re-checks `FactionStorage.totals` minus other-agent reservations.
- `src/simulation/construction.rs` — `HaulToBlueprint` arrival re-checks `bp.slot_satisfied(rid)`.
- Test: `gather::tests::stale_target_aborts_on_arrival`, `production::tests::storage_emptied_between_dispatch_and_arrival_aborts`.

### Phase 7 — Posting slot enforcement at claim time + cluster-slot clamp
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
