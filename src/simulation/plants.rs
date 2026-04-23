use ahash::AHashMap;
use bevy::prelude::*;

use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
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

const SEED_TO_SEEDLING_TICKS:  u16 = 60;
const SEEDLING_TO_MATURE_TICKS: u16 = 120;
const MATURE_TO_OVERRIPE_TICKS: u16 = 300;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlantKind {
    FruitBush = 0,
    Grain     = 1,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrowthStage {
    Seed     = 0,
    Seedling = 1,
    Mature   = 2,
    Overripe = 3,
}

#[derive(Component)]
pub struct Plant {
    pub kind:         PlantKind,
    pub stage:        GrowthStage,
    pub growth_ticks: u16,
    pub tile_pos:     (i16, i16),
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

#[derive(Resource, Default)]
pub struct PlantMaterials {
    pub fruit_seed:      Handle<ColorMaterial>,
    pub fruit_seedling:  Handle<ColorMaterial>,
    pub fruit_mature:    Handle<ColorMaterial>,
    pub fruit_overripe:  Handle<ColorMaterial>,
    pub grain_seed:      Handle<ColorMaterial>,
    pub grain_seedling:  Handle<ColorMaterial>,
    pub grain_mature:    Handle<ColorMaterial>,
    pub grain_overripe:  Handle<ColorMaterial>,
}

impl PlantMaterials {
    pub fn handle_for(&self, kind: PlantKind, stage: GrowthStage) -> Handle<ColorMaterial> {
        match (kind, stage) {
            (PlantKind::FruitBush, GrowthStage::Seed)     => self.fruit_seed.clone(),
            (PlantKind::FruitBush, GrowthStage::Seedling) => self.fruit_seedling.clone(),
            (PlantKind::FruitBush, GrowthStage::Mature)   => self.fruit_mature.clone(),
            (PlantKind::FruitBush, GrowthStage::Overripe) => self.fruit_overripe.clone(),
            (PlantKind::Grain,     GrowthStage::Seed)     => self.grain_seed.clone(),
            (PlantKind::Grain,     GrowthStage::Seedling) => self.grain_seedling.clone(),
            (PlantKind::Grain,     GrowthStage::Mature)   => self.grain_mature.clone(),
            (PlantKind::Grain,     GrowthStage::Overripe) => self.grain_overripe.clone(),
        }
    }
}

#[derive(Resource, Default)]
pub struct PlantMeshHandle(pub Handle<Mesh>);

pub fn setup_plant_materials(
    mut plant_materials: ResMut<PlantMaterials>,
    mut plant_mesh: ResMut<PlantMeshHandle>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut meshes:    ResMut<Assets<Mesh>>,
) {
    plant_mesh.0 = meshes.add(Rectangle::new(5.0, 5.0));

    plant_materials.fruit_seed     = materials.add(ColorMaterial::from_color(Color::srgb(0.25, 0.15, 0.05)));
    plant_materials.fruit_seedling = materials.add(ColorMaterial::from_color(Color::srgb(0.40, 0.70, 0.30)));
    plant_materials.fruit_mature   = materials.add(ColorMaterial::from_color(Color::srgb(0.85, 0.15, 0.15)));
    plant_materials.fruit_overripe = materials.add(ColorMaterial::from_color(Color::srgb(0.50, 0.10, 0.05)));
    plant_materials.grain_seed     = materials.add(ColorMaterial::from_color(Color::srgb(0.75, 0.70, 0.45)));
    plant_materials.grain_seedling = materials.add(ColorMaterial::from_color(Color::srgb(0.60, 0.75, 0.20)));
    plant_materials.grain_mature   = materials.add(ColorMaterial::from_color(Color::srgb(0.90, 0.80, 0.10)));
    plant_materials.grain_overripe = materials.add(ColorMaterial::from_color(Color::srgb(0.85, 0.85, 0.55)));
}

/// Spawn a plant entity at the given tile. No-op if the tile is already occupied.
pub fn spawn_plant_at(
    commands: &mut Commands,
    plant_map: &mut PlantMap,
    plant_sprite_index: &mut PlantSpriteIndex,
    plant_materials: &PlantMaterials,
    plant_mesh: &PlantMeshHandle,
    tile_x: i32,
    tile_y: i32,
    kind: PlantKind,
    stage: GrowthStage,
) {
    if plant_map.0.contains_key(&(tile_x, tile_y)) {
        return;
    }

    let world_pos = tile_to_world(tile_x, tile_y);
    let mat = plant_materials.handle_for(kind, stage);

    let entity = commands.spawn((
        Plant {
            kind,
            stage,
            growth_ticks: 0,
            tile_pos: (tile_x as i16, tile_y as i16),
        },
        Mesh2d(plant_mesh.0.clone()),
        MeshMaterial2d(mat),
        Transform::from_xyz(world_pos.x, world_pos.y, 0.5),
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
    plant_materials: Res<PlantMaterials>,
    mut query: Query<(Entity, &mut Plant, &mut MeshMaterial2d<ColorMaterial>)>,
) {
    if clock.tick % 5 != 0 {
        return;
    }

    for (_entity, mut plant, mut mat) in query.iter_mut() {
        let tx = plant.tile_pos.0 as i32;
        let ty = plant.tile_pos.1 as i32;

        let fertility = match chunk_map.tile_at(tx, ty) {
            Some(t) => t.fertility,
            None    => continue, // unloaded chunk — pause growth
        };

        // Higher fertility = faster growth
        let multiplier = 2.0 - (fertility as f32 / 255.0) * 1.5;

        plant.growth_ticks = plant.growth_ticks.saturating_add(5);

        let threshold = match plant.stage {
            GrowthStage::Seed     => SEED_TO_SEEDLING_TICKS,
            GrowthStage::Seedling => SEEDLING_TO_MATURE_TICKS,
            GrowthStage::Mature   => MATURE_TO_OVERRIPE_TICKS,
            GrowthStage::Overripe => continue,
        };

        let effective = (threshold as f32 * multiplier) as u16;
        if plant.growth_ticks >= effective {
            plant.growth_ticks = 0;
            plant.stage = match plant.stage {
                GrowthStage::Seed     => GrowthStage::Seedling,
                GrowthStage::Seedling => GrowthStage::Mature,
                GrowthStage::Mature   => GrowthStage::Overripe,
                GrowthStage::Overripe => GrowthStage::Overripe,
            };
            mat.0 = plant_materials.handle_for(plant.kind, plant.stage);
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
    plant_materials: Res<PlantMaterials>,
    plant_mesh: Res<PlantMeshHandle>,
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
        // Confirm still in plant_map (avoid double-processing if somehow duplicated)
        if plant_map.0.get(&(tx, ty)) != Some(&entity) {
            continue;
        }

        // Determine scatter count and radius
        let (count, radius): (u8, i32) = match kind {
            PlantKind::FruitBush => (1 + fastrand::u8(..2), 1),
            PlantKind::Grain     => (2 + fastrand::u8(..3), 2),
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
                        &plant_materials, &plant_mesh,
                        nx, ny, kind, GrowthStage::Seed,
                    );
                }
            }
        }

        // Remove and despawn the overripe plant
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
/// Runs before production_system so we can zero work_progress to prevent double-yield.
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
        if ai.job_id != crate::simulation::jobs::JobKind::Farmer as u16 { continue; }

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

        // Harvest
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

        // Drop a seed nearby on the ground
        let (dx, dy) = adjacent_offset();
        let sx = tx + dx;
        let sy = ty + dy;
        let seed_pos = tile_to_world(sx, sy);
        commands.spawn((
            GroundItem { good: Good::Seed, qty: 1 },
            Transform::from_xyz(seed_pos.x, seed_pos.y, 0.3),
            GlobalTransform::default(),
        ));

        // Remove plant from map and despawn
        plant_map.0.remove(&(tx, ty));
        let cx = tx.div_euclid(CHUNK_SIZE as i32);
        let cy = ty.div_euclid(CHUNK_SIZE as i32);
        if let Some(vec) = plant_sprite_index.by_chunk.get_mut(&ChunkCoord(cx, cy)) {
            vec.retain(|(e, _)| *e != plant_entity);
        }
        commands.entity(plant_entity).despawn();

        // Zero progress so production_system doesn't double-yield this tick
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

        // Scan 7×7 box for a mature FruitBush
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
            // Consume the plant
            plant_map.0.remove(&(tx, ty));
            let cx = tx.div_euclid(CHUNK_SIZE as i32);
            let cy = ty.div_euclid(CHUNK_SIZE as i32);
            if let Some(vec) = plant_sprite_index.by_chunk.get_mut(&ChunkCoord(cx, cy)) {
                vec.retain(|(e, _)| *e != entity);
            }
            commands.entity(entity).despawn();

            // Drop 1–2 seeds on adjacent tiles (seeds pass through and spread)
            let count = 1 + fastrand::u8(..2);
            for _ in 0..count {
                let (dx, dy) = adjacent_offset();
                let sx = tx + dx;
                let sy = ty + dy;
                let pos = tile_to_world(sx, sy);
                commands.spawn((
                    GroundItem { good: Good::Seed, qty: 1 },
                    Transform::from_xyz(pos.x, pos.y, 0.3),
                    GlobalTransform::default(),
                ));
            }
        }

        grazer.graze_timer = 120;
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
