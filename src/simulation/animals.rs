use super::combat::{Body, CombatCooldown, CombatTarget, Health};
use super::lod::LodLevel;
use super::person::Person;
use super::schedule::{BucketSlot, SimClock};
use crate::simulation::line_of_sight::has_los;
use crate::world::chunk::{ChunkMap, CHUNK_SIZE};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, TILE_SIZE, WORLD_CHUNKS_X, WORLD_CHUNKS_Y};
use crate::world::tile::TileKind;
use bevy::prelude::*;
use rand::Rng;
use std::time::Instant;

const WOLF_COUNT: u32 = 150;
const DEER_COUNT: u32 = 400;
const ANIMAL_SPEED: f32 = 32.0; // pixels/sec, slower than persons

#[derive(Component)]
pub struct Wolf;

#[derive(Component)]
pub struct Deer;

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AnimalState {
    #[default]
    Wander = 0,
    Chase = 1,
    Flee = 2,
    Attack = 3,
}

#[derive(Component, Clone, Copy, Default)]
pub struct AnimalAI {
    pub state: AnimalState,
    pub target_tile: (i16, i16),
    pub target_entity: Option<Entity>,
    pub wander_timer: f32,
}

const WANDER_INTERVAL: f32 = 3.0;

pub fn spawn_animals(
    mut commands: Commands,
    chunk_map: Res<ChunkMap>,
    mut clock: ResMut<SimClock>,
) {
    let now = Instant::now();
    use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};

    let start_cx = (GLOBE_WIDTH / 2) * GLOBE_CELL_CHUNKS;
    let start_cy = (GLOBE_HEIGHT / 2) * GLOBE_CELL_CHUNKS;

    let start_tx = start_cx * CHUNK_SIZE as i32;
    let start_ty = start_cy * CHUNK_SIZE as i32;

    let total_x = WORLD_CHUNKS_X * CHUNK_SIZE as i32;
    let total_y = WORLD_CHUNKS_Y * CHUNK_SIZE as i32;

    // Use random sampling instead of O(N) full world scan to find spawn tiles.
    let mut forest_tiles: Vec<(i32, i32)> = Vec::new();
    let mut grass_tiles: Vec<(i32, i32)> = Vec::new();
    let mut rng = rand::thread_rng();

    for _ in 0..10000 {
        let tx = start_tx + rng.gen_range(0..total_x);
        let ty = start_ty + rng.gen_range(0..total_y);
        if !chunk_map.is_passable(tx, ty) {
            continue;
        }
        match chunk_map.tile_kind_at(tx, ty) {
            Some(TileKind::Forest) => {
                if forest_tiles.len() < WOLF_COUNT as usize * 2 {
                    forest_tiles.push((tx, ty));
                }
            }
            Some(TileKind::Grass) => {
                if grass_tiles.len() < DEER_COUNT as usize * 2 {
                    grass_tiles.push((tx, ty));
                }
            }
            _ => {}
        }
        if forest_tiles.len() >= WOLF_COUNT as usize * 2
            && grass_tiles.len() >= DEER_COUNT as usize * 2
        {
            break;
        }
    }

    info!(
        "Animal spawn tiles found: {} forest, {} grass in {:?}",
        forest_tiles.len(),
        grass_tiles.len(),
        now.elapsed()
    );

    if forest_tiles.is_empty() || grass_tiles.is_empty() {
        warn!("spawn_animals: could not find enough forest or grass tiles via random sampling!");
    }

    let mut slot = clock.population;

    // Spawn wolves on forest tiles
    let wolf_step = (forest_tiles.len() / WOLF_COUNT as usize).max(1);
    for i in 0..WOLF_COUNT as usize {
        let idx = (i * wolf_step) % forest_tiles.len().max(1);
        if idx >= forest_tiles.len() {
            break;
        }
        let (tx, ty) = forest_tiles[idx];
        let pos = tile_to_world(tx, ty);
        commands.spawn((
            Wolf,
            Transform::from_xyz(pos.x, pos.y, 1.0),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
            AnimalAI {
                target_tile: (tx as i16, ty as i16),
                wander_timer: i as f32 * 0.05,
                ..Default::default()
            },
            Health::new(30),
            CombatTarget::default(),
            CombatCooldown::default(),
            LodLevel::Full,
            BucketSlot(slot),
        ));
        slot += 1;
    }

    // Spawn deer on grass tiles
    let deer_step = (grass_tiles.len() / DEER_COUNT as usize).max(1);
    for i in 0..DEER_COUNT as usize {
        let idx = (i * deer_step) % grass_tiles.len().max(1);
        if idx >= grass_tiles.len() {
            break;
        }
        let (tx, ty) = grass_tiles[idx];
        let pos = tile_to_world(tx, ty);
        commands.spawn((
            Deer,
            Transform::from_xyz(pos.x, pos.y, 1.0),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
            AnimalAI {
                target_tile: (tx as i16, ty as i16),
                wander_timer: i as f32 * 0.02,
                ..Default::default()
            },
            Health::new(20),
            CombatTarget::default(),
            CombatCooldown::default(),
            LodLevel::Full,
            BucketSlot(slot),
            crate::simulation::plants::DeerGrazer {
                graze_timer: fastrand::u16(0..120),
            },
        ));
        slot += 1;
    }

    clock.population = slot;
    clock.current_end = clock.bucket_size.min(slot);
}

pub fn animal_movement_system(
    time: Res<Time>,
    chunk_map: Res<ChunkMap>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    mut query: Query<(&mut Transform, &mut AnimalAI, &LodLevel, &BucketSlot), Without<Person>>,
    clock: Res<SimClock>,
) {
    let dt = time.delta_secs();
    let sim_dt = dt * clock.scale_factor();

    for (mut transform, mut ai, lod, slot) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if ai.state == AnimalState::Attack {
            continue; // combat_system handles this
        }

        let pos = transform.translation.truncate();
        let target_world = tile_to_world(ai.target_tile.0 as i32, ai.target_tile.1 as i32);
        let to_target = target_world - pos;
        let dist = to_target.length();

        if dist > 2.0 {
            let dir = to_target.normalize();
            let step = dir * ANIMAL_SPEED * dt * clock.speed;
            let new_pos = pos + step;
            transform.translation.x = new_pos.x;
            transform.translation.y = new_pos.y;
        } else {
            // Arrived — snap and pick next wander tile if in Wander/Flee state
            transform.translation.x = target_world.x;
            transform.translation.y = target_world.y;

            if matches!(ai.state, AnimalState::Wander | AnimalState::Flee) {
                // Bucketed wander logic
                if clock.is_active(slot.0) {
                    ai.wander_timer -= sim_dt;
                    if ai.wander_timer <= 0.0 {
                        ai.wander_timer = WANDER_INTERVAL;
                        ai.state = AnimalState::Wander;
                        ai.target_entity = None;

                        let cur_tx = (pos.x / TILE_SIZE).floor() as i32;
                        let cur_ty = (pos.y / TILE_SIZE).floor() as i32;

                        // Pick random adjacent passable tile that no other agent occupies.
                        let dirs: [(i32, i32); 8] = [
                            (-1, 0),
                            (1, 0),
                            (0, -1),
                            (0, 1),
                            (-1, -1),
                            (1, -1),
                            (-1, 1),
                            (1, 1),
                        ];
                        let start = fastrand::usize(..8);
                        for i in 0..8 {
                            let (dx, dy) = dirs[(start + i) % 8];
                            let ntx = cur_tx + dx;
                            let nty = cur_ty + dy;
                            let ntz = chunk_map.surface_z_at(ntx, nty);
                            if chunk_map.is_passable(ntx, nty)
                                && !spatial.agent_occupied(ntx, nty, ntz)
                            {
                                ai.target_tile = (ntx as i16, nty as i16);
                                break;
                            }
                        }
                    }
                }
            } else if ai.state == AnimalState::Chase {
                // Target moved — sense system will update target_tile next frame
                ai.state = AnimalState::Wander;
            }
        }
    }
}

/// Wolves chase deer/lone humans; deer flee from wolves.
/// Runs in ParallelA — writes only AnimalAI on self.
pub fn animal_sense_system(
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    wolf_query: Query<(Entity, &Transform, &BucketSlot, &LodLevel), With<Wolf>>,
    deer_query: Query<(Entity, &Transform, &BucketSlot, &LodLevel), With<Deer>>,
    person_query: Query<&Transform, With<Person>>,
    mut ai_query: Query<(&mut AnimalAI, &mut CombatTarget)>,
    target_query: Query<(&Transform, Option<&Health>, Option<&Body>)>,
) {
    const WOLF_HUNT_RADIUS: i32 = 12;
    const DEER_FLEE_RADIUS: i32 = 8;
    const LONE_HUMAN_RADIUS: i32 = 3; // humans within this radius of each other = "group"

    // Wolf sense: find deer or lone humans
    for (wolf_entity, transform, slot, lod) in &wolf_query {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let Ok((mut ai, mut combat_target)) = ai_query.get_mut(wolf_entity) else {
            continue;
        };

        // If already chasing/attacking a valid target, keep it
        if let Some(existing) = ai.target_entity {
            if ai.state == AnimalState::Chase || ai.state == AnimalState::Attack {
                if let Ok((prey_transform, health, body)) = target_query.get(existing) {
                    let is_dead = match (health, body) {
                        (Some(h), _) if h.is_dead() => true,
                        (_, Some(b)) if b.is_dead() => true,
                        _ => false,
                    };
                    if is_dead {
                        ai.state = AnimalState::Wander;
                        ai.target_entity = None;
                        combat_target.0 = None;
                    } else {
                        // Update target tile to prey's current position
                        let ptx = (prey_transform.translation.x / TILE_SIZE).floor() as i16;
                        let pty = (prey_transform.translation.y / TILE_SIZE).floor() as i16;
                        ai.target_tile = (ptx, pty);

                        // Check adjacency for attack
                        let target_tile = ai.target_tile;
                        let dist =
                            (target_tile.0 as i32 - tx).abs() + (target_tile.1 as i32 - ty).abs();
                        if dist <= 1 {
                            ai.state = AnimalState::Attack;
                            combat_target.0 = Some(existing);
                        } else {
                            ai.state = AnimalState::Chase;
                        }
                        continue;
                    }
                } else {
                    ai.state = AnimalState::Wander;
                    ai.target_entity = None;
                    combat_target.0 = None;
                }
            }
        }

        // Scan for prey
        let mut found: Option<(Entity, i16, i16)> = None;

        'scan: for dy in -WOLF_HUNT_RADIUS..=WOLF_HUNT_RADIUS {
            for dx in -WOLF_HUNT_RADIUS..=WOLF_HUNT_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    if candidate == wolf_entity {
                        continue;
                    }

                    let Ok((_, health, body)) = target_query.get(candidate) else {
                        continue;
                    };
                    let is_dead = match (health, body) {
                        (Some(h), _) if h.is_dead() => true,
                        (_, Some(b)) if b.is_dead() => true,
                        _ => false,
                    };
                    if is_dead {
                        continue;
                    }

                    // Terrain must not block sight between wolf and prey.
                    if !has_los(&chunk_map, (tx, ty), (tx + dx, ty + dy)) {
                        continue;
                    }

                    // Prefer deer
                    if deer_query.contains(candidate) {
                        found = Some((candidate, (tx + dx) as i16, (ty + dy) as i16));
                        break 'scan;
                    }

                    // Lone human check
                    if person_query.get(candidate).is_ok() {
                        // Count other persons nearby
                        let mut nearby_persons = 0u32;
                        for ndy in -LONE_HUMAN_RADIUS..=LONE_HUMAN_RADIUS {
                            for ndx in -LONE_HUMAN_RADIUS..=LONE_HUMAN_RADIUS {
                                for &nb in spatial.get(tx + dx + ndx, ty + dy + ndy) {
                                    if nb != candidate && person_query.get(nb).is_ok() {
                                        nearby_persons += 1;
                                    }
                                }
                            }
                        }
                        if nearby_persons == 0 {
                            found = Some((candidate, (tx + dx) as i16, (ty + dy) as i16));
                        }
                    }
                }
            }
        }

        if let Some((prey, ptx, pty)) = found {
            ai.state = AnimalState::Chase;
            ai.target_entity = Some(prey);
            ai.target_tile = (ptx, pty);
        } else {
            if ai.state != AnimalState::Wander {
                ai.state = AnimalState::Wander;
                ai.target_entity = None;
                combat_target.0 = None;
            }
        }
    }

    // Deer sense: flee from wolves
    for (deer_entity, transform, slot, lod) in &deer_query {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let Ok((mut ai, _)) = ai_query.get_mut(deer_entity) else {
            continue;
        };

        let mut threat_dx = 0i32;
        let mut threat_dy = 0i32;
        let mut threat_count = 0i32;

        for dy in -DEER_FLEE_RADIUS..=DEER_FLEE_RADIUS {
            for dx in -DEER_FLEE_RADIUS..=DEER_FLEE_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    if wolf_query.contains(candidate) || person_query.get(candidate).is_ok() {
                        if has_los(&chunk_map, (tx, ty), (tx + dx, ty + dy)) {
                            threat_dx += dx;
                            threat_dy += dy;
                            threat_count += 1;
                        }
                    }
                }
            }
        }

        if threat_count > 0 {
            use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
            let total_tiles_x = GLOBE_WIDTH * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
            let total_tiles_y = GLOBE_HEIGHT * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;

            // Flee in opposite direction from average threat position
            let flee_tx = (tx - threat_dx / threat_count).clamp(0, total_tiles_x - 1);
            let flee_ty = (ty - threat_dy / threat_count).clamp(0, total_tiles_y - 1);
            ai.state = AnimalState::Flee;
            ai.target_tile = (flee_tx as i16, flee_ty as i16);
            ai.wander_timer = 1.5; // hold flee direction for 1.5s
        } else if ai.state == AnimalState::Flee {
            ai.state = AnimalState::Wander;
        }
    }
}
