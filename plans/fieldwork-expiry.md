# Farm FieldWork Expiry & Reconciliation

Shipped. Supersedes `farm-stale-fix.md` (in-season capacity drift) and `seasonal-jobs.md` (season-change expiry). Same teardown path now covers both.

## What ships

1. **Seed-budget-aware Plant posting** (`jobs.rs` Spring `SpringPrepPlant`).
   `seed_remaining = faction.storage.seed_total()` minus uncompleted-commit on every live Plant posting, decremented per plot as it emits. Two plots with 50+40 plantable tiles against 70 seeds yield postings totalling 70, not 90.
2. **`fieldwork_expiry_system`** (`farm.rs`, Economy, before `chief_job_posting_system` + `workforce_budget_system`). Per-posting:
   - `fieldwork_phase_seasonally_valid(phase, season, assigned)` — Spring→Prepare+Plant open; Summer→only assigned-farmer Prepare; Autumn→Harvest; Winter→none. Invalid → drop.
   - In-season capacity via `count_unprepared_in_rect` / `count_plantable_in_rect`∩seed pool / `count_mature_crop_in_rect`.
     - `capacity == 0 && completed == 0` → drop, `JobCompletedEvent{completed:false}`.
     - `capacity == 0 && completed > 0` → shrink `target = completed`, drop with `JobCompletedEvent{completed:true}` (funded postings pay partial work via `job_payout_system`).
     - `capacity < remaining_target` → in-place `target = completed + capacity`, keep claim.
   - On drop: strip `JobClaim`+`ClaimTarget`, `release_reservation`, `aq.cancel_chain` per phase (Prepare→PrepareField, Plant→WithdrawMaterial+Planter, Harvest→pre-yield Gather; deposit-tail `DepositResource` left alone).
3. **Eager dispatcher-side release** (`htn::release_farm_claim_eagerly`). All three farm dispatchers strip a Farm `JobClaim` at no-task `continue` (no seed/no plantable/no mature crop/no tech). Calls `goal_contract::blocked(NoFarmPhaseWork)`. Sub-tick recovery vs ~900-tick `chronic_failure_release_system`.
4. **Summer caretaker gate** — `chief_job_posting_system` Summer Prepare emit skips when `assigned_farmer.is_none()`.
5. **Seasonal-validity priority gate** — `compute_priority` only lifts open `FieldWork` to `SEASONAL_FARM_PRIORITY` when `fieldwork_phase_seasonally_valid` returns true.

## Verification

Tests pending (see plan). Manual: `cargo run`, observe Subsistence village across one year — Farm-claimed worker never idle > ~20 ticks; Autumn arrival cleanly reassigns Prepare/Plant claimants.

## Files

- `src/simulation/jobs.rs` — Change 1 (seed budget), Change 4 (summer gate).
- `src/simulation/farm.rs` — Change 2 (`fieldwork_expiry_system` + count helpers + `fieldwork_phase_seasonally_valid`).
- `src/simulation/htn.rs` — Change 3 (`release_farm_claim_eagerly` + call sites in all 3 farm dispatchers; plant dispatcher folds `plant_map`+`plant_reservations` into existing `PlantingDispatchParams` to stay under the 16-param ceiling).
- `src/simulation/projects.rs` — Change 5 (seasonal validity gate in `compute_priority`).
- `src/simulation/mod.rs` — schedule wiring (Economy stage, before `chief_job_posting_system` + `workforce_budget_system`).
- `src/simulation/CLAUDE.md` — Farming-section docs.
