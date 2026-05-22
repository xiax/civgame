# Fishing System Plan

**Status: shipped.** v1 (shore/bank/marsh/coast fishing) implemented end-to-end —
`fishing.rs` module, `TaskKind::Fishing`, daily regen, `fish`/`preserved_fish`
resources + recipe, `SkillKind`/`ActivityKind::Fishing`.
HTN integration ships as proper **registry methods** — `FishForImmediateFoodMethod`
(`AcquireFood`) and `FishForStorageMethod` (`StockpileFood`), competing at
`UTIL_BASELINE` in the argmax against forage/scavenge/withdraw. `PlannerCtx`
gained a `fish_spot_tile` field, populated by a `ChunkMap`-only scan in the two
food dispatchers (gated on faction `FISHING`); depleted-spot avoidance is
`MethodHistory` failure-biasing, so the dispatcher needs no `FishStock` and the
param ceiling is a non-issue. `FISHING` tech keeps its `Foraging` discovery
trigger (re-pointing it onto `Fishing` would deadlock discovery); `LOG_RAFT`
re-pointed instead. Phase 8 (weirs/nets/boats) deferred as designed.

## Summary

Add fishing as a terrain-based renewable food source gated on the existing
Mesolithic `FISHING` tech. Historically plausible shore/bank/marsh/coast
fishing that plugs into existing tech, HTN food goals, food storage, and
economy presets — no per-tile fish entities. Extensible toward weirs, nets, and
boats later. Baseline fishing needs zero structures once `FISHING` is known.

Three corrections from codebase verification are baked into this plan:
1. No coast/ocean/lake tile kinds exist — open water is `TileKind::Water`,
   fresh/brackish/salt tracked via salinity. `FishHabitat` is **derived** from
   `TileKind` + salinity, never stored as terrain.
2. Pre-generating `FishStock` for every water tile is infeasible on a streamed
   world. Stock is **lazily initialized per tile** on first access,
   deterministically from world seed; un-touched tiles are implicitly full.
3. Spot discovery uses a **terrain scan**, not memory clusters — `SharedKnowledge`
   clusters decay/merge, wrong fit for static water. Depletion lives in
   `FishStock` only.

Also: reuse `GatherClaims` (no new `FisheryClaims`); don't pre-compute
`stand_tile` — `assign_task_with_routing` routes to a passable water neighbour.

## Phase 1 — Resources & sprite

- `assets/data/resources/core.ron`: add `fish` (`class: food`,
  `edible_calories: ~120`, `bulk: small`, `tags: ["food","edible","perishable",
  "animal_product"]`, `sprite_key: "item_fish"`) and `preserved_fish`
  (`edible_calories: ~200`, `tags: [...,"preserved",...]`). Catalog is
  data-driven; `ResourceId` assigned alphabetically at load.
- `src/economy/core_ids.rs`: `fish()` / `preserved_fish()` `OnceLock` accessors,
  mirroring `wood()`/`meat()`.
- `src/rendering/sprite_library.rs`: `RESOURCE_FISH` pixel-art sprite (existing
  32-colour palette), registered under `"item_fish"`.
- No food-counting work: `FactionStorage::food_total()` sums all catalog edibles
  via `core_ids::edibles()`; `is_preserved_ration()` keys on the `"preserved"`
  tag, so two-pass eating (`production.rs` ~L1420) treats `preserved_fish` as a
  ration automatically. `policy::apply_preset` is catalog-wide — Mixed/Market
  include fish with no change.

## Phase 2 — Fish ecology (`src/simulation/fishing.rs`, new module)

- `FishHabitat { River, Lake, Marsh, Coast }`, derived by
  `habitat_at(tile, chunk_map, runtime_water) -> Option<FishHabitat>`:
  River→River, Marsh→Marsh, `Water`→Coast if salinity Salt/Brackish else Lake.
  `Bridge` is an access tile, not a spot; `Dam` is not fishable.
- `FishingMethod { Handline, Trap }` for v1; reserve `Weir`, `Net`, `BoatLine`
  (data-model only).
- `FishStockCell { habitat, biomass: f32, capacity: f32, last_regen_tick,
  seasonal_phase }`.
- `FishStock(AHashMap<(i32,i32), FishStockCell>)` Bevy `Resource`, mirroring
  `farm::FieldTileIndex` — survives chunk streaming, **not** restamped on load.
  - `get_or_init(tile, habitat, seed, calendar)`: lazy; absent tiles implicitly
    full; first access seeds capacity/biomass from `seed ^ hash(tile)` (per-tile
    hash pattern from `chunk_streaming.rs::spawn_chunk_plants`). Capacity scales
    by habitat and `river_distance_at`.
  - `harvest(tile, amount)` clamps biomass to `0..=capacity`.
- `fish_regen_system` — `SimulationSet::Economy`, daily
  (`clock.tick % TICKS_PER_DAY == 0`), iterates only populated entries, logistic
  regen toward capacity with seasonal multiplier (spring/autumn river runs ↑,
  winter open-water ↓); drops fully-recovered entries to keep the map sparse.
- Capacity/regen constants centralized at module top.

## Phase 3 — Task plumbing (`src/simulation/tasks.rs`)

- `TaskKind::Fishing` (next discriminant) and
  `Task::Fish { spot_tile: (i32,i32), method: FishingMethod, output_resource:
  ResourceId }` — **no `stand_tile` field**.
- Update the four helpers, mirroring `Gather`: `task_kind_label` → `"Fishing"`;
  `task_requires_free_hands` → 1; `task_interacts_from_adjacent` → `true`;
  `task_is_labor` → `true`. Update `#[cfg(test)]` assertions.

## Phase 4 — Executor (`fish_task_system` in `fishing.rs`)

- `SimulationSet::Sequential`, after movement, near `gather`.
- On arrival (chebyshev ≤ 1 to `spot_tile`, mirroring `gather_system`):
  revalidate faction has `FISHING` (`FactionTechs::has`), spot still fishable,
  agent on a passable adjacent tile, biomass > 0, claim held. Accumulate
  `FISH_WORK_TICKS` (`Trap` cheaper labour, lower yield than `Handline`).
- On completion: `harvest`, yield scaled by skill + `FISHING` `food_yield_bonus`
  + season + habitat; output `fish` into a free hand, overflow spills as a
  `GroundItem` via existing `spawn_ground_drop`/`spawn_or_merge_ground_item`.
  Log `ActivityKind::Fishing`, award `SkillKind::Fishing` XP.
- On exhausted/invalid stock: release the `GatherClaim`, cancel the tail task
  cleanly (mirror `gather` cancel paths).
- Bridge: worker stands on passable `Bridge`, fishes adjacent `River` — handled
  by the routing layer naturally.

## Phase 5 — Claims (reuse `gather_claims.rs`)

No new type. Stake a `GatherClaim` on the water `spot_tile` at dispatch; release
on success/cancel/failed-validation via `release_gather_claim`; TTL via
`suggested_expiry`; `gather_claim_expiry_system` already sweeps. Dispatch uses
`GatherClaims::pressure` to prefer unclaimed, higher-stock, closer spots.

## Phase 6 — HTN integration

- `src/simulation/htn.rs`: register methods in `register_builtin_methods()` —
  no new `AgentGoal`.
  - `AgentGoal::Survive`: `FishForImmediateFood` → `[Fish, Eat]`.
  - `AgentGoal::GatherFood`: `FishForStorage` → `[Fish, DepositToFactionStorage]`.
  - `precondition`: faction has `FISHING`, free hand, fishable water with
    stock > 0 within `FISHING_SEARCH_RADIUS` (terrain scan via `SpatialIndex`/
    `ChunkMap` — no memory lookup). `tech_gate: Some(FISHING)`. `utility`:
    expected yield × stock fraction ÷ distance, lifted by hunger urgency and
    seasonal/`food_yield` multipliers; competes with forage/hunt at same tier.
- `src/simulation/goal_contract.rs`: add `FISHING_SEARCH_RADIUS` (~12–15).
- `goals.rs` `goal_dispatch_system`: preserve-arms for
  `(GatherFood, TaskKind::Fishing)` and `(Survive, TaskKind::Fishing)`.
- Dispatch passes the water `spot_tile` to `assign_task_with_routing` — it finds
  the passable stand neighbour; do not pre-pick a stand tile.

## Phase 7 — Skill, activity, preservation recipe

- `src/simulation/skills.rs`: add `SkillKind::Fishing`, bump `SKILL_COUNT`, add
  `name()` arm, update `#[cfg(test)]` count assertions. Fixed arrays expand via
  the const.
- `src/simulation/technology.rs`: add `ActivityKind::Fishing`, bump
  `ACTIVITY_COUNT`, update tests. Re-point the `FISHING` tech `TechTrigger`
  (and `LOG_RAFT` where relevant) from `Foraging` onto `Fishing`.
- `src/simulation/crafting.rs`: add a `Preserved Fish` `CraftRecipe` to
  `build_craft_recipes()`, mirroring "Preserved Meat" — inputs `[(fish,2),
  (wood,1)]` → `preserved_fish` ×3, `work_ticks: 60`,
  `tech_gate: Some(FOOD_SMOKING)`, `requires_station: Some(Workbench)`.
- `ui/inspector.rs` renders skills/activities from the enums — no extra wiring.

## Phase 8 — Deferred (data hooks only)

Keep `FishingMethod::{Weir,Net,BoatLine}` and habitat fields. Defer
`FishingWeir`/`Dock`/`Net` construction and `LOG_RAFT`/`DUGOUT_CANOE`
boat-reachable deep-water spots to a follow-up.

## Doc updates

- New `src/simulation/CLAUDE.md` entry for the `fishing` module (lazy
  `FishStock`, daily regen, terrain-scan dispatch, `GatherClaims` reuse).
- Root `CLAUDE.md`: note `TaskKind::Fishing`, `SkillKind`/`ActivityKind` count
  bumps, fish habitat derived from `TileKind` + salinity.

## Test Plan

- Unit (`fishing.rs` `#[cfg(test)]`): `habitat_at` classifies River/Marsh/Lake/
  Coast and rejects non-water + Dam; `FishStock` lazy init deterministic per
  seed+tile, un-touched tiles read full, `harvest`/regen clamp `0..=capacity`;
  seasonal multiplier never negative; `task_*` helpers correct;
  `Preserved Fish` recipe `FOOD_SMOKING`-gated, yields `preserved_fish`.
- Integration: faction with `FISHING` + low food + nearby river dispatches a
  worker to fish then eat/deposit; without `FISHING` falls back to forage;
  workers spread across spots via claims/pressure; depleted spots stop
  attracting until regen; salt coastal `Water` fishable but not drinkable; fish
  deposits satisfy chief food postings; Mixed/Market price fish via catalog.
- Run focused fishing tests, then `cargo test --bin civgame`.
- Manual: `cargo run`, riverside faction — confirm agents fish, fish enters
  storage/food totals, depleted bank recovers over following days.

## Assumptions

- v1 is shore/bank/bridge/marsh/coast fishing only — no boats/deep water.
- Fish are local renewable stocks, not spawned animals.
- Reuses `FISHING`, `FOOD_SMOKING`, `LOG_RAFT`, `DUGOUT_CANOE` techs; no new
  techs or crates.
- Weirs/docks/nets are in the data model, implemented after the core loop.
