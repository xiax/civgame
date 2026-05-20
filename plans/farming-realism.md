# Seasonal Farming And Field Realism

## Summary
- Make farming seasonal and communal without permanently reassigning everyone: Spring creates high-priority field prep/planting work, Autumn creates high-priority harvest work, Summer is light maintenance, Winter is dormant.
- Change Agricultural plot carving from “instant cropland” to “reserved field land”; workers must prepare tiles before grain can be planted.
- Add a real fertility cycle: harvest depletes soil, fallow recovers it, exhausted tiles cannot be planted until rested.
- Reduce farm over-zoning by sizing Agricultural parcels from food need, seed/active-crop availability, and seasonal labor capacity.

## Public Interfaces And Types
- Add farm state in `src/simulation/farm.rs` or `src/simulation/land.rs`:
  - `FarmSeasonPhase { SpringPrepPlant, SummerMaintenance, AutumnHarvest, WinterDormant }`
  - `FieldUse { Unprepared, Prepared, Planted { year }, Stubble { year }, Fallow { since_year }, Exhausted }`
  - `FieldTileState { plot_id, use_state, nutrients: u8, last_worked_year }`
  - `FieldTileIndex: AHashMap<(i32, i32), FieldTileState>`
- Replace farm job progress with a phase-aware shape:
  - `JobProgress::FieldWork { phase: FarmWorkPhase, completed, target, area, plot_id, assigned_farmer }`
  - `FarmWorkPhase { Prepare, Plant, Harvest }`
  - Migrate existing `Planting` matches to `FieldWork`, preserving `plot_id` and `assigned_farmer` semantics.
- Add `TaskKind::PrepareField` / `Task::PrepareField { tile }`; executor spends `FIELD_PREP_WORK_TICKS = 80`, writes `TileKind::Cropland`, updates `FieldTileState`, grants Farming XP, and emits `TileChangedEvent`.
- Keep `JobKind::Farm` as the public job category so existing claim, wage, goal, and UI paths still route through `AgentGoal::Farm`.

## Implementation Changes
- Field creation:
  - `carve_plots_system` should still create `Plot` entities and `PlotIndex.ag_tiles`, but it should initialize `FieldTileIndex` as `Unprepared` and stop stamping Cropland immediately.
  - `seed_starting_farms_system` should reserve one starting plot and seed grain, but only pre-prepare a small bootstrap area if needed for playability; the rest requires Spring prep.
  - Existing prepared/planted field state should survive settlement replans for tiles that remain inside an Agricultural plot.
- Seasonal job posting:
  - Spring posts open `Prepare` jobs for unprepared/fallow plantable tiles, then `Plant` jobs for prepared non-exhausted tiles while grain seeds exist.
  - Autumn posts open `Harvest` jobs for mature grain inside plot rects.
  - Winter posts no planting/prep jobs; Summer posts only low-priority maintenance/fallow recovery if needed.
  - Seasonal farm postings should have `assigned_farmer: None` so non-farmers can help during peak windows; the one-to-one Farmer assignment remains useful for non-burst maintenance.
  - `posting_target_workers` for Farm should scale with target, e.g. `min(12, max(3, target / 8))`, so harvest/planting can become a true village-wide surge.
- Fertility cycle:
  - Planting requires `Prepared` or recovered `Fallow` with `nutrients >= 80`.
  - Harvest yield for grain scales by nutrients: `>=180 => 5 grain`, `120..179 => 4`, `80..119 => 3`; seed co-yield remains 1.
  - Harvest reduces nutrients by `30` and marks the tile `Stubble`; Autumn/Winter transition turns stubble into `Fallow`.
  - Each full unplanted season on `Fallow` restores `+15 nutrients` up to the tile’s natural fertility floor/cap; below `80` remains `Exhausted`.
- Plot scaling:
  - Replace `Agricultural: ((members + 2) / 3).clamp(2, 24)` with a helper based on active tiles:
    - `food_tiles = ceil(member_count * 16 / 4)` using 16 target grain per person and 4 expected grain per tile.
    - `labor_tiles = ceil(member_count * 0.60) * 24` for seasonal burst capacity.
    - `seed_tiles = max(grain_seed_stock, current_planted_or_mature_tiles, 32 bootstrap floor)`.
    - `target_active_tiles = min(food_tiles, labor_tiles, seed_tiles)`.
    - `target_plots = ceil(target_active_tiles / 96).clamp(1, 12)` where a 16x16 plot assumes about 96 active crop tiles after paths/fallow/edges.
  - Kitchen gardens count toward the same active-tile budget; do not emit one behind every residence once the farm target is satisfied.
- Workforce pressure:
  - `workforce_budget_system` should read `Calendar` and farm-state counts.
  - Farm pressure should be near-max during Spring when prep/plant work exists and during Autumn when mature grain exists; Winter farm pressure should be zero.
  - Private `FarmWorkScorer` should only fire when the household plot has seasonal work available, avoiding idle farm-goal loops.

## Test Plan
- Unit-test `FarmSeasonPhase` classification from `Calendar`.
- Test plot scaling: a 20-member crop-capable settlement targets about one 16x16 field, not 6-7 fields.
- Test plot carving reserves Agricultural land without writing Cropland until `PrepareField` runs.
- Test Spring posting creates `Prepare`/`Plant` Farm jobs and allows non-Farmer claimants via open seasonal postings.
- Test Autumn posting creates `Harvest` Farm jobs for mature grain and credits job progress on successful gather.
- Test fertility: harvest lowers nutrients/yield, fallow recovers nutrients over seasons, exhausted tiles are skipped by planting.
- Run `cargo test --bin civgame`.

## Assumptions
- Grain is the crop governed by the seasonal field system; berry/tree planting stays outside this pass unless later promoted into orchards.
- No new crates are needed.
- Update `AGENTS.md` and `src/simulation/CLAUDE.md` to document the new seasonal farming, field prep, plot scaling, and fertility behavior.
