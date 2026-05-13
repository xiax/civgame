# Thirst, Drinking, and Sanitation

## Context
Thirst is a physiological survival pressure peer to hunger. People and animals seek out water; raw/contaminated sources roll nonlethal sickness. `Health` is never damaged by thirst or sickness — the floor is "slow + miserable," not "dying."

Revised from the original (`evaluate-this-plan-please…`). Key revisions:
- `ThirstScorer` is the sole goal-selection path (Phase F-2 removed the imperative cascade).
- Waste is a tile-entity `WastePile`, not a `ResourceId`.
- Sickness is a parallel `Sickness` component (not a `cause` field on `Injury`) — `injury_tracking_system` derives `Injury` from `Body` damage every frame and would clobber illness markers.
- Boiling = `CraftRecipe` (`raw_water × 2 + wood × 1 → clean_water × 1`), not a bespoke `BoilWater` task kind.
- Ocean classification = `WaterKind::{Fresh, Salt}` via `world::biome::water_kind_at` (Globe sample at runtime — no `TileData` layout change).

## Shipped

### Resources & catalog (Phase 1)
- `clean_water` + `raw_water` in `assets/data/resources/core.ron`; `core_ids::{clean_water, raw_water}` accessors.
- 500 g/unit; `material` class so they flow through `FactionStorage.totals` automatically.

### Tile semantics
- `TileKind::is_drinkable_candidate()` covers `Water`/`River`/`Marsh`.
- `world::biome::{WaterKind, water_kind_at}` classifies ocean `Water` as `Salt` via `Globe::sample_climate` + `Biome::Ocean`. Rivers and Marsh are always `Fresh`.

### Needs & ticking (Phase 1)
- `Needs.thirst` field; `THIRST_RATE = 4.0/s` (≈ 2× hunger); con_scale floor 0.25×.
- `Needs::worst()` and `avg_distress()` (divisor 7→8) include thirst.
- `AnimalNeeds.{thirst, sickness}` fields; per-animal decay in `animal_needs_tick_system`.
- `AnimalState::Drinking` added.

### Goals & HTN (Phase 2)
- `AgentGoal::Drink` + `TaskKind::Drink (50)` + `Task::Drink { source }` + `DrinkSource::{Inventory, Tile { tile } }`.
- `ThirstScorer` (Survival class) — fires when `thirst >= THIRST_TRIGGER (180)`; urgency curve `[0.30, 1.0]`; reason `"Thirsty"` / `"Parched"` at severe.
- `htn_drink_dispatch_system` (ParallelB): inventory-first, then `nearest_fresh_drinkable_tile` (walks chebyshev rings, skips salt tiles); scan widens at `THIRST_SEVERE (230)`.
- `drink_task_system` (Sequential): adjacency executor; consumes `clean_water` from inventory or sips from adjacent tile; rolls sickness on raw / contaminated tile sources.
- Boiling CraftRecipe added (`raw_water × 2 + wood × 1 → clean_water × 1`, gated on `FIRE_MAKING`, Workbench station).
- `goal_dispatch_system` preserves the `(Drink, TaskKind::Drink)` walk leg across goal-eval ticks.
- `AgentGoal::Drink` allowed while packed (`allowed_while_packed`).

### Animals (Phase 3)
- `animal_water_seek_system` (ParallelA after `animal_needs_tick_system`): thirsty wandering animals flip to `Drinking` and route to an adjacent passable tile next to the nearest non-salt water source.
- `animal_drink_system` (Sequential after movement): on adjacency to a fresh-water tile, consumes `ANIMAL_DRINK_THIRST_REDUCTION (90)` thirst; raw (non-River) sources bump `AnimalNeeds.sickness` by 20.

### Sanitation (Phase 4)
- `sanitation.rs`: `SanitationMap` Resource (sparse `AHashMap<(i32,i32), f32>`).
- `WastePile { intensity, created_tick }` Component, `LatrineContained` marker, `Latrine` marker.
- `sanitation_emit_system` (daily, run_if-gated): distributes each pile's intensity into the map via `1/(d²+1)` falloff within `CONTAMINATION_RADIUS (6)`. Latrine-contained piles emit at `LATRINE_CONTAINMENT_FACTOR (0.25)`.
- `sanitation_decay_system` (daily): factor `2^(-day/4)`, drops cells below `CONTAMINATION_FLOOR (0.05)`.
- `SanitationMap::is_contaminated(tile)` returns true above `CONTAMINATION_DRINK_THRESHOLD (0.5)`.

### Sickness (Phase 5)
- Parallel `Sickness { severity, applied_tick }` Component (NOT a field on `Injury` — see Context for why).
- `apply_sickness_severity(existing, severity, now)` helper merges/inserts.
- `sickness_decay_system` (daily): `severity -= SICKNESS_DECAY_PER_DAY (16)`; removes at zero.
- `sickness_work_factor(severity) -> f32 ∈ [0.5, 1.0]` slowdown helper (call sites that consult it land in follow-up).
- Drink executor wired: contaminated tile → severity `140`; raw tile (non-river fresh) → severity `60`; inventory clean → no roll.
- `Health` is never touched.

### Latrine structure (shipped)
- `BuildSiteKind::Latrine` + `BuildRecipeIdx::Latrine`; recipe `2 wood + 1 stone`, 50 work_ticks, no tech gate (open-trench latrines predate writing). Deconstruct refund 1 wood.
- Finalize path in `construction_system` spawns the `sanitation::Latrine` marker + `StructureLabel("Latrine")` + Transform/Visibility. Wired into the right-click "Build Latrine" menu; ghost sprite reuses `wall_stone_ascii` until dedicated art ships.
- `agent_defecation_system` already queries `Query<&Transform, With<Latrine>>` (Phase 8a); newly-spawned latrines participate immediately — agents defecating within `LATRINE_ROUTING_RADIUS (8)` chebyshev tag their `WastePile` `LatrineContained`, dropping contamination contribution to `LATRINE_CONTAINMENT_FACTOR (0.25)` of raw intensity.

### Phase 8 follow-ups (shipped)
- **Agent defecation tick** (`sanitation::agent_defecation_system`): every `DEFECATION_INTERVAL_TICKS (2 days)` per agent (staggered by `Entity.index() % interval` so the band doesn't fire in lockstep), spawns a `WastePile` at the agent's current tile with `intensity = 1.0`. Tags `LatrineContained` when a `Latrine` is within `LATRINE_ROUTING_RADIUS (8)` chebyshev tiles, so the emit pass scales down by `LATRINE_CONTAINMENT_FACTOR`.
- **Corpse rot → waste**: `corpse_decay_system` spawns a `WastePile` (`intensity = 1.5`) at the corpse tile on despawn. Second contamination source — a battle-aftermath cluster pollutes ambient water without any agent intervention.
- **Sickness slowdown wired** into `movement_system`'s two `work_progress.saturating_add` sites (Working-while-adjacent + Working-during-arrival arms). Added `Option<&Sickness>` to the movement query; per-tick base * `sickness_work_factor(severity)` so sickness 255 halves progress. Applies uniformly across every executor that runs work_progress through movement.
- **Smoke test** (`test_fixture.rs::smoke::thirsty_agent_with_clean_water_drinks_and_drops_thirst`): agent with `thirst = TRIGGER + 20` and 3× clean_water in inventory drinks within 300 ticks — thirst drops below trigger, ≥1 clean_water consumed, no Health damage, no Sickness inserted.

## Deferred (with concrete entry points)

### Chief water stockpiling — **scrapped (anachronism)**
Bulk grain-style water stockpiling doesn't match pre-modern reality. Pre-industrial water access was: drink at the source (river / spring), fetch in a personal vessel for immediate household use, or — once dug — draw from a well. Faction-treasury-funded chief postings to "fill the granary with water" treat water like a tradeable surplus commodity, which it isn't. **Replace with wells when the well structure lands** (entry points below). Until then, `clean_water` in inventory comes only from boiling raw water at a hearth (the existing CraftRecipe 13).

### Wells — **not yet built**
No `Well` structure exists in the codebase as of this writing. When added:
- New `BuildSiteKind::Well` + recipe (likely `4 stone + 2 wood`, gated on `PERM_SETTLEMENT` or higher).
- A well functions as a tile-tagged drinking source: `Task::Drink { source: DrinkSource::Well { entity } }` reduces thirst without consuming inventory and without rolling sickness (assumes underground water table is clean unless `SanitationMap` contamination at the well tile exceeds threshold).
- `htn_drink_dispatch_system` learns a Well branch ordered above the river branch when one is closer than the nearest fresh-water tile.

### Sickness slowdown wiring
`sickness_work_factor` is implemented and exported; the per-tick `work_progress` advance in `eat_task_system` / `crafting.rs` / `dig.rs` / `construction.rs` etc. can multiply by it. Today no executor consults the helper — a follow-up sweep adds `Option<&Sickness>` to each `work_progress` write site.

### Inspector readouts
`Sickness` is not yet surfaced in the inspector (the main query is already near Bevy's tuple ceiling — needs the `WageInspectorParams` SystemParam treatment to add another optional component cleanly).

## Test Plan
- `cargo test --bin civgame` covers: `Needs::worst` / `avg_distress` includes thirst; `SanitationMap` decay math; threshold gates.
- Manual: `cargo run`, observe thirsty agent walking to a river / drinking inventory water / sipping from a marsh (with sickness bump). Verify ocean tiles are skipped.
- Tile-classification negative test: `water_kind_at` returns `Salt` on a `Water` tile with `Biome::Ocean` and `Fresh` otherwise.

## Critical files
- `assets/data/resources/core.ron`, `src/economy/core_ids.rs`
- `src/world/{tile.rs, biome.rs}`
- `src/simulation/{needs.rs, animals.rs, goals.rs, tasks.rs, typed_task.rs, drink.rs, sanitation.rs, medicine.rs, crafting.rs, goal_scorers.rs}`
- `src/simulation/mod.rs` (registrations)
- `src/ui/{inspector.rs, hover.rs}` (thirst bar)
