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
    Tree      = 2,
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
            PlantKind::Tree => match self.stage {
                GrowthStage::Seed     => 108_000, // 30 days
                GrowthStage::Seedling => 324_000, // 90 days
                GrowthStage::Mature   => 648_000, // 180 days
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

pub fn get_plant_texture(textures: &EntityTextures, kind: PlantKind, stage: GrowthStage) -> Handle<Image> {
    match kind {
        PlantKind::Tree => match stage {
            GrowthStage::Seed     => textures.plant_seed.clone(),
            GrowthStage::Seedling => textures.tree_seedling.clone(),
            _ => textures.tree_mature.clone(),
        },
        _ => match stage {
            GrowthStage::Seed => textures.plant_seed.clone(),
            GrowthStage::Seedling => textures.plant_seedling.clone(),
            _ => textures.plant_mature.clone(),
        }
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

    let mut sprite = Sprite::from_image(get_plant_texture(textures, kind, stage));
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

            sprite.image = get_plant_texture(&textures, plant.kind, plant.stage);
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
    mut query: Query<(Entity, &mut Plant, &mut Sprite)>,
) {
    if clock.tick % 5 != 0 {
        return;
    }

    let mut overripe_entities = Vec::new();
    for (entity, mut plant, mut sprite) in query.iter_mut() {
        if plant.stage == GrowthStage::Overripe {
            let tx = plant.tile_pos.0 as i32;
            let ty = plant.tile_pos.1 as i32;
            let kind = plant.kind;

            if kind == PlantKind::Tree {
                // Trees drop branches and return to Mature
                let (dx, dy) = adjacent_offset();
                let sx = tx + dx;
                let sy = ty + dy;
                let pos = tile_to_world(sx, sy);
                commands.spawn((
                    GroundItem { item: Item::new_commodity(Good::Wood), qty: 1 },
                    Transform::from_xyz(pos.x, pos.y - 8.0, 0.3),
                    GlobalTransform::default(),
                ));

                plant.stage = GrowthStage::Mature;
                plant.growth_ticks = 0;
                sprite.image = get_plant_texture(&textures, plant.kind, plant.stage);
            } else {
                overripe_entities.push((entity, (tx, ty), kind));
            }
        }
    }

    for (entity, (tx, ty), kind) in overripe_entities {
        if plant_map.0.get(&(tx, ty)) != Some(&entity) {
            continue;
        }

        let (count, radius): (u8, i32) = match kind {
            PlantKind::FruitBush => (1, 1),
            PlantKind::Grain     => (1, 2),
            PlantKind::Tree      => (0, 0), // Should not reach here
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
    textures: Res<EntityTextures>,
    mut plant_query: Query<&mut Plant>,
    mut sprite_query: Query<&mut Sprite>,
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
        let is_woodcutter = job == crate::simulation::jobs::JobKind::Woodcutter as u16;
        let is_farmer = job == crate::simulation::jobs::JobKind::Farmer as u16;
        let is_forager = job == crate::simulation::jobs::JobKind::Forager as u16;

        if !is_farmer && !is_forager && !is_woodcutter {
            continue;
        }

        let tx = ai.target_tile.0 as i32;
        let ty = ai.target_tile.1 as i32;

        let plant_entity = match plant_map.0.get(&(tx, ty)).copied() {
            Some(e) => e,
            None    => continue,
        };

        let mut plant = match plant_query.get_mut(plant_entity) {
            Ok(p) => p,
            Err(_) => {
                plant_map.0.remove(&(tx, ty));
                continue;
            }
        };

        if plant.stage != GrowthStage::Mature {
            continue;
        }

        // Foragers can harvest FruitBush or gather branches from Trees.
        // Woodcutters can only harvest Trees.
        // Farmers can harvest Grain or FruitBush.
        if is_woodcutter && plant.kind != PlantKind::Tree {
            continue;
        }
        if is_forager && plant.kind == PlantKind::Grain {
            continue;
        }
        if is_farmer && plant.kind == PlantKind::Tree {
            continue;
        }

        let (mut wood_qty, mut food_qty, mut seed_qty, mut despawn_plant) = (0u8, 0u8, 0u8, true);

        match plant.kind {
            PlantKind::FruitBush => {
                food_qty = 2;
                seed_qty = 1;
            }
            PlantKind::Grain => {
                food_qty = 3;
            }
            PlantKind::Tree => {
                if agent.has_tool() {
                    // Felling: requires tools, despawns tree, higher yield
                    wood_qty = 3;
                    despawn_plant = true;
                    skills.gain_xp(SkillKind::Farming, 5); // Using Farming for woodcutting XP as per original
                } else {
                    // Branch gathering: no tools, tree survives as seedling
                    wood_qty = 1;
                    despawn_plant = false;
                    skills.gain_xp(SkillKind::Farming, 2);
                }
            }
        }

        if food_qty > 0 {
            agent.add_good(Good::Food, food_qty);
            if let Some(ref mut mem) = memory_opt {
                mem.record((tx as i16, ty as i16), MemoryKind::Food);
            }
            if let Some(ref mut plan) = active_plan_opt {
                plan.reward_acc += food_qty as f32 * 0.4;
            }
        }
        if wood_qty > 0 {
            agent.add_good(Good::Wood, wood_qty);
            if let Some(ref mut mem) = memory_opt {
                mem.record((tx as i16, ty as i16), MemoryKind::Wood);
            }
            if let Some(ref mut plan) = active_plan_opt {
                plan.reward_acc += wood_qty as f32 * 0.4;
            }
        }
        if seed_qty > 0 {
            agent.add_good(Good::Seed, seed_qty);
        }

        // Drop some extra on ground
        let (drop_good, drop_qty) = match plant.kind {
            PlantKind::FruitBush => (Good::Seed, 1),
            PlantKind::Grain     => (Good::Seed, 1), // Grain harvest drops a seed too
            PlantKind::Tree      => (Good::Wood, if despawn_plant { 2 } else { 1 }),
        };

        for _ in 0..drop_qty {
            let (dx, dy) = adjacent_offset();
            let sx = tx + dx;
            let sy = ty + dy;
            let drop_pos = tile_to_world(sx, sy);
            commands.spawn((
                GroundItem { item: Item::new_commodity(drop_good), qty: 1 },
                Transform::from_xyz(drop_pos.x, drop_pos.y - 8.0, 0.3),
                GlobalTransform::default(),
            ));
        }

        if despawn_plant {
            plant_map.0.remove(&(tx, ty));
            let cx = tx.div_euclid(CHUNK_SIZE as i32);
            let cy = ty.div_euclid(CHUNK_SIZE as i32);
            if let Some(vec) = plant_sprite_index.by_chunk.get_mut(&ChunkCoord(cx, cy)) {
                vec.retain(|(e, _)| *e != plant_entity);
            }
            commands.entity(plant_entity).despawn();
        } else {
            // Revert to seedling
            plant.stage = GrowthStage::Seedling;
            plant.growth_ticks = 0;
            if let Ok(mut sprite) = sprite_query.get_mut(plant_entity) {
                sprite.image = get_plant_texture(&textures, plant.kind, plant.stage);
            }
        }

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
