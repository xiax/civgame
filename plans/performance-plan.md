# Comprehensive Conservative Performance Plan

## Summary
The first-plan items are still important. This plan keeps them and adds the specific systems you called out: trader stepping, opportunity rebuilds, herd threats, and debug/cohort scans. The priority remains smoother frames with minimal behavior risk: measure, spread bursts, move pure compute off-thread, and keep ECS/world mutation on the main thread.

## Core Interfaces
- Extend `PerfWorkBudget` with caps for:
  - faction planner snapshots/applies
  - trader market applies/plans
  - opportunity dirty rebuilds
  - herd repulsion builds
  - diagnostics/cohort samples
  - diplomacy/land/faction-maintenance batches
- Extend `BackgroundWorkDiagnostics` with backlog, compute-us, apply-us, stale-drop, and applied-count fields for:
  - faction job planning
  - trader planning
  - opportunity index
  - herd threat/flow rebuilds
  - debug/cohort sampling
- Add async/delta resources:
  - `FactionPlannerTaskState`
  - `TraderMarketTaskState`
  - `OpportunityDirty`
  - `HerdThreatIndex`
  - `HerdRepulsionBuildQueue`
  - `DiagnosticsSamplingState`

## Implementation Changes

### 1. Instrumentation First
- Add timing and backlog counters around the current 20/60/daily cadence systems before changing behavior.
- Surface these in the debug panel beside existing sim timing/background work diagnostics.
- Baseline in sandbox and normal starts at 1x and 5x, tracking worst tick, EMA tick time, and cadence backlogs.

### 2. Chief/Faction Planning Bursts
- Convert these from main-thread burst planners into snapshot/background/apply pipelines:
  - construction procurement classification
  - chief job posting
  - chief loose stockpile posting
- Snapshot faction storage, blueprints, projects, job board state, shared knowledge summaries, craft orders, and farm snapshots on the main thread.
- Compute `FactionPlanDelta` off-thread.
- Apply a bounded number of deltas per tick, validating faction id, posting generation, blueprint existence, project phase, and relevant storage/procurement freshness.
- Keep all `Commands`, `JobBoard`, `JobCompletedEvent`, and escrow mutation on the main thread.

### 3. Trader Market Step
- Split `trader_market_step_system` into:
  - read-only trader/settlement snapshot
  - pure arbitrage plan selection
  - bounded main-thread trade/plan apply
- Keep actual buy/sell operations on the main thread because they mutate markets, agents, inventory, and action queues.
- Replace current per-trader pair scanning with cheapest-buy/highest-sell selection for the current Cloth-only behavior.
- Cap arrivals and new plan installs per tick so one market cadence cannot monopolize Economy.

### 4. Opportunity Index
- Replace full clear/rebuild every 20 ticks with dirty partial rebuilds.
- Mark dirty on:
  - job posting added/claimed/expired/removed/reward changed
  - faction storage, material target, or resource demand change
  - `Injury` add/change/remove
- Rebuild only dirty faction/kind buckets per tick, capped by budget.
- Keep cheap expiry eviction every tick and add a daily full-audit fallback to catch missed dirty marks.
- Add a test-only full rebuild helper and assert dirty rebuild output matches it.

### 5. Herd Threat Scans
- Stop scanning a 25x25 tile square per herd member.
- Build a predator tile/index snapshot once per threat cadence from Wolf/Fox entities.
- Aggregate herd members by cluster into bounds/center/member count.
- For each active cluster, search predators against the expanded cluster bounds and pick nearest threat.
- Queue repulsion field rebuilds and build only a small budget per tick.
- Preserve existing cooldown, threat tile selection, and flow-field behavior.

### 6. Debug, Decision, and Cohort Scans
- Gate debug-only scans on `DebugPanelState.open`:
  - `sample_decision_metrics_system`
  - `rebuild_cohort_registry_system`
  - cohort demote candidate counting if enabled later
- Keep gameplay counters updated only where gameplay reads them.
- Convert `cohort_pin_full_sim_system` from full-person every-tick scan to changed/dirty processing plus periodic audit.
- Dirty triggers: `PersonAI`, `AgentGoal`, `Commanded`, `Drafted`, `FactionChief`, and existing `PinnedFullSim`.
- Preserve prompt pinning for commanded, drafted, chief, and combat entities.

### 7. Remaining Cadence Systems
- Stagger daily/quarter-day systems by faction id or stable entity id:
  - hunter/farmer/bureaucrat/crafter/architect/healer assignment
  - land listing/acquisition/rent cleanup
  - nomad pool balance and migration checks
  - diplomacy/federation/access-grant maintenance
  - tablet posting and self-actualization teaching
- For systems that must remain bursty, add per-tick apply caps and deterministic queues.
- Keep existing async systems intact: path graph/connectivity, settlement survey, world sim, and water sim already follow the right architecture.

### 8. Scan and Queue Optimizations
- Add a loose-ground-item dirty index for stockpile posting instead of rescanning every anchor disc every 60 ticks.
- Share per-tick/faction snapshots where multiple systems currently rescan members, postings, storage, or settlements.
- Replace repeated full hash-set sorting in chunk streaming/tile refresh paths with deterministic queue cursors where behavior remains stable.
- Add `par_iter_mut` only for read-heavy per-agent systems with no shared mutable resource or `Commands` conflict.

## Test Plan
- `cargo check`
- `cargo test --bin civgame`
- Equivalence tests:
  - faction job planner deltas match current posting behavior over a cadence window
  - trader buy/sell and plan selection match current Cloth arbitrage behavior
  - dirty opportunity rebuild equals full rebuild
  - herd threat detection matches old within-radius cases and cooldown expiry
  - debug-closed scans no-op, debug-open scans populate metrics
  - dirty cohort pinning adds/removes pins correctly
- Performance acceptance:
  - lower 60-tick worst spikes in debug panel
  - no growing backlog at 1x in normal play
  - bounded backlog recovery at 5x
  - no visible changes to job posting, trading, herd fleeing, or goal selection outcomes

## Assumptions
- Optimize for smoother frames first.
- Conservative risk level: no new crates and no broad AI rewrite.
- Background tasks never access ECS directly; they consume snapshots and produce declarative deltas.
- Main-thread apply remains authoritative and validates stale results before mutation.
