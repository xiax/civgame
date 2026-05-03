use ahash::AHashMap;
use bevy::prelude::*;

use crate::economy::goods::{Bulk, Good};
use crate::economy::item::Item;
use crate::simulation::animals::Deer;
use crate::simulation::items::GroundItem;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::MemoryKind;
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::SkillKind;
use crate::simulation::technology::ActivityKind;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::seasons::{Calendar, Season, TICKS_PER_SEASON};
use crate::world::terrain::{tile_to_world, TILE_SIZE};

const DEER_GRAZE_INTERVAL: u16 = 120;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PlantKind {
    #[default]
    BerryBush = 0,
    Grain = 1,
    Tree = 2,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GrowthStage {
    #[default]
    Seed = 0,
    Seedling = 1,
    Harvested = 2,
    Mature = 3,
    Overripe = 4,
}

#[derive(Component)]
pub struct Plant {
    pub kind: PlantKind,
    pub stage: GrowthStage,
    pub growth_ticks: u32,
    pub tile_pos: (i16, i16),
}

impl Plant {
    pub fn duration_for_stage(&self, calendar: &Calendar) -> u32 {
        match self.kind {
            PlantKind::Grain => match self.stage {
                GrowthStage::Seed => TICKS_PER_SEASON / 6, // 1/6 season
                GrowthStage::Seedling => TICKS_PER_SEASON / 2, // 1/2 season
                GrowthStage::Harvested => 0,
                GrowthStage::Mature => TICKS_PER_SEASON / 3, // 1/3 season
                GrowthStage::Overripe => 0,
            },
            PlantKind::BerryBush => match self.stage {
                GrowthStage::Seed => TICKS_PER_SEASON / 3, // 1/3 season
                GrowthStage::Seedling => TICKS_PER_SEASON, // 1 season
                GrowthStage::Harvested => {
                    if matches!(calendar.season, Season::Spring | Season::Summer) {
                        TICKS_PER_SEASON / 6 // 1/6 season recovery
                    } else {
                        TICKS_PER_SEASON // 1 season recovery
                    }
                }
                GrowthStage::Mature => TICKS_PER_SEASON * 2 / 3, // 2/3 season
                GrowthStage::Overripe => 0,
            },
            PlantKind::Tree => match self.stage {
                GrowthStage::Seed => TICKS_PER_SEASON,         // 1 season
                GrowthStage::Seedling => TICKS_PER_SEASON * 3, // 3 seasons
                GrowthStage::Harvested => 0,
                GrowthStage::Mature => TICKS_PER_SEASON * 6, // 6 seasons
                GrowthStage::Overripe => 0,
            },
        }
    }
}

impl PlantKind {
    /// Ticks the agent must spend Working before a harvest triggers.
    /// Plants are harvested instantly (0); this mirrors how tile-based gathering uses work_ticks.
    pub fn harvest_work_ticks(self) -> u8 {
        0
    }

    /// Primary good produced and its base quantity.
    /// `has_tool` only matters for trees (felling vs branch-gathering).
    /// Bulk class of the primary harvest yield. Used by `enforce_hand_state_system`
    /// to require both hands free before chopping a tree (Wood is TwoHand) while
    /// leaving Berry/Grain pickups (Small) at the lighter 1-free-hand requirement.
    pub fn harvest_yield_bulk(self, has_tool: bool) -> Bulk {
        self.harvest_yield(has_tool).0.bulk()
    }

    pub fn harvest_yield(self, has_tool: bool) -> (Good, u32) {
        match self {
            PlantKind::Grain => (Good::Grain, 5),
            PlantKind::BerryBush => (Good::Fruit, 3),
            PlantKind::Tree => (Good::Wood, if has_tool { 3 } else { 1 }),
        }
    }

    /// Fixed co-yields always added alongside the primary yield (no faction multiplier).
    pub fn harvest_extra_yields(self) -> &'static [(Good, u32)] {
        match self {
            PlantKind::Grain => &[(Good::GrainSeed, 1)],
            PlantKind::BerryBush => &[(Good::BerrySeed, 1)],
            _ => &[],
        }
    }

    /// Items spawned as loose ground entities adjacent to the harvest tile.
    pub fn harvest_ground_drops(self, has_tool: bool) -> &'static [(Good, u32)] {
        match self {
            PlantKind::Grain => &[],
            PlantKind::BerryBush => &[(Good::BerrySeed, 1)],
            PlantKind::Tree => {
                if has_tool {
                    &[(Good::Wood, 2)]
                } else {
                    &[(Good::Wood, 1)]
                }
            }
        }
    }

    /// Plan reward multiplier per unit of primary yield.
    pub fn harvest_reward_per_unit(self) -> f32 {
        match self {
            PlantKind::Grain => 0.24,
            PlantKind::BerryBush => 0.133,
            PlantKind::Tree => 0.4,
        }
    }

    /// Faction activity to log for technology progression.
    pub fn harvest_activity(self) -> ActivityKind {
        match self {
            PlantKind::Grain => ActivityKind::Farming,
            PlantKind::BerryBush => ActivityKind::Foraging,
            PlantKind::Tree => ActivityKind::WoodGathering,
        }
    }

    /// Whether harvesting destroys the plant (true) or reverts it to Seedling (false).
    pub fn harvest_despawns(self, has_tool: bool) -> bool {
        match self {
            PlantKind::Tree => has_tool,
            PlantKind::BerryBush => false,
            _ => true,
        }
    }

    /// Returns (SkillKind, XP amount) for harvesting this plant.
    pub fn harvest_skill_xp(self, has_tool: bool) -> (SkillKind, u32) {
        match self {
            PlantKind::Tree => (SkillKind::Building, if has_tool { 10 } else { 2 }),
            PlantKind::Grain => (SkillKind::Farming, 5),
            PlantKind::BerryBush => (SkillKind::Building, 3),
        }
    }

    /// Memory category for recording this plant's location.
    pub fn harvest_memory_kind(self) -> MemoryKind {
        match self {
            PlantKind::Tree => MemoryKind::Wood,
            _ => MemoryKind::Food,
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

/// Spawn a plant entity at the given tile. No-op if the tile is already occupied.
pub fn spawn_plant_at(
    commands: &mut Commands,
    plant_map: &mut PlantMap,
    plant_sprite_index: &mut PlantSpriteIndex,
    tile_x: i32,
    tile_y: i32,
    kind: PlantKind,
    stage: GrowthStage,
) {
    if plant_map.0.contains_key(&(tile_x, tile_y)) {
        return;
    }

    let world_pos = tile_to_world(tile_x, tile_y);

    let entity = commands
        .spawn((
            Plant {
                kind,
                stage,
                growth_ticks: 0,
                tile_pos: (tile_x as i16, tile_y as i16),
            },
            Transform::from_xyz(world_pos.x, world_pos.y, 0.5),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
        ))
        .id();

    plant_map.0.insert((tile_x, tile_y), entity);

    let chunk_x = tile_x.div_euclid(CHUNK_SIZE as i32);
    let chunk_y = tile_y.div_euclid(CHUNK_SIZE as i32);
    let coord = ChunkCoord(chunk_x, chunk_y);
    plant_sprite_index
        .by_chunk
        .entry(coord)
        .or_default()
        .push((entity, (tile_x, tile_y)));
}

/// Advance plant growth stages based on tile fertility.
pub fn plant_growth_system(
    clock: Res<SimClock>,
    calendar: Res<Calendar>,
    chunk_map: Res<ChunkMap>,
    mut query: Query<(Entity, &mut Plant)>,
) {
    if clock.tick % 5 != 0 {
        return;
    }

    for (_entity, mut plant) in query.iter_mut() {
        let tx = plant.tile_pos.0 as i32;
        let ty = plant.tile_pos.1 as i32;

        let fertility = match chunk_map.tile_fertility_at(tx, ty) {
            Some(f) => f,
            None => continue, // unloaded chunk — pause growth
        };

        // Higher fertility = faster growth
        let multiplier = 2.0 - (fertility as f32 / 255.0) * 1.5;

        plant.growth_ticks = plant.growth_ticks.saturating_add(5);

        let threshold = plant.duration_for_stage(&calendar);
        if threshold == 0 {
            continue;
        }

        let effective = (threshold as f32 * multiplier) as u32;
        if plant.growth_ticks >= effective {
            plant.growth_ticks = 0;
            plant.stage = match plant.stage {
                GrowthStage::Seed => GrowthStage::Seedling,
                GrowthStage::Seedling => GrowthStage::Mature,
                GrowthStage::Harvested => GrowthStage::Mature,
                GrowthStage::Mature => GrowthStage::Overripe,
                GrowthStage::Overripe => GrowthStage::Overripe,
            };
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
    mut query: Query<(Entity, &mut Plant)>,
) {
    if clock.tick % 5 != 0 {
        return;
    }

    let mut overripe_entities = Vec::new();
    for (entity, mut plant) in query.iter_mut() {
        if plant.stage == GrowthStage::Overripe {
            let tx = plant.tile_pos.0 as i32;
            let ty = plant.tile_pos.1 as i32;
            let kind = plant.kind;

            if kind == PlantKind::Tree || kind == PlantKind::BerryBush {
                // Perennials: return to a previous stage instead of dying
                if kind == PlantKind::Tree {
                    // Trees drop branches and return to Mature
                    let (dx, dy) = adjacent_offset();
                    let sx = tx + dx;
                    let sy = ty + dy;
                    let pos = tile_to_world(sx, sy);
                    commands.spawn((
                        GroundItem {
                            item: Item::new_commodity(Good::Wood),
                            qty: 1,
                        },
                        Transform::from_xyz(pos.x, pos.y, 0.3),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ));

                    plant.stage = GrowthStage::Mature;
                } else {
                    // Berry bushes revert to Harvested (fruit fell off)
                    plant.stage = GrowthStage::Harvested;
                }
                plant.growth_ticks = 0;
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
            PlantKind::BerryBush => (1, 1),
            PlantKind::Grain => (1, 2),
            PlantKind::Tree => (0, 0), // Should not reach here
        };

        for _ in 0..count {
            let dx = fastrand::i32(-radius..=radius);
            let dy = fastrand::i32(-radius..=radius);
            if dx == 0 && dy == 0 {
                continue;
            }
            let nx = tx + dx;
            let ny = ty + dy;
            use crate::world::tile::TileKind as TK;
            if matches!(
                chunk_map.tile_kind_at(nx, ny),
                Some(TK::Grass | TK::Farmland)
            ) {
                spawn_plant_at(
                    &mut commands,
                    &mut plant_map,
                    &mut plant_sprite_index,
                    nx,
                    ny,
                    kind,
                    GrowthStage::Seed,
                );
            }
        }

        plant_map.0.remove(&(tx, ty));
        let cx = tx.div_euclid(CHUNK_SIZE as i32);
        let cy = ty.div_euclid(CHUNK_SIZE as i32);
        if let Some(vec) = plant_sprite_index.by_chunk.get_mut(&ChunkCoord(cx, cy)) {
            vec.retain(|(e, _)| *e != entity);
        }
        commands.entity(entity).despawn_recursive();
    }
}

/// Deer graze on mature BerryBush plants in their vicinity.
pub fn deer_graze_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    plant_map: Res<PlantMap>,
    _plant_sprite_index: Res<PlantSpriteIndex>,
    mut plant_query: Query<&mut Plant>,
    mut deer_query: Query<
        (
            &Transform,
            &mut DeerGrazer,
            &BucketSlot,
            &LodLevel,
            Option<&mut crate::simulation::animals::AnimalNeeds>,
        ),
        With<Deer>,
    >,
) {
    for (transform, mut grazer, slot, lod, mut animal_needs) in deer_query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

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
                        if plant.kind == PlantKind::BerryBush && plant.stage == GrowthStage::Mature
                        {
                            found = Some((tx, ty, entity));
                            break 'search;
                        }
                    }
                }
            }
        }

        if let Some((tx, ty, entity)) = found {
            if let Ok(mut plant) = plant_query.get_mut(entity) {
                plant.stage = GrowthStage::Harvested;
                plant.growth_ticks = 0;
            }
            if let Some(ref mut needs) = animal_needs {
                needs.hunger = (needs.hunger
                    - crate::simulation::animals::ANIMAL_HUNGER_RECOVER_DEER)
                    .max(0.0);
            }

            let count = 1 + fastrand::u8(..2);
            for _ in 0..count {
                let (dx, dy) = adjacent_offset();
                let sx = tx + dx;
                let sy = ty + dy;
                let pos = tile_to_world(sx, sy);
                commands.spawn((
                    GroundItem {
                        item: Item::new_commodity(Good::BerrySeed),
                        qty: 1,
                    },
                    Transform::from_xyz(pos.x, pos.y, 0.3),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
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
