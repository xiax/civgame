//! Faction-aware vision sources: active lookouts + base reveal radii.
//!
//! Per `plans/lookout-base.md`. Three vision-source kinds share one
//! `CachedVisionSet` component:
//!
//! - **Per-agent** sweeps (radius 15) live in `rendering::fog::fog_update_system`
//!   and `simulation::memory::vision_system`; they don't cache because agents
//!   move every tick. `effective_vision_radius(active_lookout)` is the
//!   shared "what radius does this agent see at right now?" helper.
//!
//! - **Active lookouts** (`ActiveLookout` on a Person) bump that agent's
//!   radius to `LOOKOUT_VIEW_RADIUS = 50` and attach a `CachedVisionSet`
//!   so the 50-radius raycast runs once on activation and the result is
//!   reused every fog tick. Manual lookouts (`PlayerCommand::Lookout`)
//!   hold indefinitely; autonomous HTN-driven lookouts carry an
//!   `expires_tick`.
//!
//! - **Base vision** (`Settlement` / `Camp` owned by the player faction)
//!   gets a `CachedVisionSet` from a one-shot raycast at
//!   `base_vision_radius_for_era(era)`; the cache is invalidated when
//!   the faction's era advances or the local terrain / walls change.
//!
//! Cache invalidation rides three event channels:
//!
//! - `TileChangedEvent` — any mutation that could open or block a ray.
//! - `WallDestroyed` / `WallConstructed` — wall lifecycle.
//! - Era advance — handled by `recompute_dirty_vision_sets_system` reading
//!   `current_era(faction.buildable_techs)` and comparing to the cached
//!   radius before the next raycast.
//!
//! Combat / sound / projectile LOS keep using `simulation::line_of_sight::has_los`
//! (own walls must defend); only fog and memory use `has_vision_los`.

use crate::collections::AHashSet;
use bevy::prelude::*;

use crate::simulation::camp::Camp;
use crate::simulation::construction::{
    DoorMap, Wall, WallConstructed, WallDestroyed, WallMap,
};
use crate::simulation::faction::{FactionRegistry, PlayerFaction};
use crate::simulation::line_of_sight::has_vision_los;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::settlement::Settlement;
use crate::simulation::tasks::TaskKind;
use crate::simulation::technology::{current_era, Era};
use crate::simulation::typed_task::{ActionQueue, Task};
use crate::world::chunk::ChunkMap;
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::terrain::TILE_SIZE;

/// Standard per-agent vision radius. Mirrors the legacy `VIEW_RADIUS` that
/// used to live in `rendering::fog` and `simulation::memory`; both modules
/// now ask `effective_vision_radius(active_lookout)` for their per-tick
/// scan radius so a lookout extends both fog reveal AND resource sighting.
pub const STANDARD_VIEW_RADIUS: u32 = 15;

/// Active-lookout vision radius. A future telescope tech parameterises
/// `ActiveLookout.radius` at task-dispatch time, so this constant is the
/// default for unaided eyes.
pub const LOOKOUT_VIEW_RADIUS: u32 = 50;

/// Autonomous (HTN) lookout pause duration. Manual lookouts hold
/// indefinitely (`expires_tick = None`); scout arrivals into Explore
/// without a sighting trigger a 120-tick lookout.
pub const AUTONOMOUS_LOOKOUT_TICKS: u64 = 120;

/// Per-era base vision radius. A settled / nomadic player-faction
/// base anchor radiates fog vision at this radius without an agent
/// nearby — the player can see their own home and its near surroundings
/// even when every worker is off foraging.
pub fn base_vision_radius_for_era(era: Era) -> u32 {
    match era {
        Era::Paleolithic => 16,
        Era::Mesolithic => 20,
        Era::Neolithic => 25,
        Era::Chalcolithic => 30,
        Era::BronzeAge => 40,
    }
}

/// One Person component per active lookout. Removed when (a) `expires_tick`
/// elapses, (b) the agent's tile/z drifts off `anchor_tile / anchor_z`, or
/// (c) the player issues a new command that supersedes the lookout.
#[derive(Component, Debug, Clone, Copy)]
pub struct ActiveLookout {
    pub anchor_tile: (i32, i32),
    pub anchor_z: i8,
    pub radius: u32,
    /// `None` = manual / indefinite; `Some(t)` = autonomous, expire at `t`.
    pub expires_tick: Option<u64>,
}

/// Cached visible-tile set for a static vision source (lookout, settlement,
/// camp). Built once on activation by `recompute_dirty_vision_sets_system`,
/// then unioned into `FogMap.visible` every fog tick without LOS work.
/// `dirty = true` forces a recompute on the next scheduled pass.
#[derive(Component, Debug, Default, Clone)]
pub struct CachedVisionSet {
    pub tiles: AHashSet<(i32, i32)>,
    pub dirty: bool,
    /// Faction the cached source belongs to. Stored so the union pass
    /// can filter to the player faction without joining the source
    /// component (Settlement / Camp / Person each store faction in
    /// their own field).
    pub faction: u32,
    /// Radius the cache was last built at. Era-advance compares against
    /// `base_vision_radius_for_era(current_era)` and marks dirty when
    /// the radius grew.
    pub radius: u32,
    /// Origin tile + z the cache was last built around. Compared to the
    /// source's current anchor; a moved Camp (post-migration) invalidates
    /// in place.
    pub origin: (i32, i32, i8),
}

impl CachedVisionSet {
    pub fn new(faction: u32) -> Self {
        Self {
            tiles: AHashSet::default(),
            dirty: true,
            faction,
            radius: 0,
            origin: (0, 0, 0),
        }
    }
}

/// Per-agent radius selector: takes the lookout's value when present, else
/// falls through to the standard radius. Wired into both
/// `rendering::fog::fog_update_system` and `simulation::memory::vision_system`
/// so a lookout extends fog AND resource sighting at the same time.
pub fn effective_vision_radius(lookout: Option<&ActiveLookout>) -> u32 {
    lookout.map(|l| l.radius).unwrap_or(STANDARD_VIEW_RADIUS)
}

/// Compute the visible-tile set for a static source. Pure helper: walks every
/// candidate tile in the radius once, runs a faction-aware LOS check, and
/// inserts the visible ones into `out`. Endpoint tile is always visible.
#[allow(clippy::too_many_arguments)]
pub fn compute_vision_set(
    out: &mut AHashSet<(i32, i32)>,
    chunk_map: &ChunkMap,
    wall_map: &WallMap,
    door_map: &DoorMap,
    edge_map: &crate::simulation::construction::EdgeStructureMap,
    wall_q: &Query<&Wall>,
    origin: (i32, i32, i8),
    radius: u32,
    faction: u32,
) {
    out.clear();
    let r = radius as i32;
    let r_sq = r * r;
    let (ox, oy, oz) = origin;
    out.insert((ox, oy));
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy > r_sq {
                continue;
            }
            let tx = ox + dx;
            let ty = oy + dy;
            if (tx, ty) == (ox, oy) {
                continue;
            }
            let raw_z = chunk_map.surface_z_at(tx, ty);
            let tz = raw_z.clamp(i8::MIN as i32, i8::MAX as i32) as i8;
            let in_los = dx * dx + dy * dy <= 1
                || has_vision_los(
                    chunk_map,
                    wall_map,
                    door_map,
                    edge_map,
                    wall_q,
                    (ox, oy, oz),
                    (tx, ty, tz),
                    faction,
                );
            if in_los {
                out.insert((tx, ty));
            }
        }
    }
}

// ─── lookout / cache lifecycle ───────────────────────────────────────────

/// Stand-and-hold executor for `Task::Lookout`. On arrival at the anchor
/// (state `Working`) attaches `ActiveLookout` + `CachedVisionSet` so the
/// next `recompute_dirty_vision_sets_system` pass builds the 50-tile fog
/// reveal. Auto-finishes when `expires_tick` elapses (autonomous scout
/// pauses); manual lookouts never auto-finish here — they end when the
/// player issues a new command (`aq.cancel` runs in
/// `dispatch_player_command_system`) or the agent dies.
///
/// Runs in `Sequential` after movement so the worker's tile reflects
/// arrival; before `prune_active_lookouts_system` so the expiry path
/// here removes the components on the same tick.
pub fn lookout_task_system(
    mut commands: Commands,
    clock: Res<crate::simulation::schedule::SimClock>,
    member_q: Query<&crate::simulation::faction::FactionMember>,
    mut q: Query<(
        Entity,
        &Transform,
        &mut PersonAI,
        &mut ActionQueue,
        Option<&ActiveLookout>,
    )>,
) {
    let now = clock.tick;
    for (entity, transform, mut ai, mut aq, existing_lookout) in q.iter_mut() {
        if aq.current_task_kind() != TaskKind::Lookout as u16 {
            continue;
        }
        let Task::Lookout {
            anchor,
            anchor_z,
            expires_tick,
        } = aq.current
        else {
            // Stale typed-channel mismatch — drop the chain.
            aq.cancel_chain(&mut ai);
            continue;
        };

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let at_anchor = (tx, ty) == anchor && ai.current_z == anchor_z;
        if !at_anchor {
            // Still en route — movement_system owns the walk.
            continue;
        }

        // Expiry (autonomous pauses only — manual lookouts pass None).
        if expires_tick.map(|t| now >= t).unwrap_or(false) {
            commands.entity(entity).remove::<ActiveLookout>();
            commands.entity(entity).remove::<CachedVisionSet>();
            aq.finish_task(&mut ai);
            continue;
        }

        // Arrived — pin Working + ensure the lookout components exist.
        aq.begin_working(&mut ai);
        if existing_lookout.is_none() {
            let faction = member_q
                .get(entity)
                .map(|m| m.faction_id)
                .unwrap_or(crate::simulation::faction::SOLO);
            commands.entity(entity).insert(ActiveLookout {
                anchor_tile: anchor,
                anchor_z,
                radius: LOOKOUT_VIEW_RADIUS,
                expires_tick,
            });
            let mut cache = CachedVisionSet::new(faction);
            cache.dirty = true;
            commands.entity(entity).insert(cache);
        }
    }
}

/// Autonomous scout-arrival → Lookout pause. When a Hunter on `Survive` /
/// `GatherFood` finishes a `Task::Explore` leg (state Working with the
/// Explore task still current — `goal_dispatch_system`'s catch-all is
/// about to cancel it back to Idle), convert that into a
/// `Task::Lookout { expires_tick: now + AUTONOMOUS_LOOKOUT_TICKS }` at the
/// current tile so the scout actually scans the vantage point they walked
/// to before resuming exploration. After the pause expires the scout
/// dispatcher fires another Explore on the next idle tick.
///
/// This is the "scout pause" autonomous behavior from
/// `plans/lookout-base.md`. Scoped to Hunters so a non-scout's HTN
/// Explore fallback (food / material fallback) doesn't burn 120 ticks
/// standing still — those agents legitimately need to keep walking.
///
/// Runs in `Sequential` after `movement_system` (transform reflects
/// arrival) and before `lookout_task_system` (which then ticks the new
/// Lookout the same fixed-tick).
pub fn autonomous_scout_lookout_pause_system(
    clock: Res<crate::simulation::schedule::SimClock>,
    mut query: Query<(
        &Transform,
        &mut crate::simulation::person::PersonAI,
        &mut ActionQueue,
        &crate::simulation::goals::AgentGoal,
        &crate::simulation::person::Profession,
    )>,
) {
    use crate::simulation::goals::AgentGoal;
    use crate::simulation::person::{AiState, Profession};
    let now = clock.tick;
    for (transform, mut ai, mut aq, goal, profession) in query.iter_mut() {
        if *profession != Profession::Hunter {
            continue;
        }
        if !matches!(*goal, AgentGoal::Survive | AgentGoal::GatherFood) {
            continue;
        }
        if aq.current_task_kind() != TaskKind::Explore as u16 {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        // Replace the about-to-be-cancelled Explore with a bounded
        // stand-and-scan pause at this tile. `aq.cancel` drops the
        // Explore; `aq.dispatch` promotes the Lookout into `current`
        // since cancel left the queue empty.
        aq.cancel_chain(&mut ai);
        aq.dispatch(Task::Lookout {
            anchor: (tx, ty),
            anchor_z: ai.current_z,
            expires_tick: Some(now + AUTONOMOUS_LOOKOUT_TICKS),
        });
    }
}

/// Remove `ActiveLookout` from any agent that moved off its anchor.
/// Expiry-based removal happens inside `lookout_task_system` so the
/// task and the component drop together; the prune system is the
/// catch-all for movement drift (a Disband / new command moves the
/// agent without `lookout_task_system` running).
///
/// Runs in `Sequential` after `movement_system` so the agent's
/// `Transform` reflects the new tile.
pub fn prune_active_lookouts_system(
    mut commands: Commands,
    query: Query<(Entity, &Transform, &PersonAI, &ActiveLookout)>,
) {
    for (entity, transform, ai, lookout) in query.iter() {
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let drifted = (tx, ty) != lookout.anchor_tile || ai.current_z != lookout.anchor_z;
        if drifted {
            commands.entity(entity).remove::<ActiveLookout>();
            commands.entity(entity).remove::<CachedVisionSet>();
        }
    }
}

/// Drain `TileChangedEvent` / `WallConstructed` / `WallDestroyed` and mark
/// every `CachedVisionSet` whose source bounding-box contains the changed
/// tile dirty. Era advance is folded in: when the faction's
/// `current_era`-derived radius exceeds the cached radius, the cache is
/// also marked dirty.
///
/// Runs in `Sequential` before the recompute system.
#[allow(clippy::too_many_arguments)]
pub fn invalidate_vision_caches_system(
    mut tile_changes: EventReader<TileChangedEvent>,
    mut walls_built: EventReader<WallConstructed>,
    mut walls_destroyed: EventReader<WallDestroyed>,
    registry: Res<FactionRegistry>,
    mut sources: Query<(&mut CachedVisionSet, Option<&Settlement>, Option<&Camp>, Option<&ActiveLookout>)>,
) {
    let mut dirty_tiles: Vec<(i32, i32)> = Vec::new();
    for ev in tile_changes.read() {
        dirty_tiles.push((ev.tx, ev.ty));
    }
    for ev in walls_built.read() {
        dirty_tiles.push(ev.tile);
    }
    for ev in walls_destroyed.read() {
        dirty_tiles.push(ev.tile);
    }

    for (mut cache, settlement, camp, lookout) in sources.iter_mut() {
        // Resolve the source's authoritative origin + current radius.
        let (origin, current_radius) =
            resolve_source(settlement, camp, lookout, &registry, &cache);

        // Origin drift (e.g. camp migrated, lookout swapped anchor in place).
        if origin != cache.origin {
            cache.dirty = true;
            continue;
        }

        // Era growth: a Settlement/Camp's era advanced, so the cached
        // radius is smaller than the current era allows. (Lookouts never
        // change radius post-spawn, so this branch no-ops for them.)
        if current_radius > cache.radius {
            cache.dirty = true;
            continue;
        }

        // Tile / wall mutations inside the cached source's bounding box.
        if dirty_tiles.is_empty() {
            continue;
        }
        let r = cache.radius as i32;
        let (ox, oy, _) = cache.origin;
        for &(tx, ty) in &dirty_tiles {
            if (tx - ox).abs() <= r && (ty - oy).abs() <= r {
                cache.dirty = true;
                break;
            }
        }
    }
}

/// Resolve the canonical (origin, current_radius) for a vision source.
/// Lookouts read straight from `ActiveLookout`; Settlement/Camp read
/// `current_era` of their owning faction's `buildable_techs`.
fn resolve_source(
    settlement: Option<&Settlement>,
    camp: Option<&Camp>,
    lookout: Option<&ActiveLookout>,
    registry: &FactionRegistry,
    cache: &CachedVisionSet,
) -> ((i32, i32, i8), u32) {
    if let Some(l) = lookout {
        return ((l.anchor_tile.0, l.anchor_tile.1, l.anchor_z), l.radius);
    }
    if let Some(s) = settlement {
        let radius = era_radius_for_faction(s.owner_faction, registry);
        return ((s.market_tile.0, s.market_tile.1, 0), radius);
    }
    if let Some(c) = camp {
        let radius = era_radius_for_faction(c.owner_faction, registry);
        return ((c.home_tile.0, c.home_tile.1, 0), radius);
    }
    // Fallback: keep the cached origin / radius. Unreachable in practice
    // because every `CachedVisionSet` is attached to exactly one of the
    // three source kinds.
    (cache.origin, cache.radius)
}

fn era_radius_for_faction(faction_id: u32, registry: &FactionRegistry) -> u32 {
    let era = registry
        .factions
        .get(&faction_id)
        .map(|f| current_era(&f.buildable_techs))
        .unwrap_or(Era::Paleolithic);
    base_vision_radius_for_era(era)
}

/// Rebuild every `CachedVisionSet { dirty: true }`. Reads each source's
/// origin + radius (era-derived for bases, component-derived for lookouts),
/// raycasts once via `compute_vision_set`, clears `dirty`.
///
/// Surface z for Settlement/Camp anchors is read from `ChunkMap.surface_z_at`
/// (the market_tile / home_tile is always a passable surface tile).
#[allow(clippy::too_many_arguments)]
pub fn recompute_dirty_vision_sets_system(
    chunk_map: Res<ChunkMap>,
    wall_map: Res<WallMap>,
    door_map: Res<DoorMap>,
    edge_map: Res<crate::simulation::construction::EdgeStructureMap>,
    wall_q: Query<&Wall>,
    registry: Res<FactionRegistry>,
    mut sources: Query<(
        &mut CachedVisionSet,
        Option<&Settlement>,
        Option<&Camp>,
        Option<&ActiveLookout>,
    )>,
) {
    for (mut cache, settlement, camp, lookout) in sources.iter_mut() {
        if !cache.dirty {
            continue;
        }
        let (mut origin, radius) =
            resolve_source(settlement, camp, lookout, &registry, &cache);
        // For Settlement/Camp anchors, lift z to the surface (their stored
        // z is the cache placeholder 0).
        if lookout.is_none() {
            let raw_z = chunk_map.surface_z_at(origin.0, origin.1);
            origin.2 = raw_z.clamp(i8::MIN as i32, i8::MAX as i32) as i8;
        }
        let faction = cache.faction;
        compute_vision_set(
            &mut cache.tiles,
            &chunk_map,
            &wall_map,
            &door_map,
            &edge_map,
            &wall_q,
            origin,
            radius,
            faction,
        );
        cache.origin = origin;
        cache.radius = radius;
        cache.dirty = false;
    }
}

/// Attach a `CachedVisionSet` to every newly-spawned player-faction
/// Settlement / Camp so base vision fans out without a separate
/// founding hook. Runs in `Sequential`; idempotent (skips entities that
/// already carry the component).
pub fn attach_base_vision_caches_system(
    mut commands: Commands,
    player_faction: Res<PlayerFaction>,
    settlements: Query<(Entity, &Settlement), Without<CachedVisionSet>>,
    camps: Query<(Entity, &Camp), Without<CachedVisionSet>>,
) {
    for (entity, s) in settlements.iter() {
        if s.owner_faction == player_faction.faction_id {
            commands
                .entity(entity)
                .insert(CachedVisionSet::new(s.owner_faction));
        }
    }
    for (entity, c) in camps.iter() {
        if c.owner_faction == player_faction.faction_id {
            commands
                .entity(entity)
                .insert(CachedVisionSet::new(c.owner_faction));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::construction::{DoorEntry, WallMaterial};
    use crate::world::chunk::{Chunk, ChunkCoord, CHUNK_SIZE};
    use crate::world::tile::{TileData, TileKind};

    fn flat_chunk_map(kind: TileKind) -> ChunkMap {
        let mut map = ChunkMap::default();
        let surface_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[kind; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        map.0.insert(
            ChunkCoord(0, 0),
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        map
    }

    #[test]
    fn era_radii_match_plan() {
        assert_eq!(base_vision_radius_for_era(Era::Paleolithic), 16);
        assert_eq!(base_vision_radius_for_era(Era::Mesolithic), 20);
        assert_eq!(base_vision_radius_for_era(Era::Neolithic), 25);
        assert_eq!(base_vision_radius_for_era(Era::Chalcolithic), 30);
        assert_eq!(base_vision_radius_for_era(Era::BronzeAge), 40);
    }

    #[test]
    fn effective_radius_uses_lookout_when_present() {
        assert_eq!(effective_vision_radius(None), STANDARD_VIEW_RADIUS);
        let l = ActiveLookout {
            anchor_tile: (0, 0),
            anchor_z: 0,
            radius: LOOKOUT_VIEW_RADIUS,
            expires_tick: None,
        };
        assert_eq!(effective_vision_radius(Some(&l)), LOOKOUT_VIEW_RADIUS);
    }

    /// Vision LOS passes through own constructed wall, blocks enemy wall.
    /// (Verified with `has_vision_los` directly via a constructed
    /// `WallMap` + `Wall` query; the corresponding `has_los` call still
    /// blocks regardless of ownership — exercised by the existing
    /// `line_of_sight::tests::solid_rock_between_underground_agents_blocks_los`.)
    #[test]
    fn vision_los_passes_own_wall_blocks_enemy_wall() {
        use crate::simulation::line_of_sight::has_vision_los;

        let mut app = App::new();
        let mut chunk_map = flat_chunk_map(TileKind::Grass);
        // Stamp a wall tile at (5, 0) above the surface.
        chunk_map.set_tile(
            5,
            0,
            1,
            TileData {
                kind: TileKind::Wall,
                elevation: 0,
                fertility: 0,
                flags: 0b0001,
                ore: 0,
            },
        );
        app.insert_resource(chunk_map);
        app.insert_resource(DoorMap::default());
        let mut wall_map = WallMap::default();
        let wall_entity = app
            .world_mut()
            .spawn(Wall {
                material: WallMaterial::Palisade,
                owner_faction: Some(1),
            })
            .id();
        wall_map.0.insert((5, 0), wall_entity);
        app.insert_resource(wall_map);

        let mut state: bevy::ecs::system::SystemState<(
            Res<ChunkMap>,
            Res<WallMap>,
            Res<DoorMap>,
            Query<&Wall>,
        )> = bevy::ecs::system::SystemState::new(app.world_mut());
        let (chunk_map, wall_map, door_map, wall_q_data) = state.get(app.world());
        let chunk_map = &*chunk_map;
        let wall_map = &*wall_map;
        let door_map = &*door_map;
        let edge_map = crate::simulation::construction::EdgeStructureMap::default();

        // Own faction (1) sees through.
        assert!(has_vision_los(
            chunk_map,
            wall_map,
            door_map,
            &edge_map,
            &wall_q_data,
            (0, 0, 0),
            (10, 0, 0),
            1,
        ));
        // Foreign faction (2) is blocked.
        assert!(!has_vision_los(
            chunk_map,
            wall_map,
            door_map,
            &edge_map,
            &wall_q_data,
            (0, 0, 0),
            (10, 0, 0),
            2,
        ));
        // Natural-rock (no WallMap entry) still blocks own vision.
        let mut chunk_map_natural = flat_chunk_map(TileKind::Grass);
        chunk_map_natural.set_tile(
            5,
            0,
            1,
            TileData {
                kind: TileKind::Wall,
                elevation: 0,
                fertility: 0,
                flags: 0b0001,
                ore: 0,
            },
        );
        let empty_walls = WallMap::default();
        assert!(!has_vision_los(
            &chunk_map_natural,
            &empty_walls,
            door_map,
            &edge_map,
            &wall_q_data,
            (0, 0, 0),
            (10, 0, 0),
            1,
        ));
    }

    /// Own-faction door is transparent to vision in either open or closed
    /// state; enemy closed door still blocks (mirrors the `has_los` rules
    /// for the combat path).
    #[test]
    fn vision_los_passes_own_door_open_and_closed() {
        use crate::simulation::line_of_sight::has_vision_los;

        let mut app = App::new();
        app.insert_resource(flat_chunk_map(TileKind::Grass));
        let mut door_map = DoorMap::default();
        let door_entity = app.world_mut().spawn_empty().id();
        door_map.0.insert(
            (5, 0),
            DoorEntry {
                entity: door_entity,
                open: false, // closed
                faction_id: 1,
            },
        );
        app.insert_resource(door_map);
        app.insert_resource(WallMap::default());

        let mut state: bevy::ecs::system::SystemState<(
            Res<ChunkMap>,
            Res<WallMap>,
            Res<DoorMap>,
            Query<&Wall>,
        )> = bevy::ecs::system::SystemState::new(app.world_mut());
        let (chunk_map, wall_map, door_map, wall_q_data) = state.get(app.world());
        let chunk_map = &*chunk_map;
        let wall_map = &*wall_map;
        let door_map = &*door_map;
        let edge_map = crate::simulation::construction::EdgeStructureMap::default();

        // Own faction sees through own closed door.
        assert!(has_vision_los(
            chunk_map,
            wall_map,
            door_map,
            &edge_map,
            &wall_q_data,
            (0, 0, 0),
            (10, 0, 0),
            1,
        ));
        // Foreign faction blocked by own-faction closed door.
        assert!(!has_vision_los(
            chunk_map,
            wall_map,
            door_map,
            &edge_map,
            &wall_q_data,
            (0, 0, 0),
            (10, 0, 0),
            2,
        ));
    }

    /// `compute_vision_set` includes the origin tile + the 1-tile chebyshev
    /// ring even with no LOS support; on a flat map it covers the full
    /// circle.
    #[test]
    fn compute_vision_set_covers_flat_radius() {
        let mut app = App::new();
        app.insert_resource(flat_chunk_map(TileKind::Grass));
        app.insert_resource(WallMap::default());
        app.insert_resource(DoorMap::default());

        let mut state: bevy::ecs::system::SystemState<(
            Res<ChunkMap>,
            Res<WallMap>,
            Res<DoorMap>,
            Query<&Wall>,
        )> = bevy::ecs::system::SystemState::new(app.world_mut());
        let (chunk_map, wall_map, door_map, wall_q_data) = state.get(app.world());
        let chunk_map = &*chunk_map;
        let wall_map = &*wall_map;
        let door_map = &*door_map;
        let edge_map = crate::simulation::construction::EdgeStructureMap::default();

        let mut out: AHashSet<(i32, i32)> = AHashSet::default();
        compute_vision_set(
            &mut out,
            chunk_map,
            wall_map,
            door_map,
            &edge_map,
            &wall_q_data,
            (10, 10, 0),
            5,
            1,
        );
        assert!(out.contains(&(10, 10)));
        assert!(out.contains(&(15, 10)));
        assert!(out.contains(&(10, 15)));
        // Outside the radius — never visible.
        assert!(!out.contains(&(20, 10)));
    }
}
