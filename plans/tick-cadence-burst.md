# Remove Tick-Cadence Burst Systems

## Summary
- Replace system-level `tick % N`, `run_if(... tick % N ...)`, and “run every N ticks” gates with one of three patterns: per-tick scaled updates, per-tick cursors/queues, or background async tasks with bounded main-thread apply.
- Preserve gameplay timing per item/faction/agent where needed, but spread the work so no tick multiple becomes a CPU spike.
- Add guardrails so future systems cannot reintroduce cadence bursts without an explicit reviewed exception.

## Public Interfaces And Guardrails
- Add `src/simulation/cadence.rs` with shared helpers:
  - `WorkCursor<K>` for stable round-robin processing of factions, settlements, plots, entities, markets, herds, and net clients.
  - `DueQueue<K>` / `NextDueTick` style helpers for “this item is due later” semantics without global modulo gates.
  - `RollingRate` helpers for converting daily/hourly/5-tick numeric changes into per-tick rates.
  - `BudgetedApplyQueue<T>` for async/background results that must mutate ECS on the main thread.
- Extend `PerfWorkBudget` and `BackgroundWorkDiagnostics` with budgets/backlogs for faction maintenance, settlement planning, land/jobs, animal/nomad, diplomacy, vehicle, water, and net replication queues.
- Update root `AGENTS.md` plus subsystem notes: new rule is “systems run every schedule tick; expensive work is cursor-budgeted, event-driven, or async. No new system-level tick modulo gates.”
- Add a no-new-crates source scan test, run by `cargo test --bin civgame`, that fails on new system-level cadence gates:
  - Flag `tick %`, `now %`, `clock_tick %`, `run_if(...tick...)`, and constants named `*_CADENCE`, `*_INTERVAL`, `*_PERIOD` used as system gates.
  - Allow only explicit inline `// cadence-ok:` comments for true gameplay cooldowns or protocol constants, not workload scheduling.

## Implementation Changes
- **Core migration rule:** each affected system remains scheduled every tick, then processes a bounded amount of work. If behavior used to happen once per day/hour/window, store `next_due_tick` per item or apply an equivalent per-tick rate.
- **Economy and markets:** replace `PRICE_UPDATE_INTERVAL` gates with `update_prices_scaled(window_fraction)` every tick, or a market cursor for settlement/camp markets if counts grow. Preserve the same 5-tick aggregate price movement.
- **Settlement and construction:** convert organic pressure, morphology, project selection, bridge/dam emitters, settlement replanning, chief directives, procurement classification, loose stockpile posting, workforce budgets, building upgrades, bed assignment, and door proximity into settlement/faction/entity cursors. Door proximity should become movement/event-driven where possible; otherwise process a bounded door batch every tick.
- **Faction and labor systems:** create a `FactionMaintenanceState` that walks factions every tick for hunter/bureaucrat/architect/crafter/farmer/healer assignment, cross-profession switching, hunt decisions, salaries, tribute, household contracts, worker self-posts, and esteem posts. Salary/tribute/posting timing is kept with per-faction `next_due_tick`.
- **Jobs, land, and farms:** convert escrow GC, wage EMA, chief posting, tablet posting, claim release, land listing, household acquisition, rent collection, farm plot assignment, gather-claim expiry, planting reservation GC, field/fish/tile regeneration into cursors or due queues. Monthly/daily effects remain equivalent, but processing is spread across plots, households, postings, reservations, and depleted tiles.
- **Knowledge, social, AI, and metrics:** migrate tech adoption, knowledge tier promotion, shared-cluster decay, relationship/skill/sickness/reputation decay, social pairing rescans, opportunity index rebuilds, opportunistic goal switching, chronic failure release, cohort rebuilds, and decision metrics to per-entity/per-faction cursors or per-tick scaled decay.
- **Animals, nomads, vehicles, diplomacy:** migrate animal claims, herd cluster/threat updates, wild herd migration, husbandry housing/training, following-band redirects, nomad migration/survey/chief/pool/sedentarize/collapse, vehicle assembly/recovery/proposals, diplomacy decay/expiry/AI response/proposals to bounded cursors or per-faction due queues.
- **World and background work:** keep existing budgeted chunk streaming/pathfinding patterns. For water sim, remove the fixed `WATER_SIM_CADENCE` main-thread gate; use in-flight task backpressure, active-region window cursors, elapsed-tick simulation input, and bounded apply.
- **Networking:** replace Update modulo throttles with per-tick token/bandwidth budgets and dirty/event-driven work. Entity replication uses per-entity/client rate accumulators; interest rebuilds become dirty-chunk/faction cursors; camera focus sends on chunk change with coalescing; stats reporting uses a rolling elapsed window, not modulo gating.
- **Cleanup:** delete or rename workload cadence constants after migration. Keep true gameplay cooldown constants, but document them as per-actor/item cooldowns, not system scheduling.

## Test Plan
- Add unit tests for `WorkCursor`, `DueQueue`, scaled-rate helpers, stale async result rejection, and budgeted apply queues.
- Add behavioral regression tests:
  - Market prices after 5 ticks match the old aggregate nudge.
  - Bureaucrat wages after one day match old totals.
  - Sickness, skill, relationship, reputation, fish, and tile regen match old daily/window totals within rounding rules.
  - Hunt, land rent, household acquisition, job posting, and nomad migration fire once per intended item window, but on staggered due ticks.
  - Door opening remains responsive after movement near doors.
  - Water sim continues while active and does not block the main tick.
  - Net replication respects previous effective send rates without global bursts.
- Run `cargo check` and `cargo test --bin civgame`.
- Add a stress/perf verification pass in sandbox and a larger save: diagnostics should show bounded backlogs and no recurring spikes at old multiples like 5, 20, 60, 900, or `TICKS_PER_DAY`.

## Assumptions
- The goal is to remove workload bursts, not to remove legitimate gameplay cooldowns such as attack cooldowns, path retry cooldowns, raid steal cooldowns, or per-agent timers.
- Gameplay timing should remain equivalent at aggregate scale unless a system is explicitly converted to smoother per-tick behavior.
- No new crates are added; cursor/queue utilities and the static guard use existing Rust/std tooling.
