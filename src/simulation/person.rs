use bevy::prelude::*;
use rand::Rng;

use crate::world::chunk::{ChunkMap, CHUNK_SIZE};
use crate::world::terrain::{tile_to_world, WORLD_CHUNKS_X, WORLD_CHUNKS_Y};
use crate::world::tile::TileKind;
use crate::economy::agent::EconomicAgent;

use super::combat::{CombatTarget, Body, CombatCooldown};
use super::faction::{FactionMember, FactionRegistry, PlayerFaction, FactionCenter, PlayerFactionMarker};
use super::goals::{AgentGoal, Personality};
use super::items::{Equipment, TargetItem};
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

/// Core person AI component — 12-16 bytes.
#[derive(Component, Clone, Copy, Default)]
pub struct PersonAI {
    pub job_id:        u16,
    pub state:         AiState,
    /// Progress ticks toward the next production event.
    pub work_progress: u8,
    pub target_tile:   (i16, i16),
    pub dest_tile:     (i16, i16),
    pub ticks_idle:    u8,
    pub last_plan_id:  u16,
    pub last_goal_eval_tick: u64,
    pub target_entity: Option<Entity>,
}

impl PersonAI {
    pub const UNEMPLOYED: u16 = u16::MAX;
}

/// Player-issued order that overrides autonomous AI for this entity.
#[derive(Component, Clone, Copy, Debug)]
pub struct PlayerOrder {
    pub order: PlayerOrderKind,
    pub target_tile: (i16, i16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerOrderKind {
    Move,
    Mine,
    Gather,
    PickUp,
    BuildWall,
    BuildBed,
}

impl PlayerOrderKind {
    pub fn label(self) -> &'static str {
        match self {
            PlayerOrderKind::Move      => "Move here",
            PlayerOrderKind::Mine      => "Mine",
            PlayerOrderKind::Gather    => "Gather",
            PlayerOrderKind::PickUp    => "Pick up",
            PlayerOrderKind::BuildWall => "Build Wall",
            PlayerOrderKind::BuildBed  => "Build Bed",
        }
    }
}

/// Marker for a person entity.
#[derive(Component)]
pub struct Person;

pub const INITIAL_POPULATION: u32 = 200;
const GROUP_SIZE: u32 = 20;
const SPAWN_RADIUS: i32 = 12;

pub fn spawn_population(
    mut commands: Commands,
    chunk_map: Res<ChunkMap>,
    mut clock: ResMut<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    mut player_faction: ResMut<PlayerFaction>,
) {
    use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};

    let start_cx = (GLOBE_WIDTH  / 2) * GLOBE_CELL_CHUNKS;
    let start_cy = (GLOBE_HEIGHT / 2) * GLOBE_CELL_CHUNKS;

    let mut rng = rand::thread_rng();
    let total_tiles_x = WORLD_CHUNKS_X * CHUNK_SIZE as i32;
    let total_tiles_y = WORLD_CHUNKS_Y * CHUNK_SIZE as i32;

    let start_tx = start_cx * CHUNK_SIZE as i32;
    let start_ty = start_cy * CHUNK_SIZE as i32;

    let num_groups = INITIAL_POPULATION / GROUP_SIZE;
    let mut spawned = 0u32;
    let mut spawned_homes: Vec<(i32, i32)> = Vec::new();

    // Helper: find a valid passable non-stone tile near (cx, cy) within radius, or anywhere in
    // the spawn region as fallback.
    let find_tile = |rng: &mut rand::rngs::ThreadRng, cx: i32, cy: i32| -> Option<(i32, i32)> {
        for _ in 0..200 {
            let tx = cx + rng.gen_range(-SPAWN_RADIUS..=SPAWN_RADIUS);
            let ty = cy + rng.gen_range(-SPAWN_RADIUS..=SPAWN_RADIUS);
            if chunk_map.is_passable(tx, ty)
                && !matches!(chunk_map.tile_kind_at(tx, ty), Some(TileKind::Stone))
            {
                return Some((tx, ty));
            }
        }
        None
    };

    for group_idx in 0..num_groups {
        // Find a home tile for this group anywhere in the spawn region,
        // ensuring it's at least 300 tiles away from other factions.
        let home = {
            let mut result = None;
            for _ in 0..1000 {
                let tx = start_tx + rng.gen_range(0..total_tiles_x);
                let ty = start_ty + rng.gen_range(0..total_tiles_y);
                
                if chunk_map.is_passable(tx, ty)
                    && !matches!(chunk_map.tile_kind_at(tx, ty), Some(TileKind::Stone))
                {
                    // Distance check
                    let too_close = spawned_homes.iter().any(|(hx, hy)| {
                        let dx = (tx - hx) as f32;
                        let dy = (ty - hy) as f32;
                        (dx*dx + dy*dy).sqrt() < 300.0
                    });

                    if !too_close || spawned_homes.is_empty() {
                        result = Some((tx, ty));
                        break;
                    }
                }
            }
            // Fallback: if we couldn't find a spot 300 tiles away, just take any passable tile.
            if result.is_none() {
                for _ in 0..500 {
                    let tx = start_tx + rng.gen_range(0..total_tiles_x);
                    let ty = start_ty + rng.gen_range(0..total_tiles_y);
                    if chunk_map.is_passable(tx, ty)
                        && !matches!(chunk_map.tile_kind_at(tx, ty), Some(TileKind::Stone))
                    {
                        result = Some((tx, ty));
                        break;
                    }
                }
            }
            result
        };

        let Some((home_tx, home_ty)) = home else { continue };
        spawned_homes.push((home_tx, home_ty));

        let faction_id = registry.create_faction((home_tx as i16, home_ty as i16));

        if group_idx == 0 {
            player_faction.faction_id = faction_id;

            // Mark the player's faction center
            let world_pos = tile_to_world(home_tx, home_ty);
            commands.spawn((
                FactionCenter,
                PlayerFactionMarker,
                Transform::from_xyz(world_pos.x, world_pos.y, 0.5),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ));
        }

        for _ in 0..GROUP_SIZE {
            let Some((tx, ty)) = find_tile(&mut rng, home_tx, home_ty) else { continue };

            let world_pos = tile_to_world(tx, ty);

            commands.spawn((
                (
                    Person,
                    Transform::from_xyz(world_pos.x, world_pos.y, 1.0),
                    GlobalTransform::default(),
                    Needs::new(30.0, 20.0, 10.0, 5.0, 40.0),
                    Mood::default(),
                    Skills::default(),
                        PersonAI {
                            job_id: PersonAI::UNEMPLOYED,
                            state: AiState::Idle,
                            target_tile: (tx as i16, ty as i16),
                            dest_tile: (tx as i16, ty as i16),
                            ticks_idle: 0,
                            work_progress: 0,
                            last_plan_id: PersonAI::UNEMPLOYED,
                            last_goal_eval_tick: 0,
                            target_entity: None,
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
                    FactionMember { faction_id, ..Default::default() },
                    Body::new_humanoid(),
                    Equipment::default(),
                    TargetItem::default(),
                    CombatTarget::default(),
                    CombatCooldown::default(),
                ),
                (
                    AgentMemory::default(),
                    RelationshipMemory::default(),
                    UtilityNet::new_random(),
                    KnownPlans::with_innate(&[0, 1, 2, 3, 5, 6, 7, 8]),
                    PlanScoringMethod::UtilityNN,
                ),
            ));

            registry.add_member(faction_id);
            spawned += 1;
        }
    }

    clock.population = spawned;
    clock.current_end = clock.bucket_size.min(spawned);

    info!("Spawned {} people in {} factions of {}", spawned, num_groups, GROUP_SIZE);
}
