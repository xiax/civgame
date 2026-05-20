# Animal Husbandry v2 — Draft Work (Plow + Cart)

## Context

v1 (taming generalisation, seeded herds, pens/stables) shipped. Domestic animals are owned and housed but contribute no economic labour. v2 wires the existing `AnimalWorkClaim { worker, use_kind, expires_tick }` + `AnimalUse::{Plow, Cart, Pack, Mount}` data shapes (already in `animals.rs`) into real plowing and cart hauling, gated on `ARD_PLOW` (Chalcolithic) and `OX_CART` (Chalcolithic) — both techs already exist in `technology.rs`.

## Scope

- **Plow loop**: trained cattle + ard plow → plot tilled → grain yield boost.
- **Cart loop**: trained horse/cattle + ox cart → larger hauls than hand-carry.
- **HitchingPost** finalises into the cart/plow workflow (entity already builds in v1 as inert).

## Entry points to modify

- `assets/data/resources/core.ron` — add `ard_plow` (wood + tools, weight ~5kg, class material) and `cart` (wood + tools + skin, weight ~30kg, class material). `core_ids.rs` autopicks IDs alphabetically.
- `crafting.rs` recipes — `Ard Plow`: 4 wood + 1 tools, gated by `ARD_PLOW`, Workbench. `Ox Cart`: 8 wood + 2 tools + 2 skin, gated by `OX_CART`, Workbench.
- `typed_task.rs` — three new variants: `Task::HitchAnimal { animal, implement }`, `Task::PlowPlot { plot_id, animal, plow }`, `Task::CartHaul { source, dest, animal, cart, resource_id, qty }`. Register `TaskKind` discriminants. Add `task_interacts_from_adjacent` arms (plow walks the plot, cart walks source→dest).
- `htn.rs` — three new abstract tasks + methods: `PlowPlotForFaction`, `HaulMaterialViaCart`, `EngagePlowAnimal`. All `MF_UNINTERRUPTIBLE`. Dispatcher scans for an idle trained animal claim slot + a posted Farm job whose `plot_id` lacks the plowed-year stamp.
- `production.rs` — executor for the three new tasks. Plow ticks plot fertility +1 and stamps `Plot.plowed_year = current_year`; cart writes residual into faction storage at dest.
- `farm.rs` — extend `Plot` with `plowed_year: Option<u16>`; `chief_farm_plot_assignment_system` increments `plant_yield_factor` (new field on PlotIndex.ag_tile_yield) when plowed; planting precedes plowing → cancels plant order if plowed_year != current_year (or accepts lower yield, configurable).
- `chief_job_posting_system` (jobs.rs) — when a Farm posting has `plot_id` AND faction has `ARD_PLOW` + a hitched bull + plow, route a `JobKind::PlowPlot` posting before the planting posting. Posting reward 12.0.
- `compute_faction_storage_system` (economy/) — third pass: settled-faction cart cargo (if hitched cart is current_z within `HAUL_FOOTPRINT` of storage) folds into faction storage rollup. Mirror the nomadic pack-inventory pass.
- `chief_directive_system` (construction.rs) — Stables emit when horse count > 0 AND `HORSE_TAMING` known. Already partially live via v1 `husbandry_intent_emitter_system`; v2 extends to surface stable-when-horses bug if any.

## Open questions

- Cart-cargo into faction storage for **settled** factions — does the cart need to physically reach `FactionStorageTile`, or can a Trader-style atomic transfer handle it on arrival? Recommend: atomic transfer on `CartHaul` task completion (mirrors `buy_material_task_system` exclusive-system pattern).
- Plowing as a wage-earning job vs. attached to existing farmer's chain? Recommend: own posting via `JobKind::PlowPlot` so household farmers don't need to retrain.
- Bull training thresholds — when is a cattle considered "trained"? Recommend `DomesticAnimal.training >= 80` (training ticks from being in a faction-owned pen for 30 days at +1/day from `assign_preferred_home_system` while housed).

## Verification

- New tests in `production.rs`:
  - `plow_task_increments_plot_plowed_year`
  - `cart_haul_delivers_more_than_hand_haul`
  - `hitch_releases_on_task_complete`
- `cargo run` Bronze era — confirm a chief posts a Plow job, an ox is claimed via `AnimalWorkClaim::Plow`, the plot's grain yield bumps after plowing, deposit follows.
