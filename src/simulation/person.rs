use bevy::prelude::*;
use rand::Rng;
use std::time::Instant;

use crate::economy::agent::EconomicAgent;
use crate::world::chunk::{ChunkMap, CHUNK_SIZE};
use crate::world::terrain::{tile_to_world, WORLD_CHUNKS_X, WORLD_CHUNKS_Y};
use crate::world::tile::TileKind;

use super::carry::Carrier;
use super::combat::{Body, CombatCooldown, CombatTarget};
use super::faction::{
    FactionCenter, FactionMember, FactionRegistry, FactionStorageTile, PlayerFaction,
    PlayerFactionMarker,
};
use super::goals::{AgentGoal, Personality};
use super::items::{Equipment, TargetItem};
use super::lod::LodLevel;
use super::memory::{AgentMemory, RelationshipMemory};
use super::mood::Mood;
use super::movement::MovementState;
use super::needs::Needs;
use super::plan::{KnownPlans, PlanHistory, PlanScoringMethod};
use super::reproduction::BiologicalSex;
use super::schedule::{BucketSlot, SimClock};
use super::skills::Skills;
use super::stats::Stats;
use crate::pathfinding::path_request::PathFollow;

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
    Idle = 0,
    Working = 1,
    Seeking = 2,
    Sleeping = 3,
    Routing = 4,
    Attacking = 5,
}

#[derive(Component, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Profession {
    #[default]
    None,
    Farmer,
    Hunter,
}

/// Player-controlled target Hunter headcount. `assign_hunters_system`
/// promotes the highest-Combat-skill agents up to this count and demotes
/// excess Hunters when the player lowers it.
#[derive(Resource, Default, Clone, Copy)]
pub struct HunterTargetCount {
    pub count: u32,
}

#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub enum SkinTone {
    Tan,
    Pale,
    Dark,
}

impl SkinTone {
    pub fn random() -> Self {
        match fastrand::u8(0..3) {
            0 => Self::Pale,
            1 => Self::Dark,
            _ => Self::Tan,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tan => "tan",
            Self::Pale => "pale",
            Self::Dark => "dark",
        }
    }
}

#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub enum HairColor {
    Brown,
    Black,
    Blonde,
    White,
}

impl HairColor {
    pub fn random() -> Self {
        match fastrand::u8(0..4) {
            0 => Self::Black,
            1 => Self::Blonde,
            2 => Self::White,
            _ => Self::Brown,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Brown => "brown",
            Self::Black => "black",
            Self::Blonde => "blonde",
            Self::White => "white",
        }
    }
}

const MALE_NAMES: &[&str] = &[
    "Aldric", "Bram", "Caius", "Davan", "Eryn", "Finn", "Garic", "Holt", "Idris", "Jorn", "Kael",
    "Lund", "Maren", "Nils", "Orin", "Pell", "Rath", "Soren", "Tor", "Ulric", "Vael", "Wynn",
    "Xeno", "Yorn", "Zane",
];
const FEMALE_NAMES: &[&str] = &[
    "Asha", "Brea", "Calla", "Dwyn", "Elara", "Faye", "Gara", "Hira", "Inna", "Jova", "Kela",
    "Lyra", "Mira", "Nara", "Ora", "Pira", "Rhea", "Saya", "Tara", "Una", "Vira", "Wren", "Xara",
    "Yara", "Zola",
];

pub fn generate_person_name(sex: BiologicalSex) -> &'static str {
    let list = match sex {
        BiologicalSex::Male => MALE_NAMES,
        BiologicalSex::Female => FEMALE_NAMES,
    };
    list[fastrand::usize(..list.len())]
}

/// Core person AI component.
#[derive(Component, Clone, Copy)]
pub struct PersonAI {
    pub task_id: u16,
    pub state: AiState,
    /// Progress ticks toward the next production event.
    pub work_progress: u8,
    pub target_tile: (i16, i16),
    pub dest_tile: (i16, i16),
    pub ticks_idle: u8,
    pub last_plan_id: u16,
    pub last_goal_eval_tick: u64,
    pub target_entity: Option<Entity>,
    /// The agent's current foot Z (the floor they stand on). Set at spawn
    /// to surface_z and updated as they walk over ramps or dig down.
    pub current_z: i8,
    /// Destination foot Z when routed across Z slices (e.g. from a
    /// PlayerOrder targeting underground). Equal to current_z by default.
    pub target_z: i8,
    /// Recipe index for active Craft tasks. Written by `plan.rs` on
    /// dispatch, read by `crafting.rs`. Unrelated to target_z.
    pub craft_recipe_id: u8,
    /// Material the agent has committed to withdraw on the next
    /// `TaskKind::WithdrawMaterial` step. Written by the step resolver at
    /// dispatch time, consumed (and cleared) by `withdraw_material_task_system`.
    /// `None` means no targeted withdrawal is active.
    pub withdraw_good: Option<crate::economy::goods::Good>,
    /// Upper bound on units to take when fulfilling `withdraw_good`. Bounded
    /// by carrier capacity at execution time, so a value larger than the hands
    /// can hold is harmless.
    pub withdraw_qty: u8,
    /// Tile against which the current `StorageReservations` entry is held.
    /// Tracked separately from `dest_tile` so we can release the reservation
    /// even after the agent has been retargeted.
    pub reserved_tile: (i16, i16),
    /// Good promised to the storage tile via `StorageReservations`. `None`
    /// means no reservation is currently active.
    pub reserved_good: Option<crate::economy::goods::Good>,
    /// Reserved quantity. The reservation is decremented by exactly this many
    /// units when the task ends (success, abort, or plan teardown), so the
    /// fields must be kept in sync with the actual `StorageReservations` map.
    pub reserved_qty: u8,
    /// Corpse the agent is currently dragging. Treated as occupying both
    /// hands by `Carrier::pickup_capacity`. Set by `pickup_corpse_task_system`,
    /// cleared by Butcher / drop-on-rescue / corpse-despawn paths. Drives
    /// `corpse_follow_system` which snaps the corpse `Transform` to the agent.
    pub carried_corpse: Option<Entity>,
    /// Equipment slot for an active `TaskKind::Equip` step, encoded as
    /// `EquipmentSlot as u8`. `EQUIP_SLOT_NONE` (0xFF) means no equip is
    /// pending. Set by `plan_execution_system` on dispatch and consumed by
    /// `equip_task_system`. The good to equip is carried in
    /// `craft_recipe_id` (same channel WithdrawGood already uses).
    pub equip_slot: u8,
}

impl PersonAI {
    pub const UNEMPLOYED: u16 = u16::MAX;
}

impl Default for PersonAI {
    fn default() -> Self {
        Self {
            task_id: 0,
            state: AiState::default(),
            work_progress: 0,
            target_tile: (0, 0),
            dest_tile: (0, 0),
            ticks_idle: 0,
            last_plan_id: 0,
            last_goal_eval_tick: 0,
            target_entity: None,
            current_z: 0,
            target_z: 0,
            craft_recipe_id: 0,
            withdraw_good: None,
            withdraw_qty: 0,
            reserved_tile: (0, 0),
            reserved_good: None,
            reserved_qty: 0,
            carried_corpse: None,
            equip_slot: crate::simulation::items::EQUIP_SLOT_NONE,
        }
    }
}

/// Player-issued order that overrides autonomous AI for this entity.
#[derive(Component, Clone, Copy, Debug)]
pub struct PlayerOrder {
    pub order: PlayerOrderKind,
    pub target_tile: (i16, i16),
    /// Foot Z of the target. For surface clicks, equal to the tile's surface_z.
    /// For underground clicks (CameraViewZ != i32::MAX), this is the camera Z.
    pub target_z: i8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerOrderKind {
    Move,
    Mine,
    Gather,
    PickUp,
    Build(crate::simulation::construction::BuildSiteKind),
    DigDown,
    Deconstruct,
}

impl PlayerOrderKind {
    pub fn label(self) -> &'static str {
        use crate::simulation::construction::{BuildSiteKind, WallMaterial};
        match self {
            PlayerOrderKind::Move => "Move here",
            PlayerOrderKind::Mine => "Mine",
            PlayerOrderKind::Gather => "Gather",
            PlayerOrderKind::PickUp => "Pick up",
            PlayerOrderKind::Build(kind) => match kind {
                BuildSiteKind::Wall(WallMaterial::Palisade) => "Build Palisade",
                BuildSiteKind::Wall(WallMaterial::WattleDaub) => "Build Wattle Wall",
                BuildSiteKind::Wall(WallMaterial::Stone) => "Build Stone Wall",
                BuildSiteKind::Wall(WallMaterial::Mudbrick) => "Build Mudbrick Wall",
                BuildSiteKind::Wall(WallMaterial::CutStone) => "Build Cut Stone Wall",
                BuildSiteKind::Door => "Build Door",
                BuildSiteKind::Bed => "Build Bed",
                BuildSiteKind::Campfire => "Build Campfire",
                BuildSiteKind::Workbench => "Build Workbench",
                BuildSiteKind::Loom => "Build Loom",
                BuildSiteKind::Table => "Build Table",
                BuildSiteKind::Chair => "Build Chair",
                BuildSiteKind::Granary => "Build Granary",
                BuildSiteKind::Shrine => "Build Shrine",
                BuildSiteKind::Market => "Build Market",
                BuildSiteKind::Barracks => "Build Barracks",
                BuildSiteKind::Monument => "Build Monument",
            },
            PlayerOrderKind::DigDown => "Dig Down",
            PlayerOrderKind::Deconstruct => "Deconstruct",
        }
    }
}

/// Marker for a person entity.
#[derive(Component)]
pub struct Person;

/// Persistent player-issued military mode. While present, the agent skips
/// autonomous goal selection (gathering, hauling, socializing, etc.) and
/// only acts on player orders. Toggled by the HUD Draft button.
#[derive(Component, Default)]
pub struct Drafted;

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
    let now = Instant::now();
    use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};

    let start_cx = ((GLOBE_WIDTH / 2) * GLOBE_CELL_CHUNKS) - (WORLD_CHUNKS_X / 2);
    let start_cy = ((GLOBE_HEIGHT / 2) * GLOBE_CELL_CHUNKS) - (WORLD_CHUNKS_Y / 2);

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
                        (dx * dx + dy * dy).sqrt() < 300.0
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

        let Some((home_tx, home_ty)) = home else {
            continue;
        };
        spawned_homes.push((home_tx, home_ty));

        let faction_id = registry.create_faction((home_tx as i16, home_ty as i16));

        let home_world = tile_to_world(home_tx, home_ty);

        // Spawn a storage tile marker for every faction at its home tile
        commands.spawn((
            FactionStorageTile { faction_id },
            Transform::from_xyz(home_world.x, home_world.y, 0.5),
            GlobalTransform::default(),
            Visibility::Hidden,
            InheritedVisibility::default(),
        ));

        if group_idx == 0 {
            player_faction.faction_id = faction_id;

            // Mark the player's faction center
            commands.spawn((
                FactionCenter,
                PlayerFactionMarker,
                Transform::from_xyz(home_world.x, home_world.y, 0.5),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ));
        }

        for _ in 0..GROUP_SIZE {
            let Some((tx, ty)) = find_tile(&mut rng, home_tx, home_ty) else {
                continue;
            };

            let world_pos = tile_to_world(tx, ty);
            let sex = BiologicalSex::random();

            commands.spawn((
                (
                    Person,
                    Transform::from_xyz(world_pos.x, world_pos.y, 1.0),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    Needs::new(30.0, 20.0, 10.0, 5.0, 40.0, 200.0),
                    Mood::default(),
                    Skills::default(),
                    Stats::roll_3d6(),
                    PersonAI {
                        task_id: PersonAI::UNEMPLOYED,
                        state: AiState::Idle,
                        target_tile: (tx as i16, ty as i16),
                        dest_tile: (tx as i16, ty as i16),
                        last_plan_id: PersonAI::UNEMPLOYED,
                        current_z: chunk_map.surface_z_at(tx, ty) as i8,
                        target_z: chunk_map.surface_z_at(tx, ty) as i8,
                        ..PersonAI::default()
                    },
                    EconomicAgent::default(),
                ),
                (
                    LodLevel::Full,
                    BucketSlot(spawned),
                    MovementState {
                        wander_timer: (spawned % 100) as f32 * 0.025,
                        ..Default::default()
                    },
                    sex,
                    SkinTone::random(),
                    HairColor::random(),
                    Personality::random(),
                    AgentGoal::default(),
                    Profession::None,
                    FactionMember {
                        faction_id,
                        ..Default::default()
                    },
                    Body::new_humanoid(),
                    Equipment::default(),
                    TargetItem::default(),
                    CombatTarget::default(),
                    CombatCooldown::default(),
                ),
                (
                    AgentMemory::default(),
                    RelationshipMemory::default(),
                    KnownPlans::with_innate(&[
    0, 1, 2, 3, 5, 6, 7, 9, 10, 13, 14, 15, 16, 23, 24, 25, 26, 27, 29, 30, 31, 32, 33, 34, 35,
    36, 37, 38, 39, 60, 61, 62, 63, 64,
]),
                    PlanHistory::default(),
                    PlanScoringMethod::Weighted,
                    Name::new(generate_person_name(sex)),
                    PathFollow::default(),
                    Carrier::default(),
                    crate::simulation::reproduction::CoSleepTracker::default(),
                    crate::simulation::reproduction::MaleConceptionCooldown::default(),
                ),
            ));

            registry.add_member(faction_id);
            spawned += 1;
        }
    }

    clock.population = spawned;
    clock.current_end = clock.bucket_size.min(spawned);

    info!(
        "Spawned {} people in {} factions of {} in {:?}",
        spawned,
        num_groups,
        GROUP_SIZE,
        now.elapsed()
    );
}
