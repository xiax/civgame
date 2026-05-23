# Follow-up: carve / plan sync after unified seed pipeline

## Context

When the seed pipeline switched from legacy `generate_candidates` to the organic `pressure_to_intent` path (this branch), the test `seeded_cropland_stays_inside_agricultural_plots` (`src/simulation/test_fixture.rs` line ~17961) became sensitive to a downstream gap between `PlotIndex.ag_tiles` and `SettlementPlans` Agricultural zone rects.

The test panics at `post-tick180` with a tile like `(261, 255)`:

- `PlotIndex.ag_tiles.contains((261, 255))` — passes (so a Plot entity still tracks that tile)
- `plans.0.values().any(zone.kind == Agricultural && zone.rect.contains((261, 255)))` — **fails**

Meaning a Plot entity still owns `(261, 255)` as Agricultural, but no current `SettlementPlan` Agricultural zone projects onto it. The two should be in sync: every live Plot must correspond to a current `brain.parcels` entry that `compat_plan_from_brain` emits as a Zone.

## Why the unified pipeline triggers it

Seed-time hut anchors differ slightly under the organic pipeline (commons-respecting radial fallback vs legacy radial `find_building_origin`). Different anchors → different doormats → different road_carve queue entries → different brain.parcels at tick 60+. When `compute_settlement_survey` re-emits brain.parcels with a different Agricultural rect, `carve_plots_system` should tear down the stale plot and add the new one. The test shows the teardown isn't fully happening — `PlotIndex.ag_tiles` keeps the old plot's tiles while the plan emits only the new parcel.

The test was added in 5bca4c0 ("Implement Zone-Backed Spawn Farm Seeding") specifically to catch belt-shift regressions; the gap is real and the test is doing its job.

## Likely fix area

- `src/simulation/land.rs::carve_plots_system` — review the stale-teardown path: does it remove every `ag_tiles` entry from the old plot's rect before adding the new plot? Check whether `culture_hash` mismatch detection actually fires for *minor* parcel shifts (different `frontage_edge`, different rect by a few tiles) or only major resets.
- Alternative: `compat_plan_from_brain` could emit a zone for every live Plot (not just live brain.parcels) so the plan stays in sync with PlotIndex even when the brain drifts.

## Current state

- `seeded_cropland_stays_inside_agricultural_plots` is `#[ignore]`d with a pointer to this plan.
- Every other test in the OnEnter / cropland / farming / construction / organic_settlement suites passes (1065/1066).
- The user-visible behaviour change from the unified pipeline (commons buffer honoured in Neolithic+ starts) is intact and verifiable in-game.

## Out of scope here

- Reverting any part of the unified pipeline. That was the explicit ask.
- Loosening the test's assertion: the invariant is correct and worth preserving once the carve/plan sync gap is fixed.
