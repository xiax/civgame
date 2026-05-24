# Incremental Mining & Digging

## Context

Today every stone/ore harvest and every Dig Down is **binary**: workers accumulate `work_progress` and a single `carve_tile` call removes both the head and the floor at task completion. Gather has a special "no-pick trickle" branch that mints 1 stone without carving. Two problems: no visible progression (a tile is rock or it's a hole), and tool gating is binary (pick = full carve, no pick = infinite drip).

Goal: shared **7-level excavation** used by stone/ore gather and by Dig Down. Levels 1-6 leave the tile in place but visibly damaged (slowdown + cover + per-level yield); level 7 performs the existing carve. Bare hands cap at level 3 on stone-like material; any Pick reaches 7. Tool tier (Bone..Bronze) only shortens per-level work time.

## Files to modify

| Area | File |
|---|---|
| Shared excavation authority | new `src/simulation/excavation.rs` |
| Worker executors | `src/simulation/dig.rs`, `src/simulation/gather.rs` (stone/ore branch) |
| Carve split | `src/simulation/carve.rs` |
| Tile flag bits | `src/world/tile.rs` |
| Path/movement cost reads | `src/pathfinding/tile_cost.rs`, `src/pathfinding/astar.rs`, `src/pathfinding/flow_field.rs`, `src/simulation/movement.rs` |
| Ranged cover | `src/simulation/combat.rs` (ranged branch around `:449`) |
| Tile refresh / sprite overlay | `src/rendering/entity_sprites.rs`, `src/rendering/sprite_library.rs` |
| Right-click menu progress label | `src/ui/orders.rs` |
| Player command failure UX | `src/simulation/player_command.rs` (new `CommandFailure::MissingTool`) |
| Restamp on chunk load | new system in `src/simulation/excavation.rs`, ordered alongside other `restamp_*_on_chunk_load` |
| Docs | per-directory `CLAUDE.md` + root `CLAUDE.md` (tile-flag layout) |

## Design

### 1. `simulation::excavation` — single authority

```rust
pub const EXCAVATION_LEVEL_MAX: u8 = 7;
pub const HAND_DEPTH_LIMIT: u8 = 3;
pub const LEVEL_WORK_TICKS: u32 = 12;

#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub enum ExcavationMode { Mine, DigDown }

#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct ExcavationKey { pub tile: (i32, i32), pub z: i8, pub mode: ExcavationMode }

#[derive(Copy, Clone)]
pub struct ExcavationCell { pub level: u8, pub completed_carve: bool }

#[derive(Resource, Default)]
pub struct ExcavationMap { pub cells: AHashMap<ExcavationKey, ExcavationCell> }
```

Map is the durable, mode-aware source of truth. `TileData.flags` is a fast-read projection on the *worked tile* (the floor cell for Dig Down, the wall cell for Mine).

### 2. `TileData.flags` layout

Existing bits (0-3): building, road, explored, visible. Allocate **bits 4-6 (3 bits) as `excavation_level`** (0..=7). Bit 7 reserved.

```rust
impl TileData {
    pub fn excavation_level(&self) -> u8 { (self.flags >> 4) & 0b111 }
    pub fn set_excavation_level(&mut self, lvl: u8) {
        debug_assert!(lvl <= EXCAVATION_LEVEL_MAX);
        self.flags = (self.flags & !0b0111_0000) | ((lvl & 0b111) << 4);
    }
    pub fn is_partially_excavated(&self) -> bool {
        let l = self.excavation_level(); l > 0 && l < EXCAVATION_LEVEL_MAX
    }
}
```

### 3. Per-cycle advancement

Replace the per-call carve in `dig_system` and `gather_system` (stone/ore branch) with `excavation::advance(map, chunk_map, gen, globe, key, carrier, tile_changed, tile_carved)`:

1. Look up or insert `ExcavationCell { level: 1, completed_carve: false }`.
2. Pay this level's yield (§5).
3. If `level < 7`: bump `level`, write the cache bit, emit `TileChangedEvent`.
4. Else: call **non-yielding** `carve::finalize_carved_tile(...)`, clear the cache bit, set `completed_carve = true`, emit `TileChangedEvent` **and** `TileCarvedEvent`.

Executor keeps the task alive across levels until: level 7 finalized, hand-depth cap hit (no Pick on stone-like), carrier full, target invalid, or `gather_claims` reservation lost. Hand-depth halt under a player `Commanded` returns `CommandFailure::MissingTool`.

### 4. Tool gate (per-cycle, material-aware)

```rust
pub fn excavation_depth_cap(toolkit: Option<&ToolKit>, kind: TileKind) -> u8 {
    if !kind.is_stone_like() { return EXCAVATION_LEVEL_MAX; }
    match toolkit {
        None => EXCAVATION_LEVEL_MAX,
        Some(tk) if tk.satisfies(&pick_req()) => EXCAVATION_LEVEL_MAX,
        Some(_) => HAND_DEPTH_LIMIT,
    }
}
```

Evaluated per cycle. Dropping the pick mid-excavation halts at level 3. Tier scales `work_speed_mult` only. Removes the legacy no-pick trickle path in `gather.rs:883-908` entirely.

### 5. Yield curve — flat 1 unit per level

```rust
pub fn level_yield(kind: TileKind) -> Option<(ResourceId, u32)> {
    if kind == TileKind::Ore       { return Some((ore_yield_resource_id(...), 1)); }
    if kind.is_stone_like()        { return Some((stone_id(), 1)); }
    None
}
```

Fully-mined stone column: 7 units (vs. 2 today). Ore: 7 (vs. 2). Deliberate inflation — slower visible labor earns more material. Limestone's `stone_yield_count` edge is dropped at the excavation site; `stone_yield_count` keeps its other callers (one-shot `carve_tile` from wells/terraform).

For Dig Down on dual stone-like columns, run two `ExcavationKey`s (head + floor) in lockstep — each pays 1/level independently. Fully dug stone-on-stone yields 14; stone-on-dirt yields 7.

Level-7 step pays its 1-unit slot then calls `finalize_carved_tile` (no extra yield).

### 6. Carve split

Extract the head+floor tile mutation from `carve_tile` into `finalize_carved_tile(chunk_map, gen, globe, tx, ty, target_floor_z, tile_changed)` — the **non-yielding** body. Keep `carve_tile` as a one-shot wrapper. One-shot callers (`well::carve_well_geometry`, `construction::wall_destruction_system`, `terraform`) keep using `carve_tile` unchanged.

### 7. Pathfinding & movement

```rust
pub fn tile_speed_multiplier_from_data(data: TileData) -> f32 {
    let base = tile_speed_multiplier(data.kind);
    if base <= 0.0 { return base; }
    let lvl = data.excavation_level();
    if lvl == 0 || lvl >= EXCAVATION_LEVEL_MAX { return base; }
    base * (1.0 - 0.08 * lvl as f32) // 0.92..0.52 over levels 1..6
}
```

Update `pathfinding::astar` (`step_cost_for` → `step_cost_for_data`), `pathfinding::flow_field` same swap, `simulation::movement` tile-speed read. **Flow fields not invalidated on per-level events** — hotspot-anchored, rebuilt lazily; per-agent A* always reads fresh data.

### 8. Ranged cover (combat)

In the `weapon_stats.is_ranged()` branch around `combat.rs:449`, before the `attack_lands` roll:

```rust
let cover_pct = chunk_map.tile_data_at(target_tx, target_ty, target_z)
    .map(|d| (d.excavation_level() as f32 * 0.05).min(0.30))
    .unwrap_or(0.0);
let hit_chance = (base_hit - cover_pct).clamp(0.20, 0.95);
```

Ranged only. Melee untouched. LOS still established; projectile still fires.

### 9. Restamp on chunk load

```rust
pub fn restamp_excavation_on_chunk_load(
    events: EventReader<ChunkLoadedEvent>, map: Res<ExcavationMap>,
    chunk_map: ResMut<ChunkMap>, gen: Res<WorldGen>, globe: Res<Globe>,
    tile_changed: EventWriter<TileChangedEvent>,
)
```

For each loaded chunk, for each `ExcavationMap` entry inside it: `completed_carve` → `finalize_carved_tile`; partial → write the level bit. Order: `.after(chunk_streaming_system).after(restamp_walls_on_chunk_load).before(restamp_runtime_water_on_chunk_load)` so completed carves open the column before water lays in.

### 10. `gather_claims` integration

Prefer tiles with WIP excavation (`level > 0 && < 7`) over fresh neighbors, all else equal. Filter no-pick workers off any stone-like candidate with `excavation_level >= HAND_DEPTH_LIMIT`.

### 11. UI

- `ui/orders.rs`: when hovered tile has `excavation_level > 0`, append ` (N/7)` to Mine / Dig Down label.
- `dispatch_player_command_system`: no-Pick actor + stone-like target at `level >= HAND_DEPTH_LIMIT` → `CommandFailure::MissingTool`. Surfaces via existing toast/log path.

### 12. Rendering

Add a partial-excavation **child sprite overlay** (no `TileMaterials` key explosion). New procedural rubble sprite in `sprite_library.rs` (`SPRITE_TILE_EXCAVATION_OVERLAY`), six variants by level (1-2 cracks, 3-4 chips, 5-6 pile). In `apply_tile_refreshes_system`, attach child sprite when `data.is_partially_excavated()`, despawn at 0 or 7. z-offset `+0.02`, tint to lithology. `TileChangedEvent` debounces via `PendingTileRefreshes: HashSet`.

### 13. Doc updates

- `src/world/CLAUDE.md` — flag-bit layout, `is_partially_excavated`.
- `src/simulation/CLAUDE.md` — `excavation` module, 7-level model, hand-depth cap, restamp pattern, removal of no-pick trickle.
- `src/pathfinding/CLAUDE.md` — cost reads `TileData`.
- `src/rendering/CLAUDE.md` — overlay sprite key + tint convention.
- Root `CLAUDE.md` — one line under `TileKind` palette: "Excavation levels (1-6) on stone/ore tiles slow traversal and provide ranged cover via `TileData.excavation_level()`; level 7 triggers `carve_tile`."

## Interaction with the map change-tracking model

**Layer 1 — per-chunk tile deltas (ephemeral).** `Chunk::set_delta` overrides; chunk evicted on stream-out; regenerated from `Globe + seed` on stream-in (deltas gone). Where worker carves land.

**Layer 2 — durable off-chunk Resources (`WallMap`, `DamMap`, `BridgeMap`, `WellMap`, `RuntimeWater`).** Each ships a `restamp_*_on_chunk_load` system that re-projects entries onto freshly-generated chunks via `ChunkLoadedEvent`.

`ExcavationMap` is a Layer-2 resource. The flag-bit cache is Layer-1, re-derived on stream-in from either `ExcavationMap` or worldgen.

Event story unchanged: `TileChangedEvent` per level (deduped by `PendingTileRefreshes`), `TileCarvedEvent` only at level 7 (preserves `aquifer_seep_emitter_system` semantics), `invalidate_vision_caches_system` already listens to `TileChangedEvent` without code changes.

## Worldgen-seeded partial deformation

Supported by treating the flag-bit cache as a write target for worldgen too.

**Approach A — WorldGen-painted partials (deterministic, no `ExcavationMap` entry).** In `terrain::generate_chunk_from_globe`, set `excavation_level` bits on stone-like `TileData`s where the worldgen wants visible weathering: `ReliefClass::Badlands` cliffs, `MountainSlope` exposed rock, `MountainRidge` flanks (level 1-2), scree below cliffs (level 1-3). Zero runtime memory cost — regenerated deterministically every stream-in. Workers can chip further; on first interaction `excavation::advance` reads the cached level and seeds `ExcavationMap` from there. Rule: worldgen partials are stone-like/ore only and capped at level 3.

**Approach B — Seeded `ExcavationMap` entries (history / ruins / abstract-faction activity).** For "this site was mined long ago" — ruins, abandoned settlements, historical mining tied to abstract `Globe` factions — emit `ExcavationCell { level, completed_carve }` into `ExcavationMap` at `OnEnter(Playing)` (mirroring seeded wells). Restamp pass projects onto chunks as they stream in. Use when the partial state carries semantics beyond "looks chipped" — e.g. a tile already fully carved by a long-dead faction.

**Interop:** A and B coexist. Restamp pass writes B last so it wins where they overlap.

## Test plan

- Unit: `TileData::set_excavation_level` round-trips 0..=7 without disturbing bits 0-3.
- Unit: `level_yield` returns `Some((stone_id, 1))` for every stone-like kind, `Some((ore_resource, 1))` for Ore, `None` for soil/grass.
- Unit: `excavation_depth_cap` — soil → 7, stone+pick → 7, stone-no-pick → 3, stone-no-toolkit → 7.
- Behavioural (`test_fixture.rs`):
  - Stone tile, no pick: advances 1→3 over 3·`LEVEL_WORK_TICKS`, pays 3 stone, stops; `CommandFailure::MissingTool` if commanded.
  - Stone tile, bronze pick: advances 1→7, pays 7 stone, level 7 triggers `finalize_carved_tile` + `TileCarvedEvent`.
  - Dig Down on dual stone column: head+floor both reach 7; total yield 14; floor → Dirt, head → Air.
  - Worker drops pick at level 5 → next cycle halts at 5 (no regress).
- Path cost: A* over level-6 partials reports ~2× step cost (0.52 vs 1.0 mult).
- Combat: ranged at level-6 partial = -30% hit; melee unchanged; LOS still established.
- Chunk reload: excavate to level 4, unload, reload → `excavation_level() == 4` and overlay re-attaches.
- `cargo test --bin civgame` + `cargo check`.
- `cargo run` smoke: Mine on stone face, watch label tick and overlay; Dig Down on stone with no pick, verify halt at 3 + toast.

## Out of scope

- Soil-yield resource (Dirt commodity is a separate change).
- Siege-ram interaction with excavation (existing wall durability path).
- XP curve overhaul (Mining XP can stay proportional to `level_yield`).
- Save-game serialization (project doesn't ship saves).
