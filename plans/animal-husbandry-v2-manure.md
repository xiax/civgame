# Animal Husbandry v2 — Manure Loop

## Context

v1 ships pens/stables and feed troughs but no waste loop. Real subsistence agriculture closes the loop: livestock are fed grain → drop manure → manure restores soil fertility. This makes domestic animals a positive economic input on farming (not just a sink) and gives the player a reason to co-locate pens with agricultural belts.

## Scope

- Add a `manure` resource.
- Cattle / horses / pigs (not dogs / cats) periodically drop `manure` while hunger is satisfied AND `preferred_home` is set.
- A new `Task::CollectManure` walks from pen to manure piles and deposits to a faction `FactionStorageTile` (or, ideally, scatters back onto adjacent farm plots directly).
- Farm plant-growth bumps `plant_yield_factor` per manured plot.

## Entry points to modify

- `assets/data/resources/manure.ron` — new resource (class organic, bulk small, weight 800g, storage_class pile, trade_base_value 1). Add `manure_smell` tag for any future flag.
- `core_ids.rs::manure()` is auto-derived from the alphabetic-sort.
- `animals.rs` — `domestic_manure_drop_system` (Economy, daily, faction-staggered): for each `DomesticAnimal` of species Cattle/Horse/Pig with `last_cared_tick` recent (≤ 1 day) AND adjacent to its `preferred_home`, roll 25% chance to drop a `GroundItem { resource_id: manure, qty: 1 }` at the animal's tile.
- `typed_task.rs` — `Task::CollectManure { source_tile, dest_tile }` + `TaskKind::CollectManure` discriminant + `task_interacts_from_adjacent` arm (walks source).
- `htn.rs` — `CollectManureMethod`, `MF_UNINTERRUPTIBLE`. Goal `AgentGoal::Farm` (extends the farm goal scope) — dispatcher fires when a household farmer has a plot AND a same-faction manure ground-item lies within radius 12.
- `production.rs` — `collect_manure_task_system`: picks up the manure, walks to dest (a `Plot` tile in the agent's household's Agricultural plot), deposits, increments that plot's `manure_applied_year` counter.
- `farm.rs` — `Plot.manure_applied_year: Option<u16>`. `chief_farm_plot_assignment_system` reads this; if applied this year, yield factor +0.25 (additive — *not* multiplied with `soil_fertility_mult`, to keep the boost bounded).
- `compute_faction_storage_system` already rolls in pile-stored items; no change.

## Open questions

- Storage: should manure deposit to `FactionStorage` (then a separate haul-to-plot job spreads it) or apply directly on plot tiles when collected? Recommend the latter — fewer round trips, matches real subsistence practice. Faction storage clutter argues against it too.
- Pet bonus for adjacency: if a domestic animal sleeps in a manured plot's adjacent pen, +0.1 yield factor that growing season (small, encourages co-location). Defer to playtesting.
- Maximum stack: cap manure GroundItems per tile at 4 to avoid storage clutter; surplus despawn (returns to soil).

## Verification

- New tests:
  - `domestic_animal_drops_manure_when_fed`
  - `farmer_with_manure_increments_plot_yield`
  - `manure_does_not_drop_from_dog_or_cat`
- `cargo run` Neolithic settled — confirm cattle near pen drop manure piles, household farmer collects them onto own plot, next growing year shows yield boost in inspector.
