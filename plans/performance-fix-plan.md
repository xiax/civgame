# Early-Game Performance Fix Plan (revised)

## Problem
Year-1, ~20-pop slowdown in `cargo run` (debug); tick time **climbs over time**.

## Diagnosis correction
The original five hot-path targets (vision / faction storage / loose-stockpile / survey / goal snapshots) were **already optimized in commit 6d4f727** (round-robin cursors, per-faction stagger, change-detection gating, async surveys). A growth audit found all major accumulators (ground items, postings, blueprints, chunk graph, clusters, path logs, agent memory) **bounded**. So: not a leak. "Climbs over time" + everything-bounded ⇒ explored-area-correlated cost (resource clusters, loaded chunks, gossip) that plateaus. Decision: **measure-first**.

## Shipped
- **Phase 0 — build profile.** `Cargo.toml`: `[profile.dev.package."*"] opt-level = 3` (deps optimized in debug; our crate stays opt-level 1). Highest-leverage debug-build fix.
- **Phase 1a — per-set timing.** `SetTimingDiagnostics` (EMA/worst/p99 per `SimulationSet`) via boundary systems in `speed.rs` + `mod.rs`. Answers "which set climbs?".
- **Phase 1b — suspect attribution.** `SuspectSystemTimings` (atomic, parallel-safe `Res` + `Drop` guard) on `cluster_decay` / `ambient_social_pairing` / `awareness_gossip`; folded into `SetTimingDiagnostics.system_us`. `goal_update` omitted (param ceiling → read ParallelA/B totals); `world_sim` reuses `world_sim_compute_us`.
- **Phase 1c — Performance panel.** `render_performance_section` + `PerfPanelParams` in `debug_panel.rs`: per-set timings, suspect µs, and `PerfHistory` growth sparklines (ground items, postings, blueprints, loaded chunks, focus points, clusters, path failures, world-sim queue) with a Δ "climb" column. Sampler `perf::perf_history_sample_system` (panel-open-gated, 600-tick cadence).
- **Phase 3 — offscreen fidelity.** `perf::PerformanceSettings { OffscreenFidelity::{AllLive, Balanced(default), Minimal} }` wired into `update_simulation_focus_system` (off-camera region focus radius: base / base÷2 / none) and `update_lod_levels_system` (promote radius 8/4/0). Combo selector in the panel. Camera region always full.
- Tests: `perf::tests` (series ring/delta/cap + fidelity ladders), `speed::tests` (per-set records, fold_sample). Suite green.

## Phase 2 — Sequential attribution + animal-A* de-spike (shipped)
Panel reading at ~20 pop localised the cost to **`SimulationSet::Sequential` = 65 ms of a 74 ms tick** (everything else < 8 ms). The climbing `know. clusters` was a red herring — its 7 dispatchers live in ParallelB (0.94 ms). Sequential had no per-system attribution (the 3 suspects were ParallelA/Economy). Fix (`plans/i-ran-the-performance-vast-dream.md`):
- **2a — Sequential attribution.** Extended `speed.rs::suspect` with `vision` / `animal_movement` / `movement` / `gather` / `combat`; one-line `SuspectSystemTimings::guard` per system. `gather`/`combat` were at the 16-param ceiling → folded the `Res` into their existing `GatherRoutingResources` / `CombatEventWriters` bundles. Fold loop + panel already generic.
- **2b — `animal_movement_system` de-spike.** Animals ran inline, un-budgeted 256-node A* every replan (vs persons' 64/tick queue), and `BudgetExhausted` partials made out-of-range goals re-A* every tick. Added `PerfWorkBudget::animal_replans_per_tick = 64` + `AnimalReplanCursor` (round-robin, `select_replan_slice` pure-fn unit-tested) gating only the A* call, and `AnimalAI.replan_cooldown_until` (`ANIMAL_REPLAN_COOLDOWN_TICKS = 20`) throttling fruitless searches. HERD flow-field replans stay free.

Still data-driven: re-run year-1 with the panel **at 1×** to read the new per-system rows, confirm `animal_movement` dropped, and tune the two constants. If `vision` then dominates, its lever is the `report_sighting` cluster-write cost (already capped at 32 recomputes) — a separate pass. Other pre-identified candidates if a different row climbs (apply via cursor/stagger — never `tick % N`): cluster scan (`shared_knowledge.rs`), loaded chunks (`chunk_streaming.rs`), gossip O(pop²) cap, world-sim drain budget.

## Verify
1. After Phase 0, `cargo run`, compare year-1 avg/p99 in "Sim Timing".
2. Panel open at ~20 pop — watch per-set timings + sparkline Δ columns; identify the climber.
3. After the Phase-2 fix, confirm the implicated Δ flattens and the set EMA stops climbing.
4. Toggle Balanced→Minimal with off-camera regions; focus-point + Dormant counts drop, camera settlement unaffected.
