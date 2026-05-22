# Vehicle Designer V2: Large-Scale Builder + Multi-Cell Weapons

## Summary
- Expand custom vehicle design from `6×4×4` to `10×8×6`.
- Rework the designer into a larger workbench UI with hover explanations for every part, material, stat, validation issue, and cell.
- Add data-backed part variants so “wheel,” “axle,” “frame,” and “weapon mount” choices have distinct behavior beyond material.
- Add multi-cell weapon modules for rams, ballistae, and turrets. Large weapons fire/strike once per module, not once per occupied cell.
- Preserve existing stock/custom designs by defaulting missing variants/modules to legacy-compatible values.

## Data Model
- Update grid bounds in `src/simulation/vehicle.rs`:
  - `GRID_MAX_WIDTH = 10`
  - `GRID_MAX_DEPTH = 8`
  - `GRID_MAX_HEIGHT = 6`
- Extend `VehicleCell`:
  - `variant: VehiclePartVariantId`
  - `module_id: Option<VehicleModuleId>`
- Add `VehicleGrid.modules: Vec<VehicleModuleInstance>` while keeping `cells` as the authoritative occupied-cell list for pathing, mass, footprint, health, and rendering.
- Add `PartVariantDef` loaded from `assets/data/vehicles/core.ron`:
  - `id`, `part_kind`, `label`, `description`
  - optional multipliers/additions for mass, support, traction, turn radius, cargo volume, durability, engine power, tool cost, and tech gates.
- Add `VehicleModuleDef` loaded from RON:
  - `id`, `label`, `description`, `part_kind`, `footprint`, `allowed_rotations`
  - `crew_required`, `gunner_required`, `firing_arc`, `range`, `damage`, `siege_damage`, `cooldown_ticks`, `tech_gates`
  - `required_support`: how many occupied cells need direct support from below.
- Existing `CellDef` gains optional `variant` and `module_id`; old entries default to `standard_<part>` and no module.
- Custom designs derive `tech_gates` from every placed cell variant and module instead of registering with `Vec::new()`.

## Part Variants
- Frame variants:
  - `light_chassis`: lower mass, lower support.
  - `heavy_chassis`: higher mass, higher support and stress margin.
  - `truss_chassis`: medium mass, strong support for tall siege builds.
- Wheel variants:
  - `solid_wheel`: default, durable, average speed.
  - `spoked_wheel`: lighter/faster, less durable.
  - `iron_rim_wheel`: higher traction/durability, metal tech gate.
  - `large_offroad_wheel`: better off-road, wider turn radius.
- Axle variants:
  - `fixed_axle`: default.
  - `reinforced_axle`: higher support, heavier.
  - `steering_axle`: reduces turn radius, modest support.
- Track variants:
  - `wooden_track`: early armored/siege traction.
  - `metal_track`: better support and off-road traction, heavier.
- Weapon variants:
  - `weapon_platform`: 1-cell legacy mount, no siege damage unless paired with a ranged module.
  - `ram_head`: siege-only, must be part of a multi-cell ram module.
  - `fixed_ballista`: ranged, must be part of a multi-cell forward-facing module.
- Turret variants:
  - `light_turret_ring`: 2×2 module, 360-degree firing arc, one gunner.
  - `heavy_turret_ring`: 3×3 module, 360-degree firing arc, two crew/gunner capacity, higher damage and mass.

## Multi-Cell Weapon Modules
- Add module placement mode in the designer: pick a module, rotate it, then click an anchor cell to stamp all occupied cells.
- Module definitions for this pass:
  - `ram_head_1x2`: 1×2 forward-facing `WeaponMount`, siege damage only, must sit on the vehicle’s front edge.
  - `battering_ram_2x3`: 2×3 forward-facing module, includes ram head plus bracing footprint, siege damage, no ranged fire.
  - `ballista_2x2`: 2×2 forward-facing `WeaponMount`, range/damage, 90-degree front arc, one gunner required.
  - `light_turret_2x2`: 2×2 `Turret`, range/damage, 360-degree arc, one gunner required.
  - `heavy_turret_3x3`: 3×3 `Turret`, higher range/damage, 360-degree arc, two crew/gunners required.
- Validation rules:
  - Module footprint must be inside `10×8×6`.
  - Module cells cannot overlap unrelated occupied cells.
  - All module cells must remain connected to the vehicle body.
  - Large turrets need required support underneath unless placed on the bottom layer.
  - Forward-facing modules must have a valid module facing and a clear front edge.
  - A module is operational only if its required cells still have health.
- Combat/siege behavior:
  - `vehicle_turret_fire_system` iterates modules first, then falls back to legacy single-cell mounts.
  - Cooldowns are keyed by `(vehicle_entity, module_id)`.
  - Multi-cell ballista/turret fires one projectile per cooldown, not one per cell.
  - Projectile origin uses the rotated module muzzle offset.
  - `vehicle_siege_system` uses module `siege_damage`; generic `WeaponMount` no longer makes a design siege-capable by itself.
  - Destroying any required module cell disables that module; destroying support cells may still trigger rollover through existing health logic.

## Designer UI
- Rebuild `src/ui/vehicle_designer.rs` as a three-column workbench:
  - Left: part/module palette, variant picker, material picker.
  - Center: large `10×8` Z-slice canvas with stable square cells.
  - Right: live stats, validation, selected-cell details, and material bill.
- Add Z tabs `0..5` with per-layer cell counts.
- Add module rotation controls and a footprint preview before placement.
- Left-click behavior:
  - Cell mode places one part cell.
  - Module mode stamps the full module footprint.
- Right-click clears:
  - A normal cell clears just that cell.
  - A module cell clears the whole module after removing its grouped cells.
- Hover explanations:
  - Part buttons explain gameplay meaning, not just labels.
  - Variant buttons explain the tradeoff and tech gate.
  - Material picker explains density, strength, traction, durability, and why material matters.
  - Cell hover shows part, variant, material, module membership, durability, mass, support, cargo/power/weapon contribution, and validation issues.
  - Stat hover explains deck vs cargo, frame support, axle support, track width, wheelbase, stability, stress margin, engine power, siege damage, and mounted range.
- Show a compact “What this vehicle can do” summary:
  - Cargo capacity, crew seats, gunner seats, draft/engine driven, siege-capable, mounted ranged weapons, max height/clearance, rollover risk.

## Templates And Content
- Update stock templates to use explicit variants/modules:
  - Handcart/Ox Cart/Four-Wheel Wagon use basic frame/axle/wheel variants.
  - War Chariot uses `weapon_platform` or a small ranged mount, not siege damage.
  - Battering Ram uses `battering_ram_2x3`.
  - Ballista Vehicle becomes wide enough for `ballista_2x2`.
  - Tank uses tracks, engine, armor plate, and `light_turret_2x2`.
  - Add an optional `Heavy Tank` or `Heavy Turret Test Rig` using `heavy_turret_3x3` to exercise the large module path.
- Update `design_bill`:
  - Counts every occupied cell material.
  - Adds tool/weapon costs from variants/modules.
  - Large weapons can require extra `weapon`, `tools`, or metal resources via module cost fields.
- Update docs in `src/ui/CLAUDE.md`, `src/simulation/CLAUDE.md`, and `assets/data/vehicles/core.ron` comments.

## Tests
- Data loading:
  - Variant and module definitions parse.
  - Old templates without variants/modules still load through defaults.
  - New stock templates validate under `10×8×6`.
- Validation:
  - Reject module out of bounds, overlaps, unsupported heavy turret, disconnected module, and invalid forward-facing module placement.
  - Reject underpowered engine designs after the larger grid expansion.
- Stats:
  - Wheel/axle/frame variants change mass, support, speed, turn radius, and stress margin.
  - Tracks improve off-road behavior.
  - Heavy turret raises mass and center of mass enough to affect stability.
- Combat/siege:
  - `ballista_2x2` fires one projectile per cooldown.
  - `light_turret_2x2` fires 360 degrees when crewed.
  - `heavy_turret_3x3` disables when a required cell is destroyed.
  - `War Chariot` is not siege-capable by default.
  - `Battering Ram` damages walls through module siege damage.
- UI helpers:
  - Every part, variant, module, material, stat, and validation error has a non-empty hover explanation.
- Verification commands:
  - `cargo check`
  - `cargo test --bin civgame vehicle -- --quiet`
  - `cargo test --bin civgame`

## Assumptions
- No new crates.
- The sparse cell grid remains the single source for footprint, mass, durability, pathing, rendering, and health.
- `VehicleGrid.modules` is metadata for grouped behavior and UI editing, not a second occupancy model.
- Single-cell mounts remain supported for light weapons and backward compatibility; large weapons and turrets use module definitions.
- The designer may expose tech-locked parts/modules, but clearly marks missing tech and prevents queueing or assembly when gates are not met.
