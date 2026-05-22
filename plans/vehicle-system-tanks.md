# Tank / Siege Content Extension — Implementation Plan

Follow-up to `plans/vehicle-system.md`; replaces the earlier skeleton.

## Implementation status — SHIPPED (Phases 1-8)

All eight phases implemented; `cargo test --bin civgame` green (1012 tests).
Deviations from the plan, all deliberate:

- **Phase 7 siege** is driven by a `SiegeOrder` component on the `Vehicle`
  (set by `VehicleOrderKind::SiegeWall`), not the HTN `Task::SiegeWall`. The
  `Task`/`TaskKind::SiegeWall` variants exist but are unused — a wall is a
  static tile, so a vehicle-side standing order is cleaner than threading a
  person task through goal-dispatch preserve-arms. **Raider AI does not yet
  use siege autonomously** (the plan's `raid.rs` opportunistic hook is the
  one piece deferred).
- **Phase 6 turret fire** is a sibling system `vehicle_turret_fire_system`
  rather than an extension of `vehicle_combat_system` (param-count + the two
  iterate different things). A turret counts as "manned" when the vehicle
  carries any crew.
- **Phase 2 projectile rendering** is an inline `Sprite` on the `Projectile`
  entity moved by `projectile_system` — no separate `projectile_render_system`
  / `sprite_library` entry.
- The inspector was not extended with engine/siege fields (lowest-value item).

## Context

The vehicle subsystem (`src/simulation/vehicle.rs`, Phases 1-7 of `plans/vehicle-system.md`)
shipped ancient content (carts, wagons, chariots) on a deliberately generic architecture:
a 3D cell grid, height-aware `derive_stats`, clearance-aware `footprint_astar`, rollover,
and per-cell `vehicle_combat_system` damage all already work for tall multi-Z bodies. The
`VehiclePartKind` enum reserves `Engine, Track, ArmorPlate, Turret` but ships no `core.ron`
defs and no mechanics for them.

The goal is to activate that reserved content: siege engines (battering ram, siege tower,
ballista vehicle) and powered/armored war vehicles (armored wagon, tank). The skeleton plan
under-scoped two hard prerequisites — **the project has no ranged-combat system and walls
have no damage model** — so those are built here as explicit early phases.

### Scope decisions (locked with the user)

- **Engine = abstract "powered traction"** — no fuel resource, no new eras. All new techs
  sit in the existing `Era::BronzeAge` cap. Anachronistic but contained; user accepted this.
- **Ranged combat and wall durability are built in this plan**, as general systems usable
  beyond vehicles, then mounted on Turret/WeaponMount and consumed by siege tasks.

### Corrections to the skeleton plan

- "Iron Age+" techs / "consider whether it belongs before gunpowder" — dead. All techs are
  `BronzeAge`; both open questions are answered.
- Skeleton claims `Track` "already routes to the movement-disable branch" — misleading.
  `Track` is an unused enum variant: no part def, no `derive_stats`/`validate` handling.
  Treat track behaviour as new work (the damage match arm grouping it with Wheel/Axle is
  the only thing that exists).
- Skeleton omits: wall durability entirely, `tech_id_from_name()` registration,
  `tech_scale()` / `TECH_COUNT` bump, gunner tracking, projectile rendering, AI/UI/docs.
- A `BOW_AND_ARROW` tech **already exists** (Personal-scale) — Phase 2 reuses it for
  bow/sling weapons; no new archery tech needed.

## Ordering

Phases 1 (walls) and 2 (ranged combat) are independent — do first, may run concurrently.
Then strict: 3 → 4 → 5 → 6 → 7 → 8. Phase 3 must precede 4 (core.ron gates reference tech
names); 5 depends on 3+4; 6 on 2+5; 7 on 1+5; 8 last.

---

## Phase 1 — Wall durability

Walls take damage, can be destroyed (despawn → passable tile), survive chunk restamp.

**Files:** `src/simulation/construction.rs` (`Wall`, `WallMaterial`, `WallMap`,
`restamp_walls_on_chunk_load`), `src/simulation/combat.rs` (reuse `Health`).

- Add the existing `combat::Health` component to wall entities at spawn/restamp.
- `WallMaterial::max_hp(self) -> u8` + `damage_resist(self) -> f32` across all 5 variants
  (Palisade ~40 / WattleDaub ~60 / Stone ~140 / Mudbrick ~90 / CutStone ~240; resist
  scales with tier — CutStone roughly halves incoming damage).
- `apply_wall_damage(wall_entity, raw_damage, &WallMaterial) -> bool` — the **single**
  damage entry point; mitigated = `raw * (1 - resist)`, saturating-sub on `Health`.
- `WallDestroyed` event carrying the tile; `wall_destruction_system` (Sequential): on dead
  `Health`, despawn entity, remove `WallMap` entry, restamp tile to underlying `TileKind`,
  fire `WallDestroyed`. `restamp_walls_on_chunk_load` auto-stops once the entry is gone.
- Audit `WallMap` readers (pathfinding cache, fog, raid pathing); `WallDestroyed` must
  invalidate any cached path that crossed the tile.

**Tests:** sub-resist hits still chip HP; CutStone survives N hits Palisade dies to;
destroyed wall → entity/`WallMap`/tile all cleared + event fired; reload does not re-stamp.

---

## Phase 2 — Ranged combat (general system)

A general projectile model for archers/slingers, not vehicle-specific.

**Files:** `src/simulation/combat.rs` (`combat_system`, target acquisition),
`src/simulation/line_of_sight.rs` (reuse `has_los()`), `src/economy/item.rs`
(`WeaponStats`), `src/rendering/{entity_sprites.rs,sprite_library.rs}` (projectile sprite).

- Extend `WeaponStats` with `range: u8` (melee = 1) + `projectile_speed: f32`. Melee
  weapons keep range 1 → behaviour unchanged.
- Bow + sling crafted weapons via `economy::core_ids::weapon()` recipes, gated on the
  **existing** `BOW_AND_ARROW` tech.
- `combat_system` acquisition: when equipped weapon `range > 1`, widen the `SpatialIndex`
  query from Chebyshev≤1 to ≤range, filter by `has_los()`; melee path unchanged. Faction
  filter applies at acquisition (no friendly fire).
- `Projectile` component (`source/target/damage/origin/dest/progress/speed`).
  `fire_projectile()` spawns one instead of applying instant damage when range > 1.
- `projectile_system` (Sequential, after `combat_system`): advance progress; on arrival
  apply damage through the **existing** armor-mitigation + faction-bonus path (and the
  `vehicle_query` bodiless-target route → `apply_vehicle_cell_damage`); despawn. Handle
  target moved/died → re-aim to last tile or despawn. Ensure ranged path does **not** also
  apply melee damage (no double-hit).
- `projectile_render_system` interpolates a sprite along origin→dest.

**Tests:** archer hits a target 4 tiles away with LOS, no fire when LOS blocked; melee
weapon still uses the 3×3 path with no projectile; projectile applies armor-mitigated
damage on arrival; mid-flight target death handled; ranged hit on a bodiless `Vehicle`
routes through `apply_vehicle_cell_damage`.

**Risk:** keep range modest (R ≤ 6) and only widen acquisition for ranged-equipped units.

---

## Phase 3 — New techs (ids 47-49, all `Era::BronzeAge`)

**Files:** `src/simulation/technology.rs` (`TECH_TREE`, `TECH_COUNT`),
`src/simulation/technology_adoption.rs` (`tech_scale()`), `src/simulation/vehicle.rs`
(`tech_id_from_name()`).

New `TechDef`s appended (DAG invariant: every prereq id < own id — test-enforced):

- **47 `SIEGE_ENGINEERING`** — prereqs `WAR_CHARIOT`(37) + `MONUMENTAL_BUILDING`(41);
  trigger `{Combat, ~0.03}`; `TechBonus::ZERO`; scale `MilitaryTransport`. Gates siege
  templates + `turret` part.
- **48 `ARMOR_PLATING`** — prereqs `SCALE_ARMOR`(34) + `BRONZE_CASTING`(31); trigger
  `{Combat, ~0.03}`; `TechBonus::ZERO`; scale `MilitaryTransport`. Gates `armor_plate`
  and `track` parts + armored wagon.
- **49 `POWERED_TRACTION`** — prereqs `SIEGE_ENGINEERING`(47) + `BRONZE_CASTING`(31);
  empty triggers (institutional, like `DAM_BUILDING`); `TechBonus::ZERO`; scale
  `Institutional`. Gates the `engine` part + `tank` template.

- `TECH_COUNT` 47 → 50; keep ids dense.
- Add a `tech_scale()` match arm for each new id (omission seeds founders wrong — silent).
- Add `"siege_engineering"`, `"armor_plating"`, `"powered_traction"` →
  `tech_id_from_name()` in `vehicle.rs` (omission silently drops the gate).

**Tests:** existing DAG / `len == TECH_COUNT` tests pass; assert
`tech_id_from_name("powered_traction").is_some()`; `tech_scale()` covers the new ids.

---

## Phase 4 — `core.ron` part defs + templates

**Files:** `assets/data/vehicles/core.ron`, `src/simulation/vehicle.rs` (`PartDef` schema
+ loader).

`PartDef` schema additions, all `#[serde(default)]` so the existing 10 parts / 5 templates
deserialize unchanged:
- `engine_power_g: u32` (0) — abstract powered draft output; non-zero only on `engine`.
- `traction_pct: u32` (0) — offroad-resistance reduction; non-zero on `track`.
- `armor_durability_mult: f32` (1.0) — multiplies per-cell `VehicleHealth`; >1 on
  `armor_plate`.
- `mounted_weapon_range: u8` + `mounted_weapon_damage: u8` (0) — for `turret`/`weapon_mount`
  (Phase 6).

New parts: `engine` (kind `Engine`, heavy mass, `engine_power_g` sized to draft a mid
wagon, gate `powered_traction`); `track` (kind `Track`, moderate mass, high `traction_pct`,
gate `armor_plating`); `armor_plate` (kind `ArmorPlate`, very high mass,
`armor_durability_mult` ~2.5, cantilever-capable, gate `armor_plating`); `turret` (kind
`Turret`, `crew_capacity` 1 gunner, cantilever-capable, mounted-weapon fields, gate
`siege_engineering`).

New templates: `battering_ram` (low wheeled, WeaponMount ram head, animal-drawn,
`siege_engineering`); `siege_tower` (tall multi-Z Frame/Deck/Wall stack — exercises
height/clearance/cantilever, `siege_engineering`); `armored_wagon` (CargoBay + ArmorPlate
walls + Hitch/Yoke, `armor_plating`); `ballista_vehicle` (Frame + Turret/WeaponMount +
CrewSeat, `siege_engineering`); `tank` (Engine + Track + ArmorPlate + Turret + CrewSeat,
no Hitch/Yoke, `powered_traction`).

**Tests:** loader parses new defs; defaults apply to old defs; `tank` loads with 0 draft
animals and no Hitch/Yoke. (Full `validate_design` pass deferred to Phase 5.)

---

## Phase 5 — `derive_stats` engine branch + `validate_design`

**Files:** `src/simulation/vehicle.rs` — `derive_stats` (~754-890), `validate_grid`
(~549-714), `VehiclePartKind` classifiers.

`derive_stats`:
- Sum `engine_power_g` over `Engine` cells → `engine_power`. When `engine_power > 0`, the
  draft need is met by engine power rather than `required_animals`; an underpowered engine
  (`engine_power < draft_power_needed`) is invalid.
- Engine-driven speed caps derive from `engine_power / loaded_mass`; `wheel_quality` stays
  a secondary cap.
- Sum `traction_pct` over `Track` cells → reduce the `TERRAIN_RESISTANCE` term (raises
  offroad speed).
- `armor_durability_mult` multiplies the cell's entry when building `VehicleHealth`
  per-cell health; armor mass auto-sums into `empty_mass`. Heavy armored designs must
  still pass the existing axle/frame overload check.
- Extend the stats struct with `engine_power: u32` + `is_engine_driven: bool` for UI/AI.

`validate_grid`:
- A design with ≥1 `Engine` cell needs no Hitch/Yoke and skips the
  draft-capacity-vs-`required_animals` check.
- A design with no Engine **and** no draft control stays invalid (regression-guard
  current behaviour).
- Add `Engine` (and `Track`) to `is_structural()` so connectivity/floating checks treat
  them as load-bearing — no existing template uses these parts, so safe; re-run the full
  vehicle validation suite.

**Tests:** `tank` validates, `is_engine_driven == true`, 0 animals, speed > 0; underpowered
engine → invalid; no-engine no-Hitch → still invalid; armored wagon ArmorPlate raises
`empty_mass` + per-cell health; Track design has higher offroad speed than a wheel-only
equivalent; existing animal-drawn wagon speeds unchanged.

---

## Phase 6 — Turret/WeaponMount ranged combat + gunner

**Files:** `src/simulation/vehicle.rs` (`vehicle_combat_system`, crew sync), reuse Phase 2
`fire_projectile`/`projectile_system`.

- Gunner tracking: a `Turret` has `crew_capacity 1`; the existing `vehicle_crew_sync_system`
  already seats crew — add a `VehicleGunner` cell→Entity map so a manned turret is known.
- Extend `vehicle_combat_system` (Sequential, after `combat_system`): for each vehicle with
  a **manned** Turret/WeaponMount, acquire a target within `mounted_weapon_range` via
  `SpatialIndex` + `has_los()` (origin = the cell's world position from the 3D grid),
  fire via `fire_projectile` on a per-cell cooldown.
- An un-crewed or destroyed (`VehicleHealth` cell dead) turret does not fire.

**Tests:** crewed `ballista_vehicle` fires at an in-range enemy; un-crewed turret does not;
destroyed turret stops firing; mounted damage routes through projectile → armor mitigation.

**Risk:** projectile origin must be the turret cell's true world Z (tall vehicles); reuse
the 3D-grid→world transform that `vehicle_crew_sync_system` uses. Cooldown is per-cell.

---

## Phase 7 — Siege interaction (vehicle vs wall)

**Files:** `src/simulation/vehicle.rs` (siege system), `src/simulation/construction.rs`
(`apply_wall_damage`), task enum/executors, `src/simulation/raid.rs`.

- `derive_stats` sets `is_siege_capable: bool` + `siege_damage` from the presence of a
  ram-type `WeaponMount` (battering ram).
- New `Task::SiegeWall { target_tile }` (dedicated task — a wall is a static tile, not a
  unit; preserve the `goal_dispatch_system` preserve-arm pattern).
- `vehicle_siege_system` (Sequential): a siege-capable, crewed vehicle adjacent to a
  `WallMap` tile with `Task::SiegeWall`, on cooldown calls `apply_wall_damage`; completes
  / re-targets on `WallDestroyed`. Pathing to adjacency via existing `footprint_astar`.
- `raid.rs`: raiders currently ignore walls. Add opportunistic use — if a raiding party
  has a siege vehicle and no open path to the objective exists, issue `Task::SiegeWall`
  against the nearest blocking wall. Keep it bounded by crew presence + task completion.
- On `WallDestroyed`, invalidate vehicle path caches so vehicles re-route through the gap.

**Tests:** crewed battering ram destroys an adjacent Palisade over N ticks → tile passable;
un-crewed siege vehicle does no damage; CutStone takes proportionally longer;
`footprint_astar` finds a path through the gap after destruction; non-siege vehicles /
ranged units do not damage walls.

---

## Phase 8 — AI + UI + docs

**Files:** `src/simulation/vehicle.rs` (`vehicle_ai_design_proposal_system`,
`vehicle_ai_queue_system`), `src/ui/vehicle_designer.rs`, `src/ui/orders.rs`, inspector,
`CLAUDE.md` (root + `src/simulation/CLAUDE.md`).

- AI: `vehicle_ai_design_proposal_system` (weekly, Economy) proposes a siege template for
  a war-footing faction that knows `SIEGE_ENGINEERING`, and allows the `tank` once it
  knows `POWERED_TRACTION`. All AI proposals must pass `validate_design` before queueing.
- Designer UI: add `engine`/`track`/`armor_plate`/`turret` to the part palette with
  tech-gate lock state; surface `engine_power` / `is_engine_driven` / `is_siege_capable`.
- `orders.rs`: a `Task::SiegeWall` order from the right-click menu when a wall is targeted.
- Inspector: engine power, siege capability, gunner/turret crew, per-cell armor health.
- Docs: document the powered-traction abstraction, ranged combat, wall durability, siege
  task, and new techs/parts/templates. Note `vertical_clearance_at` already covered.

---

## Cross-cutting risks

- **`Engine`/`Track` in `is_structural()`** is the riskiest change — re-run the full
  vehicle validation + `derive_stats` suite after Phases 4-5; a regression test on an
  existing animal-drawn wagon guards the speed formula.
- **Silent failures:** `tech_scale()` and `tech_id_from_name()` omissions fail silently —
  add explicit guard tests.
- **System ordering:** `combat_system`, then `projectile_system`, `vehicle_combat_system`,
  `vehicle_siege_system`, `wall_destruction_system` — all Sequential after combat; verify
  damage applies exactly once and despawns are clean.
- **serde back-compat:** every new `PartDef` field needs `#[serde(default)]`.

## Verification

- `cargo test --bin civgame` — all phase tests above plus the existing vehicle/combat/tech
  suites green; expect the suite to grow well past the current ~953.
- Manual (`cargo run`, never `--sandbox`, Bronze Age start):
  - Build a `VehicleYard`; assemble a battering ram and a siege tower from templates.
  - Equip a unit with a bow; confirm it fires a projectile at a target several tiles away
    and misses behind a wall.
  - Drive a crewed battering ram to an enemy palisade, run `SiegeWall`, watch the wall
    fall and the gap open for pathing.
  - Compose a `tank` in the designer; confirm it validates with 0 draft animals, shows
    `is_engine_driven`, and a manned turret fires at enemies.

## Critical files

- `src/simulation/vehicle.rs` — parts, `derive_stats`, `validate_grid`,
  `vehicle_combat_system`, siege system, AI.
- `src/simulation/combat.rs` — ranged combat, projectiles.
- `src/simulation/construction.rs` — wall durability.
- `src/simulation/technology.rs` + `technology_adoption.rs` — new techs.
- `assets/data/vehicles/core.ron` — parts + templates.
