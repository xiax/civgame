use bevy::prelude::*;
use rand::Rng;

use crate::world::chunk::{ChunkMap, CHUNK_SIZE};
use crate::world::terrain::{tile_to_world, WORLD_CHUNKS_X, WORLD_CHUNKS_Y};
use crate::world::tile::TileKind;
use crate::economy::agent::EconomicAgent;

use super::combat::{CombatTarget, Body};
use super::faction::FactionMember;
use super::goals::{AgentGoal, Personality};
use super::items::Equipment;
use super::lod::LodLevel;
use super::memory::{AgentMemory, RelationshipMemory};
use super::mood::Mood;
use super::movement::MovementState;
use super::needs::Needs;
use super::neural::UtilityNet;
use super::plan::{KnownPlans, PlanScoringMethod};
use super::reproduction::BiologicalSex;
use super::schedule::{BucketSlot, SimClock};
use super::skills::Skills;

/// Size of an entity on the grid. Absent = 1×1.
#[derive(Component, Clone, Copy)]
pub struct GridSize {
    pub w: u8,
    pub h: u8,
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AiState {
    #[default]
    Idle      = 0,
    Working   = 1,
    Seeking   = 2,
    Sleeping  = 3,
    Routing   = 4,
    Attacking = 5,
}

/// Core person AI component — 8 bytes.
#[derive(Component, Clone, Copy, Default)]
pub struct PersonAI {
    pub job_id:        u16,
    pub state:         AiState,
    /// Progress ticks toward the next production event.
    pub work_progress: u8,
    pub target_tile:   (i16, i16),
    pub ticks_idle:    u8,
}

impl PersonAI {
    pub const UNEMPLOYED: u16 = u16::MAX;
}

/// Marker for a person entity.
#[derive(Component)]
pub struct Person;

pub const INITIAL_POPULATION: u32 = 1_000;

pub fn spawn_population(
    mut commands: Commands,
    chunk_map: Res<ChunkMap>,
    mut clock: ResMut<SimClock>,
) {
    use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};

    let start_cx = (GLOBE_WIDTH  / 2) * GLOBE_CELL_CHUNKS;
    let start_cy = (GLOBE_HEIGHT / 2) * GLOBE_CELL_CHUNKS;

    let mut rng = rand::thread_rng();
    let total_tiles_x = WORLD_CHUNKS_X * CHUNK_SIZE as i32;
    let total_tiles_y = WORLD_CHUNKS_Y * CHUNK_SIZE as i32;

    let start_tx = start_cx * CHUNK_SIZE as i32;
    let start_ty = start_cy * CHUNK_SIZE as i32;

    let mut spawned = 0u32;
    let mut attempts = 0u32;

    while spawned < INITIAL_POPULATION && attempts < INITIAL_POPULATION * 20 {
        attempts += 1;
        let tx = start_tx + rng.gen_range(0..total_tiles_x);
        let ty = start_ty + rng.gen_range(0..total_tiles_y);

        if !chunk_map.is_passable(tx, ty) {
            continue;
        }
        if let Some(tile) = chunk_map.tile_at(tx, ty) {
            if matches!(tile.kind, TileKind::Stone) {
                continue;
            }
        }

        let world_pos = tile_to_world(tx, ty);

        commands.spawn((
            (
                Person,
                Transform::from_xyz(world_pos.x, world_pos.y, 1.0),
                GlobalTransform::default(),
                Needs::new(30, 20, 10, 5, 40),
                Mood::default(),
                Skills::default(),
                PersonAI {
                    job_id: PersonAI::UNEMPLOYED,
                    state: AiState::Idle,
                    target_tile: (tx as i16, ty as i16),
                    ticks_idle: 0,
                    work_progress: 0,
                },
                EconomicAgent::default(),
            ),
            (
                LodLevel::Full,
                BucketSlot(spawned),
                MovementState { wander_timer: (spawned % 100) as f32 * 0.025 },
                BiologicalSex::random(),
                Personality::random(),
                AgentGoal::default(),
                FactionMember::default(),
                Body::new_humanoid(),
                Equipment::default(),
                CombatTarget::default(),
            ),
            (
                AgentMemory::default(),
                RelationshipMemory::default(),
                UtilityNet::new_random(),
                KnownPlans::with_innate(&[0, 1]),
                PlanScoringMethod::UtilityNN,
            ),
        ));

        spawned += 1;
    }

    clock.population = spawned;
    clock.current_end = clock.bucket_size.min(spawned);

    info!("Spawned {} people", spawned);
}
