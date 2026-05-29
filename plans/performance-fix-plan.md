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

## Phase 2 — DATA-DRIVEN, pending user panel reading
Run year-1 with the Performance panel open; the climbing set + counter selects the fix. Pre-identified candidates (apply only what the panel implicates, via existing cursor/stagger — never `tick % N`):
1. **Cluster scan** (`shared_knowledge.rs`) — if `know. clusters` climbs: tighten decay TTL/merge radius, round-robin the cluster walk, or spatial-bucket consumers.
2. **Loaded chunks** (`chunk_streaming.rs`) — if `loaded chunks` climbs: verify off-camera focus pruning + `UNLOAD_RADIUS`; overlaps Minimal fidelity.
3. **Gossip O(pop²)** (ParallelA) — if `ambient_social`/`awareness_gossip` µs climbs: cap candidate-pairs/tick via the existing cursor.
4. **World-sim drain** (`world_sim.rs`) — if `worldsim queue` climbs: raise `world_sim_deltas_per_tick` or bound batch submission.

## Verify
1. After Phase 0, `cargo run`, compare year-1 avg/p99 in "Sim Timing".
2. Panel open at ~20 pop — watch per-set timings + sparkline Δ columns; identify the climber.
3. After the Phase-2 fix, confirm the implicated Δ flattens and the set EMA stops climbing.
4. Toggle Balanced→Minimal with off-camera regions; focus-point + Dormant counts drop, camera settlement unaffected.
