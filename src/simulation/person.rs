use bevy::prelude::*;
use rand::Rng;
use std::time::Instant;

use crate::economy::agent::EconomicAgent;
use crate::world::chunk::{ChunkMap, CHUNK_SIZE};
use crate::world::spatial::{Indexed, IndexedKind};
use crate::world::terrain::{tile_to_world, WORLD_CHUNKS_X, WORLD_CHUNKS_Y};
use crate::world::tile::TileKind;

use super::carry::Carrier;
use super::combat::{Body, CombatCooldown, CombatTarget};
use super::faction::{
    FactionCenter, FactionChief, FactionMember, FactionRegistry, FactionStorageTile, PlayerFaction,
    PlayerFactionMarker,
};
use super::goals::{AgentGoal, Personality};
use super::items::{Equipment, TargetItem};
use super::lod::LodLevel;
use super::memory::{AgentMemory, RelationshipMemory};
use super::mood::Mood;
use super::movement::MovementState;
use super::needs::Needs;
use super::htn::{MethodHistory, MethodId};
use super::reproduction::BiologicalSex;
use super::schedule::{BucketSlot, SimClock};
use super::knowledge::PersonKnowledge;
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
    /// Pluralist Economy R5: physical government official. Appointed
    /// by the chief when the faction's `state_funds_public_works`
    /// flag is true; paid a per-day wage from the settlement
    /// treasury. Posts public-works jobs (R6+). Demotes when the
    /// treasury empty-streak crosses `BUREAUCRAT_QUIT_DAYS`.
    Bureaucrat,
    /// Pluralist Economy R10: market arbitrageur. Walks between
    /// settlements with price gaps, buying low and selling high.
    /// Currency settles via `pay()` against settlement market state.
    /// Autonomous dispatch lives in `trader::autonomous_trader_dispatch_system`
    /// (R10 follow-on): a deterministic state machine driven by
    /// `TraderPlan` mirroring the Bureaucrat single-system pattern.
    Trader,
}

/// Pluralist Economy R10 follow-on: per-trader arbitrage state. Tracks
/// which settlement pair the trader is shuttling between, what
/// resource, and which leg of the cycle they're on. Removed when the
/// cycle completes (after the sell leg) so the next dispatch tick
/// re-scans for the best gap.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraderPlan {
    pub phase: TraderPhase,
    pub buy_settlement: crate::simulation::settlement::SettlementId,
    pub sell_settlement: crate::simulation::settlement::SettlementId,
    pub resource_id: crate::economy::resource_catalog::ResourceId,
    pub qty: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraderPhase {
    /// Heading to the cheap market with currency.
    TravelingToBuy,
    /// Heading to the expensive market with bought goods.
    TravelingToSell,
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
    pub target_tile: (i32, i32),
    pub dest_tile: (i32, i32),
    pub ticks_idle: u8,
    pub last_goal_eval_tick: u64,
    pub target_entity: Option<Entity>,
    /// The agent's current foot Z (the floor they stand on). Set at spawn
    /// to surface_z and updated as they walk over ramps or dig down.
    pub current_z: i8,
    /// Destination foot Z when routed across Z slices (e.g. from a
    /// PlayerOrder targeting underground). Equal to current_z by default.
    pub target_z: i8,
    /// Tile against which the current `StorageReservations` entry is held.
    /// Tracked separately from `dest_tile` so we can release the reservation
    /// even after the agent has been retargeted.
    pub reserved_tile: (i32, i32),
    /// Catalog `ResourceId` promised to the storage tile via
    /// `StorageReservations`. `None` means no reservation is currently active.
    pub reserved_resource: Option<crate::economy::resource_catalog::ResourceId>,
    /// Reserved quantity. The reservation is decremented by exactly this many
    /// units when the task ends (success, abort, or plan teardown), so the
    /// fields must be kept in sync with the actual `StorageReservations` map.
    pub reserved_qty: u8,
    /// The HTN `MethodId` whose expansion produced the agent's currently-running
    /// task chain. Stamped by each `htn_*_dispatch_system` after a successful
    /// dispatch and cleared by `htn_method_completion_system` (Sequential, after
    /// executors) when the chain drains to `Task::Idle`. Failure dispatch paths
    /// also clear it before pushing `MethodOutcome::FailedRouting` /
    /// `FailedTarget` onto `MethodHistory`. `None` when no HTN-driven chain is
    /// in flight (legacy plans, player orders, sleep states between dispatches).
    pub active_method: Option<MethodId>,
    /// Outstanding `GatherClaims` entry held by this agent. Set by HTN
    /// dispatchers when picking a gather/scavenge target tile from
    /// `SharedKnowledge`, cleared by gather/scavenge finish helpers via
    /// `release_gather_claim`. `None` when no gather chain is in flight.
    pub active_gather_claim: Option<((i32, i32), crate::simulation::memory::MemoryKind)>,
    /// Last tick `gather_system` re-targeted this agent's `Task::Gather` to a
    /// neighboring tile after arriving to find the original plant despawned
    /// or immature (P6b). Throttles retargeting to one swap per ~40 ticks so
    /// a stale-cluster reflex can't loop forever.
    pub last_retarget_tick: u64,
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
            last_goal_eval_tick: 0,
            target_entity: None,
            current_z: 0,
            target_z: 0,
            reserved_tile: (0, 0),
            reserved_resource: None,
            reserved_qty: 0,
            active_method: None,
            active_gather_claim: None,
            last_retarget_tick: 0,
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
    mut settled: ResMut<crate::simulation::region::SettledRegions>,
    pending: Res<crate::PendingSpawn>,
    options: Res<crate::GameStartOptions>,
    catalog: Res<crate::economy::resource_catalog::ResourceCatalog>,
    archetype_registry: Res<crate::simulation::archetype::FactionArchetypeRegistry>,
) {
    let now = Instant::now();
    use crate::simulation::region::MegaChunkCoord;
    use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH, MEGACHUNK_SIZE_CHUNKS};

    // Centre the spawn region on the player-picked mega-chunk; fall back to
    // globe centre if nothing was picked.
    let (center_cx, center_cy) = match pending.0 {
        Some((mx, my)) => (
            mx * MEGACHUNK_SIZE_CHUNKS + MEGACHUNK_SIZE_CHUNKS / 2,
            my * MEGACHUNK_SIZE_CHUNKS + MEGACHUNK_SIZE_CHUNKS / 2,
        ),
        None => (
            (GLOBE_WIDTH / 2) * GLOBE_CELL_CHUNKS,
            (GLOBE_HEIGHT / 2) * GLOBE_CELL_CHUNKS,
        ),
    };
    let start_cx = center_cx - (WORLD_CHUNKS_X / 2);
    let start_cy = center_cy - (WORLD_CHUNKS_Y / 2);

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

    // Score a candidate home tile: positive = better. Combines river-proximity
    // (riverside camps preferred over arid interior) with the existing
    // 300-tile inter-faction spacing. Inside the channel itself is penalised
    // (we don't want spawns on water tiles even though that's already
    // rejected by the passability check — guards against future passable
    // shallow-water variants).
    let score_home_candidate = |tx: i32, ty: i32, others: &[(i32, i32)]| -> i32 {
        let too_close = others.iter().any(|(hx, hy)| {
            let dx = (tx - hx) as f32;
            let dy = (ty - hy) as f32;
            (dx * dx + dy * dy).sqrt() < 300.0
        });
        let spacing = if too_close { -100 } else { 50 };
        let river_score = match chunk_map.river_distance_at(tx, ty) {
            0..=1 => -50,
            2..=4 => 60,
            5..=8 => 30,
            _ => 0,
        };
        spacing + river_score
    };

    for group_idx in 0..num_groups {
        // Find a home tile for this group anywhere in the spawn region.
        // Best-of-N over 200 candidates so river proximity nudges the pick
        // without hard-rejecting otherwise-fine inland tiles.
        let home = {
            let mut best: Option<((i32, i32), i32)> = None;
            for _ in 0..200 {
                let tx = start_tx + rng.gen_range(0..total_tiles_x);
                let ty = start_ty + rng.gen_range(0..total_tiles_y);
                if !chunk_map.is_passable(tx, ty)
                    || matches!(chunk_map.tile_kind_at(tx, ty), Some(TileKind::Stone))
                {
                    continue;
                }
                let score = score_home_candidate(tx, ty, &spawned_homes);
                if best.as_ref().map_or(true, |(_, s)| score > *s) {
                    best = Some(((tx, ty), score));
                }
            }
            // Fallback: if no candidate scored well, take any passable tile.
            best.map(|(t, _)| t).or_else(|| {
                let mut result = None;
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
                result
            })
        };

        let Some((home_tx, home_ty)) = home else {
            continue;
        };
        spawned_homes.push((home_tx, home_ty));

        let faction_id = registry.create_faction((home_tx as i32, home_ty as i32));

        // Apply economy preset to this faction's `economic_policy` map.
        if let Some(faction_data) = registry.factions.get_mut(&faction_id) {
            crate::economy::policy::apply_preset(
                &mut faction_data.economic_policy,
                options.economy,
                &catalog,
            );
            faction_data.land_policy = crate::economy::policy::land_policy_for(options.economy);
            // Apply lifestyle: only the player faction reads the user-picked
            // option. AI factions stay Settled by default. Households inherit
            // via `spawn_household` so a nomadic player faction's bonded pairs
            // form nomadic households automatically.
            if group_idx == 0 {
                faction_data.lifestyle = options.lifestyle;
            }
            // P5: route through the archetype registry. `derive_from_archetype_key`
            // hits the registry when the key is authored (today: the
            // four supported legacy archetypes inserted by
            // `default_registry`); falls back to `derive_from_legacy`
            // for any caller that hands us an unknown key. The fallback
            // is what keeps the bit-for-bit P1a invariant intact while
            // RON loading lands.
            let key = crate::simulation::archetype::legacy_archetype_key(
                faction_data.lifestyle,
                options.economy,
            );
            faction_data.caps = crate::simulation::archetype::derive_from_archetype_key(
                &archetype_registry,
                key,
                Some((faction_data.lifestyle, options.economy, &catalog)),
            )
            .expect("derive_from_archetype_key with legacy fallback always returns Some");
        }

        let home_world = tile_to_world(home_tx, home_ty);

        // Settled factions get a fixed storage tile at home. Nomadic factions
        // skip this — their `FactionStorage.totals` are pooled across member /
        // pack-animal / PackBundle inventories (Phase 4 backend split). The
        // tile would be misleading: it'd accept deposits but not travel with
        // the band on migration.
        // Capability check: only `FactionTile` storage backends spawn a tile.
        let storage_kind = registry
            .factions
            .get(&faction_id)
            .map(|d| d.caps.storage)
            .unwrap_or(crate::simulation::archetype::StorageBackendKind::FactionTile);
        if matches!(
            storage_kind,
            crate::simulation::archetype::StorageBackendKind::FactionTile
                | crate::simulation::archetype::StorageBackendKind::Hybrid
        ) {
            commands.spawn((
                FactionStorageTile { faction_id },
                Transform::from_xyz(home_world.x, home_world.y, 0.5),
                GlobalTransform::default(),
                Visibility::Hidden,
                InheritedVisibility::default(),
            ));
        }

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

            // Seed the player's first settled region.
            let megachunk = MegaChunkCoord::from_tile(home_tx, home_ty);
            settled.settle(
                megachunk,
                clock.tick,
                "Home".to_string(),
                home_world,
                true,
            );
        }

        // Player faction respects the user-chosen population; AI factions
        // stay at the hardcoded GROUP_SIZE.
        let group_size = if group_idx == 0 {
            options.player_population
        } else {
            GROUP_SIZE
        };

        let mut first_member: Option<Entity> = None;
        // M2 (Market-preset households): collect every spawned adult so we can
        // form a one-member household per worker after the spawn loop. Each
        // capitalist worker then has its own storage tile + seeded treasury
        // at tick 0, so household contracts can post immediately rather than
        // waiting on cosleep-bond formation (which takes a full game-week).
        let mut spawned_members: Vec<Entity> = Vec::with_capacity(group_size as usize);
        for _ in 0..group_size {
            let Some((tx, ty)) = find_tile(&mut rng, home_tx, home_ty) else {
                continue;
            };

            let world_pos = tile_to_world(tx, ty);
            let sex = BiologicalSex::random();

            let person_entity = commands.spawn((
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
                        target_tile: (tx as i32, ty as i32),
                        dest_tile: (tx as i32, ty as i32),
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
                    MethodHistory::default(),
                    crate::simulation::memory::CurrentVision::default(),
                    Name::new(generate_person_name(sex)),
                    PathFollow::default(),
                    Carrier::default(),
                    crate::simulation::reproduction::CoSleepTracker::default(),
                    crate::simulation::reproduction::MaleConceptionCooldown::default(),
                    Indexed::new(IndexedKind::Person),
                    PersonKnowledge::seeded_through_era(options.era, clock.tick as u32),
                    crate::simulation::typed_task::ActionQueue::idle(),
                ),
            )).id();

            if first_member.is_none() {
                first_member = Some(person_entity);
            }
            spawned_members.push(person_entity);
            registry.add_member(faction_id);
            spawned += 1;
        }

        // Designate the first spawned member as chief. Without this,
        // chief-driven systems (chief_directive_system, chief_job_posting,
        // chief_hunt_order, chief_tablet_posting) wait for a runtime
        // bonding event that may never fire on freshly seeded factions.
        if let Some(chief) = first_member {
            if let Some(faction_data) = registry.factions.get_mut(&faction_id) {
                faction_data.chief_entity = Some(chief);
            }
            commands.entity(chief).insert(FactionChief);
        }

        // M2: under Market preset, every spawned adult founds a one-person
        // household with its own home tile near the village center, its own
        // `FactionStorageTile`, and a seeded treasury so it can post paid
        // contracts on the next `HOUSEHOLD_POSTING_CADENCE` cycle. Cosleep
        // bond households continue to form later via
        // `household_formation_system`. Subsistence/Mixed presets skip this
        // — those villages keep household formation gated on bonding.
        // Capability check: archetypes whose `inheritance.seed_storage_tile`
        // is true seed one-person households at spawn (today: Market only).
        // Subsistence/Mixed villages keep household formation gated on
        // bonding (`household_formation_system`).
        let seed_households_for_archetype = registry
            .factions
            .get(&faction_id)
            .map(|d| d.caps.inheritance.seed_storage_tile)
            .unwrap_or(false);
        if seed_households_for_archetype {
            seed_market_households(
                &mut commands,
                &mut registry,
                &chunk_map,
                &catalog,
                faction_id,
                (home_tx as i32, home_ty as i32),
                &spawned_members,
            );
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

/// Form one household per spawned adult under the Market preset, each with
/// its own plot tile near the village home, a `FactionStorageTile`, and a
/// `HOUSEHOLD_SEED_TREASURY` so contract posting can fire on the first
/// cadence cycle.
///
/// Caller (the Market branch of `spawn_population`) owns the iteration; this
/// helper just executes the per-household side-effects so it can be unit-
/// tested without driving the full spawn pipeline.
pub(crate) fn seed_market_households(
    commands: &mut Commands,
    registry: &mut FactionRegistry,
    chunk_map: &ChunkMap,
    catalog: &crate::economy::resource_catalog::ResourceCatalog,
    village_faction_id: u32,
    village_home: (i32, i32),
    members: &[Entity],
) {
    use ahash::AHashSet;
    let mut used: AHashSet<(i32, i32)> = AHashSet::new();
    used.insert(village_home);
    for &member in members {
        let plot = match crate::simulation::construction::next_clear_tile(
            village_home,
            &used,
            chunk_map,
            16,
        ) {
            Some(t) => t,
            None => continue,
        };
        used.insert(plot);
        let household_id =
            registry.spawn_household(village_faction_id, plot, member, catalog);
        if let Some(hh) = registry.factions.get_mut(&household_id) {
            hh.treasury = crate::simulation::faction::HOUSEHOLD_SEED_TREASURY;
            hh.member_count = 1;
        }
        let plot_world = tile_to_world(plot.0, plot.1);
        commands.spawn((
            FactionStorageTile {
                faction_id: household_id,
            },
            Transform::from_xyz(plot_world.x, plot_world.y, 0.5),
            GlobalTransform::default(),
            Visibility::Hidden,
            InheritedVisibility::default(),
        ));
        commands
            .entity(member)
            .insert(crate::simulation::reproduction::HouseholdMember {
                household_id,
            });
    }
}
