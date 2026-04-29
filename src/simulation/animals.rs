use super::combat::{Body, CombatCooldown, CombatTarget, Health};
use super::lod::LodLevel;
use super::person::Person;
use super::reproduction::BiologicalSex;
use super::schedule::{BucketSlot, SimClock};
use crate::simulation::line_of_sight::has_los;
use crate::world::chunk::{ChunkMap, CHUNK_SIZE};
use crate::world::seasons::TICKS_PER_SEASON;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, TILE_SIZE, WORLD_CHUNKS_X, WORLD_CHUNKS_Y};
use crate::world::tile::TileKind;
use bevy::prelude::*;
use rand::Rng;
use std::time::Instant;

const WOLF_COUNT: u32 = 150;
const DEER_COUNT: u32 = 400;
const HORSE_COUNT: u32 = 200;
const HORSE_POP_CAP: usize = 300;
const HORSE_HP: u8 = 40;
const HORSE_REPRO_MALE_THRESHOLD: f32 = 160.0;
const HORSE_REPRO_FEMALE_THRESHOLD: f32 = 190.0;
const ANIMAL_SPEED: f32 = 32.0; // pixels/sec, slower than persons
const WANDER_INTERVAL: f32 = 3.0;

// Need rates
const ANIMAL_HUNGER_RATE: f32 = 0.03;
const ANIMAL_SLEEP_RATE: f32 = 0.25;
const ANIMAL_SLEEP_RECOVER_RATE: f32 = 2.5;
const ANIMAL_SLEEP_THRESHOLD: f32 = 180.0;
const ANIMAL_SLEEP_WAKE_THRESHOLD: f32 = 20.0;
const ANIMAL_REPRO_RATE: f32 = 0.04;
const ANIMAL_HUNGER_RECOVER_WOLF: f32 = 150.0;
pub const ANIMAL_HUNGER_RECOVER_DEER: f32 = 80.0;
const WOLF_REPRO_MALE_THRESHOLD: f32 = 150.0;
const WOLF_REPRO_FEMALE_THRESHOLD: f32 = 180.0;
const DEER_REPRO_MALE_THRESHOLD: f32 = 150.0;
const DEER_REPRO_FEMALE_THRESHOLD: f32 = 180.0;
const ANIMAL_BIRTH_CHANCE: u32 = 5; // out of 10,000
const WOLF_POP_CAP: usize = 250;
const DEER_POP_CAP: usize = 600;
const ANIMAL_BIRTH_COOLDOWN: u32 = TICKS_PER_SEASON * 2;
const REPRO_SEARCH_RADIUS: i32 = 3;

#[derive(Component)]
pub struct Wolf;

#[derive(Component)]
pub struct Deer;

#[derive(Component)]
pub struct Horse;

/// Placed on a horse once tamed by a faction.
/// The horse stops fleeing from persons and can be ridden.
#[derive(Component, Clone, Copy)]
pub struct Tamed {
    pub owner_faction: u32,
}

/// Placed on a horse while it is being ridden by a person.
/// Causes animal_movement_system to skip this entity (position managed by rider sync).
#[derive(Component, Clone, Copy)]
pub struct CarriedBy(pub Entity);

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AnimalState {
    #[default]
    Wander = 0,
    Chase = 1,
    Flee = 2,
    Attack = 3,
    Sleeping = 4,
}

#[derive(Component, Clone, Copy, Default)]
pub struct AnimalAI {
    pub state: AnimalState,
    pub target_tile: (i16, i16),
    pub target_entity: Option<Entity>,
    pub wander_timer: f32,
}

/// Lightweight biological needs for animals. Separate from the person Needs struct —
/// animals only track hunger, sleep, and reproduction.
#[derive(Component, Clone, Copy, Default)]
pub struct AnimalNeeds {
    pub hunger: f32,        // 0=satiated, 255=starving
    pub sleep: f32,         // 0=rested, 255=exhausted
    pub reproduction: f32,  // 0=not ready, 255=peak
}

/// Countdown in ticks before a female can give birth again.
#[derive(Component, Clone, Copy, Default)]
pub struct AnimalReproductionCooldown(pub u32);

pub fn spawn_animals(
    mut commands: Commands,
    chunk_map: Res<ChunkMap>,
    mut clock: ResMut<SimClock>,
) {
    let now = Instant::now();
    use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};

    let start_cx = ((GLOBE_WIDTH / 2) * GLOBE_CELL_CHUNKS) - (WORLD_CHUNKS_X / 2);
    let start_cy = ((GLOBE_HEIGHT / 2) * GLOBE_CELL_CHUNKS) - (WORLD_CHUNKS_Y / 2);

    let start_tx = start_cx * CHUNK_SIZE as i32;
    let start_ty = start_cy * CHUNK_SIZE as i32;

    let total_x = WORLD_CHUNKS_X * CHUNK_SIZE as i32;
    let total_y = WORLD_CHUNKS_Y * CHUNK_SIZE as i32;

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
            AnimalNeeds {
                hunger: fastrand::f32() * 60.0,
                sleep: fastrand::f32() * 40.0,
                reproduction: fastrand::f32() * 80.0,
            },
            AnimalReproductionCooldown(0),
            BiologicalSex::random(),
        ));
        slot += 1;
    }

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
            AnimalNeeds {
                hunger: fastrand::f32() * 60.0,
                sleep: fastrand::f32() * 40.0,
                reproduction: fastrand::f32() * 80.0,
            },
            AnimalReproductionCooldown(0),
            BiologicalSex::random(),
        ));
        slot += 1;
    }

    // Horses: spawn in grassland, staggered offset from deer to avoid overlap
    let horse_step = (grass_tiles.len() / HORSE_COUNT as usize).max(1);
    let horse_offset = grass_tiles.len() / 3;
    for i in 0..HORSE_COUNT as usize {
        let idx = (horse_offset + i * horse_step) % grass_tiles.len().max(1);
        if idx >= grass_tiles.len() {
            break;
        }
        let (tx, ty) = grass_tiles[idx];
        let pos = tile_to_world(tx, ty);
        commands.spawn((
            Horse,
            Transform::from_xyz(pos.x, pos.y, 1.0),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
            AnimalAI {
                target_tile: (tx as i16, ty as i16),
                wander_timer: i as f32 * 0.03,
                ..Default::default()
            },
            Health::new(HORSE_HP),
            CombatTarget::default(),
            CombatCooldown::default(),
            LodLevel::Full,
            BucketSlot(slot),
            AnimalNeeds {
                hunger: fastrand::f32() * 60.0,
                sleep: fastrand::f32() * 40.0,
                reproduction: fastrand::f32() * 80.0,
            },
            AnimalReproductionCooldown(0),
            BiologicalSex::random(),
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
    mut query: Query<(&mut Transform, &mut AnimalAI, &LodLevel, &BucketSlot, Option<&CarriedBy>), Without<Person>>,
    clock: Res<SimClock>,
) {
    let dt = time.delta_secs();
    let sim_dt = dt * clock.scale_factor();

    for (mut transform, mut ai, lod, slot, carried) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        // Ridden horses are positioned by horse_position_sync_system
        if carried.is_some() {
            continue;
        }
        if ai.state == AnimalState::Attack || ai.state == AnimalState::Sleeping {
            continue;
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
            transform.translation.x = target_world.x;
            transform.translation.y = target_world.y;

            if matches!(ai.state, AnimalState::Wander | AnimalState::Flee) {
                if clock.is_active(slot.0) {
                    ai.wander_timer -= sim_dt;
                    if ai.wander_timer <= 0.0 {
                        ai.wander_timer = WANDER_INTERVAL;
                        ai.state = AnimalState::Wander;
                        ai.target_entity = None;

                        let cur_tx = (pos.x / TILE_SIZE).floor() as i32;
                        let cur_ty = (pos.y / TILE_SIZE).floor() as i32;

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
                ai.state = AnimalState::Wander;
            }
        }
    }
}

/// Ticks animal needs and transitions the sleep state. Runs in ParallelA.
pub fn animal_needs_tick_system(
    time: Res<Time>,
    clock: Res<SimClock>,
    mut query: Query<
        (&BucketSlot, &LodLevel, &mut AnimalNeeds, &mut AnimalAI),
        bevy::prelude::Or<(With<Wolf>, With<Deer>, With<Horse>)>,
    >,
) {
    let dt = time.delta_secs() * clock.scale_factor();

    query.par_iter_mut().for_each(|(slot, lod, mut needs, mut ai)| {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            return;
        }

        needs.reproduction = (needs.reproduction + ANIMAL_REPRO_RATE * dt).clamp(0.0, 255.0);

        if ai.state == AnimalState::Sleeping {
            needs.sleep = (needs.sleep - ANIMAL_SLEEP_RECOVER_RATE * dt).clamp(0.0, 255.0);
            if needs.sleep <= ANIMAL_SLEEP_WAKE_THRESHOLD {
                ai.state = AnimalState::Wander;
            }
        } else {
            needs.hunger = (needs.hunger + ANIMAL_HUNGER_RATE * dt).clamp(0.0, 255.0);
            needs.sleep = (needs.sleep + ANIMAL_SLEEP_RATE * dt).clamp(0.0, 255.0);
            // Only sleep from Wander — never interrupt Chase/Flee/Attack
            if needs.sleep >= ANIMAL_SLEEP_THRESHOLD && ai.state == AnimalState::Wander {
                ai.state = AnimalState::Sleeping;
                ai.target_entity = None;
            }
        }
    });
}

/// Wolves chase deer/lone humans; deer flee from wolves; horses flee wolves and unknown persons.
/// Runs in ParallelA — writes only AnimalAI on self.
pub fn animal_sense_system(
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    door_map: Res<crate::simulation::construction::DoorMap>,
    wolf_query: Query<(Entity, &Transform, &BucketSlot, &LodLevel), With<Wolf>>,
    deer_query: Query<(Entity, &Transform, &BucketSlot, &LodLevel), With<Deer>>,
    horse_query: Query<(Entity, &Transform, &BucketSlot, &LodLevel, Option<&Tamed>), With<Horse>>,
    person_query: Query<&Transform, With<Person>>,
    mut ai_query: Query<(&mut AnimalAI, &mut CombatTarget, Option<&mut AnimalNeeds>)>,
    target_query: Query<(&Transform, Option<&Health>, Option<&Body>)>,
) {
    const WOLF_HUNT_RADIUS: i32 = 12;
    const DEER_FLEE_RADIUS: i32 = 8;
    const LONE_HUMAN_RADIUS: i32 = 3;

    // Wolf sense: find deer or lone humans
    for (wolf_entity, transform, slot, lod) in &wolf_query {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let Ok((mut ai, mut combat_target, mut animal_needs)) = ai_query.get_mut(wolf_entity)
        else {
            continue;
        };

        // Don't interrupt sleep
        if ai.state == AnimalState::Sleeping {
            continue;
        }

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
                        // Prey is dead — wolf ate
                        if let Some(ref mut needs) = animal_needs {
                            needs.hunger = (needs.hunger - ANIMAL_HUNGER_RECOVER_WOLF).max(0.0);
                        }
                    } else {
                        let ptx = (prey_transform.translation.x / TILE_SIZE).floor() as i16;
                        let pty = (prey_transform.translation.y / TILE_SIZE).floor() as i16;
                        ai.target_tile = (ptx, pty);

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
                    // Target entity gone from world — wolf ate or prey escaped
                    ai.state = AnimalState::Wander;
                    ai.target_entity = None;
                    combat_target.0 = None;
                    if let Some(ref mut needs) = animal_needs {
                        needs.hunger = (needs.hunger - ANIMAL_HUNGER_RECOVER_WOLF).max(0.0);
                    }
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

                    let z_from = chunk_map.surface_z_at(tx, ty) as i8;
                    let z_to = chunk_map.surface_z_at(tx + dx, ty + dy) as i8;
                    if !has_los(&chunk_map, &door_map, (tx, ty, z_from), (tx + dx, ty + dy, z_to)) {
                        continue;
                    }

                    // Prefer deer
                    if deer_query.contains(candidate) {
                        found = Some((candidate, (tx + dx) as i16, (ty + dy) as i16));
                        break 'scan;
                    }

                    // Lone human check
                    if person_query.get(candidate).is_ok() {
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

        let Ok((mut ai, _, _)) = ai_query.get_mut(deer_entity) else {
            continue;
        };

        // Don't interrupt sleep
        if ai.state == AnimalState::Sleeping {
            continue;
        }

        let mut threat_dx = 0i32;
        let mut threat_dy = 0i32;
        let mut threat_count = 0i32;

        for dy in -DEER_FLEE_RADIUS..=DEER_FLEE_RADIUS {
            for dx in -DEER_FLEE_RADIUS..=DEER_FLEE_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    if wolf_query.contains(candidate) || person_query.get(candidate).is_ok() {
                        let z_from = chunk_map.surface_z_at(tx, ty) as i8;
                        let z_to = chunk_map.surface_z_at(tx + dx, ty + dy) as i8;
                        if has_los(&chunk_map, &door_map, (tx, ty, z_from), (tx + dx, ty + dy, z_to)) {
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

            let flee_tx = (tx - threat_dx / threat_count).clamp(0, total_tiles_x - 1);
            let flee_ty = (ty - threat_dy / threat_count).clamp(0, total_tiles_y - 1);
            ai.state = AnimalState::Flee;
            ai.target_tile = (flee_tx as i16, flee_ty as i16);
            ai.wander_timer = 1.5;
        } else if ai.state == AnimalState::Flee {
            ai.state = AnimalState::Wander;
        }
    }

    // Horse sense: flee from wolves always; flee from persons if wild (untamed)
    for (horse_entity, transform, slot, lod, tamed_opt) in &horse_query {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

        let Ok((mut ai, _, _)) = ai_query.get_mut(horse_entity) else {
            continue;
        };

        if ai.state == AnimalState::Sleeping {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        const HORSE_FLEE_RADIUS: i32 = 10;
        let mut threat_dx = 0i32;
        let mut threat_dy = 0i32;
        let mut threat_count = 0i32;
        let is_wild = tamed_opt.is_none();

        for dy in -HORSE_FLEE_RADIUS..=HORSE_FLEE_RADIUS {
            for dx in -HORSE_FLEE_RADIUS..=HORSE_FLEE_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    let is_wolf_threat = wolf_query.contains(candidate);
                    let is_person_threat = is_wild && person_query.get(candidate).is_ok();
                    if is_wolf_threat || is_person_threat {
                        let z_from = chunk_map.surface_z_at(tx, ty) as i8;
                        let z_to = chunk_map.surface_z_at(tx + dx, ty + dy) as i8;
                        if has_los(&chunk_map, &door_map, (tx, ty, z_from), (tx + dx, ty + dy, z_to)) {
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
            let flee_tx = (tx - threat_dx / threat_count).clamp(0, total_tiles_x - 1);
            let flee_ty = (ty - threat_dy / threat_count).clamp(0, total_tiles_y - 1);
            ai.state = AnimalState::Flee;
            ai.target_tile = (flee_tx as i16, flee_ty as i16);
            ai.wander_timer = 1.5;
        } else if ai.state == AnimalState::Flee {
            ai.state = AnimalState::Wander;
        }
    }
}

/// Counts down reproduction cooldowns. Runs in Economy set.
pub fn animal_reproduction_cooldown_system(
    clock: Res<SimClock>,
    mut query: Query<
        (&mut AnimalReproductionCooldown, &BucketSlot, &LodLevel),
        bevy::prelude::Or<(With<Wolf>, With<Deer>, With<Horse>)>,
    >,
) {
    query.par_iter_mut().for_each(|(mut cd, slot, lod)| {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            return;
        }
        if cd.0 > 0 {
            cd.0 = cd.0.saturating_sub(1);
        }
    });
}

/// Animal reproduction: pairs up nearby males and females of the same species to spawn offspring.
/// Runs in Economy set, after animal_reproduction_cooldown_system.
pub fn animal_reproduction_system(
    mut commands: Commands,
    spatial: Res<SpatialIndex>,
    mut clock: ResMut<SimClock>,
    wolf_count: Query<(), With<Wolf>>,
    deer_count: Query<(), With<Deer>>,
    horse_count: Query<(), With<Horse>>,
    mut animal_query: Query<(
        Entity,
        &Transform,
        &BiologicalSex,
        &mut AnimalNeeds,
        &mut AnimalReproductionCooldown,
        &LodLevel,
        &BucketSlot,
        bevy::prelude::Has<Wolf>,
        bevy::prelude::Has<Deer>,
        bevy::prelude::Has<Horse>,
    )>,
) {
    let wolf_pop = wolf_count.iter().count();
    let deer_pop = deer_count.iter().count();
    let horse_pop = horse_count.iter().count();

    // Phase 1: collect eligible males (immutable pass)
    let mut wolf_males: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    let mut deer_males: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    let mut horse_males: ahash::AHashSet<Entity> = ahash::AHashSet::default();

    for (entity, _, sex, needs, cooldown, lod, slot, is_wolf, is_deer, is_horse) in animal_query.iter() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if *sex != BiologicalSex::Male || cooldown.0 > 0 {
            continue;
        }
        if is_wolf && needs.reproduction >= WOLF_REPRO_MALE_THRESHOLD {
            wolf_males.insert(entity);
        } else if is_deer && needs.reproduction >= DEER_REPRO_MALE_THRESHOLD {
            deer_males.insert(entity);
        } else if is_horse && needs.reproduction >= HORSE_REPRO_MALE_THRESHOLD {
            horse_males.insert(entity);
        }
    }

    // Phase 2: find female-male pairs (immutable pass)
    // (female, birth_pos, species: 0=wolf 1=deer 2=horse)
    let mut found_pairs: Vec<(Entity, Vec2, u8)> = Vec::new();

    for (entity, transform, sex, needs, cooldown, lod, slot, is_wolf, is_deer, is_horse) in
        animal_query.iter()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if *sex != BiologicalSex::Female || cooldown.0 > 0 {
            continue;
        }

        let (threshold, pop, cap, species, male_set) = if is_wolf {
            (WOLF_REPRO_FEMALE_THRESHOLD, wolf_pop, WOLF_POP_CAP, 0u8, &wolf_males)
        } else if is_deer {
            (DEER_REPRO_FEMALE_THRESHOLD, deer_pop, DEER_POP_CAP, 1u8, &deer_males)
        } else if is_horse {
            (HORSE_REPRO_FEMALE_THRESHOLD, horse_pop, HORSE_POP_CAP, 2u8, &horse_males)
        } else {
            continue;
        };

        if needs.reproduction < threshold || pop >= cap {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let mut found_male = false;
        'search: for dy in -REPRO_SEARCH_RADIUS..=REPRO_SEARCH_RADIUS {
            for dx in -REPRO_SEARCH_RADIUS..=REPRO_SEARCH_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    if male_set.contains(&candidate) {
                        found_male = true;
                        break 'search;
                    }
                }
            }
        }

        if found_male {
            found_pairs.push((entity, transform.translation.truncate(), species));
        }
    }

    // Phase 3: reset female needs, roll birth, spawn offspring
    let mut births: Vec<(Vec2, u8)> = Vec::new();

    for (female_ent, birth_pos, species) in found_pairs {
        if let Ok((_, _, _, mut needs, mut cooldown, _, _, _, _, _)) =
            animal_query.get_mut(female_ent)
        {
            needs.reproduction = 0.0;
            cooldown.0 = ANIMAL_BIRTH_COOLDOWN;
        }
        if fastrand::u32(..10_000) < ANIMAL_BIRTH_CHANCE {
            births.push((birth_pos, species));
        }
    }

    for (pos, species) in births {
        let slot = clock.population;
        clock.population += 1;
        clock.bucket_size = clock.population.min(10_000);

        let tx = (pos.x / TILE_SIZE).floor() as i32;
        let ty = (pos.y / TILE_SIZE).floor() as i32;
        let world_pos = tile_to_world(tx, ty);
        let sex = BiologicalSex::random();

        match species {
            0 => {
                commands.spawn((
                    Wolf,
                    Transform::from_xyz(world_pos.x, world_pos.y, 1.0),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    AnimalAI {
                        target_tile: (tx as i16, ty as i16),
                        ..Default::default()
                    },
                    Health::new(30),
                    CombatTarget::default(),
                    CombatCooldown::default(),
                    LodLevel::Full,
                    BucketSlot(slot),
                    AnimalNeeds::default(),
                    AnimalReproductionCooldown(0),
                    sex,
                ));
            }
            1 => {
                commands.spawn((
                    Deer,
                    Transform::from_xyz(world_pos.x, world_pos.y, 1.0),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    AnimalAI {
                        target_tile: (tx as i16, ty as i16),
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
                    AnimalNeeds::default(),
                    AnimalReproductionCooldown(0),
                    sex,
                ));
            }
            _ => {
                commands.spawn((
                    Horse,
                    Transform::from_xyz(world_pos.x, world_pos.y, 1.0),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    AnimalAI {
                        target_tile: (tx as i16, ty as i16),
                        ..Default::default()
                    },
                    Health::new(HORSE_HP),
                    CombatTarget::default(),
                    CombatCooldown::default(),
                    LodLevel::Full,
                    BucketSlot(slot),
                    AnimalNeeds::default(),
                    AnimalReproductionCooldown(0),
                    sex,
                ));
            }
        }
    }
}
