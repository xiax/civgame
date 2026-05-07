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
    pub fn spawn_ground_item(
        &mut self,
        tile: (i32, i32),
        resource: impl Into<crate::economy::resource_catalog::ResourceId>,
        qty: u32,
    ) -> Entity {
        use crate::simulation::items::GroundItem;
        let world_pos = tile_to_world(tile.0, tile.1);
        self.app
            .world_mut()
            .spawn((
                GroundItem {
                    item: Item::new_commodity(resource.into()),
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
    inventory: Vec<(crate::economy::resource_catalog::ResourceId, u32)>,
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

    pub fn add_inventory(
        &mut self,
        resource: impl Into<crate::economy::resource_catalog::ResourceId>,
        qty: u32,
    ) -> &mut Self {
        self.inventory.push((resource.into(), qty));
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
        for (rid, qty) in &self.inventory {
            economic.add_item(Item::new_commodity(*rid), *qty);
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
                    MethodHistory::default(),
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

/// Quick accessor for an agent's EconomicAgent inventory totals keyed by
/// `ResourceId` (returns a clone).
pub fn person_inventory(
    app: &App,
    entity: Entity,
) -> AHashMap<crate::economy::resource_catalog::ResourceId, u32> {
    let econ = app
        .world()
        .get::<EconomicAgent>(entity)
        .expect("EconomicAgent missing");
    let mut out = AHashMap::new();
    for (item, qty) in econ.inventory.iter() {
        if *qty > 0 {
            *out.entry(item.resource_id).or_insert(0) += *qty;
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
            b.hunger(210.0).add_inventory(crate::economy::core_ids::fruit(), 10);
        });

        let initial_food = person_inventory(&sim.app, person)
            .get(&crate::economy::core_ids::fruit())
            .copied()
            .unwrap_or(0);
        assert_eq!(initial_food, 10);

        // Eat task takes TICKS_EAT (~60) ticks of Working state to fire,
        // and goal_update_system has a 32-tick cooldown. 400 ticks is
        // ample headroom.
        sim.tick_n(400);

        let final_food = person_inventory(&sim.app, person)
            .get(&crate::economy::core_ids::fruit())
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
        sim.spawn_ground_item(storage_tile, crate::economy::core_ids::wood(), 5);

        // Storage rollup runs in Economy each tick but spatial-index
        // sync needs a Transform-changed pass first. ~80 ticks is
        // overkill but cheap.
        sim.tick_n(80);

        let registry = sim.app.world().resource::<FactionRegistry>();
        let faction = registry
            .factions
            .get(&sim.player_faction_id)
            .expect("player faction missing");
        let wood_total = faction.storage.stock_of(crate::economy::core_ids::wood());
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
            ai.reserved_qty == 0 && ai.reserved_resource.is_none(),
            "idle agent leaked a storage reservation: resource={:?}, qty={}",
            ai.reserved_resource,
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
        sim.spawn_ground_item(storage_tile, crate::economy::core_ids::wood(), 3);

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
                filter: WithdrawGoodFilter::Specific(crate::economy::core_ids::wood()),
            };
        }
        sim.tick();

        let ai = person_ai(&sim.app, person);
        let task = person_task(&sim.app, person);
        let inv = person_inventory(&sim.app, person);
        let wood_in_hand = inv.get(&crate::economy::core_ids::wood()).copied().unwrap_or(0);

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
                resource_id: crate::economy::core_ids::clay_tablet(),
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
            b.hunger(0.0).add_inventory(crate::economy::core_ids::weapon(), 1);
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
                resource_id: crate::economy::core_ids::weapon(),
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
        sim.spawn_ground_item(storage_tile, crate::economy::core_ids::wood(), 3);

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
                resource_id: crate::economy::core_ids::wood(),
                qty: 1,
            };
        }
        sim.tick();

        let ai = person_ai(&sim.app, person);
        let task = person_task(&sim.app, person);
        let inv = person_inventory(&sim.app, person);
        // Wood is Bulk::TwoHand 5kg — fits in either hands or inventory.
        let wood_total = inv.get(&crate::economy::core_ids::wood()).copied().unwrap_or(0);
        let in_hand = sim
            .app
            .world()
            .get::<crate::simulation::carry::Carrier>(person)
            .map(|c| {
                let wood = crate::economy::core_ids::wood();
                let l = c.left.map(|s| if s.item.resource_id == wood { s.qty } else { 0 }).unwrap_or(0);
                let r = c.right.map(|s| if s.item.resource_id == wood { s.qty } else { 0 }).unwrap_or(0);
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
                .add_inventory(crate::economy::core_ids::fruit(), 5)
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
        sim.spawn_ground_item(storage_tile, crate::economy::core_ids::fruit(), 5);

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
        let ground_item = sim.spawn_ground_item(scavenge_tile, crate::economy::core_ids::fruit(), 3);

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
        sim.spawn_ground_item(storage_tile, crate::economy::core_ids::wood(), 5);

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
                .map(|f| f.storage.stock_of(crate::economy::core_ids::wood()))
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
                    resource_id: crate::economy::core_ids::wood(),
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
                resource_id: Some(crate::economy::core_ids::wood()),
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
            Task::WithdrawMaterial { resource_id, qty } => {
                assert_eq!(
                    resource_id,
                    crate::economy::core_ids::wood(),
                    "head resource should match ClaimTarget"
                );
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
                    resource_id: crate::economy::core_ids::wood(),
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
            Some(Task::DepositToFactionStorage { resource_id }) => {
                assert_eq!(
                    resource_id,
                    crate::economy::core_ids::wood(),
                    "queued deposit resource should match GatherWood goal"
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
        let ground_item = sim.spawn_ground_item(scavenge_tile, crate::economy::core_ids::wood(), 3);

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
                    resource_id: crate::economy::core_ids::wood(),
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
            Some(Task::DepositToFactionStorage { resource_id }) => {
                assert_eq!(
                    resource_id,
                    crate::economy::core_ids::wood(),
                    "queued deposit resource should match GatherWood goal"
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
                    resource_id: crate::economy::core_ids::wood(),
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
        let fruit_entity = sim.spawn_ground_item((5, 0), crate::economy::core_ids::fruit(), 3);

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
                    resource_id: crate::economy::core_ids::fruit(),
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
                resource_id: Some(crate::economy::core_ids::fruit()),
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
            Some(Task::DepositToFactionStorage { resource_id: crate::economy::core_ids::fruit() }),
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
        // Spawn a chief first so the test agent doesn't get auto-promoted
        // to chief and locked into Goal::Lead (Phase 5e-x: Lead is now an
        // HTN method — its `Task::Lead { dest }` has no executor, so once
        // an agent enters it, `aq.current` never returns to Idle and the
        // chain-completion system can't observe the drain).
        let _chief = sim.spawn_person(sim.player_faction_id, (1, 1), |_| {});
        let person = sim.spawn_person(sim.player_faction_id, (4, 4), |b| {
            b.hunger(210.0).add_inventory(crate::economy::core_ids::fruit(), 10);
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

    /// Phase 5e-vi: an agent under `AgentGoal::Build` holding a
    /// `JobClaim::Build` with a `ClaimTarget.blueprint` pointing at a satisfied
    /// blueprint dispatches `Task::Construct { blueprint }` via
    /// `htn_build_claimed_blueprint_dispatch_system` +
    /// `BuildClaimedBlueprintMethod`. Replaces the legacy `ClaimedBuild` plan
    /// (PlanId 34, `[BuildClaimedBlueprint]`).
    #[test]
    fn build_claimed_blueprint_goal_dispatches_construct_task() {
        use crate::simulation::construction::{Blueprint, BuildSiteKind, WallMaterial};
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::jobs::{
            ClaimTarget, JobBoard, JobClaim, JobKind, JobPosting, JobProgress, JobSource,
        };
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(13);
        sim.flat_world(2, 0, TileKind::Grass);

        // Spawn a Palisade blueprint and pre-fill all deposit slots so
        // `bp.is_satisfied()` returns true (the dispatcher's gate).
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
        {
            let mut bp = sim.app.world_mut().get_mut::<Blueprint>(blueprint).unwrap();
            for i in 0..bp.deposit_count as usize {
                bp.deposits[i].deposited = bp.deposits[i].needed;
            }
            assert!(bp.is_satisfied(), "test setup: bp must read as satisfied");
        }

        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});

        // Brief warm-up so SpatialIndex / `Added<Indexed>` settle for the
        // blueprint entity. `htn_build_claimed_blueprint_dispatch_system`
        // doesn't read the spatial index for the bp lookup (uses bp_query +
        // ClaimTarget directly), so this is just for routing readiness.
        sim.tick_n(5);

        let job_id = {
            let mut board = sim.app.world_mut().resource_mut::<JobBoard>();
            let id = board.alloc_id();
            let posting = JobPosting {
                id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Build,
                progress: JobProgress::Building { blueprint },
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
                kind: JobKind::Build,
                posted_tick: 0,
                fail_count: 0,
            });
            entity.insert(ClaimTarget {
                blueprint: Some(blueprint),
                resource_id: None,
            });
            let mut goal = entity.get_mut::<AgentGoal>().unwrap();
            *goal = AgentGoal::Build;
        }

        // Two ticks: ParallelB's `htn_build_claimed_blueprint_dispatch_system`
        // argmaxes the registry, routes the agent toward the bp tile, and
        // dispatches `Task::Construct { blueprint }`.
        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");

        match aq.current {
            Task::Construct { blueprint: bp } => {
                assert_eq!(
                    bp, blueprint,
                    "head Task::Construct should target the claimed blueprint entity"
                );
            }
            other => panic!(
                "expected Task::Construct as head of ConstructBlueprint chain, got {:?}",
                other
            ),
        }
        assert_eq!(
            aq.queued_len(),
            0,
            "ConstructBlueprint is single-leg — nothing should be queued behind Construct"
        );
    }

    /// Phase 5e-xiii-a: an agent owning a *personal* blueprint (deposits NOT
    /// satisfied) under `AgentGoal::Build`, with the faction's storage holding
    /// the resource the bp still needs, dispatches the
    /// `[Task::WithdrawMaterial { wood, 1 }, Task::HaulToBlueprint { bp }]`
    /// chain via `htn_build_claimed_blueprint_dispatch_system` Path B +
    /// `WithdrawAndHaulToPersonalBlueprintMethod`. Replaces the legacy
    /// `HaulFromStorageAndBuild` plan (PlanId 29).
    #[test]
    fn personal_build_dispatches_withdraw_then_haul_chain_when_storage_has_wood() {
        use crate::simulation::construction::{Blueprint, BlueprintMap, BuildSiteKind};
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::person::Drafted;
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(91);
        sim.flat_world(2, 0, TileKind::Grass);

        // Storage tile holding 5 wood at (4, 4) — within range of (0, 0).
        let storage_tile = (4, 4);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);
        sim.spawn_ground_item(storage_tile, crate::economy::core_ids::wood(), 5);

        // Spawn a chief at (-5, -5) and explicitly mark them as FactionChief
        // so `goal_update_system`'s chief override doesn't pin our test
        // agent's goal to `Lead`. Mirrors `lead_goal_dispatches_typed_lead_task`
        // setup.
        let chief = sim.spawn_person(sim.player_faction_id, (-5, -5), |_| {});
        sim.app
            .world_mut()
            .entity_mut(chief)
            .insert(crate::simulation::faction::FactionChief);

        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        // Mark the agent `Drafted` during warm-up so neither plan_execution
        // nor any HTN dispatcher fires. Without this, the agent's auto Build
        // goal (`has_personal_build_site = true`) would let the dispatcher
        // run and partially complete the haul during warm-up, leaving the
        // bp deposits filled before we can capture the WithdrawMaterial head.
        sim.app.world_mut().entity_mut(person).insert(Drafted);

        // Spawn a personal Bed blueprint owned by this agent. Bed needs 3
        // wood; deposits start empty (not satisfied), so Path B's haul branch
        // wins.
        let blueprint_tile = (10, 10);
        let blueprint_world = tile_to_world(blueprint_tile.0, blueprint_tile.1);
        let blueprint = sim
            .app
            .world_mut()
            .spawn((
                Blueprint::new(
                    sim.player_faction_id,
                    Some(person),
                    BuildSiteKind::Bed,
                    blueprint_tile,
                    0,
                ),
                Transform::from_xyz(blueprint_world.x, blueprint_world.y, 0.5),
                GlobalTransform::default(),
                Visibility::Hidden,
                InheritedVisibility::default(),
            ))
            .id();
        sim.app
            .world_mut()
            .resource_mut::<BlueprintMap>()
            .0
            .insert(blueprint_tile, blueprint);

        // Warm-up: SpatialIndex needs `Added<Indexed>` for the GroundItem to
        // settle and StorageTileMap must register the storage tile. Mirrors
        // `acquire_good_haul_goal_dispatches_withdraw_then_haul_chain` (80
        // ticks). Drafted blocks dispatch so the agent stays inert.
        sim.tick_n(80);

        // After warm-up, ensure the chief assignment landed on `chief` (not
        // our test agent) and pin the registry's `chief_entity` field
        // accordingly. The chief override in `goal_update_system` keys off
        // the `FactionChief` component, so any stale assignment on `person`
        // would force it into `Lead` and pre-empt our Build dispatch.
        {
            sim.app
                .world_mut()
                .entity_mut(person)
                .remove::<crate::simulation::faction::FactionChief>();
            sim.app
                .world_mut()
                .entity_mut(chief)
                .insert(crate::simulation::faction::FactionChief);
            let mut registry = sim
                .app
                .world_mut()
                .resource_mut::<crate::simulation::faction::FactionRegistry>();
            if let Some(faction) = registry.factions.get_mut(&sim.player_faction_id) {
                faction.chief_entity = Some(chief);
            }
        }

        // Lift the draft and pin AgentGoal::Build (mirrors goal_update_system's
        // `has_personal_build_site` branch — already true for this agent, but
        // the explicit set is deterministic).
        {
            let mut entity = sim.app.world_mut().entity_mut(person);
            entity.remove::<Drafted>();
            let mut goal = entity.get_mut::<AgentGoal>().unwrap();
            *goal = AgentGoal::Build;
        }

        // Two ticks: ParallelB's dispatcher runs, picks Path B's
        // WithdrawAndHaulToPersonalBlueprintMethod (UTIL_CLAIMED_HAUL=2.0),
        // routes the agent toward the storage tile, dispatches WithdrawMaterial
        // and prefetches HaulToBlueprint.
        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");
        match aq.current {
            Task::WithdrawMaterial { resource_id, qty } => {
                assert_eq!(
                    resource_id,
                    crate::economy::core_ids::wood(),
                    "head must withdraw the bp's needed resource (Wood)"
                );
                assert_eq!(qty, 1, "WithdrawAndHaulToPersonalBlueprint withdraws qty=1");
            }
            other => panic!(
                "expected Task::WithdrawMaterial as head of personal-build chain, got {:?}",
                other
            ),
        }
        match aq.peek_next() {
            Some(Task::HaulToBlueprint { blueprint: bp }) => {
                assert_eq!(
                    bp, blueprint,
                    "queued tail HaulToBlueprint should target the personal bp"
                );
            }
            other => panic!(
                "expected queued Task::HaulToBlueprint targeting the personal bp, got {:?}",
                other
            ),
        }
    }

    /// Phase 5e-xiii-b: an agent owning a personal blueprint with NO faction
    /// storage of the needed resource but a remembered gather source
    /// (`MemoryKind::Resource(wood)`) dispatches the
    /// `[Task::Gather { tile }, Task::HaulToBlueprint { bp }]` chain via
    /// `htn_build_claimed_blueprint_dispatch_system` Path B +
    /// `GatherAndHaulToPersonalBlueprintMethod`. Replaces the legacy
    /// `BuildBlueprint` plan (PlanId 7).
    #[test]
    fn personal_build_dispatches_gather_then_haul_chain_when_storage_empty_but_memory_has_wood() {
        use crate::simulation::construction::{Blueprint, BlueprintMap, BuildSiteKind};
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::memory::{AgentMemory, MemoryKind};
        use crate::simulation::person::Drafted;
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(92);
        sim.flat_world(2, 0, TileKind::Grass);

        // Storage tile but NO wood — forces gather method over withdraw.
        let storage_tile = (4, 4);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);

        // Spawn a chief at (-5, -5) and lock chief assignment to them so the
        // test agent's goal isn't flipped to `Lead`.
        let chief = sim.spawn_person(sim.player_faction_id, (-5, -5), |_| {});
        sim.app
            .world_mut()
            .entity_mut(chief)
            .insert(crate::simulation::faction::FactionChief);

        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        sim.app.world_mut().entity_mut(person).insert(Drafted);

        // Personal Bed blueprint owned by this agent. Bed needs 3 wood.
        let blueprint_tile = (10, 10);
        let blueprint_world = tile_to_world(blueprint_tile.0, blueprint_tile.1);
        let blueprint = sim
            .app
            .world_mut()
            .spawn((
                Blueprint::new(
                    sim.player_faction_id,
                    Some(person),
                    BuildSiteKind::Bed,
                    blueprint_tile,
                    0,
                ),
                Transform::from_xyz(blueprint_world.x, blueprint_world.y, 0.5),
                GlobalTransform::default(),
                Visibility::Hidden,
                InheritedVisibility::default(),
            ))
            .id();
        sim.app
            .world_mut()
            .resource_mut::<BlueprintMap>()
            .0
            .insert(blueprint_tile, blueprint);

        // Warm-up so SpatialIndex / StorageTileMap settle. Drafted blocks
        // dispatch.
        sim.tick_n(80);

        // Inject `MemoryKind::Resource(wood)` outside `VIEW_RADIUS=15` so
        // `vision_system` doesn't clear it (mirrors the AcquireGood gather
        // test's pattern).
        let memory_tile = (-25, 0);
        {
            let mut entity = sim.app.world_mut().entity_mut(person);
            entity.remove::<Drafted>();
            entity
                .remove::<crate::simulation::faction::FactionChief>();
            if let Some(mut mem) = entity.get_mut::<AgentMemory>() {
                mem.record(memory_tile, MemoryKind::wood());
            } else {
                panic!("Person should have AgentMemory");
            }
            let mut goal = entity.get_mut::<AgentGoal>().unwrap();
            *goal = AgentGoal::Build;
        }
        // Pin chief_entity AFTER warm-up so it doesn't drift.
        {
            sim.app
                .world_mut()
                .entity_mut(chief)
                .insert(crate::simulation::faction::FactionChief);
            let mut registry = sim
                .app
                .world_mut()
                .resource_mut::<crate::simulation::faction::FactionRegistry>();
            if let Some(faction) = registry.factions.get_mut(&sim.player_faction_id) {
                faction.chief_entity = Some(chief);
            }
        }

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
                    "head Gather tile should match the injected MemoryKind::wood() entry"
                );
            }
            other => panic!(
                "expected Task::Gather as head of personal-build gather chain, got {:?}",
                other
            ),
        }
        match aq.peek_next() {
            Some(Task::HaulToBlueprint { blueprint: bp }) => {
                assert_eq!(
                    bp, blueprint,
                    "queued HaulToBlueprint should target the personal bp"
                );
            }
            other => panic!(
                "expected queued Task::HaulToBlueprint, got {:?}",
                other
            ),
        }
    }

    /// Phase 5e-xiv: a worker holding a `JobClaim::Stockpile { Skin }` claim
    /// (set by `posting_claim_target` for the chief-posted CraftOrder demand)
    /// scavenges a visible loose Skin GroundItem via
    /// `htn_acquire_good_dispatch_system`'s extended Stockpile branch and
    /// dispatches `[Task::Scavenge { target }, Task::DepositToFactionStorage { Skin }]`.
    /// Replaces the legacy `DeliverHideToCraftOrder` plan (PlanId 13) which
    /// chained Hunt → CollectSkin → HaulToCraftOrder; the new flow has skin
    /// land in storage first, then a separate worker delivers via
    /// `WithdrawAndHaulToCraftOrderMethod`.
    #[test]
    fn stockpile_goal_dispatches_scavenge_then_deposit_chain_for_skin() {
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::jobs::{
            ClaimTarget, JobBoard, JobClaim, JobKind, JobPosting, JobProgress, JobSource,
        };
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(93);
        sim.flat_world(2, 0, TileKind::Grass);
        let storage_tile = (-10, 0);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);

        // Spawn a chief at (1, 1) (auto-promoted) so the worker isn't
        // chosen as chief.
        let _chief = sim.spawn_person(sim.player_faction_id, (1, 1), |_| {});

        // Spawn a Skin GroundItem at (5, 0) — within VIEW_RADIUS=15 of the
        // worker at (0, 0) and outside the storage tile filter.
        let skin_id = crate::economy::core_ids::skin();
        let skin_entity = sim.spawn_ground_item((5, 0), skin_id, 1);

        let worker = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});

        sim.tick_n(10);

        // Post a `JobKind::Stockpile { Skin }` posting + claim onto the
        // worker. `posting_goal()` maps Stockpile{Skin} → AgentGoal::Stockpile;
        // `posting_claim_target()` sets `ClaimTarget.resource_id = Skin`.
        let job_id = {
            let mut board = sim.app.world_mut().resource_mut::<JobBoard>();
            let id = board.alloc_id();
            let posting = JobPosting {
                id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Stockpile,
                progress: JobProgress::Stockpile {
                    resource_id: skin_id,
                    deposited: 0,
                    target: 4,
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
        {
            let mut entity = sim.app.world_mut().entity_mut(worker);
            entity.insert(JobClaim {
                job_id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Stockpile,
                posted_tick: 0,
                fail_count: 0,
            });
            entity.insert(ClaimTarget {
                blueprint: None,
                resource_id: Some(skin_id),
            });
            let mut goal = entity.get_mut::<AgentGoal>().unwrap();
            *goal = AgentGoal::Stockpile;
        }

        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(worker)
            .expect("ActionQueue missing");
        match aq.current {
            Task::Scavenge { target } => {
                assert_eq!(
                    target, skin_entity,
                    "head Scavenge should target the visible Skin GroundItem"
                );
            }
            other => panic!(
                "expected Task::Scavenge as head of Stockpile chain, got {:?}",
                other
            ),
        }
        match aq.peek_next() {
            Some(Task::DepositToFactionStorage { resource_id }) => {
                assert_eq!(
                    resource_id, skin_id,
                    "queued DepositToFactionStorage should carry the Skin resource"
                );
            }
            other => panic!(
                "expected queued Task::DepositToFactionStorage, got {:?}",
                other
            ),
        }
    }

    /// Phase 5e-viii-c: a hunter under a fresh `HuntOrder::Hunt` (party not
    /// yet deployed, not stale) dispatches `Task::HuntPartyMuster { hearth }`
    /// via `htn_join_hunt_party_dispatch_system` + `MusterAtHearthMethod`.
    /// `TravelToHuntAreaMethod` rejects (deployed=false, stale=false). The
    /// hearth tile resolves to the faction's `home_tile` because no campfires
    /// exist in the fixture.
    #[test]
    fn join_hunt_party_dispatches_muster_when_not_deployed() {
        use crate::simulation::corpse::CorpseSpecies;
        use crate::simulation::faction::HuntOrder;
        use crate::simulation::knowledge::PersonKnowledge;
        use crate::simulation::person::Profession;
        use crate::simulation::technology::HUNTING_SPEAR;
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(57);
        sim.flat_world(2, 0, TileKind::Grass);

        // Hunter at (5, 5); faction home is (0, 0). Area at (10, 10) (hunt
        // target). With no campfires in the fixture, hearth resolves to the
        // home tile (0, 0).
        let person = sim.spawn_person(sim.player_faction_id, (5, 5), |b| {
            b.profession(Profession::Hunter);
        });
        {
            let mut knowledge = sim
                .app
                .world_mut()
                .get_mut::<PersonKnowledge>(person)
                .unwrap();
            knowledge.aware |= 1u64 << HUNTING_SPEAR;
            knowledge.learned |= 1u64 << HUNTING_SPEAR;
        }
        // Post a fresh HuntOrder::Hunt with an empty mustered list and
        // deployed_tick = None — the muster phase precondition.
        {
            let mut registry = sim
                .app
                .world_mut()
                .resource_mut::<crate::simulation::faction::FactionRegistry>();
            let faction = registry.factions.get_mut(&sim.player_faction_id).unwrap();
            faction.hunt_order = Some(HuntOrder::Hunt {
                species: CorpseSpecies::Deer,
                area_tile: (10, 10),
                target_party_size: 4,
                mustered: Vec::new(),
                deployed_tick: None,
                posted_tick: 1,
            });
        }

        sim.tick_n(5);
        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");
        match aq.current {
            Task::HuntPartyMuster { hearth } => {
                assert_eq!(
                    hearth,
                    (0, 0),
                    "muster hearth should fall back to faction home_tile"
                );
            }
            other => panic!(
                "expected Task::HuntPartyMuster as head of JoinHuntParty chain, got {:?}",
                other
            ),
        }
    }

    /// Phase 5e-viii-b: a hunter (Profession::Hunter, Learned HUNTING_SPEAR)
    /// in a faction with a live `HuntOrder::Hunt`, with a fresh corpse within
    /// VIEW_RADIUS, dispatches `Task::PickUpCorpse { corpse }` via
    /// `htn_engage_prey_dispatch_system` + `PickUpFreshCorpseMethod` (which
    /// outscores `HuntPreyMethod` since no live prey is in range).
    #[test]
    fn engage_prey_method_dispatches_pickup_when_corpse_in_range() {
        use crate::simulation::corpse::{Corpse, CorpseMap, CorpseSpecies};
        use crate::simulation::faction::HuntOrder;
        use crate::simulation::knowledge::PersonKnowledge;
        use crate::simulation::person::Profession;
        use crate::simulation::technology::HUNTING_SPEAR;
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(42);
        sim.flat_world(2, 0, TileKind::Grass);

        // Hunter at (5, 5); fresh corpse at (10, 10) — within VIEW_RADIUS=15 but
        // far enough that the agent can't walk-and-pickup before the assertion.
        let person = sim.spawn_person(sim.player_faction_id, (5, 5), |b| {
            b.profession(Profession::Hunter);
        });

        // Mark HUNTING_SPEAR Learned on the hunter (paleolithic_seed only sets
        // Paleolithic techs; HUNTING_SPEAR is also Paleolithic so it's already
        // there, but assert explicitly to make the test legible).
        {
            let mut knowledge = sim
                .app
                .world_mut()
                .get_mut::<PersonKnowledge>(person)
                .unwrap();
            knowledge.aware |= 1u64 << HUNTING_SPEAR;
            knowledge.learned |= 1u64 << HUNTING_SPEAR;
        }

        // Spawn corpse + insert into CorpseMap (the dispatcher reads the map
        // directly, mirroring the legacy `NearestFreshCorpse` resolver).
        let corpse_tile = (10, 10);
        let corpse_world = tile_to_world(corpse_tile.0, corpse_tile.1);
        let corpse = sim
            .app
            .world_mut()
            .spawn((
                Corpse {
                    species: CorpseSpecies::Deer,
                    fresh_until_tick: 1_000_000,
                },
                Transform::from_xyz(corpse_world.x, corpse_world.y, 0.4),
                GlobalTransform::default(),
                Visibility::Hidden,
                InheritedVisibility::default(),
            ))
            .id();
        sim.app
            .world_mut()
            .resource_mut::<CorpseMap>()
            .insert(corpse_tile, corpse);

        // Faction needs a live HuntOrder::Hunt — the dispatcher gates on it.
        {
            let mut registry = sim
                .app
                .world_mut()
                .resource_mut::<crate::simulation::faction::FactionRegistry>();
            let faction = registry.factions.get_mut(&sim.player_faction_id).unwrap();
            faction.hunt_order = Some(HuntOrder::Hunt {
                species: CorpseSpecies::Deer,
                area_tile: (5, 5),
                target_party_size: 1,
                mustered: vec![person],
                deployed_tick: Some(0),
                posted_tick: 0,
            });
        }

        sim.tick_n(5);

        // Two ticks: ParallelB's `htn_engage_prey_dispatch_system` resolves
        // the corpse, scores `PickUpFreshCorpseMethod` (1.5) above
        // `HuntPreyMethod` (no prey → precondition fails), routes the agent
        // toward the corpse tile, and dispatches `Task::PickUpCorpse`.
        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");

        match aq.current {
            Task::PickUpCorpse { corpse: c } => {
                assert_eq!(
                    c, corpse,
                    "head Task::PickUpCorpse should target the spawned corpse entity"
                );
            }
            other => panic!(
                "expected Task::PickUpCorpse as head of EngagePrey chain, got {:?}",
                other
            ),
        }
        assert_eq!(
            aq.queued_len(),
            0,
            "PickUpFreshCorpseMethod is single-leg — nothing should be queued"
        );
    }

    /// Phase 5e-viii-a: a hunter holding a `Carrying` component (the corpse)
    /// with no `ActivePlan` triggers `htn_deliver_hunt_kill_dispatch_system` →
    /// `DeliverHuntKillMethod`, which dispatches `Task::HaulCorpse { dest }`
    /// as the head and prefetches `Task::Butcher` on the queue. Replaces the
    /// trailing two steps (`HaulCorpse` + `Butcher`) of the legacy `HuntFood`
    /// plan (PlanId 5).
    #[test]
    fn carrying_agent_dispatches_haul_corpse_then_butcher_chain() {
        use crate::simulation::corpse::{Carrying, Corpse, CorpseSpecies};
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(31);
        sim.flat_world(2, 0, TileKind::Grass);

        // Put person at (5, 5); faction home is (0, 0) so the butcher site
        // resolves via `faction_registry.home_tile` fallback (no campfires
        // in the fixture). Agent has work to do reaching the destination.
        let person = sim.spawn_person(sim.player_faction_id, (5, 5), |_| {});

        // Spawn a corpse at the agent's tile (the legacy pickup_corpse_task
        // would have placed it there; we shortcut to skip routing).
        let corpse_world = tile_to_world(5, 5);
        let corpse = sim
            .app
            .world_mut()
            .spawn((
                Corpse {
                    species: CorpseSpecies::Deer,
                    fresh_until_tick: 1_000_000,
                },
                Transform::from_xyz(corpse_world.x, corpse_world.y, 0.4),
                GlobalTransform::default(),
                Visibility::Hidden,
                InheritedVisibility::default(),
            ))
            .id();
        sim.app
            .world_mut()
            .entity_mut(person)
            .insert(Carrying(corpse));

        // Warm-up so SpatialIndex / `Added<Indexed>` settle. The dispatcher
        // doesn't read the spatial index for the corpse lookup, but routing
        // does.
        sim.tick_n(5);

        // Two ticks: ParallelB's `htn_deliver_hunt_kill_dispatch_system`
        // resolves the butcher site, scores `DeliverHuntKillMethod`, routes
        // the agent toward home, and dispatches `Task::HaulCorpse` as head
        // + queues `Task::Butcher`.
        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");

        match aq.current {
            Task::HaulCorpse { dest } => {
                assert_eq!(
                    dest,
                    (0, 0),
                    "head Task::HaulCorpse should target the faction home tile (no campfires in fixture)"
                );
            }
            other => panic!(
                "expected Task::HaulCorpse as head of DeliverHuntKill chain, got {:?}",
                other
            ),
        }
        assert_eq!(
            aq.queued_len(),
            1,
            "DeliverHuntKill is two-leg — Task::Butcher should be queued behind HaulCorpse"
        );
        assert!(
            aq.peek_next().map(|t| t.is_butcher()).unwrap_or(false),
            "queued tail should be Task::Butcher; got {:?}",
            aq.peek_next()
        );
    }

    /// Phase 5e-x: a chief with `AgentGoal::Lead` dispatches
    /// `Task::Lead { dest }` via `htn_combat_faction_dispatch_system` +
    /// `LeadCampMethod`, walking to faction `home_tile`. Lead is the
    /// simplest of the four combat/faction goals (single-method, single-leg,
    /// no faction-state lookups beyond `home_tile`).
    #[test]
    fn lead_goal_dispatches_typed_lead_task() {
        use crate::simulation::faction::FactionChief;
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(101);
        sim.flat_world(2, 0, TileKind::Grass);

        // Spawn a chief at (5, 5) and explicitly mark them as FactionChief.
        // Faction home defaults to (0, 0) in the fixture's `create_faction`.
        let chief = sim.spawn_person(sim.player_faction_id, (5, 5), |_| {});
        sim.app.world_mut().entity_mut(chief).insert(FactionChief);

        // Warm up SpatialIndex.
        sim.tick_n(5);

        // Pin the goal to Lead. `goal_update_system` re-derives every tick,
        // and the FactionChief + peacetime + low-need conditions normally
        // produce Lead — but `last_goal_eval_tick` cooldown can keep an
        // older goal pinned. Pinning right before the dispatch tick makes
        // the assertion deterministic.
        {
            let mut goal = sim.app.world_mut().get_mut::<AgentGoal>(chief).unwrap();
            *goal = AgentGoal::Lead;
        }
        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(chief)
            .expect("ActionQueue missing");
        match aq.current {
            Task::Lead { dest } => {
                assert_eq!(
                    dest,
                    (0, 0),
                    "head Task::Lead should target the faction home tile"
                );
            }
            other => panic!(
                "expected Task::Lead as head of Lead chain, got {:?}",
                other
            ),
        }
        assert_eq!(
            aq.queued_len(),
            0,
            "Lead is single-leg — nothing should be queued"
        );
    }

    /// Phase 5e-ix: an agent with `AgentGoal::Socialize` and another Person
    /// within 12 tiles dispatches `Task::Socialize { partner }` via
    /// `htn_socialize_dispatch_system` + `SocializeWithPartnerMethod`.
    /// Drives the goal naturally via high `needs.social` so
    /// `goal_update_system` settles on `Socialize` and stays there across
    /// dispatch ticks.
    #[test]
    fn socialize_goal_dispatches_typed_socialize_task() {
        use crate::simulation::needs::Needs;
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(91);
        sim.flat_world(2, 0, TileKind::Grass);

        // Two people in the same faction: the actor at (0, 0) and a partner
        // at (3, 0) — well within PARTNER_RADIUS=12 and not adjacent so the
        // dispatcher actually has to route. Actor needs `social > 160` to
        // beat the default Survive goal in `goal_update_system`; everything
        // else stays at default (low) so Survive / Sleep / Tired don't
        // preempt.
        let actor = sim.spawn_person(sim.player_faction_id, (0, 0), |b| {
            b.needs(Needs {
                hunger: 20.0,
                sleep: 20.0,
                shelter: 20.0,
                safety: 20.0,
                social: 220.0,
                reproduction: 20.0,
                willpower: 220.0,
            });
        });
        let partner = sim.spawn_person(sim.player_faction_id, (3, 0), |_| {});

        // Seven ticks: SpatialIndex / `Added<Indexed>` settle for both spawn
        // sites, `goal_update_system` sees high social need and flips to
        // Socialize, ParallelB's `htn_socialize_dispatch_system` argmaxes
        // the registry, routes the agent toward the partner, and dispatches
        // `Task::Socialize { partner }`.
        sim.tick_n(7);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(actor)
            .expect("ActionQueue missing");
        match aq.current {
            Task::Socialize { partner: p } => {
                assert_eq!(
                    p, partner,
                    "head Task::Socialize should target the nearest other Person"
                );
            }
            other => panic!(
                "expected Task::Socialize as head of Socialize chain, got {:?}",
                other
            ),
        }
        assert_eq!(
            aq.queued_len(),
            0,
            "Socialize is single-leg — nothing should be queued"
        );
    }

    /// Phase 5e-xi-a: an agent under `AgentGoal::Craft` with an open faction
    /// `CraftOrder` needing Wood and Wood in storage dispatches the chain
    /// `[WithdrawMaterial { Wood, 1 }, HaulToCraftOrder { order }]` via
    /// `htn_deliver_material_to_craft_order_dispatch_system` +
    /// `WithdrawAndHaulToCraftOrderMethod`. Replaces the legacy
    /// `DeliverFromStorageToCraftOrder` plan (PlanId 15).
    #[test]
    fn craft_goal_dispatches_withdraw_then_haul_to_craft_order_chain() {
        use crate::simulation::crafting::{CraftOrder, CraftOrderMap};
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(123);
        sim.flat_world(2, 0, TileKind::Grass);
        let storage_tile = (4, 4);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);
        sim.spawn_ground_item(storage_tile, crate::economy::core_ids::wood(), 5);

        // Spawn a craft order at a distinct tile. Recipe 1 (Spear) needs
        // 2 wood + 1 stone; we leave deposits empty so both legs see unmet
        // demand. The dispatcher will resolve to Wood (most-deficient on the
        // tile we stocked).
        let order_tile = (10, 10);
        let order_world = tile_to_world(order_tile.0, order_tile.1);
        let order = CraftOrder::new(
            sim.player_faction_id,
            /* recipe_id = Spear */ 1,
            /* workbench_tile */ None,
            order_tile,
            /* spawn_tick */ 0,
            /* tech_payload */ None,
        )
        .expect("recipe 1 (Spear) should construct");
        let order_entity = sim
            .app
            .world_mut()
            .spawn((
                order,
                Transform::from_xyz(order_world.x, order_world.y, 0.32),
                GlobalTransform::default(),
                Visibility::Hidden,
                InheritedVisibility::default(),
            ))
            .id();
        sim.app
            .world_mut()
            .resource_mut::<CraftOrderMap>()
            .0
            .insert(order_tile, order_entity);

        // Spawn the agent with empty hands so the WithdrawMaterial path is
        // the only viable expansion.
        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});

        // Warm-up ticks: storage rollup must populate
        // `FactionData.storage.totals[Wood] > 0` and `StorageTileMap` must
        // know the storage tile before the dispatcher's tile scan can find
        // it. `Added<Indexed>` for the GroundItem also needs a few
        // FixedUpdate frames.
        sim.tick_n(80);

        {
            let registry = sim.app.world().resource::<FactionRegistry>();
            let stock = registry
                .factions
                .get(&sim.player_faction_id)
                .map(|f| f.storage.stock_of(crate::economy::core_ids::wood()))
                .unwrap_or(0);
            assert!(
                stock > 0,
                "faction storage rollup should report Wood stock > 0 after warm-up; got {}",
                stock
            );
        }

        // Pin the goal to Craft right before the dispatch tick.
        // `goal_update_system` re-derives every tick — pinning here is
        // resilient to the bucketed cooldown.
        {
            let mut goal = sim.app.world_mut().get_mut::<AgentGoal>(person).unwrap();
            *goal = AgentGoal::Craft;
        }
        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");

        match aq.current {
            Task::WithdrawMaterial { resource_id, qty } => {
                assert_eq!(
                    resource_id,
                    crate::economy::core_ids::wood(),
                    "head resource should be Wood (most-deficient on the stocked tile)"
                );
                assert_eq!(qty, 1, "DeliverMaterialToCraftOrder uses qty:1 contract");
            }
            other => panic!(
                "expected Task::WithdrawMaterial as head of DeliverMaterialToCraftOrder chain, got {:?}",
                other
            ),
        }

        assert_eq!(
            aq.queued_len(),
            1,
            "expected exactly one queued task (HaulToCraftOrder) behind WithdrawMaterial"
        );
        match aq.peek_next() {
            Some(Task::HaulToCraftOrder { order: o }) => {
                assert_eq!(
                    o, order_entity,
                    "queued HaulToCraftOrder should target the spawned order entity"
                );
            }
            other => panic!(
                "expected Task::HaulToCraftOrder queued behind WithdrawMaterial, got {:?}",
                other
            ),
        }
    }

    /// Phase 5e-xi-b: an agent under `AgentGoal::Craft` with a satisfied
    /// faction `CraftOrder` (deposits filled) dispatches the chain
    /// `[WorkOnCraftOrder { order }, DepositToFactionStorage { output }]`
    /// via `htn_work_on_craft_order_dispatch_system` +
    /// `WorkOnSatisfiedCraftOrderMethod`. Replaces the legacy `WorkOnCraft`
    /// plan (PlanId 16).
    #[test]
    fn craft_goal_dispatches_work_on_craft_order_chain_when_order_satisfied() {
        use crate::simulation::crafting::{CraftOrder, CraftOrderMap};
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::jobs::{
            ClaimTarget, JobBoard, JobClaim, JobKind, JobPosting, JobProgress, JobSource,
        };
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(124);
        sim.flat_world(2, 0, TileKind::Grass);

        // Spawn a Spear (recipe 1) order at (10, 10) and pre-fill its
        // deposits so `is_satisfied()` is true the moment the dispatcher
        // sees it. Recipe 1 needs 2 Wood + 1 Stone.
        let order_tile = (10, 10);
        let order_world = tile_to_world(order_tile.0, order_tile.1);
        let mut order = CraftOrder::new(
            sim.player_faction_id,
            /* recipe_id = Spear */ 1,
            None,
            order_tile,
            0,
            None,
        )
        .expect("recipe 1 should construct");
        for i in 0..order.deposit_count as usize {
            order.deposits[i].deposited = order.deposits[i].needed;
        }
        assert!(order.is_satisfied());
        let order_entity = sim
            .app
            .world_mut()
            .spawn((
                order,
                Transform::from_xyz(order_world.x, order_world.y, 0.32),
                GlobalTransform::default(),
                Visibility::Hidden,
                InheritedVisibility::default(),
            ))
            .id();
        sim.app
            .world_mut()
            .resource_mut::<CraftOrderMap>()
            .0
            .insert(order_tile, order_entity);

        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});

        // Brief warm-up so SpatialIndex / Indexed insertion settles.
        sim.tick_n(5);

        // Post a `JobKind::Craft` posting + claim onto the agent so
        // `job_goal_lock_system` pins `AgentGoal::Craft` deterministically
        // (the test faction has no craft-tech, so `should_craft` would
        // return false and `goal_update_system` would re-derive the goal).
        let job_id = {
            let mut board = sim.app.world_mut().resource_mut::<JobBoard>();
            let id = board.alloc_id();
            let posting = JobPosting {
                id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Craft,
                progress: JobProgress::Crafting {
                    crafted: 0,
                    target: 1,
                    recipe: 1,
                    bench: None,
                    tech_payload: None,
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
                kind: JobKind::Craft,
                posted_tick: 0,
                fail_count: 0,
            });
            entity.insert(ClaimTarget::default());
            let mut goal = entity.get_mut::<AgentGoal>().unwrap();
            *goal = AgentGoal::Craft;
        }
        sim.tick_n(2);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");

        match aq.current {
            Task::WorkOnCraftOrder { order: o } => {
                assert_eq!(
                    o, order_entity,
                    "head Task::WorkOnCraftOrder should target the spawned satisfied order"
                );
            }
            other => panic!(
                "expected Task::WorkOnCraftOrder as head of WorkOnCraft chain, got {:?}",
                other
            ),
        }

        assert_eq!(
            aq.queued_len(),
            1,
            "expected exactly one queued task (DepositToFactionStorage) behind WorkOnCraftOrder"
        );
        match aq.peek_next() {
            Some(Task::DepositToFactionStorage { resource_id }) => {
                assert_eq!(
                    resource_id,
                    crate::economy::core_ids::weapon(),
                    "queued deposit should carry the recipe output (Spear → Weapon class)"
                );
            }
            other => panic!(
                "expected Task::DepositToFactionStorage queued behind WorkOnCraftOrder, got {:?}",
                other
            ),
        }
    }

    /// Phase 5e-xi-c: an agent under `AgentGoal::Craft` with an open faction
    /// `CraftOrder` needing Grain and a remembered mature Grain plant
    /// dispatches `[Gather { plant_tile }, HaulToCraftOrder { order }]` via
    /// `htn_harvest_grain_for_craft_order_dispatch_system` +
    /// `HarvestAndHaulGrainToCraftOrderMethod`. Replaces the legacy
    /// `DeliverGrainToCraftOrder` plan (PlanId 14).
    #[test]
    fn craft_goal_dispatches_harvest_grain_then_haul_to_craft_order_chain() {
        use crate::simulation::construction::GoodNeed;
        use crate::simulation::crafting::{CraftOrder, CraftOrderMap, MAX_CRAFT_INPUTS};
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::jobs::{
            ClaimTarget, JobBoard, JobClaim, JobKind, JobPosting, JobProgress, JobSource,
        };
        use crate::simulation::memory::{AgentMemory, MemoryKind};
        use crate::simulation::plants::{GrowthStage, Plant, PlantKind, PlantMap};
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(125);
        sim.flat_world(2, 0, TileKind::Grass);

        // Spawn a mature Grain plant outside VIEW_RADIUS=15 so injected
        // memory survives `vision_system` clearing on the dispatch tick.
        // (Memory-driven test targets must be outside VIEW_RADIUS — see
        // test_fixture quirks.)
        let grain_tile = (40, 0);
        let grain_world = tile_to_world(grain_tile.0, grain_tile.1);
        let grain_entity = sim
            .app
            .world_mut()
            .spawn((
                Plant {
                    kind: PlantKind::Grain,
                    stage: GrowthStage::Mature,
                    growth_ticks: 0,
                    tile_pos: grain_tile,
                },
                Transform::from_xyz(grain_world.x, grain_world.y, 0.4),
                GlobalTransform::default(),
                Visibility::Hidden,
                InheritedVisibility::default(),
            ))
            .id();
        sim.app
            .world_mut()
            .resource_mut::<PlantMap>()
            .0
            .insert(grain_tile, grain_entity);

        // Spawn a CraftOrder needing Grain (Woven Cloth, recipe 4 — needs
        // 3 Grain, station Loom, but tech_gate isn't checked at order-spawn
        // time; the dispatcher just walks the deposits). To bypass any
        // station-availability concerns and keep the test focused on the
        // dispatcher, hand-construct an order whose deposit is Grain.
        let order_tile = (10, 10);
        let order_world = tile_to_world(order_tile.0, order_tile.1);
        let mut deposits = [GoodNeed::default(); MAX_CRAFT_INPUTS];
        deposits[0] = GoodNeed {
            resource_id: crate::economy::core_ids::grain(),
            needed: 3,
            deposited: 0,
        };
        let order = CraftOrder {
            faction_id: sim.player_faction_id,
            workbench_tile: None,
            anchor_tile: order_tile,
            recipe_id: 4, // Woven Cloth
            deposits,
            deposit_count: 1,
            work_progress: 0,
            spawn_tick: 0,
            tech_payload: None,
        };
        let order_entity = sim
            .app
            .world_mut()
            .spawn((
                order,
                Transform::from_xyz(order_world.x, order_world.y, 0.32),
                GlobalTransform::default(),
                Visibility::Hidden,
                InheritedVisibility::default(),
            ))
            .id();
        sim.app
            .world_mut()
            .resource_mut::<CraftOrderMap>()
            .0
            .insert(order_tile, order_entity);

        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});

        // Inject `AgentMemory::AnyEdible` pointing at the grain tile so the
        // dispatcher's `best_for(AnyEdible)` lookup finds it.
        {
            let mut mem = sim
                .app
                .world_mut()
                .get_mut::<AgentMemory>(person)
                .expect("AgentMemory missing");
            mem.record(grain_tile, MemoryKind::AnyEdible);
        }

        // Pin AgentGoal::Craft via JobClaim::Craft so `job_goal_lock_system`
        // keeps it stuck across dispatch ticks (test faction has no craft
        // tech, so `should_craft` would return false).
        let job_id = {
            let mut board = sim.app.world_mut().resource_mut::<JobBoard>();
            let id = board.alloc_id();
            let posting = JobPosting {
                id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Craft,
                progress: JobProgress::Crafting {
                    crafted: 0,
                    target: 1,
                    recipe: 4,
                    bench: None,
                    tech_payload: None,
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
                kind: JobKind::Craft,
                posted_tick: 0,
                fail_count: 0,
            });
            entity.insert(ClaimTarget::default());
            let mut goal = entity.get_mut::<AgentGoal>().unwrap();
            *goal = AgentGoal::Craft;
        }

        sim.tick_n(7);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");

        match aq.current {
            Task::Gather { tile } => {
                assert_eq!(
                    tile, grain_tile,
                    "head Task::Gather should target the remembered grain plant tile"
                );
            }
            other => panic!(
                "expected Task::Gather as head of HarvestGrainForCraftOrder chain, got {:?}",
                other
            ),
        }

        assert_eq!(
            aq.queued_len(),
            1,
            "expected one queued task (HaulToCraftOrder) behind Gather"
        );
        match aq.peek_next() {
            Some(Task::HaulToCraftOrder { order: o }) => {
                assert_eq!(
                    o, order_entity,
                    "queued HaulToCraftOrder should target the spawned grain order"
                );
            }
            other => panic!(
                "expected Task::HaulToCraftOrder queued behind Gather, got {:?}",
                other
            ),
        }
    }

    /// Phase 5 closure: an agent under `AgentGoal::Farm` (set via
    /// `JobClaim::Farm`) with Learned `CROP_CULTIVATION` and a remembered
    /// mature edible plant dispatches a `[Gather { tile },
    /// DepositToFactionStorage { resource_id }]` chain via
    /// `htn_harvest_plant_dispatch_system` +
    /// `HarvestMaturePlantForStorageMethod`. Replaces the legacy `FarmFood`
    /// plan (PlanId 1) — the last live legacy plan.
    #[test]
    fn farm_goal_dispatches_harvest_then_deposit_chain_when_memory_has_grain() {
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::jobs::{
            ClaimTarget, JobBoard, JobClaim, JobKind, JobPosting, JobProgress, JobSource,
            TileAabb,
        };
        use crate::simulation::knowledge::PersonKnowledge;
        use crate::simulation::memory::{AgentMemory, MemoryKind};
        use crate::simulation::plants::{GrowthStage, Plant, PlantKind, PlantMap};
        use crate::simulation::technology::CROP_CULTIVATION;
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(173);
        sim.flat_world(2, 0, TileKind::Grass);

        // Storage tile so the trailing DepositToFactionStorage's deposit-tile
        // resolution succeeds; without it the dispatcher still chooses the
        // method (deposit_tile is informational), but the chain handoff at
        // gather completion would route nowhere. Place at (4, 4) — within
        // the agent's chunk for routing.
        let storage_tile = (4, 4);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);

        // Mature Grain plant outside VIEW_RADIUS=15 so injected memory survives
        // the vision_system clear on the dispatch tick (test_fixture quirk).
        let grain_tile = (40, 0);
        let grain_world = tile_to_world(grain_tile.0, grain_tile.1);
        let grain_entity = sim
            .app
            .world_mut()
            .spawn((
                Plant {
                    kind: PlantKind::Grain,
                    stage: GrowthStage::Mature,
                    growth_ticks: 0,
                    tile_pos: grain_tile,
                },
                Transform::from_xyz(grain_world.x, grain_world.y, 0.4),
                GlobalTransform::default(),
                Visibility::Hidden,
                InheritedVisibility::default(),
            ))
            .id();
        sim.app
            .world_mut()
            .resource_mut::<PlantMap>()
            .0
            .insert(grain_tile, grain_entity);

        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});

        // Grant CROP_CULTIVATION (Neolithic — not in `paleolithic_seed`).
        // Setting both aware and learned so `has_learned` returns true.
        {
            let mut knowledge = sim
                .app
                .world_mut()
                .get_mut::<PersonKnowledge>(person)
                .unwrap();
            knowledge.aware |= 1u64 << CROP_CULTIVATION;
            knowledge.learned |= 1u64 << CROP_CULTIVATION;
        }

        // Inject memory of the grain tile.
        {
            let mut mem = sim
                .app
                .world_mut()
                .get_mut::<AgentMemory>(person)
                .expect("AgentMemory missing");
            mem.record(grain_tile, MemoryKind::AnyEdible);
        }

        // Pin AgentGoal::Farm via JobClaim::Farm so `job_goal_lock_system`
        // keeps it stuck across dispatch ticks (the test faction's chief
        // wouldn't otherwise post Farm jobs in the harness).
        let job_id = {
            let mut board = sim.app.world_mut().resource_mut::<JobBoard>();
            let id = board.alloc_id();
            let posting = JobPosting {
                id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Farm,
                progress: JobProgress::Planting {
                    planted: 0,
                    target: 1,
                    area: TileAabb {
                        min: (-10, -10),
                        max: (10, 10),
                    },
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
                kind: JobKind::Farm,
                posted_tick: 0,
                fail_count: 0,
            });
            entity.insert(ClaimTarget::default());
            let mut goal = entity.get_mut::<AgentGoal>().unwrap();
            *goal = AgentGoal::Farm;
        }

        sim.tick_n(7);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(person)
            .expect("ActionQueue missing");

        match aq.current {
            Task::Gather { tile } => {
                assert_eq!(
                    tile, grain_tile,
                    "head Task::Gather should target the remembered grain plant tile"
                );
            }
            other => panic!(
                "expected Task::Gather as head of HarvestPlant chain, got {:?}",
                other
            ),
        }

        assert_eq!(
            aq.queued_len(),
            1,
            "expected one queued task (DepositToFactionStorage) behind Gather"
        );
        match aq.peek_next() {
            Some(Task::DepositToFactionStorage { resource_id }) => {
                assert_eq!(
                    resource_id,
                    crate::economy::core_ids::grain(),
                    "queued DepositToFactionStorage should carry the grain resource"
                );
            }
            other => panic!(
                "expected Task::DepositToFactionStorage queued behind Gather, got {:?}",
                other
            ),
        }
    }

    /// Phase 5e-xii-a: an agent under `AgentGoal::Play` with a nearby other
    /// Person dispatches `Task::Play { partner: Some(e) }` via
    /// `htn_play_dispatch_system` + `PlayWithPartnerMethod`. Replaces the
    /// legacy `PlaySocial` plan (PlanId 26).
    #[test]
    fn play_goal_dispatches_play_with_partner_when_partner_in_range() {
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::needs::Needs;
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(141);
        sim.flat_world(2, 0, TileKind::Grass);

        // Two people: actor at (0, 0) with low willpower so goal_update_system
        // settles on AgentGoal::Play; partner at (3, 0) within PLAY_RADIUS=12.
        let actor = sim.spawn_person(sim.player_faction_id, (0, 0), |b| {
            b.needs(Needs {
                hunger: 20.0,
                sleep: 20.0,
                shelter: 20.0,
                safety: 20.0,
                social: 20.0,
                reproduction: 20.0,
                willpower: 30.0, // below PLAY_THRESHOLD so Play goal naturally fires
            });
        });
        let partner = sim.spawn_person(sim.player_faction_id, (3, 0), |_| {});

        // Seven ticks: SpatialIndex / Added<Indexed> settle for both spawn
        // sites, goal_update_system flips to Play, ParallelB dispatcher
        // argmaxes the registry and routes to partner.
        sim.tick_n(7);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(actor)
            .expect("ActionQueue missing");

        match aq.current {
            Task::Play { partner: Some(p) } => {
                assert_eq!(
                    p, partner,
                    "head Task::Play should target the nearest other Person"
                );
            }
            other => panic!(
                "expected Task::Play with partner as head of Play chain, got {:?}",
                other
            ),
        }
        assert_eq!(
            aq.queued_len(),
            0,
            "Play is single-leg — nothing should be queued"
        );
        // Verify the goal actually settled on Play during the dispatch tick.
        let goal = sim.app.world().get::<AgentGoal>(actor).expect("goal missing");
        assert_eq!(*goal, AgentGoal::Play, "expected goal to be Play");
    }

    /// Phase 5e-xii-b: an agent under `AgentGoal::Play` with no nearby partner
    /// or held entertainment item but with Stone in faction storage dispatches
    /// the `[Task::WithdrawMaterial { stone, 1 }, Task::PlayThrow]` chain via
    /// `htn_play_dispatch_system` + `WithdrawAndThrowStonesAsPlayMethod`.
    /// Replaces the legacy `PlayByThrowingRocks` plan (PlanId 31).
    #[test]
    fn play_goal_dispatches_withdraw_then_throw_stones_chain_when_alone() {
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::needs::Needs;
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(142);
        sim.flat_world(2, 0, TileKind::Grass);

        // Storage tile with Stone, far enough away that the actor has to walk.
        let storage_tile = (4, 4);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);
        sim.spawn_ground_item(storage_tile, crate::economy::core_ids::stone(), 3);

        // Solo actor — no partner spawned, hands empty (no entertainment item),
        // no adjacent ground items besides the stone in storage. Low willpower
        // pins AgentGoal::Play.
        let actor = sim.spawn_person(sim.player_faction_id, (0, 0), |b| {
            b.needs(Needs {
                hunger: 20.0,
                sleep: 20.0,
                shelter: 20.0,
                safety: 20.0,
                social: 20.0,
                reproduction: 20.0,
                willpower: 30.0, // below PLAY_THRESHOLD so Play goal naturally fires
            });
        });

        // Seven ticks: SpatialIndex / Added<Indexed> settle, FactionStorage
        // rollup populates `totals[Stone] > 0`, goal_update_system flips to
        // Play, ParallelB dispatcher argmaxes the registry, and routes to the
        // storage tile. WithdrawAndThrowStonesAsPlayMethod (UTIL_BASELINE=1.0)
        // is the only applicable Play method since no partner is in range and
        // the actor holds no entertainment item.
        sim.tick_n(7);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(actor)
            .expect("ActionQueue missing");

        match aq.current {
            Task::WithdrawMaterial { resource_id, qty } => {
                assert_eq!(
                    resource_id,
                    crate::economy::core_ids::stone(),
                    "head WithdrawMaterial should target Stone"
                );
                assert_eq!(qty, 1, "throw chain withdraws exactly one stone");
            }
            other => panic!(
                "expected Task::WithdrawMaterial{{Stone}} as head of throw chain, got {:?}",
                other
            ),
        }
        assert_eq!(
            aq.queued_len(),
            1,
            "trailing Task::PlayThrow should be queued"
        );
        match aq.peek_next() {
            Some(Task::PlayThrow) => {}
            other => panic!(
                "expected queued Task::PlayThrow as trailing leg, got {:?}",
                other
            ),
        }
        let goal = sim.app.world().get::<AgentGoal>(actor).expect("goal missing");
        assert_eq!(*goal, AgentGoal::Play, "expected goal to be Play");
    }

    /// Phase 5e-xii-c: an agent under `AgentGoal::Play` with no nearby partner
    /// and no held entertainment item but with a Luxury (entertainment_value=50)
    /// in faction storage dispatches the
    /// `[Task::WithdrawMaterial { luxury, 1 }, Task::Play { partner: None }]`
    /// chain via `htn_play_dispatch_system` + `WithdrawAndPlayWithToyMethod`.
    /// Replaces the legacy `PlayWithStoredToy` plan (PlanId 32).
    #[test]
    fn play_goal_dispatches_withdraw_then_solo_play_chain_when_only_toy_in_storage() {
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::needs::Needs;
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(143);
        sim.flat_world(2, 0, TileKind::Grass);

        // Storage tile holding Luxury — no Stone, so the throw-rocks method
        // can't fire. Toy method should win as the only applicable Play
        // method (besides PlaySolo, which requires a held / adjacent
        // entertainment item — the actor has neither).
        let storage_tile = (4, 4);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);
        sim.spawn_ground_item(storage_tile, crate::economy::core_ids::luxury(), 2);

        // Solo actor — no partner spawned, hands empty. Low willpower pins
        // AgentGoal::Play.
        let actor = sim.spawn_person(sim.player_faction_id, (0, 0), |b| {
            b.needs(Needs {
                hunger: 20.0,
                sleep: 20.0,
                shelter: 20.0,
                safety: 20.0,
                social: 20.0,
                reproduction: 20.0,
                willpower: 30.0,
            });
        });

        sim.tick_n(7);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(actor)
            .expect("ActionQueue missing");

        match aq.current {
            Task::WithdrawMaterial { resource_id, qty } => {
                assert_eq!(
                    resource_id,
                    crate::economy::core_ids::luxury(),
                    "head WithdrawMaterial should target the toy resource"
                );
                assert_eq!(qty, 1, "toy chain withdraws exactly one");
            }
            other => panic!(
                "expected Task::WithdrawMaterial{{Luxury}} as head of toy chain, got {:?}",
                other
            ),
        }
        assert_eq!(
            aq.queued_len(),
            1,
            "trailing Task::Play{{partner: None}} should be queued"
        );
        match aq.peek_next() {
            Some(Task::Play { partner: None }) => {}
            other => panic!(
                "expected queued Task::Play{{partner: None}} as trailing leg, got {:?}",
                other
            ),
        }
        let goal = sim.app.world().get::<AgentGoal>(actor).expect("goal missing");
        assert_eq!(*goal, AgentGoal::Play, "expected goal to be Play");
    }

    /// Phase 5e-xii-d: an agent under `AgentGoal::Play` with a Grain seed in
    /// faction storage and unplanted Grass tiles in range dispatches the
    /// `[Task::WithdrawMaterial { grain_seed, 1 }, Task::PlayPlant { tile }]`
    /// chain via `htn_play_dispatch_system` +
    /// `WithdrawAndPlantGrainSeedAsPlayMethod`. Replaces the legacy
    /// `PlayByPlanting` plan (PlanId 30).
    #[test]
    fn play_goal_dispatches_withdraw_then_plant_grain_seed_chain() {
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::needs::Needs;
        use crate::simulation::typed_task::{ActionQueue, Task};

        let mut sim = TestSim::new(144);
        sim.flat_world(2, 0, TileKind::Grass);

        // Storage tile holding a Grain seed — only resource available, so
        // the plant-as-play method is the only applicable Play option
        // (besides PlaySolo, which gates on held / adjacent entertainment
        // items — the actor has neither).
        let storage_tile = (4, 4);
        sim.spawn_storage_tile(sim.player_faction_id, storage_tile);
        sim.spawn_ground_item(storage_tile, crate::economy::core_ids::grain_seed(), 2);

        let actor = sim.spawn_person(sim.player_faction_id, (0, 0), |b| {
            b.needs(Needs {
                hunger: 20.0,
                sleep: 20.0,
                shelter: 20.0,
                safety: 20.0,
                social: 20.0,
                reproduction: 20.0,
                willpower: 30.0,
            });
        });

        sim.tick_n(7);

        let aq = sim
            .app
            .world()
            .get::<ActionQueue>(actor)
            .expect("ActionQueue missing");

        match aq.current {
            Task::WithdrawMaterial { resource_id, qty } => {
                assert_eq!(
                    resource_id,
                    crate::economy::core_ids::grain_seed(),
                    "head WithdrawMaterial should target grain_seed"
                );
                assert_eq!(qty, 1, "play-plant chain withdraws exactly one seed");
            }
            other => panic!(
                "expected Task::WithdrawMaterial{{GrainSeed}} as head of plant chain, got {:?}",
                other
            ),
        }
        assert_eq!(
            aq.queued_len(),
            1,
            "trailing Task::PlayPlant should be queued"
        );
        match aq.peek_next() {
            Some(Task::PlayPlant { .. }) => {}
            other => panic!(
                "expected queued Task::PlayPlant as trailing leg, got {:?}",
                other
            ),
        }
        let goal = sim.app.world().get::<AgentGoal>(actor).expect("goal missing");
        assert_eq!(*goal, AgentGoal::Play, "expected goal to be Play");
    }
}
