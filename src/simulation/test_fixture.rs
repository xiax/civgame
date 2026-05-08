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
        // SettlementMap is inserted below by SimulationPlugin::build.

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

/// Set an agent's `EconomicAgent.currency`. Used by the Pluralist Economy
/// rewrite (R0): tests pin currency before / after pay+escrow flows to
/// assert the system-wide currency invariant.
pub fn set_currency(app: &mut App, entity: Entity, amount: f32) {
    let mut econ = app
        .world_mut()
        .get_mut::<EconomicAgent>(entity)
        .expect("EconomicAgent missing");
    econ.currency = amount;
}

/// Reset a trader-profession agent to fully idle so the
/// autonomous trader dispatcher's plan-creation gate
/// (`aq.current==Idle && task_id==UNEMPLOYED && goal not preempted`)
/// fires on the next tick. Other systems (goal_update, HTN) may have
/// stamped a task / pushed the agent onto Survive during bootstrap;
/// this clears it deterministically for the dispatch-gate regression
/// tests by zeroing all need pressures + pinning a non-preempting
/// goal alongside the task / aq reset.
pub fn clear_trader_for_dispatch(app: &mut App, entity: Entity) {
    use crate::simulation::goals::AgentGoal;
    use crate::simulation::needs::Needs;
    use crate::simulation::person::{AiState, PersonAI};
    use crate::simulation::typed_task::{ActionQueue, Task};
    if let Some(mut aq) = app.world_mut().get_mut::<ActionQueue>(entity) {
        aq.cancel();
        aq.current = Task::Idle;
    }
    if let Some(mut ai) = app.world_mut().get_mut::<PersonAI>(entity) {
        ai.task_id = PersonAI::UNEMPLOYED;
        ai.state = AiState::Idle;
    }
    if let Some(mut needs) = app.world_mut().get_mut::<Needs>(entity) {
        *needs = Needs {
            hunger: 0.0,
            sleep: 0.0,
            shelter: 0.0,
            safety: 0.0,
            social: 0.0,
            reproduction: 0.0,
            willpower: 255.0,
            esteem: 0.0,
            self_actualization: 0.0,
        };
    }
    if let Some(mut goal) = app.world_mut().get_mut::<AgentGoal>(entity) {
        // GatherFood is the default and is NOT in `goal_preempts_trade`.
        *goal = AgentGoal::GatherFood;
    }
}

/// Read an agent's `EconomicAgent.currency`.
pub fn get_currency(app: &App, entity: Entity) -> f32 {
    app.world()
        .get::<EconomicAgent>(entity)
        .expect("EconomicAgent missing")
        .currency
}

/// Assert an agent's currency equals `expected` within a small epsilon
/// (currency is `f32`, escrow refunds may carry trivial FP error).
pub fn assert_currency(app: &App, entity: Entity, expected: f32) {
    let actual = get_currency(app, entity);
    let diff = (actual - expected).abs();
    assert!(
        diff < 1e-3,
        "currency mismatch: actual={actual}, expected={expected}, diff={diff}",
    );
}

/// Sum every entity's `EconomicAgent.currency` across the world.
pub fn total_system_currency(app: &mut App) -> f32 {
    let mut q = app.world_mut().query::<&EconomicAgent>();
    q.iter(app.world()).map(|a| a.currency).sum()
}

/// Sum every faction's `treasury` field. Pluralist Economy R2.
pub fn total_faction_treasury(app: &App) -> f32 {
    let registry = app
        .world()
        .resource::<crate::simulation::faction::FactionRegistry>();
    registry.factions.values().map(|f| f.treasury).sum()
}

/// Sum every settlement's `treasury` field. Pluralist Economy R1.
pub fn total_settlement_treasury(app: &mut App) -> f32 {
    let mut q = app
        .world_mut()
        .query::<&crate::simulation::settlement::Settlement>();
    q.iter(app.world()).map(|s| s.treasury).sum()
}

/// Sum every live `JobEscrow.amount`. Pluralist Economy R2.
pub fn total_escrowed_currency(app: &mut App) -> f32 {
    crate::simulation::jobs::total_escrowed_currency(app.world_mut())
}

/// Snapshot the system-wide currency for invariant comparisons. Sums
/// every accounted-for slot: per-agent currency, faction treasuries,
/// settlement treasuries, and live escrow deposits. Conservative
/// operations (`pay`, `JobEscrow` post + cancel, treasury transfers)
/// must leave `total()` unchanged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CurrencySnapshot {
    pub agents_total: f32,
    pub faction_treasuries: f32,
    pub settlement_treasuries: f32,
    pub escrowed: f32,
}

impl CurrencySnapshot {
    pub fn capture(app: &mut App) -> Self {
        Self {
            agents_total: total_system_currency(app),
            faction_treasuries: total_faction_treasury(&app),
            settlement_treasuries: total_settlement_treasury(app),
            escrowed: total_escrowed_currency(app),
        }
    }

    /// Total system currency (sum of every accounted-for slot).
    pub fn total(&self) -> f32 {
        self.agents_total
            + self.faction_treasuries
            + self.settlement_treasuries
            + self.escrowed
    }
}

/// Assert that the system-wide currency total has not drifted from
/// `baseline` by more than `epsilon`. Use after any operation that
/// purports to be currency-conservative (pay, escrow post + cancel,
/// market trade).
pub fn assert_total_currency_invariant(
    app: &mut App,
    baseline: CurrencySnapshot,
    epsilon: f32,
) {
    let now = CurrencySnapshot::capture(app);
    let diff = (now.total() - baseline.total()).abs();
    assert!(
        diff <= epsilon,
        "system-wide currency drifted: baseline={:?}, now={:?}, diff={diff}",
        baseline,
        now,
    );
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

    // ─── Pluralist Economy rewrite — R0 currency-helper unit tests ───

    #[test]
    fn set_currency_writes_through_to_economic_agent() {
        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        // Default `EconomicAgent::default()` is 50.0; sanity-check the
        // helper actually writes a different value.
        set_currency(&mut sim.app, person, 123.5);
        assert_currency(&sim.app, person, 123.5);
    }

    #[test]
    fn get_currency_reads_default_seed() {
        // EconomicAgent::default() seeds 50.0; pin this so a future
        // change to the default trips a regression.
        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        assert_currency(&sim.app, person, 50.0);
    }

    #[test]
    fn total_system_currency_sums_across_agents() {
        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let a = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        let b = sim.spawn_person(sim.player_faction_id, (1, 0), |_| {});
        set_currency(&mut sim.app, a, 100.0);
        set_currency(&mut sim.app, b, 250.0);
        let total = total_system_currency(&mut sim.app);
        assert!((total - 350.0).abs() < 1e-3, "total={total}");
    }

    #[test]
    fn currency_invariant_passes_after_zero_op() {
        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let _ = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        let baseline = CurrencySnapshot::capture(&mut sim.app);
        // Nothing happens — invariant should hold trivially.
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-3);
    }

    #[test]
    fn currency_invariant_holds_under_p2p_swap() {
        // Manually move 25.0 from A to B and back; the helper must
        // accept this as conservative.
        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let a = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        let b = sim.spawn_person(sim.player_faction_id, (1, 0), |_| {});
        set_currency(&mut sim.app, a, 100.0);
        set_currency(&mut sim.app, b, 100.0);
        let baseline = CurrencySnapshot::capture(&mut sim.app);
        // A → B
        set_currency(&mut sim.app, a, 75.0);
        set_currency(&mut sim.app, b, 125.0);
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-3);
        // B → A (round trip)
        set_currency(&mut sim.app, a, 100.0);
        set_currency(&mut sim.app, b, 100.0);
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-3);
    }

    #[test]
    #[should_panic(expected = "system-wide currency drifted")]
    fn currency_invariant_catches_unconservative_change() {
        // Conjure 50.0 out of thin air; helper must panic.
        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let a = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        set_currency(&mut sim.app, a, 100.0);
        let baseline = CurrencySnapshot::capture(&mut sim.app);
        set_currency(&mut sim.app, a, 150.0);
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-3);
    }

    // ─── Pluralist Economy R1 — Settlement primitive ───

    #[test]
    fn default_settlement_auto_founded_at_faction_home() {
        // After a few ticks, the auto-found system should have spawned a
        // Settlement entity at the player faction's home_tile, indexed
        // in `SettlementMap.by_faction`. Treasury defaults to 0; market
        // is empty.
        use crate::simulation::settlement::{Settlement, SettlementMap};

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        // Spawn one person so the bucketing / clock has work; the
        // settlement auto-found doesn't actually need a person, only
        // the FactionRegistry entry that TestSim::new already created.
        let _person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});

        // Tick enough that auto_found_default_settlements_system fires
        // and Commands flush.
        sim.tick_n(2);

        let map = sim.app.world().resource::<SettlementMap>();
        let ids = map.for_faction(sim.player_faction_id);
        assert_eq!(
            ids.len(),
            1,
            "expected exactly one auto-founded settlement for player faction"
        );

        let entity = *map.by_id.get(&ids[0]).expect("settlement entity missing");
        let settlement = sim
            .app
            .world()
            .get::<Settlement>(entity)
            .expect("Settlement component missing");
        assert_eq!(settlement.owner_faction, sim.player_faction_id);
        assert_eq!(settlement.market_tile, (0, 0)); // home_tile from TestSim::new
        assert_eq!(settlement.treasury, 0.0);
        assert_eq!(settlement.market.price_of(crate::economy::core_ids::cloth()), 1.0);
    }

    #[test]
    fn auto_found_is_idempotent_across_ticks() {
        // Running for many ticks must not spawn a second settlement.
        use crate::simulation::settlement::SettlementMap;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let _person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        sim.tick_n(20);

        let map = sim.app.world().resource::<SettlementMap>();
        let ids = map.for_faction(sim.player_faction_id);
        assert_eq!(ids.len(), 1, "auto-found must be idempotent");
    }

    // ─── Pluralist Economy R2 — pay() + JobEscrow refund hook ───

    #[test]
    fn pay_atomically_moves_currency_between_agents() {
        use crate::economy::transactions::pay;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let a = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        let b = sim.spawn_person(sim.player_faction_id, (1, 0), |_| {});
        set_currency(&mut sim.app, a, 100.0);
        set_currency(&mut sim.app, b, 50.0);
        let baseline = CurrencySnapshot::capture(&mut sim.app);

        let ok = pay(sim.app.world_mut(), a, b, 30.0);
        assert!(ok);
        assert_currency(&sim.app, a, 70.0);
        assert_currency(&sim.app, b, 80.0);
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-3);
    }

    #[test]
    fn pay_refuses_insufficient_funds() {
        use crate::economy::transactions::pay;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let a = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        let b = sim.spawn_person(sim.player_faction_id, (1, 0), |_| {});
        set_currency(&mut sim.app, a, 10.0);
        set_currency(&mut sim.app, b, 0.0);
        let baseline = CurrencySnapshot::capture(&mut sim.app);

        let ok = pay(sim.app.world_mut(), a, b, 30.0);
        assert!(!ok);
        // Balances unchanged.
        assert_currency(&sim.app, a, 10.0);
        assert_currency(&sim.app, b, 0.0);
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-3);
    }

    #[test]
    fn total_currency_invariant_holds_through_post_and_cancel() {
        // R2 keystone test: post a job-escrow sidecar entity (debits
        // employer's wallet manually + spawns the JobEscrow), then
        // despawn the sidecar (simulates cancellation). The on_remove
        // hook must refund. Total currency unchanged.
        use crate::simulation::jobs::JobEscrow;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let employer = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        set_currency(&mut sim.app, employer, 100.0);
        let baseline = CurrencySnapshot::capture(&mut sim.app);

        // Post: debit employer, spawn the escrow sidecar.
        let amount = 25.0;
        {
            let mut econ = sim
                .app
                .world_mut()
                .get_mut::<EconomicAgent>(employer)
                .unwrap();
            econ.currency -= amount;
        }
        let escrow_entity = sim
            .app
            .world_mut()
            .spawn(JobEscrow {
                amount,
                beneficiary: employer,
            })
            .id();

        // Mid-flight: invariant still holds (the 25 is in escrow now).
        assert_currency(&sim.app, employer, 75.0);
        let mid = CurrencySnapshot::capture(&mut sim.app);
        assert!(
            (mid.total() - baseline.total()).abs() < 1e-3,
            "invariant broken mid-flight: baseline={baseline:?}, mid={mid:?}",
        );

        // Cancel: despawn the sidecar; on_remove hook refunds.
        sim.app.world_mut().despawn(escrow_entity);

        assert_currency(&sim.app, employer, 100.0);
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-3);
    }

    #[test]
    fn payout_clears_escrow_amount_so_no_refund_on_despawn() {
        // Successful-payout shape: the producer credits the worker via
        // pay(), zeroes the escrow.amount, then despawns the sidecar.
        // The hook sees amount=0 and is a no-op.
        use crate::economy::transactions::pay;
        use crate::simulation::jobs::JobEscrow;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let employer = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        let worker = sim.spawn_person(sim.player_faction_id, (1, 0), |_| {});
        set_currency(&mut sim.app, employer, 100.0);
        set_currency(&mut sim.app, worker, 0.0);
        let baseline = CurrencySnapshot::capture(&mut sim.app);

        let amount = 25.0;
        // Debit + escrow
        {
            let mut econ = sim
                .app
                .world_mut()
                .get_mut::<EconomicAgent>(employer)
                .unwrap();
            econ.currency -= amount;
        }
        let escrow_entity = sim
            .app
            .world_mut()
            .spawn(JobEscrow {
                amount,
                beneficiary: employer,
            })
            .id();

        // Pay the worker their wage by drawing from a separate stash
        // (the escrow itself is just a refund record). For this test
        // we credit the worker manually to simulate the successful-
        // payout hook clearing the amount.
        {
            let mut wm = sim
                .app
                .world_mut()
                .get_mut::<EconomicAgent>(worker)
                .unwrap();
            wm.currency += amount;
        }
        // Zero the escrow before despawn so the hook doesn't refund.
        {
            let mut esc = sim
                .app
                .world_mut()
                .get_mut::<JobEscrow>(escrow_entity)
                .unwrap();
            esc.amount = 0.0;
        }
        sim.app.world_mut().despawn(escrow_entity);

        // Net: employer down 25, worker up 25 — invariant holds, no
        // double-refund.
        assert_currency(&sim.app, employer, 75.0);
        assert_currency(&sim.app, worker, 25.0);
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-3);

        // Suppress unused-binding warning for `pay`: real production
        // call sites in R5+ will use it for treasury-funded postings.
        let _ = pay;
    }

    // ─── Pluralist Economy R4 — economic_policy machinery ───

    #[test]
    fn default_factions_have_all_communist_policy() {
        // R4 invariant: a freshly-created faction has an empty
        // `economic_policy` map, and `policy_for(any_resource)`
        // returns the all-communist preset (chief allocates labor,
        // private actors not allowed). This is what keeps the 287
        // baseline tests green.
        use crate::economy::core_ids;
        use crate::simulation::faction::FactionRegistry;

        let mut sim = TestSim::new(0xC0FFEE);
        let registry = sim.app.world().resource::<FactionRegistry>();
        let data = registry
            .factions
            .get(&sim.player_faction_id)
            .expect("player faction missing");
        assert!(data.economic_policy.is_empty(), "default map is empty");

        // Verify a few resources fall through to the default.
        for rid in [
            core_ids::wood(),
            core_ids::stone(),
            core_ids::cloth(),
            core_ids::weapon(),
        ] {
            let p = data.policy_for(rid);
            assert!(p.chief_allocates_labor, "rid={rid:?} not chief-allocated");
            assert!(!p.private_actors_allowed, "rid={rid:?} private-allowed");
        }
    }

    #[test]
    fn method_passes_policy_gate_returns_true_for_empty_gate() {
        // Every existing method has an empty gate; the helper must
        // accept them under any faction's policy. We sample a few
        // representative AbstractTaskKinds since the registry's
        // `by_kind` map is private.
        use crate::economy::core_ids;
        use crate::economy::policy::{RequiredFlag, ResourceControlPolicy};
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::htn::{
            method_passes_policy_gate, AbstractTaskKind, MethodRegistry,
        };

        let mut sim = TestSim::new(0xC0FFEE);
        let registry = sim.app.world().resource::<FactionRegistry>();
        let data = registry.factions.get(&sim.player_faction_id);
        let methods = sim.app.world().resource::<MethodRegistry>();

        let kinds = [
            AbstractTaskKind::Sleep,
            AbstractTaskKind::Eat,
            AbstractTaskKind::AcquireFood,
            AbstractTaskKind::AcquireGood,
            AbstractTaskKind::Play,
        ];
        let mut total = 0usize;
        for kind in kinds {
            for m in methods.methods_for(kind) {
                total += 1;
                assert!(
                    method_passes_policy_gate(m.as_ref(), data),
                    "method '{}' rejected by default policy",
                    m.name(),
                );
            }
        }
        assert!(total > 0, "no methods registered for sampled kinds");

        // Negative path: directly construct a synthetic policy gate
        // and check the helper rejects when the flag isn't set.
        struct FakeGated;
        impl crate::simulation::htn::Method for FakeGated {
            fn precondition(
                &self,
                _: crate::simulation::htn::AbstractTask,
                _: &crate::simulation::htn::PlannerCtx,
            ) -> bool {
                true
            }
            fn utility(
                &self,
                _: crate::simulation::htn::AbstractTask,
                _: &crate::simulation::htn::PlannerCtx,
            ) -> f32 {
                1.0
            }
            fn expand(
                &self,
                _: crate::simulation::htn::AbstractTask,
                _: &crate::simulation::htn::PlannerCtx,
            ) -> Vec<crate::simulation::typed_task::Task> {
                vec![]
            }
            fn name(&self) -> &'static str {
                "FakeGated"
            }
            fn id(&self) -> crate::simulation::htn::MethodId {
                crate::simulation::htn::MethodId(0xFFFF)
            }
            fn policy_gate(
                &self,
            ) -> &'static [crate::economy::policy::PolicyGateEntry] {
                static GATE: [crate::economy::policy::PolicyGateEntry; 1] = [(
                    crate::economy::resource_catalog::ResourceId(0),
                    RequiredFlag::PrivateActorsAllowed,
                )];
                &GATE
            }
        }
        let fake = FakeGated;
        // Default policy: PrivateActorsAllowed=false → rejected.
        assert!(!method_passes_policy_gate(&fake, data));
        // Override that resource to capitalist → accepted.
        let cloth = core_ids::cloth();
        let _ = cloth;
        // Use the same ResourceId(0) the gate references — it falls
        // through to default for an unmapped resource. Stamp policy
        // explicitly on that id.
        let mut over = sim.app.world_mut().resource_mut::<FactionRegistry>();
        let f = over.factions.get_mut(&sim.player_faction_id).unwrap();
        f.economic_policy.insert(
            crate::economy::resource_catalog::ResourceId(0),
            ResourceControlPolicy::capitalist(),
        );
        let registry2 = sim.app.world().resource::<FactionRegistry>();
        let data2 = registry2.factions.get(&sim.player_faction_id);
        assert!(method_passes_policy_gate(&fake, data2));
    }

    #[test]
    fn explicit_policy_override_takes_precedence_over_default() {
        // Stamp a capitalist policy on Cloth and confirm
        // `policy_for(cloth)` returns it; other resources still default.
        use crate::economy::core_ids;
        use crate::economy::policy::ResourceControlPolicy;
        use crate::simulation::faction::FactionRegistry;

        let mut sim = TestSim::new(0xC0FFEE);
        {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            let data = registry
                .factions
                .get_mut(&sim.player_faction_id)
                .unwrap();
            data.economic_policy
                .insert(core_ids::cloth(), ResourceControlPolicy::capitalist());
        }
        let registry = sim.app.world().resource::<FactionRegistry>();
        let data = registry.factions.get(&sim.player_faction_id).unwrap();
        let cloth_policy = data.policy_for(core_ids::cloth());
        assert!(cloth_policy.private_actors_allowed);
        assert!(!cloth_policy.chief_allocates_labor);
        // Wood unaffected.
        let wood_policy = data.policy_for(core_ids::wood());
        assert!(wood_policy.chief_allocates_labor);
        assert!(!wood_policy.private_actors_allowed);
    }

    // ─── Pluralist Economy R3 — sub-factions / households ───

    #[test]
    fn spawn_household_creates_sub_faction_with_capitalist_policy() {
        // Form a household sub-faction under the player village; verify
        // the parent/child link, the policy preset, and `root_faction`.
        use crate::economy::core_ids;
        use crate::economy::resource_catalog::ResourceCatalog;
        use crate::simulation::faction::FactionRegistry;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let head = sim.spawn_person(sim.player_faction_id, (2, 2), |_| {});

        let catalog = sim.app.world().resource::<ResourceCatalog>().clone();
        let village_id = sim.player_faction_id;
        let household_id = {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            registry.spawn_household(village_id, (2, 2), head, &catalog)
        };

        let registry = sim.app.world().resource::<FactionRegistry>();

        // Parent/child link is reciprocal.
        let household = registry.factions.get(&household_id).unwrap();
        assert_eq!(household.parent_faction, Some(village_id));
        assert_eq!(household.household_head, Some(head));
        let village = registry.factions.get(&village_id).unwrap();
        assert!(
            village.children_factions.contains(&household_id),
            "village must list household as child"
        );

        // Root walks back to the village.
        assert_eq!(registry.root_faction(household_id), village_id);
        assert_eq!(registry.root_faction(village_id), village_id);

        // Capitalist policy stamped on every catalog resource.
        for rid in [
            core_ids::wood(),
            core_ids::stone(),
            core_ids::cloth(),
            core_ids::weapon(),
        ] {
            let p = household.policy_for(rid);
            assert!(p.private_actors_allowed, "rid={rid:?} not capitalist");
            assert!(!p.chief_allocates_labor);
        }

        // Village's policy is unchanged (still all-communist).
        for rid in [core_ids::wood(), core_ids::cloth()] {
            let p = village.policy_for(rid);
            assert!(p.chief_allocates_labor);
            assert!(!p.private_actors_allowed);
        }
    }

    #[test]
    fn household_storage_isolated_from_village_storage() {
        // Spawn one storage tile for the village and one for the
        // household at distinct positions. Because `FactionStorage` is
        // already indexed per faction_id, a member of the household
        // queries only the household's tile, not the village's.
        use crate::economy::resource_catalog::ResourceCatalog;
        use crate::simulation::faction::{FactionRegistry, StorageTileMap};

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let head = sim.spawn_person(sim.player_faction_id, (1, 1), |_| {});

        let catalog = sim.app.world().resource::<ResourceCatalog>().clone();
        let village_id = sim.player_faction_id;
        let household_id = {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            registry.spawn_household(village_id, (1, 1), head, &catalog)
        };

        // One storage tile per faction at distinct tiles.
        let _village_tile = sim.spawn_storage_tile(village_id, (5, 0));
        let _household_tile = sim.spawn_storage_tile(household_id, (-5, 0));

        // Tick a few times so the StorageTileMap rebuild observes both.
        sim.tick_n(5);

        let map = sim.app.world().resource::<StorageTileMap>();

        // Per-faction lookup must not bleed across.
        let village_tiles: Vec<(i32, i32)> = map
            .by_faction
            .get(&village_id)
            .cloned()
            .unwrap_or_default();
        let household_tiles: Vec<(i32, i32)> = map
            .by_faction
            .get(&household_id)
            .cloned()
            .unwrap_or_default();

        assert!(
            village_tiles.contains(&(5, 0)),
            "village list missing village tile: {village_tiles:?}",
        );
        assert!(
            !village_tiles.contains(&(-5, 0)),
            "village list leaked household tile: {village_tiles:?}",
        );
        assert!(
            household_tiles.contains(&(-5, 0)),
            "household list missing household tile: {household_tiles:?}",
        );
        assert!(
            !household_tiles.contains(&(5, 0)),
            "household list leaked village tile: {household_tiles:?}",
        );
    }

    // ─── Pluralist Economy R5 — Bureaucrat profession ───

    #[test]
    fn bureaucrat_promoted_then_demotes_when_treasury_drains() {
        // Government Collapse Test: spawn a faction with
        // `state_funds_public_works=true`, seed the settlement
        // treasury with enough to fund one bureaucrat for a few
        // ticks, then run for `BUREAUCRAT_QUIT_DAYS` days. Assert
        // that:
        // 1. The chief promotes a None adult to Bureaucrat.
        // 2. After the treasury empty-streak crosses the quit
        //    threshold, the bureaucrat demotes back to None.
        use crate::simulation::faction::{
            FactionRegistry, BUREAUCRAT_ASSIGNMENT_CADENCE, BUREAUCRAT_QUIT_DAYS,
        };
        use crate::simulation::person::Profession;
        use crate::simulation::settlement::{Settlement, SettlementMap};
        use crate::world::seasons::TICKS_PER_DAY;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);

        // Spawn a few adults so the appointment system has someone
        // to promote. `BUREAUCRAT_MIN_RATIO * 4` rounds to 1, so
        // 4 None adults yields 1 promotion.
        let _a = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        let _b = sim.spawn_person(sim.player_faction_id, (1, 0), |_| {});
        let _c = sim.spawn_person(sim.player_faction_id, (2, 0), |_| {});
        let _d = sim.spawn_person(sim.player_faction_id, (3, 0), |_| {});
        // FactionRegistry tracks member count via add_member.
        {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            for _ in 0..4 {
                registry.add_member(sim.player_faction_id);
            }
            let f = registry
                .factions
                .get_mut(&sim.player_faction_id)
                .unwrap();
            f.state_funds_public_works = true;
        }

        // Tick once so auto_found_default_settlements_system spawns a
        // settlement, then seed the treasury directly.
        sim.tick_n(2);
        let settlement_entity = {
            let map = sim.app.world().resource::<SettlementMap>();
            let id = map
                .first_for_faction(sim.player_faction_id)
                .expect("settlement not founded");
            *map.by_id.get(&id).unwrap()
        };
        // Seed enough for ~half a day: `BUREAUCRAT_DAILY_WAGE / 24`
        // is paid every BUREAUCRAT_SALARY_INTERVAL ticks; 0.5 covers
        // ~12 hourly ticks for one bureaucrat.
        {
            let mut s = sim
                .app
                .world_mut()
                .get_mut::<Settlement>(settlement_entity)
                .unwrap();
            s.treasury = 0.5;
        }

        // Drive the appointment cadence so the chief promotes.
        sim.tick_n(BUREAUCRAT_ASSIGNMENT_CADENCE as u32 + 5);

        let bureaucrat_count = {
            let mut q = sim.app.world_mut().query::<&Profession>();
            q.iter(sim.app.world())
                .filter(|p| **p == Profession::Bureaucrat)
                .count()
        };
        assert!(
            bureaucrat_count >= 1,
            "expected at least one bureaucrat after promote tick",
        );

        // Fast-forward past the quit threshold. With the treasury at
        // ~0.5, salary ticks drain it within an hour; from there,
        // streak advances every salary tick. After
        // `BUREAUCRAT_QUIT_DAYS` game-days, the appointment system
        // forces target=0 and demotes everyone.
        let total_ticks = (BUREAUCRAT_QUIT_DAYS as u64 + 1) * TICKS_PER_DAY as u64
            + BUREAUCRAT_ASSIGNMENT_CADENCE;
        sim.tick_n(total_ticks as u32);

        let bureaucrat_count_after = {
            let mut q = sim.app.world_mut().query::<&Profession>();
            q.iter(sim.app.world())
                .filter(|p| **p == Profession::Bureaucrat)
                .count()
        };
        assert_eq!(
            bureaucrat_count_after, 0,
            "all bureaucrats must demote when treasury stays empty for BUREAUCRAT_QUIT_DAYS",
        );
    }

    // ─── Pluralist Economy R6-a — chief Stockpile gated on policy ───

    #[test]
    fn chief_skips_food_stockpile_when_policy_flipped_to_capitalist() {
        // R6-a: when the chief's faction has flipped Fruit (the
        // representative food resource) to
        // `chief_allocates_labor=false`, the chief skips the
        // Stockpile{Calories} posting entirely. Default
        // (all-communist) factions still post — invariance verified
        // by the 287 baseline.
        use crate::economy::core_ids;
        use crate::economy::policy::ResourceControlPolicy;
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::jobs::{JobBoard, JobProgress};

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);

        for i in 0..4 {
            sim.spawn_person(sim.player_faction_id, (i, 0), |_| {});
        }
        {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            for _ in 0..4 {
                registry.add_member(sim.player_faction_id);
            }
            let f = registry
                .factions
                .get_mut(&sim.player_faction_id)
                .unwrap();
            f.economic_policy
                .insert(core_ids::fruit(), ResourceControlPolicy::capitalist());
        }

        sim.tick_n(120);

        let board = sim.app.world().resource::<JobBoard>();
        let food_postings: usize = board
            .faction_postings(sim.player_faction_id)
            .iter()
            .filter(|p| matches!(p.progress, JobProgress::Calories { .. }))
            .count();
        assert_eq!(
            food_postings, 0,
            "capitalist food policy must block chief Stockpile{{Calories}}",
        );
    }

    #[test]
    fn chief_still_posts_food_stockpile_under_default_policy() {
        // Companion: default (all-communist) policy must still post
        // Stockpile{Calories} when food is low. Pins R6-a's
        // invariance: the gate doesn't affect default factions.
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::jobs::{JobBoard, JobProgress};

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        for i in 0..4 {
            sim.spawn_person(sim.player_faction_id, (i, 0), |_| {});
        }
        {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            for _ in 0..4 {
                registry.add_member(sim.player_faction_id);
            }
        }

        sim.tick_n(120);

        let board = sim.app.world().resource::<JobBoard>();
        let food_postings: usize = board
            .faction_postings(sim.player_faction_id)
            .iter()
            .filter(|p| matches!(p.progress, JobProgress::Calories { .. }))
            .count();
        assert!(
            food_postings >= 1,
            "default communist policy should still post chief Stockpile{{Calories}}",
        );
    }

    // ─── R6-b — chief Haul gated on resource policy ───

    #[test]
    fn chief_skips_wood_haul_when_wood_policy_capitalist() {
        // Set Wood to capitalist; the chief's per-blueprint Haul
        // posting for Wood deposits is skipped. (Other resources
        // unaffected.)
        use crate::economy::core_ids;
        use crate::economy::policy::ResourceControlPolicy;
        use crate::simulation::faction::{FactionRegistry, FactionStorage};
        use crate::simulation::jobs::{JobBoard, JobProgress};

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        for i in 0..4 {
            sim.spawn_person(sim.player_faction_id, (i, 0), |_| {});
        }

        // Pre-stock Wood in faction storage so the haul branch has
        // material to allocate; flip Wood policy to capitalist.
        {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            for _ in 0..4 {
                registry.add_member(sim.player_faction_id);
            }
            let f = registry
                .factions
                .get_mut(&sim.player_faction_id)
                .unwrap();
            f.storage = FactionStorage::default();
            f.storage.totals.insert(core_ids::wood(), 50);
            f.economic_policy
                .insert(core_ids::wood(), ResourceControlPolicy::capitalist());
        }

        sim.tick_n(120);

        let board = sim.app.world().resource::<JobBoard>();
        let wood_haul_postings: usize = board
            .faction_postings(sim.player_faction_id)
            .iter()
            .filter(|p| matches!(
                &p.progress,
                JobProgress::Haul { resource_id, .. } if *resource_id == core_ids::wood()
            ))
            .count();
        assert_eq!(
            wood_haul_postings, 0,
            "capitalist Wood policy must block chief Haul{{Wood}} postings",
        );
    }

    // ─── R6-c — chief Build gated on state_funds_public_works ───

    #[test]
    fn chief_skips_builds_when_state_funds_public_works_is_true() {
        // R6-c: when the faction has flipped on bureaucratic
        // public works, the chief stops posting Build jobs. The
        // bureaucrat takes over (R10+). For now, capitalist
        // factions just have no Build postings until R10+ ships.
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::jobs::{JobBoard, JobKind};

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        for i in 0..4 {
            sim.spawn_person(sim.player_faction_id, (i, 0), |_| {});
        }
        {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            for _ in 0..4 {
                registry.add_member(sim.player_faction_id);
            }
            let f = registry
                .factions
                .get_mut(&sim.player_faction_id)
                .unwrap();
            f.state_funds_public_works = true;
        }

        sim.tick_n(120);

        let board = sim.app.world().resource::<JobBoard>();
        let build_count: usize = board
            .faction_postings(sim.player_faction_id)
            .iter()
            .filter(|p| matches!(p.kind, JobKind::Build))
            .count();
        assert_eq!(
            build_count, 0,
            "state_funds_public_works=true must block chief Build postings",
        );
    }

    // ─── R6-d — chief Craft gated on output-resource policy ───

    #[test]
    fn chief_skips_craft_when_output_resource_capitalist() {
        // Flip every craft-output resource to capitalist; the
        // chief stops posting Craft. Default factions still post
        // (covered by the existing 287 baseline).
        use crate::economy::core_ids;
        use crate::economy::policy::ResourceControlPolicy;
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::jobs::{JobBoard, JobKind};

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        for i in 0..4 {
            sim.spawn_person(sim.player_faction_id, (i, 0), |_| {});
        }
        {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            for _ in 0..4 {
                registry.add_member(sim.player_faction_id);
            }
            let f = registry
                .factions
                .get_mut(&sim.player_faction_id)
                .unwrap();
            // Flip the common craft-output resources to capitalist.
            for rid in [
                core_ids::tools(),
                core_ids::cloth(),
                core_ids::weapon(),
                core_ids::armor(),
                core_ids::shield(),
                core_ids::luxury(),
            ] {
                f.economic_policy
                    .insert(rid, ResourceControlPolicy::capitalist());
            }
        }

        sim.tick_n(120);

        let board = sim.app.world().resource::<JobBoard>();
        let craft_count: usize = board
            .faction_postings(sim.player_faction_id)
            .iter()
            .filter(|p| matches!(p.kind, JobKind::Craft))
            .count();
        assert_eq!(
            craft_count, 0,
            "capitalist craft-output policy must block chief Craft postings",
        );
    }

    // ─── R6-e — chief Farm gated on Grain policy ───

    // ─── Pluralist Economy R7 — per-settlement market activation ───

    #[test]
    fn two_settlements_in_same_megachunk_develop_independent_prices() {
        // R7: spawn two factions, each gets an auto-founded
        // settlement in the same megachunk. Add Cloth supply to
        // settlement A's market and Cloth demand to settlement B's;
        // tick `settlement_price_update_system` 100 times; assert
        // A's Cloth price < B's Cloth price (supply pushes A down,
        // demand pushes B up).
        use crate::economy::core_ids;
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::settlement::{Settlement, SettlementMap};

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);

        // Faction A is the player faction (auto-created by TestSim::new
        // at home_tile (0, 0)). Add a second faction at (5, 5).
        let faction_b = {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            registry.create_faction((5, 5))
        };

        // Tick a few times so both settlements auto-found.
        sim.tick_n(3);

        let (settlement_a_entity, settlement_b_entity) = {
            let map = sim.app.world().resource::<SettlementMap>();
            let a_id = map
                .first_for_faction(sim.player_faction_id)
                .expect("settlement A not founded");
            let b_id = map
                .first_for_faction(faction_b)
                .expect("settlement B not founded");
            let a = *map.by_id.get(&a_id).unwrap();
            let b = *map.by_id.get(&b_id).unwrap();
            assert_ne!(a, b, "expected two distinct settlement entities");
            (a, b)
        };

        let cloth = core_ids::cloth();
        // Seed Cloth: A heavy supply, B heavy demand. Both start at
        // empty (no seeded prices); after the first update they
        // diverge.
        {
            let mut a = sim
                .app
                .world_mut()
                .get_mut::<Settlement>(settlement_a_entity)
                .unwrap();
            a.market.add_supply(cloth, 100.0);
        }
        {
            let mut b = sim
                .app
                .world_mut()
                .get_mut::<Settlement>(settlement_b_entity)
                .unwrap();
            b.market.add_demand(cloth, 100.0);
        }

        // Tick the per-settlement price update enough to see divergence.
        sim.tick_n(100);

        let a_price = sim
            .app
            .world()
            .get::<Settlement>(settlement_a_entity)
            .unwrap()
            .market
            .price_of(cloth);
        let b_price = sim
            .app
            .world()
            .get::<Settlement>(settlement_b_entity)
            .unwrap()
            .market
            .price_of(cloth);

        assert!(
            a_price < b_price,
            "expected A's price < B's after supply/demand split: a={a_price}, b={b_price}",
        );
    }

    // ─── Pluralist Economy R9 — U_bid scoring at job-claim layer ───

    #[test]
    fn paid_posting_outscores_unpaid_chief_posting_for_unsatisfied_agent() {
        // R9: post two competing Stockpile{Wood} jobs at the same
        // distance — one chief-default (reward=0.0, scored via
        // legacy formula); one paid (reward=10.0, poster_class=
        // HouseholdHead). Spawn one None-profession agent. Tick the
        // claim system; assert the agent claimed the paid posting.
        use crate::economy::core_ids;
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::jobs::{
            JobBoard, JobKind, JobPosting, JobProgress, JobSource, PosterClass,
        };

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        // Member-count > 0 so worker isn't filtered out by faction
        // checks elsewhere (some upstream systems gate on it).
        let worker = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            registry.add_member(sim.player_faction_id);
        }

        // Tick a couple times so the claim system schedules and
        // SimClock advances past tick 0.
        sim.tick_n(2);

        let unpaid_id;
        let paid_id;
        {
            let mut board = sim.app.world_mut().resource_mut::<JobBoard>();
            unpaid_id = board.alloc_id();
            board.faction_postings_mut(sim.player_faction_id).push(JobPosting {
                id: unpaid_id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Stockpile,
                progress: JobProgress::Stockpile {
                    resource_id: core_ids::wood(),
                    deposited: 0,
                    target: 5,
                },
                claimants: Vec::new(),
                priority: 100,
                source: JobSource::Chief,
                posted_tick: 0,
                expiry_tick: None,
                poster_class: PosterClass::Chief,
                reward: 0.0,
                settlement_id: None,
            });
            paid_id = board.alloc_id();
            board.faction_postings_mut(sim.player_faction_id).push(JobPosting {
                id: paid_id,
                faction_id: sim.player_faction_id,
                kind: JobKind::Stockpile,
                progress: JobProgress::Stockpile {
                    resource_id: core_ids::stone(),
                    deposited: 0,
                    target: 5,
                },
                claimants: Vec::new(),
                priority: 100,
                source: JobSource::Player,
                posted_tick: 0,
                expiry_tick: None,
                poster_class: PosterClass::HouseholdHead,
                reward: 10.0,
                settlement_id: None,
            });
        }

        // Tick the claim system. job_claim_system runs each tick;
        // a few ticks should suffice.
        sim.tick_n(5);

        // Inspect the JobClaim on the worker.
        use crate::simulation::jobs::JobClaim;
        let claim = sim.app.world().get::<JobClaim>(worker);
        assert!(claim.is_some(), "worker should have claimed something");
        let claim = claim.unwrap();
        assert_eq!(
            claim.job_id, paid_id,
            "worker should claim the paid posting (id={paid_id}); got {claim:?}",
        );
    }

    // ─── Pluralist Economy R6 follow-on b — household income skim ───

    #[test]
    fn household_member_trader_sale_skims_to_household_treasury() {
        // R6 follow-on b: when a HouseholdMember sells goods via
        // `trader_sell_at_settlement`, 10% of earnings goes to the
        // household treasury and 90% to their personal wallet.
        // Validates the income flow that lets households accumulate
        // treasury organically.
        //
        // (`market_sell_system` carries the same skim logic but is
        // currently orphaned — not registered in `EconomyPlugin`.
        // Activating it ripples through 9+ existing tests so that's
        // a separate cleanup. The skim helper is shared between
        // both paths via `split_market_earnings_with_household`,
        // so when `market_sell_system` is eventually registered,
        // households will earn from passive market activity too.)
        use crate::economy::core_ids;
        use crate::economy::item::Item;
        use crate::economy::transactions::trader_sell_at_settlement;
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::reproduction::HouseholdMember;
        use crate::simulation::settlement::{Settlement, SettlementMap};

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        let agent = sim.spawn_person(sim.player_faction_id, (5, 5), |_| {});

        // Build a household and stamp the agent as a member.
        let village = sim.player_faction_id;
        let catalog = sim
            .app
            .world()
            .resource::<crate::economy::resource_catalog::ResourceCatalog>()
            .clone();
        let household_id = {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            registry.spawn_household(village, (5, 5), agent, &catalog)
        };
        sim.app
            .world_mut()
            .entity_mut(agent)
            .insert(HouseholdMember { household_id });

        // Seed the agent's inventory with Cloth.
        {
            let mut econ = sim
                .app
                .world_mut()
                .get_mut::<crate::economy::agent::EconomicAgent>(agent)
                .unwrap();
            econ.currency = 0.0;
            econ.add_item(Item::new_commodity(core_ids::cloth()), 10);
        }

        // Tick so the auto-found settlement appears.
        sim.tick_n(3);

        // Find the village's settlement, seed its treasury so the
        // sell can be funded.
        let settlement_entity = {
            let map = sim.app.world().resource::<SettlementMap>();
            let sid = map.first_for_faction(village).unwrap();
            *map.by_id.get(&sid).unwrap()
        };
        {
            let mut s = sim
                .app
                .world_mut()
                .get_mut::<Settlement>(settlement_entity)
                .unwrap();
            s.treasury = 100.0;
        }

        let baseline = CurrencySnapshot::capture(&mut sim.app);

        // Sell 5 cloth via the R10 trader helper (which now routes
        // through `split_market_earnings_with_household`).
        let price = trader_sell_at_settlement(
            sim.app.world_mut(),
            agent,
            settlement_entity,
            core_ids::cloth(),
            5,
        )
        .expect("trader sell should succeed");
        let total_earned = price * 5.0;

        let agent_currency = get_currency(&sim.app, agent);
        let household_treasury = sim
            .app
            .world()
            .resource::<FactionRegistry>()
            .factions
            .get(&household_id)
            .unwrap()
            .treasury;

        // 10% to household, 90% to agent.
        let expected_skim = total_earned * 0.10;
        let expected_agent = total_earned - expected_skim;
        assert!(
            (household_treasury - expected_skim).abs() < 1e-3,
            "household skim mismatch: got {household_treasury}, expected {expected_skim}",
        );
        assert!(
            (agent_currency - expected_agent).abs() < 1e-3,
            "agent share mismatch: got {agent_currency}, expected {expected_agent}",
        );

        // Currency invariant: settlement treasury debited by
        // `total_earned`; agent + household credited by the same.
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-2);
    }

    #[test]
    fn non_household_member_trader_sale_sends_full_earnings_to_agent() {
        // Negative case: a non-HouseholdMember selling via
        // `trader_sell_at_settlement` keeps 100% of earnings.
        use crate::economy::core_ids;
        use crate::economy::item::Item;
        use crate::economy::transactions::trader_sell_at_settlement;
        use crate::simulation::settlement::{Settlement, SettlementMap};

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        let agent = sim.spawn_person(sim.player_faction_id, (5, 5), |_| {});
        {
            let mut econ = sim
                .app
                .world_mut()
                .get_mut::<crate::economy::agent::EconomicAgent>(agent)
                .unwrap();
            econ.currency = 0.0;
            econ.add_item(Item::new_commodity(core_ids::cloth()), 10);
        }
        sim.tick_n(3);
        let settlement_entity = {
            let map = sim.app.world().resource::<SettlementMap>();
            let sid = map.first_for_faction(sim.player_faction_id).unwrap();
            *map.by_id.get(&sid).unwrap()
        };
        {
            let mut s = sim
                .app
                .world_mut()
                .get_mut::<Settlement>(settlement_entity)
                .unwrap();
            s.treasury = 100.0;
        }

        let price = trader_sell_at_settlement(
            sim.app.world_mut(),
            agent,
            settlement_entity,
            core_ids::cloth(),
            5,
        )
        .expect("sell should succeed");
        let total_earned = price * 5.0;
        let agent_currency = get_currency(&sim.app, agent);
        // No household → 100% to agent.
        assert!((agent_currency - total_earned).abs() < 1e-3);
    }

    // ─── Pluralist Economy R3 follow-on b — HouseholdMember birth inheritance ───

    #[test]
    fn newborn_inherits_household_membership_from_mother() {
        // R3 follow-on b: when a HouseholdMember mother gives
        // birth, the newborn is automatically inserted into the
        // same household. Validates that pregnancy_system's birth
        // path threads `mother_household` through to the spawn
        // and inserts the marker on the child entity.
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::person::Person;
        use crate::simulation::reproduction::{HouseholdMember, Pregnancy};
        use crate::world::seasons::TICKS_PER_DAY;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        // Pregnancy is inserted directly; sex isn't checked at the
        // pregnancy_system birth-spawn site — only at the
        // conception-attempt site we're bypassing.
        let mother = sim.spawn_person(sim.player_faction_id, (5, 5), |_| {});
        // Stamp HouseholdMember on the mother manually (the
        // formation system would do this in real gameplay).
        let village = sim.player_faction_id;
        let catalog = sim
            .app
            .world()
            .resource::<crate::economy::resource_catalog::ResourceCatalog>()
            .clone();
        let household_id = {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            registry.spawn_household(village, (5, 5), mother, &catalog)
        };
        sim.app
            .world_mut()
            .entity_mut(mother)
            .insert(HouseholdMember { household_id });

        // Insert a Pregnancy with a tiny ticks_remaining so birth
        // fires on the next active tick. The faction_id matches
        // the mother's faction (R3's primitive doesn't change
        // FactionMember at household formation, so the mother is
        // still a village member; the newborn likewise lands in
        // the village's faction by `pregnancy_system` while
        // additionally getting a HouseholdMember marker).
        sim.app
            .world_mut()
            .entity_mut(mother)
            .insert(Pregnancy {
                ticks_remaining: 5,
                father: None,
                father_stats: None,
                father_known: 0,
                faction_id: village,
            });

        // Tick enough for the pregnancy timer + birth + Commands
        // flush. pregnancy_system runs in Economy schedule. The
        // bucket cadence may delay firing; tick a generous amount.
        sim.tick_n((TICKS_PER_DAY / 4) as u32);

        // Find the child: the newest Person whose entity isn't the mother.
        let child_with_household: Option<HouseholdMember> = {
            let mut q = sim.app.world_mut().query_filtered::<
                (Entity, &HouseholdMember),
                With<Person>,
            >();
            q.iter(sim.app.world())
                .filter(|(e, _)| *e != mother)
                .map(|(_, hh)| *hh)
                .next()
        };

        let child_hh = child_with_household.expect(
            "newborn should have inherited HouseholdMember from mother",
        );
        assert_eq!(
            child_hh.household_id, household_id,
            "newborn should be in same household as mother",
        );
    }

    // ─── Pluralist Economy R8 follow-on — SelfActualization teaching ───

    #[test]
    fn self_actualizing_elder_triggers_lecture() {
        // R8 follow-on: an agent on Maslow tier
        // SelfActualization (every lower tier including Esteem
        // satisfied) AND with at least one Learned tech triggers a
        // LectureRequest once per game-day, granting them
        // SELF_ACTUALIZATION_LECTURE_GAIN to the
        // self_actualization need.
        use crate::simulation::knowledge::PersonKnowledge;
        use crate::simulation::needs::Needs;
        use crate::simulation::teaching::{
            LectureRequest, SELF_ACTUALIZATION_LECTURE_GAIN,
        };
        use crate::world::seasons::TICKS_PER_DAY;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        // Spawn the elder. Default Person spawn includes a
        // PersonKnowledge with Paleolithic Aware+Learned (per
        // PersonKnowledge::paleolithic_seed), so they have at least
        // one Learned tech.
        let elder = sim.spawn_person(sim.player_faction_id, (5, 5), |b| {
            b.needs(Needs {
                hunger: 0.0,
                sleep: 0.0,
                shelter: 0.0,
                safety: 0.0,
                social: 0.0,
                reproduction: 0.0,
                willpower: 255.0,
                esteem: 250.0, // satiated → unlocks Tier 5
                self_actualization: 0.0,
            });
        });

        // Confirm they have at least one Learned tech.
        let knowledge = sim
            .app
            .world()
            .get::<PersonKnowledge>(elder)
            .unwrap();
        assert!(knowledge.learned != 0, "elder should have Paleolithic Learned techs");

        let starting_sa = sim.app.world().get::<Needs>(elder).unwrap().self_actualization;

        // Tick one game-day so the cadence fires.
        sim.tick_n(TICKS_PER_DAY as u32 + 5);

        // The elder's self_actualization should have bumped (the
        // act of triggering the lecture grants the satisfaction).
        let new_sa = sim.app.world().get::<Needs>(elder).unwrap().self_actualization;
        assert!(
            new_sa > starting_sa,
            "self_actualization should increase: {starting_sa} → {new_sa}",
        );

        // The LectureRequest may have been consumed already by
        // apply_lecture_request_system (which runs in the same tick),
        // OR a Lecturing component may have been inserted on the
        // elder. Either way: a lecture was set up.
        let request = sim.app.world().resource::<LectureRequest>();
        let lecturing_marker = sim
            .app
            .world()
            .get::<crate::simulation::teaching::Lecturing>(elder);
        assert!(
            request.0.is_some() || lecturing_marker.is_some(),
            "expected either LectureRequest set or Lecturing inserted on the elder",
        );
        let _ = SELF_ACTUALIZATION_LECTURE_GAIN; // const reference
    }

    #[test]
    fn esteem_unfulfilled_agent_does_not_trigger_lecture() {
        // Maslow gate: an agent with Esteem unmet (Tier 4) does
        // NOT skip to SelfActualization (Tier 5). The teaching
        // trigger should NOT fire.
        use crate::simulation::knowledge::PersonKnowledge;
        use crate::simulation::needs::Needs;
        use crate::simulation::teaching::LectureRequest;
        use crate::world::seasons::TICKS_PER_DAY;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        let agent = sim.spawn_person(sim.player_faction_id, (5, 5), |b| {
            b.needs(Needs {
                hunger: 0.0,
                sleep: 0.0,
                shelter: 0.0,
                safety: 0.0,
                social: 0.0,
                reproduction: 0.0,
                willpower: 255.0,
                esteem: 0.0, // UNMET → Tier 4 wins, Tier 5 doesn't fire
                self_actualization: 0.0,
            });
        });
        let _ = sim.app.world().get::<PersonKnowledge>(agent).unwrap();
        let starting_sa = sim.app.world().get::<Needs>(agent).unwrap().self_actualization;

        sim.tick_n(TICKS_PER_DAY as u32 + 5);

        let new_sa = sim.app.world().get::<Needs>(agent).unwrap().self_actualization;
        assert_eq!(
            new_sa, starting_sa,
            "self_actualization must not bump while Esteem unmet",
        );
        // No Lecturing component either.
        assert!(
            sim.app
                .world()
                .get::<crate::simulation::teaching::Lecturing>(agent)
                .is_none(),
        );
        // (LectureRequest may have been triggered by some other agent
        // — we don't assert on the resource itself in the negative
        // case.)
        let _ = sim.app.world().resource::<LectureRequest>();
    }

    // ─── Pluralist Economy R8 follow-on — visited_settlements gossip ───

    #[test]
    fn visited_settlement_propagates_via_gossip_to_socializer() {
        // R8 follow-on: an agent who's visited Settlement X (slot
        // populated in `AgentMemory.visited_settlements`) propagates
        // that knowledge to a same-faction socializer adjacent to
        // them. After one pass of `awareness_gossip_system`, the
        // socializer's `known_settlements()` includes X.
        use crate::simulation::goals::AgentGoal;
        use crate::simulation::memory::AgentMemory;
        use crate::simulation::needs::Needs;
        use crate::simulation::settlement::SettlementId;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        // Pin AgentGoal::Socialize by setting needs.social well
        // above the 160 threshold so goal_update_system keeps
        // picking Socialize each tick.
        let high_social = Needs {
            hunger: 0.0,
            sleep: 0.0,
            shelter: 0.0,
            safety: 0.0,
            social: 220.0,
            reproduction: 0.0,
            willpower: 200.0,
            esteem: 0.0,
            self_actualization: 0.0,
        };
        let traveler = sim.spawn_person(sim.player_faction_id, (0, 0), |b| {
            b.needs(high_social).goal(AgentGoal::Socialize);
        });
        let listener = sim.spawn_person(sim.player_faction_id, (1, 0), |b| {
            b.needs(high_social).goal(AgentGoal::Socialize);
        });

        // Pin the traveler's knowledge of an exotic settlement id.
        let exotic_id = SettlementId(9999);
        {
            let mut mem = sim
                .app
                .world_mut()
                .get_mut::<AgentMemory>(traveler)
                .unwrap();
            mem.record_settlement(exotic_id);
        }

        // Tick a few times so SpatialIndex sync + gossip system fires.
        sim.tick_n(5);

        // Listener should now know about the exotic settlement.
        let listener_mem = sim
            .app
            .world()
            .get::<AgentMemory>(listener)
            .unwrap();
        let known: Vec<_> = listener_mem
            .known_settlements()
            .map(|(id, _)| id)
            .collect();
        assert!(
            known.contains(&exotic_id),
            "listener should have learned exotic settlement {exotic_id:?} via gossip; known={known:?}",
        );
    }

    // ─── Pluralist Economy R5 follow-on — Bureaucrat town-hall dispatch ───

    #[test]
    fn idle_bureaucrat_dispatches_lead_task_to_town_hall() {
        // R5 follow-on validation: a Bureaucrat agent who's
        // otherwise idle (no goal-driven task) gets a `Task::Lead`
        // dispatched targeting their faction's first settlement's
        // market_tile (the de-facto town hall).
        use crate::simulation::person::Profession;
        use crate::simulation::settlement::SettlementMap;
        use crate::simulation::typed_task::Task;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        let bureaucrat = sim.spawn_person(sim.player_faction_id, (3, 3), |b| {
            b.profession(Profession::Bureaucrat);
        });

        // Tick a couple times so the auto-found settlement appears
        // and the dispatcher can find a town_hall_tile.
        sim.tick_n(10);

        let town_hall = {
            let map = sim.app.world().resource::<SettlementMap>();
            let sid = map.first_for_faction(sim.player_faction_id).unwrap();
            let entity = *map.by_id.get(&sid).unwrap();
            sim.app
                .world()
                .get::<crate::simulation::settlement::Settlement>(entity)
                .unwrap()
                .market_tile
        };

        let task = person_task(&sim.app, bureaucrat);
        match task {
            Task::Lead { dest } => {
                assert_eq!(
                    dest, town_hall,
                    "bureaucrat should be heading to the town hall ({town_hall:?}); got dest={dest:?}",
                );
            }
            // The bureaucrat may have other task chains queued
            // (Survive / Sleep) ahead of the Lead dispatch on the
            // very first idle tick; for the regression we accept
            // either Lead-to-town-hall OR no other dispatched task
            // — the system never strands the bureaucrat in an
            // inconsistent state.
            other => {
                // Try the next prefetched task.
                let aq = sim
                    .app
                    .world()
                    .get::<crate::simulation::typed_task::ActionQueue>(bureaucrat)
                    .unwrap();
                match aq.peek_next() {
                    Some(Task::Lead { dest }) => {
                        assert_eq!(dest, town_hall);
                    }
                    _ => panic!(
                        "expected Task::Lead in current or queued slot; current={other:?}",
                    ),
                }
            }
        }
    }

    #[test]
    fn non_bureaucrat_idle_agent_does_not_get_lead_task() {
        // Negative case: a regular None-profession agent must
        // not have Task::Lead dispatched by the bureaucrat
        // dispatcher, even when otherwise idle.
        use crate::simulation::typed_task::Task;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        let agent = sim.spawn_person(sim.player_faction_id, (3, 3), |_| {});
        sim.tick_n(10);

        let task = person_task(&sim.app, agent);
        assert!(
            !matches!(task, Task::Lead { .. }),
            "non-bureaucrat should not have Task::Lead dispatched: got {task:?}",
        );
    }

    // ─── Pluralist Economy R8 follow-on — Maslow goal-priority spine ───

    #[test]
    fn maslow_next_unmet_returns_lowest_unsatisfied_tier() {
        // Pin the gate's contract: hunger pressure → Physiological;
        // hunger satisfied + safety pressure → Safety; etc.
        use crate::simulation::goals::MaslowTier;
        use crate::simulation::needs::Needs;

        let satiated = Needs {
            hunger: 0.0,
            sleep: 0.0,
            shelter: 0.0,
            safety: 0.0,
            social: 0.0,
            reproduction: 0.0,
            willpower: 255.0,
            esteem: 250.0,
            self_actualization: 250.0,
        };
        assert_eq!(MaslowTier::next_unmet(&satiated), None);

        let hungry = Needs { hunger: 200.0, ..satiated };
        assert_eq!(MaslowTier::next_unmet(&hungry), Some(MaslowTier::Physiological));

        let unsafe_ = Needs { safety: 200.0, ..satiated };
        assert_eq!(MaslowTier::next_unmet(&unsafe_), Some(MaslowTier::Safety));

        let lonely = Needs { social: 200.0, ..satiated };
        assert_eq!(MaslowTier::next_unmet(&lonely), Some(MaslowTier::Belonging));

        let unfulfilled = Needs { esteem: 100.0, ..satiated };
        assert_eq!(MaslowTier::next_unmet(&unfulfilled), Some(MaslowTier::Esteem));

        let mastering = Needs { self_actualization: 100.0, ..satiated };
        assert_eq!(
            MaslowTier::next_unmet(&mastering),
            Some(MaslowTier::SelfActualization),
        );
    }

    #[test]
    fn esteem_seeking_wealthy_agent_posts_luxury_contract_per_day() {
        // R8 follow-on validation: a wealthy agent with all lower
        // Maslow tiers satiated AND esteem unfulfilled posts a
        // Torch (recipe 2 = Luxury) contract per game-day. Esteem
        // bumps on each post; system-wide currency invariant holds.
        use crate::simulation::jobs::{
            JobBoard, JobKind, PosterClass, ESTEEM_CONTRACT_REWARD,
        };
        use crate::simulation::needs::Needs;
        use crate::world::seasons::TICKS_PER_DAY;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        let agent = sim.spawn_person(sim.player_faction_id, (5, 5), |b| {
            b.needs(Needs {
                hunger: 0.0,
                sleep: 0.0,
                shelter: 0.0,
                safety: 0.0,
                social: 0.0,
                reproduction: 0.0,
                willpower: 255.0,
                esteem: 0.0, // unfulfilled — Maslow tier 4
                self_actualization: 0.0,
            });
        });
        set_currency(&mut sim.app, agent, 100.0);
        let baseline = CurrencySnapshot::capture(&mut sim.app);
        let starting_esteem = sim.app.world().get::<Needs>(agent).unwrap().esteem;

        sim.tick_n(TICKS_PER_DAY as u32 + 5);

        let board = sim.app.world().resource::<JobBoard>();
        let postings: Vec<_> = board
            .faction_postings(sim.player_faction_id)
            .iter()
            .filter(|p| {
                p.kind == JobKind::Craft
                    && p.poster_class == PosterClass::Individual
                    && (p.reward - ESTEEM_CONTRACT_REWARD).abs() < 1e-3
            })
            .collect();
        assert!(
            !postings.is_empty(),
            "expected at least one Esteem-driven Individual contract on the board",
        );

        // Esteem bumped — agent felt prestigious.
        let new_esteem = sim.app.world().get::<Needs>(agent).unwrap().esteem;
        assert!(
            new_esteem > starting_esteem,
            "esteem should increase after posting: {starting_esteem} → {new_esteem}",
        );

        // Currency invariant holds across debit + escrow.
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-2);
    }

    #[test]
    fn hungry_agent_does_not_post_esteem_contract() {
        // Maslow gate negative: an agent with hunger pressure stays
        // on Physiological tier and does NOT post an Esteem
        // contract, even if wealthy.
        use crate::simulation::jobs::{JobBoard, JobKind, PosterClass};
        use crate::simulation::needs::Needs;
        use crate::world::seasons::TICKS_PER_DAY;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        let agent = sim.spawn_person(sim.player_faction_id, (5, 5), |b| {
            b.needs(Needs {
                hunger: 200.0, // pressure → Physiological tier
                sleep: 0.0,
                shelter: 0.0,
                safety: 0.0,
                social: 0.0,
                reproduction: 0.0,
                willpower: 255.0,
                esteem: 0.0,
                self_actualization: 0.0,
            });
        });
        set_currency(&mut sim.app, agent, 100.0);
        let baseline = CurrencySnapshot::capture(&mut sim.app);

        sim.tick_n(TICKS_PER_DAY as u32 + 5);

        let board = sim.app.world().resource::<JobBoard>();
        let count = board
            .faction_postings(sim.player_faction_id)
            .iter()
            .filter(|p| {
                p.kind == JobKind::Craft
                    && p.poster_class == PosterClass::Individual
            })
            .count();
        assert_eq!(
            count, 0,
            "hungry agent must not post Esteem contract while Physiological tier unmet",
        );
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-2);
    }

    // ─── Pluralist Economy R6 follow-on — household-poster path ───

    #[test]
    fn funded_household_posts_paid_craft_contract_per_day() {
        // R6 follow-on: spawn a household sub-faction with a
        // pre-funded treasury; tick one game-day; assert exactly
        // one paid `JobKind::Craft` posting with
        // `poster_class=HouseholdHead` lands on the village's job
        // board. Validates:
        // - household_contract_posting_system fires per-day.
        // - post_craft_contract_from_treasury debits the household
        //   treasury and credits a JobEscrow sidecar.
        // - The posting routes to the village board (parent
        //   faction), not a separate per-household board.
        // - System-wide currency invariant holds (debit + escrow ==
        //   const).
        use crate::simulation::faction::{
            FactionRegistry, HOUSEHOLD_CONTRACT_REWARD,
        };
        use crate::simulation::jobs::{JobBoard, JobKind, PosterClass};
        use crate::world::seasons::TICKS_PER_DAY;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        let head = sim.spawn_person(sim.player_faction_id, (5, 5), |_| {});

        // Spawn a household with the player faction as parent + seed
        // treasury well above the minimum.
        let village_id = sim.player_faction_id;
        let household_id = {
            let catalog = sim
                .app
                .world()
                .resource::<crate::economy::resource_catalog::ResourceCatalog>()
                .clone();
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            let id = registry.spawn_household(village_id, (5, 5), head, &catalog);
            registry.factions.get_mut(&id).unwrap().treasury = 50.0;
            id
        };
        let _ = household_id;

        let baseline = CurrencySnapshot::capture(&mut sim.app);

        // Tick one game-day so the cadence fires.
        sim.tick_n(TICKS_PER_DAY as u32 + 5);

        // The village's job board should now have at least one
        // HouseholdHead-posted Craft job with the right reward.
        let board = sim.app.world().resource::<JobBoard>();
        let postings: Vec<_> = board
            .faction_postings(village_id)
            .iter()
            .filter(|p| {
                p.kind == JobKind::Craft
                    && p.poster_class == PosterClass::HouseholdHead
                    && (p.reward - HOUSEHOLD_CONTRACT_REWARD).abs() < 1e-3
            })
            .collect();
        assert!(
            !postings.is_empty(),
            "expected at least one HouseholdHead Craft posting on village board",
        );

        // System-wide currency invariant: debit went household
        // treasury → escrow; total preserved.
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-2);

        // Household treasury debited by the reward(s).
        let registry = sim.app.world().resource::<FactionRegistry>();
        let h = registry.factions.get(&household_id).unwrap();
        assert!(
            h.treasury < 50.0,
            "household treasury should be debited: now={}",
            h.treasury,
        );
    }

    #[test]
    fn underfunded_household_posts_nothing() {
        // Edge case: a household with treasury below
        // HOUSEHOLD_MIN_TREASURY_FOR_POSTING posts nothing.
        use crate::simulation::faction::{
            FactionRegistry, HOUSEHOLD_MIN_TREASURY_FOR_POSTING,
        };
        use crate::simulation::jobs::{JobBoard, JobKind, PosterClass};
        use crate::world::seasons::TICKS_PER_DAY;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        let head = sim.spawn_person(sim.player_faction_id, (5, 5), |_| {});

        let village_id = sim.player_faction_id;
        {
            let catalog = sim
                .app
                .world()
                .resource::<crate::economy::resource_catalog::ResourceCatalog>()
                .clone();
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            let id = registry.spawn_household(village_id, (5, 5), head, &catalog);
            // Treasury just below the threshold.
            registry.factions.get_mut(&id).unwrap().treasury =
                HOUSEHOLD_MIN_TREASURY_FOR_POSTING - 0.5;
        }
        let baseline = CurrencySnapshot::capture(&mut sim.app);
        sim.tick_n(TICKS_PER_DAY as u32 + 5);

        let board = sim.app.world().resource::<JobBoard>();
        let house_postings = board
            .faction_postings(village_id)
            .iter()
            .filter(|p| {
                p.kind == JobKind::Craft
                    && p.poster_class == PosterClass::HouseholdHead
            })
            .count();
        assert_eq!(house_postings, 0);
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-2);
    }

    // ─── Pluralist Economy R3 follow-on — household formation trigger ───

    #[test]
    fn cosleep_bond_above_threshold_spawns_household() {
        // R3 follow-on: drive `CoSleepTracker.bond_strength` past
        // `HOUSEHOLD_BOND_THRESHOLD` for two pair-bonded agents in
        // the same village; assert `household_formation_system`
        // spawns a sub-faction, marks both agents as
        // `HouseholdMember`, and stamps the capitalist policy on
        // every catalog resource for the household.
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::reproduction::{
            CoSleepTracker, HouseholdMember, HOUSEHOLD_BOND_THRESHOLD,
        };

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        let head = sim.spawn_person(sim.player_faction_id, (5, 5), |_| {});
        let partner = sim.spawn_person(sim.player_faction_id, (5, 6), |_| {});

        // Tick a couple times so spawns settle.
        sim.tick_n(2);

        // Manually drive the bond_strength past threshold for both
        // agents (the cosleep_observation_system would do this
        // organically over a game-week, but headless tests would
        // need to put both agents into Sleeping AiState which is
        // outside the harness's quick-setup vocabulary).
        {
            let mut head_tracker = sim
                .app
                .world_mut()
                .get_mut::<CoSleepTracker>(head)
                .expect("CoSleepTracker missing on head");
            head_tracker.partner = Some(partner);
            head_tracker.bond_strength = HOUSEHOLD_BOND_THRESHOLD + 1;
        }
        {
            let mut partner_tracker = sim
                .app
                .world_mut()
                .get_mut::<CoSleepTracker>(partner)
                .expect("CoSleepTracker missing on partner");
            partner_tracker.partner = Some(head);
            partner_tracker.bond_strength = HOUSEHOLD_BOND_THRESHOLD + 1;
        }

        // Tick the household formation system (Economy schedule).
        sim.tick_n(2);

        // Both parents should now carry HouseholdMember.
        let head_marker = sim
            .app
            .world()
            .get::<HouseholdMember>(head)
            .expect("head should have HouseholdMember inserted");
        let partner_marker = sim
            .app
            .world()
            .get::<HouseholdMember>(partner)
            .expect("partner should have HouseholdMember inserted");
        assert_eq!(
            head_marker.household_id, partner_marker.household_id,
            "both pair members must be in the same household",
        );

        // Household exists in the registry with capitalist policy +
        // village as parent.
        let registry = sim.app.world().resource::<FactionRegistry>();
        let household = registry
            .factions
            .get(&head_marker.household_id)
            .expect("household FactionData missing");
        assert_eq!(household.parent_faction, Some(sim.player_faction_id));
        // System iterates query in arbitrary order; either pair
        // member could be the head. The other is then a member by
        // virtue of also having `HouseholdMember` inserted.
        assert!(
            household.household_head == Some(head)
                || household.household_head == Some(partner),
            "household head should be one of the pair: got {:?}",
            household.household_head,
        );
        let village = registry.factions.get(&sim.player_faction_id).unwrap();
        assert!(village.children_factions.contains(&head_marker.household_id));

        // Capitalist on a sample resource; village still communist.
        use crate::economy::core_ids;
        let h_wood = household.policy_for(core_ids::wood());
        assert!(h_wood.private_actors_allowed);
        assert!(!h_wood.chief_allocates_labor);
        let v_wood = village.policy_for(core_ids::wood());
        assert!(v_wood.chief_allocates_labor);
        assert!(!v_wood.private_actors_allowed);
    }

    #[test]
    fn cosleep_below_threshold_does_not_spawn_household() {
        // Negative case: bond_strength below threshold → no
        // household formation.
        use crate::simulation::reproduction::{
            CoSleepTracker, HouseholdMember, HOUSEHOLD_BOND_THRESHOLD,
        };

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        let head = sim.spawn_person(sim.player_faction_id, (5, 5), |_| {});
        let partner = sim.spawn_person(sim.player_faction_id, (5, 6), |_| {});
        sim.tick_n(2);

        {
            let mut t = sim.app.world_mut().get_mut::<CoSleepTracker>(head).unwrap();
            t.partner = Some(partner);
            t.bond_strength = HOUSEHOLD_BOND_THRESHOLD / 2;
        }
        sim.tick_n(2);

        assert!(
            sim.app.world().get::<HouseholdMember>(head).is_none(),
            "below-threshold bond must not form a household",
        );
    }

    #[test]
    fn household_formation_idempotent_across_ticks() {
        // After formation, ticking many more times must not form a
        // second household for the same pair (HouseholdMember
        // marker is the gate).
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::reproduction::{
            CoSleepTracker, HouseholdMember, HOUSEHOLD_BOND_THRESHOLD,
        };

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        let head = sim.spawn_person(sim.player_faction_id, (5, 5), |_| {});
        let partner = sim.spawn_person(sim.player_faction_id, (5, 6), |_| {});
        sim.tick_n(2);

        {
            let mut t = sim.app.world_mut().get_mut::<CoSleepTracker>(head).unwrap();
            t.partner = Some(partner);
            t.bond_strength = HOUSEHOLD_BOND_THRESHOLD + 5;
        }
        {
            let mut t = sim.app.world_mut().get_mut::<CoSleepTracker>(partner).unwrap();
            t.partner = Some(head);
            t.bond_strength = HOUSEHOLD_BOND_THRESHOLD + 5;
        }
        sim.tick_n(20);

        let households_under_village = {
            let registry = sim.app.world().resource::<FactionRegistry>();
            registry
                .factions
                .get(&sim.player_faction_id)
                .unwrap()
                .children_factions
                .len()
        };
        assert_eq!(
            households_under_village, 1,
            "pair must form exactly one household even after many ticks",
        );

        let head_id = sim.app.world().get::<HouseholdMember>(head).unwrap().household_id;
        let partner_id = sim.app.world().get::<HouseholdMember>(partner).unwrap().household_id;
        assert_eq!(head_id, partner_id);
    }

    // ─── Pluralist Economy R12 — P2P craft contracts (Phase 8c) ───

    #[test]
    fn wealthy_agent_posts_craft_contract_and_escrow_lifecycle_holds() {
        // R12 worked example: a wealthy agent posts a P2P craft
        // contract paying a reward. Validates:
        // - Currency is debited from the poster at post time.
        // - A JobEscrow sidecar exists with the right amount +
        //   beneficiary.
        // - The posting carries `poster_class=Individual` and
        //   `reward > 0`, so R9's U_bid scorer routes through the
        //   paid branch when smiths claim it.
        // - On cancellation (despawning the escrow entity), the
        //   on_remove hook refunds the poster.
        // - System-wide currency invariant holds across post and
        //   cancel.
        //
        // Hard guardrail: zero diff in tasks.rs / typed_task.rs.
        // Zero new TaskKind / Task variant. The contract reuses
        // the existing JobKind::Craft + JobProgress::Crafting +
        // JobEscrow primitives.
        use crate::simulation::jobs::{
            post_craft_contract, JobBoard, JobEscrow, JobKind, PosterClass,
        };

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);

        let poster = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        set_currency(&mut sim.app, poster, 100.0);
        let baseline = CurrencySnapshot::capture(&mut sim.app);

        // Recipe id 0 is always valid (Tools).
        let reward = 25.0;
        let escrow_entity = post_craft_contract(
            sim.app.world_mut(),
            poster,
            sim.player_faction_id,
            0,
            1,
            reward,
            None,
        )
        .expect("post_craft_contract should succeed for funded agent + valid recipe");

        // Poster's wallet debited.
        assert_currency(&sim.app, poster, 75.0);

        // Escrow sidecar reflects the right state.
        let escrow = sim
            .app
            .world()
            .get::<JobEscrow>(escrow_entity)
            .expect("JobEscrow component missing on sidecar");
        assert_eq!(escrow.amount, reward);
        assert_eq!(escrow.beneficiary, poster);

        // Posting landed on the board with the right shape.
        let board = sim.app.world().resource::<JobBoard>();
        let posting = board
            .faction_postings(sim.player_faction_id)
            .iter()
            .find(|p| p.kind == JobKind::Craft && p.poster_class == PosterClass::Individual)
            .expect("contract posting not found on board");
        assert_eq!(posting.reward, reward);
        assert!(posting.claimants.is_empty(), "fresh contract has no claimants");

        // Mid-flight invariant: 25 currency in escrow + 75 on poster
        // = 100 baseline. Total system currency unchanged.
        let mid = CurrencySnapshot::capture(&mut sim.app);
        assert!(
            (mid.total() - baseline.total()).abs() < 1e-3,
            "invariant must hold mid-flight: baseline={baseline:?}, mid={mid:?}",
        );

        // Cancel: despawning the escrow triggers the on_remove
        // refund hook (R2). Poster gets their 25 back.
        sim.app.world_mut().despawn(escrow_entity);

        assert_currency(&sim.app, poster, 100.0);
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-3);
    }

    #[test]
    fn post_craft_contract_refuses_insufficient_funds() {
        // A poor agent can't post a contract they can't fund.
        use crate::simulation::jobs::post_craft_contract;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);
        let poster = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        set_currency(&mut sim.app, poster, 10.0);
        let baseline = CurrencySnapshot::capture(&mut sim.app);

        let result = post_craft_contract(
            sim.app.world_mut(),
            poster,
            sim.player_faction_id,
            0,
            1,
            50.0,
            None,
        );
        assert!(result.is_none(), "should refuse insufficient funds");
        assert_currency(&sim.app, poster, 10.0); // unchanged
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-3);
    }

    // ─── Pluralist Economy R11 — Tribute (Phase 8b) ───

    #[test]
    fn subordinate_faction_pays_tribute_to_dominant_per_day() {
        // R11 worked example: configure faction A as dominant over
        // B; seed B's treasury; tick a few game-days; assert B's
        // treasury was debited and A's treasury credited by
        // `TRIBUTE_PER_DAY` per cycle. Currency invariant holds
        // (faction-treasury-to-faction-treasury transfer is
        // conservative).
        //
        // Hard guardrail: this test imports nothing from tasks.rs /
        // typed_task.rs / executors. The only new surfaces are:
        // FactionData.dominance_over / subordinate_to (relationship
        // primitive) + FactionRegistry::set_dominance + the
        // periodic tribute_payment_system.
        use crate::simulation::faction::{
            FactionRegistry, TRIBUTE_PER_DAY,
        };
        use crate::world::seasons::TICKS_PER_DAY;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);

        // Two factions: A is the player faction (auto-created) and
        // B is a new one. Set A dominant over B; seed B's treasury.
        let dominant = sim.player_faction_id;
        let subordinate = {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            let id = registry.create_faction((10, 10));
            registry.set_dominance(dominant, id);
            registry.factions.get_mut(&id).unwrap().treasury = 100.0;
            id
        };

        // Verify reciprocal relationship recorded.
        {
            let registry = sim.app.world().resource::<FactionRegistry>();
            assert!(
                registry
                    .factions
                    .get(&dominant)
                    .unwrap()
                    .dominance_over
                    .contains(&subordinate)
            );
            assert_eq!(
                registry.factions.get(&subordinate).unwrap().subordinate_to,
                Some(dominant),
            );
        }

        let baseline = CurrencySnapshot::capture(&mut sim.app);

        // Tick 3 game-days. tribute_payment_system fires on
        // `tick % TICKS_PER_DAY == 0`, which is at ticks 0, 3600,
        // 7200, 10800 — but tick 0 is the bootstrap tick before
        // anything is set up, and the relationship was just stamped.
        // To be safe, ensure the system fires multiple times.
        sim.tick_n((TICKS_PER_DAY * 3) as u32 + 5);

        let registry = sim.app.world().resource::<FactionRegistry>();
        let dom_treasury = registry.factions.get(&dominant).unwrap().treasury;
        let sub_treasury = registry.factions.get(&subordinate).unwrap().treasury;

        // Subordinate paid at least 2 days of tribute; dominant
        // received the corresponding amount.
        assert!(
            sub_treasury <= 100.0 - 2.0 * TRIBUTE_PER_DAY,
            "subordinate treasury must have paid at least 2 days: now={sub_treasury}",
        );
        assert!(
            dom_treasury >= 2.0 * TRIBUTE_PER_DAY,
            "dominant treasury must have received at least 2 days: now={dom_treasury}",
        );

        // Currency invariant: per-faction transfer is conservative.
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-2);
    }

    #[test]
    fn destitute_subordinate_pays_no_tribute() {
        // Edge case: a subordinate with empty treasury pays
        // nothing — no debt accrual.
        use crate::simulation::faction::FactionRegistry;
        use crate::world::seasons::TICKS_PER_DAY;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);

        let dominant = sim.player_faction_id;
        let subordinate = {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            let id = registry.create_faction((10, 10));
            registry.set_dominance(dominant, id);
            // Subordinate's treasury stays at 0 (default).
            id
        };

        let baseline = CurrencySnapshot::capture(&mut sim.app);
        sim.tick_n((TICKS_PER_DAY * 3) as u32 + 5);

        let registry = sim.app.world().resource::<FactionRegistry>();
        assert_eq!(
            registry.factions.get(&subordinate).unwrap().treasury,
            0.0,
            "subordinate must not go into debt",
        );
        assert_eq!(
            registry.factions.get(&dominant).unwrap().treasury,
            0.0,
            "dominant must not have received money from a destitute subordinate",
        );
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-2);
    }

    // ─── Pluralist Economy R10 — Trader / market arbitrage (Phase 8a) ───

    #[test]
    fn trader_arbitrage_cycle_converges_settlement_prices() {
        // R10 worked example: with two settlements at very
        // different Cloth prices, a Trader's buy-low/sell-high
        // cycle moves goods from cheap to expensive and the prices
        // converge. Validates:
        // - `Profession::Trader` variant
        // - `trader_buy_at_settlement` primitive (currency + stock
        //   move atomically; treasury credited)
        // - `trader_sell_at_settlement` primitive (treasury debited
        //   if available; agent currency credited)
        // - Currency invariant holds across the full cycle (sum of
        //   agent + faction + settlement treasury + escrow == const)
        // - `settlement_price_update_system` ratchets prices toward
        //   equilibrium based on per-settlement supply/demand
        //
        // Hard guardrail check: this test imports nothing from
        // tasks.rs / typed_task.rs / executors. The only new
        // surfaces touched are: Profession::Trader (one variant) +
        // trader_buy_at_settlement / trader_sell_at_settlement (two
        // helpers in transactions.rs).
        use crate::economy::core_ids;
        use crate::economy::transactions::{
            trader_buy_at_settlement, trader_sell_at_settlement,
        };
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::person::Profession;
        use crate::simulation::settlement::{Settlement, SettlementMap};

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(2, 0, TileKind::Grass);

        // Two factions, two settlements. Spawn the trader as a
        // member of faction A.
        let faction_a = sim.player_faction_id;
        let faction_b = {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            registry.create_faction((10, 10))
        };
        let trader = sim.spawn_person(faction_a, (0, 0), |b| {
            b.profession(Profession::Trader);
        });
        // Bootstrap currency so the trader can fund the first buy.
        set_currency(&mut sim.app, trader, 200.0);

        // Tick a few times so both settlements auto-found.
        sim.tick_n(3);

        let (settlement_a, settlement_b) = {
            let map = sim.app.world().resource::<SettlementMap>();
            let a_id = map.first_for_faction(faction_a).unwrap();
            let b_id = map.first_for_faction(faction_b).unwrap();
            (
                *map.by_id.get(&a_id).unwrap(),
                *map.by_id.get(&b_id).unwrap(),
            )
        };

        let cloth = core_ids::cloth();
        // Seed initial conditions:
        //   A: lots of cloth, low price; modest treasury.
        //   B: no cloth, high price; large treasury (it'll buy at
        //      premium).
        {
            let mut a = sim
                .app
                .world_mut()
                .get_mut::<Settlement>(settlement_a)
                .unwrap();
            a.market.set_stock(cloth, 50.0);
            a.market.add_supply(cloth, 50.0); // bias price down
            a.treasury = 100.0;
        }
        {
            let mut b = sim
                .app
                .world_mut()
                .get_mut::<Settlement>(settlement_b)
                .unwrap();
            b.market.add_demand(cloth, 50.0); // bias price up
            b.treasury = 1000.0;
        }
        // Tick price update so the supply/demand bias materialises
        // into divergent prices before the trader acts.
        sim.tick_n(50);

        let baseline = CurrencySnapshot::capture(&mut sim.app);
        let p_a_initial = sim
            .app
            .world()
            .get::<Settlement>(settlement_a)
            .unwrap()
            .market
            .price_of(cloth);
        let p_b_initial = sim
            .app
            .world()
            .get::<Settlement>(settlement_b)
            .unwrap()
            .market
            .price_of(cloth);
        assert!(
            p_a_initial < p_b_initial,
            "expected initial price gap: a={p_a_initial}, b={p_b_initial}",
        );

        // Run the arbitrage cycle several times — buy 5 cloth at A,
        // sell at B, observe prices converge.
        for _ in 0..5 {
            let bought = trader_buy_at_settlement(
                sim.app.world_mut(),
                trader,
                settlement_a,
                cloth,
                5,
            );
            assert!(bought.is_some(), "buy must succeed when stock + funds available");

            let sold = trader_sell_at_settlement(
                sim.app.world_mut(),
                trader,
                settlement_b,
                cloth,
                5,
            );
            assert!(sold.is_some(), "sell must succeed when treasury funds it");

            // Tick the per-settlement price update so prices
            // ratchet on each side.
            sim.tick_n(20);
        }

        // Convergence check: gap should narrow.
        let p_a_final = sim
            .app
            .world()
            .get::<Settlement>(settlement_a)
            .unwrap()
            .market
            .price_of(cloth);
        let p_b_final = sim
            .app
            .world()
            .get::<Settlement>(settlement_b)
            .unwrap()
            .market
            .price_of(cloth);
        let gap_initial = p_b_initial - p_a_initial;
        let gap_final = p_b_final - p_a_final;
        assert!(
            gap_final < gap_initial,
            "expected price gap to narrow: initial={gap_initial}, final={gap_final}",
        );

        // Currency invariant: the trader's buys at A debit agent +
        // credit A treasury; sells at B credit agent + debit B
        // treasury. Net: total system currency unchanged.
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-2);
    }

    #[test]
    fn autonomous_trader_completes_buy_sell_cycle_via_dispatch() {
        // R10 follow-on: with two settlements at diverging Cloth
        // prices and a Trader who knows both, the autonomous
        // dispatch state machine should:
        //   1. Install a `TraderPlan` (TravelingToBuy → cheap mkt).
        //   2. On arrival at the buy market, execute the buy +
        //      advance phase to TravelingToSell.
        //   3. On arrival at the sell market, execute the sell +
        //      remove the plan.
        // Currency invariant holds across the full cycle.
        //
        // We bypass pathfinding by teleporting the trader between
        // legs (writing `Transform.translation` directly) so the
        // test exercises the dispatch state machine without
        // depending on the routing pipeline.
        use crate::economy::core_ids;
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::memory::AgentMemory;
        use crate::simulation::person::{Profession, TraderPhase, TraderPlan};
        use crate::simulation::settlement::{Settlement, SettlementMap};

        let mut sim = TestSim::new(0xC0FFEE_42);
        sim.flat_world(2, 0, TileKind::Grass);

        let faction_a = sim.player_faction_id;
        let faction_b = {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            registry.create_faction((10, 10))
        };
        let trader = sim.spawn_person(faction_a, (0, 0), |b| {
            b.profession(Profession::Trader);
        });
        set_currency(&mut sim.app, trader, 200.0);
        sim.tick_n(3);

        // Resolve settlement ids + entities.
        let (sid_a, sid_b, settlement_a, settlement_b) = {
            let map = sim.app.world().resource::<SettlementMap>();
            let a = map.first_for_faction(faction_a).unwrap();
            let b = map.first_for_faction(faction_b).unwrap();
            (a, b, *map.by_id.get(&a).unwrap(), *map.by_id.get(&b).unwrap())
        };

        let cloth = core_ids::cloth();
        // Seed price gap: A cheap (high stock + supply bias);
        // B expensive (high demand bias + funded treasury).
        {
            let mut a = sim
                .app
                .world_mut()
                .get_mut::<Settlement>(settlement_a)
                .unwrap();
            a.market.set_stock(cloth, 50.0);
            a.market.add_supply(cloth, 50.0);
            a.treasury = 100.0;
        }
        {
            let mut b = sim
                .app
                .world_mut()
                .get_mut::<Settlement>(settlement_b)
                .unwrap();
            b.market.add_demand(cloth, 50.0);
            b.treasury = 1000.0;
        }
        sim.tick_n(50);

        // Teach the trader about both settlements.
        {
            let mut mem = sim
                .app
                .world_mut()
                .get_mut::<AgentMemory>(trader)
                .unwrap();
            mem.record_settlement(sid_a);
            mem.record_settlement(sid_b);
        }

        let baseline = CurrencySnapshot::capture(&mut sim.app);
        let trader_currency_pre = get_currency(&mut sim.app, trader);

        // Resolve market tiles.
        let (buy_tile, sell_tile) = {
            let a = sim.app.world().get::<Settlement>(settlement_a).unwrap();
            let b = sim.app.world().get::<Settlement>(settlement_b).unwrap();
            (a.market_tile, b.market_tile)
        };

        // Sanity check: the price gap must exceed `TRADER_MIN_GAP`
        // for the dispatcher to commit to a cycle.
        let p_a = sim
            .app
            .world()
            .get::<Settlement>(settlement_a)
            .unwrap()
            .market
            .price_of(cloth);
        let p_b = sim
            .app
            .world()
            .get::<Settlement>(settlement_b)
            .unwrap()
            .market
            .price_of(cloth);
        assert!(
            p_b - p_a > crate::simulation::trader::TRADER_MIN_GAP,
            "test bug: seeded gap too small for dispatcher: a={p_a} b={p_b}",
        );

        // Pin the trader fully idle so the plan-creation gate fires.
        // Other systems (goal_update / HTN) may have given the
        // trader a task during settlement bootstrap; clear it and
        // invoke `trader_market_step_system` directly to exercise
        // the dispatcher's logic without scheduling perturbation
        // re-stamping a task within the same tick.
        clear_trader_for_dispatch(&mut sim.app, trader);
        crate::simulation::trader::trader_market_step_system(sim.app.world_mut());
        let plan_after_install = sim
            .app
            .world()
            .get::<TraderPlan>(trader)
            .copied()
            .expect("market step should install TraderPlan when arbitrage exists");
        assert_eq!(plan_after_install.phase, TraderPhase::TravelingToBuy);
        assert_eq!(plan_after_install.buy_settlement, sid_a);
        assert_eq!(plan_after_install.sell_settlement, sid_b);
        assert_eq!(plan_after_install.resource_id, cloth);

        // Teleport trader to the buy market and step the dispatcher
        // directly — the buy leg should fire.
        {
            let mut t = sim
                .app
                .world_mut()
                .get_mut::<Transform>(trader)
                .unwrap();
            t.translation.x = buy_tile.0 as f32 * crate::world::terrain::TILE_SIZE
                + crate::world::terrain::TILE_SIZE * 0.5;
            t.translation.y = buy_tile.1 as f32 * crate::world::terrain::TILE_SIZE
                + crate::world::terrain::TILE_SIZE * 0.5;
        }
        crate::simulation::trader::trader_market_step_system(sim.app.world_mut());
        let plan_after_buy = sim
            .app
            .world()
            .get::<TraderPlan>(trader)
            .copied()
            .expect("plan should still exist with phase advanced after buy");
        assert_eq!(plan_after_buy.phase, TraderPhase::TravelingToSell);
        let trader_currency_after_buy = get_currency(&mut sim.app, trader);
        assert!(
            trader_currency_after_buy < trader_currency_pre,
            "currency must drop after buy: pre={trader_currency_pre} post={trader_currency_after_buy}",
        );
        let trader_cloth_after_buy = sim
            .app
            .world()
            .get::<crate::economy::agent::EconomicAgent>(trader)
            .unwrap()
            .quantity_of_resource(cloth);
        assert_eq!(
            trader_cloth_after_buy, plan_after_install.qty,
            "trader should hold the bought quantity",
        );

        // Teleport to the sell market — the sell leg should fire and
        // remove the plan.
        {
            let mut t = sim
                .app
                .world_mut()
                .get_mut::<Transform>(trader)
                .unwrap();
            t.translation.x = sell_tile.0 as f32 * crate::world::terrain::TILE_SIZE
                + crate::world::terrain::TILE_SIZE * 0.5;
            t.translation.y = sell_tile.1 as f32 * crate::world::terrain::TILE_SIZE
                + crate::world::terrain::TILE_SIZE * 0.5;
        }
        crate::simulation::trader::trader_market_step_system(sim.app.world_mut());
        assert!(
            sim.app.world().get::<TraderPlan>(trader).is_none(),
            "plan should be cleared after sell leg",
        );
        let trader_cloth_after_sell = sim
            .app
            .world()
            .get::<crate::economy::agent::EconomicAgent>(trader)
            .unwrap()
            .quantity_of_resource(cloth);
        assert_eq!(
            trader_cloth_after_sell, 0,
            "trader's cloth inventory should be sold off",
        );
        let trader_currency_after_sell = get_currency(&mut sim.app, trader);
        assert!(
            trader_currency_after_sell > trader_currency_after_buy,
            "currency must rise after sell: post-buy={trader_currency_after_buy} post-sell={trader_currency_after_sell}",
        );

        // Currency invariant: agent + faction treasuries + settlement
        // treasuries + escrow stays constant across the cycle.
        assert_total_currency_invariant(&mut sim.app, baseline, 1e-2);
    }

    #[test]
    fn autonomous_trader_skips_install_when_no_capital() {
        // Capital floor: a Trader with currency < TRADER_MIN_CAPITAL
        // and a known price gap should NOT install a plan — the
        // dispatcher waits for funding rather than committing to a
        // cycle the trader can't afford.
        use crate::economy::core_ids;
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::memory::AgentMemory;
        use crate::simulation::person::{Profession, TraderPlan};
        use crate::simulation::settlement::{Settlement, SettlementMap};

        let mut sim = TestSim::new(0xC0FFEE_43);
        sim.flat_world(2, 0, TileKind::Grass);
        let faction_a = sim.player_faction_id;
        let faction_b = {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            registry.create_faction((10, 10))
        };
        let trader = sim.spawn_person(faction_a, (0, 0), |b| {
            b.profession(Profession::Trader);
        });
        // Below TRADER_MIN_CAPITAL.
        set_currency(&mut sim.app, trader, 5.0);
        sim.tick_n(3);
        let (sid_a, sid_b, settlement_a, settlement_b) = {
            let map = sim.app.world().resource::<SettlementMap>();
            let a = map.first_for_faction(faction_a).unwrap();
            let b = map.first_for_faction(faction_b).unwrap();
            (a, b, *map.by_id.get(&a).unwrap(), *map.by_id.get(&b).unwrap())
        };
        let cloth = core_ids::cloth();
        {
            let mut a = sim
                .app
                .world_mut()
                .get_mut::<Settlement>(settlement_a)
                .unwrap();
            a.market.set_stock(cloth, 50.0);
            a.market.add_supply(cloth, 50.0);
        }
        {
            let mut b = sim
                .app
                .world_mut()
                .get_mut::<Settlement>(settlement_b)
                .unwrap();
            b.market.add_demand(cloth, 50.0);
            b.treasury = 1000.0;
        }
        sim.tick_n(50);
        {
            let mut mem = sim
                .app
                .world_mut()
                .get_mut::<AgentMemory>(trader)
                .unwrap();
            mem.record_settlement(sid_a);
            mem.record_settlement(sid_b);
        }
        clear_trader_for_dispatch(&mut sim.app, trader);
        crate::simulation::trader::trader_market_step_system(sim.app.world_mut());
        assert!(
            sim.app.world().get::<TraderPlan>(trader).is_none(),
            "plan must NOT install when trader currency is below floor",
        );
    }

    #[test]
    fn wealth_modifier_decays_with_currency() {
        // R9 unit test: wealthy agents apply a smaller multiplier
        // than poor ones to the same reward.
        use crate::simulation::jobs::wealth_modifier;
        let poor = wealth_modifier(0.0);
        let middle = wealth_modifier(50.0);
        let rich = wealth_modifier(1000.0);
        assert!(poor > middle);
        assert!(middle > rich);
        assert!(rich >= 1.0, "modifier never drops below 1.0");
    }

    // ─── Pluralist Economy R8 — VisitedSettlements + Maslow needs (data layer) ───

    #[test]
    fn record_settlement_idempotent_and_evicts_lowest_freshness() {
        // R8: record up to 8 settlement ids; recording a 9th evicts
        // the lowest-freshness slot. Re-recording an existing id
        // refreshes its freshness to 255 without adding a duplicate.
        use crate::simulation::memory::AgentMemory;
        use crate::simulation::settlement::SettlementId;

        let mut mem = AgentMemory::default();
        for i in 0..8 {
            mem.record_settlement(SettlementId(i));
        }
        // All 8 slots full; each at freshness 255.
        let known: Vec<_> = mem.known_settlements().collect();
        assert_eq!(known.len(), 8);
        for (_, f) in &known {
            assert_eq!(*f, 255);
        }

        // Idempotent re-record.
        mem.record_settlement(SettlementId(3));
        let known2: Vec<_> = mem.known_settlements().collect();
        assert_eq!(known2.len(), 8, "re-record must not add duplicate");

        // Manually drop the freshness of slot 0 to force eviction.
        if let Some(slot) = mem.visited_settlements.iter_mut().find(|s| {
            matches!(s, Some((id, _)) if *id == SettlementId(0))
        }) {
            if let Some((_, f)) = slot {
                *f = 1;
            }
        }
        // New id should evict the freshness=1 slot.
        mem.record_settlement(SettlementId(99));
        let ids: Vec<_> = mem.known_settlements().map(|(id, _)| id).collect();
        assert!(ids.contains(&SettlementId(99)), "new id must be recorded");
        assert!(
            !ids.contains(&SettlementId(0)),
            "lowest-freshness slot must have been evicted",
        );
    }

    #[test]
    fn maslow_need_fields_default_to_zero() {
        // R8 inert-data check: every newly-spawned Person has
        // esteem=0 and self_actualization=0 — Maslow tiers start
        // unfulfilled, accumulate via lifetime activity (R9+
        // wires the goal-selection rewrite that consumes them).
        use crate::simulation::needs::Needs;

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        let person = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        sim.tick_n(2);

        let needs = sim
            .app
            .world()
            .get::<Needs>(person)
            .expect("Needs missing");
        assert_eq!(needs.esteem, 0.0);
        assert_eq!(needs.self_actualization, 0.0);
    }

    #[test]
    fn chief_skips_farm_when_grain_policy_capitalist() {
        // Flip Grain to capitalist; the chief stops posting Farm.
        use crate::economy::core_ids;
        use crate::economy::policy::ResourceControlPolicy;
        use crate::simulation::faction::FactionRegistry;
        use crate::simulation::jobs::{JobBoard, JobKind};

        let mut sim = TestSim::new(0xC0FFEE);
        sim.flat_world(1, 0, TileKind::Grass);
        for i in 0..4 {
            sim.spawn_person(sim.player_faction_id, (i, 0), |_| {});
        }
        {
            let mut registry = sim.app.world_mut().resource_mut::<FactionRegistry>();
            for _ in 0..4 {
                registry.add_member(sim.player_faction_id);
            }
            let f = registry
                .factions
                .get_mut(&sim.player_faction_id)
                .unwrap();
            // Pretend the faction has CROP_CULTIVATION + grain seeds
            // so the only thing blocking the post would be the policy
            // gate. (Default test factions don't have either, so this
            // test's negative assertion is over-determined; it pins
            // behaviour for the future when farm-capable factions
            // exist.)
            f.economic_policy
                .insert(core_ids::grain(), ResourceControlPolicy::capitalist());
        }

        sim.tick_n(120);

        let board = sim.app.world().resource::<JobBoard>();
        let farm_count: usize = board
            .faction_postings(sim.player_faction_id)
            .iter()
            .filter(|p| matches!(p.kind, JobKind::Farm))
            .count();
        assert_eq!(
            farm_count, 0,
            "capitalist Grain policy must block chief Farm postings",
        );
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
                esteem: 0.0,
                self_actualization: 0.0,
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
                poster_class: crate::simulation::jobs::PosterClass::Chief,
                reward: 0.0,
                settlement_id: None,
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
                poster_class: crate::simulation::jobs::PosterClass::Chief,
                reward: 0.0,
                settlement_id: None,
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
                poster_class: crate::simulation::jobs::PosterClass::Chief,
                reward: 0.0,
                settlement_id: None,
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
                poster_class: crate::simulation::jobs::PosterClass::Chief,
                reward: 0.0,
                settlement_id: None,
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
                poster_class: crate::simulation::jobs::PosterClass::Chief,
                reward: 0.0,
                settlement_id: None,
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
                poster_class: crate::simulation::jobs::PosterClass::Chief,
                reward: 0.0,
                settlement_id: None,
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
                poster_class: crate::simulation::jobs::PosterClass::Chief,
                reward: 0.0,
                settlement_id: None,
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
                esteem: 0.0,
                self_actualization: 0.0,
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
                poster_class: crate::simulation::jobs::PosterClass::Chief,
                reward: 0.0,
                settlement_id: None,
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
                poster_class: crate::simulation::jobs::PosterClass::Chief,
                reward: 0.0,
                settlement_id: None,
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
                poster_class: crate::simulation::jobs::PosterClass::Chief,
                reward: 0.0,
                settlement_id: None,
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
                esteem: 0.0,
                self_actualization: 0.0,
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
                esteem: 0.0,
                self_actualization: 0.0,
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
                esteem: 0.0,
                self_actualization: 0.0,
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
                esteem: 0.0,
                self_actualization: 0.0,
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
