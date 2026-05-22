# Vehicle System — Tank / Siege Content (deferred extension)

Follow-up to `plans/vehicle-system.md`. The vehicle subsystem (`src/simulation/vehicle.rs`)
shipped ancient-first content (carts, wagons, chariots) but was built generic: the 3D cell
grid, `validate_design`'s 3D hooks, `derive_stats`, clearance-aware pathing, rollover, and
per-cell combat damage all already work for tall multi-Z bodies. Tank / siege content is
**content + two new systems**, not an architecture change.

## What already exists (do not rebuild)

- `VehiclePartKind::{Engine, Track, ArmorPlate, Turret}` — enum variants defined; `is_structural`
  / `may_cantilever` / `is_control` already classify them. No `core.ron` part defs ship yet.
- `validate_design` 3D rules (connectivity, floating cells, cantilever for Turret/ArmorPlate).
- `derive_stats` — mass / 3D COM / `stability` / `height_z` / clearance all height-aware.
- `footprint_astar` clearance gate — a tall vehicle already fails at low overhangs.
- `vehicle_combat_system` + `VehicleHealth` — per-cell hit-location damage; `Track` already
  routes to the movement-disable branch alongside `Wheel`/`Axle`.

## Scope

1. **Tech tree** — new techs (Iron Age+): e.g. `SIEGE_ENGINEERING`, `BRONZE_ARMOR` / later
   `IRON_PLATE`, and a powered-traction tech for `Engine`. Add to `technology.rs` (prereq DAG,
   `TechTrigger`s, era). Gate the new `core.ron` parts + templates on them.
2. **`core.ron` part defs** — `engine` (draft-power source, replaces Hitch/Yoke draft need —
   `derive_stats` must learn an engine-power branch), `track` (traction part, pairs with the
   existing wheel/axle support maths), `armor_plate` (high mass + high durability, cantilever
   cell), `turret` (crew/weapon platform, cantilever cell). Plus stock templates: a battering
   ram, a siege tower, a simple armored wagon.
3. **`derive_stats` engine branch** — when a design has `Engine` cells, `draft_power_needed`
   is met by engine power instead of `required_animals`; speed caps derive from engine power.
   `validate_design` — an `Engine` design needs no Hitch/Yoke; a no-engine no-draft design
   stays invalid.
4. **Ranged / turret combat** — the real new system. `WeaponMount` / `Turret` cells need a
   ranged-attack model: a vehicle-mounted weapon picks a target in range and fires. This is
   blocked on the project having **no ranged-combat system at all** — build that first (it is
   also wanted for archers/slingers generally), then mount it on `Turret`/`WeaponMount`.
5. **Siege interaction** — a battering ram / siege tower vs. `WallMap` walls: a dedicated
   `Task`/executor that lets a crewed siege engine damage a wall structure.

## Open questions

- Does `Engine` need a fuel resource, or is it abstract “powered traction”? (Ancient/classical
  has no real engines — this is genuinely a later-era extension; consider whether it belongs
  before gunpowder at all.)
- Ranged combat is a large prerequisite — siege content should probably wait until a general
  ranged-combat system lands, then this plan is mostly content (`core.ron` + techs).

## Entry points

- `src/simulation/vehicle.rs` — `VehiclePartKind`, `derive_stats`, `validate_grid`,
  `vehicle_combat_system`, `apply_vehicle_cell_damage`.
- `assets/data/vehicles/core.ron` — parts + templates.
- `src/simulation/technology.rs` — tech tree.
- `src/simulation/combat.rs` — ranged-combat prerequisite.
