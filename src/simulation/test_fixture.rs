//! Headless test harness for the simulation plugin.
//!
//! Builds an `App` configured like the real game minus rendering, UI, and
//! globe generation. Tests construct a `TestSim`, scaffold a flat patch of
//! grass, spawn agents with explicit needs / inventory / goal, and tick the
//! schedule a controlled number of frames before asserting.
//!
//! Behavioural fixtures live alongside the systems they assert against
//! (e.g. `simulation::plan::tests`, `simulation::tasks::tests`). This module
//! provides the shared scaffolding only.

#![cfg(test)]

use std::time::Duration;

use ahash::AHashMap;
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy::time::{TimePlugin, TimeUpdateStrategy};

use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::economy::item::Item;
use crate::pathfinding::path_request::PathFollow;
use crate::simulation::carry::Carrier;
use crate::simulation::combat::{Body, CombatCooldown, CombatTarget};
use crate::simulation::faction::{FactionMember, FactionStorageTile, PlayerFaction};
use crate::simulation::goals::{AgentGoal, Personality};
use crate::simulation::items::{Equipment, TargetItem};
use crate::simulation::knowledge::PersonKnowledge;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::{AgentMemory, RelationshipMemory};
use crate::simulation::mood::Mood;
use crate::simulation::movement::MovementState;
use crate::simulation::needs::Needs;
use crate::simulation::person::{
    AiState, HairColor, Person, PersonAI, Profession, SkinTone,
};
use crate::simulation::htn::MethodHistory;
use crate::simulation::plan::{KnownPlans, PlanHistory, PlanId, PlanScoringMethod};
use crate::simulation::reproduction::BiologicalSex;
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::Skills;
use crate::simulation::stats::Stats;
use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::spatial::{Indexed, IndexedKind};
use crate::world::terrain::tile_to_world;
use crate::world::tile::TileKind;

/// Tick frequency the real game runs at (`Time::<Fixed>::from_hz(20.0)`).
pub const TEST_TICK_HZ: f64 = 20.0;
const TICK_DURATION: Duration = Duration::from_nanos(50_000_000); // 1/20 s

/// Headless app harness for behavioural simulation tests.
///
/// Construct with [`TestSim::new`], then build a world with [`flat_world`]
/// and spawn agents via [`spawn_person`]. Drive the schedule with
/// [`tick`] / [`tick_n`].
pub struct TestSim {
    pub app: App,
    pub player_faction_id: u32,
}

impl TestSim {
    /// Build a new headless app seeded with `seed` (drives `fastrand`).
    pub fn new(seed: u64) -> Self {
        fastrand::seed(seed);

        let mut app = App::new();

        // Time + states. We deliberately skip ScheduleRunnerPlugin /
        // FrameCountPlugin / TaskPoolPlugin — `MinimalPlugins` is a
        // convenient bundle but pulls in a few non-essentials. We add the
        // pieces we need explicitly so the harness fails fast if a sim
        // system grows a new dependency.
        app.add_plugins((TimePlugin, StatesPlugin));
        app.insert_resource(Time::<Fixed>::from_hz(TEST_TICK_HZ));
        // Override Bevy's real-time clock with a fixed per-frame
        // duration. Each `app.update()` advances Time by exactly
        // `TICK_DURATION`, which matches the FixedUpdate timestep so
        // FixedUpdate runs once per app.update() with `delta_secs() ==
        // 1/TEST_TICK_HZ` — no real-clock noise.
        app.insert_resource(TimeUpdateStrategy::ManualDuration(TICK_DURATION));

        // Asset machinery. SimulationPlugin doesn't touch assets, but
        // PathfindingPlugin's chunk-graph rebuild reads ChunkMap and that's
        // about it — no ColorMaterial/Mesh assets are touched on this code
        // path. We skip AssetPlugin entirely.

        // Game state — stay in SpawnSelect so we never run
        // person::spawn_population (which would try to allocate 200
        // agents using the real globe / world generator).
        app.init_state::<crate::GameState>();
        app.insert_resource(crate::PendingSpawn::default());

        // Resource catalog must be inserted before any system queries
        // it. Idempotent across test runs because OnceLock::set on a
        // populated cell silently no-ops.
        let catalog = crate::economy::resource_catalog::load_resource_catalog();
        crate::economy::core_ids::install_catalog(catalog.clone());
        app.insert_resource(catalog);

        // World resources (mirrors WorldPlugin minus the rendering
        // PostUpdate system). Globe::new is empty — enough for chunk
        // streaming queries that only read ChunkMap.
        app.world_mut()
            .register_component_hooks::<Indexed>()
            .on_remove(crate::world::spatial::on_indexed_remove);
        app.insert_resource(crate::world::globe::Globe::new(seed));
        app.insert_resource(ChunkMap::default());
        app.insert_resource(crate::world::spatial::SpatialIndex::default());
        app.insert_resource(crate::world::seasons::Calendar::default());
        app.insert_resource(crate::world::terrain::WorldGen::new());
        app.insert_resource(crate::world::chunk_streaming::ChunkRetention::default());
        app.add_event::<crate::world::chunk_streaming::TileChangedEvent>();
        app.add_event::<crate::world::chunk_streaming::ChunkLoadedEvent>();
        app.add_event::<crate::world::chunk_streaming::ChunkUnloadedEvent>();

        // Region resources (normally inserted in main.rs).
        app.insert_resource(crate::simulation::region::SettledRegions::default());
        app.insert_resource(crate::simulation::region::SimulationFocus::default());

        // Rendering-side resources that sim systems read but don't write.
        // (lod.rs reads CameraState to compute LOD distance.)
        app.insert_resource(crate::rendering::camera::CameraState::default());

        // UI-side events that sim systems emit (activity log etc). Without
        // these registered, EventWriter::send panics.
        app.add_event::<crate::ui::activity_log::ActivityLogEvent>();

        // The real plugins. SimulationPlugin's OnEnter(Playing) hooks
        // never fire because we stay in SpawnSelect.
        app.add_plugins(crate::pathfinding::PathfindingPlugin);
        app.add_plugins(crate::economy::EconomyPlugin);
        app.add_plugins(crate::simulation::SimulationPlugin);

        // Spawn a camera at world origin. Without it,
        // `update_lod_levels_system` reports every agent as Dormant
        // (cam_dist = i32::MAX) and every task executor skips them.
        // Headless tests don't render, but the LOD logic doesn't know
        // that and gates on Camera presence.
        app.world_mut().spawn((
            Camera2d,
            Transform::from_xyz(0.0, 0.0, 100.0),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
        ));

        // Seed a player faction so faction-aware systems have something to
        // read. Tests that want their own factions can ignore this.
        let player_faction_id = {
            let mut registry = app
                .world_mut()
                .resource_mut::<crate::simulation::faction::FactionRegistry>();
            registry.create_faction((0, 0))
        };
        app.world_mut()
            .resource_mut::<PlayerFaction>()
            .faction_id = player_faction_id;

        Self {
            app,
            player_faction_id,
        }
    }

    /// Insert a flat patch of `kind`-tiles at `surface_z` covering
    /// chunks `[(-radius, -radius)..=(radius, radius)]` (inclusive).
    pub fn flat_world(&mut self, radius: i32, surface_z: i8, kind: TileKind) {
        let mut chunk_map = self.app.world_mut().resource_mut::<ChunkMap>();
        for cy in -radius..=radius {
            for cx in -radius..=radius {
                let chunk = flat_chunk(surface_z, kind);
                chunk_map.0.insert(ChunkCoord(cx, cy), chunk);
            }
        }
    }

    /// Spawn a `Person` at world tile `(tx, ty)` belonging to `faction_id`.
    /// `customise` runs after the bundle is built so callers can tweak
    /// needs / skills / inventory before it lands in the world.
    pub fn spawn_person<F>(
        &mut self,
        faction_id: u32,
        tile: (i32, i32),
        customise: F,
    ) -> Entity
    where
        F: FnOnce(&mut PersonBuilder),
    {
        let surface_z = self
            .app
            .world()
            .resource::<ChunkMap>()
            .surface_z_at(tile.0, tile.1) as i8;
        let mut builder = PersonBuilder::new(faction_id, tile, surface_z);
        customise(&mut builder);
        let entity = builder.spawn(self.app.world_mut());

        // Account for it in the SimClock bucketing so plan scoring runs.
        let mut clock = self.app.world_mut().resource_mut::<SimClock>();
        clock.population += 1;
        clock.current_end = clock.bucket_size.min(clock.population);

        entity
    }

    /// Spawn a faction storage tile at `(tx, ty)` so storage-aware
    /// systems (compute_faction_storage_system, withdraw resolvers) treat
    /// the location as a real depot.
    pub fn spawn_storage_tile(&mut self, faction_id: u32, tile: (i32, i32)) -> Entity {
        let world_pos = tile_to_world(tile.0, tile.1);
        self.app
            .world_mut()
            .spawn((
                FactionStorageTile { faction_id },
                Transform::from_xyz(world_pos.x, world_pos.y, 0.5),
                GlobalTransform::default(),
                Visibility::Hidden,
                InheritedVisibility::default(),
            ))
            .id()
    }

    /// Drop a stack of `good` × `qty` directly on `(tx, ty)`. Spawned as
    /// a `GroundItem` with the standard `Indexed` hook so spatial-index
    /// queries find it on the next sync.
    pub fn spawn_ground_item(&mut self, tile: (i32, i32), good: Good, qty: u32) -> Entity {
        use crate::simulation::items::GroundItem;
        let world_pos = tile_to_world(tile.0, tile.1);
        self.app
            .world_mut()
            .spawn((
                GroundItem {
                    item: Item::new_commodity(good),
                    qty,
                },
                Transform::from_xyz(world_pos.x, world_pos.y, 0.3),
                GlobalTransform::default(),
                Visibility::Hidden,
                InheritedVisibility::default(),
                Indexed::new(IndexedKind::GroundItem),
            ))
            .id()
    }

    /// Run a single frame. With `TimeUpdateStrategy::ManualDuration`
    /// installed, each call advances `Time` by exactly one fixed
    /// timestep so `FixedUpdate` fires once per call.
    pub fn tick(&mut self) {
        self.app.update();
    }

    /// Convenience: tick `n` times.
    pub fn tick_n(&mut self, n: u32) {
        for _ in 0..n {
            self.tick();
        }
    }

    /// Look up the current `SimClock.tick`.
    pub fn tick_count(&self) -> u64 {
        self.app.world().resource::<SimClock>().tick
    }
}

/// Builder pattern for spawning customised people in tests.
pub struct PersonBuilder {
    faction_id: u32,
    tile: (i32, i32),
    surface_z: i8,
    needs: Needs,
    skills: Skills,
    profession: Profession,
    goal: AgentGoal,
    inventory: Vec<(Good, u32)>,
    known_plan_ids: Vec<PlanId>,
    bucket: u32,
}

impl PersonBuilder {
    fn new(faction_id: u32, tile: (i32, i32), surface_z: i8) -> Self {
        Self {
            faction_id,
            tile,
            surface_z,
            needs: Needs::new(30.0, 20.0, 10.0, 5.0, 40.0, 200.0),
            skills: Skills::default(),
            profession: Profession::None,
            goal: AgentGoal::default(),
            inventory: Vec::new(),
            // Match the live spawn_population innate-plan list so
            // candidate filtering behaves identically.
            known_plan_ids: vec![
                // FORAGE_FOOD retired in the Forage→HTN migration.
                PlanId::FARM_FOOD,
                // GATHER_WOOD / GATHER_STONE retired 5c-ii-c-ii.
                PlanId::HUNT_FOOD,
                // SCAVENGE_FOOD retired 5c-ii-d-vi.
                PlanId::BUILD_BLUEPRINT,
                PlanId::TAME_HORSE,
                PlanId::DELIVER_HIDE_TO_CRAFT_ORDER,
                PlanId::DELIVER_GRAIN_TO_CRAFT_ORDER,
                PlanId::DELIVER_FROM_STORAGE_TO_CRAFT_ORDER,
                PlanId::WORK_ON_CRAFT,
                PlanId::RESCUE_ALLY,
                PlanId::RETURN_SURPLUS_FOOD,
                PlanId::PLAY_SOCIAL,
                PlanId::PLAY_SOLO,
                PlanId::HAUL_FROM_STORAGE_AND_BUILD,
                PlanId::PLAY_BY_PLANTING,
                PlanId::PLAY_BY_THROWING_ROCKS,
                PlanId::PLAY_WITH_STORED_TOY,
                PlanId::CLAIMED_BUILD,
                // EXPLORE_FOR_FOOD retired 5c-ii-d-vi.
                // EXPLORE_FOR_WOOD / EXPLORE_FOR_STONE retired 5c-ii-d-iv-ii.
                // SCAVENGE_WOOD / SCAVENGE_STONE retired 5c-ii-d-ii-b.
                PlanId::SOCIALIZE,
                PlanId::RAID,
                PlanId::DEFEND,
                PlanId::LEAD,
                PlanId::ACQUIRE_HUNTING_SPEAR,
                PlanId::SCOUT_FOR_PREY,
            ],
            bucket: 0,
        }
    }

    pub fn needs(&mut self, needs: Needs) -> &mut Self {
        self.needs = needs;
        self
    }

    pub fn hunger(&mut self, hunger: f32) -> &mut Self {
        self.needs.hunger = hunger;
        self
    }

    pub fn skills(&mut self, skills: Skills) -> &mut Self {
        self.skills = skills;
        self
    }

    pub fn profession(&mut self, profession: Profession) -> &mut Self {
        self.profession = profession;
        self
    }

    pub fn goal(&mut self, goal: AgentGoal) -> &mut Self {
        self.goal = goal;
        self
    }

    pub fn add_inventory(&mut self, good: Good, qty: u32) -> &mut Self {
        self.inventory.push((good, qty));
        self
    }

    pub fn known_plans(&mut self, plans: Vec<PlanId>) -> &mut Self {
        self.known_plan_ids = plans;
        self
    }

    pub fn bucket(&mut self, bucket: u32) -> &mut Self {
        self.bucket = bucket;
        self
    }

    fn spawn(self, world: &mut World) -> Entity {
        let world_pos = tile_to_world(self.tile.0, self.tile.1);
        let sex = BiologicalSex::random();

        let mut economic = EconomicAgent::default();
        for (good, qty) in &self.inventory {
            economic.add_item(Item::new_commodity(*good), *qty);
        }

        let now_tick = world.resource::<SimClock>().tick;

        world
            .spawn((
                (
                    Person,
                    Transform::from_xyz(world_pos.x, world_pos.y, 1.0),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    self.needs,
                    Mood::default(),
                    self.skills,
                    Stats::roll_3d6(),
                    PersonAI {
                        task_id: PersonAI::UNEMPLOYED,
                        state: AiState::Idle,
                        target_tile: self.tile,
                        dest_tile: self.tile,
                        last_plan_id: PersonAI::UNEMPLOYED,
                        current_z: self.surface_z,
                        target_z: self.surface_z,
                        ..PersonAI::default()
                    },
                    economic,
                ),
                (
                    LodLevel::Full,
                    BucketSlot(self.bucket),
                    MovementState::default(),
                    sex,
                    SkinTone::random(),
                    HairColor::random(),
                    Personality::random(),
                    self.goal,
                    self.profession,
                    FactionMember {
                        faction_id: self.faction_id,
                        ..Default::default()
                    },
                    Body::new_humanoid(),
                ),
                (
                    Equipment::default(),
                    TargetItem::default(),
                    CombatTarget::default(),
                    CombatCooldown::default(),
                ),
                (
                    AgentMemory::default(),
                    RelationshipMemory::default(),
                    KnownPlans::with_innate(&self.known_plan_ids),
                    PlanHistory::default(),
                    MethodHistory::default(),
                    PlanScoringMethod::Weighted,
                    Name::new("TestPerson"),
                    PathFollow::default(),
                    Carrier::default(),
                    crate::simulation::reproduction::CoSleepTracker::default(),
                    crate::simulation::reproduction::MaleConceptionCooldown::default(),
                    Indexed::new(IndexedKind::Person),
                    PersonKnowledge::paleolithic_seed(now_tick as u32),
                    crate::simulation::typed_task::ActionQueue::idle(),
                ),
            ))
            .id()
    }
}

/// Build a single chunk where every (lx, ly) reads as `surface_z` of
/// `kind`. Subsurface tiles synthesise as Wall via `Chunk::tile_at_local`.
fn flat_chunk(surface_z: i8, kind: TileKind) -> Chunk {
    let surface_z_arr = Box::new([[surface_z; CHUNK_SIZE]; CHUNK_SIZE]);
    let surface_kind = Box::new([[kind; CHUNK_SIZE]; CHUNK_SIZE]);
    let surface_fertility = Box::new([[8u8; CHUNK_SIZE]; CHUNK_SIZE]);
    Chunk::new(surface_z_arr, surface_kind, surface_fertility)
}

/// Quick accessor for inspecting an agent's PersonAI without verbose
/// world-query boilerplate at the call site.
pub fn person_ai(app: &App, entity: Entity) -> PersonAI {
    *app.world().get::<PersonAI>(entity).expect("PersonAI missing")
}

/// Quick accessor for an agent's typed `ActionQueue.current` task. Phase 4a
/// promoted the typed task off `PersonAI` onto its own component, so tests
/// that used to read `person_ai(...).task` must now go through this helper.
pub fn person_task(app: &App, entity: Entity) -> crate::simulation::typed_task::Task {
    app.world()
        .get::<crate::simulation::typed_task::ActionQueue>(entity)
        .expect("ActionQueue missing")
        .current
}

/// Quick accessor for an agent's EconomicAgent (returns a clone).
pub fn person_inventory(app: &App, entity: Entity) -> AHashMap<Good, u32> {
    let econ = app
        .world()
        .get::<EconomicAgent>(entity)
        .expect("EconomicAgent missing");
    let mut out = AHashMap::new();
    for (item, qty) in econ.inventory.iter() {
        if *qty > 0 {
            *out.entry(item.good()).or_insert(0) += *qty;
        }
    }
    out
}

#[cfg(test)]
mod smoke {
    use super::*;

    /// The fixture itself must build, accept a single chunk and a single
    /// person, and tick once without panicking. Catches missing-resource
    /// regressions whenever a sim system grows a new dependency.
    #[test]
    fn fixture_builds_and_ticks() {
        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let _person = sim.spawn_person(sim.player_faction_id, (4, 4), |_| {});
        sim.tick_n(5);
        assert!(sim.tick_count() > 0);
    }
}

/// Phase 2b parity tests. Confirms that the new `ResourceId`-keyed APIs
/// on `EconomicAgent`, `Carrier`, and `Bulk` give the same answers as
/// the legacy `Good`-keyed methods for every legacy resource. Locks the
/// migration: Phase 2c can't quietly diverge consumer behaviour.
#[cfg(test)]
mod resource_id_parity {
    use super::*;
    use crate::economy::core_ids::good_to_resource_id;
    use crate::economy::core_ids::resource_id_to_good;
    use crate::economy::goods::{Bulk, Good};
    use crate::economy::resource_catalog::ResourceCatalog;
    use crate::simulation::carry::Carrier;

    /// Constructing a fixture initialises `core_ids` and the catalog as
    /// a side effect. Helper extracts the catalog so the test bodies can
    /// stay short.
    fn catalog_from_fixture() -> (TestSim, &'static ResourceCatalog) {
        let sim = TestSim::new(0xC4742106);
        // Borrow the catalog with a static lifetime via the world's
        // resource pointer — only safe because the catalog is read-only
        // after init and the App outlives the test body.
        let catalog: *const ResourceCatalog = sim.app.world().resource::<ResourceCatalog>();
        // SAFETY: `sim` is held for the full test body via the returned
        // tuple. Catalog lives inside `sim.app`'s world; the static cast
        // is purely to satisfy the borrow checker for the test's
        // ergonomics.
        let catalog: &'static ResourceCatalog = unsafe { &*catalog };
        (sim, catalog)
    }

    /// `core_ids::good_to_resource_id` and `resource_id_to_good` form
    /// inverse mappings on the 22 legacy goods.
    #[test]
    fn good_resource_id_is_invertible() {
        let _sim = TestSim::new(0xA001);
        for good in Good::all() {
            let id = good_to_resource_id(good);
            assert_eq!(
                resource_id_to_good(id),
                Some(good),
                "round-trip Good::{:?} → ResourceId({}) → Good failed",
                good,
                id.raw()
            );
        }
    }

    /// `Bulk::for_resource(id, &catalog)` returns the same `Bulk` as the
    /// legacy `Good::bulk()` for every legacy good.
    #[test]
    fn bulk_lookup_matches_legacy_for_every_good() {
        let (_sim, catalog) = catalog_from_fixture();
        for good in Good::all() {
            let id = good_to_resource_id(good);
            let from_catalog = Bulk::for_resource(id, catalog).expect("catalog has bulk");
            assert_eq!(
                from_catalog,
                good.bulk(),
                "Bulk mismatch for {:?}: catalog={:?}, legacy={:?}",
                good,
                from_catalog,
                good.bulk()
            );
        }
    }

    /// Adding via `EconomicAgent::add_resource` and reading via
    /// `quantity_of_resource` round-trips identically to the
    /// `add_good`/`quantity_of` pair for every legacy resource.
    #[test]
    fn economic_agent_resource_apis_match_good_apis() {
        let _sim = TestSim::new(0xA002);
        for good in Good::all() {
            let id = good_to_resource_id(good);

            let mut via_good = crate::economy::agent::EconomicAgent::default();
            let leftover_good = via_good.add_good(good, 3);

            let mut via_resource = crate::economy::agent::EconomicAgent::default();
            let leftover_resource = via_resource.add_resource(id, 3);

            assert_eq!(
                leftover_good, leftover_resource,
                "{:?}: add_good leftover={}, add_resource leftover={}",
                good, leftover_good, leftover_resource
            );
            assert_eq!(
                via_good.quantity_of(good),
                via_resource.quantity_of_resource(id),
                "{:?}: quantity_of vs quantity_of_resource diverge",
                good
            );

            // iter_resource_stacks reports the resource — but only when
            // at least one unit actually fit. Heavy goods (Armor at 8kg
            // vs the 5kg base cap) leave the inventory empty.
            let stacks: Vec<_> = via_resource.iter_resource_stacks().collect();
            let added = via_resource.quantity_of_resource(id);
            if added > 0 {
                assert!(
                    stacks.iter().any(|(rid, q)| *rid == id && *q == added),
                    "{:?} not visible in iter_resource_stacks: {:?}",
                    good,
                    stacks
                );
            } else {
                assert!(
                    stacks.is_empty(),
                    "{:?} reported empty quantity but iter_resource_stacks shows {:?}",
                    good,
                    stacks
                );
            }
        }
    }

    /// `Carrier::pickup_capacity_resource` matches `pickup_capacity` for
    /// commodity items derived from the same good.
    #[test]
    fn carrier_pickup_capacity_resource_matches_legacy() {
        let _sim = TestSim::new(0xA003);
        let carrier = Carrier::default();
        for good in Good::all() {
            let id = good_to_resource_id(good);
            let item = crate::economy::item::Item::new_commodity(good);
            assert_eq!(
                carrier.pickup_capacity(item),
                carrier.pickup_capacity_resource(id),
                "pickup capacity diverges for {:?}",
                good,
            );
        }
    }
}

/// Behavioural baselines pinned by Phase 0. These fixtures lock in the
/// observable AI behaviour of the legacy plan/task system so that the
/// HTN migration phases can detect regressions.
#[cfg(test)]
mod baseline_behaviour {
    use super::*;
    use crate::simulation::tasks::TaskKind;

    /// A hungry agent carrying food in inventory selects the
    /// EatFromInventory plan and consumes food within a few hundred ticks.
    /// Pins: needs → goal selection → plan candidate filter → plan
    /// scoring → step dispatch → eat task pipeline.
    #[test]
    fn hungry_agent_eats_from_inventory() {
        let mut sim = TestSim::new(1);
        sim.flat_world(1, 0, TileKind::Grass);
        let person = sim.spawn_person(sim.player_faction_id, (4, 4), |b| {
            b.hunger(210.0).add_inventory(Good::Fruit, 10);
        });

        let initial_food = person_inventory(&sim.app, person)
            .get(&Good::Fruit)
            .copied()
            .unwrap_or(0);
        assert_eq!(initial_food, 10);

        // Eat task takes TICKS_EAT (~60) ticks of Working state to fire,
        // and goal_update_system has a 32-tick cooldown. 400 ticks is
        // ample headroom.
        sim.tick_n(400);

        let final_food = person_inventory(&sim.app, person)
            .get(&Good::Fruit)
            .copied()
            .unwrap_or(0);
        assert!(
            final_food < initial_food,
            "expected hungry agent to eat at least one Fruit (started {}, ended {})",
            initial_food,
            final_food
        );
    }

    /// A `PlayerOrder::Move` short-circuits autonomous goal selection and
    /// dispatches the agent to the ordered tile. We assert the agent
    /// leaves its starting tile within a handful of ticks even though it
    /// has no autonomous reason to move.
    #[test]
    fn player_order_move_short_circuits_autonomy() {
        use crate::simulation::person::{PlayerOrder, PlayerOrderKind};

        let mut sim = TestSim::new(2);
        sim.flat_world(1, 0, TileKind::Grass);
        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |b| {
            b.hunger(0.0); // sated → no autonomous Survive goal
        });

        let start_pos = sim
            .app
            .world()
            .get::<Transform>(person)
            .unwrap()
            .translation;

        sim.app.world_mut().entity_mut(person).insert(PlayerOrder {
            order: PlayerOrderKind::Move,
            target_tile: (8, 0),
            target_z: 0,
        });

        sim.tick_n(120);

        let end_pos = sim
            .app
            .world()
            .get::<Transform>(person)
            .unwrap()
            .translation;
        let moved = (end_pos - start_pos).length();
        assert!(
            moved > 1.0,
            "expected PlayerOrder::Move to move agent; moved {} units",
            moved
        );
    }

    /// `compute_faction_storage_system` (Economy) walks each faction
    /// storage tile and reports totals on `FactionData.storage`. This
    /// pins the resource → faction-storage rollup pipeline so changes to
    /// indexing don't silently zero out chief decisions.
    #[test]
    fn ground_items_at_storage_tile_count_in_faction_storage() {
        use crate::simulation::faction::FactionRegistry;

        let mut sim = TestSim::new(3);
        sim.flat_world(1, 0, TileKind::Grass);
        let storage_tile = (4, 4);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);
        sim.spawn_ground_item(storage_tile, Good::Wood, 5);

        // Storage rollup runs in Economy each tick but spatial-index
        // sync needs a Transform-changed pass first. ~80 ticks is
        // overkill but cheap.
        sim.tick_n(80);

        let registry = sim.app.world().resource::<FactionRegistry>();
        let faction = registry
            .factions
            .get(&sim.player_faction_id)
            .expect("player faction missing");
        let wood_total = faction.storage.stock_of(Good::Wood);
        assert!(
            wood_total > 0,
            "expected wood at storage tile to register on faction storage; got {}",
            wood_total
        );
    }

    /// Confirms an idle, well-fed agent doesn't get stuck in an
    /// active task. After a long idle stretch they should remain
    /// `AiState::Idle` (or briefly `Working`/`Seeking` while running
    /// short Socialize/Play plans, but never accumulate a leaked
    /// `withdraw_qty` reservation).
    #[test]
    fn idle_agent_does_not_leak_reservations() {
        let mut sim = TestSim::new(4);
        sim.flat_world(1, 0, TileKind::Grass);
        let person = sim.spawn_person(sim.player_faction_id, (4, 4), |b| {
            b.hunger(0.0); // remove every survive trigger
        });

        sim.tick_n(200);

        let ai = person_ai(&sim.app, person);
        assert!(
            ai.reserved_qty == 0 && ai.reserved_good.is_none(),
            "idle agent leaked a storage reservation: good={:?}, qty={}",
            ai.reserved_good,
            ai.reserved_qty
        );
        assert!(
            ai.task_id == PersonAI::UNEMPLOYED || ai.task_id != TaskKind::WithdrawMaterial as u16,
            "idle agent ended up holding a WithdrawMaterial task with no reservation"
        );
    }

    /// Phase 3b-i: `withdraw_good_task_system` reads its filter from the
    /// typed `Task::WithdrawGood` variant, not the legacy `craft_recipe_id`
    /// channel. Pins that an agent with `Task::WithdrawGood{Specific(Wood)}`
    /// at a storage tile holding Wood pulls one Wood into inventory and
    /// clears the typed task.
    #[test]
    fn withdraw_good_pulls_specific_resource_via_typed_task() {
        use crate::simulation::person::Drafted;
        use crate::simulation::typed_task::{Task, WithdrawGoodFilter};

        let mut sim = TestSim::new(6);
        sim.flat_world(1, 0, TileKind::Grass);
        let storage_tile = (4, 4);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);
        sim.spawn_ground_item(storage_tile, Good::Wood, 3);

        let person = sim.spawn_person(sim.player_faction_id, storage_tile, |b| {
            b.hunger(0.0); // sated, no autonomous goal interference
        });

        // Tick once first so the SpatialIndex syncs the freshly-spawned
        // ground item (ground items only become visible after the first
        // sync_indexed_after_move_system pass). `Drafted` exempts the agent
        // from `goal_dispatch_system`'s "no plan → clear task" reset, which
        // would otherwise wipe our hand-placed Working state before the
        // executor sees it.
        sim.app.world_mut().entity_mut(person).insert(Drafted);
        // Multiple ticks to give the SpatialIndex time to register the
        // freshly-spawned ground item (Added<Indexed> sync passes), then
        // re-establish the typed task right before the firing tick.
        sim.tick_n(5);
        {
            let mut ai = sim.app.world_mut().get_mut::<PersonAI>(person).unwrap();
            ai.task_id = TaskKind::WithdrawGood as u16;
            ai.state = AiState::Working;
            ai.dest_tile = storage_tile;
        }
        {
            let mut aq = sim
                .app
                .world_mut()
                .get_mut::<crate::simulation::typed_task::ActionQueue>(person)
                .unwrap();
            aq.current = Task::WithdrawGood {
                filter: WithdrawGoodFilter::Specific(Good::Wood),
            };
        }
        sim.tick();

        let ai = person_ai(&sim.app, person);
        let task = person_task(&sim.app, person);
        let inv = person_inventory(&sim.app, person);
        let wood_in_hand = inv.get(&Good::Wood).copied().unwrap_or(0);

        assert!(
            wood_in_hand >= 1,
            "expected agent to pull at least 1 Wood from storage; got {} (inventory: {:?})",
            wood_in_hand,
            inv
        );
        assert_eq!(
            task,
            Task::Idle,
            "expected typed task cleared on completion, got {:?}",
            task
        );
        assert_eq!(
            ai.task_id,
            PersonAI::UNEMPLOYED,
            "expected legacy task_id cleared on completion"
        );
    }

    /// Phase 3d-ii: `read_task_system` reads `tech` from the typed
    /// `Task::Read { tech }` variant (replacing the retired `tech_focus`
    /// channel) and accumulates `study_progress` while the agent holds a
    /// matching tablet. Pins the read pipeline end-to-end.
    #[test]
    fn read_task_accumulates_study_progress_via_typed_task() {
        use crate::economy::item::Item;
        use crate::simulation::knowledge::PersonKnowledge;
        use crate::simulation::person::Drafted;
        use crate::simulation::technology::CROP_CULTIVATION;
        use crate::simulation::typed_task::Task;

        let mut sim = TestSim::new(9);
        sim.flat_world(1, 0, TileKind::Grass);
        let person = sim.spawn_person(sim.player_faction_id, (4, 4), |b| {
            b.hunger(0.0);
        });

        // Pick a non-Paleolithic tech: `paleolithic_seed` marks every
        // Paleolithic tech both Aware and Learned, so reading a Paleolithic
        // tablet returns `AlreadyLearned` with zero progress accrual.
        // CROP_CULTIVATION (Neolithic) starts not-aware-not-learned, so
        // study_progress accumulates as expected.
        let tech = CROP_CULTIVATION;

        // Hand-place a tablet encoding `tech` into the agent's inventory.
        // `Item::new_commodity` doesn't set `tech_payload`, so we build
        // the Item literal explicitly.
        {
            let mut econ = sim
                .app
                .world_mut()
                .get_mut::<crate::economy::agent::EconomicAgent>(person)
                .unwrap();
            let tablet = Item {
                resource_id: crate::economy::core_ids::good_to_resource_id(Good::ClayTablet),
                material: None,
                quality: None,
                display_name: None,
                weapon_stats: None,
                armor_stats: None,
                tech_payload: Some(tech),
            };
            econ.add_item(tablet, 1);
        }

        // Drafted bypasses goal_dispatch; tick_n(2) flushes Time accumulator.
        sim.app.world_mut().entity_mut(person).insert(Drafted);
        sim.tick_n(2);
        let progress_before = sim
            .app
            .world()
            .get::<PersonKnowledge>(person)
            .map(|k| k.study_progress.get(&tech).copied().unwrap_or(0))
            .unwrap_or(0);

        // Set up the typed Task::Read; the legacy task_id stays in lockstep.
        {
            let mut ai = sim.app.world_mut().get_mut::<PersonAI>(person).unwrap();
            ai.task_id = TaskKind::Read as u16;
            ai.state = AiState::Working;
            ai.work_progress = 0;
        }
        {
            let mut aq = sim
                .app
                .world_mut()
                .get_mut::<crate::simulation::typed_task::ActionQueue>(person)
                .unwrap();
            aq.current = Task::Read { tech };
        }
        // 30 ticks: under the 60-tick session-done threshold, so the read
        // executor stays Working and just accumulates progress.
        sim.tick_n(30);

        let knowledge = sim.app.world().get::<PersonKnowledge>(person).unwrap();
        let progress_after = knowledge.study_progress.get(&tech).copied().unwrap_or(0);
        let learned = (knowledge.learned >> tech) & 1 != 0;
        assert!(
            progress_after > progress_before || learned,
            "expected study_progress to accumulate (or tech to be learned outright); \
             before={}, after={}, learned={}",
            progress_before,
            progress_after,
            learned
        );

        // Typed task is still Read while the session is active (or Idle if
        // it completed mid-window). Either is consistent with a working
        // executor; just confirm task_id and the typed task agree.
        let ai = person_ai(&sim.app, person);
        let task = person_task(&sim.app, person);
        match task {
            Task::Read { tech: t } => assert_eq!(t, tech, "tech drift mid-task"),
            Task::Idle => assert_eq!(ai.task_id, PersonAI::UNEMPLOYED, "task_id stale after Idle"),
            other => panic!("unexpected task variant after Read: {:?}", other),
        }
    }

    /// Phase 3d-i: `equip_task_system` reads slot + good from the typed
    /// `Task::Equip { slot, good }` variant, not the legacy
    /// `equip_slot`/`craft_recipe_id` channels (the former is now retired,
    /// the latter still serves Craft). Pins the happy path: an agent with
    /// a Spear in inventory and `Task::Equip { MainHand, Spear }` ends the
    /// tick with the spear equipped.
    #[test]
    fn equip_task_moves_inventory_item_into_slot() {
        use crate::simulation::items::{Equipment, EquipmentSlot};
        use crate::simulation::person::Drafted;
        use crate::simulation::typed_task::Task;

        let mut sim = TestSim::new(8);
        sim.flat_world(1, 0, TileKind::Grass);
        let person = sim.spawn_person(sim.player_faction_id, (4, 4), |b| {
            b.hunger(0.0).add_inventory(Good::Weapon, 1);
        });

        sim.app.world_mut().entity_mut(person).insert(Drafted);
        sim.tick_n(2);
        {
            let mut ai = sim.app.world_mut().get_mut::<PersonAI>(person).unwrap();
            ai.task_id = TaskKind::Equip as u16;
            ai.state = AiState::Working;
        }
        {
            let mut aq = sim
                .app
                .world_mut()
                .get_mut::<crate::simulation::typed_task::ActionQueue>(person)
                .unwrap();
            aq.current = Task::Equip {
                slot: EquipmentSlot::MainHand,
                good: Good::Weapon,
            };
        }
        sim.tick();

        let ai = person_ai(&sim.app, person);
        let task = person_task(&sim.app, person);
        let equipment = sim.app.world().get::<Equipment>(person).unwrap();
        assert!(
            equipment.items.contains_key(&EquipmentSlot::MainHand),
            "expected Weapon in MainHand after Equip task"
        );
        assert_eq!(
            task,
            Task::Idle,
            "expected typed task cleared after equip, got {:?}",
            task
        );
        assert_eq!(ai.task_id, PersonAI::UNEMPLOYED);
    }

    /// Phase 3b-ii: `withdraw_material_task_system` reads `(good, qty)` from
    /// the typed `Task::WithdrawMaterial` variant. Pins that an agent
    /// dispatched with the typed variant pulls Wood into hands/inventory and
    /// clears the typed task on completion. Mirror of the WithdrawGood
    /// regression but exercises the qty path + reservation interaction.
    #[test]
    fn withdraw_material_pulls_via_typed_task() {
        use crate::simulation::person::Drafted;
        use crate::simulation::typed_task::Task;

        let mut sim = TestSim::new(7);
        sim.flat_world(1, 0, TileKind::Grass);
        let storage_tile = (4, 4);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);
        sim.spawn_ground_item(storage_tile, Good::Wood, 3);

        let person = sim.spawn_person(sim.player_faction_id, storage_tile, |b| {
            b.hunger(0.0);
        });

        // Drafted bypasses goal_dispatch_system; tick_n(5) lets the
        // SpatialIndex register the freshly-spawned ground item.
        sim.app.world_mut().entity_mut(person).insert(Drafted);
        sim.tick_n(5);
        {
            let mut ai = sim.app.world_mut().get_mut::<PersonAI>(person).unwrap();
            ai.task_id = TaskKind::WithdrawMaterial as u16;
            ai.state = AiState::Working;
            ai.dest_tile = storage_tile;
        }
        {
            // Phase 3b-iii: the typed `Task::WithdrawMaterial` variant is now
            // the sole intent channel — the legacy `withdraw_good`/
            // `withdraw_qty` fields were retired.
            let mut aq = sim
                .app
                .world_mut()
                .get_mut::<crate::simulation::typed_task::ActionQueue>(person)
                .unwrap();
            aq.current = Task::WithdrawMaterial {
                good: Good::Wood,
                qty: 1,
            };
        }
        sim.tick();

        let ai = person_ai(&sim.app, person);
        let task = person_task(&sim.app, person);
        let inv = person_inventory(&sim.app, person);
        // Wood is Bulk::TwoHand 5kg — fits in either hands or inventory.
        let wood_total = inv.get(&Good::Wood).copied().unwrap_or(0);
        let in_hand = sim
            .app
            .world()
            .get::<crate::simulation::carry::Carrier>(person)
            .map(|c| {
                let l = c.left.map(|s| if s.item.good() == Good::Wood { s.qty } else { 0 }).unwrap_or(0);
                let r = c.right.map(|s| if s.item.good() == Good::Wood { s.qty } else { 0 }).unwrap_or(0);
                l + r
            })
            .unwrap_or(0);

        assert!(
            wood_total + in_hand >= 1,
            "expected agent to pull at least 1 Wood (inv={}, hands={})",
            wood_total,
            in_hand
        );
        assert_eq!(
            task,
            Task::Idle,
            "expected typed task cleared on completion, got {:?}",
            task
        );
        assert_eq!(
            ai.task_id,
            PersonAI::UNEMPLOYED,
            "expected legacy task_id cleared on completion"
        );
    }

    /// Phase 3a: a drafted unit whose typed `Task::WalkTo` resolves at its
    /// current tile gets cleared back to `Task::Idle` and `task_id ==
    /// UNEMPLOYED` on the next `military_task_system` tick. Pins the new
    /// arrival pathway that reads dest/z from the typed variant rather than
    /// the legacy `dest_tile`/`target_z` fields.
    #[test]
    fn military_move_arrival_clears_typed_walk_to() {
        use crate::simulation::person::Drafted;
        use crate::simulation::typed_task::{Task, WalkReason};

        let mut sim = TestSim::new(5);
        sim.flat_world(1, 0, TileKind::Grass);
        let person = sim.spawn_person(sim.player_faction_id, (4, 4), |b| {
            b.hunger(0.0); // sated, no autonomous goal interference
        });

        // Hand-place the drafted unit at (4,4) with a typed WalkTo whose
        // destination is also (4,4) — the unit is "already there." The
        // executor expects state == Working on arrival, mirroring what
        // movement_system would set when the agent steps onto its dest.
        {
            let mut entity_mut = sim.app.world_mut().entity_mut(person);
            entity_mut.insert(Drafted);
        }
        {
            let mut ai = sim.app.world_mut().get_mut::<PersonAI>(person).unwrap();
            ai.task_id = TaskKind::MilitaryMove as u16;
            ai.state = AiState::Working;
            ai.dest_tile = (4, 4);
            ai.target_z = 0;
        }
        {
            let mut aq = sim
                .app
                .world_mut()
                .get_mut::<crate::simulation::typed_task::ActionQueue>(person)
                .unwrap();
            aq.current = Task::WalkTo {
                tile: (4, 4),
                z: 0,
                why: WalkReason::MilitaryMove,
            };
        }

        // Two ticks: first to flush Time accumulation, second to actually
        // run military_task_system. FixedUpdate may not fire on the first
        // call depending on how Time::<Fixed> accumulates.
        sim.tick_n(2);

        let ai = person_ai(&sim.app, person);
        let task = person_task(&sim.app, person);
        assert_eq!(
            task,
            Task::Idle,
            "expected typed task cleared on arrival, got {:?}",
            task
        );
        assert_eq!(
            ai.task_id,
            PersonAI::UNEMPLOYED,
            "expected legacy task_id cleared on arrival"
        );
        assert_eq!(ai.state, AiState::Idle, "expected agent to settle to Idle");
    }

    /// Phase 4b-ii regression: when an executor finishes its current task via
    /// `aq.advance()`, any task that was prefetched into the queue is promoted
    /// into `current` rather than dropped on the floor. This is the consumer
    /// half of the queue wiring — until a method actually pre-decomposes a
    /// chain there's no production producer, so we manually enqueue a follow-up
    /// task and verify the executor exit promotes it.
    #[test]
    fn advance_promotes_queued_task_after_executor_exit() {
        use crate::simulation::person::Drafted;
        use crate::simulation::typed_task::{Task, WalkReason};

        let mut sim = TestSim::new(5);
        sim.flat_world(1, 0, TileKind::Grass);
        let person = sim.spawn_person(sim.player_faction_id, (4, 4), |b| {
            b.hunger(0.0);
        });

        {
            let mut entity_mut = sim.app.world_mut().entity_mut(person);
            entity_mut.insert(Drafted);
        }
        {
            let mut ai = sim.app.world_mut().get_mut::<PersonAI>(person).unwrap();
            ai.task_id = TaskKind::MilitaryMove as u16;
            ai.state = AiState::Working;
            ai.dest_tile = (4, 4);
            ai.target_z = 0;
        }
        // Current = a "we're already at the dest" WalkTo (executor will
        // immediately promote it through advance()). Queue a follow-up
        // Task::Idle marker disguised as a Dig at a sentinel tile so we can
        // observe promotion. (Idle would also work but Dig makes the
        // promotion direction obvious.)
        let queued_follow_up = Task::Dig { tile: (99, 99) };
        {
            let mut aq = sim
                .app
                .world_mut()
                .get_mut::<crate::simulation::typed_task::ActionQueue>(person)
                .unwrap();
            aq.current = Task::WalkTo {
                tile: (4, 4),
                z: 0,
                why: WalkReason::MilitaryMove,
            };
            assert!(
                aq.enqueue(queued_follow_up),
                "fixture invariant: queue should accept the follow-up"
            );
            assert_eq!(aq.queued_len(), 1);
        }

        // Two ticks for FixedUpdate to flush.
        sim.tick_n(2);

        let task = person_task(&sim.app, person);
        assert_eq!(
            task,
            queued_follow_up,
            "expected advance() to promote the queued Dig into current, got {:?}",
            task
        );
        let aq = sim
            .app
            .world()
            .get::<crate::simulation::typed_task::ActionQueue>(person)
            .unwrap();
        assert!(
            aq.queued_is_empty(),
            "queue should be empty after promotion, got len={}",
            aq.queued_len()
        );
    }

    /// Phase 5a-ii regression: when goal flips to Sleep, `htn_dispatch_system`
    /// (which since Phase 5a-ii owns the Sleep dispatch path that used to
    /// live in `goal_dispatch_system`) consults the `MethodRegistry`,
    /// expands `SleepMethod` into a `Task::Sleep { bed }`, and dispatches it
    /// onto `ActionQueue.current`. A solo agent (faction == SOLO, no
    /// `HomeBed`) takes the third dispatch branch ("sleep in place"), so we
    /// expect `Task::Sleep { bed: None }` and `task_id == Sleep`. After the
    /// 5a-ii migration the *only* code path that produces a Sleep task is
    /// the HTN dispatcher, so this test doubles as proof that the registry
    /// is the live source of truth.
    ///
    /// The cleanup path (typed task cleared when goal flips off Sleep) is
    /// already covered by the existing `aq.cancel()` stale-reset machinery
    /// that's exercised by `military_move_arrival_clears_typed_walk_to`; we
    /// don't re-test it here because driving a deterministic Sleep→wake
    /// transition through the goal_update bucket cadence is finicky.
    #[test]
    fn sleep_goal_dispatches_typed_sleep_task() {
        use crate::simulation::faction::SOLO;
        use crate::simulation::typed_task::Task;

        let mut sim = TestSim::new(7);
        sim.flat_world(1, 0, TileKind::Grass);
        // Solo (no faction) → no faction home, no bed claim → "sleep in place"
        // branch fires unconditionally.
        let person = sim.spawn_person(SOLO, (4, 4), |b| {
            // Tired enough to flip the goal to Sleep, but not hungry — Survive
            // would beat Sleep otherwise.
            b.hunger(0.0).needs(Needs {
                hunger: 0.0,
                sleep: 220.0,
                shelter: 0.0,
                safety: 0.0,
                social: 0.0,
                reproduction: 0.0,
                willpower: 200.0,
            });
        });

        // Goal_update_system has a 32-tick cooldown; goal_dispatch fires every
        // tick once the goal is set. 60 ticks is ample for both passes plus
        // bucketing slack.
        sim.tick_n(60);

        let task = person_task(&sim.app, person);
        assert_eq!(
            task,
            Task::Sleep { bed: None },
            "expected typed Sleep task on goal flip, got {:?}",
            task
        );

        // The legacy task_id channel is still populated alongside the typed
        // variant — `htn_dispatch_system` writes `task_id = Sleep` directly
        // for the in-place branch and via `assign_task_with_routing` for the
        // bed/home branches. The dual-write goes away when Sleep gets a
        // proper task executor (Phase 6+).
        let ai = person_ai(&sim.app, person);
        assert_eq!(
            ai.task_id,
            TaskKind::Sleep as u16,
            "htn_dispatch_system should mirror the typed task into task_id",
        );
    }

    /// Phase 5b-ii regression: a hungry agent carrying food has its Eat task
    /// dispatched by `htn_eat_dispatch_system` (driven by the HTN registry's
    /// `EatFromInventoryMethod`), not by `plan_execution_system`. The legacy
    /// `EatFromInventory` plan (PlanId 25) was deleted in this PR, so the
    /// typed `Task::Eat` variant proves the registry-driven dispatch is
    /// authoritative.
    #[test]
    fn eat_goal_dispatches_typed_eat_task() {
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::typed_task::Task;

        let mut sim = TestSim::new(11);
        sim.flat_world(1, 0, TileKind::Grass);
        // Hungry enough to clear EatFromInventoryMethod's 180-trigger; carrying
        // food so the precondition holds. Pre-seed `AgentGoal::Survive` so
        // dispatch fires on the very first ParallelB tick rather than waiting
        // for goal_update_system's 32-tick cooldown — keeps the test below
        // TICKS_EAT (60) so the executor doesn't reset task_id to UNEMPLOYED
        // before we can observe the dispatch.
        let person = sim.spawn_person(sim.player_faction_id, (4, 4), |b| {
            b.hunger(210.0)
                .add_inventory(Good::Fruit, 5)
                .goal(AgentGoal::Survive);
        });

        sim.tick_n(5);

        let task = person_task(&sim.app, person);
        assert_eq!(
            task,
            Task::Eat,
            "expected typed Eat task on Survive+food+hunger, got {:?}",
            task
        );

        let ai = person_ai(&sim.app, person);
        assert_eq!(
            ai.task_id,
            TaskKind::Eat as u16,
            "htn_eat_dispatch_system should mirror the typed task into task_id",
        );
    }

    /// Phase 5b-iii-ii: a hungry agent with no food on hand but with edibles
    /// available at a faction storage tile gets dispatched a typed
    /// `Task::WithdrawFood{tile}` by `htn_acquire_food_dispatch_system` and
    /// the trailing `Task::Eat` rides the prefetch ring. After the executor
    /// finishes the withdraw, `aq.advance()` promotes the queued Eat into
    /// `current` and primes the legacy task_id channel so `eat_task_system`
    /// can pick up without re-entering dispatch.
    ///
    /// Pins the first registry-driven multi-task chain in the runtime: if
    /// either the producer (HTN dispatcher) or the consumer (executor's
    /// `advance` + Eat-priming) regresses, this test fails.
    #[test]
    fn acquire_food_goal_dispatches_withdraw_then_eat_chain() {
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::typed_task::{ActionQueue, Task};
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::faction::StorageTileMap;
        use crate::simulation::needs::EAT_TRIGGER_HUNGER;
        let _ = EAT_TRIGGER_HUNGER;

        let mut sim = TestSim::new(42);
        sim.flat_world(2, 0, TileKind::Grass);
        let storage_tile = (4, 4);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);
        sim.spawn_ground_item(storage_tile, Good::Fruit, 5);

        // Spawn the agent sated and Idle so it does not start dispatching
        // during the warm-up. The warm-up is needed for SpatialIndex sync
        // (Added<Indexed> takes a few ticks), the Economy storage rollup,
        // and the StorageTileMap to populate.
        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |b| {
            b.hunger(0.0);
        });

        sim.tick_n(80);

        // Sanity: the world state should now satisfy the method's
        // precondition. If either of these is false the regression is on
        // the world-init side and any later assertions would be meaningless.
        {
            let registry = sim.app.world().resource::<FactionRegistry>();
            let stm = sim.app.world().resource::<StorageTileMap>();
            assert!(
                registry.food_stock(sim.player_faction_id) > 0.0,
                "storage rollup should report food stock > 0 after warm-up"
            );
            assert!(
                stm.nearest_for_faction(sim.player_faction_id, (0, 0)).is_some(),
                "StorageTileMap should know about the spawned storage tile"
            );
        }

        // Now arm the agent: spike hunger past EAT_TRIGGER_HUNGER (180) and
        // pin AgentGoal::Survive so `htn_acquire_food_dispatch_system` fires
        // on the very next ParallelB tick rather than waiting for
        // `goal_update_system`'s 32-tick cadence.
        {
            let mut entity = sim.app.world_mut().entity_mut(person);
            let mut needs = entity
                .get_mut::<crate::simulation::needs::Needs>()
                .unwrap();
            needs.hunger = 220.0;
        }
        {
            let mut entity = sim.app.world_mut().entity_mut(person);
            let mut goal = entity.get_mut::<AgentGoal>().unwrap();
            *goal = AgentGoal::Survive;
        }

        // One ParallelB tick is enough for `htn_acquire_food_dispatch_system`
        // to argmax the registry, route the agent, and dispatch
        // `Task::WithdrawFood`. We tick 2 to leave headroom for the
        // dispatcher to actually run after the goal mutation lands. The
        // agent is several tiles from the storage tile so movement won't
        // complete the WithdrawFood within this window.
        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");

        // The head must be a WithdrawFood pointed at the storage tile —
        // the executor consumes that, then advance() promotes Eat.
        match aq.current {
            Task::WithdrawFood { tile } => {
                assert_eq!(
                    tile, storage_tile,
                    "WithdrawFood should target the spawned storage tile"
                );
            }
            other => panic!(
                "expected Task::WithdrawFood as head of AcquireFood chain, got {:?}",
                other
            ),
        }

        // The Eat must be queued behind it — proving the dispatcher pushed
        // the second task in the expansion onto the prefetch ring rather
        // than dropping it.
        assert_eq!(
            aq.queued_len(),
            1,
            "expected exactly one queued task (Eat) behind WithdrawFood"
        );
        assert_eq!(
            aq.peek_next(),
            Some(Task::Eat),
            "expected Task::Eat queued behind WithdrawFood"
        );
    }

    /// Phase 5c-ii-d-iii-ii: a hungry Survive-goal agent with empty hands and
    /// no faction storage stock but a visible loose food `GroundItem` within
    /// `VIEW_RADIUS=15` gets dispatched the typed `Task::Scavenge { target }`
    /// chain by `htn_acquire_food_dispatch_system`'s scavenge branch. The
    /// trailing `Task::Eat` rides the prefetch ring; after the executor
    /// finishes the scavenge, `finish_scavenge`'s `Task::Eat` arm primes the
    /// legacy Eat channel directly so `eat_task_system` picks up next tick
    /// without re-entering dispatch.
    ///
    /// Pins the second AcquireFood method (`ScavengeFoodFromGroundMethod`,
    /// utility 1.5) outranking the bare-withdraw path (utility 1.0) when both
    /// are applicable. Also serves as the regression for the legacy
    /// `ScavengeFood` plan's `serves_goals` retarget — the plan no longer
    /// fires under Survive (HTN owns that case); only GatherFood goal still
    /// uses the legacy `[CollectFood, DepositGoods]` chain.
    #[test]
    fn acquire_food_scavenge_dispatches_scavenge_then_eat_chain() {
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(91);
        sim.flat_world(2, 0, TileKind::Grass);

        // Deliberately no storage tile and no faction food stock — the only
        // applicable AcquireFood method is the scavenge branch, so the
        // argmax is unambiguous (1.5 from ScavengeFood vs 0 applicable
        // others). Ground item is within VIEW_RADIUS=15 of (0,0).
        let scavenge_tile = (5, 0);
        let ground_item = sim.spawn_ground_item(scavenge_tile, Good::Fruit, 3);

        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |b| {
            b.hunger(0.0);
        });

        // Warm-up so the Added<Indexed> hook registers the GroundItem in
        // SpatialIndex. The 5c-ii-d-ii-a Wood test uses 10 ticks for the
        // same reason; reuse that budget.
        sim.tick_n(10);

        // Spike hunger past EAT_TRIGGER_HUNGER (180) and pin Survive so the
        // dispatcher fires on the next ParallelB tick rather than waiting
        // for `goal_update_system`'s 32-tick cadence.
        {
            let mut entity = sim.app.world_mut().entity_mut(person);
            let mut needs = entity
                .get_mut::<crate::simulation::needs::Needs>()
                .unwrap();
            needs.hunger = 220.0;
        }
        {
            let mut entity = sim.app.world_mut().entity_mut(person);
            let mut goal = entity.get_mut::<AgentGoal>().unwrap();
            *goal = AgentGoal::Survive;
        }

        // Two ticks: one for the goal mutation to land in the dispatcher's
        // query, one for the dispatch itself. The scavenge tile is 5 tiles
        // away; movement won't complete the Scavenge within this window.
        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");

        match aq.current {
            Task::Scavenge { target } => {
                assert_eq!(
                    target, ground_item,
                    "head target should match the spawned Fruit GroundItem"
                );
            }
            other => panic!(
                "expected Task::Scavenge as head of AcquireFood scavenge chain, got {:?}",
                other
            ),
        }
        assert_eq!(
            aq.queued_len(),
            1,
            "expected exactly one queued task (Eat) behind Scavenge"
        );
        assert_eq!(
            aq.peek_next(),
            Some(Task::Eat),
            "expected Task::Eat queued behind Scavenge"
        );
    }

    /// Phase 5c-ii-b: a Haul-claimed agent with no material on hand but with
    /// the named good available at a faction storage tile and a live
    /// `ClaimTarget` (blueprint + good) gets dispatched a typed
    /// `Task::WithdrawMaterial { good, qty: 1 }` by
    /// `htn_acquire_good_dispatch_system` and the trailing
    /// `Task::HaulToBlueprint { blueprint }` rides the prefetch ring. After
    /// the executor finishes the withdraw, `finish_withdraw_material`
    /// promotes the queued HaulToBlueprint into `current` and routes the
    /// agent onto `TaskKind::HaulMaterials` toward the blueprint, where
    /// `construction_system`'s hauler branch takes over.
    ///
    /// Pins the second registry-driven multi-task chain in the runtime
    /// (after `AcquireFood → [WithdrawFood, Eat]`) and the first whose
    /// trailing leg requires its own routing decision. Replaces the legacy
    /// `ClaimedHaul` plan (PlanId 33).
    #[test]
    fn acquire_good_haul_goal_dispatches_withdraw_then_haul_chain() {
        use crate::simulation::construction::{Blueprint, BuildSiteKind, WallMaterial};
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::jobs::{
            ClaimTarget, JobBoard, JobClaim, JobKind, JobPosting, JobProgress, JobSource,
        };
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(7);
        sim.flat_world(2, 0, TileKind::Grass);
        let storage_tile = (4, 4);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);
        sim.spawn_ground_item(storage_tile, Good::Wood, 5);

        // Spawn the construction target somewhere reachable but distinct
        // from the storage tile. The blueprint isn't satisfied (no deposits
        // yet), so the haul method's downstream `bp.is_satisfied()` check
        // still permits the route.
        let blueprint_tile = (10, 10);
        let blueprint_world = tile_to_world(blueprint_tile.0, blueprint_tile.1);
        let blueprint = sim
            .app
            .world_mut()
            .spawn((
                Blueprint::new(
                    sim.player_faction_id,
                    None,
                    BuildSiteKind::Wall(WallMaterial::Palisade),
                    blueprint_tile,
                    0,
                ),
                Transform::from_xyz(blueprint_world.x, blueprint_world.y, 0.5),
                GlobalTransform::default(),
                Visibility::Hidden,
                InheritedVisibility::default(),
            ))
            .id();

        // Spawn the agent with empty hands so the WithdrawMaterial path is
        // the only viable expansion. Default needs are non-crisis.
        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});

        // Warm-up ticks: storage rollup must populate
        // `FactionData.storage.totals[Wood] > 0` and `StorageTileMap` must
        // know the storage tile before `htn_acquire_good_dispatch_system`'s
        // tile scan can find it. `Added<Indexed>` for the GroundItem also
        // needs a few FixedUpdate frames.
        sim.tick_n(80);

        {
            let registry = sim.app.world().resource::<FactionRegistry>();
            let stock = registry
                .factions
                .get(&sim.player_faction_id)
                .map(|f| f.storage.stock_of(Good::Wood))
                .unwrap_or(0);
            assert!(
                stock > 0,
                "faction storage rollup should report Wood stock > 0 after warm-up; got {}",
                stock
            );
        }

        // Post a real `JobPosting` of `JobKind::Haul` onto the `JobBoard` so
        // `job_goal_lock_system` (Economy) refreshes `ClaimTarget` from the
        // posting's `JobProgress::Haul { blueprint, good, .. }` rather than
        // overriding it with `ClaimTarget::default()`. Without a posting, the
        // claim's `ClaimTarget` companion gets zeroed every Economy tick and
        // the dispatcher sees `target.good == None` / `target.blueprint == None`.
        let job_id = {
            let mut board = sim.app.world_mut().resource_mut::<JobBoard>();
            let id = board.alloc_id();
            let posting = JobPosting {
                id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Haul,
                progress: JobProgress::Haul {
                    blueprint,
                    good: Good::Wood,
                    delivered: 0,
                    target: 2,
                },
                claimants: vec![person],
                priority: 100,
                source: JobSource::Chief,
                posted_tick: 0,
                expiry_tick: None,
            };
            board
                .postings
                .entry(sim.player_faction_id)
                .or_default()
                .push(posting);
            id
        };

        // Inject the Haul claim + companion ClaimTarget so the dispatcher
        // sees a hauler with both `good` and `blueprint` populated.
        {
            let mut entity = sim.app.world_mut().entity_mut(person);
            entity.insert(JobClaim {
                job_id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Haul,
                posted_tick: 0,
                fail_count: 0,
            });
            entity.insert(ClaimTarget {
                blueprint: Some(blueprint),
                good: Some(Good::Wood),
            });
            let mut goal = entity.get_mut::<AgentGoal>().unwrap();
            *goal = AgentGoal::Haul;
        }

        // One ParallelB tick is enough for `htn_acquire_good_dispatch_system`
        // to argmax the registry, route the agent, and dispatch
        // `Task::WithdrawMaterial`. Tick 2 to leave headroom for the goal
        // mutation to land.
        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");

        match aq.current {
            Task::WithdrawMaterial { good, qty } => {
                assert_eq!(good, Good::Wood, "head good should match ClaimTarget");
                assert_eq!(qty, 1, "5c-ii-b uses the qty:1 unit-acquisition contract");
            }
            other => panic!(
                "expected Task::WithdrawMaterial as head of AcquireGood haul chain, got {:?}",
                other
            ),
        }

        assert_eq!(
            aq.queued_len(),
            1,
            "expected exactly one queued task (HaulToBlueprint) behind WithdrawMaterial"
        );
        match aq.peek_next() {
            Some(Task::HaulToBlueprint { blueprint: bp }) => {
                assert_eq!(
                    bp, blueprint,
                    "queued HaulToBlueprint should target the claimed blueprint entity"
                );
            }
            other => panic!(
                "expected Task::HaulToBlueprint queued behind WithdrawMaterial, got {:?}",
                other
            ),
        }
    }

    /// Phase 5c-ii-c-ii: the gather → deposit chain is now produced by the
    /// HTN registry under `AgentGoal::GatherWood`, replacing the legacy
    /// `GatherWood` plan (PlanId 2, `[Gather, DepositGoods]`). Pins the
    /// third multi-task chain in the runtime — the dispatcher routes the head
    /// `Task::Gather { tile }` and `aq.enqueue`s the trailing
    /// `Task::DepositToFactionStorage { good: Wood }` onto the prefetch ring.
    #[test]
    fn gather_wood_goal_dispatches_gather_then_deposit_chain() {
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::jobs::{
            ClaimTarget, JobBoard, JobClaim, JobKind, JobPosting, JobProgress, JobSource,
        };
        use crate::simulation::memory::{AgentMemory, MemoryKind};
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(11);
        sim.flat_world(2, 0, TileKind::Grass);
        let storage_tile = (4, 4);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);

        // Spawn the chief first (first member of a faction is auto-promoted
        // by `update_chief_assignment_system`); they get
        // `AgentGoal::Lead` which the dispatcher rejects. The second agent
        // is the regular worker the test exercises.
        let _chief = sim.spawn_person(sim.player_faction_id, (1, 1), |_| {});
        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});

        // Warm-up ticks for SpatialIndex / storage rollup. Less than the
        // haul test's 80 because we're not depending on `material_targets`
        // — the JobPosting + JobClaim hack below pins the goal to
        // GatherWood directly via `job_goal_lock_system`.
        sim.tick_n(10);

        // Inject a Wood memory entry. The tile must lie outside the
        // 15-tile `VIEW_RADIUS` from the agent's spawn at (0,0) — otherwise
        // `vision_system` iterates that tile, sees no plant there, and
        // forgets the entry on the next tick.
        let memory_tile = (20, 0);

        // Post a `JobKind::Stockpile` for Wood + claim it on the agent.
        // This locks the goal to `GatherWood` via two complementary paths:
        //   - `goal_update_system` skips agents with a JobClaim
        //     (preventing the goal from flipping based on need state),
        //   - `job_goal_lock_system` (Economy) sets `*goal = posting_goal(p)`
        //     which maps `Stockpile + Wood` → `AgentGoal::GatherWood`.
        // This mirrors `acquire_good_haul_goal_dispatches...` precisely —
        // the same posting/claim hack but `Stockpile{Wood}` instead of
        // `Haul{...}`.
        let job_id = {
            let mut board = sim.app.world_mut().resource_mut::<JobBoard>();
            let id = board.alloc_id();
            let posting = JobPosting {
                id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Stockpile,
                progress: JobProgress::Stockpile {
                    good: Good::Wood,
                    deposited: 0,
                    target: 8,
                },
                claimants: vec![person],
                priority: 100,
                source: JobSource::Chief,
                posted_tick: 0,
                expiry_tick: None,
            };
            board
                .postings
                .entry(sim.player_faction_id)
                .or_default()
                .push(posting);
            id
        };
        {
            let mut entity = sim.app.world_mut().entity_mut(person);
            if let Some(mut mem) = entity.get_mut::<AgentMemory>() {
                mem.record(memory_tile, MemoryKind::wood());
            } else {
                panic!("Person should have AgentMemory");
            }
            entity.insert(JobClaim {
                job_id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Stockpile,
                posted_tick: 0,
                fail_count: 0,
            });
            entity.insert(ClaimTarget::default());
            let mut goal = entity.get_mut::<AgentGoal>().unwrap();
            *goal = AgentGoal::GatherWood;
        }

        // Two ticks: ParallelA's `goal_update_system` skips (JobClaim
        // present), Economy's `job_goal_lock_system` confirms goal as
        // GatherWood, ParallelB's `htn_acquire_good_dispatch_system`
        // argmaxes the registry, routes the agent, and dispatches
        // `Task::Gather`.
        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");

        match aq.current {
            Task::Gather { tile } => {
                assert_eq!(
                    tile, memory_tile,
                    "head tile should match the injected `MemoryKind::wood()` entry"
                );
            }
            other => panic!(
                "expected Task::Gather as head of AcquireGood gather chain, got {:?}",
                other
            ),
        }

        assert_eq!(
            aq.queued_len(),
            1,
            "expected exactly one queued task (DepositToFactionStorage) behind Gather"
        );
        match aq.peek_next() {
            Some(Task::DepositToFactionStorage { good }) => {
                assert_eq!(
                    good,
                    Good::Wood,
                    "queued deposit good should match GatherWood goal"
                );
            }
            other => panic!(
                "expected Task::DepositToFactionStorage queued behind Gather, got {:?}",
                other
            ),
        }
    }

    /// Phase 5c-ii-d-ii-a: when a `GatherWood`-goal agent has a visible loose
    /// `Wood` `GroundItem` within `VIEW_RADIUS=15`, the scavenge chain
    /// (`[Task::Scavenge { target }, Task::DepositToFactionStorage { Wood }]`)
    /// is preferred over the gather chain because
    /// `ScavengeFromGroundMethod`'s utility (1.5) outranks
    /// `GatherFromKnownMethod`'s (1.0). Mirrors the
    /// `gather_wood_goal_dispatches_gather_then_deposit_chain` pattern but
    /// with a real `GroundItem` instead of a memory entry — and *no* memory
    /// entry, so the gather method's precondition fails and only the
    /// scavenge method is applicable.
    #[test]
    fn scavenge_wood_goal_dispatches_scavenge_then_deposit_chain() {
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::jobs::{
            ClaimTarget, JobBoard, JobClaim, JobKind, JobPosting, JobProgress, JobSource,
        };
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(13);
        sim.flat_world(2, 0, TileKind::Grass);
        let storage_tile = (4, 4);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);

        let _chief = sim.spawn_person(sim.player_faction_id, (1, 1), |_| {});
        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});

        // Spawn a loose Wood GroundItem within VIEW_RADIUS=15 of the worker
        // at (0,0). Avoid the storage tile (4,4) — the dispatcher excludes
        // storage tiles from the scavenge scan, mirroring the legacy
        // `StepTarget::NearestItem` resolver.
        let scavenge_tile = (5, 0);
        let ground_item = sim.spawn_ground_item(scavenge_tile, Good::Wood, 3);

        // Warm-up ticks: SpatialIndex picks up the new GroundItem (Added<Indexed>
        // hooks need at least 2-3 FixedUpdate frames to register), storage
        // rollup runs, and `update_chief_assignment_system` settles.
        sim.tick_n(10);

        let job_id = {
            let mut board = sim.app.world_mut().resource_mut::<JobBoard>();
            let id = board.alloc_id();
            let posting = JobPosting {
                id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Stockpile,
                progress: JobProgress::Stockpile {
                    good: Good::Wood,
                    deposited: 0,
                    target: 8,
                },
                claimants: vec![person],
                priority: 100,
                source: JobSource::Chief,
                posted_tick: 0,
                expiry_tick: None,
            };
            board
                .postings
                .entry(sim.player_faction_id)
                .or_default()
                .push(posting);
            id
        };
        {
            let mut entity = sim.app.world_mut().entity_mut(person);
            entity.insert(JobClaim {
                job_id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Stockpile,
                posted_tick: 0,
                fail_count: 0,
            });
            entity.insert(ClaimTarget::default());
            let mut goal = entity.get_mut::<AgentGoal>().unwrap();
            *goal = AgentGoal::GatherWood;
        }

        // Two ticks: ParallelA's `goal_update_system` skips (JobClaim
        // present), Economy's `job_goal_lock_system` keeps the goal at
        // GatherWood, and ParallelB's `htn_acquire_good_dispatch_system`
        // scans SpatialIndex, finds the Wood GroundItem, builds a ctx with
        // `scavenge_target_*` populated, argmaxes the registry (scavenge 1.5
        // > gather 1.0), routes the agent, and dispatches the chain.
        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");

        match aq.current {
            Task::Scavenge { target } => {
                assert_eq!(
                    target, ground_item,
                    "head target should match the spawned Wood GroundItem"
                );
            }
            other => panic!(
                "expected Task::Scavenge as head of AcquireGood scavenge chain, got {:?}",
                other
            ),
        }

        assert_eq!(
            aq.queued_len(),
            1,
            "expected exactly one queued task (DepositToFactionStorage) behind Scavenge"
        );
        match aq.peek_next() {
            Some(Task::DepositToFactionStorage { good }) => {
                assert_eq!(
                    good,
                    Good::Wood,
                    "queued deposit good should match GatherWood goal"
                );
            }
            other => panic!(
                "expected Task::DepositToFactionStorage queued behind Scavenge, got {:?}",
                other
            ),
        }
    }

    /// Phase 5c-ii-d-iv-ii: when a `GatherWood`-goal agent has *no* memory and
    /// *no* visible loose Wood within `VIEW_RADIUS`, `ExploreForMaterialMethod`
    /// (utility 0.3) is the only applicable method — the dispatcher wins by
    /// fallback ranking and dispatches `Task::Explore { kind: MemoryKind::wood() }`.
    /// Mirrors the gather/scavenge dispatch tests but with neither memory nor
    /// vision populated, so only the Explore method's precondition fires.
    #[test]
    fn gather_wood_goal_with_no_targets_dispatches_explore() {
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::jobs::{
            ClaimTarget, JobBoard, JobClaim, JobKind, JobPosting, JobProgress, JobSource,
        };
        use crate::simulation::memory::MemoryKind;
        use crate::simulation::tasks::TaskKind;
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(13);
        sim.flat_world(2, 0, TileKind::Grass);

        // No memory, no GroundItem, no storage tile — the only applicable
        // method should be `ExploreForMaterialMethod`.
        let _chief = sim.spawn_person(sim.player_faction_id, (1, 1), |_| {});
        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});

        sim.tick_n(10);

        // Pin the goal to GatherWood via the JobPosting + JobClaim hack
        // (mirrors `gather_wood_goal_dispatches_gather_then_deposit_chain`).
        let job_id = {
            let mut board = sim.app.world_mut().resource_mut::<JobBoard>();
            let id = board.alloc_id();
            let posting = JobPosting {
                id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Stockpile,
                progress: JobProgress::Stockpile {
                    good: Good::Wood,
                    deposited: 0,
                    target: 8,
                },
                claimants: vec![person],
                priority: 100,
                source: JobSource::Chief,
                posted_tick: 0,
                expiry_tick: None,
            };
            board
                .postings
                .entry(sim.player_faction_id)
                .or_default()
                .push(posting);
            id
        };
        {
            let mut entity = sim.app.world_mut().entity_mut(person);
            entity.insert(JobClaim {
                job_id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Stockpile,
                posted_tick: 0,
                fail_count: 0,
            });
            entity.insert(ClaimTarget::default());
            let mut goal = entity.get_mut::<AgentGoal>().unwrap();
            *goal = AgentGoal::GatherWood;
        }

        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");
        match aq.current {
            Task::Explore { kind } => {
                assert_eq!(
                    kind,
                    MemoryKind::wood(),
                    "Explore kind should match GatherWood goal's MemoryKind"
                );
            }
            other => panic!(
                "expected Task::Explore as fallback head when no targets known, got {:?}",
                other
            ),
        }
        // Single-task expansion — no trailing tasks on the prefetch ring.
        assert_eq!(
            aq.queued_len(),
            0,
            "ExploreForMaterialMethod expansion is single-task; queue should be empty"
        );

        // Legacy task channel should also reflect Explore.
        let person_ai = sim
            .app
            .world()
            .get::<crate::simulation::person::PersonAI>(person)
            .expect("PersonAI missing");
        assert_eq!(
            person_ai.task_id,
            TaskKind::Explore as u16,
            "legacy task_id should mirror the typed Explore variant"
        );
    }

    /// Phase 5c-ii-a → 5c-ii-d-i: `htn_acquire_good_dispatch_system` is wired
    /// into ParallelB after `htn_acquire_food_dispatch_system`, and the
    /// `MethodRegistry` resource is reachable from the running app. After
    /// 5c-ii-d-i the registry has four AcquireGood methods:
    /// `WithdrawMaterialFromStorageMethod` (single-task bare withdraw),
    /// `WithdrawAndHaulToBlueprintMethod` (two-task chain for hauler claims),
    /// `GatherFromKnownMethod` (two-task chain for known harvest tiles), and
    /// `ScavengeFromGroundMethod` (two-task chain for known loose ground
    /// items — wired in 5c-ii-d-ii-a for Wood/Stone, plans 38/39 deleted in
    /// 5c-ii-d-ii-b; ScavengeFood (PlanId 6) deferred to 5c-ii-d-iii).
    #[test]
    fn acquire_good_method_registered_in_simulation_plugin() {
        use crate::simulation::htn::{AbstractTaskKind, MethodRegistry};

        let mut sim = TestSim::new(0);
        // No world / agents needed — we only inspect the resource set built
        // by `SimulationPlugin::build`. One tick keeps the schedule honest:
        // if the new dispatch system fails to add (e.g. signature mismatch
        // against the ParallelB set) the schedule build panics here.
        sim.tick();

        let registry = sim
            .app
            .world()
            .get_resource::<MethodRegistry>()
            .expect("MethodRegistry resource should be inserted by SimulationPlugin");
        assert_eq!(
            registry.method_count(AbstractTaskKind::AcquireGood),
            5,
            "register_builtin_methods should register \
             WithdrawMaterialFromStorageMethod, WithdrawAndHaulToBlueprintMethod, \
             GatherFromKnownMethod, ScavengeFromGroundMethod, and \
             ExploreForMaterialMethod under AcquireGood at 5c-ii-d-iv-i"
        );
        assert_eq!(
            registry.method_count(AbstractTaskKind::StockpileFood),
            3,
            "register_builtin_methods should register \
             ScavengeFoodForStorageMethod, ForageFromKnownForStorageMethod, \
             and ExploreForFoodForStorageMethod under StockpileFood"
        );
        assert_eq!(
            registry.method_count(AbstractTaskKind::AcquireFood),
            4,
            "register_builtin_methods should register \
             WithdrawFromStorageMethod, ScavengeFoodFromGroundMethod, \
             ForageFromKnownMethod, and ExploreForFoodMethod under AcquireFood"
        );
    }

    /// Phase 5c-ii-d-vi: HTN-driven StockpileFood chain dispatch under
    /// `AgentGoal::GatherFood`. Replaces the legacy `ScavengeFood` plan
    /// (PlanId 6, GatherFood case). Mirrors the
    /// `acquire_food_scavenge_dispatches_scavenge_then_eat_chain` pattern
    /// (5c-ii-d-iii-ii) but for the chief-driven storage-fill goal: agent
    /// not hungry, goal pinned to GatherFood, fruit on the ground →
    /// `htn_stockpile_food_dispatch_system` dispatches
    /// `Task::Scavenge { target }` with `Task::DepositToFactionStorage { Fruit }`
    /// queued behind it.
    ///
    /// Pins the goal across `goal_update_system` ticks via a
    /// `JobClaim::Stockpile` + `JobPosting{Stockpile, Fruit}` hack — same
    /// pattern as `gather_wood_goal_dispatches_gather_then_deposit_chain`
    /// (5c-ii-c-ii). Without the JobClaim, `goal_update_system` re-evaluates
    /// idle agents every tick and would flip the goal away from GatherFood.
    #[test]
    fn gather_food_goal_dispatches_scavenge_then_deposit_chain() {
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::jobs::{
            ClaimTarget, JobBoard, JobClaim, JobKind, JobPosting, JobProgress, JobSource,
        };
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(13);
        sim.flat_world(2, 0, TileKind::Grass);

        // Storage tile far enough away that VIEW_RADIUS=15 still excludes it
        // from the scavenge scan (`storage_tile_map.tiles.contains_key` filter
        // would skip it anyway, but distance keeps the test intent legible).
        let storage_tile = (-10, 0);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);

        // Spawn a chief so the auto-promoted FactionChief doesn't pin our
        // agent's goal to Lead. Mirrors the gather/scavenge fixture pattern.
        let _chief = sim.spawn_person(sim.player_faction_id, (-9, 0), |_| {});

        // Spawn a Fruit GroundItem at (5, 0) — within VIEW_RADIUS=15 of the
        // worker at (0, 0) and outside the storage tile filter.
        let fruit_entity = sim.spawn_ground_item((5, 0), Good::Fruit, 3);

        let worker = sim.spawn_person(sim.player_faction_id, (0, 0), |b| {
            b.hunger(0.0);
        });

        // Warmup so SpatialIndex picks up the GroundItem.
        sim.tick_n(5);

        // Inject a Stockpile/Fruit posting + JobClaim so `posting_goal(p)`
        // (`jobs.rs:1264`) maps Stockpile + Fruit → GatherFood and
        // `job_goal_lock_system` re-pins the goal every Economy tick. This
        // also makes `goal_update_system` skip the agent (line 237 — JobClaim
        // present), preventing goal churn.
        let job_id = {
            let mut board = sim.app.world_mut().resource_mut::<JobBoard>();
            let id = board.alloc_id();
            let posting = JobPosting {
                id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Stockpile,
                progress: JobProgress::Stockpile {
                    good: Good::Fruit,
                    deposited: 0,
                    target: 5,
                },
                claimants: vec![worker],
                priority: 100,
                source: JobSource::Chief,
                posted_tick: 0,
                expiry_tick: None,
            };
            board
                .postings
                .entry(sim.player_faction_id)
                .or_default()
                .push(posting);
            id
        };

        sim.app.world_mut().entity_mut(worker).insert((
            JobClaim {
                job_id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Stockpile,
                posted_tick: 0,
                fail_count: 0,
            },
            ClaimTarget {
                good: Some(Good::Fruit),
                blueprint: None,
            },
            AgentGoal::GatherFood,
        ));

        // Two ticks: ParallelA → ParallelB → dispatcher fires.
        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(worker)
            .expect("worker should have ActionQueue");

        assert_eq!(
            aq.current,
            Task::Scavenge {
                target: fruit_entity
            },
            "htn_stockpile_food_dispatch_system should dispatch Scavenge \
             toward the visible Fruit GroundItem under GatherFood; \
             current = {:?}",
            aq.current,
        );
        assert_eq!(
            aq.peek_next(),
            Some(Task::DepositToFactionStorage { good: Good::Fruit }),
            "the trailing DepositToFactionStorage{{Fruit}} should be queued \
             behind the Scavenge head"
        );
    }

    /// Phase 6b-ii: when an HTN-dispatched chain drains naturally to
    /// `Task::Idle`, `htn_method_completion_system` records
    /// `MethodOutcome::Success` against `MethodHistory` and clears
    /// `PersonAI.active_method`. Pinned via the eat-from-inventory chain:
    /// `htn_eat_dispatch_system` stamps `active_method = EAT_FROM_INVENTORY`,
    /// `eat_task_system` runs to completion and `aq.advance()`s the typed
    /// channel to Idle, and the next Economy phase records Success.
    #[test]
    fn htn_chain_completion_records_method_success() {
        use crate::simulation::htn::{MethodHistory, MethodId, MethodOutcome};

        let mut sim = TestSim::new(42);
        sim.flat_world(1, 0, TileKind::Grass);
        let person = sim.spawn_person(sim.player_faction_id, (4, 4), |b| {
            b.hunger(210.0).add_inventory(Good::Fruit, 10);
        });

        // Eat task takes TICKS_EAT (~60) ticks of Working state. 400 ticks
        // is enough for at least one full Eat chain to dispatch, run, and
        // be recorded by `htn_method_completion_system` (which runs in
        // Economy after `drop_items_at_destination_system`).
        sim.tick_n(400);

        let ai = person_ai(&sim.app, person);
        let history = sim
            .app
            .world()
            .get::<MethodHistory>(person)
            .expect("person should have MethodHistory");
        let now = sim.app.world().resource::<SimClock>().tick;

        // active_method clears once the chain drains and the completion
        // system records the outcome.
        assert!(
            ai.active_method.is_none(),
            "expected PersonAI.active_method to be None after chain drained, got {:?}",
            ai.active_method
        );

        // MethodHistory contains a Success entry for EAT_FROM_INVENTORY.
        // We don't gate on TTL — the test runs 400 ticks but each Eat chain
        // is short, so the buffer may have rotated past the TTL window. The
        // key fact is "Success was recorded at all," which fails today only
        // when the dispatch + drain pipeline doesn't stamp + clear the
        // outcome. (`recently_failed_count` only counts failures, so we
        // walk the ring directly.)
        let _ = now; // ticks are visible in the asserted entries below
        let has_eat_success = history.entries.iter().any(|slot| {
            matches!(
                slot,
                Some((id, outcome, _tick))
                    if *id == MethodId::EAT_FROM_INVENTORY
                        && *outcome == MethodOutcome::Success
            )
        });
        assert!(
            has_eat_success,
            "expected MethodHistory to carry Success(EAT_FROM_INVENTORY); \
             entries = {:?}",
            history.entries
        );
    }
}
