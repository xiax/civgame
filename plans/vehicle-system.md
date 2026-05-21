# Customizable Vehicle System

## Context

`src/simulation/cart.rs` (~1000 lines, Animal Husbandry v2.1) is a hardcoded two-class cart
(`Handcart` / `OxCart`) that only ferries construction material. Replace it with a unified,
freeform-designed `Vehicle` subsystem whose data model also serves future tanks and siege
engines. First playable content is ancient-first (handcarts, ox carts, wagons, chariots) but
assembly, movement, damage, and UI are all built generic.

Locked decisions:
- **Vehicles are designed on a 3D cell grid** — they have real height (tanks 2+ cells tall,
  siege engines taller). Height is load-bearing for stats, pathing, combat, and rendering.
- Player gets both AI-proposed templates and a freeform grid designer.
- Deterministic physics/stat simulation, not a continuous rigid-body engine.
- Vehicles occupy true multi-tile world footprints.
- Chariots + crew + combat ship in v1.
- Pathfinding uses a full heading + turn-radius search (`(anchor, z, heading)` nodes) **and
  enforces vertical clearance** — a tall vehicle fails cleanly at low overhangs/tunnels.
- **Rollover is a live failure mode in v1** — tall/narrow/overloaded vehicles can overturn.
- No new crates.

### 3D cell grid — what height drives

Every physics quantity is real and observable because vehicles have a modelled vertical extent:
- **Center of mass & tipping** — `center_of_mass` is the mass-weighted 3D centroid of cells;
  `stability = track_width / com_height`. A tall narrow siege tower genuinely overturns; a
  wide low cart does not. Live mechanic (Phase 3).
- **Vertical clearance** — a vehicle spanning `height_z` world Z-levels needs that many open
  Z-levels above every footprint tile; pathing blocks it from low cliff overhangs and short
  tunnels (Phase 3).
- **Hit-location damage** — attacks resolve against specific cells; height makes high cells
  (turret, crew platform) and low cells (wheels, axles) distinct targets (Phase 6).
- **Footprint shape** — the grid's XY bounding box is the pathing footprint.
- **Ground pressure** — `loaded_mass / footprint_area`, gates marsh/sand/snow traversal.
- **Draft/engine power, speed caps, durability/stress** — derive from the cell bill-of-materials.

## Core Architecture

New module `src/simulation/vehicle.rs`; `cart.rs` migrates into it and is deleted in Phase 4.
Registered in `src/simulation/mod.rs` alongside the husbandry modules.

ECS surface (components unless noted):
- `Vehicle { owner_faction, design_id, purpose, heading, state, anchor_tile, z }`
- `VehicleDesign { name, grid, allowed_purpose, tech_gates, author_faction, revision }` —
  stored in the `VehicleDesignRegistry` resource, not per-entity.
- `VehicleGrid` — `Vec<(IVec3, VehicleCell)>` over a bounded 3D grid (cap ~6 wide × 4 deep ×
  4 tall; v1 carts are 1 cell tall, tanks 2, siege engines 3–4). One grid Z-cell = one world
  Z-level for clearance.
- `VehicleCell { kind: VehiclePartKind, material: ResourceId, durability: u16 }`
- `VehiclePartKind` — `Frame, Deck, Wall, Axle, Wheel, Hitch, Yoke, CargoBay, CrewSeat,
  WeaponMount` + reserved `Engine, Track, ArmorPlate, Turret`.
- `VehicleStats` (cached on entity) — `empty_mass, max_payload, loaded_mass,
  draft_power_needed, wheelbase, track_width, ground_pressure, turn_radius, road_speed_cap,
  offroad_speed_cap, height_z, center_of_mass: Vec3, stability, stress_margin`.
- `VehicleInventory` — generalised `CartInventory` (add/take/qty_of/total_qty API reused).
- `VehicleCrew { driver, passengers, gunners }`
- `VehicleDraft { hitched, required_animals, species_mask }`
- `VehicleHealth` — per-cell health mirror + `disabled: VehicleDisableFlags` + `cargo_spill_threshold`.
- `VehicleFootprint { offsets_by_heading: [Vec<IVec2>; 4], height_z: u8 }`
- `VehicleState` — includes `Overturned` (rollover result).
- `VehicleOccupancyIndex` (resource) — `tile -> Entity`, 2D (a footprint tile is exclusive
  regardless of Z).

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
- `validate_design(grid) -> Result<(), Vec<DesignError>>` — deterministic, pre-queue, **3D**:
  one connected body (6-neighbour 3D); every non-bottom cell rests on a cell below or is a
  Turret/ArmorPlate adjacent to a supporting cell (no floating cells); every Wheel linked to
  an Axle; axles support loaded mass; ≥1 control cell; draft vehicles need Hitch/Yoke matching
  animal count; CargoBay cells reachable from a deck/side cell; chariot rule.
- `derive_stats(grid, materials)` — `empty_mass = Σ cell mass`; `center_of_mass = Σ(cell_pos ×
  cell_mass) / empty_mass`; `height_z = grid Z-extent`; `stability = track_width /
  max(center_of_mass.z, ε)`; `support_limit = axle+wheel+frame strength`; `max_payload =
  min(cargo volume, support_limit − empty_mass)`; `draft_power_needed = loaded_mass ×
  terrain_resistance / wheel_efficiency`; `speed caps = min(draft/engine, wheel, terrain)`;
  `turn_radius = wheelbase / steering_factor`; `ground_pressure = loaded_mass / footprint_area`.
- **Tests:** validation rejects disconnected bodies, floating cells, unsupported wheels,
  missing driver, bad hitch, overloaded axles, blocked cargo; material swaps move mass/
  durability/payload/speed; a tall narrow design reports low `stability`, a wide low one
  high; footprint rotation maps occupied cells for all 4 headings.

## Phase 2 — VehicleYard, assembly orders, stock templates, queue UI

**Files:** `src/simulation/construction.rs`, `src/simulation/husbandry.rs`,
`src/simulation/vehicle.rs`, `src/simulation/typed_task.rs`, `src/ui/orders.rs`.

- `BuildSiteKind::VehicleYard` — ~12 wood + 6 stone, gated `ANIMAL_HUSBANDRY`; tile-indexed
  `VehicleYardMap` on the `PenMap` on_add/on_remove hook pattern. Assembly + parking anchor.
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

## Phase 3 — Occupancy, clearance-aware pathfinding, movement, rollover

**Files:** `src/pathfinding/` (new `vehicle_path.rs`; extend `astar.rs` / `step.rs`),
`src/world/chunk.rs` (clearance helper), `src/world/spatial.rs`, `src/simulation/movement.rs`,
`src/simulation/vehicle.rs`, `src/simulation/tasks.rs`.

- `vertical_clearance_at(chunk_map, x, y) -> i32` in `src/world/chunk.rs` — counts open
  `TileKind::Air` Z-levels above `surface_z_at(x, y)` until the first solid tile. The existing
  `Air`-above-floor model already represents ceilings; this is a ~6-line loop.
- `VehicleOccupancyIndex` — `tile -> Entity`; sync system (Sequential, after movement, before
  vision/combat) mirrors `sync_indexed_after_move_system`; `on_remove` hook clears on despawn.
- `footprint_astar` (`vehicle_path.rs`) — node `(anchor_x, anchor_y, z, heading)`. Transitions:
  forward, diagonal/side step, turn-in-place, turn-while-moving (gated on `turn_radius`). Each
  successor tests the full `VehicleFootprint` rotated to the candidate heading against
  `passable_step_3d` per footprint tile, `WallMap` / `StructureIndex` / `BlueprintMap`, doors,
  `VehicleOccupancyIndex`, z-step limits, **and `min(vertical_clearance_at) over footprint
  tiles >= height_z`**. Dedicated `AStarPool` slot (index 3). Deterministic tie-breaks.
- Vehicle passability: Road/Bridge/Dam best; grass/scrub/soil/cropland penalised; sand/snow/
  forest/marsh gated on `ground_pressure`/traction; water/river/wall/ore/air/occupied blocked.
- Movement: vehicle owns a `VehiclePathFollow`, authoritative on the `Vehicle` entity.
  Driver/passengers/hitched animals/cargo visuals sync to vehicle-relative offsets each tick
  (generalises `cart_follow_system`, inverted — vehicle leads).
- **Rollover (`vehicle_rollover_system`, Sequential, after vehicle movement)** — accumulates
  tip-torque from this tick's move: turn sharper than `turn_radius` allows, terrain Z-slope
  on the step, rough terrain (marsh/sand/scrub), and overload (`loaded_mass > max_payload`).
  When torque exceeds `stability`, the vehicle enters `VehicleState::Overturned`: sets
  `VehicleDisableFlags::movement`, ejects crew (`BoardedVehicle` removed; people placed on
  adjacent passable tiles, fall damage scaled by `height_z`), spills `VehicleInventory` as
  `GroundItem`s, releases hitched animals. Recovery = a labor task to right it
  (`AssembleVehicle`-style), or it is wrecked if durability is already low. Tuning floor: a
  loaded ox cart on a road never tips — only tall/narrow or overloaded/off-road designs do.
- `assign_task_with_routing` gains `Option<&VehicleFootprint>`; `Some` routes via
  `footprint_astar`, `None` is unchanged single-tile behaviour.
- LOD: a vehicle whose driver is Dormant holds/parks; `cohort.rs` pins a vehicle with an
  in-flight order to Full via `PinnedFullSim`. Stranded/abandoned vehicles re-park.
- **Tests:** pathing accepts wide clear roads, rejects narrow doors/walls/water/other
  vehicles/bad z; a 3-tall siege engine is rejected under a 2-clearance overhang while a
  1-tall cart passes; footprint rotation collision correct; occupancy updates on move/despawn/
  park; a tall-narrow overloaded vehicle on a slope rolls over and spills crew+cargo, a wide
  cart on a road does not.

## Phase 4 — Migrate cargo hauling; remove cart-specific code (parity)

**Files:** `src/simulation/vehicle.rs`, `cart.rs` (deleted), `construction.rs`,
`faction.rs` (`compute_faction_storage_system`), `mod.rs`.

- `VehicleCargoHaul` task replaces `Task::CartHaul` — any vehicle with cargo capacity, valid
  loading side, driver, and draft/traction. Port the `htn_cart_haul_dispatch_system` /
  `cart_haul_task_system` two-phase load/deliver logic.
- Construction `JobKind::Haul` prefers a vehicle when remaining qty + route width justify it
  (keep a `CART_HAUL_MIN_REMAINING`-style threshold + a `footprint_astar` route check).
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

- `vehicle_designer.rs` — `egui::Window`. The grid editor is 3D: a per-Z-layer view with a
  layer selector (edit one Z-slice at a time, like a stacked-floor editor). Part palette,
  material picker, live stat preview (`derive_stats`, incl. `stability` + `height_z`),
  validation results (`validate_design`), Queue button → `PlayerCommand::QueueVehicle`.
  Follow the `inspector.rs` `CollapsingHeader` + `ScrollArea` structure.
- AI designers — daily Economy system proposes designs per faction from tech/materials/
  animals/need: cargo pressure → carts/wagons; raid/military pressure + `WAR_CHARIOT` →
  chariots. Deterministic per faction/culture seed; stored in `VehicleDesignRegistry`. AI
  auto-queues conservative stock templates; player can open/edit/save/queue any proposal.
- Rendering — stock templates get hand-drawn sprites keyed by `design.sprite_key`; tall
  vehicles use a taller `custom_size` (sprite extends upward from the `BottomCenter` anchor;
  `ProjectedAnchor::Dynamic` already handles the floor projection). Freeform designs render as
  a composed multi-child sprite (one `VisualChild` per cell, XY+Z-offset placed).
- Inspector — add a "Vehicle" `CollapsingHeader` (design name, footprint, height, cargo bar,
  crew, hitched animals, speed, stability, damage, blocked-path reason, overturned flag).
- Right-click on a vehicle — move, load/unload, repair, right (un-overturn), assign crew,
  hitch/unhitch, deconstruct/salvage (each a `MenuAction` → `PlayerCommand` arm).

## Phase 6 — Chariot crew, draft, combat

**Files:** `src/simulation/vehicle.rs`, `src/simulation/combat.rs`.

- Chariots are mobile crew platforms: crew board via `BoardedVehicle`, fight through existing
  combat rules with vehicle-provided speed/protection/damage modifiers.
- Combat attacks target crew or vehicle cells; **hit-location is height-aware** — melee biases
  low cells (wheels/axles), and a taller vehicle exposes more cell surface. `VehicleHealth`
  per-cell damage: destroyed wheel/axle disables movement, destroyed CargoBay spills
  `VehicleInventory`, destroyed CrewSeat exposes occupants, destroyed Hitch releases animals.
  A destroyed low/support cell can also drop `center_of_mass` support and force a rollover
  check. Wheel/axle damage is the intended ancient counterplay.
- `WeaponMount` cells modelled and validated now; advanced ranged/turret behaviour stays
  dormant until a ranged-combat system exists.
- **Tests:** damage disables wheels/axles, spills cargo, exposes crew; crew board/unboard;
  drafted chariot move + attack; a hit that destroys a low support cell triggers rollover.

## Phase 7 — Docs

- Replace `src/simulation/CLAUDE.md` "Animal husbandry v2.1 — carts" with a "Vehicle system"
  section; update root `CLAUDE.md` if vehicle systems add `SimulationSet` entries, and note
  the new `vertical_clearance_at` helper in the Z-conventions section. Terse.

## Reused infrastructure (do not rebuild)

- `CartInventory` add/take/qty API → `VehicleInventory` verbatim.
- `HitchingPost` / `VehicleYardMap` ride the `BedMap`/`PenMap` on_add/on_remove hook pattern.
- `JobBoard` + `JobClaim` + `record_progress_filtered` + `JobEscrow` funding pipeline.
- `CraftOrder` templates `VehicleAssemblyOrder`; `HaulToCraftOrder` / `WorkOnCraftOrder`
  template the new haul/assemble tasks.
- `assign_task_with_routing`, `AStarPool`, `passable_step_3d`/`passable_diagonal_step`,
  `ChunkRouter`/`ChunkGraph`/`ChunkConnectivity`, `surface_z_at` — extended, not replaced.
- `compute_faction_storage_system` already folds `CartInventory`; swap to `VehicleInventory`.
- `Indexed` / `SpatialIndex` registration pattern for `VehicleOccupancyIndex`.
- `ProjectedAnchor::Dynamic` already projects tall sprites correctly from the floor.
- `PlayerCommandEvent` → `dispatch_player_command_system` for every vehicle order.
- `goal_dispatch_system` preserve-arms for the new task kinds (mirror `(Haul, CartHaul)`).

## Risks & mitigations

- **Pathfinding cost** — heading/turn-radius + clearance search is the heaviest new code.
  Dedicated `AStarPool` slot; vehicles are few; replan only on path-invalidation; bound node
  budget. `vertical_clearance_at` is a cheap per-tile loop, cacheable per chunk if hot.
- **Rollover over-firing** — torque thresholds tuned so a road-bound, properly-loaded vehicle
  never rolls; only tall/narrow, overloaded, or off-road-on-slope designs do. Covered by an
  explicit "wide cart does not tip" test.
- **Vehicle deadlock** — two multi-tile vehicles in a 2-wide lane. `VehicleOccupancyIndex`
  makes the other vehicle a hard block; the blocked one yields, re-plans, or re-parks.
- **Migration regression** — Phase 4 deletes `cart.rs`. Port every cart test before deletion;
  Phase 4 gated on parity tests passing.
- **Born-trapped vehicles** — `VehicleYard` placement biased to settlement edges; assembly
  validates parking footprint + a clear (clearance-aware) exit route before spawning.

## Test Plan

`cargo test --bin civgame`. Cases per phase above, plus:
- Design validation rejects disconnected bodies, floating cells, unsupported wheels, missing
  driver, invalid hitches, overloaded axles, blocked cargo access.
- Material swaps change mass/durability/payload/speed/stress.
- Footprint rotation maps occupied cells for all headings.
- Tall vehicle rejected under a low overhang; short vehicle passes.
- Rollover fires for tall/narrow/overloaded designs, never for a road-bound loaded cart.
- Occupancy index updates on move, despawn, parking, blocked reroute.
- Vehicle cargo hauling deposits into blueprints and preserves faction storage totals.
- Existing cart tests ported to vehicle equivalents.

Manual acceptance (`cargo run`, never `--sandbox`, Bronze Age start):
- Build a `VehicleYard`; assemble Handcart, Ox Cart, Wagon, Chariot from templates.
- Compose a tall multi-Z-layer custom vehicle in the designer, queue it; verify stat preview
  (incl. stability/height) & material bill match the spawned vehicle.
- Drive a multi-tile wagon down a wide road; watch it fail at a narrow choke, and a tall
  design fail at a low overhang.
- Haul wood/stone to a construction blueprint with a cargo vehicle; storage totals conserved.
- Crew a chariot with horses, run a drafted move + attack; damage a wheel → immobilised and
  cargo spills; take a tall-narrow design over a slope → rollover ejects the crew.

## Deferred (actionable)

- **Tank / siege content** — `Engine/Track/ArmorPlate/Turret` enum variants exist and
  `validate_design` has the 3D hooks; activating them needs tech-tree entries, `core.ron`
  part defs, and a ranged-combat/turret system. Write `plans/vehicle-system-tanks.md`.
- **Save-game serialization** of freeform designs — blocked on the project having no save
  layer; `VehicleDesignRegistry` is a single serializable resource when one lands.

## Assumptions

- First playable content stops at ancient vehicles; tank/siege parts are extension points.
- Vehicle pathfinding can be rarer and more expensive than person pathfinding (vehicles few).
- Road widening is not required for v1 (carved roads are already 2 tiles wide); vehicle yards
  sit near settlement edges so large vehicles are not born trapped.
