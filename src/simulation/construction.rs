use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::simulation::faction::{FactionRegistry, FactionTechs, SOLO};
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::plan::ActivePlan;
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::TaskKind;
use crate::simulation::technology::{CITY_STATE_ORG, PERM_SETTLEMENT};
use crate::world::chunk::ChunkMap;
use crate::world::terrain::tile_to_world;
use crate::world::tile::{TileData, TileKind};
use ahash::AHashMap;
use bevy::prelude::*;

pub const TICKS_BUILD_WALL: u8 = 60;
pub const TICKS_BUILD_BED: u8 = 80;
/// Safety cap: prevents the blueprint queue growing unbounded due to bugs.
pub const MAX_BLUEPRINTS_SAFETY_CAP: usize = 20;
pub const WALL_WOOD_COST: u8 = 2;
pub const BED_WOOD_COST: u8 = 3;

/// Global toggle: when false, agents skip the Build goal entirely.
#[derive(Resource)]
pub struct AutonomousBuildingToggle(pub bool);

/// Maps tile positions to bed entities placed there.
#[derive(Resource, Default)]
pub struct BedMap(pub AHashMap<(i16, i16), Entity>);

/// Maps tile positions to wall entities placed there.
#[derive(Resource, Default)]
pub struct WallMap(pub AHashMap<(i16, i16), Entity>);

/// Maps tile positions to active Blueprint entities (faction build reservations).
#[derive(Resource, Default)]
pub struct BlueprintMap(pub AHashMap<(i16, i16), Entity>);

/// Marker placed on completed bed entities.
#[derive(Component)]
pub struct Bed;

/// Marker placed on completed wall entities.
#[derive(Component)]
pub struct Wall;

/// What kind of structure is being built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildSiteKind {
    Wall,
    Bed,
}

/// A faction-reserved construction site. Agents converge on Blueprint entities
/// to deposit wood and contribute build progress. Despawned when construction completes.
#[derive(Component)]
pub struct Blueprint {
    pub faction_id: u32,
    pub kind: BuildSiteKind,
    pub tile: (i16, i16),
    pub wood_needed: u8,
    pub wood_deposited: u8,
    pub build_progress: u8,
}

/// Count how many of the 4 cardinal directions have a wall (or higher-z terrain)
/// within 3 tiles. Score range: 0–4.
pub fn enclosure_score(chunk_map: &ChunkMap, tx: i32, ty: i32) -> u8 {
    let agent_z = chunk_map.surface_z_at(tx, ty);
    let mut score = 0u8;
    for (dx, dy) in [(-1i32, 0), (1, 0), (0, -1i32), (0, 1)] {
        for step in 1..=3i32 {
            let nx = tx + dx * step;
            let ny = ty + dy * step;
            let kind_wall = chunk_map.tile_kind_at(nx, ny) == Some(TileKind::Wall);
            let z_higher = chunk_map.surface_z_at(nx, ny) > agent_z;
            if kind_wall || z_higher {
                score += 1;
                break;
            }
        }
    }
    score
}

// ── Construction phase ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConstructionPhase {
    /// Scattered lean-tos near a hearth. No walls.
    PrehistoricBand,
    /// Individual walled huts clustering organically, then a palisade wraps them.
    NeolithicVillage,
    /// Larger longhouses in rows, heavier outer wall.
    BronzeAgeTown,
}

fn determine_phase(_member_count: u32, techs: &FactionTechs) -> ConstructionPhase {
    if techs.has(CITY_STATE_ORG) {
        ConstructionPhase::BronzeAgeTown
    } else if techs.has(PERM_SETTLEMENT) {
        ConstructionPhase::NeolithicVillage
    } else {
        ConstructionPhase::PrehistoricBand
    }
}

// ── Placement helpers ─────────────────────────────────────────────────────────

fn count_beds_near(bed_map: &BedMap, home: (i16, i16), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    bed_map
        .0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

/// Phase 0: place a bed near the camp center with a small gap from existing beds.
fn find_organic_bed_site(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    camp_home: (i16, i16),
    max_radius: i32,
) -> Option<(i16, i16)> {
    let (hx, hy) = (camp_home.0 as i32, camp_home.1 as i32);
    for dist in 1..=max_radius {
        for dy in -dist..=dist {
            for dx in -dist..=dist {
                if dx.abs().max(dy.abs()) != dist {
                    continue;
                }
                let tx = hx + dx;
                let ty = hy + dy;
                let pos = (tx as i16, ty as i16);
                if bp_map.0.contains_key(&pos) {
                    continue;
                }
                if bed_map.0.contains_key(&pos) {
                    continue;
                }
                let Some(kind) = chunk_map.tile_kind_at(tx, ty) else {
                    continue;
                };
                if !kind.is_passable() {
                    continue;
                }
                // Keep a 1-tile gap so the scatter feels organic, not wall-to-wall.
                let adj_bed = [(-1i32, 0), (1, 0), (0, -1i32), (0, 1)]
                    .iter()
                    .any(|(ddx, ddy)| {
                        bed_map
                            .0
                            .contains_key(&((tx + ddx) as i16, (ty + ddy) as i16))
                    });
                if !adj_bed {
                    return Some(pos);
                }
            }
        }
    }
    None
}

/// Returns true if every tile in the half_w × half_h footprint centred at (cx,cy)
/// is passable, not a wall, and not reserved by an existing bed or blueprint.
fn is_clear_footprint(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    cx: i32,
    cy: i32,
    half_w: i32,
    half_h: i32,
) -> bool {
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            let pos = ((cx + dx) as i16, (cy + dy) as i16);
            if bp_map.0.contains_key(&pos) {
                return false;
            }
            if bed_map.0.contains_key(&pos) {
                return false;
            }
            let Some(kind) = chunk_map.tile_kind_at(cx + dx, cy + dy) else {
                return false;
            };
            if !kind.is_passable() || kind == TileKind::Wall {
                return false;
            }
        }
    }
    true
}

/// Returns true if any wall tile or bed exists within `radius` tiles of the
/// expanded bounding box of the footprint — i.e. there is something to attach to.
fn has_nearby_structure(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    cx: i32,
    cy: i32,
    half_w: i32,
    half_h: i32,
    radius: i32,
) -> bool {
    let outer_w = half_w + radius;
    let outer_h = half_h + radius;
    for dy in -outer_h..=outer_h {
        for dx in -outer_w..=outer_w {
            if dy.abs() <= half_h && dx.abs() <= half_w {
                continue;
            } // skip own footprint
            let nx = cx + dx;
            let ny = cy + dy;
            if bed_map.0.contains_key(&(nx as i16, ny as i16)) {
                return true;
            }
            if chunk_map.tile_kind_at(nx, ny) == Some(TileKind::Wall) {
                return true;
            }
        }
    }
    false
}

/// Phase 1/2: find the center of a clear (2·half_w+1) × (2·half_h+1) footprint.
/// Returns a site near the camp center for the first few buildings, then expands
/// outward organically (adjacent to existing structures).
fn find_building_origin(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    camp_home: (i16, i16),
    half_w: i32,
    half_h: i32,
    max_radius: i32,
) -> Option<(i16, i16)> {
    let (hx, hy) = (camp_home.0 as i32, camp_home.1 as i32);
    let min_ring = half_w.max(half_h) + 1;
    let early_ring = min_ring + 3; // within this ring: always accept a clear footprint

    for ring in min_ring..=max_radius {
        for dy in -ring..=ring {
            for dx in -ring..=ring {
                if dx.abs().max(dy.abs()) != ring {
                    continue;
                }
                let cx = hx + dx;
                let cy = hy + dy;
                if !is_clear_footprint(chunk_map, bed_map, bp_map, cx, cy, half_w, half_h) {
                    continue;
                }
                if ring <= early_ring {
                    return Some((cx as i16, cy as i16));
                }
                // Beyond the seeding zone, grow organically: require adjacency.
                if has_nearby_structure(chunk_map, bed_map, cx, cy, half_w, half_h, 2) {
                    return Some((cx as i16, cy as i16));
                }
            }
        }
    }
    None
}

/// Phase 1/2: plan all wall and bed blueprints for a single rectangular building.
/// The perimeter wall tile closest to camp_home becomes the entrance (left open).
fn plan_building(
    commands: &mut Commands,
    bp_map: &mut BlueprintMap,
    cx: i32,
    cy: i32,
    half_w: i32,
    half_h: i32,
    faction_id: u32,
    camp_home: (i16, i16),
    interior_beds: &[(i32, i32)],
) {
    let (hx, hy) = (camp_home.0 as i32, camp_home.1 as i32);

    // The perimeter tile whose world-position is closest to the camp center becomes entrance.
    let entrance: (i32, i32) = {
        let mut best = (0i32, half_h);
        let mut best_dist = i64::MAX;
        for dy in -half_h..=half_h {
            for dx in -half_w..=half_w {
                if dx.abs() < half_w && dy.abs() < half_h {
                    continue;
                } // interior
                let d = ((cx + dx - hx) as i64).pow(2) + ((cy + dy - hy) as i64).pow(2);
                if d < best_dist {
                    best_dist = d;
                    best = (dx, dy);
                }
            }
        }
        best
    };

    // Walls: all perimeter tiles except the entrance.
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            if dx.abs() < half_w && dy.abs() < half_h {
                continue;
            } // interior — beds go here
            if (dx, dy) == entrance {
                continue;
            }
            let tile = ((cx + dx) as i16, (cy + dy) as i16);
            if bp_map.0.contains_key(&tile) {
                continue;
            }
            let wp = tile_to_world(cx + dx, cy + dy);
            let e = commands
                .spawn((
                    Blueprint {
                        faction_id,
                        kind: BuildSiteKind::Wall,
                        tile,
                        wood_needed: WALL_WOOD_COST,
                        wood_deposited: 0,
                        build_progress: 0,
                    },
                    Transform::from_xyz(wp.x, wp.y, 0.3),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            bp_map.0.insert(tile, e);
        }
    }

    // Beds: at the specified interior offsets.
    for &(bdx, bdy) in interior_beds {
        let tile = ((cx + bdx) as i16, (cy + bdy) as i16);
        if bp_map.0.contains_key(&tile) {
            continue;
        }
        let wp = tile_to_world(cx + bdx, cy + bdy);
        let e = commands
            .spawn((
                Blueprint {
                    faction_id,
                    kind: BuildSiteKind::Bed,
                    tile,
                    wood_needed: BED_WOOD_COST,
                    wood_deposited: 0,
                    build_progress: 0,
                },
                Transform::from_xyz(wp.x, wp.y, 0.3),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ))
            .id();
        bp_map.0.insert(tile, e);
    }
}

/// Phase 1/2: find a single open slot on the rectangular palisade that wraps the
/// settlement's bed bounding box plus a buffer. Returns None when the palisade is
/// complete or no beds exist near camp.
fn find_palisade_site(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    camp_home: (i16, i16),
    buffer: i32,
) -> Option<(i16, i16)> {
    let (hx, hy) = (camp_home.0 as i32, camp_home.1 as i32);
    let search = 25i32;

    let mut min_x = i32::MAX;
    let mut max_x = i32::MIN;
    let mut min_y = i32::MAX;
    let mut max_y = i32::MIN;
    for &pos in bed_map.0.keys() {
        let (bx, by) = (pos.0 as i32, pos.1 as i32);
        if (bx - hx).abs() > search || (by - hy).abs() > search {
            continue;
        }
        min_x = min_x.min(bx);
        max_x = max_x.max(bx);
        min_y = min_y.min(by);
        max_y = max_y.max(by);
    }
    if min_x == i32::MAX {
        return None;
    }

    min_x -= buffer;
    max_x += buffer;
    min_y -= buffer;
    max_y += buffer;

    // Top and bottom rows.
    for x in min_x..=max_x {
        for &y in &[min_y, max_y] {
            let tile = (x as i16, y as i16);
            if bp_map.0.contains_key(&tile) {
                continue;
            }
            let Some(kind) = chunk_map.tile_kind_at(x, y) else {
                continue;
            };
            if !kind.is_passable() || kind == TileKind::Wall {
                continue;
            }
            return Some(tile);
        }
    }
    // Left and right columns (excluding corners already checked).
    for y in (min_y + 1)..max_y {
        for &x in &[min_x, max_x] {
            let tile = (x as i16, y as i16);
            if bp_map.0.contains_key(&tile) {
                continue;
            }
            let Some(kind) = chunk_map.tile_kind_at(x, y) else {
                continue;
            };
            if !kind.is_passable() || kind == TileKind::Wall {
                continue;
            }
            return Some(tile);
        }
    }
    None
}

// ── Blueprint planning system ─────────────────────────────────────────────────

/// Maintains the faction build queue each 60 ticks. Plans one project at a time:
///   Phase 0 (prehistoric):  one scattered bed near camp, no walls.
///   Phase 1 (neolithic):    one 3×3 walled hut (8 walls + 1 bed); once beds
///                           cover the population, batches of 4 palisade walls.
///   Phase 2 (bronze age):   5×3 longhouse (11 walls + 2 beds); wider palisade.
pub fn faction_blueprint_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    auto_build: Res<AutonomousBuildingToggle>,
    chunk_map: Res<ChunkMap>,
    faction_registry: Res<FactionRegistry>,
    bed_map: Res<BedMap>,
    mut bp_map: ResMut<BlueprintMap>,
    bp_query: Query<&Blueprint>,
) {
    if clock.tick % 60 != 0 || !auto_build.0 {
        return;
    }

    let mut faction_bp_count: AHashMap<u32, usize> = AHashMap::new();
    for &bp_entity in bp_map.0.values() {
        if let Ok(bp) = bp_query.get(bp_entity) {
            *faction_bp_count.entry(bp.faction_id).or_insert(0) += 1;
        }
    }

    let factions: Vec<(u32, (i16, i16), u32, FactionTechs)> = faction_registry
        .factions
        .iter()
        .filter(|(&id, _)| id != SOLO)
        .map(|(&id, fd)| (id, fd.home_tile, fd.member_count, fd.techs.clone()))
        .collect();

    for (faction_id, home, member_count, techs) in factions {
        let count = faction_bp_count.get(&faction_id).copied().unwrap_or(0);
        if count >= MAX_BLUEPRINTS_SAFETY_CAP || member_count == 0 {
            continue;
        }
        // One project at a time: wait until all current blueprints are built.
        if count > 0 {
            continue;
        }

        match determine_phase(member_count, &techs) {
            ConstructionPhase::PrehistoricBand => {
                if let Some(tile) = find_organic_bed_site(&chunk_map, &bed_map, &bp_map, home, 4) {
                    let wp = tile_to_world(tile.0 as i32, tile.1 as i32);
                    let e = commands
                        .spawn((
                            Blueprint {
                                faction_id,
                                kind: BuildSiteKind::Bed,
                                tile,
                                wood_needed: BED_WOOD_COST,
                                wood_deposited: 0,
                                build_progress: 0,
                            },
                            Transform::from_xyz(wp.x, wp.y, 0.3),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    bp_map.0.insert(tile, e);
                }
            }

            ConstructionPhase::NeolithicVillage => {
                let beds_near = count_beds_near(&bed_map, home, 20);
                let beds_needed = (member_count as f32 * 0.8) as usize;

                if beds_near < beds_needed {
                    // Build a 3×3 hut: 8 walls + 1 bed.
                    if let Some(origin) =
                        find_building_origin(&chunk_map, &bed_map, &bp_map, home, 1, 1, 15)
                    {
                        plan_building(
                            &mut commands,
                            &mut bp_map,
                            origin.0 as i32,
                            origin.1 as i32,
                            1,
                            1,
                            faction_id,
                            home,
                            &[(0, 0)],
                        );
                    }
                } else if beds_near > 0 {
                    // Beds are sufficient; build a segment of the palisade.
                    let mut planned = 0;
                    while planned < 4 {
                        let Some(tile) = find_palisade_site(&chunk_map, &bed_map, &bp_map, home, 2)
                        else {
                            break;
                        };
                        let wp = tile_to_world(tile.0 as i32, tile.1 as i32);
                        let e = commands
                            .spawn((
                                Blueprint {
                                    faction_id,
                                    kind: BuildSiteKind::Wall,
                                    tile,
                                    wood_needed: WALL_WOOD_COST,
                                    wood_deposited: 0,
                                    build_progress: 0,
                                },
                                Transform::from_xyz(wp.x, wp.y, 0.3),
                                GlobalTransform::default(),
                                Visibility::Visible,
                                InheritedVisibility::default(),
                            ))
                            .id();
                        bp_map.0.insert(tile, e);
                        planned += 1;
                    }
                }
            }

            ConstructionPhase::BronzeAgeTown => {
                let beds_near = count_beds_near(&bed_map, home, 30);
                let beds_needed = (member_count as f32 * 0.8) as usize;

                if beds_near < beds_needed {
                    // Prefer a 5×3 longhouse (11 walls + 2 beds); fall back to 3×3 hut.
                    if let Some(origin) =
                        find_building_origin(&chunk_map, &bed_map, &bp_map, home, 2, 1, 20)
                    {
                        plan_building(
                            &mut commands,
                            &mut bp_map,
                            origin.0 as i32,
                            origin.1 as i32,
                            2,
                            1,
                            faction_id,
                            home,
                            &[(-1, 0), (1, 0)],
                        );
                    } else if let Some(origin) =
                        find_building_origin(&chunk_map, &bed_map, &bp_map, home, 1, 1, 20)
                    {
                        plan_building(
                            &mut commands,
                            &mut bp_map,
                            origin.0 as i32,
                            origin.1 as i32,
                            1,
                            1,
                            faction_id,
                            home,
                            &[(0, 0)],
                        );
                    }
                } else if beds_near > 0 {
                    // Build a segment of the heavier outer wall (4-tile buffer).
                    let mut planned = 0;
                    while planned < 4 {
                        let Some(tile) = find_palisade_site(&chunk_map, &bed_map, &bp_map, home, 4)
                        else {
                            break;
                        };
                        let wp = tile_to_world(tile.0 as i32, tile.1 as i32);
                        let e = commands
                            .spawn((
                                Blueprint {
                                    faction_id,
                                    kind: BuildSiteKind::Wall,
                                    tile,
                                    wood_needed: WALL_WOOD_COST,
                                    wood_deposited: 0,
                                    build_progress: 0,
                                },
                                Transform::from_xyz(wp.x, wp.y, 0.3),
                                GlobalTransform::default(),
                                Visibility::Visible,
                                InheritedVisibility::default(),
                            ))
                            .id();
                        bp_map.0.insert(tile, e);
                        planned += 1;
                    }
                }
            }
        }
    }
}

/// Handles agents building at Blueprint entities (TaskKind::Construct / ConstructBed).
/// Multiple agents can contribute wood and labor to the same blueprint each tick.
/// Runs in Sequential set after gather_system.
pub fn construction_system(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut bed_map: ResMut<BedMap>,
    mut wall_map: ResMut<WallMap>,
    mut bp_map: ResMut<BlueprintMap>,
    clock: Res<SimClock>,
    mut bp_query: Query<&mut Blueprint>,
    mut agent_query: Query<(
        Entity,
        &mut PersonAI,
        &mut EconomicAgent,
        &mut Skills,
        &BucketSlot,
        &LodLevel,
        Option<&mut ActivePlan>,
    )>,
) {
    // Pass 1: collect pending contributions from Working agents.
    // Map: bp_entity → Vec<(agent_entity, wood_available)>
    let mut bp_pending: AHashMap<Entity, Vec<(Entity, u32)>> = AHashMap::new();

    for (entity, mut ai, agent, mut skills, slot, lod, _) in agent_query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }
        if ai.task_id != TaskKind::Construct as u16 && ai.task_id != TaskKind::ConstructBed as u16 {
            continue;
        }

        let Some(bp_entity) = ai.target_entity else {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            continue;
        };

        skills.gain_xp(SkillKind::Building, 1);
        let wood = agent.quantity_of(Good::Wood);
        bp_pending
            .entry(bp_entity)
            .or_default()
            .push((entity, wood));
    }

    if bp_pending.is_empty() {
        return;
    }

    let mut completed_agents: Vec<Entity> = Vec::new();
    let mut orphaned_agents: Vec<Entity> = Vec::new();
    // (agent_entity, wood_to_remove)
    let mut wood_removals: Vec<(Entity, u32)> = Vec::new();

    // Pass 2: apply contributions to blueprints, check completion.
    for (bp_entity, workers) in &bp_pending {
        let Ok(mut bp) = bp_query.get_mut(*bp_entity) else {
            orphaned_agents.extend(workers.iter().map(|(e, _)| *e));
            continue;
        };

        // Greedily deposit wood from workers until blueprint is satisfied.
        let mut still_needed = bp.wood_needed.saturating_sub(bp.wood_deposited) as u32;
        for &(agent_e, wood_available) in workers {
            if still_needed == 0 {
                break;
            }
            let give = wood_available.min(still_needed);
            if give > 0 {
                wood_removals.push((agent_e, give));
                bp.wood_deposited = bp.wood_deposited.saturating_add(give as u8);
                still_needed -= give;
            }
        }

        // Each worker contributes one tick of build progress.
        bp.build_progress = bp.build_progress.saturating_add(workers.len() as u8);

        let threshold = match bp.kind {
            BuildSiteKind::Wall => TICKS_BUILD_WALL,
            BuildSiteKind::Bed => TICKS_BUILD_BED,
        };

        if bp.build_progress >= threshold && bp.wood_deposited >= bp.wood_needed {
            let tile = bp.tile;
            let (tx, ty) = (tile.0 as i32, tile.1 as i32);

            match bp.kind {
                BuildSiteKind::Wall => {
                    let surf_z = chunk_map.surface_z_at(tx, ty);
                    chunk_map.set_tile(
                        tx,
                        ty,
                        surf_z + 1,
                        TileData {
                            kind: TileKind::Wall,
                            elevation: 0,
                            fertility: 0,
                            flags: 0b0001,
                        },
                    );

                    let world_pos = tile_to_world(tx, ty);
                    let wall_entity = commands
                        .spawn((
                            Wall,
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    wall_map.0.insert(tile, wall_entity);
                }
                BuildSiteKind::Bed => {
                    let world_pos = tile_to_world(tx, ty);
                    let bed_entity = commands
                        .spawn((
                            Bed,
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    bed_map.0.insert(tile, bed_entity);
                }
            }

            bp_map.0.remove(&tile);
            commands.entity(*bp_entity).despawn_recursive();

            completed_agents.extend(workers.iter().map(|(e, _)| *e));
        }
    }

    if wood_removals.is_empty() && completed_agents.is_empty() && orphaned_agents.is_empty() {
        return;
    }

    // Pass 3: remove wood from assigned agents and reset completed/orphaned agents.
    for (entity, mut ai, mut agent, _, _, _, mut plan_opt) in agent_query.iter_mut() {
        // Apply any wood removal from this pass.
        for &(ae, qty) in &wood_removals {
            if ae == entity {
                agent.remove_good(Good::Wood, qty);
                break;
            }
        }

        let is_completed = completed_agents.contains(&entity);
        let is_orphaned = orphaned_agents.contains(&entity);

        if is_completed || is_orphaned {
            if is_completed {
                if let Some(ref mut plan) = plan_opt {
                    plan.reward_acc += if ai.task_id == TaskKind::ConstructBed as u16 {
                        2.0
                    } else {
                        1.0
                    };
                }
            }
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;
            ai.work_progress = 0;
        }
    }
}
