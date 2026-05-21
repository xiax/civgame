> **SUPERSEDED** by the physically-excavated well feature (see
> `~/.claude/plans/mossy-snuggling-puddle.md`, shipped in `src/simulation/well.rs`).
> The aquifer-depth derivation here was kept (`well::aquifer_z_at`,
> `well_spec_at`, `TooDeep`); the abstract finite-sip yield was dropped in
> favour of a physical `RuntimeWater` column. Retained for history only.

# Realistic Dynamic Wells

## Goal
Replace the fixed `WELL_REACH_Z = 4.0` model with **tile-derived well depth** and **finite
daily yield**. A well's shaft depth, build cost, work time, refill rate, and storage are
computed from the local aquifer table. Deep/arid sites become unbuildable or low-yield;
wet/shallow sites support a village.

## Current state (verified)
- `Well { faction_id }` — `construction.rs:135`, `#[derive(Component, Clone, Copy, Debug)]`.
- `WELL_REACH_Z: f32 = 4.0` const — `drink.rs:494`. Used by `well_has_water` (`:503`) and
  the pure `well_reaches(surface_z, aquifer_z)` (`:523`).
- Aquifer frame: `cell_surface_z` from `globe.sample_climate` macro elevation;
  `aquifer_depth_z = (hc.filled_height - hc.aquifer_level) * GLOBE_H_TO_Z`;
  `aquifer_z = cell_surface_z - aquifer_depth_z` (`drink.rs:511-516`). Same frame as
  Pass 4.5 / fluid-sim seep gate — reuse verbatim.
- `perform_drink` (`drink.rs:67`) takes `well_map: &WellMap` (only `contains_key`); the
  `DrinkSource::Well` branch returns `WellDry` on `!well_has_water`. `DrinkOutcome` already
  has `WellDry` (`:152`).
- `drink_task_system` (`drink.rs:207`) multi-sip loop (≤ `MAX_SIPS_PER_ACTION=4`).
- `nearest_well_tile` (`drink.rs:532`) skips dry wells; `htn_drink_dispatch_system` ranks
  `[local_well, local_water, home_well, home_water]`.
- `Blueprint` (`construction.rs:638`) — no `work_required`; construction completion gate
  reads `recipe.work_ticks` at `:5944` / `:6234` / `:6249`. `build_progress: u8`.
- Well recipe — `4 stone + 2 wood`, `work_ticks: 120`, `WELL_DIGGING`-gated (`:1047`).
- Well blueprint finalize spawns `Well { faction_id }` + `WellMap` insert (`:6773`).
- Well placement: organic `SettlementPressureKind::WaterAccess` (`organic_settlement.rs:2443`),
  pressure→intent at `:2648`; nomadic direct seed at `construction.rs:7970`; manual
  right-click `Build Well` via `PlayerCommand::Build`.

## Design

### 1. `WellSpec` + helpers (`drink.rs`)
- `WellSpec { shaft_depth_z: i8, stone: u8, wood: u8, work: u8, refill_sips_per_day: u32,
  max_stored_sips: u32 }`.
- `well_spec_at(globe, chunk_map, tile) -> WellResult` where
  `WellResult { Ok(WellSpec), TooDeep, Unresolvable }`:
  - Compute `surface_z` (`chunk_map.surface_z_at`) and `aquifer_z` using the **exact
    existing frame** (factor the `drink.rs:511-516` block into a shared
    `aquifer_z_at(globe, tile) -> Option<f32>`; `well_has_water` calls it too — no second
    formula).
  - `needed_depth_z = (surface_z as f32 - aquifer_z).ceil() as i8 + WELL_SAFETY_BUFFER_Z(1)`.
  - `shaft_depth_z = needed_depth_z.clamp(MIN_WELL_DEPTH_Z(2), MAX_HAND_DUG_WELL_DEPTH_Z(16))`.
  - `needed_depth_z > MAX_HAND_DUG_WELL_DEPTH_Z` → `TooDeep` (site rejected).
  - Chunk/hydro not resolvable → `Unresolvable` (callers fall back to a default shallow
    spec — never reject a buildable well on a transient unloaded read).
  - Cost/work scale linearly in depth, hard-capped (keep within `u8`, `build_progress`
    is `u8`): `work = lerp(120 → 240)`, `stone = lerp(4 → 10)`, `wood = lerp(2 → 5)`
    across `shaft_depth_z ∈ [MIN, MAX]`.
  - `refill_sips_per_day` from **recharge** = `aquifer wetness × depth penalty`:
    wetter table (smaller `aquifer_depth_z`) + lower aridity (`river_distance_at`) →
    higher refill; deeper shaft → lower. `max_stored_sips = refill_sips_per_day × STORE_DAYS`.
- Pure tests get a new signature: `well_reaches(surface_z, aquifer_z, shaft_depth_z) -> bool`
  (was 2-arg with the `WELL_REACH_Z` const). `WELL_REACH_Z` is removed; `MIN_WELL_DEPTH_Z`
  becomes the floor. **Existing `well_reaches` tests at `drink.rs:561-570` must be updated**
  to the 3-arg form.
- Three-level predicate, explicit:
  - `well_reaches(surface_z, aquifer_z, shaft_depth_z)` — pure shaft-vs-table.
  - `well_table_reachable(globe, chunk_map, tile, shaft_depth_z)` — replaces `well_has_water`;
    takes the well's own depth instead of the old const.
  - `well_is_usable(globe, chunk_map, tile, well: &Well)` — `well_table_reachable && stored_sips > 0`.

### 2. `Well` component (`construction.rs`)
Extend (stays `Copy`):
```
Well { faction_id, shaft_depth_z: i8, stored_sips: u32, max_stored_sips: u32,
       refill_sips_per_day: u32 }
```

### 3. Blueprint integration — single chokepoint (improvement over original plan)
The original plan said "well creation applies the tile-specific spec," which scatters
across every well-blueprint spawn site (chief intent, player command, organic, seed) and
risks a missed site. Instead:
- Add `Blueprint.work_required: u8`; `Blueprint::new` defaults it to `recipe.work_ticks`.
  Construction reads `bp.work_required` at the three `recipe.work_ticks` sites
  (`:5944`, `:6234`, `:6249`).
- Add **`populate_well_blueprint_system`** — an observer on `Added<Blueprint>` (mirrors
  `populate_pending_clear_system`). When `kind == Well`, compute `well_spec_at`; on `Ok`/
  `Unresolvable` rewrite `deposits` (stone/wood) + `work_required`; on `TooDeep` despawn the
  blueprint and (if `posted_by` is a player command) the routing layer surfaces the failure.
  Every well blueprint — no matter which producer — flows through this one system.
- **Finalize recomputes** `well_spec_at` (pure, deterministic given globe+chunk_map) to
  populate the `Well` fields — no new `Blueprint` field for depth/refill. The aquifer is
  quasi-static; if the fluid sim ever mutates `aquifer_level` mid-build, snapshot the spec
  onto the blueprint instead. `stored_sips` starts at `max_stored_sips` (a freshly dug
  well is full).

### 4. Finite yield + daily refill
- New **`well_refill_system`** (Economy, daily — `tick % TICKS_PER_DAY == 0`, alongside
  `feed_trough_consume_system`): `stored_sips = (stored_sips + refill_sips_per_day).min(max_stored_sips)`.
- Optional seasonal modulation: scale `refill_sips_per_day` by `Calendar::discharge_multiplier`
  (the dam emitter already uses it) so wells run lower in the dry season. Include it — low
  cost, and it is what makes deep/arid wells "temporarily run dry."
- `well_table_reachable` already handles a seasonal table drop → `WellDry` independent of sips.

### 5. Drink path (`drink.rs`)
- `perform_drink` `DrinkSource::Well` branch needs `&mut Well`. Change the signature to
  take `well: Option<&mut Well>` for the well branch (caller resolves
  `WellMap → Entity → Query<&mut Well>`). On success decrement `stored_sips -= 1`.
  Outcomes:
  - well entity gone / not adjacent → `SourceGone` (unchanged).
  - `!well_table_reachable` → `WellDry` (unchanged meaning: shaft can't reach table).
  - `stored_sips == 0` → new `DrinkOutcome::WellExhausted` (distinct from `WellDry`:
    temporary, refills next day; same caller handling — cancel chain, agent re-plans).
- `drink_task_system` multi-sip loop: each `perform_drink` consumes one `stored_sips`;
  loop exits early on `WellExhausted` just like `WellDry`. The `&mut Well` borrow is held
  across the ≤4 iterations.
- `nearest_well_tile` + `htn_drink_dispatch_system` skip exhausted wells: add a
  `Query<&Well>` (read-only, ParallelB-safe) so the scan calls `well_is_usable` instead of
  `well_has_water`. An all-exhausted-well village then reads "fresh water far" and drives
  `WaterAccess` pressure — no new event bus.
- Update the chief-stockpile reuse path of `perform_drink` for the new signature.

### 6. Placement
- **Manual `Build Well`:** `PlayerCommand::Build` dispatch re-checks `well_spec_at`; on
  `TooDeep` reject with `CommandFailure::Ineligible` (reuse — no new variant). `orders.rs`
  right-click menu may additionally grey the action when the hovered tile resolves `TooDeep`
  (cheap, optional polish).
- **Organic / chief / seed placement:** rank candidate well sites by
  `(shaft_depth_z asc, refill_sips_per_day desc, spread)`; never pick `TooDeep`.
- **`WaterAccess` pressure (`organic_settlement.rs:2443`):** compare settlement drink
  demand (`members × SIPS_PER_PERSON_PER_DAY`) against **total daily refill of existing
  wells**, not well count. A pending well counts as one conservative average well until
  finalized. `count_near` / `nearest_fresh_or_well_distance` must treat an exhausted or
  too-deep-table well as "no well" so a depleted village still requests capacity.
- **Seed wells** (`seed_apply_intent` direct stamp + nomadic `construction.rs:7970`):
  compute `well_spec_at` and spawn the `Well` with the full spec; `Unresolvable` → default
  shallow spec.

### 7. UI / docs
- Structure hover for `Well`: shaft depth, `stored_sips / max_stored_sips`, daily refill.
- Blueprint hover: dynamic `work_required` (not `recipe.work_ticks`).
- Update root `CLAUDE.md` (tile/water notes if any), `src/simulation/CLAUDE.md`
  (thirst-pipeline well bullet), `src/world/CLAUDE.md` (water-table notes), and `AGENTS.md`.

## Calibration (tune empirically; starting points)
- `SIPS_PER_PERSON_PER_DAY` — derive from thirst decay (~2× hunger) and
  `DRINK_THIRST_REDUCTION`; start ≈ 20.
- Healthy shallow well: `refill_sips_per_day` ≈ `15 × SIPS_PER_PERSON_PER_DAY` (≈ 300).
- `STORE_DAYS` ≈ 1.5 → buffer over a single dry-season day.
- Deep/arid wells scale down toward ≈ 1–3 persons of support.
- Verify with a Neolithic start: village wells should not chronically exhaust unless arid.

## Public API / type changes
- `WellSpec`, `WellResult`, `well_spec_at`, `aquifer_z_at`, `well_table_reachable`,
  `well_is_usable`; `well_reaches` gains `shaft_depth_z`.
- `Well` gains four fields; all construction/seed/test sites build from `WellSpec`.
- `Blueprint.work_required: u8` (defaults to `recipe.work_ticks`).
- `DrinkOutcome::WellExhausted`.
- `perform_drink` well branch takes `Option<&mut Well>`.
- `WELL_REACH_Z` removed.

## Edge cases
- Chunk unloaded at placement → `Unresolvable` → default shallow spec, never a spurious
  reject.
- Seasonal table drop on a built well → `WellDry` (sips irrelevant) → graceful re-plan.
- `build_progress`/`work_required` are `u8`; cap dynamic well work ≤ 250.
- Animals do not draw from wells today — finite yield is agent-only; no change to
  `animal_water_seek_system`.
- Contamination unchanged: well tiles still read `SanitationMap`.

## Test plan
- Unit: `aquifer_z_at` sampling; depth derivation + clamping; `TooDeep` rejection;
  cost/work scaling endpoints; `well_reaches` (3-arg) variants; `stored_sips` depletion;
  daily refill cap; seasonal refill modulation. **Rewrite the existing `well_reaches`
  tests** (`drink.rs:561-570`).
- Drink integration: agent consumes finite sips; `WellExhausted` distinct from `WellDry`;
  exhausted/too-deep wells skipped by dispatch; fall back to river or another wet well.
- Construction: a deep-but-within-limit well finalizes with the correct shaft depth and is
  drinkable; `populate_well_blueprint_system` rewrites cost/work; too-deep manual/chief
  sites rejected; full well at finalize (`stored_sips == max_stored_sips`).
- Seed/organic: Neolithic start stamps ≥ 1 valid well when groundwater is reachable;
  low-refill village requests more wells as population grows; `WaterAccess` pressure keys
  on total refill vs demand.
- `cargo test --bin civgame`.

## Assumptions
- No new tech tier or separate "Deep Well" build action.
- Yield in drink sips, matching the multi-sip thirst system.
- Groundwater clean unless `SanitationMap` contaminates the tile.
- Rivers remain effectively unlimited; wells are cleaner, local, finite public supply.

## Out of scope (deferred)
- Animal use of wells; well durability/repair; bucket/rope as a consumable;
  player-visible aquifer overlay.
