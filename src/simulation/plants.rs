use ahash::AHashMap;
use bevy::prelude::*;
use bevy::sprite::Anchor;

use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::economy::item::Item;
use crate::simulation::animals::Deer;
use crate::simulation::items::GroundItem;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::{AgentMemory, MemoryKind};
use crate::simulation::plan::ActivePlan;
use crate::simulation::person::{AiState, Person, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::terrain::{tile_to_world, TILE_SIZE};
use crate::rendering::pixel_art::EntityTextures;

const DEER_GRAZE_INTERVAL: u16 = 120;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PlantKind {
    #[default]
    FruitBush = 0,
    Grain     = 1,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GrowthStage {
    #[default]
    Seed     = 0,
    Seedling = 1,
    Mature   = 2,
    Overripe = 3,
}

#[derive(Component)]
pub struct Plant {
    pub kind:         PlantKind,
    pub stage:        GrowthStage,
    pub growth_ticks: u32,
    pub tile_pos:     (i16, i16),
}

impl Plant {
    pub fn duration_for_stage(&self) -> u32 {
        match self.kind {
            PlantKind::Grain => match self.stage {
                GrowthStage::Seed     => 18_000, // 5 days
                GrowthStage::Seedling => 54_000, // 15 days
                GrowthStage::Mature   => 36_000, // 10 days
                GrowthStage::Overripe => 0,
            },
            PlantKind::FruitBush => match self.stage {
                GrowthStage::Seed     => 36_000,  // 10 days
                GrowthStage::Seedling => 108_000, // 30 days
                GrowthStage::Mature   => 72_000,  // 20 days
                GrowthStage::Overripe => 0,
            },
        }
    }
}

#[derive(Component)]
pub struct DeerGrazer {
    pub graze_timer: u16,
}

#[derive(Resource, Default)]
pub struct PlantMap(pub AHashMap<(i32, i32), Entity>);

#[derive(Resource, Default)]
pub struct PlantSpriteIndex {
    pub by_chunk: AHashMap<ChunkCoord, Vec<(Entity, (i32, i32))>>,
}

pub fn get_plant_texture(textures: &EntityTextures, stage: GrowthStage) -> Handle<Image> {
    match stage {
        GrowthStage::Seed => textures.plant_seed.clone(),
        GrowthStage::Seedling => textures.plant_seedling.clone(),
        _ => textures.plant_mature.clone(),
    }
}

/// Spawn a plant entity at the given tile. No-op if the tile is already occupied.
pub fn spawn_plant_at(
    commands: &mut Commands,
    plant_map: &mut PlantMap,
    plant_sprite_index: &mut PlantSpriteIndex,
    textures: &EntityTextures,
    tile_x: i32,
    tile_y: i32,
    kind: PlantKind,
    stage: GrowthStage,
) {
    if plant_map.0.contains_key(&(tile_x, tile_y)) {
        return;
    }

    let world_pos = tile_to_world(tile_x, tile_y);

    let mut sprite = Sprite::from_image(get_plant_texture(textures, stage));
    sprite.custom_size = Some(Vec2::new(24.0, 36.0));
    sprite.anchor = Anchor::BottomCenter;

    let entity = commands.spawn((
        Plant {
            kind,
            stage,
            growth_ticks: 0,
            tile_pos: (tile_x as i16, tile_y as i16),
        },
        sprite,
        Transform::from_xyz(world_pos.x, world_pos.y - 8.0, 0.5),
        GlobalTransform::default(),
        Visibility::Visible,
    )).id();


    plant_map.0.insert((tile_x, tile_y), entity);

    let chunk_x = tile_x.div_euclid(CHUNK_SIZE as i32);
    let chunk_y = tile_y.div_euclid(CHUNK_SIZE as i32);
    let coord = ChunkCoord(chunk_x, chunk_y);
    plant_sprite_index.by_chunk
        .entry(coord)
        .or_default()
        .push((entity, (tile_x, tile_y)));
}

/// Advance plant growth stages based on tile fertility.
pub fn plant_growth_system(
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    textures: Res<EntityTextures>,
    mut query: Query<(Entity, &mut Plant, &mut Sprite)>,
) {
    if clock.tick % 5 != 0 {
        return;
    }

    for (_entity, mut plant, mut sprite) in query.iter_mut() {
        let tx = plant.tile_pos.0 as i32;
        let ty = plant.tile_pos.1 as i32;

        let fertility = match chunk_map.tile_at(tx, ty) {
            Some(t) => t.fertility,
            None    => continue, // unloaded chunk — pause growth
        };

        // Higher fertility = faster growth
        let multiplier = 2.0 - (fertility as f32 / 255.0) * 1.5;

        plant.growth_ticks = plant.growth_ticks.saturating_add(5);

        let threshold = plant.duration_for_stage();
        if threshold == 0 {
            continue;
        }

        let effective = (threshold as f32 * multiplier) as u32;
        if plant.growth_ticks >= effective {
            plant.growth_ticks = 0;
            plant.stage = match plant.stage {
                GrowthStage::Seed     => GrowthStage::Seedling,
                GrowthStage::Seedling => GrowthStage::Mature,
                GrowthStage::Mature   => GrowthStage::Overripe,
                GrowthStage::Overripe => GrowthStage::Overripe,
            };

            sprite.image = get_plant_texture(&textures, plant.stage);
            sprite.color = Color::WHITE;
        }
    }
}

/// Overripe plants scatter seeds to adjacent tiles, then die.
pub fn seed_scatter_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    textures: Res<EntityTextures>,
    query: Query<(Entity, &Plant)>,
) {
    if clock.tick % 5 != 0 {
        return;
    }

    let overripe: Vec<(Entity, (i32, i32), PlantKind)> = query
        .iter()
        .filter(|(_, p)| p.stage == GrowthStage::Overripe)
        .map(|(e, p)| (e, (p.tile_pos.0 as i32, p.tile_pos.1 as i32), p.kind))
        .collect();

    for (entity, (tx, ty), kind) in overripe {
        if plant_map.0.get(&(tx, ty)) != Some(&entity) {
            continue;
        }

        let (count, radius): (u8, i32) = match kind {
            PlantKind::FruitBush => (1, 1),
            PlantKind::Grain     => (1, 2),
        };

        for _ in 0..count {
            let dx = fastrand::i32(-radius..=radius);
            let dy = fastrand::i32(-radius..=radius);
            if dx == 0 && dy == 0 { continue; }
            let nx = tx + dx;
            let ny = ty + dy;
            if let Some(tile) = chunk_map.tile_at(nx, ny) {
                use crate::world::tile::TileKind;
                if matches!(tile.kind, TileKind::Grass | TileKind::Farmland) {
                    spawn_plant_at(
                        &mut commands, &mut plant_map, &mut plant_sprite_index,
                        &textures,
                        nx, ny, kind, GrowthStage::Seed,
                    );
                }
            }
        }

        plant_map.0.remove(&(tx, ty));
        let cx = tx.div_euclid(CHUNK_SIZE as i32);
        let cy = ty.div_euclid(CHUNK_SIZE as i32);
        if let Some(vec) = plant_sprite_index.by_chunk.get_mut(&ChunkCoord(cx, cy)) {
            vec.retain(|(e, _)| *e != entity);
        }
        commands.entity(entity).despawn();
    }
}

/// Harvest a mature plant when an agent arrives at the target tile.
pub fn plant_harvest_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    plant_query: Query<&Plant>,
    mut agent_query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &mut Skills,
        &BucketSlot,
        &LodLevel,
        Option<&mut AgentMemory>,
        Option<&mut ActivePlan>,
    ), With<Person>>,
) {
    for (mut ai, mut agent, mut skills, slot, lod, mut memory_opt, mut active_plan_opt) in agent_query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) { continue; }
        if ai.state != AiState::Working { continue; }
        let job = ai.job_id;
        if job != crate::simulation::jobs::JobKind::Farmer as u16 && job != crate::simulation::jobs::JobKind::Forager as u16 {
            continue;
        }

        let tx = ai.target_tile.0 as i32;
        let ty = ai.target_tile.1 as i32;

        let plant_entity = match plant_map.0.get(&(tx, ty)).copied() {
            Some(e) => e,
            None    => continue,
        };

        let plant = match plant_query.get(plant_entity) {
            Ok(p) => p,
            Err(_) => {
                plant_map.0.remove(&(tx, ty));
                continue;
            }
        };

        if plant.stage != GrowthStage::Mature {
            continue;
        }

        // Foragers can only harvest FruitBush
        if job == crate::simulation::jobs::JobKind::Forager as u16 && plant.kind != PlantKind::FruitBush {
            continue;
        }

        let (food_qty, give_seed_to_agent) = match plant.kind {
            PlantKind::FruitBush => (2u8, true),
            PlantKind::Grain     => (3u8, false),
        };

        agent.add_good(Good::Food, food_qty);
        if give_seed_to_agent {
            agent.add_good(Good::Seed, 1);
        }
        skills.gain_xp(SkillKind::Farming, 3);
        if let Some(ref mut mem) = memory_opt {
            mem.record((tx as i16, ty as i16), MemoryKind::Food);
        }
        if let Some(ref mut plan) = active_plan_opt {
            plan.reward_acc += food_qty as f32 * 0.4;
        }

        let (dx, dy) = adjacent_offset();
        let sx = tx + dx;
        let sy = ty + dy;
        let seed_pos = tile_to_world(sx, sy);
        commands.spawn((
            GroundItem { item: Item::new_commodity(Good::Seed), qty: 1 },
            Transform::from_xyz(seed_pos.x, seed_pos.y - 8.0, 0.3),
            GlobalTransform::default(),
        ));

        plant_map.0.remove(&(tx, ty));
        let cx = tx.div_euclid(CHUNK_SIZE as i32);
        let cy = ty.div_euclid(CHUNK_SIZE as i32);
        if let Some(vec) = plant_sprite_index.by_chunk.get_mut(&ChunkCoord(cx, cy)) {
            vec.retain(|(e, _)| *e != plant_entity);
        }
        commands.entity(plant_entity).despawn();

        ai.work_progress = 0;
    }
}

/// Deer graze on mature FruitBush plants in their vicinity.
pub fn deer_graze_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    plant_query: Query<&Plant>,
    mut deer_query: Query<(
        &Transform,
        &mut DeerGrazer,
        &BucketSlot,
        &LodLevel,
    ), With<Deer>>,
) {
    for (transform, mut grazer, slot, lod) in deer_query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) { continue; }

        if grazer.graze_timer > 0 {
            grazer.graze_timer -= 1;
            continue;
        }

        let deer_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let deer_ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let mut found: Option<(i32, i32, Entity)> = None;
        'search: for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                let tx = deer_tx + dx;
                let ty = deer_ty + dy;
                if let Some(&entity) = plant_map.0.get(&(tx, ty)) {
                    if let Ok(plant) = plant_query.get(entity) {
                        if plant.kind == PlantKind::FruitBush && plant.stage == GrowthStage::Mature {
                            found = Some((tx, ty, entity));
                            break 'search;
                        }
                    }
                }
            }
        }

        if let Some((tx, ty, entity)) = found {
            plant_map.0.remove(&(tx, ty));
            let cx = tx.div_euclid(CHUNK_SIZE as i32);
            let cy = ty.div_euclid(CHUNK_SIZE as i32);
            if let Some(vec) = plant_sprite_index.by_chunk.get_mut(&ChunkCoord(cx, cy)) {
                vec.retain(|(e, _)| *e != entity);
            }
            commands.entity(entity).despawn();

            let count = 1 + fastrand::u8(..2);
            for _ in 0..count {
                let (dx, dy) = adjacent_offset();
                let sx = tx + dx;
                let sy = ty + dy;
                let pos = tile_to_world(sx, sy);
                commands.spawn((
                    GroundItem { item: Item::new_commodity(Good::Seed), qty: 1 },
                    Transform::from_xyz(pos.x, pos.y - 8.0, 0.3),
                    GlobalTransform::default(),
                ));
            }
        }

        grazer.graze_timer = DEER_GRAZE_INTERVAL;
    }
}

fn adjacent_offset() -> (i32, i32) {
    match fastrand::u8(..4) {
        0 => (1, 0),
        1 => (-1, 0),
        2 => (0, 1),
        _ => (0, -1),
    }
}
