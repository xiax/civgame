# Prevent Arbitrary Farm Relocation

## Summary
- Farms become sticky at the tile level once worked.
- The planner may only reclaim farm land on a hard expansion conflict, and it should nibble overlapping tiles instead of moving the whole plot.
- Relocation is staged: old farm tiles stop receiving new work, standing crops finish, and replacement farm capacity is added elsewhere.

## Key Changes
- Add committed/retiring farm state:
  - Track active Agricultural tiles per plot, using `PlotIndex.ag_tiles` / `FieldTileIndex` as the source of truth.
  - Add a small retirement resource keyed by tile: old plot id, requested tick, and whether the tile is waiting on crop harvest or in-flight fieldwork.
- Update organic planning:
  - Preserve existing worked Agricultural plots across surveys unless a new non-ag road/building/parcel footprint directly overlaps them.
  - Treat score-only farm-belt changes as invalid for committed plots.
  - On hard conflict, subtract only overlapping tiles from the farm; keep the rest of the plot active.
  - Add replacement Agricultural parcels only to recover lost active farm capacity or satisfy demand growth.
- Update plot carving:
  - Stop using whole-rectangle teardown for committed Agricultural plots.
  - Release only retired tiles from `by_tile`, `ag_tiles`, and `FieldTileIndex`.
  - Keep planted/claimed tiles protected until harvest/fieldwork clears; then allow housing/roads to claim them.
- Update farm work:
  - Count/search only tiles still active for the plot, not every coordinate inside `Plot.rect`.
  - Do not post Prepare/Plant work on retiring tiles; Harvest remains allowed for standing crops.
- Update docs in `src/simulation/CLAUDE.md` and the relevant AGENTS notes.

## Test Plan
- Regression: later survey finds a “better” farm belt but no hard overlap; original worked plot and prepared tiles remain.
- Regression: expansion parcel overlaps 4 tiles of a worked farm; only those 4 retire, the rest remains farmable.
- Regression: planted overlapping tiles are not released until harvest/season-safe retirement.
- Existing checks:
  - `cargo test --bin civgame seeded_cropland_stays_inside_agricultural_plots`
  - `cargo test --bin civgame carve_plots_reconciles_shifted_agricultural_rect`

## Assumptions
- “Hard conflict” means direct overlap by expansion roads or non-ag settlement parcels, not a planner score change.
- Empty prepared field tiles may retire immediately; planted or actively claimed tiles retire after harvest/fieldwork clears.
