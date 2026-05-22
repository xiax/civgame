# Seasonal Farm Job Expiry

## Summary
Fix stale farm labor so workers stop preparing/planting fields once that work is out of season. Keep summer as low-priority next-year prep, but cancel all remaining Prepare/Plant fieldwork when autumn starts so workers can switch to harvest, food, or other jobs.

## Key Changes
- Add a small seasonal validity helper for `FieldWork` postings:
  - Spring: open `Prepare` and `Plant` are valid.
  - Summer: only assigned-farmer `Prepare` is valid.
  - Autumn: open `Harvest` is valid.
  - Winter: no `FieldWork` is valid.
- Add a `seasonal_fieldwork_expiry_system` that removes invalid chief `Farm` postings, releases `JobClaim` / `ClaimTarget`, emits `JobCompletedEvent { completed: false }`, and cancels active stale chains:
  - `Prepare` cancels `TaskKind::PrepareField`.
  - `Plant` releases storage reservations and cancels `WithdrawMaterial` / `Planter`.
  - `Harvest` cancels only pre-yield `Gather`; deposit tails carrying food are left alone.
- Run the expiry system in `SimulationSet::Economy` before workforce budgeting and before `chief_job_posting_system`, so stale farm backlog cannot keep the farm budget alive into autumn.
- Change summer posting so caretaker prep is emitted only when `FarmPlotAssignments` has an assigned farmer; no open summer prepare jobs.
- Tighten seasonal farm priority so the hard priority boost only applies to valid spring rush work or autumn harvest work.
- Update `src/simulation/CLAUDE.md` farming notes to document seasonal expiry and summer caretaker-only prep.

## Test Plan
- Add a helper/unit test for the seasonal validity table.
- Add a regression test where an autumn calendar expires an unfinished `FieldWork{Prepare}` posting, removes the worker claim, and cancels an active `PrepareField` task.
- Add a regression test that summer does not emit an open `Prepare` posting when no caretaker farmer is assigned.
- Add/adjust an integration test showing autumn chief posting leaves no stale `Prepare`/`Plant` jobs and can post `Harvest` for mature grain.
- Run `cargo test --bin civgame`.

## Assumptions
- Summer prep remains intentional next-year prep, but only for an assigned caretaker farmer.
- Grain planting remains spring-only because summer-planted grain cannot mature before winter mortality under the current growth model.
- This fix targets communal chief `FieldWork` postings; private/autonomous farm scoring continues to use the existing seasonal gates.
