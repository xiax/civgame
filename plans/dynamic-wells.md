# Realistic Dynamic Wells

## Summary
Revamp wells as one `Build Well` action with tile-derived depth and finite daily yield. A well’s shaft depth, material cost, work time, refill rate, and storage will be computed from the local aquifer table instead of the current fixed `WELL_REACH_Z = 4.0`.

## Key Changes
- Add a shared well-spec helper in simulation code:
  - Compute water-table Z using the same `cell_surface_z - aquifer_depth_z` frame already used by terrain, runtime seep, and current well checks.
  - Use `MAX_HAND_DUG_WELL_DEPTH_Z = 16`, `MIN_WELL_DEPTH_Z = 2`, and a `+1 Z` safety buffer.
  - Reject well sites deeper than max; otherwise produce `shaft_depth_z`, dynamic inputs, work ticks, `refill_sips_per_day`, and `max_stored_sips`.
- Extend `Well` from just `{ faction_id }` to include:
  - `shaft_depth_z`
  - `stored_sips`
  - `max_stored_sips`
  - `refill_sips_per_day`
- Keep `BuildSiteKind::Well` and the right-click action unchanged, but make well blueprints dynamic:
  - `Blueprint::new` keeps the base recipe, then well creation applies the tile-specific spec.
  - Add `Blueprint.work_required` defaulting to recipe work; construction and hover read this instead of `recipe.work_ticks`.
  - Formula defaults: current shallow wells stay close to today’s `4 stone + 2 wood + 120 work`, while deep wells scale up to roughly `10 stone + 5 wood + 240 work`.
- Add well yield limits:
  - Each well drink sip consumes one `stored_sips`.
  - Daily refill adds `refill_sips_per_day`, capped at `max_stored_sips`.
  - Refill score uses rainfall plus depth: wet/shallow wells support a village well; deep/arid wells support fewer people and can temporarily run dry.
  - Add a distinct `DrinkOutcome::WellExhausted`; dry/depleted wells are skipped by drink dispatch.
- Update AI and placement:
  - Manual `Build Well` is disabled/rejected on tiles whose table is too deep.
  - Seed/chief/organic placement ranks reachable well sites by shallow depth, recharge, and existing spread.
  - Organic water pressure compares settlement demand against total well daily refill, not just well count; pending wells count as one conservative average well until completed.
- Update UI/docs:
  - Hover for well structures shows depth, stored water, and daily refill.
  - Blueprint hover shows dynamic work required.
  - Update `AGENTS.md`, `src/simulation/CLAUDE.md`, and `src/world/CLAUDE.md` well/water-table notes.

## Public API / Type Changes
- Add `WellSpec` and helpers such as `well_spec_at(globe, chunk_map, tile)`, `well_reaches(surface_z, aquifer_z, shaft_depth_z)`, and `well_is_usable`.
- Change all `Well { faction_id }` construction sites and tests to build from `WellSpec`.
- Add `Blueprint.work_required: u8`; all existing non-well blueprints use the recipe value unchanged.

## Test Plan
- Unit tests for water-table sampling, dynamic depth, unbuildable too-deep sites, cost/work scaling, variable `well_reaches`, depletion, and daily refill.
- Drink integration tests: agents consume finite well sips, skip exhausted wells, and fall back to rivers or other wet wells.
- Construction tests: a deep-but-within-limit well finalizes with enough shaft depth and is drinkable; too-deep manual/chief well sites are rejected.
- Seed/organic tests: Neolithic starts still stamp at least one valid well when groundwater is reachable; dry low-yield settlements request additional wells as population grows.
- Run `cargo test --bin civgame`.

## Assumptions
- No new tech tier or separate “Deep Well” build action in this pass.
- Yield is counted in drink sips, matching the current multi-sip thirst system.
- Groundwater quality remains clean unless `SanitationMap` contaminates the well tile.
- Rivers remain effectively unlimited water sources; wells provide cleaner, local, finite public supply.
