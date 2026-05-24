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
use crate::world::seasons::{Calendar, Season};
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
    /// Accumulated growth points. Incremented at each season edge by
    /// `season_growth(kind, prev_season)`; transitions fire when it crosses
    /// `stage_threshold(kind, stage)`. Replaces the legacy tick clock —
    /// growth is calendar-driven, season modulates the *rate*, not gates.
    pub growth: u16,
    pub tile_pos: (i32, i32),
}

/// Marks a plant that was deliberately sown by a farmer via `Task::Planter`,
/// as opposed to wild scatter (`try_scatter_seed`) or recreational
/// `PlayPlant`. A cultivated crop skips the 20% wild sprout roll in
/// `plant_lifecycle_system` — selected seed tended in worked soil reliably
/// reaches Seedling, so a deliberately-planted field doesn't lose 80% of its
/// tiles at the first season edge.
#[derive(Component)]
pub struct Cultivated;

/// Growth points contributed to a plant of `kind` for the season that just
/// ended. Encodes per-species physiology — berries peak in spring, grain
/// peaks mid-summer, trees grow through summer and a long autumn before
/// dormancy. Winter is dormant for all (and additionally lethal for Grain;
/// see `plant_lifecycle_system`).
pub fn season_growth(kind: PlantKind, season: Season) -> u8 {
    match (kind, season) {
        (_, Season::Winter) => 0,
        (PlantKind::Grain, Season::Spring) => 4,
        (PlantKind::Grain, Season::Summer) => 5,
        (PlantKind::Grain, Season::Autumn) => 2,
        (PlantKind::BerryBush, Season::Spring) => 5,
        (PlantKind::BerryBush, Season::Summer) => 4,
        (PlantKind::BerryBush, Season::Autumn) => 2,
        (PlantKind::Tree, Season::Spring) => 4,
        (PlantKind::Tree, Season::Summer) => 5,
        (PlantKind::Tree, Season::Autumn) => 3,
    }
}

/// Growth points required to leave `stage`. Returning 0 means "no
/// transition from this stage" (currently `Overripe`, and `Harvested` for
/// Grain — grain is annual and skips the regrow path).
pub fn stage_threshold(kind: PlantKind, stage: GrowthStage) -> u16 {
    match (kind, stage) {
        (PlantKind::Grain, GrowthStage::Seed) => 3,
        (PlantKind::Grain, GrowthStage::Seedling) => 5,
        (PlantKind::Grain, GrowthStage::Harvested) => 0,
        (PlantKind::Grain, GrowthStage::Mature) => 2,

        (PlantKind::BerryBush, GrowthStage::Seed) => 5,
        (PlantKind::BerryBush, GrowthStage::Seedling) => 30,
        (PlantKind::BerryBush, GrowthStage::Harvested) => 30,
        (PlantKind::BerryBush, GrowthStage::Mature) => 4,

        (PlantKind::Tree, GrowthStage::Seed) => 12,
        (PlantKind::Tree, GrowthStage::Seedling) => 48,
        (PlantKind::Tree, GrowthStage::Harvested) => 48,
        (PlantKind::Tree, GrowthStage::Mature) => 18,

        (_, GrowthStage::Overripe) => 0,
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

    /// Inverse of `seed_resource` — walks `PlantKind::ALL` to find the kind
    /// whose `seed_resource()` matches `rid`. Returns `None` for resources
    /// that aren't a plantable seed (or for kinds whose seed resource hasn't
    /// been registered in the catalog yet).
    pub fn from_seed_resource(
        rid: crate::economy::resource_catalog::ResourceId,
    ) -> Option<PlantKind> {
        PlantKind::ALL
            .iter()
            .copied()
            .find(|k| k.seed_resource() == Some(rid))
    }

    /// `true` when this plant kind can be deliberately sown via the farm
    /// pipeline — i.e. it has a registered seed resource. Used by farm
    /// posting classification, harvest-credit, and seed-stock accounting so
    /// adding a new plantable seed = one `PlantKind` entry + one arm in
    /// `seed_resource()`, no scattered crop-specific checks.
    pub fn is_farm_plantable(self) -> bool {
        self.seed_resource().is_some()
    }

    /// Ticks the agent must spend Working before a harvest triggers.
    /// Plants are harvested instantly (0); this mirrors how tile-based gathering uses work_ticks.
    pub fn harvest_work_ticks(self) -> u8 {
        0
    }

    /// Ticks a worker spends to clear this plant from a building footprint
    /// (as a `Task::ClearObstacle`). Trees take noticeably longer than
    /// bushes / grain. This is independent of `harvest_work_ticks` because
    /// the gather flow has its own pacing and shouldn't slow down for a
    /// foraging agent.
    pub fn clear_work_ticks(self) -> u32 {
        match self {
            PlantKind::Tree => 60,
            PlantKind::BerryBush => 15,
            PlantKind::Grain => 10,
        }
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
            PlantKind::Tree => (core_ids::wood(), if has_tool { 3 } else { 1 }),
        }
    }

    /// Fixed co-yields always added alongside the primary yield (no faction
    /// multiplier). `ResourceId`-typed; callers route through
    /// `core_ids::resource_id_to_good` while `Item::new_commodity` still
    /// takes `Good`.
    pub fn harvest_extra_yields(self) -> Vec<(ResourceId, u32)> {
        use crate::economy::core_ids;
        match self {
            // 2 seeds per harvest: a real grain harvest returns far more than
            // one replant-seed. At +1 the seed stock only breaks even (sow 1,
            // reap 1) so a seed-short founding village can never ramp toward
            // its demand target; +2 lets the stock grow year-over-year while
            // staying bounded.
            PlantKind::Grain => vec![(core_ids::grain_seed(), 2)],
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

    /// Whether harvesting destroys the plant (true) or reverts it to a regrowable
    /// stage (false). Trees always come down; bushes always regrow; grain (and any
    /// future kind) defaults to despawn.
    pub fn harvest_despawns(self, _has_tool: bool) -> bool {
        match self {
            PlantKind::Tree => true,
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
                growth: 0,
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

    // ────────────────────────────────────────────────────────────────────
    // Lifecycle tests
    // ────────────────────────────────────────────────────────────────────

    fn build_lifecycle_app() -> App {
        install_catalog();
        let mut app = App::new();
        app.insert_resource(Calendar::default());
        app.insert_resource(PlantMap::default());
        app.insert_resource(PlantSpriteIndex::default());
        app.insert_resource(ChunkMap::default());
        app.insert_resource(crate::simulation::seed_reservation::SeedReservation::default());
        app.add_systems(Update, plant_lifecycle_system);
        app
    }

    fn spawn_lifecycle_plant(
        app: &mut App,
        tile: (i32, i32),
        kind: PlantKind,
        stage: GrowthStage,
        growth: u16,
    ) -> Entity {
        let e = app
            .world_mut()
            .spawn(Plant {
                kind,
                stage,
                growth,
                tile_pos: tile,
            })
            .id();
        app.world_mut().resource_mut::<PlantMap>().0.insert(tile, e);
        e
    }

    fn set_season(app: &mut App, s: Season) {
        app.world_mut().resource_mut::<Calendar>().season = s;
    }

    fn plant_get(app: &App, e: Entity) -> Option<(GrowthStage, u16)> {
        app.world().get::<Plant>(e).map(|p| (p.stage, p.growth))
    }

    /// Tree growth is purely threshold-driven (no season gate). Accumulating
    /// the per-season rates should land Seedling→Mature in ~4 calendar
    /// years (12+ growth in seedling per year, threshold 48).
    #[test]
    fn tree_seedling_takes_about_four_years() {
        let mut app = build_lifecycle_app();
        let e = spawn_lifecycle_plant(&mut app, (0, 0), PlantKind::Tree, GrowthStage::Seedling, 0);

        // First update primes `last_season` and skips work.
        app.update();

        // Walk 4 calendar years of seasons.
        let cycle = [
            Season::Summer,
            Season::Autumn,
            Season::Winter,
            Season::Spring,
        ];
        let mut matured_year = None;
        for year in 1..=6 {
            for s in cycle {
                set_season(&mut app, s);
                app.update();
                if matches!(plant_get(&app, e), Some((GrowthStage::Mature, _))) {
                    matured_year = Some(year);
                    break;
                }
            }
            if matured_year.is_some() {
                break;
            }
        }

        let year = matured_year.expect("tree should mature within 6 years");
        assert!(
            (4..=5).contains(&year),
            "expected tree to mature in year 4 or 5, got year {year}"
        );
    }

    /// Trees and berries gain zero growth from a winter that just ended
    /// (dormancy applies to the season's growth contribution).
    #[test]
    fn winter_zero_growth_for_perennials() {
        let mut app = build_lifecycle_app();
        // Seed last_season=Winter so the next tick reads "prev=Winter".
        set_season(&mut app, Season::Winter);
        let tree =
            spawn_lifecycle_plant(&mut app, (0, 0), PlantKind::Tree, GrowthStage::Seedling, 5);
        let bush = spawn_lifecycle_plant(
            &mut app,
            (1, 0),
            PlantKind::BerryBush,
            GrowthStage::Seedling,
            5,
        );

        app.update(); // prime: last_season := Winter
                      // Now transition to Spring; prev=Winter contributes 0.
        set_season(&mut app, Season::Spring);
        app.update();

        assert_eq!(plant_get(&app, tree), Some((GrowthStage::Seedling, 5)));
        assert_eq!(plant_get(&app, bush), Some((GrowthStage::Seedling, 5)));
    }

    /// Same season twice is a no-op — the system edge-triggers on change.
    #[test]
    fn season_idempotent_within_same_season() {
        let mut app = build_lifecycle_app();
        let e = spawn_lifecycle_plant(&mut app, (0, 0), PlantKind::Tree, GrowthStage::Seedling, 0);

        app.update(); // prime: last_season = Spring
        app.update(); // still Spring → no-op
        app.update(); // still Spring → no-op
        assert_eq!(plant_get(&app, e), Some((GrowthStage::Seedling, 0)));
    }

    /// Grain alive at Winter onset despawns regardless of stage.
    #[test]
    fn grain_dies_at_winter_onset() {
        let mut app = build_lifecycle_app();
        let seedling =
            spawn_lifecycle_plant(&mut app, (0, 0), PlantKind::Grain, GrowthStage::Seedling, 3);
        let seed = spawn_lifecycle_plant(&mut app, (1, 0), PlantKind::Grain, GrowthStage::Seed, 0);

        app.update(); // prime
        set_season(&mut app, Season::Winter);
        app.update();

        assert!(
            plant_get(&app, seedling).is_none(),
            "seedling grain should despawn"
        );
        assert!(plant_get(&app, seed).is_none(), "seed grain should despawn");
        // PlantMap entries should also be cleaned up.
        let map = app.world().resource::<PlantMap>();
        assert!(map.0.get(&(0, 0)).is_none());
        assert!(map.0.get(&(1, 0)).is_none());
    }

    /// Sprouted grain (Seedling) matures by Autumn. Skips the 20% sprout
    /// dice to avoid RNG flakiness under cargo test's parallel runner.
    #[test]
    fn grain_seedling_matures_by_autumn() {
        let mut app = build_lifecycle_app();
        let e = spawn_lifecycle_plant(&mut app, (0, 0), PlantKind::Grain, GrowthStage::Seedling, 0);
        app.update(); // prime in Spring

        set_season(&mut app, Season::Summer);
        app.update(); // prev=Spring (+4 growth)
        set_season(&mut app, Season::Autumn);
        app.update(); // prev=Summer (+5 growth) → threshold 5 crossed → Mature

        assert_eq!(
            plant_get(&app, e).map(|(s, _)| s),
            Some(GrowthStage::Mature),
            "grain seedling should reach Mature by Autumn"
        );
    }

    /// Probabilistic sprint: across many runs, sprout success rate stays
    /// within a window of the configured 20% chance. Tests the dice path
    /// without relying on a single deterministic seed (which is fragile
    /// under parallel test execution).
    #[test]
    fn sprout_chance_is_about_twenty_percent() {
        const TRIALS: u32 = 4_000;
        let mut sprouts = 0u32;
        for _ in 0..TRIALS {
            let mut app = build_lifecycle_app();
            let e = spawn_lifecycle_plant(&mut app, (0, 0), PlantKind::Tree, GrowthStage::Seed, 12);
            app.update(); // prime
            set_season(&mut app, Season::Summer);
            app.update();
            if plant_get(&app, e).is_some() {
                sprouts += 1;
            }
        }
        let rate = sprouts as f32 / TRIALS as f32;
        assert!(
            (0.16..0.24).contains(&rate),
            "sprout rate {rate} (got {sprouts}/{TRIALS}) should be near 0.20"
        );
    }

    /// A `Cultivated` (deliberately-sown) grain seed skips the 20% wild
    /// sprout roll — it always advances to Seedling, so a planted field
    /// doesn't lose 80% of its tiles at the first season edge.
    #[test]
    fn cultivated_grain_seed_always_sprouts() {
        for _ in 0..200 {
            let mut app = build_lifecycle_app();
            let e =
                spawn_lifecycle_plant(&mut app, (0, 0), PlantKind::Grain, GrowthStage::Seed, 3);
            app.world_mut().entity_mut(e).insert(Cultivated);
            app.update(); // prime in Spring
            set_season(&mut app, Season::Summer);
            app.update(); // prev=Spring (+4) → threshold 3 crossed
            assert_eq!(
                plant_get(&app, e).map(|(s, _)| s),
                Some(GrowthStage::Seedling),
                "cultivated grain seed must always sprout"
            );
        }
    }

    /// Mature tree fruiting cycle reverts to Mature with zeroed growth excess
    /// regardless of whether scatter dice rolls success or failure.
    #[test]
    fn mature_tree_reverts_after_fruiting() {
        let mut app = build_lifecycle_app();
        let e = spawn_lifecycle_plant(&mut app, (0, 0), PlantKind::Tree, GrowthStage::Mature, 18);

        app.update(); // prime
        set_season(&mut app, Season::Summer);
        app.update();

        let (stage, growth) = plant_get(&app, e).expect("tree should still exist");
        assert_eq!(
            stage,
            GrowthStage::Mature,
            "tree reverts to Mature post-fruiting"
        );
        // Each transition resets growth to 0 (no carry-over).
        assert_eq!(growth, 0);
    }

    /// stage_threshold sanity check — Tree Seedling = 48 (≈4 calendar years
    /// at avg growth of 12/year).
    #[test]
    fn tree_seedling_threshold_constant() {
        assert_eq!(stage_threshold(PlantKind::Tree, GrowthStage::Seedling), 48);
        assert_eq!(stage_threshold(PlantKind::Grain, GrowthStage::Harvested), 0);
        assert_eq!(
            stage_threshold(PlantKind::BerryBush, GrowthStage::Mature),
            4
        );
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

/// Per-tile reservation for an in-flight planting chain
/// (`Task::Planter` / `Task::PlayPlant`). Multiple workers dispatched in
/// the same tick used to race for the nearest plantable tile and only one
/// would actually spawn a plant — the others walked, performed the planting
/// motion, and silently no-op'd. Reserving the destination tile at
/// dispatch-time and releasing on success/cancel makes the race impossible.
///
/// Reservation is keyed by tile only — one planting attempt per tile at a
/// time, regardless of seed/worker. `worker` + `seed_resource` are recorded
/// for the daily GC pass to detect stale entries (worker died /
/// Dormant-demoted / chain dropped without releasing).
#[derive(Resource, Default)]
pub struct PlantingReservations {
    pub by_tile: AHashMap<(i32, i32), PlantingReservation>,
}

#[derive(Clone, Copy, Debug)]
pub struct PlantingReservation {
    pub worker: Entity,
    pub seed_resource: crate::economy::resource_catalog::ResourceId,
    pub reserved_tick: u64,
}

impl PlantingReservations {
    /// Returns `true` if `tile` already carries a live reservation.
    pub fn is_reserved(&self, tile: (i32, i32)) -> bool {
        self.by_tile.contains_key(&tile)
    }

    /// Attempts to reserve `tile` for `worker`. Returns `true` on success,
    /// `false` if another worker already holds the slot.
    pub fn try_reserve(
        &mut self,
        tile: (i32, i32),
        worker: Entity,
        seed_resource: crate::economy::resource_catalog::ResourceId,
        now: u64,
    ) -> bool {
        if self.by_tile.contains_key(&tile) {
            return false;
        }
        self.by_tile.insert(
            tile,
            PlantingReservation {
                worker,
                seed_resource,
                reserved_tick: now,
            },
        );
        true
    }

    /// Drops the reservation at `tile`. No-op if `tile` isn't reserved.
    /// Idempotent — every teardown path can call this without checking.
    pub fn release(&mut self, tile: (i32, i32)) {
        self.by_tile.remove(&tile);
    }

    /// Drops every reservation held by `worker`. Used by the GC pass when a
    /// worker died / Dormant-demoted / dropped its chain without releasing.
    pub fn release_for_worker(&mut self, worker: Entity) {
        self.by_tile.retain(|_, r| r.worker != worker);
    }
}

/// Despawn a plant entity and clean up the spatial / sprite indices that
/// every plant-removing system relies on. Caller handles yields and any
/// `SharedKnowledge::report_depleted` reporting (those need contextual
/// inputs — agent_tier, harvester — that don't belong here).
pub fn despawn_plant_internals(
    commands: &mut Commands,
    entity: Entity,
    tile: (i32, i32),
    plant_map: &mut PlantMap,
    plant_sprite_index: &mut PlantSpriteIndex,
) {
    plant_map.0.remove(&tile);
    let cx = tile.0.div_euclid(CHUNK_SIZE as i32);
    let cy = tile.1.div_euclid(CHUNK_SIZE as i32);
    if let Some(vec) = plant_sprite_index.by_chunk.get_mut(&ChunkCoord(cx, cy)) {
        vec.retain(|(e, _)| *e != entity);
    }
    commands.entity(entity).despawn_recursive();
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

    let (clear_skill, clear_xp) = kind.harvest_skill_xp(false);
    let entity = commands
        .spawn((
            Plant {
                kind,
                stage,
                growth: 0,
                tile_pos: (tile_x as i32, tile_y as i32),
            },
            Transform::from_xyz(world_pos.x, world_pos.y, 0.5),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
            crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Plant),
            crate::simulation::obstacle::ConstructionObstacle {
                resolution: crate::simulation::obstacle::ObstacleResolution::WorkerClear {
                    work_ticks: kind.clear_work_ticks(),
                    skill: clear_skill,
                    skill_xp: clear_xp,
                },
            },
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

/// Tile predicate for where a scattered seed may land. Rejects tiles the
/// bootstrap pipeline reserved (footprints, doormats, planned roads,
/// ag plots) so a streaming-time plant can't land on a planned-but-uncarved
/// road or on a doormat that hasn't yet flipped to `Road`.
fn seed_target_tile_ok(
    chunk_map: &ChunkMap,
    reservation: &crate::simulation::seed_reservation::SeedReservation,
    x: i32,
    y: i32,
) -> bool {
    use crate::world::tile::TileKind as TK;
    if reservation.is_reserved((x, y)) {
        return false;
    }
    match chunk_map.tile_kind_at(x, y) {
        Some(TK::Grass) => true,
        Some(k) if k.is_soil_like() => true,
        _ => false,
    }
}

/// Roll once: with probability `chance`, drop one fresh `Seed` plant of
/// `parent_kind` somewhere in chebyshev `radius` of the parent. No-op on
/// miss or on collision; never spawns at the parent's own tile.
fn try_scatter_seed(
    commands: &mut Commands,
    plant_map: &mut PlantMap,
    plant_sprite_index: &mut PlantSpriteIndex,
    chunk_map: &ChunkMap,
    reservation: &crate::simulation::seed_reservation::SeedReservation,
    parent_tile: (i32, i32),
    parent_kind: PlantKind,
    chance: f32,
    radius: i32,
) {
    if fastrand::f32() >= chance {
        return;
    }
    let dx = fastrand::i32(-radius..=radius);
    let dy = fastrand::i32(-radius..=radius);
    if dx == 0 && dy == 0 {
        return;
    }
    let (nx, ny) = (parent_tile.0 + dx, parent_tile.1 + dy);
    if !seed_target_tile_ok(chunk_map, reservation, nx, ny) {
        return;
    }
    spawn_plant_at(
        commands,
        plant_map,
        plant_sprite_index,
        nx,
        ny,
        parent_kind,
        GrowthStage::Seed,
    );
}

/// Calendar-driven plant lifecycle. Replaces the legacy `plant_growth_system` +
/// `seed_scatter_system` pair.
///
/// Fires once per season transition (≤4 calls per game year). Each call:
///   1. Applies winter mortality to grain (annual crop dies in winter; mature
///      plants get a final scatter roll first).
///   2. Adds growth points for the season that just ended (`season_growth`).
///   3. Advances stages while `growth >= stage_threshold(...)`. Sprouts,
///      maturation, and Mature→Overripe spread rolls all run inline; multiple
///      thresholds can cross in one call (e.g. grain Seed→Seedling→Mature on
///      a hot summer if it sprouted late in spring).
pub fn plant_lifecycle_system(
    mut commands: Commands,
    calendar: Res<Calendar>,
    chunk_map: Res<ChunkMap>,
    reservation: Res<crate::simulation::seed_reservation::SeedReservation>,
    mut last_season: Local<Option<Season>>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    mut query: Query<(Entity, &mut Plant, Option<&Cultivated>)>,
) {
    let prev = match *last_season {
        Some(s) if s == calendar.season => return,
        Some(s) => s,
        None => {
            *last_season = Some(calendar.season);
            return;
        }
    };
    *last_season = Some(calendar.season);

    // Snapshot per-entity work to keep the borrow on `query` short while we
    // mutate `plant_map` / spawn new plants below.
    let mut to_kill: Vec<(Entity, (i32, i32), bool)> = Vec::new();
    let mut scatter_jobs: Vec<((i32, i32), PlantKind, f32, i32)> = Vec::new();

    for (entity, mut plant, cultivated) in query.iter_mut() {
        // ── Winter mortality for grain ────────────────────────────────────
        if calendar.season == Season::Winter && plant.kind == PlantKind::Grain {
            let scatter_now = plant.stage == GrowthStage::Mature;
            to_kill.push((entity, plant.tile_pos, scatter_now));
            continue;
        }

        // ── Accumulate growth from the season that just ended ────────────
        let gain = season_growth(plant.kind, prev) as u16;
        plant.growth = plant.growth.saturating_add(gain);

        // ── Stage advance — at most one transition per season tick ──────
        // `growth` tracks time spent in the current stage; on transition
        // we reset to zero so each stage gets its full duration regardless
        // of carry-over from the previous one. (E.g. grain Mature has a
        // small threshold; carry-over would blast through Mature→Overripe
        // in the same tick that Seedling→Mature fires.)
        let threshold = stage_threshold(plant.kind, plant.stage);
        if threshold > 0 && plant.growth >= threshold {
            match plant.stage {
                GrowthStage::Seed => {
                    // Deliberately-sown crops (`Cultivated`) sprout reliably;
                    // only wild scatter / PlayPlant rolls the 20% odds.
                    if cultivated.is_some() || fastrand::f32() < 0.20 {
                        plant.growth = 0;
                        plant.stage = GrowthStage::Seedling;
                    } else {
                        to_kill.push((entity, plant.tile_pos, false));
                    }
                }
                GrowthStage::Seedling | GrowthStage::Harvested => {
                    plant.growth = 0;
                    plant.stage = GrowthStage::Mature;
                }
                GrowthStage::Mature => {
                    let (chance, radius) = match plant.kind {
                        PlantKind::Grain => (0.20_f32, 2_i32),
                        PlantKind::BerryBush => (0.10, 2),
                        PlantKind::Tree => (0.05, 3),
                    };
                    scatter_jobs.push((plant.tile_pos, plant.kind, chance, radius));

                    plant.growth = 0;
                    plant.stage = match plant.kind {
                        PlantKind::Grain => GrowthStage::Overripe,
                        PlantKind::BerryBush => GrowthStage::Harvested,
                        PlantKind::Tree => GrowthStage::Mature,
                    };
                }
                GrowthStage::Overripe => {}
            }
        }
    }

    // ── Apply scatter jobs ───────────────────────────────────────────────
    for (parent_tile, kind, chance, radius) in scatter_jobs {
        try_scatter_seed(
            &mut commands,
            &mut plant_map,
            &mut plant_sprite_index,
            &chunk_map,
            &reservation,
            parent_tile,
            kind,
            chance,
            radius,
        );
    }

    // ── Apply death pass (after scatter so winter-mature grain seeds first) ─
    for (entity, tile, scatter_first) in to_kill {
        if scatter_first {
            try_scatter_seed(
                &mut commands,
                &mut plant_map,
                &mut plant_sprite_index,
                &chunk_map,
                &reservation,
                tile,
                PlantKind::Grain,
                0.20,
                2,
            );
        }
        if plant_map.0.get(&tile) == Some(&entity) {
            despawn_plant_internals(
                &mut commands,
                entity,
                tile,
                &mut plant_map,
                &mut plant_sprite_index,
            );
        }
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
                plant.growth = 0;
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
                        item: Item::new_commodity(crate::economy::core_ids::berry_seed()),
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

/// Daily GC for `PlantingReservations`. Drops any reservation whose holder
/// (a) no longer exists (died / despawned), (b) is no longer running or
/// queueing a `Planter`/`PlayPlant` task for the reserved tile, or (c) was
/// reserved more than `RESERVATION_MAX_AGE_TICKS` ticks ago (catches goal-
/// flip cancels, Dormant LOD demotions, and any other release path the
/// executor / chain-handoff didn't cover).
///
/// Without this backstop a stale reservation pins a plantable tile out of
/// reach forever — the per-task release sites cover the common cases but a
/// goal-flip mid-walk drops the chain without notifying us. A daily sweep
/// is plenty: the worst-case latency on a leaked slot is one game-day.
pub fn planting_reservation_gc_system(
    clock: Res<SimClock>,
    mut reservations: ResMut<PlantingReservations>,
    aq_q: Query<&crate::simulation::typed_task::ActionQueue>,
) {
    const RESERVATION_MAX_AGE_TICKS: u64 = 3600; // one game-day
    let now = clock.tick;
    if now % RESERVATION_MAX_AGE_TICKS != 0 {
        return;
    }
    reservations.by_tile.retain(|tile, r| {
        if now.saturating_sub(r.reserved_tick) > RESERVATION_MAX_AGE_TICKS {
            return false;
        }
        let Ok(aq) = aq_q.get(r.worker) else {
            // Worker entity gone.
            return false;
        };
        // Reservation is live iff the worker's current or any queued task is
        // a Planter/PlayPlant aimed at *this* tile. Match by tile so a
        // worker that chained from one planting task to another at a
        // different tile drops the stale slot.
        let cur_matches = aq
            .current
            .as_planter_full()
            .map(|(t, _)| t == *tile)
            .unwrap_or(false)
            || aq
                .current
                .as_play_plant_full()
                .map(|(t, _)| t == *tile)
                .unwrap_or(false);
        if cur_matches {
            return true;
        }
        // Walk the prefetch ring for a queued planting leg.
        for slot in aq.queued_iter() {
            if let Some((t, _)) = slot.as_planter_full() {
                if t == *tile {
                    return true;
                }
            }
            if let Some((t, _)) = slot.as_play_plant_full() {
                if t == *tile {
                    return true;
                }
            }
        }
        false
    });
}
