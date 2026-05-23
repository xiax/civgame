# Lookout And Base Vision — Comprehensive Plan

## Context

Today vision is uniformly 15 tiles per agent (`VIEW_RADIUS` referenced from both `rendering/fog.rs::fog_update_system` and `simulation/memory.rs::vision_system`). There is no "vantage" mechanic, no base-radiated vision, and no faction-aware LOS — `simulation/line_of_sight.rs::has_los` treats every `TileKind::Wall` and `TileKind::Ore` the same whether the wall is natural bedrock or a Palisade the player just built. As a result a player who walls in a courtyard cannot "see" their own interior from the outside, and bases never reveal terrain on their own.

This plan adds (a) a reusable vision-source layer with three radii — standard 15, active lookout 50, era-scaled base 16/20/25/30/40 — (b) an `ActiveLookout` component + `Task::Lookout` + `PlayerCommand::Lookout` + right-click "Lookout here", (c) faction-aware LOS that lets a faction see through its own walls / closed doors while leaving combat and sound LOS untouched, and (d) constructed-wall ownership metadata.

## Design Decisions

1. **Fork LOS, don't overload it.** `has_los` is shared by fog-vision, resource sighting, **and combat**. Keep combat LOS untouched: add a sibling `has_vision_los(chunk_map, wall_map, door_map, from, to, observer_faction)` and refactor the inner Bresenham walker into a private `walk_with(predicate)` so both callers share ray math.
2. **Constructed-wall identification is via `WallMap`, not a tile-kind variant.** Constructed walls already insert a `Wall` entity into `WallMap` and stamp `TileKind::Wall`. Natural bedrock has no `WallMap` entry. "Own wall pass-through" = tile is `Wall`, `WallMap` has an entry, and `Wall.owner_faction == observer_faction`.
3. **Cache static sources; never recompute on a timer.** Lookouts and bases don't move, so their visible set is constant until terrain or LOS-relevant entities change. Each lookout / base entity holds `CachedVisionSet { tiles, dirty }`. Compute once on activation, then union into `FogMap.visible` every fog tick. Recompute only on:
   - `ActiveLookout` inserted / anchor changed.
   - `Settlement` / `Camp` founded, moved, or its faction's era advances.
   - `TileChangedEvent` inside any source's bounding-radius.
   - `WallConstructed` / `WallDestroyed` within radius. Own-door open/close does NOT invalidate own-faction sources (own doors are always transparent to vision); foreign door changes inside a player base's radius do.
   The standard per-agent radius-15 sweep keeps its current per-frame cadence (agents move continuously).
4. **Autonomous Lookout goes through the HTN registry**, not direct injection. Add a `ScoutLookout` method that fires post-`Task::Explore` arrival when `CurrentVision` produced no concrete sighting, expanding to `Task::Lookout { expires_tick: Some(now + 120) }`. Manual `PlayerCommand::Lookout` uses direct dispatch (player authority).
5. **Routing target is the tile itself, not adjacent.** Stand-on, not adjacent — follow `Task::Lead { dest }` precedent.
6. **`ActiveLookout` is removed by movement, not by task change.** Cleared when (a) tile/z differs from anchor, (b) `expires_tick` elapsed, (c) a new player command lands. `Task::Idle` does NOT clear it.
7. **`FogMap` stays player-only.** Settlement/Camp vision iterates entities filtered to `owner_faction == PlayerFaction.faction_id`. Non-player bases are no-ops on fog.
8. **One radius source.** `fog_update_system` and `memory::vision_system` both call `effective_vision_radius(active_lookout)`. Resource sighting participates in the wider lookout sweep — otherwise lookouts would expose terrain but not resources on it.

## Critical files

| File | Change |
|------|--------|
| `src/simulation/line_of_sight.rs` | Refactor inner walker; add `has_vision_los(...)` taking `wall_map` + `observer_faction`. Skip opacity when `WallMap` entry's owner matches, or for own-faction door. |
| `src/simulation/construction.rs` | Add `owner_faction: u32` to `Wall`. Set at finalize from builder's `FactionMember`. Update `restamp_walls_on_chunk_load` if needed. Emit `WallConstructed { tile, faction }` alongside existing `WallDestroyed`. |
| `src/simulation/vision.rs` *(new)* | `STANDARD_VIEW_RADIUS=15`, `LOOKOUT_VIEW_RADIUS=50`, `base_vision_radius_for_era(Era)`, `effective_vision_radius`, `ActiveLookout`, `CachedVisionSet`, `compute_vision_set(...)`, `apply_cached_sets_to_fog(...)`, `prune_active_lookouts_system`, `recompute_dirty_vision_sets_system`, `VisionSourceDirty` event. |
| `src/simulation/tasks.rs` / `typed_task.rs` | `TaskKind::Lookout`, `Task::Lookout { anchor, anchor_z, expires_tick }`. Executor: if at anchor, ensure `ActiveLookout` present and idle; else rely on Move prefix from `assign_task_with_routing`. |
| `src/simulation/player_command.rs` | `PlayerCommand::Lookout { tile, z }`. Dispatcher routes each selected worker through `assign_task_with_routing` with the tile as both target and stand tile. |
| `src/ui/orders.rs` | "Lookout here" item in tile section of `right_click_context_menu_system`; visible when ≥1 worker selected and tile is passable. |
| `src/simulation/htn.rs` | Register `ScoutLookout` method under `AgentGoal::Scout`. Applies post-Explore-arrival when `CurrentVision` is empty; expands to `Task::Lookout { expires_tick: Some(now + 120) }`. |
| `src/simulation/memory.rs` | `vision_system` reads `Option<&ActiveLookout>` and uses `effective_vision_radius(...)`. |
| `src/rendering/fog.rs` | Keep per-agent scan; after the sweep, union every player-faction `CachedVisionSet` into `FogMap.visible` — set-union only, no LOS work. |
| `src/simulation/CLAUDE.md`, `src/rendering/CLAUDE.md` | Document the new vision layer, faction-aware LOS, lookout lifecycle, base reveal cadence. |

## Key types

```rust
// src/simulation/vision.rs
pub const STANDARD_VIEW_RADIUS: u32 = 15;
pub const LOOKOUT_VIEW_RADIUS: u32 = 50;

#[derive(Component, Debug, Clone, Copy)]
pub struct ActiveLookout {
    pub anchor_tile: (i32, i32),
    pub anchor_z: i8,
    pub radius: u32,
    pub expires_tick: Option<u64>,   // None = manual / indefinite
}

#[derive(Component, Default, Debug, Clone)]
pub struct CachedVisionSet {
    pub tiles: AHashSet<(i32, i32)>,
    pub dirty: bool,
}

pub fn base_vision_radius_for_era(era: Era) -> u32 {
    match era {
        Era::Paleolithic  => 16,
        Era::Mesolithic   => 20,
        Era::Neolithic    => 25,
        Era::Chalcolithic => 30,
        Era::BronzeAge    => 40,
    }
}

pub fn effective_vision_radius(lookout: Option<&ActiveLookout>) -> u32 {
    lookout.map(|l| l.radius).unwrap_or(STANDARD_VIEW_RADIUS)
}
```

```rust
// src/simulation/line_of_sight.rs
fn walk_ray(from, to, mut blocker: impl FnMut((i32,i32,i8)) -> bool) -> bool { ... }

pub fn has_los(chunk_map, door_map, from, to) -> bool { ... }

pub fn has_vision_los(chunk_map, wall_map, door_map, from, to, observer_faction: u32) -> bool {
    walk_ray(from, to, |p| {
        let kind = chunk_map.tile_kind(p);
        if kind.is_opaque() {
            if let Some(w) = wall_map.get(&(p.0, p.1)) {
                if w.owner_faction == observer_faction { return false; }
            }
            return true;
        }
        if let Some(d) = door_map.get(&(p.0, p.1)) {
            if d.faction_id == observer_faction { return false; }
            return !d.open;
        }
        false
    })
}
```

## Behavior contracts

- **Manual Lookout** — `PlayerCommand::Lookout { tile }` routes selected worker(s) to tile. On arrival, `Task::Lookout` parks the queue, inserts `ActiveLookout { expires_tick: None }` + `CachedVisionSet { dirty: true }`. Set built once next tick. New command or movement away from anchor removes both; lookout disappears from fog on next union pass.
- **Autonomous Lookout** — HTN `ScoutLookout` method expands `Task::Lookout { expires_tick: Some(now + 120) }` after Explore arrival without a sighting. Auto-clears on expiry.
- **Base vision** — On Settlement/Camp spawn for a player-faction owner, attach `CachedVisionSet { dirty: true }`. `recompute_dirty_vision_sets_system` raycasts once from `market_tile` / `home_tile` at `base_vision_radius_for_era(era)`. Era-advance marks dirty.
- **Forest stays transparent.** No regression on `is_opaque`.
- **Combat / sound / projectile LOS unchanged.** `has_los` keeps its existing signature and behavior.

## Verification

1. `cargo check` — clean.
2. `cargo test --bin civgame` — full suite green; existing combat-LOS tests unchanged.
3. New tests:
   - `vision_los_passes_own_wall_blocks_enemy_wall`
   - `vision_los_passes_own_door_open_and_closed`
   - `lookout_radius_reveals_distant_resource`
   - `lookout_breaks_on_move`
   - `base_vision_era_radii` (table-driven)
   - `base_vision_ignores_own_walls`
   - `cached_vision_invalidates_on_wall_built`
   - `cached_vision_stable_without_change` (raycast counter)
   - `explore_arrival_starts_autonomous_lookout`
4. Manual run (`cargo run`):
   - Right-click far tile → "Lookout here" → worker walks, fog fans out radius 50, stays.
   - Move that worker → 50-radius patch reverts to normal coverage, `ActiveLookout` cleared.
   - Wall a courtyard around home tile → interior stays visible; an enemy palisade still blocks player's sight into enemy ground.
5. Update `src/simulation/CLAUDE.md` and `src/rendering/CLAUDE.md`.

## Deferred (out of scope)

- Telescope / spyglass tech scaling `LOOKOUT_VIEW_RADIUS`. Seam: `ActiveLookout.radius` is per-instance.
- Per-faction `FogMap`s (currently player-only).
- Partial-cover tiles (forest as half-vision).
