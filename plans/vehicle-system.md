# Customizable Vehicle System

## Summary
Build a unified `Vehicle` subsystem that replaces the current hardcoded cart path with freeform, grid-designed vehicles. The first playable content stays ancient-first: handcarts, ox carts, wagons, and chariots. The data model deliberately reserves engine, track, armor, and turret concepts so later tanks use the same design, assembly, movement, damage, and UI systems.

Defaults locked from planning:
- Player gets both AI-proposed templates and a freeform grid designer.
- Runtime uses deterministic physics/stat simulation, not a continuous rigid-body engine.
- Vehicles occupy true multi-tile world footprints.
- No new crates.

## Core Architecture
Add a new `src/simulation/vehicle.rs` module and migrate `src/simulation/cart.rs` into it as the first compatibility target.

Define the main ECS/runtime surface:
- `Vehicle { owner_faction, design_id, purpose, heading, state, anchor_tile, target_z }`
- `VehicleDesign { name, grid, allowed_purpose, tech_gates, author_faction, revision }`
- `VehicleCell { kind, material, durability }`
- `VehiclePartKind`: `Frame`, `Deck`, `Wall`, `Axle`, `Wheel`, `Hitch`, `Yoke`, `CargoBay`, `CrewSeat`, `WeaponMount`, with reserved future variants `Engine`, `Track`, `ArmorPlate`, `Turret`.
- `VehicleStats`: empty mass, max payload, loaded mass, draft power needed, wheelbase, track width, ground pressure, turn radius, road/offroad speed caps, stability, stress margin.
- `VehicleInventory`: generalized replacement for `CartInventory`.
- `VehicleCrew`: driver/passenger/gunner slots.
- `VehicleDraft`: hitched animals and draft requirements.
- `VehicleHealth`: per-cell health, disabled flags, cargo spill threshold.
- `VehicleFootprint`: occupied world-tile offsets for each heading.
- `VehicleOccupancyIndex`: tile-to-vehicle map for collision/pathing.

Keep existing `Cart` behavior as a migration shim only long enough to prove cargo hauling parity, then remove cart-specific assembly/haul code.

## Design, Materials, And Assembly
Add a data-driven vehicle catalog under `assets/data/vehicles/core.ron`:
- Material profiles keyed by existing `ResourceId`: wood, stone, skin/leather, copper, bronze/iron where available.
- Part definitions with mass, strength, friction, traction, axle load, wheel radius, durability, crew/cargo volume, and tech gates.
- Stock templates: Handcart, Ox Cart, Four-Wheel Wagon, Light Chariot, War Chariot.

Use raw resources plus tools for custom designs instead of exploding the resource catalog into one resource per possible part/material combination. Assembly cost is computed from grid cells into a `ResourceId -> qty` map.

Add a `VehicleYard` buildable structure:
- Gated on `ANIMAL_HUSBANDRY`.
- Acts as the assembly/parking anchor for multi-tile vehicles.
- Existing `HitchingPost` becomes a small-vehicle tether point, with `parked_vehicle: Option<Entity>` replacing `parked_cart`.

Add `VehicleAssemblyOrder`:
- Arbitrary input deposits, not limited by `Blueprint::MAX_BUILD_INPUTS`.
- Posted by player or AI.
- Uses new typed tasks `HaulToVehicleOrder` and `AssembleVehicle`.
- On completion, validates the parking footprint, spawns a `Vehicle`, registers occupancy, and stamps the design snapshot.

## Physics And Validation
Implement deterministic design validation before an order can be queued:
- Body cells must form one connected structure.
- Every wheel must be linked to an axle or suspension-equivalent cell.
- Axles must have enough wheels and support the loaded mass.
- At least one driver/control slot is required.
- Draft vehicles need a hitch/yoke compatible with required animal count.
- Cargo cells must be reachable from a side or deck cell.
- Chariots require light frame, spoked wheels, crew platform, and horse-compatible hitch.
- Future tanks use the same checks with engine/track/armor/turret gates.

Compute stats from the grid:
- `empty_mass = sum(cell material/part mass)`.
- `support_limit = axle + wheel + frame strength`.
- `payload_capacity = min(cargo volume, support_limit - empty_mass)`.
- `draft_power_needed = loaded_mass * terrain_resistance / wheel_efficiency`.
- `max_speed = min(draft/engine speed, wheel speed, chassis stress cap, terrain cap)`.
- `turn_radius = wheelbase / steering factor`.
- `stability = track_width / center_of_mass_height`.
- `stress` accumulates on rough terrain, overload, sharp turns, and combat hits.

Runtime damage is per-cell. Destroyed wheels/axles disable movement; destroyed cargo cells spill inventory; destroyed crew cells expose occupants; destroyed hitch releases animals.

## Movement, Pathing, And Occupancy
Add vehicle-specific pathfinding instead of forcing multi-tile vehicles through the single-agent `PathFollow` path:
- `VehiclePathRequestQueue`, `VehiclePathFollow`, and `footprint_astar`.
- Search node is `(anchor_x, anchor_y, z, heading)`.
- Transitions include forward, diagonal/side step where valid, and turn-in-place or turn-while-moving depending on `turn_radius`.
- Each transition tests the full `VehicleFootprint` against terrain, structure maps, doors, blueprints, other vehicles, and z-step limits.

Vehicle passability rules:
- `Road`, `Bridge`, and `Dam` are best.
- Grass, scrub, soil, and cropland are allowed with penalties.
- Sand, snow, forest, and marsh are allowed only if stats permit the ground pressure/traction.
- Water, river, wall, ore, air, and occupied structure tiles are blocked.
- Wider vehicles need actual footprint clearance; if a village lane is too narrow, pathing fails cleanly.

Vehicle movement is authoritative on the vehicle entity:
- Driver, passengers, hitched animals, and cargo visuals sync to vehicle-relative offsets.
- People get `BoardedVehicle { vehicle, slot }`.
- Animals get `VehicleHitched { vehicle, hitch_index }` and still use `AnimalWorkClaim`.
- `VehicleOccupancyIndex` updates after movement and before vision/combat.

## Gameplay Integration
Replace current cart hauling with generalized vehicle cargo hauling:
- `VehicleCargoHaul` uses any vehicle with cargo capacity, valid loading side, driver, and draft/traction.
- Existing construction `JobKind::Haul` prefers vehicles when remaining quantity and route width justify it.
- `compute_faction_storage_system` folds `VehicleInventory` into faction totals, replacing the cart-specific pass.
- Small loads still use hand carry.

AI vehicle designers:
- Daily Economy system proposes designs per faction from tech, materials, animals, and need.
- Cargo pressure proposes carts/wagons.
- Raid/military pressure plus `WAR_CHARIOT` proposes chariots.
- Designs are deterministic per faction/culture seed and stored in a `VehicleDesignRegistry`.
- AI auto-queues conservative templates; player can open, edit, save, and queue any proposal.

Player UI:
- Add `ui/vehicle_designer.rs`.
- Egui panel has template/proposal list, grid editor, part palette, material picker, stat preview, validation results, and queue button.
- Right-click/selection support adds vehicle move, load/unload, repair, assign crew, hitch/unhitch, and deconstruct/salvage commands.
- Inspector/hover shows design name, footprint, cargo, crew, hitched animals, speed, damage, and blocked-path reason.

Combat:
- Chariots act as mobile crew platforms first.
- Crew can fight through existing combat rules with vehicle-provided speed, protection, and damage modifiers.
- Weapon mounts are modeled now but advanced ranged/turret behavior stays dormant until ranged combat exists.
- Attacks can target either crew or vehicle cells; wheel/axle damage is the main ancient counterplay.

## Implementation Phases
1. Add vehicle data model, design registry, grid validation, stat derivation, and unit tests.
2. Add `VehicleYard`, assembly orders, UI queueing, and stock template generation.
3. Add multi-tile occupancy and vehicle footprint pathfinding.
4. Migrate current carts to `VehicleCargoHaul`; keep behavior parity with existing construction hauling.
5. Add freeform designer UI and AI proposal generation.
6. Add chariot crew/draft/combat behavior.
7. Remove cart-specific components/systems once tests prove parity.
8. Update `AGENTS.md` and subsystem notes for vehicle architecture, commands, and extension rules.

## Test Plan
Use `cargo test --bin civgame`.

Test cases:
- Design validation rejects disconnected bodies, unsupported wheels, missing driver slot, invalid hitches, overloaded axles, and blocked cargo access.
- Material swaps change mass, durability, payload, speed, stress, and flammability as expected.
- Footprint rotation maps occupied cells correctly for all headings.
- Vehicle pathing accepts wide clear roads and rejects narrow doors, walls, water, other vehicles, and bad z transitions.
- Occupancy index updates on move, despawn, parking, and blocked reroute.
- Assembly consumes exact materials and spawns the expected design snapshot.
- Vehicle cargo hauling deposits into blueprints and preserves faction storage totals.
- Draft animals are claimed, synced, released, and recovered on abort/death.
- Damage disables wheels/axles, spills cargo, and exposes crew.
- Existing cart tests are ported to vehicle equivalents.

Manual acceptance:
- In Bronze Age sandbox, build a handcart, ox cart, wagon, and chariot from templates.
- Create a custom freeform vehicle, queue it, and verify stats/material costs match the design.
- Move a multi-tile wagon through a wide path and watch it fail cleanly at a narrow choke.
- Use a cargo vehicle to haul wood/stone to a construction blueprint.
- Use a chariot with crew and horses in a drafted move/attack scenario.

## Assumptions
- First playable content stops at ancient vehicles; tank parts are modeled as future extension points, not active tech/content.
- Freeform vehicle designs are runtime data for now; persistent save-game serialization is deferred unless the project already has a save layer added later.
- Vehicle pathfinding can be rarer and more expensive than person pathfinding because vehicles are few.
- Road widening is not required for v1, but vehicle yards should be placed near settlement edges so large vehicles are not born trapped inside dense villages.
