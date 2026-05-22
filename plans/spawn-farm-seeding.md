# Zone-Backed Spawn Farm Seeding

**Status: SHIPPED.** Survey → project → carve segment runs in-chain at
`OnEnter(Playing)`; `seed_starting_farms_system` refactored to stamp into the
carved plot. `seed_belt_pre_stamps_bounded_starter_cropland`,
`seed_starting_farms_spawns_physical_grain_seed_at_storage`, and
`seeded_cropland_stays_inside_agricultural_plots` (post-OnEnter + post-tick180
re-assertion) green. `cargo test --bin civgame` — 1049 passed.

## Context

`seed_starting_farms_system` (`farm.rs:469`) currently creates a `Plot` entity **directly**
from the pre-building `SettlementBrain` belt parcel, then stamps a bounded `Cropland`
starter patch inside it. The plot is created **outside** the `carve_plots_system`
pipeline and never writes `PlotIndex.by_faction_hash`.

Traced consequences:

1. `carve_plots_system` (`land.rs:687`, FixedUpdate Economy, staggered `(fid+tick)%60`)
   re-carves whenever `plot_index.by_faction_hash[fid] != plan.culture_hash`. Because
   the seed system never set that hash, the **first** runtime carve always mismatches —
   it tears down every plot in `by_settlement` (including the seed plot), despawns them,
   and drops their `ag_tiles` entries (`land.rs:760-790`).
2. `kickoff_initial_survey_system` computes the brain **before** `seed_starting_buildings_system`
   runs, so it has no built structures. The first post-seed runtime survey recomputes the
   brain *with* the seeded buildings; `build_ag_belt` keys belt placement on the built-up
   footprint (anchors/districts), so the Agricultural belt can shift.
3. When the belt shifts, the re-carved Agricultural plots no longer cover the
   seed-stamped `Cropland` tiles. `carve_plots_system` deliberately leaves `Cropland`
   terrain in place on teardown (`land.rs:774-786`) → **orphaned `Cropland`**: tilled
   terrain with no backing plot, no `ag_tiles` road protection, no `FieldTileIndex`
   nutrient state. The zone overlay also doesn't show the seeded farm until ~tick 60.

**Goal:** keep the year-one pre-tilled `Cropland` starter patch, but make spawn-time
cropland impossible to exist outside a `carve_plots_system`-owned Agricultural plot.
The fix is to run the survey → project → carve pipeline **inside** the `OnEnter(Playing)`
chain (post-build), so farm seeding stamps into a real carved plot and `by_faction_hash`
is established before any runtime carve — aligning with the unified-build-pipeline
direction (one organic pipeline for seed and runtime).

## Implementation

### 1. Factor the plan projection (`settlement.rs`)

`settlement_planner_system` (`settlement.rs:1047`) is staggered (`(fid+tick)%60`) and
gated on `needs_plan`, so it cannot be reused verbatim at `OnEnter`. Extract its
per-faction body into a shared helper:

```rust
pub fn project_plan_for_faction(
    fid: u32, faction: &FactionData, tick: u64,
    settlement_map: &SettlementMap, brains: &SettlementBrains,
) -> Option<(SettlementPlan, u64 /*new_hash*/)>
```

`settlement_planner_system` calls it (keeping its stagger + `needs_plan` + spine-push).
Add a new `OnEnter` system `project_initial_settlement_plans_system` that iterates **all**
non-SOLO factions unconditionally, calls the helper, and inserts into `SettlementPlans`.
The OnEnter variant does **not** push spine segments to `RoadCarveQueue` — OnEnter road
carving is already owned by `kickoff_initial_survey_system` + `seed_starting_buildings_system`.

### 2. Re-survey after seeding (`organic_settlement.rs`)

Add a thin OnEnter wrapper `resurvey_after_seeding_system` that reuses the existing
synchronous `survey_one_settlement` core (the same body `kickoff_initial_survey_system`
calls). Running it after `seed_starting_buildings_system` recomputes each `SettlementBrain`
against the *actual built structures*, so the belt placement is final and matches what
the first runtime survey will produce. **This re-survey is load-bearing** — without it
the fix only delays orphaning to the first post-build runtime survey instead of preventing
it.

### 3. New OnEnter chain segment (`mod.rs`)

Insert between `seed_starting_buildings_system` and `seed_starting_farms_system`, each
carrying the existing `run_if(not(resource_exists::<SandboxMode>))` guard:

```
seed_starting_buildings_system
  → resurvey_after_seeding_system          (SettlementBrain reflects built structures)
  → project_initial_settlement_plans_system (SettlementBrain → SettlementPlans)
  → land::carve_plots_system               (SettlementPlans → Plot entities, ag_tiles, FieldTileIndex, by_faction_hash)
  → seed_starting_farms_system             (find carved Ag plot, stamp starter Cropland, seed grain)
```

`carve_plots_system` is reused **as-is** — it already takes only resources that exist at
OnEnter (`SettlementPlans`, `WorldGen`, `Globe`, `SettlementMap`, `FactionRegistry`,
`ChunkMap`, `PlotIndex`, `FieldTileIndex`) and is idempotent on `by_faction_hash`.
Downstream OnEnter systems already ordered after `seed_starting_farms_system`
(`populate_seed_reservation_system`, `relocate_stranded_members_system`,
`backfill_field_tile_index_system`, `mark_warmup_complete_system`) keep working since
they read `ag_tiles` after the carve.

### 4. Refactor `seed_starting_farms_system` (`farm.rs:469`)

It no longer creates `Plot` entities. Per settled non-SOLO, non-nomadic faction
(`seed_buildings` true):

- **Remove** the `already_seeded` guard (`farm.rs:502-510`) — after the in-chain carve
  every faction has Agricultural plots, so that guard would now skip *everyone*.
- **Remove** the `brains`-based `belt_rect` scan (`farm.rs:546-571`) and the direct
  `Plot` spawn / `plot_index` inserts (`farm.rs:573-594`). Drop the now-unused `brains`
  parameter.
- **Find** the carved Agricultural plot: walk `plot_index.by_settlement[sid]`, resolve
  each via `plot_q`, filter `zone_kind == Agricultural && faction_id == fid`, pick the
  one nearest `home_tile` with deterministic tie-breaks (`(chebyshev, rect.x0, rect.y0)` —
  same key already used for the belt scan).
- **Stamp** the bounded starter `Cropland` patch inside that plot's `rect`, keeping the
  existing budget `min(demand_tiles/2, plot_area/2)`, the `Grass | is_soil_like` pre-stamp
  guard, and `TileChangedEvent`. Do **not** re-insert `ag_tiles` / `by_tile` /
  `FieldTileIndex` — `carve_plots_system` already owns those (fertility is preserved by
  `set_tile`, so nutrients already equal natural fertility).
- If no Agricultural plot exists for the faction, stamp no `Cropland` but still seed the
  grain seeds + year-1 food buffer (unchanged).
- Grain / food provisioning logic (`farm.rs:638-675`) is unchanged.

Sandbox (`!seed_buildings`) and nomadic factions remain skipped. No new crates / tile kinds.

## Tests (`test_fixture.rs`)

- **`seed_belt_pre_stamps_bounded_starter_cropland`** (~5450): `seed_starting_farms_system`
  no longer creates the plot. Rework setup to inject the belt parcel into the brain, then
  run `project_initial_settlement_plans_system` + `carve_plots_system` before
  `seed_starting_farms_system` (or directly insert a carved `Agricultural` `Plot` into
  `PlotIndex`). Keep the assertions: all 256 belt tiles in `ag_tiles` + `FieldTileIndex`,
  `1..=128` tiles stamped `Cropland`, nutrients == natural fertility.
- **`seeded_cropland_stays_inside_agricultural_plots`** (~17552): after `trigger_onenter`,
  add a `tick_n(...)` long enough to clear at least one `settlement_planner_system` +
  `carve_plots_system` cycle (≥ ~150 ticks for stagger + replan headroom), then re-run
  the scan. Assert every `TileKind::Cropland` within ±80 of home is **both** in
  `PlotIndex.ag_tiles` **and** covered by a current `SettlementPlans` Agricultural zone —
  immediately after spawn *and* after the ticks.
- Run the full suite: `cargo test --bin civgame`.

## Verification

- `cargo run` a Neolithic start; confirm the starter farm patch is visible at tick 0,
  the Agricultural zone overlay shows it immediately (not after ~60 ticks), and after
  several in-game minutes no `Cropland` sits outside an Agricultural plot near home.
- `cargo test --bin civgame` — both updated tests plus `onenter_era_seeding` pass.

## Doc updates

Update `src/simulation/CLAUDE.md` (the `OnEnter(Playing)` chain in "Game-start seeding",
the "Game-start seeding (`seed_starting_farms_system`)" Farming entry) and the root
`CLAUDE.md` `Cropland` note to describe the new survey → project → carve → seed-farms
order and that the starter patch is now stamped into a carve-owned plot.

## Notes

- Persistent abandoned `Cropland` from genuine runtime rezoning later in the game is
  still allowed by design — this plan only removes it as a *spawn-seeding byproduct*.
- Creating non-Agricultural plots (Residential/Crafting/…) at OnEnter via the in-chain
  carve is harmless: `carve_plots_system` only registers index entries (no tile stamping),
  and having plots from tick 0 is more correct for land-ownership consumers.
