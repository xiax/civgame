# Carve/Plan Sync After Unified Seed Pipeline

## Context

`seeded_cropland_stays_inside_agricultural_plots` (`src/simulation/test_fixture.rs:17969`) is `#[ignore]`d because the unified seed pipeline (commits `5bca4c0`, `f2329b5`) exposes a real desync between `PlotIndex.ag_tiles` and `SettlementPlans` Agricultural zones. The test fires at post-tick180 with e.g. `(261, 255)`:

- `PlotIndex.ag_tiles.contains((261, 255))` ✓ — a Plot entity still owns it
- `plans.0.values().any(z.kind == Agricultural && z.rect.contains((261, 255)))` ✗ — no current zone covers it

The invariant is correct; the production code has a hole.

## Root cause

`layout_hash` is positionally blind. `src/simulation/organic_settlement.rs:4582–4596` hashes only:

```
seed ^ phase ^ pop_bucket ^ parcels.len() ^ road_hash ^ culture_traits
```

It does **not** include any parcel `shape.rect()` coords. So when a re-survey produces the same number of Agricultural parcels covering the same road network but at slightly shifted rects (commons-respecting fallback vs the OnEnter pre-seed survey), `layout_hash` is identical, `culture_hash` is identical, and `carve_plots_system` (`src/simulation/land.rs:687–856`, gate at line 718) **skips the settlement** — old Plot entities survive with the old rects.

Meanwhile `compat_plan_from_brain` (`organic_settlement.rs:1129–1232`, Agricultural emission at 1143–1167) emits zones directly from the **new** `brain.parcels`. Result: `SettlementPlan.zones` has the new rect, `PlotIndex.ag_tiles` still holds the old rect.

Secondary structural facts:

- `Plot` (`land.rs:150–184`) has no `parcel_id` backlink.
- `Parcel.id` is **not stable across surveys** — `build_parcels` reallocates each cycle.
- `SettlementPlan.zone` carries only `(kind, rect)`.
- Existing teardown is **wholesale per settlement** when `culture_hash` flips: every plot is despawned and re-carved, losing durable state.

## Rejected alternatives

- **"`compat_plan_from_brain` emits a zone per live Plot."** Inverts the unified pipeline (makes `PlotIndex` the source of truth) and reintroduces the seed/runtime divergence that `f2329b5` deliberately removed.
- **Strengthen `layout_hash` to include parcel rects.** Works, but every parcel shift thrashes every Plot in the settlement, blowing away `plowed_year`, `tenure`/`holder`, `base_value`, `last_valued_tick`, `missed_payments`, `parent_plot`. Mid-game surveys would routinely lose this state. Also still couples carve correctness to a coarse trigger.

## Fix: per-plot rect reconciliation

Make `carve_plots_system` idempotent against the current plan instead of gated on a hash. Per settlement, every tick:

1. Collect `current_zones: Vec<(ZoneKind, TileRect)>` from `plan.zones`.
2. For each existing `Plot` in this settlement, check whether `plot.rect` is fully contained in some `current_zones` entry of the same `zone_kind`.
   - **Yes** → keep the Plot (preserves `plowed_year`, `tenure`, etc.).
   - **No** → call `tear_down_plot(pid)` (factored from the existing cleanup body at `land.rs:760–790`): despawn entity, drop from `by_id`/`by_settlement`/`by_tile`/`ag_tiles`/`field_tiles`.
3. For each `current_zones` entry, compute the tiles not already covered by a kept plot of the same kind and carve fresh plots over the uncovered remainder. Reuse the existing subdivision pass.
4. Drop `by_faction_hash` as the correctness gate. Keep it (or a per-settlement variant) only as an optional fast-path skip when nothing changed AND every settlement plot still matches. Correctness must not depend on it.

This restores the natural invariant: every live Plot is backed by a current `plan.zones` rect, and every current `plan.zones` rect has plots over its tiles.

### Cropland revert on teardown

After step 3, walk the set of tiles touched by Agricultural plot teardowns. For each such tile:

- If still inside some current Agricultural plot → leave `Cropland` as-is.
- Otherwise → revert to the underlying natural soil (mirror the write-side of `farm::prepare_field_task_system`) and emit `TileChangedEvent`.

This satisfies the *first* test invariant (`every Cropland tile is in plot_index.ag_tiles`) and prevents long-lived orphaned Cropland anywhere the plot belt shifts.

Order matters: revert must happen **after** step 3 so we know tiles are truly orphaned, not just uncovered for one intra-tick step.

## Files

- `src/simulation/land.rs`
  - `carve_plots_system`: replace the `by_faction_hash` early-return with per-plot rect reconciliation. Factor cleanup into `tear_down_plot(pid, ...)`. After reconcile, run the orphaned-Cropland revert pass.
  - `PlotIndex`: optional `is_tile_in_any_ag_plot(tile)` helper (trivial via `ag_tiles`).
- `src/simulation/organic_settlement.rs` — no change (`layout_hash` and `compat_plan_from_brain` stay as-is).
- `src/simulation/test_fixture.rs` — unignore `seeded_cropland_stays_inside_agricultural_plots`. Add a regression test: same parcel count, same roads, shifted Agricultural rect → reconcile drops old, keeps unrelated plots' `plowed_year`.
- `src/simulation/CLAUDE.md` — one-line note on the new behaviour.

## Reused utilities

- `TileRect` containment helpers in `world/`.
- `TileChangedEvent` (existing pattern in `prepare_field_task_system`).
- Existing teardown body (`land.rs:760–790`) — extract, don't rewrite.
- `FieldTileIndex` mutation paths already used during teardown.

## Verification

1. `cargo test --bin civgame seeded_cropland_stays_inside_agricultural_plots` (unignored) — passes at post-tick60/120/180.
2. `cargo test --bin civgame` — full suite, expect previous 1065 + the unignored one.
3. New regression test (above): confirms position-blind `layout_hash` no longer masks drift AND that durable Plot state survives.
4. `cargo run`, default settled start, year 2 spring: no Cropland outside Ag plots in inspector; ox-plowed plots stay plowed across re-survey; no plot flicker between ticks.

## Out of scope

- Reverting any part of the unified pipeline.
- Loosening the test assertions.
- Adding stable `Parcel` IDs. (Useful long-term — would replace rect-containment with ID matching — but not needed to fix this invariant.)
