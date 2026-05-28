# Farm Scaling Plan

## Context

The farm pipeline has three independent caches/scans that recompute the same per-plot `{unprepared, plantable, mature}` state every tick or every 60 ticks:

- `goal_update_system` (`goals.rs:653,705`): 60-tick `seasonal_work_cache` walks every Agricultural plot's rect to set `private_plot_has_seasonal_work` for `FarmWorkScorer`.
- `chief_job_posting_system` Farm branch (`jobs.rs:3041`): calls `plot_tile_counts` per plot per 60-tick posting cycle.
- `fieldwork_expiry_system` (`farm.rs:1499`): calls `count_unprepared_in_rect` / `count_plantable_in_rect` / `count_mature_crop_in_rect` **every Economy tick per live `FieldWork` posting** for season + capacity drift checks.
- Three HTN dispatchers (`htn_{prepare_field,plant_from_storage,harvest_plant}_dispatch_system`) each call `find_nearest_*_in_rect` per idle farmer per tick.

Each of these scanners walks `Plot.rect`. After partial retirement (`FarmRetirements` drains a tile out of the plot but the rect hull stays intact), the rect can contain holes that no longer belong to the plot. `plot_tile_counts` already guards via `FarmRetirements::is_retiring`, but the other scanners only check `FarmRetirements` for Prepare/Plant and not consistently — and none of them have a way to ask "which tiles does this plot actually own right now?" because `FieldTileIndex.by_tile` doesn't expose a per-plot reverse index.

Net effect: at 20 fields × 60 farmers × 20 Hz, `fieldwork_expiry_system` alone is doing thousands of rect walks per second to repeat the same answer; the goal-scorer cache is keyed on household id, not plot id, so it can't be reused by the posting/expiry/dispatch sides. The Sim Timing budget reddens before the population reaches the cap the plan files in `plans/` are designed for.

The aim is one shared, incrementally-maintained per-plot snapshot of farm work state, with all five consumers reading it instead of re-scanning rects.

## Approach

### 1. Per-plot membership index

Extend `FieldTileIndex` (farm.rs:230) with a reverse index from plot to tile set, and gate every mutation behind helpers so the two views can never disagree.

```rust
pub struct FieldTileIndex {
    pub by_tile: AHashMap<(i32, i32), FieldTileState>,
    by_plot: AHashMap<PlotId, AHashSet<(i32, i32)>>, // private; access via plot_tiles()
}

impl FieldTileIndex {
    pub fn ensure_entry(&mut self, tile, plot_id, fertility) { ... }   // updates both
    pub fn remove_tile(&mut self, tile) -> Option<PlotId> { ... }      // updates both
    pub fn plot_tiles(&self, plot_id) -> impl Iterator<Item = (i32,i32)> { ... }
    pub fn plot_has_members(&self, plot_id) -> bool { ... }
    pub fn plot_tile_count(&self, plot_id) -> usize { ... }
    #[cfg(debug_assertions)]
    pub fn debug_assert_consistent(&self) { ... }
}
```

Only two call sites mutate `FieldTileIndex` today (farm.rs:238 `ensure_entry` in `carve_plots_system`; farm.rs:1109 inside `drain_farm_retirements_system`) — both already touch the helpers and need only the implementation switch. Keep `by_tile` `pub` for now to limit the read-side churn; the next pass can encapsulate.

### 2. Single shared work index — `FarmWorkIndex`

```rust
#[derive(Resource, Default)]
pub struct FarmWorkIndex {
    pub by_plot: AHashMap<PlotId, PlotWorkSnapshot>,
    pub state_owned_by_faction: AHashMap<u32, Vec<PlotId>>,
    pub household_plots: AHashMap<u32, Vec<PlotId>>,
    dirty: AHashSet<PlotId>,
    last_full_audit_day: u16,
}

pub struct PlotWorkSnapshot {
    pub unprepared: u32,
    pub plantable: u32,           // gated on Cropland + nutrients only; seed-budgeting is consumer-side
    pub mature: u32,
    pub updated_tick: u64,
}
```

Drop the original plan's `member_tiles` / `holder` / `faction_id` / `rect` fields — `FieldTileIndex::plot_tiles(pid)` and a one-shot `plot_q.get(plot_entity)` lookup carry that info already, and duplicating it leaks ownership of the consistency contract back across the two data structures.

This index **replaces** the `seasonal_work_cache` `Local` in `goal_update_system`. Single source of truth; one O(plot) walk per tick consumes it; cadence-decoupled from goal selection.

### 3. Dirty tracking — explicit, event-driven

`FarmWorkIndex.dirty` is the source of truth for what to rebuild. Mark dirty at every site that mutates the inputs `PlotWorkSnapshot` depends on. No `Changed<>` filters — those don't fire for `FieldTileIndex` (Resource) or `PlantMap` (Resource) mutations.

| Event | Site | Mark dirty for |
|---|---|---|
| Plot carve / hull extension | `carve_plots_system` (land.rs) on every `field_tiles.ensure_entry` / `plot_index.by_id.insert` | the affected `PlotId` |
| Retirement queued | `carve_plots_system` when staging into `FarmRetirements` | the source `PlotId` |
| Retirement drained | `drain_farm_retirements_system` (farm.rs:1058) after `field_tiles.remove_tile` | the source `PlotId` |
| Prepare-field completion | `prepare_field_task_system` (farm.rs:1173) on the `Cropland` stamp | `PlotIndex.plot_at(tile)` |
| Planting completion | `production::finish_withdraw_material` Planter arm AND `play_plant_task_system` on successful `PlantMap` insert | `plot_at(tile)` |
| Harvest completion | `gather::gather_system` Grain branch on `PlantMap.remove` | `plot_at(tile)` |
| Plant lifecycle stage change on an Ag tile | `plant_lifecycle_system` per transitioned plant | `plot_at(plant.tile)` |
| Plant death (winter mortality / scatter despawn) | same | `plot_at(plant.tile)` |
| Fallow recovery bump crossing `MIN_PLANTABLE_NUTRIENTS` | `fallow_recovery_system` (farm.rs:1330) | `plot_at(tile)` |

`plot_at(tile)` is a thin wrapper that reads `FieldTileIndex.by_tile[tile].plot_id`; cheaper than `PlotIndex.by_tile` (surface-only, redundant for Ag tiles) and avoids confusion with non-Ag plots.

The taxonomy is explicit so reviewers can spot omissions; a sweep across these eight sites is the entire dirty-write surface.

### 4. Refresh strategy

`refresh_farm_work_index_system` runs in `SimulationSet::Economy` after `drain_farm_retirements_system` and after the prepare/plant/harvest executors (Sequential), before `fieldwork_expiry_system` and `chief_job_posting_system`:

1. **Season transition**: rebuild every snapshot (`farm_season_phase` change observed via `Local<Option<FarmSeasonPhase>>`). Bounded by plot count — typically <100.
2. **Dirty drain**: rebuild at most `MAX_DIRTY_REBUILDS_PER_TICK = 16` plots per tick; surplus stays in `dirty` for the next tick. Cap matches `PerfWorkBudget` shape; a 256-tile plot rebuild is ~256 reads, cheap.
3. **Daily audit**: once per `TICKS_PER_DAY`, force-rebuild every snapshot regardless of `dirty` — backstop against missed dirty writes while the new code matures. Drop after the dirty taxonomy is verified in practice (track via a `#[cfg(debug_assertions)]` mismatch counter).

`goal_update_system` (200-tick agent cadence) reads `FarmWorkIndex` directly; up to one tick of staleness is fine since the agent re-evaluation cadence is already 200×.

### 5. Consumer rewrites

- **`goal_update_system` / `FarmWorkScorer`**: drop the local `seasonal_work_cache` and the per-tile scan inside it. `FarmWorkScorer::scorer_potential` reads `ctx.private_plot_has_seasonal_work` from a pre-built `&AHashSet<u32>` populated by walking each household's plots in `FarmWorkIndex.household_plots` and checking the season-appropriate count > 0. Spring seed-budgeting (household-or-parent grain stock) stays in `goal_update_system` because it reads `FactionRegistry` — but it now keys on the cheap precomputed counts, not a fresh tile scan.
- **`chief_job_posting_system` Farm branch (jobs.rs:3041)**: replace the per-plot `plot_tile_counts` call with `farm_work_index.by_plot.get(pid)`. Ranking + seed-budget consumption logic unchanged.
- **`fieldwork_expiry_system` (farm.rs:1499)**: per-posting capacity check reads counts from `FarmWorkIndex` keyed on `JobProgress::FieldWork.plot_id`. Legacy `area`-only postings (no `plot_id`) keep the rect-walk fallback — minority path, fine.
- **HTN dispatchers**:
  - `htn_prepare_field_dispatch_system` / `htn_plant_from_storage_dispatch_system` (Communal + Private scopes): iterate `FieldTileIndex::plot_tiles(plot_id)`, apply live reservation + nutrient + reachability + retirement filters, pick chebyshev-nearest. Drop `find_nearest_unprepared_in_rect` / `find_nearest_plantable_in_rect` (or keep as Bootstrap-scope fallback only; bootstrap has no plot id).
  - `htn_harvest_plant_dispatch_system` (Communal + Private): iterate `FieldTileIndex::plot_tiles(plot_id)` for membership, look up `PlantMap[tile]` for mature `is_farm_plantable` crop. Retirement-aware harvest works naturally: a retiring tile keeps its `FieldTileIndex` membership until `drain_farm_retirements_system` removes it (drain only fires when no plant remains), so the standing crop on a retiring tile harvests and *then* the tile drops out. Bootstrap (no plot rect) keeps `GatherKnowledge::nearest_target_tile`.
- **`FarmScope`** (`htn.rs::resolve_farm_scope`): unchanged shape; both Communal and Private already carry `plot_id` through `JobClaim::Farm` / household plot lookup. Bootstrap stays rect-less.

### 6. Carve-skip via geometry hash

`carve_plots_system` (land.rs:726) currently rebuilds the `CarveJob` work vector every tick regardless of whether the underlying `SettlementPlan` changed. Add a `PlotCarveCache` keyed by faction id:

```rust
#[derive(Resource, Default)]
pub struct PlotCarveCache {
    last_hash: AHashMap<u32, u64>,
}
```

Hash inputs (deterministic): every `SettlementZone { kind, rect }` tuple, ordered by `(rect.x0, rect.y0, kind as u8)`; `StreetSpine` segments + endpoints; `culture_hash`. Output via `ahash::AHasher::default()`.

A faction is skipped when:
1. `plans.0[fid]` geometry hash equals `last_hash[fid]`, AND
2. `FarmRetirements` holds zero tiles whose `old_plot.faction_id == fid` (retirement is a multi-tick drain; the carve loop must keep running through it to re-evaluate hard-conflict subtraction as crops clear).

When carve runs, use `FieldTileIndex::plot_tiles(pid)` to find a plot's current members (replacing the global `by_tile.values().filter(plot_id == pid)` walk in the existing code, if any), and `FieldTileIndex::plot_has_members(pid)` in the retirement drain's "should I despawn this plot?" check.

### 7. Critical files

- `src/simulation/farm.rs` — `FieldTileIndex` helpers, new `FarmWorkIndex` + refresh system, dirty hook from `drain_farm_retirements_system` / `prepare_field_task_system` / `fallow_recovery_system`; gut `find_nearest_*_in_rect` callers.
- `src/simulation/goals.rs` — delete the 60-tick `seasonal_work_cache` Local; build `households_with_seasonal_work` from `FarmWorkIndex`.
- `src/simulation/htn.rs` — three dispatchers' Communal/Private arms switch to `plot_tiles` iteration; harvest dispatcher checks `PlantMap` per member tile.
- `src/simulation/jobs.rs` — chief Farm branch reads `FarmWorkIndex`.
- `src/simulation/land.rs` — `carve_plots_system` writes dirty marks on plot-tile changes; geometry-hash short-circuit + `PlotCarveCache`.
- `src/simulation/plants.rs` — `plant_lifecycle_system` marks affected `plot_id` dirty on stage transitions and despawn.
- `src/simulation/gather.rs` — Grain harvest path marks `plot_id` dirty alongside `PlantMap.remove`.
- `src/simulation/SimulationPlugin` — register `FarmWorkIndex`, `PlotCarveCache`; insert `refresh_farm_work_index_system` into the Economy ordering.

### 8. Tests (cargo test --bin civgame)

- **Unit (farm.rs)**: `FieldTileIndex` helper methods keep `by_tile` and `by_plot` consistent across insert / move / remove / repeated `ensure_entry`; `debug_assert_consistent` passes after each mutation.
- **Unit (farm.rs)**: dirty taxonomy — each of the 8 mark-dirty sites writes the right `PlotId` into `FarmWorkIndex.dirty` via a focused fixture.
- **Unit (farm.rs)**: `refresh_farm_work_index_system` budgeted-drain — 50 dirty plots, `MAX_DIRTY_REBUILDS_PER_TICK = 16`, expect three ticks to clear.
- **Regression (test_fixture)**: partial hard-conflict retirement drains a hole — no `JobKind::Farm` posting / Prepare / Plant task targets the removed hole; standing crop on a retiring tile completes Harvest, then `FieldTileIndex.plot_tiles(pid)` no longer contains it.
- **Regression (test_fixture)**: chief posting counts match the legacy `plot_tile_counts` output for intact plots over a Spring → Summer → Autumn → Winter cycle.
- **Regression (test_fixture)**: `fieldwork_expiry_system` shrinks/drops postings driven by `FarmWorkIndex`; legacy no-plot postings still take the rect path.
- **Regression (test_fixture)**: household goal availability updates within one tick after plot transfer, seed stock change, planting, harvest, and Spring → Winter transition.
- **Regression (test_fixture)**: unchanged `SettlementPlan` makes `carve_plots_system` a no-op (assert no plot entities respawn) after the first carve, except for factions with live `FarmRetirements`.
- **Behavioural**: `cargo run` with the default world — manually inspect that Sim Timing stays green at moderate field+household scale (qualitative).

### 9. Docs

- Update `src/simulation/CLAUDE.md` Farming section: replace the `FARM_PRECOMPUTE_CADENCE_TICKS` precompute paragraph with the `FarmWorkIndex` description; document the dirty taxonomy and budget; note that `FieldTileIndex::plot_tiles` is the authoritative per-plot membership reader.
- Root `CLAUDE.md` Land/Farming subsection: one-line pointer.
- Per `feedback_update_claudemd`, do this in the same commit as the code change.

## Acceptance

- No `plot_tile_counts` / `find_nearest_*_in_rect` / `count_*_in_rect` call survives in the hot path of `fieldwork_expiry_system`, `chief_job_posting_system` Farm branch, or any HTN farm dispatcher; the helpers stay only as Bootstrap-scope fallbacks (and the `fieldwork_expiry_system` legacy-no-plot-id arm).
- `FieldTileIndex` has a per-plot reverse index with helpers; both views proven consistent in debug.
- `goal_update_system` no longer carries the 60-tick `seasonal_work_cache` Local; `FarmWorkScorer` reads from `FarmWorkIndex` instead.
- `carve_plots_system` is a no-op for factions whose plan geometry didn't change and have no live retirements.
- `cargo test --bin civgame` green; play-tested world unchanged gameplay for intact plots.
- No new crates.
