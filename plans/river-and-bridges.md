# River-Aware Settlement & Bridge Construction

## Context
World gen produces meandering rivers (`TileKind::River`, impassable) and a cached `chunk_map.river_distance_at` index. Outer settlement-spawn scoring already rewards "near a river" (`person.rs:359-364`), but the **inner radial seeders** (`paleolithic_hearth_positions`, nomadic camp ring, organic plot jitter) use deterministic angle offsets with only `is_floor`/passability checks — so hearths, bedrolls, and farmsteads can land in rivers or straddle banks. There is also no way to cross a river: roads refuse River tiles, pathfinding treats River as impassable, and no construction path mutates non-Wall tiles. This plan delivers (1) river-aware early settlement layout and (2) a real `Bridge` tile gated on a new `BRIDGE_BUILDING` Chalcolithic tech, with player and AI build paths and proper deconstruction.

This is the v2 plan — closes earlier gaps: defines the new tile-replacing finalize/deconstruct path explicitly, names the worker-adjacency mechanic, reuses the bounded-BFS pattern for same-bank checks, and locates AI bridge-intent generation in the organic planner.

---

## Phase 1 — `TileKind::Bridge` + pathfinding

### 1.1 Tile variant and helpers (`src/world/tile.rs`)
- Add `Bridge = 24` to `TileKind`.
- Helpers: `is_passable = true`, `is_floor = true`, `is_water_like = true` (water still flows underneath, keeps `nomad.rs:2939` water-search & herd water-seek unchanged), `is_freshwater = true`, `is_drinkable_candidate = true`, `is_stone_like/is_soil_like = false`.
- Audit callers conflating "water-like" with "impassable": switch to `is_passable` or `matches!(k, River | Water)` where they actually want impassable water.
- `road_carve_system` (`construction.rs:3559-3564`): explicitly reject `Bridge` (already would skip; making intent explicit).

### 1.2 Rendering (`src/rendering/color_map.rs`, `src/world/chunk_streaming.rs`)
- `shaded_tile_color(Bridge, z)`: warm timber, e.g. `srgb(0.55, 0.38, 0.22)`.
- `TileMaterials` (`chunk_streaming.rs:168`) handles new keys on first lookup.
- `refresh_changed_tiles_system` already rebuilds sprites on `TileChangedEvent`.

### 1.3 Pathfinding (`src/pathfinding/tile_cost.rs:6`)
- `tile_speed_multiplier(Bridge) = 1.4` (Road-speed).
- Graph rebuild already event-driven on `TileChangedEvent`.

### 1.4 Tests
- `Bridge` passable, floor, water-like, freshwater, drinkable, road-speed.
- A* succeeds across a placed bridge over a 1-tile river; fails after removal.

---

## Phase 2 — `BRIDGE_BUILDING` technology

### 2.1 Tech def (`src/simulation/technology.rs`)
- `pub const BRIDGE_BUILDING: TechId = TechId(N)`.
- `TechDef { era: Chalcolithic, prerequisites: &[PERM_SETTLEMENT, DUGOUT_CANOE, COPPER_TOOLS], name: "Bridge Building", triggers: &[], bonus: None, ... }`.

### 2.2 Adoption scale (`src/simulation/technology_adoption.rs:112-114`)
- Map `BRIDGE_BUILDING` → `AdoptionScale::Institutional`.

### 2.3 Tests
- Tech reachable via prereq chain; faction with all three prereqs adopts it.

---

## Phase 3 — Bridge recipe + civic gate

### 3.1 `BuildSiteKind::Bridge` (`src/simulation/construction.rs`)
- Add variant. Recipe:
  ```
  Bridge => BuildRecipe {
      name: "Timber Bridge",
      inputs: &[(WOOD, 4), (STONE, 2)],
      work_ticks: 120,
      tech_gate: Some(BRIDGE_BUILDING),
      deconstruct_refund: Some(0.5),
  }
  ```

### 3.2 Civic milestone (`src/simulation/civic_milestones.rs`)
- `CivicKind::Bridge` threshold `(Chalcolithic, 20)` peak population (lower than Market's 40 — bridges are utility).
- Consulted at blueprint finalize via existing `civic_milestone_allows`.

### 3.3 Player UI gate (`src/ui/orders.rs:320-330`)
- `Build Bridge` appears only when right-clicked tile is `TileKind::River` and `faction_can_build(Bridge, &player_techs)`.
- Greyed-out reasons: "Requires Bridge Building" (tech) or "Settlement too small" (civic).

### 3.4 Tests
- Recipe gate `Some(BRIDGE_BUILDING)`; River+tech → menu; River no-tech → no menu; Water → no menu; Grass → no menu.

---

## Phase 4 — Tile-replacing finalize & deconstruction (NEW PATH)

`Wall` is currently the only blueprint that mutates `TileKind`. `Bridge` is the second case. Extend, don't refactor.

### 4.1 Blueprint anchor on impassable tiles (`src/simulation/construction.rs`)
- `Blueprint::new()` for `Bridge` accepts an anchor at `River`. Add `BuildSiteKind::is_water_anchored()` predicate that bypasses passability checks for `Bridge` only.
- Add `restore_tile: Option<TileKind>` to `Blueprint`. `Bridge` → `Some(River)`; others → `None`.

### 4.2 Worker adjacency (`work_stand_for`)
- New `work_stand_for(blueprint, worker) -> Option<(i32, i32)>`:
  - `Bridge`: chebyshev-1 neighbor that is passable and not another Bridge blueprint; prefer cardinals, lowest A* cost from worker.
  - Else: anchor itself.
- Gather/build dispatchers use `work_stand_for`. Build completes when worker accumulates `work_ticks` while *adjacent* to a Bridge blueprint.
- If no adjacent stand exists, abandon after 1-day stall; refund inputs as `GroundItem`s on the chosen bank.

### 4.3 Finalize (`plan_finished_blueprint`)
- For `BuildSiteKind::Bridge`: `chunk_map.set_tile(tx, ty, Bridge)`, spawn `Bridge { restore_tile, faction_id }` component entity, emit `TileChangedEvent`. Wall path unchanged.

### 4.4 Deconstruct
- Read `restore_tile`, `set_tile(..., River)`, emit `TileChangedEvent`, drop `deconstruct_refund` percentage on nearest passable bank tile, despawn `Bridge` entity.

### 4.5 Tests
- Finalize: River → Bridge, A* crosses; deconstruct restores River, A* no longer crosses, refunds on bank.
- No-stand abandonment: blueprint cancels after 1 day, inputs dropped.
- Adjacent Bridge blueprints don't claim each other as stands.

---

## Phase 5 — Same-bank reachability helper

**New file:** `src/simulation/river_context.rs`. Reuse bounded-BFS pattern from `doormat_reaches_home` (1500-node cap).

```rust
pub struct RiverContext {
    nearest_river: Option<(i32, i32)>,
    river_orientation: Option<RiverAxis>, // NS, EW, NE-SW, NW-SE
    safe_bank_tiles: SmallVec<[(i32, i32); 16]>,
}

pub fn river_context_around(chunk_map: &ChunkMap, center: (i32, i32), radius: u8) -> RiverContext;
pub fn same_bank_bfs(chunk_map: &ChunkMap, start: (i32, i32), target: (i32, i32), cap: usize) -> bool;
pub fn project_to_safe_bank(chunk_map: &ChunkMap, desired: (i32, i32), home: (i32, i32)) -> Option<(i32, i32)>;
```

`river_orientation`: scan a 5×5 window around the nearest river tile, count NS vs. EW runs. Drives Phase 6 road orientation.

### 5.1 Tests
- BFS: same-bank reachable; 1-tile-river separation unreachable.
- Projection: in-river desired → bank within radius; impossible → `None`.
- Orientation: synthetic NS river → `RiverAxis::NS`.

---

## Phase 6 — Early-era seeding (river-aware)

### 6.1 Paleolithic/Mesolithic hearths (`src/simulation/settlement.rs:414-450`)
- Each radial offset passes through `project_to_safe_bank`.
- Reject `river_distance_at(tx, ty) <= 1`.
- Prefer `3..=6` via rotated offset retries before falling back.

### 6.2 Nomadic camp (`src/simulation/nomad.rs`, `nomad_pack_labor.rs`)
- Hearth projected to safe same-bank tile.
- Bedrolls/tents/yurts in rings: each tile projected; if `None`, slot skipped (under-seed OK).

### 6.3 Neolithic+ organic planner pre-bridge (`src/simulation/organic_settlement.rs`)
- Faction lacking `BRIDGE_BUILDING`:
  - `SettlementBrain.road_segments`: reject Bresenham traces crossing River; retry rejected anchors.
  - `StreetSpine::Linear`: if river within radius ≤ 10, align spine parallel to `river_orientation`.
  - Anchor candidates filtered by `same_bank_bfs(chief_tile, candidate, 1500)`.

### 6.4 Tests
- Paleo with NS river at x=0: all hearths `x >= 3`, none in river.
- Nomadic seed east of river: no bedrolls on west bank.
- Organic Neolithic without tech: zero road segments cross river.

---

## Phase 7 — AI bridge-intent generation (post-tech)

**File:** `src/simulation/organic_settlement.rs`. When faction has `BRIDGE_BUILDING` and civic gate passes:
- Allow Bresenham traces crossing up to `MAX_BRIDGE_SPAN = 4` consecutive River tiles between passable banks.
- For each crossing: split into `(dry_lhs, river_run, dry_rhs)`. Enqueue dry sub-segments to `RoadCarveQueue`. For each river tile, emit `BuildIntent::Bridge { tile }`.
- `chief_post_funding_system` treats `Bridge` as Build category (flat per-day wage, capped by `CHIEF_BUILD_WAGE_CAP`).
- Spans `> MAX_BRIDGE_SPAN`: rejected, planner falls back to parallel-to-river route.

### 7.1 Tests
- 2-tile river run + adopted tech: exactly 2 `BuildIntent::Bridge`, dry segments carve, connectivity restored after build.
- Same without tech: no intents, alternate route.
- 5-tile run (> cap): no intents, rejected.

---

## Phase 8 — Documentation
- Root `CLAUDE.md`: "Rivers and bridges" subsection under Settlement construction.
- `src/simulation/CLAUDE.md`: bridge pipeline (intent → blueprint → adjacent work-stand → finalize → tile mutation → deconstruct).
- `src/world/CLAUDE.md`: `Bridge` semantics (passable; `is_water_like`/`is_freshwater` still true).

---

## Critical files
- `src/world/tile.rs` — `Bridge` variant + helpers.
- `src/rendering/color_map.rs` — color entry.
- `src/pathfinding/tile_cost.rs` — speed multiplier.
- `src/simulation/technology.rs` — `BRIDGE_BUILDING` def.
- `src/simulation/technology_adoption.rs` — Institutional scale.
- `src/simulation/construction.rs` — `BuildSiteKind::Bridge`, recipe, `work_stand_for`, finalize/deconstruct, `restore_tile`.
- `src/simulation/civic_milestones.rs` — `CivicKind::Bridge`.
- `src/simulation/river_context.rs` *(new)* — BFS + projection + orientation.
- `src/simulation/settlement.rs` — paleo hearth projection.
- `src/simulation/nomad.rs` / `nomad_pack_labor.rs` — camp ring projection.
- `src/simulation/organic_settlement.rs` — road planning + bridge intent emission.
- `src/ui/orders.rs` — river-tile-only Build Bridge.
- `CLAUDE.md` + `src/simulation/CLAUDE.md` + `src/world/CLAUDE.md`.

---

## Verification
1. `cargo test --bin civgame`.
2. `cargo run` smoke test:
   - Paleolithic on river map: hearths/beds on home bank, off river.
   - Nomadic near river: bedrolls/tents one bank.
   - Chalcolithic + `BRIDGE_BUILDING`: AI builds a bridge across a short run; roads connect.
   - Player right-clicks river: no option pre-tech; option post-tech; build it, walk across.
   - Deconstruct: River restored, agents can't cross, refunds on bank.
3. Regression: roads do not paint over River tiles.

## Out of scope (V2+)
- Stone arches, multi-tile single blueprint, durability/flood damage, tolls/tenure, trade-link price effects.
