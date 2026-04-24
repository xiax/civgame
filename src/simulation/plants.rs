use ahash::AHashMap;
use bevy::prelude::*;
use bevy::sprite::Anchor;

use crate::economy::goods::Good;
use crate::economy::item::Item;
use crate::simulation::animals::Deer;
use crate::simulation::memory::MemoryKind;
use crate::simulation::skills::SkillKind;
use crate::simulation::technology::ActivityKind;
use crate::simulation::items::GroundItem;
use crate::simulation::lod::LodLevel;
use crate::simulation::schedule::{BucketSlot, SimClock};
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

impl PlantKind {
    /// Ticks the agent must spend Working before a harvest triggers.
    /// Plants are harvested instantly (0); this mirrors how tile-based gathering uses work_ticks.
    pub fn harvest_work_ticks(self) -> u8 { 0 }

    /// Primary good produced and its base quantity.
    /// `has_tool` only matters for trees (felling vs branch-gathering).
    pub fn harvest_yield(self, has_tool: bool) -> (Good, u8) {
        match self {
            PlantKind::Grain     => (Good::Food, 3),
            PlantKind::FruitBush => (Good::Food, 2),
            PlantKind::Tree      => (Good::Wood, if has_tool { 3 } else { 1 }),
        }
    }

    /// Fixed co-yields always added alongside the primary yield (no faction multiplier).
    pub fn harvest_extra_yields(self) -> &'static [(Good, u8)] {
        match self {
            PlantKind::FruitBush => &[(Good::Seed, 1)],
            _                    => &[],
        }
    }

    /// Items spawned as loose ground entities adjacent to the harvest tile.
    pub fn harvest_ground_drops(self, has_tool: bool) -> &'static [(Good, u8)] {
        match self {
            PlantKind::Grain     => &[(Good::Seed, 1)],
            PlantKind::FruitBush => &[(Good::Seed, 1)],
            PlantKind::Tree      => if has_tool { &[(Good::Wood, 2)] } else { &[(Good::Wood, 1)] },
        }
    }

    /// Skill and XP gained per harvest event.
    pub fn harvest_skill_xp(self, has_tool: bool) -> (SkillKind, u8) {
        match self {
            PlantKind::Grain | PlantKind::FruitBush => (SkillKind::Farming, 3),
            PlantKind::Tree                         => (SkillKind::Farming, if has_tool { 5 } else { 2 }),
        }
    }

    /// Memory kind to record after a successful harvest.
    pub fn harvest_memory_kind(self) -> MemoryKind {
        match self {
            PlantKind::Grain | PlantKind::FruitBush => MemoryKind::Food,
            PlantKind::Tree                          => MemoryKind::Wood,
        }
    }

    /// Plan reward multiplier per unit of primary yield.
    pub fn harvest_reward_per_unit(self) -> f32 { 0.4 }

    /// Faction activity to log for technology progression.
    pub fn harvest_activity(self) -> ActivityKind {
        match self {
            PlantKind::Grain     => ActivityKind::Farming,
            PlantKind::FruitBush => ActivityKind::Foraging,
            PlantKind::Tree      => ActivityKind::WoodGathering,
        }
    }

    /// Whether harvesting destroys the plant (true) or reverts it to Seedling (false).
    pub fn harvest_despawns(self, has_tool: bool) -> bool {
        match self {
            PlantKind::Tree => has_tool,
            _               => true,
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

        let fertility = match chunk_map.tile_fertility_at(tx, ty) {
            Some(f) => f,
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
            use crate::world::tile::TileKind as TK;
            if matches!(chunk_map.tile_kind_at(nx, ny), Some(TK::Grass | TK::Farmland)) {
                spawn_plant_at(
                    &mut commands, &mut plant_map, &mut plant_sprite_index,
                    &textures,
                    nx, ny, kind, GrowthStage::Seed,
                );
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
