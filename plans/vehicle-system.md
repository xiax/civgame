# Customizable Vehicle System

## Context

`src/simulation/cart.rs` (~1000 lines, Animal Husbandry v2.1) is a hardcoded two-class cart
(`Handcart` / `OxCart`) that only ferries construction material. Replace it with a unified,
freeform-designed `Vehicle` subsystem whose data model also serves future tanks
(engine/track/armor/turret). The first playable content stays ancient-first: handcarts, ox
carts, wagons, chariots — but assembly, movement, damage, and UI are all built generic.

Locked decisions:
- Player gets both AI-proposed templates and a freeform grid designer.
- Deterministic physics/stat simulation, not a continuous rigid-body engine.
- Vehicles occupy true multi-tile world footprints.
- Chariots + crew + combat ship in v1.
- Pathfinding uses a full heading + turn-radius search (`(anchor, z, heading)` nodes).
- No new crates.

### Stat model

The cell grid is load-bearing and stays observable even at tank scale:
- **Hit-location damage** — attacks resolve against specific cells (wheel/axle/track/turret/armor).
- **Footprint shape** — the grid's bounding box *is* the pathing footprint.
- **Ground pressure** — `loaded_mass / footprint_area`, gates marsh/sand/snow traversal.
- **Draft/engine power, speed caps, durability/stress** — derive from the cell bill-of-materials.

The one mechanic cut for v1: **tipping / rollover stability** (`track_width / center-of-mass-height`).
A vehicle has no modelled vertical extent within itself on the discrete grid, so center-of-mass
*height* is unobservable for carts and tanks alike. `stability` stays a **reserved field**
(a simple `track_width`-derived scalar, surfaced in UI, consumed by no system).

## Core Architecture

New module `src/simulation/vehicle.rs`; `cart.rs` migrates into it and is deleted in Phase 4.
Registered in `src/simulation/mod.rs` alongside the husbandry modules.

ECS surface (components unless noted):
- `Vehicle { owner_faction, design_id, purpose, heading, state, anchor_tile, z }`
- `VehicleDesign { name, grid, allowed_purpose, tech_gates, author_faction, revision }` —
  stored in the `VehicleDesignRegistry` resource, not per-entity.
- `VehicleGrid` — `Vec<(IVec2, VehicleCell)>` over a bounded grid (cap ~6×4 cells; v1
  vehicles are 1–3 tiles, headroom reserved for tanks).
- `VehicleCell { kind: VehiclePartKind, material: ResourceId, durability: u16 }`
- `VehiclePartKind` — `Frame, Deck, Wall, Axle, Wheel, Hitch, Yoke, CargoBay, CrewSeat,
  WeaponMount` + reserved `Engine, Track, ArmorPlate, Turret`.
- `VehicleStats` (cached on entity) — `empty_mass, max_payload, loaded_mass,
  draft_power_needed, wheelbase, track_width, ground_pressure, turn_radius, road_speed_cap,
  offroad_speed_cap, stability (reserved), stress_margin`.
- `VehicleInventory` — generalised `CartInventory` (add/take/qty_of/total_qty API reused).
- `VehicleCrew { driver, passengers, gunners }`
- `VehicleDraft { hitched, required_animals, species_mask }`
- `VehicleHealth` — per-cell health mirror + `disabled: VehicleDisableFlags` + `cargo_spill_threshold`.
- `VehicleFootprint { offsets_by_heading: [Vec<IVec2>; 4] }` — precomputed per heading.
- `VehicleOccupancyIndex` (resource) — `tile -> Entity` for collision/pathing.

Coupling: `BoardedVehicle { vehicle, slot }` on people; draft animals keep
`AnimalWorkClaim { use_kind: AnimalUse::Cart }`.

## Phase 1 — Data model, catalog, validation, stats (no integration)

**Files:** new `src/simulation/vehicle.rs`; new `assets/data/vehicles/core.ron` + loader
(mirror `FactionArchetypeRegistry` loading at `WorldPlugin::build`).

- Define all components/enums. `VehicleInventory` is `CartInventory` moved over.
- `core.ron` — material profiles keyed by existing `ResourceId` (wood/stone/skin/copper/
  bronze/iron: mass, strength, friction, traction, durability); part definitions (mass,
  axle-load, wheel-radius, crew/cargo volume, tech gates); stock templates: **Handcart,
  Ox Cart, Four-Wheel Wagon, Light Chariot, War Chariot**.
- Custom designs cost raw resources + tools computed from grid cells into a
  `Vec<(ResourceId, u32)>` bill — do not explode the resource catalog. Stock-part designs
  reuse the existing `cart_frame_*` / `cart_wheel_*` resources.
- `validate_design(grid) -> Result<(), Vec<DesignError>>` — deterministic, pre-queue: one
  connected body; every Wheel linked to an Axle; axles support loaded mass; ≥1 control cell;
  draft vehicles need Hitch/Yoke matching animal count; CargoBay cells reachable from a
  deck/side cell; chariot rule (light frame + spoked wheels + crew platform + horse hitch).
- `derive_stats(grid, materials)` — `empty_mass = Σ cell mass`; `support_limit = axle+wheel+
  frame strength`; `max_payload = min(cargo volume, support_limit − empty_mass)`;
  `draft_power_needed = loaded_mass × terrain_resistance / wheel_efficiency`;
  `speed caps = min(draft/engine, wheel, terrain)`; `turn_radius = wheelbase / steering_factor`;
  `ground_pressure = loaded_mass / footprint_area`; `stability` = track_width scalar (reserved).
- **Tests:** validation rejects disconnected bodies, unsupported wheels, missing driver, bad
  hitch, overloaded axles, blocked cargo; material swaps move mass/durability/payload/speed;
  footprint rotation maps correctly for all 4 headings.

## Phase 2 — VehicleYard, assembly orders, stock templates, queue UI

**Files:** `src/simulation/construction.rs`, `src/simulation/husbandry.rs`,
`src/simulation/vehicle.rs`, `src/simulation/typed_task.rs`, `src/ui/orders.rs`.

- `BuildSiteKind::VehicleYard` — ~12 wood + 6 stone, gated `ANIMAL_HUSBANDRY`; tile-indexed
  `VehicleYardMap` on the `PenMap` on_add/on_remove hook pattern. Assembly + parking anchor
  for multi-tile vehicles.
- `HitchingPost.parked_cart` → `parked_vehicle` (single-tile tether) — rename touching
  `cart.rs`, `husbandry.rs`, `construction.rs`.
- `VehicleAssemblyOrder` — modelled on `CraftOrder`, but holds a `Vec<GoodNeed>` (part bills
  exceed `MAX_BUILD_INPUTS = 3`). Lands on the `JobBoard` as a new `JobKind::Assemble`
  (mirror `JobKind::Craft`: `JobKind` variant + `name()` + `to_goal()` + `available_for`).
  New typed tasks `Task::HaulToVehicleOrder` / `Task::AssembleVehicle` mirror
  `HaulToCraftOrder` / `WorkOnCraftOrder` (dispatcher/executor shape + `production.rs` exit
  helpers).
- On completion: validate the parking footprint and a clear exit route, spawn the `Vehicle`
  with `Indexed::new(...)`, register `VehicleOccupancyIndex`, snapshot the design.
- UI: right-click a `VehicleYard` → "Assemble Vehicle" submenu of stock templates; emits
  `PlayerCommand::QueueVehicle { design_id }` (new variant + `dispatch_one` + lifecycle arm).
- **Tests:** assembly consumes the exact bill and spawns the expected design snapshot.

## Phase 3 — Multi-tile occupancy + heading/turn-radius pathfinding + movement

**Files:** `src/pathfinding/` (new `vehicle_path.rs`; extend `astar.rs` / `step.rs`),
`src/world/spatial.rs`, `src/simulation/movement.rs`, `src/simulation/tasks.rs`.

- `VehicleOccupancyIndex` — `tile -> Entity`; sync system (Sequential, after movement, before
  vision/combat) mirrors `sync_indexed_after_move_system`; `on_remove` hook clears on despawn.
- `footprint_astar` (`vehicle_path.rs`) — node `(anchor_x, anchor_y, z, heading)`. Transitions:
  forward, diagonal/side step, turn-in-place, turn-while-moving (gated on `turn_radius`). Each
  successor tests the full `VehicleFootprint` rotated to the candidate heading against
  `passable_step_3d` per footprint tile, `WallMap` / `StructureIndex` / `BlueprintMap`, doors,
  `VehicleOccupancyIndex`, z-step limits. Dedicated `AStarPool` slot (index 3) so it never
  contends with person/animal pathing. Deterministic tie-breaks (heading then entity bits).
- Vehicle passability: Road/Bridge/Dam best; grass/scrub/soil/cropland penalised; sand/snow/
  forest/marsh gated on `ground_pressure`/traction; water/river/wall/ore/air/occupied blocked.
- Movement: vehicle owns a `VehiclePathFollow`, authoritative on the `Vehicle` entity.
  Driver/passengers/hitched animals/cargo visuals sync to vehicle-relative offsets each tick
  (generalises `cart_follow_system`, inverted — vehicle leads).
- `assign_task_with_routing` gains `Option<&VehicleFootprint>`; `Some` routes via
  `footprint_astar`, `None` is unchanged single-tile behaviour.
- LOD: a vehicle whose driver is Dormant holds/parks; `cohort.rs` pins a vehicle with an
  in-flight order to Full via `PinnedFullSim`. Stranded/abandoned vehicles re-park at the
  nearest yard/post.
- **Tests:** pathing accepts wide clear roads, rejects narrow doors/walls/water/other
  vehicles/bad z; footprint rotation collision correct; occupancy updates on move/despawn/park.

## Phase 4 — Migrate cargo hauling; remove cart-specific code (parity)

**Files:** `src/simulation/vehicle.rs`, `cart.rs` (deleted), `construction.rs`,
`faction.rs` (`compute_faction_storage_system`), `mod.rs`.

- `VehicleCargoHaul` task replaces `Task::CartHaul` — any vehicle with cargo capacity, valid
  loading side, driver, and draft/traction. Port the `htn_cart_haul_dispatch_system` /
  `cart_haul_task_system` two-phase load/deliver logic.
- Construction `JobKind::Haul` prefers a vehicle when remaining qty + route width justify it
  (keep a `CART_HAUL_MIN_REMAINING`-style threshold + a `footprint_astar` route-width check).
- `compute_faction_storage_system` folds `VehicleInventory` into faction totals (replaces the
  cart-specific pass) — preserves storage conservation.
- Small loads still hand-carry (`htn_acquire_good_dispatch_system` unchanged).
- Delete `cart.rs` and every cart-specific symbol once parity tests pass — no shim. Port every
  test in `cart.rs::tests` to vehicle equivalents.
- **Tests:** vehicle cargo hauling deposits into blueprints and preserves storage totals;
  draft animals claimed/synced/released/recovered on abort/death.

## Phase 5 — Freeform designer UI + AI proposal generation

**Files:** new `src/ui/vehicle_designer.rs`, `src/rendering/{entity_sprites.rs,
sprite_library.rs}`, `src/simulation/vehicle.rs`, `src/ui/mod.rs`, `src/ui/inspector.rs`.

- `vehicle_designer.rs` — `egui::Window` with template/proposal list, clickable cell-grid
  editor (new UI primitive), part palette, material picker, live stat preview (`derive_stats`),
  validation results (`validate_design`), Queue button → `PlayerCommand::QueueVehicle`.
  Follow the `inspector.rs` `CollapsingHeader` + `ScrollArea` structure.
- AI designers — daily Economy system proposes designs per faction from tech/materials/
  animals/need: cargo pressure → carts/wagons; raid/military pressure + `WAR_CHARIOT` →
  chariots. Deterministic per faction/culture seed; stored in `VehicleDesignRegistry`. AI
  auto-queues conservative stock templates; player can open/edit/save/queue any proposal.
- Rendering — stock templates get hand-drawn sprites keyed by `design.sprite_key` (generalise
  `spawn_cart_sprites`). Freeform designs render as a composed multi-child sprite (one
  `VisualChild` per part cell, z-offset layered).
- Inspector — add a "Vehicle" `CollapsingHeader` (design name, footprint, cargo bar, crew,
  hitched animals, speed, damage, blocked-path reason).
- Right-click on a vehicle — move, load/unload, repair, assign crew, hitch/unhitch,
  deconstruct/salvage (each a `MenuAction` → `PlayerCommand` arm).

## Phase 6 — Chariot crew, draft, combat

**Files:** `src/simulation/vehicle.rs`, `src/simulation/combat.rs`.

- Chariots are mobile crew platforms: crew board via `BoardedVehicle`, fight through existing
  combat rules with vehicle-provided speed/protection/damage modifiers.
- Combat attacks target crew or vehicle cells; `VehicleHealth` per-cell damage — destroyed
  wheel/axle disables movement, destroyed CargoBay spills `VehicleInventory` as `GroundItem`s,
  destroyed CrewSeat exposes occupants, destroyed Hitch releases animals. Wheel/axle damage is
  the intended ancient counterplay.
- `WeaponMount` cells are modelled and validated now; advanced ranged/turret behaviour stays
  dormant until a ranged-combat system exists.
- **Tests:** damage disables wheels/axles, spills cargo, exposes crew; crew board/unboard;
  drafted chariot move + attack scenario.

## Phase 7 — Docs

- Replace `src/simulation/CLAUDE.md` "Animal husbandry v2.1 — carts" with a "Vehicle system"
  section; update root `CLAUDE.md` if vehicle systems add `SimulationSet` entries. Terse.

## Reused infrastructure (do not rebuild)

- `CartInventory` add/take/qty API → `VehicleInventory` verbatim.
- `HitchingPost` / `VehicleYardMap` ride the `BedMap`/`PenMap` on_add/on_remove hook pattern.
- `JobBoard` + `JobClaim` + `record_progress_filtered` + `JobEscrow` funding — assembly &
  haul postings flow through the existing chief-posting/funding/payout pipeline.
- `CraftOrder` is the structural template for `VehicleAssemblyOrder`; `HaulToCraftOrder` /
  `WorkOnCraftOrder` template the new haul/assemble tasks.
- `assign_task_with_routing`, `AStarPool`, `passable_step_3d`/`passable_diagonal_step`,
  `ChunkRouter`/`ChunkGraph`/`ChunkConnectivity` — extended, not replaced.
- `compute_faction_storage_system` already folds `CartInventory`; swap to `VehicleInventory`.
- `Indexed` / `SpatialIndex` registration pattern for `VehicleOccupancyIndex`.
- `PlayerCommandEvent` → `dispatch_player_command_system` for every vehicle order.
- `goal_dispatch_system` preserve-arms for the new task kinds (mirror `(Haul, CartHaul)`).

## Risks & mitigations

- **Pathfinding cost** — heading/turn-radius search is the heaviest new code. Dedicated
  `AStarPool` slot; vehicles are few; replan only on path-invalidation; bound node budget.
- **Vehicle deadlock** — two multi-tile vehicles in a 2-wide lane. `VehicleOccupancyIndex`
  makes the other vehicle a hard block; the blocked one yields, re-plans after a cooldown, or
  re-parks. No reservation protocol in v1.
- **Migration regression** — Phase 4 deletes `cart.rs`. Port every cart test before deletion;
  Phase 4 gated on parity tests passing.
- **Born-trapped vehicles** — `VehicleYard` placement biased to settlement edges; assembly
  validates the parking footprint and a clear exit route via `footprint_astar` before spawning.

## Test Plan

`cargo test --bin civgame`. Cases per phase above, plus:
- Design validation rejects disconnected bodies, unsupported wheels, missing driver, invalid
  hitches, overloaded axles, blocked cargo access.
- Material swaps change mass/durability/payload/speed/stress.
- Footprint rotation maps occupied cells for all headings.
- Occupancy index updates on move, despawn, parking, blocked reroute.
- Vehicle cargo hauling deposits into blueprints and preserves faction storage totals.
- Existing cart tests ported to vehicle equivalents.

Manual acceptance (`cargo run`, never `--sandbox`, Bronze Age start):
- Build a `VehicleYard`; assemble Handcart, Ox Cart, Wagon, Chariot from templates.
- Compose a custom freeform vehicle, queue it; verify stat preview & material bill match.
- Drive a multi-tile wagon down a wide road; watch it fail cleanly at a narrow choke.
- Haul wood/stone to a construction blueprint with a cargo vehicle; storage totals conserved.
- Crew a chariot with horses, run a drafted move + attack; damage a wheel and confirm the
  vehicle is immobilised and cargo spills.

## Deferred (actionable)

- **Tank content** — `Engine/Track/ArmorPlate/Turret` enum variants exist and
  `validate_design` has the hooks; activating them needs a tech-tree entry, `core.ron` part
  defs, and a ranged-combat/turret system. Write `plans/vehicle-system-tanks.md` when picked up.
- **Save-game serialization** of freeform designs — blocked on the project having no save
  layer; `VehicleDesignRegistry` is a single serializable resource when one lands.
- **Rollover/tipping** — `stability` is a reserved field; needs a vertical-extent model to be
  observable.

## Assumptions

- First playable content stops at ancient vehicles; tank parts are future extension points.
- Vehicle pathfinding can be rarer and more expensive than person pathfinding (vehicles few).
- Road widening is not required for v1 (carved roads are already 2 tiles wide); vehicle yards
  sit near settlement edges so large vehicles are not born trapped.
