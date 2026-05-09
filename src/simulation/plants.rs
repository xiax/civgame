use ahash::AHashMap;
use bevy::prelude::*;

use crate::economy::goods::Bulk;
use crate::economy::item::Item;
use crate::economy::resource_catalog::ResourceId;
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
    pub tile_pos: (i32, i32),
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
    /// Single source of truth for "every plant kind that exists." Iterating this
    /// constant powers the seed↔plant mapping, faction storage walks, and the
    /// Planter executor — adding a new plant kind here plus an arm in
    /// `seed_good()` is the entire extension surface for new seeds.
    pub const ALL: &'static [PlantKind] =
        &[PlantKind::Grain, PlantKind::BerryBush, PlantKind::Tree];

    /// The seed `ResourceId` that grows into this plant, if any. Tree
    /// saplings aren't modelled as seeds yet, so trees return None.
    pub fn seed_resource(self) -> Option<crate::economy::resource_catalog::ResourceId> {
        match self {
            PlantKind::Grain => crate::economy::core_ids::GrainSeed.get().copied(),
            PlantKind::BerryBush => crate::economy::core_ids::BerrySeed.get().copied(),
            PlantKind::Tree => None,
        }
    }

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
        let (id, _qty) = self.harvest_yield(has_tool);
        Bulk::for_resource(id, crate::economy::core_ids::catalog()).unwrap_or(Bulk::Small)
    }

    /// Primary harvest yield as a catalog `ResourceId`. Sub-PR (a) of the
    /// `Good`-retirement migration: previously returned `(Good, u32)`. Callers
    /// that still need the legacy `Good` go via
    /// `core_ids::resource_id_to_good`.
    pub fn harvest_yield(self, has_tool: bool) -> (ResourceId, u32) {
        use crate::economy::core_ids;
        match self {
            PlantKind::Grain => (core_ids::grain(), 5),
            PlantKind::BerryBush => (core_ids::fruit(), 3),
            PlantKind::Tree => (
                core_ids::wood(),
                if has_tool { 3 } else { 1 },
            ),
        }
    }

    /// Fixed co-yields always added alongside the primary yield (no faction
    /// multiplier). `ResourceId`-typed; callers route through
    /// `core_ids::resource_id_to_good` while `Item::new_commodity` still
    /// takes `Good`.
    pub fn harvest_extra_yields(self) -> Vec<(ResourceId, u32)> {
        use crate::economy::core_ids;
        match self {
            PlantKind::Grain => vec![(core_ids::grain_seed(), 1)],
            PlantKind::BerryBush => {
                vec![(core_ids::berry_seed(), 1)]
            }
            _ => Vec::new(),
        }
    }

    /// Items spawned as loose ground entities adjacent to the harvest tile.
    pub fn harvest_ground_drops(self, has_tool: bool) -> Vec<(ResourceId, u32)> {
        use crate::economy::core_ids;
        match self {
            PlantKind::Grain => Vec::new(),
            PlantKind::BerryBush => {
                vec![(core_ids::berry_seed(), 1)]
            }
            PlantKind::Tree => {
                let qty = if has_tool { 2 } else { 1 };
                vec![(core_ids::wood(), qty)]
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
    ///
    /// Sub-PR 2: derived from the catalog class of `harvest_yield(false).0`
    /// rather than switching on `PlantKind` directly. Adding a new plant
    /// kind whose harvest is a non-edible material (e.g. a future Cotton
    /// plant yielding Cloth, or a Reed plant yielding a new Material)
    /// records under `MemoryKind::Resource(id)` automatically without
    /// touching this method.
    pub fn harvest_memory_kind(self) -> MemoryKind {
        let (id, _qty) = self.harvest_yield(false);
        match crate::economy::core_ids::catalog().get(id).map(|d| d.class) {
            Some(crate::economy::resource_catalog::ResourceClass::Food) => MemoryKind::AnyEdible,
            _ => MemoryKind::Resource(id),
        }
    }
}

/// P6a: live `PlantMap` fast path. Probes for a mature plant of a
/// kind passing `kind_filter` within chebyshev `radius` of `from`,
/// returning the closest hit. The original "agent stands in a wheat
/// field with active `AcquireFood`" symptom traces to vision running
/// once per ~20-tick bucket pass and `SharedKnowledge` requiring a
/// reported sighting; calling this helper before vision/knowledge
/// catches plants the agent literally arrived next to.
///
/// `kind_filter`: e.g. `|k| matches!(k, PlantKind::Grain | PlantKind::BerryBush)`
/// for `AnyEdible`, `|k| k == PlantKind::Grain` for grain-only.
pub fn nearest_mature_plant_under_agent(
    plant_map: &PlantMap,
    plant_query: &bevy::ecs::system::Query<&Plant>,
    kind_filter: impl Fn(PlantKind) -> bool,
    from: (i32, i32),
    radius: i32,
) -> Option<((i32, i32), bevy::prelude::Entity)> {
    let mut best: Option<((i32, i32), bevy::prelude::Entity, i32)> = None;
    for dx in -radius..=radius {
        for dy in -radius..=radius {
            let tile = (from.0 + dx, from.1 + dy);
            let Some(&entity) = plant_map.0.get(&tile) else {
                continue;
            };
            let Ok(plant) = plant_query.get(entity) else {
                continue;
            };
            if plant.stage != GrowthStage::Mature {
                continue;
            }
            if !kind_filter(plant.kind) {
                continue;
            }
            let dist = dx.abs().max(dy.abs());
            match best {
                None => best = Some((tile, entity, dist)),
                Some((_, _, d)) if dist < d => best = Some((tile, entity, dist)),
                _ => {}
            }
        }
    }
    best.map(|(t, e, _)| (t, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::SystemState;
    use bevy::prelude::*;

    fn install_catalog() {
        let cat = crate::economy::resource_catalog::load_resource_catalog();
        crate::economy::core_ids::install_catalog(cat);
    }

    /// Run the helper against a freshly-built `World` populated with
    /// the named plants. Returns the matched tile (if any).
    fn probe(
        world: &mut World,
        from: (i32, i32),
        radius: i32,
        kinds: &'static [PlantKind],
    ) -> Option<(i32, i32)> {
        let mut state: SystemState<(Res<PlantMap>, Query<&Plant>)> = SystemState::new(world);
        let (plant_map, plant_q) = state.get(world);
        let kinds_owned: Vec<PlantKind> = kinds.to_vec();
        nearest_mature_plant_under_agent(
            &plant_map,
            &plant_q,
            move |k| kinds_owned.contains(&k),
            from,
            radius,
        )
        .map(|(t, _)| t)
    }

    fn spawn_plant(world: &mut World, tile: (i32, i32), kind: PlantKind, stage: GrowthStage) {
        let e = world
            .spawn(Plant {
                kind,
                stage,
                growth_ticks: 0,
                tile_pos: tile,
            })
            .id();
        let mut map = world.resource_mut::<PlantMap>();
        map.0.insert(tile, e);
    }

    fn world_with_map() -> World {
        let mut w = World::new();
        w.insert_resource(PlantMap::default());
        w
    }

    #[test]
    fn fast_path_finds_mature_plant_under_agent() {
        install_catalog();
        let mut w = world_with_map();
        spawn_plant(&mut w, (5, 5), PlantKind::Grain, GrowthStage::Mature);
        let kinds: &[PlantKind] = &[PlantKind::Grain, PlantKind::BerryBush, PlantKind::Tree];
        let kinds_static: &'static [PlantKind] = Box::leak(kinds.to_vec().into_boxed_slice());
        assert_eq!(probe(&mut w, (5, 5), 2, kinds_static), Some((5, 5)));
    }

    #[test]
    fn fast_path_skips_immature_plants() {
        install_catalog();
        let mut w = world_with_map();
        spawn_plant(&mut w, (3, 3), PlantKind::Grain, GrowthStage::Seedling);
        let kinds_static: &'static [PlantKind] =
            Box::leak(vec![PlantKind::Grain].into_boxed_slice());
        assert!(probe(&mut w, (3, 3), 2, kinds_static).is_none());
    }

    #[test]
    fn fast_path_picks_closest_within_radius() {
        install_catalog();
        let mut w = world_with_map();
        spawn_plant(&mut w, (1, 0), PlantKind::Grain, GrowthStage::Mature);
        spawn_plant(&mut w, (2, 0), PlantKind::Grain, GrowthStage::Mature);
        let kinds_static: &'static [PlantKind] =
            Box::leak(vec![PlantKind::Grain].into_boxed_slice());
        assert_eq!(probe(&mut w, (0, 0), 3, kinds_static), Some((1, 0)));
    }

    #[test]
    fn fast_path_filters_by_kind() {
        install_catalog();
        let mut w = world_with_map();
        spawn_plant(&mut w, (0, 0), PlantKind::Tree, GrowthStage::Mature);
        // AnyEdible filter: Tree (yields Wood) is excluded.
        let kinds_static: &'static [PlantKind] =
            Box::leak(vec![PlantKind::Grain, PlantKind::BerryBush].into_boxed_slice());
        assert!(probe(&mut w, (0, 0), 2, kinds_static).is_none());
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

/// Spawn a plant entity at the given tile. Returns `None` if the tile is
/// already occupied. Callers that need to attach an ownership marker
/// (`LandClaim`) read the returned entity.
pub fn spawn_plant_at(
    commands: &mut Commands,
    plant_map: &mut PlantMap,
    plant_sprite_index: &mut PlantSpriteIndex,
    tile_x: i32,
    tile_y: i32,
    kind: PlantKind,
    stage: GrowthStage,
) -> Option<Entity> {
    if plant_map.0.contains_key(&(tile_x, tile_y)) {
        return None;
    }

    let world_pos = tile_to_world(tile_x, tile_y);

    let entity = commands
        .spawn((
            Plant {
                kind,
                stage,
                growth_ticks: 0,
                tile_pos: (tile_x as i32, tile_y as i32),
            },
            Transform::from_xyz(world_pos.x, world_pos.y, 0.5),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
            crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Plant),
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
    Some(entity)
}

/// Advance plant growth stages based on tile fertility.
///
/// Bucketed across a 5-tick window: each plant updates once every 5
/// ticks (same effective rate as a `tick % 5 == 0` gate), but only
/// 1/5 of plants run per tick — peak per-frame cost ÷5.
pub fn plant_growth_system(
    clock: Res<SimClock>,
    calendar: Res<Calendar>,
    chunk_map: Res<ChunkMap>,
    mut query: Query<(Entity, &mut Plant)>,
) {
    let bucket = (clock.tick % 5) as u32;
    for (entity, mut plant) in query.iter_mut() {
        if entity.index() % 5 != bucket {
            continue;
        }
        // Pre-existing logic; tx assignment moved here intact.
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

/// Overripe plants either reset (Tree → Mature, BerryBush → Harvested — perennial,
/// no wild reproduction or wood drops) or scatter seeds and die (Grain only).
pub fn seed_scatter_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    mut query: Query<(Entity, &mut Plant)>,
) {
    let bucket = (clock.tick % 5) as u32;

    let mut overripe_entities = Vec::new();
    for (entity, mut plant) in query.iter_mut() {
        if entity.index() % 5 != bucket {
            continue;
        }
        if plant.stage == GrowthStage::Overripe {
            let tx = plant.tile_pos.0 as i32;
            let ty = plant.tile_pos.1 as i32;
            let kind = plant.kind;

            if kind == PlantKind::Tree || kind == PlantKind::BerryBush {
                // Perennials: return to a previous stage instead of dying.
                // Wood comes only from chopping; berry bushes do not reproduce in the wild.
                plant.stage = if kind == PlantKind::Tree {
                    GrowthStage::Mature
                } else {
                    GrowthStage::Harvested
                };
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

        // Only Grain reaches this loop: Tree and BerryBush short-circuit above as perennials.
        let (count, radius): (u8, i32) = match kind {
            PlantKind::Grain => (1, 2),
            _ => (0, 0),
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
                        item: Item::new_commodity(
                            crate::economy::core_ids::berry_seed(),
                        ),
                        qty: 1,
                    },
                    Transform::from_xyz(pos.x, pos.y, 0.3),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    crate::world::spatial::Indexed::new(
                        crate::world::spatial::IndexedKind::GroundItem,
                    ),
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
