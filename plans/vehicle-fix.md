# Comprehensive Vehicle Fix Plan

## Summary
Bring vehicles up to a usable baseline across movement, crew, Test Drive, and weapons. The work will make vehicles visibly rotate, let crew board and leave cleanly, make debug driving reliable over longer sessions, and support both automatic and player-directed turret/siege fire.

## Implementation Details

### 1. Heading-Aware Vehicle Visuals
- Update vehicle rendering so spawned visuals are not one-time-only.
- Replace marker-only `VehicleVisual` with visual state, for example:
  - `design_id: VehicleDesignId`
  - `heading: u8`
- Change `spawn_vehicle_sprites` into a refresh system that:
  - builds visuals when missing
  - rebuilds when `vehicle.heading % 4` changes
  - rebuilds when `vehicle.design_id` changes
  - despawns only existing `VisualChild` children, not the vehicle entity
- Adjust `vehicle_sprite_plan_with_data` so world-spawned stock vehicles can use composed, heading-aware art when `VehicleData` is available.
- Keep the existing `VehicleSpritePlan::Stock` fallback for no-data callers/tests where stock placeholder rendering is still useful.
- Result: when `vehicle_movement_system` commits `v.heading = node.heading`, the visible body updates to match.

### 2. Manual Drive Behavior
- Keep existing controls:
  - `W` / Up: forward
  - `A` / Left: turn counter-clockwise
  - `D` / Right: turn clockwise
  - `Q`: forward-left diagonal
  - `E`: forward-right diagonal
  - `S` / Down: stop current in-flight step
  - `Esc`: release Test Drive mode
- Change movement keys from `just_pressed` to `pressed` when no `VehiclePathFollow` is active.
- Keep `S` and `Esc` as `just_pressed` so they remain deliberate one-shot actions.
- After clicking `Test Drive (debug)`, close the Vehicle Designer window so egui keyboard focus does not swallow drive keys.
- Add an active test-drive chunk focus:
  - `update_simulation_focus_system` should include active `DebugTestDriveVehicle + PlayerPiloted` vehicle transforms.
  - Use a small data-only radius around the vehicle so `ChunkMap::passable_at` and `vertical_clearance_at` keep working after it drives away from the camera-loaded area.
  - Do not auto-follow the camera in this pass.

### 3. Crew Roles, Boarding, And Disembark
- Add crew helper functions in `vehicle.rs`:
  - `vehicle_operator_capacity(design, data)`: sum `PartDef.crew_capacity` over cells, falling back to `crew_seat_count(design).max(1)` where needed.
  - `vehicle_gunner_demand(design, data)`: sum ranged module `gunner_required` plus one slot for each legacy single-cell `Turret` / ranged `WeaponMount` not owned by a module.
  - `available_weapon_operators(crew)`: `crew.gunners.len() + crew.passengers.len()`.
- Update `VehicleOrderKind::AssignCrew`:
  - only choose owner-faction, non-drafted, unboarded, idle people
  - fill `driver` first
  - fill `gunners` up to `vehicle_gunner_demand`
  - fill remaining capacity as `passengers`
  - insert `BoardedVehicle`
  - cancel/reset the selected crew member’s current action queue if needed so they do not remain logically busy elsewhere
- Add `VehicleOrderKind::DisembarkCrew`.
- Add UI label `Disembark Crew`.
- Show `Disembark Crew` only when the vehicle has any `driver`, `gunners`, or `passengers`.
- On disembark:
  - remove any in-flight `VehiclePathFollow` for player-directed movement
  - set upright vehicle state to `Parked`
  - drain `VehicleCrew`
  - remove `BoardedVehicle` from each rider
  - place each rider on nearest passable non-vehicle-footprint tile, searching outward from the vehicle anchor
  - update `Transform`, `PersonAI.current_z`, `target_tile`, and `dest_tile`
  - cancel the rider’s current queue so normal autonomy resumes
- If no landing tile exists for a rider, keep that rider boarded and log/skip rather than deleting the crew reference.

### 4. Automatic Weapon Fire
- Keep `vehicle_turret_fire_system` as the automatic opportunistic fire path.
- Fix module fire gating:
  - module weapons should count `available_weapon_operators(crew)`, not only `crew.gunners.len()`.
  - legacy single-cell `Turret` / ranged `WeaponMount` should require at least one available weapon operator.
- Extend target acquisition to support:
  - enemy `Person`
  - enemy `Vehicle` with intact `VehicleHealth`
- Keep current safety rules:
  - no same-faction targets
  - respect range
  - respect line of sight
  - respect `FiringArc::Front90`
  - full-360 modules can shoot in any direction
- Keep cooldowns:
  - modules use `def.cooldown_ticks`
  - legacy single-cell mounts use `TURRET_FIRE_COOLDOWN_TICKS`
- Projectiles should continue using `ProjectileFired`; no separate projectile system is needed.

### 5. Manual Fire Orders
- Add `VehicleOrderKind::FireAt((i32, i32))`.
- Add component:
  - `VehicleFireOrder { target_tile: (i32, i32), expires_tick: u64 }`
- UI:
  - selected player vehicle + right-click tile should show `Fire Here` when the design has any ranged module or legacy ranged weapon.
  - right-clicking a tile with hostile units/vehicles should still use the tile order; target entity IDs are not required for v1.
- Command behavior:
  - `FireAt(tile)` inserts/refreshes `VehicleFireOrder`.
  - It does not move the vehicle.
  - It does not rotate the chassis.
  - Weapons still obey range, LOS, and firing arc.
- Target priority while `VehicleFireOrder` is active:
  - first search valid hostile targets near `target_tile`
  - prefer closest to clicked tile
  - tie-break by closest to vehicle
  - if nothing valid exists until `expires_tick`, remove the order and resume normal auto-fire
- This gives the player “shoot that area” control without adding per-weapon aiming UI yet.

### 6. Siege Orders
- Keep `VehicleOrderKind::SiegeWall((i32, i32))`, but make it more complete.
- UI:
  - show `Siege Wall Here` only when the clicked tile is in `WallMap` and the selected vehicle is `design_is_siege_capable`.
  - keep `Fire Here` separate; ranged turret fire targets units/vehicles, while siege targets walls.
- Command behavior:
  - insert `SiegeOrder { target_tile }`
  - if already adjacent by vehicle footprint, let `vehicle_siege_system` start damage on cooldown
  - if not adjacent, plan a vehicle route toward the wall tile using existing `plan_vehicle_route`
  - when it arrives adjacent, `vehicle_siege_system` damages the wall
  - if the wall disappears, remove `SiegeOrder`
- Keep wall damage through `apply_wall_damage`; ram/siege strikes do not need projectile visuals in this pass.
- Current data model remains:
  - ram modules with `siege_damage > 0` are siege-capable
  - ballista/turret modules are ranged weapons, not wall breakers unless data later gives them `siege_damage`

### 7. UI And Inspector Updates
- Update vehicle right-click labels:
  - `Move Vehicle Here`
  - `Fire Here`
  - `Siege Wall Here`
  - `Load Cargo`
  - `Unload Cargo`
  - `Assign Crew`
  - `Disembark Crew`
  - `Hitch Animals`
  - `Unhitch Animals`
  - `Salvage Vehicle`
- Inspector vehicle section should show:
  - driver entity
  - gunner count
  - passenger count
  - active fire order target, if present
  - active siege order target, if present
- Keep overturned vehicle menu restricted to `Right Vehicle` and `Salvage Vehicle`.

### 8. Documentation
- Update `src/ui/CLAUDE.md`:
  - held-key Test Drive
  - `S` stop vs `Esc` release
  - new `Fire Here` and `Disembark Crew` menu actions
  - `Siege Wall Here` behavior
- Update `src/simulation/CLAUDE.md`:
  - vehicle visual refresh on heading/design
  - crew role assignment rules
  - weapon operator pool
  - manual fire orders
  - siege order route-to-adjacent behavior
  - test-drive chunk focus
- Update root `AGENTS.md` if its vehicle notes duplicate these behaviors.

## Test Plan
- **Rendering/unit tests**
  - stock/custom vehicle sprite plans differ across headings when data-aware.
  - visual refresh state detects heading change and rebuilds child visuals.
- **Manual drive tests**
  - repeated held input queues another step only after prior `VehiclePathFollow` clears.
  - `S` clears active `VehiclePathFollow`.
  - `Esc` clears `ManualDriveState.active` and removes `PlayerPiloted`.
- **Crew tests**
  - `AssignCrew` fills driver, then gunners, then passengers.
  - assigned gunners receive `BoardedVehicle`.
  - `DisembarkCrew` clears crew slots and removes `BoardedVehicle`.
  - disembarked riders land on passable non-footprint tiles.
- **Weapon tests**
  - crewed turret/module fires automatically at hostile person in range/LOS.
  - uncrewed weapon does not fire.
  - module fire accepts gunners/passengers as weapon operators.
  - `FireAt(tile)` prioritizes hostile targets near the clicked tile.
  - enemy vehicles can be projectile targets.
- **Siege tests**
  - `SiegeWall` on adjacent ram-capable vehicle damages wall after cooldown.
  - non-siege vehicle ignores/removes siege order.
  - non-adjacent siege order plans a route toward the wall, then damages once adjacent.
  - wall disappearance clears `SiegeOrder`.
- **Commands**
  - run `cargo test --bin civgame vehicle`
  - run `cargo test --bin civgame` if practical

## Assumptions
- Vehicle headings stay cardinal; diagonal manual movement does not create diagonal-facing sprites.
- Manual `Fire Here` controls target priority, not chassis rotation.
- Turret and ballista shots target units/vehicles; wall destruction stays under `SiegeWall`.
- This pass fixes usability and control without adding per-weapon UI, ammo, friendly-fire diplomacy, or autonomous raider siege behavior.
