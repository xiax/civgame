use bevy::prelude::*;

use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::economy::item::Item;
use crate::pathfinding::path_request::PathFollow;
use crate::simulation::animals::{AnimalAI, AnimalNeeds, AnimalReproductionCooldown, Deer, Wolf};
use crate::simulation::carry::Carrier;
use crate::simulation::combat::{Body, CombatCooldown, CombatTarget, Health};
use crate::simulation::faction::FactionMember;
use crate::simulation::goals::{AgentGoal, Personality};
use crate::simulation::items::{Equipment, GroundItem};
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::{AgentMemory, RelationshipMemory};
use crate::simulation::mood::Mood;
use crate::simulation::movement::MovementState;
use crate::simulation::needs::Needs;
use crate::simulation::person::{AiState, Person, PersonAI};
use crate::simulation::plan::{KnownPlans, PlanHistory, PlanScoringMethod};
use crate::simulation::plants::{
    spawn_plant_at, DeerGrazer, GrowthStage, PlantKind, PlantMap, PlantSpriteIndex,
};
use crate::simulation::reproduction::BiologicalSex;
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::Skills;
use crate::world::chunk::CHUNK_SIZE;
use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
use crate::world::terrain::tile_to_world;

#[derive(Resource)]
pub struct SandboxMode;

pub struct SandboxPlugin;

impl Plugin for SandboxPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(SandboxMode)
            .add_systems(PostStartup, setup_sandbox);
    }
}

fn setup_sandbox(
    mut commands: Commands,
    mut clock: ResMut<SimClock>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    chunk_map: Res<crate::world::chunk::ChunkMap>,
) {
    let start_cx = (GLOBE_WIDTH / 2) * GLOBE_CELL_CHUNKS;
    let start_cy = (GLOBE_HEIGHT / 2) * GLOBE_CELL_CHUNKS;
    // Place entities 10 tiles into the sandbox so they're immediately on screen.
    let cx = start_cx * CHUNK_SIZE as i32 + 10;
    let cy = start_cy * CHUNK_SIZE as i32 + 10;

    // Person — split into nested tuples to stay within Bevy's bundle arity limit
    let pos = tile_to_world(cx, cy);
    commands.spawn((
        (
            Person,
            Transform::from_xyz(pos.x, pos.y, 1.0),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
            Needs::new(30.0, 20.0, 10.0, 5.0, 40.0, 200.0),
            Mood::default(),
            Skills::default(),
            PersonAI {
                task_id: PersonAI::UNEMPLOYED,
                state: AiState::Idle,
                target_tile: (cx as i16, cy as i16),
                dest_tile: (cx as i16, cy as i16),
                ticks_idle: 0,
                work_progress: 0,
                last_plan_id: PersonAI::UNEMPLOYED,
                last_goal_eval_tick: 0,
                target_entity: None,
                current_z: chunk_map.surface_z_at(cx, cy) as i8,
                target_z: chunk_map.surface_z_at(cx, cy) as i8,
                craft_recipe_id: 0,
            },
            EconomicAgent::default(),
        ),
        (
            LodLevel::Full,
            BucketSlot(0),
            MovementState::default(),
            PathFollow::default(),
            BiologicalSex::random(),
            Personality::random(),
            AgentGoal::default(),
            FactionMember::default(),
            Body::new_humanoid(),
            Equipment::default(),
            CombatTarget::default(),
            CombatCooldown::default(),
        ),
        (
            AgentMemory::default(),
            RelationshipMemory::default(),
            KnownPlans::with_innate(&[
    0, 1, 2, 3, 5, 23, 25, 26, 27, 28, 30, 31, 32,
]),
            PlanHistory::default(),
            PlanScoringMethod::Weighted,
            Carrier::default(),
            crate::simulation::reproduction::CoSleepTracker::default(),
            crate::simulation::reproduction::MaleConceptionCooldown::default(),
        ),
    ));

    // Wolf (5 tiles right of person)
    let wolf_pos = tile_to_world(cx + 5, cy);
    commands.spawn((
        Wolf,
        Transform::from_xyz(wolf_pos.x, wolf_pos.y, 1.0),
        GlobalTransform::default(),
        Visibility::Visible,
        InheritedVisibility::default(),
        AnimalAI {
            target_tile: ((cx + 5) as i16, cy as i16),
            wander_timer: 0.0,
            ..Default::default()
        },
        Health::new(30),
        CombatTarget::default(),
        CombatCooldown::default(),
        LodLevel::Full,
        BucketSlot(1),
        AnimalNeeds::default(),
        AnimalReproductionCooldown(0),
        BiologicalSex::random(),
    ));

    // Deer (4 tiles left, 3 tiles up from person)
    let deer_pos = tile_to_world(cx - 4, cy + 3);
    commands.spawn((
        Deer,
        Transform::from_xyz(deer_pos.x, deer_pos.y, 1.0),
        GlobalTransform::default(),
        Visibility::Visible,
        InheritedVisibility::default(),
        AnimalAI {
            target_tile: ((cx - 4) as i16, (cy + 3) as i16),
            wander_timer: 0.5,
            ..Default::default()
        },
        Health::new(20),
        CombatTarget::default(),
        CombatCooldown::default(),
        LodLevel::Full,
        BucketSlot(2),
        DeerGrazer { graze_timer: 0 },
        AnimalNeeds::default(),
        AnimalReproductionCooldown(0),
        BiologicalSex::random(),
    ));

    clock.population = 3;
    clock.current_end = 3;

    // One of each plant kind, mature, clustered above the entity group.
    // spawn_plant_at is a no-op if the tile is already occupied by a chunk-generated plant.
    spawn_plant_at(
        &mut commands,
        &mut plant_map,
        &mut plant_sprite_index,
        cx + 2,
        cy + 5,
        PlantKind::BerryBush,
        GrowthStage::Mature,
    );
    spawn_plant_at(
        &mut commands,
        &mut plant_map,
        &mut plant_sprite_index,
        cx - 2,
        cy + 5,
        PlantKind::Grain,
        GrowthStage::Mature,
    );
    spawn_plant_at(
        &mut commands,
        &mut plant_map,
        &mut plant_sprite_index,
        cx,
        cy + 7,
        PlantKind::Tree,
        GrowthStage::Mature,
    );

    // Ground items near the person
    let food_pos = tile_to_world(cx + 1, cy + 1);
    commands.spawn((
        GroundItem {
            item: Item::new_commodity(Good::Fruit),
            qty: 5,
        },
        Transform::from_xyz(food_pos.x, food_pos.y, 0.5),
        GlobalTransform::default(),
        Visibility::Visible,
        InheritedVisibility::default(),
    ));
    let wood_pos = tile_to_world(cx - 1, cy + 1);
    commands.spawn((
        GroundItem {
            item: Item::new_commodity(Good::Wood),
            qty: 3,
        },
        Transform::from_xyz(wood_pos.x, wood_pos.y, 0.5),
        GlobalTransform::default(),
        Visibility::Visible,
        InheritedVisibility::default(),
    ));

    info!("Sandbox: 1 person, 1 wolf, 1 deer, 3 plants, 2 ground items at tile ({cx}, {cy})");
}
