# Seasonal Farming Priority Fix

**Status: implemented.** Season-aware `farm_pressure`, `SEASONAL_FARM_PRIORITY`
boost, phase-weighted claim floor, phase-gated dispatch (`claimed_fieldwork_phase`),
`record_fieldwork_progress` (incl. Autumn harvest hook in `gather_system`), and
the budget farm-backlog gate all landed. Constants rehomed to `farm.rs`. 956
tests pass.

## Context
In-season farming competes as just another low-share communal job. `farm_pressure`
(`projects.rs:333`) targets a trivial `members × 4` grain stockpile, so it reads 0 as
soon as a village holds a token grain stock — even though the *annual* need is
~`members × 48`. With the default 10% Farm workforce share, Spring prep/planting and
Autumn harvest get starved by chief Build/Haul/Craft postings.

Goal: make in-season farming a true food-security priority that outranks normal chief
construction/hauling/crafting, while still yielding to player orders and acute
survival food-gathering.

Two gaps beyond priority:
- **Phase-blind progress.** A `Prepare`-phase `FieldWork` claim can be picked up by
  `htn_plant_from_storage_dispatch_system` (it resolves scope from `JobClaim::Farm` +
  `plot_id`, never checks `phase`). Planting then credits a Prepare posting; Autumn
  `Harvest` postings are never credited at all (harvest runs through `gather_system`,
  which has no `FieldWork` hook).
- **Budget skew.** `farm_pressure` feeds both `compute_priority` and
  `compute_workforce_budget` (`projects.rs:567`). An unshaped annual pressure reads
  ~100 year-round, so Winter — zero farm postings — would still divert budget share.
  Season-awareness fixes Winter, but a second window remains: mid-Spring *after* all
  tiles are prepped/planted, grain stock is still low so the budget keeps reserving a
  large Farm slot even though every `FieldWork` posting has completed and despawned.
  The budget-facing farm input must reflect outstanding *queued work*, not stock.

## Design decisions
- **Season-aware `farm_pressure`** — one signal, takes the `Calendar`: 0 in
  `WinterDormant`, full annual deficit in `SpringPrepPlant`/`AutumnHarvest`, reduced in
  `SummerMaintenance`. Fixes budget skew + Plow-priority inflation, no second path.
- **Phase-weighted seasonal claim share** — open seasonal `FieldWork` postings get an
  effective Farm-cap floor scaled by phase: heavier in Spring, lighter in Autumn.

## Constants
Rehome annual-planning constants from `organic_settlement.rs` into `farm.rs` (single
source of truth), re-export at old paths so `parcel_targets` doesn't churn:
`GRAIN_PER_PERSON_PER_YEAR=48`, `GRAIN_YIELD_PER_TILE_PLANNING=4`,
`SUPPLY_SAFETY_NUMER=5`, `SUPPLY_SAFETY_DENOM=4`.

New tunables in `farm.rs`: `SEASONAL_FARM_CLAIM_SHARE_SPRING=0.65`,
`SEASONAL_FARM_CLAIM_SHARE_AUTUMN=0.45`, `SUMMER_FARM_PRESSURE_SCALE=0.35`.
New in `projects.rs`/`jobs.rs`: `SEASONAL_FARM_PRIORITY` (set so seasonal farm
outranks Haul 90+/Build/Craft, capped `< PRIORITY_PLAYER`).

## Changes

### 1. Annual season-aware `farm_pressure` (`projects.rs`)
New signature `farm_pressure(faction, calendar) -> u8`. Keep the
`faction_can_perform(Farm)` early-out. Annual target =
`ceil(members × GRAIN_PER_PERSON_PER_YEAR × SUPPLY_SAFETY_NUMER / SUPPLY_SAFETY_DENOM)`
(shared helper with `parcel_targets`' `food_tiles`). Base = `deficit_ratio × 100`.
Season shape via `farm::farm_season_phase`: Winter→0, Summer→`base ×
SUMMER_FARM_PRESSURE_SCALE`, Spring/Autumn→`base`.
Thread `&Calendar` into `compute_priority` (Farm + Plow arms) and
`compute_workforce_budget` (`workforce_budget_system` adds `Res<Calendar>`). Plow rides
the same curve for free; Farm budget slot zeroes in Winter.

### 2. Seasonal hard-priority for open `FieldWork` (`jobs.rs` chief posting)
In `chief_job_posting_system` Farm branch, for **open** seasonal `FieldWork`
(`assigned_farmer == None`, phase Prepare/Plant/Harvest): set
`priority = min(PRIORITY_PLAYER - 1, SEASONAL_FARM_PRIORITY)`.
**Acute-food override:** when `food_pressure(faction) >= CRITICAL_FOOD_TRIGGER (80)`,
no boost — fall back to normal `compute_priority` so emergency `Stockpile` wins.
Summer caretaker Prepare (`assigned_farmer = Some`) and Plow are not boosted.

### 3. Phase-weighted seasonal claim floor (`jobs.rs::job_claim_system`)
After `cap = (share × member_count).max(1)` (`jobs.rs:3861-3874`): for an open
seasonal `FieldWork` posting AND `food_pressure < CRITICAL_FOOD_TRIGGER`, raise to
`cap.max(ceil(season_share × member_count))` — `SEASONAL_FARM_CLAIM_SHARE_SPRING` in
Spring, `_AUTUMN` in Autumn, unchanged in Summer/Winter. `cap_bucket`/`bucket_share`
and the Plow/assigned-farmer gates untouched. Per-posting `posting_target_workers(p)`
cap still spreads the share across multiple postings. Add `Res<Calendar>` to the system.

### 4. Phase-correct Farm dispatch (`htn.rs`)
Each farm dispatcher honours the claimed posting's `phase`:
`htn_prepare_field_dispatch_system` acts only on `phase: Prepare`,
`htn_plant_from_storage_dispatch_system` only on `Plant`,
`htn_harvest_plant_dispatch_system` only on `Harvest`. Add an accessor returning the
claimed posting's `FarmWorkPhase` (read posting via `JobBoard`). Private/Bootstrap
`FarmScope` (no posting) keeps current autonomous behaviour, seasonally gated by
`FarmWorkScorer`.

### 5. Phase-aware progress recording (`jobs.rs` + `gather.rs`)
Add `jobs::record_fieldwork_progress(board, faction_id, job_id, phase, delta)` —
increments a `FieldWork` posting only when its `phase` matches, then fires normal
completion/`JobCompletedEvent`.
- `farm::prepare_field_task_system` → helper with `Prepare`.
- Planting completion → helper with `Plant`.
- `gather_system` Grain/harvest branch (no `FieldWork` hook today) → when the
  harvester holds a `JobClaim::Farm` on a Harvest posting, helper with `Harvest`,
  `delta = plants reaped`, so Autumn harvest postings progress and auto-release. Use
  `ResMut<JobBoard>` if aliasing allows, else a deferred event folded in Economy phase.

### 6. Budget-layer farm-backlog gate (`projects.rs`)
The priority/claim layers (changes 2–3) self-limit cleanly — `FieldWork` postings
carry a concrete `target`, auto-complete at `completed >= target`, and despawn;
`chief_job_posting_system` guards `if unprepared > 0` / `if plant_target > 0`
(`jobs.rs:3045,3059`) so no zero-target postings emit; the boost + cap floor are
per-posting, so no open posting ⇒ no boost, no elevated cap. The budget layer is the
one place that does **not** self-limit — it keys the Farm slot on `farm_pressure`
(grain stock), not on whether farm work remains.
- `workforce_budget_system` (`projects.rs:875`) gains `Res<JobBoard>`; per faction
  compute `farm_backlog: bool` = any non-complete `JobProgress::FieldWork` **or**
  `JobProgress::Plow` posting (open or assigned).
- `compute_workforce_budget` gains a `farm_backlog: bool` param. When `false`,
  `raw_farm = 0.0` AND treat `chief_will_post_for_slot` slot 4 as `false` (the Farm
  slot collapses past `SHARE_FLOOR`, rerouted to `free`, like a policy-disabled slot).
  When `true`, `raw_farm = farm_pressure(faction, calendar)`.

Effect: the Farm budget slot deflates when the season's queued work is worked-out, not
when grain is finally harvested. Winter collapses via the same gate. `farm_pressure`
itself stays stock-based (correct for posting *priority*); only the *budget*
consumption is backlog-gated. EMA + `BUDGET_RECOMPUTE_INTERVAL` absorb the one-tick
posting/recompute lag — no new ordering constraint.

## Public interfaces
Rehomed `pub` constants in `farm.rs`, re-exported from `organic_settlement.rs`.
`farm_pressure` / `compute_priority` gain a `&Calendar` parameter;
`compute_workforce_budget` gains `&Calendar` + `farm_backlog: bool`;
`workforce_budget_system` gains a `Res<JobBoard>` param. New
`jobs::record_fieldwork_progress`. No new crates, no schema migration, no new ECS
component.

## Critical files
`src/simulation/projects.rs` (farm_pressure, compute_priority, compute_workforce_budget,
constant re-export), `farm.rs` (constants, prepare progress routing), `jobs.rs`
(chief posting boost, claim floor, helper), `htn.rs` (dispatcher phase gates),
`gather.rs` (harvest progress wiring), `organic_settlement.rs` (`parcel_targets` uses
rehomed constants).

## Tests (`cargo test --bin civgame`)
- Annual pressure: 0 in Winter, ~100 at zero grain Spring/Autumn, attenuated Summer,
  0 when grain ≥ annual target.
- Seasonal priority: open Spring/Autumn `FieldWork` priority exceeds chief
  Haul/Build/Craft, stays `< PRIORITY_PLAYER`; with `food_pressure ≥ 80` the boost is
  suppressed and emergency `Stockpile` wins.
- Job-claim share: with open Build/Haul/Craft jobs, Spring farm backlog attracts ~65%
  of members (~45% Autumn); player posting still wins; Summer caretaker + Plow stay on
  the normal Farm budget.
- Phase-correct dispatch: Prepare claim can't plant/harvest; Plant claim can't satisfy
  Prepare; Harvest claim increments + auto-completes on mature-crop harvest.
- Budget backlog gate: `compute_workforce_budget` with `farm_backlog = false`
  collapses the Farm slot (≈0, rerouted to `free`) even at high `farm_pressure` —
  covers Winter + the mid-Spring post-planting window; `true` scales Farm with the
  season-aware pressure and other slots are not starved.
- Targeted farm/job tests first, then the full binary suite if practical.

## Verification (manual)
`cargo run`, observe a Subsistence village across a year: Spring pulls most idle
workers onto prep/planting (tiles flipping to Cropland), Autumn onto harvest, Winter
shows no farm activity + normal Build/Craft cadence. Player orders still preempt; an
artificially food-starved village prioritises foraging over farming.

## Docs
Update `src/simulation/CLAUDE.md` Farming section: season-aware annual `farm_pressure`,
seasonal hard-priority for open `FieldWork`, phase-weighted claim floor, phase-correct
dispatch + `record_fieldwork_progress`, budget-layer farm-backlog gate. Root `CLAUDE.md`
only if `SimulationSet` ordering changes (it should not). No `AGENTS.md` in this repo.
