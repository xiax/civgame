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
use super::htn::{MethodHistory, MethodId};
use super::items::{Equipment, TargetItem};
use super::knowledge::PersonKnowledge;
use super::lod::LodLevel;
use super::memory::{AgentMemory, RelationshipMemory};
use super::mood::Mood;
use super::movement::MovementState;
use super::needs::Needs;
use super::reproduction::BiologicalSex;
use super::schedule::{BucketSlot, SimClock};
use super::skills::{SkillPeaks, SkillUseTicks, Skills, SkillsLastSeen};
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

#[derive(Component, Default, Clone, Copy, PartialEq, Eq, Hash, Debug)]
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
    /// Phase 5a (wage-aware-labor-market-v2): dedicated craft worker.
    /// Promoted by `chief_craft_assignment_system` when the faction's
    /// `wage_signal[(Craft, _)].ema_per_day > 0` (i.e. paid craft work
    /// is actively being posted). Workshop-affine via `WorkshopKind::
    /// affine_to`: Workbench/Loom both lift Crafter EV. Tool affinity
    /// via the catalog `tools` resource. Primary skill: Crafting.
    /// Phase 5b apprenticeship layers on top — sub-`APPRENTICE_THRESHOLD`
    /// candidates route through `Profession::Apprentice` instead.
    Crafter,
    /// Phase 5b (wage-aware-labor-market-v2): novice training toward
    /// `Crafter`. Routed by `chief_craft_assignment_system` when a
    /// low-`Crafting` (< `APPRENTICE_THRESHOLD`) candidate is bound to
    /// a same-faction master via `ApprenticeOf` / `MentorOf`. Lives in
    /// the role for `APPRENTICESHIP_DURATION_DAYS` of in-game time;
    /// `apprentice_progress_system` ratchets `ApprenticeProgress.ticks`
    /// daily and graduates to `Crafter` on completion. Profession-choice
    /// systems treat Apprentices as committed — they're skipped from
    /// Farmer / Hunter / Bureaucrat promotion pools.
    Apprentice,
    /// Phase 5b-stretch (wage-aware-labor-market-v2): medicine
    /// practitioner. Currently *scaffolding-only* — the variant is
    /// recognised by `profession_choice` (primary skill = Medicine;
    /// shrine-affine workshop), the cross-profession switcher's
    /// `faction_cap_for`, and the inspector's EV table, but no
    /// `chief_heal_assignment_system` exists yet to promote Healers
    /// from `None`. A future Heal-job pipeline (post-paid heal contracts
    /// when members are injured, executed against `Health` deltas) is
    /// the precondition for Healers to receive a wage signal and
    /// therefore become EV-promotable. Until then, the variant lands
    /// additively so downstream consumers (capital affinity, inspector,
    /// apprenticeship plumbing) don't have to be re-wired when the
    /// Heal-job pipeline lands.
    Healer,
    /// Knowledge-posted construction (sleepy-dove plan): per-settlement
    /// appointee whose Learned construction techs cover gaps the chief
    /// lacks. Authors `Blueprint.posted_by` for runtime builds so design
    /// tiers reflect the architect's tech set, not the chief's. Variant
    /// is additive scaffolding — the appointment system + poster pool
    /// integration is deferred (see `plans/evaluate-the-users-xiao1-civgame-plans-k-sleepy-dove.md`).
    Architect,
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
///
/// "What task is running" lives on the typed `ActionQueue` component —
/// `aq.current` is canonical and `aq.current_task_kind()` projects it back
/// to the legacy `TaskKind` discriminant for consumers that still read it.
#[derive(Component, Clone, Copy)]
pub struct PersonAI {
    /// Encapsulated. **Do not mutate directly.** Transitions go through
    /// `ActionQueue` methods (`begin_working` / `begin_seeking` /
    /// `begin_routing` / `begin_sleeping` / `begin_attacking` /
    /// `finish_task` / `cancel_chain`) so `aq.current` and `ai.state` stay
    /// consistent atomically. The orphan shape
    /// (`current != Task::Idle && state == AiState::Idle`) is then
    /// unrepresentable: see `src/simulation/typed_task.rs` and the
    /// `Eliminate Orphan Task States` plan for the rationale.
    pub(in crate::simulation) state: AiState,
    /// Progress ticks toward the next production event.
    pub work_progress: u8,
    /// Sub-tick remainder of slowdown-scaled work accrual. `movement_system`
    /// accumulates `base * factor` here; whole units roll into `work_progress`
    /// and the fraction persists across ticks so a 0.45-factor worker still
    /// makes 1 progress every ~2.22 ticks instead of stalling at 0 forever.
    /// Zeroed at task entry/exit by `ActionQueue::{begin_working, finish_task,
    /// cancel_chain}`.
    pub(in crate::simulation) work_progress_fraction: f32,
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
    pub reserved_qty: u32,
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
    /// Carries the full typed `GatherTarget` (tile + kind + source tier +
    /// cluster id) so failure paths can invalidate the *exact* cluster
    /// that produced the pick, across every tier it was promoted into.
    pub active_gather_claim: Option<crate::simulation::shared_knowledge::GatherTarget>,
    /// Last tick `gather_system` re-targeted this agent's `Task::Gather` to a
    /// neighboring tile after arriving to find the original plant despawned
    /// or immature (P6b). Throttles retargeting to one swap per ~40 ticks so
    /// a stale-cluster reflex can't loop forever.
    pub last_retarget_tick: u64,
    /// Last tick this agent stole food during a raid. The raid executor
    /// enforces a per-raider steal cooldown (`RAID_STEAL_COOLDOWN_TICKS`)
    /// so a single raider can't drain a storage tile in one visit.
    pub last_raid_steal_tick: u32,
}

/// Re-export of the typed-task sentinel for "no current task".
///
/// Pre-Phase-4-step-3 this lived on `PersonAI::UNEMPLOYED`; the field-side
/// const went away when the legacy `task_id` mirror was deleted. Callers that
/// compare `ActionQueue::current_task_kind()` against the legacy `TaskKind`
/// space use this constant; new sites should prefer `aq.is_idle()`.
pub use crate::simulation::typed_task::UNEMPLOYED_TASK_KIND;

impl Default for PersonAI {
    fn default() -> Self {
        Self {
            state: AiState::default(),
            work_progress: 0,
            work_progress_fraction: 0.0,
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
            last_raid_steal_tick: 0,
        }
    }
}

impl PersonAI {
    /// Read-only accessor for the encapsulated AI state. The field is
    /// `pub(in crate::simulation)` so the simulation module's `ActionQueue`
    /// methods can mutate it (atomic with `aq.current`), while everywhere
    /// else reads through this getter. See the field doc on `state` for the
    /// rationale.
    #[inline]
    pub fn state(&self) -> AiState {
        self.state
    }

    /// Accumulate slowdown-scaled work progress, preserving the sub-tick
    /// remainder across ticks. Drops non-positive / NaN inputs silently.
    /// Whole units roll into `work_progress` (saturating at `u8::MAX`); the
    /// fractional part stays in `work_progress_fraction` until the next call
    /// pushes it past 1.0.
    #[inline]
    pub fn add_work_progress(&mut self, amount: f32) {
        if !(amount > 0.0) {
            return;
        }
        let total = self.work_progress_fraction + amount;
        let whole = total.floor();
        self.work_progress_fraction = total - whole;
        let whole_u8 = whole.min(u8::MAX as f32) as u8;
        self.work_progress = self.work_progress.saturating_add(whole_u8);
    }

    /// Constructor for placement-time spawn sites outside the simulation
    /// module (e.g. `sandbox.rs`). `state` defaults to `AiState::Idle` so
    /// callers don't need to name the encapsulated field.
    pub fn placed_at(tile: (i32, i32), z: i8) -> Self {
        Self {
            target_tile: tile,
            dest_tile: tile,
            current_z: z,
            target_z: z,
            ..Self::default()
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
pub(crate) const GROUP_SIZE: u32 = 20;
const SPAWN_RADIUS: i32 = 12;

/// Number of rival factions spawned as full entity groups near the player at
/// game start. The player faction plus this many rivals materialise in the
/// pre-generated spawn window; every other faction slot lives abstractly on
/// the world map (see `abstract_faction.rs`) until the player travels near it.
pub(crate) const NEARBY_RIVAL_COUNT: u32 = 3;

/// Inter-faction spacing reward saturates here (tiles). A candidate home this
/// far (or farther) from every already-placed home scores full marks. Not a
/// hard minimum — when the window can't satisfy it the scorer still spreads
/// the near factions as far apart as geometry allows.
const NEAR_FACTION_TARGET_SPACING: f32 = 280.0;

/// Continuous farthest-point spacing reward for a candidate faction home tile.
/// `0` when coincident with an existing home, saturating to `+100` once the
/// candidate is `>= NEAR_FACTION_TARGET_SPACING` from every placed home.
/// Empty `others` (the first faction placed) → full `+100`.
///
/// Replaces the former binary within-300-tiles `-100/+50` penalty, which
/// silently degraded once placed exclusion circles tiled the spawn window and
/// let later factions cluster.
fn faction_spacing_score(tx: i32, ty: i32, others: &[(i32, i32)]) -> i32 {
    let min_dist = others
        .iter()
        .map(|(hx, hy)| {
            let dx = (tx - hx) as f32;
            let dy = (ty - hy) as f32;
            (dx * dx + dy * dy).sqrt()
        })
        .fold(f32::INFINITY, f32::min);
    ((min_dist / NEAR_FACTION_TARGET_SPACING).min(1.0) * 100.0) as i32
}

pub fn spawn_population(
    mut commands: Commands,
    chunk_map: Res<ChunkMap>,
    globe: Res<crate::world::globe::Globe>,
    mut clock: ResMut<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    mut player_faction: ResMut<PlayerFaction>,
    mut controlled: ResMut<crate::simulation::faction::ControlledFactions>,
    mut settled: ResMut<crate::simulation::region::SettledRegions>,
    mut starts: ResMut<crate::PendingStarts>,
    net_cfg: Option<Res<crate::net::NetConfig>>,
    world_seed: Res<crate::WorldSeed>,
    options: Res<crate::GameStartOptions>,
    catalog: Res<crate::economy::resource_catalog::ResourceCatalog>,
    archetype_registry: Res<crate::simulation::archetype::FactionArchetypeRegistry>,
) {
    let now = Instant::now();
    use crate::simulation::region::MegaChunkCoord;
    use crate::world::globe::{
        GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH, MEGACHUNK_SIZE_CHUNKS,
    };

    // Ensure at least one slot exists so the legacy `PendingSpawn`-driven SP
    // path (e.g. headless test fixture that never went through MainMenu)
    // still spawns a player faction.
    if starts.slots.is_empty() {
        let mut slot =
            crate::game_state::PlayerStartSlot::singleplayer("Player", options.lifestyle);
        // Mirror in legacy `PendingSpawn` if the caller wrote one (e.g. the
        // headless test fixture). When both are set, `PendingStarts` wins.
        // (Resource access via `World::get_resource` so the fixture isn't
        // forced to add `PendingSpawn` if it doesn't use it.)
        // Note: we can't reach `World` from a system fn; in practice the
        // test fixture writes BOTH `PendingSpawn` and now also default
        // `PendingStarts`, so we read `PendingSpawn` indirectly via
        // `legacy_pending_spawn_compat_system` running first. Tests that
        // need a specific megachunk should write
        // `PendingStarts.slots[0].megachunk` directly — same one-line
        // change as MainMenu / spawn-select.
        if let Some(mc) = starts.primary_start {
            slot.megachunk = Some(mc);
        }
        starts.slots.push(slot);
    }

    // Multi-start centre = centroid of every slot's picked megachunk. Used
    // for the 32×32 AI rival search window. Slots without a pick fall back
    // to globe centre, mirroring the legacy `PendingSpawn::None` behaviour.
    let (center_cx, center_cy) = {
        let picks: Vec<(i32, i32)> = starts
            .slots
            .iter()
            .filter_map(|s| s.megachunk)
            .collect();
        if picks.is_empty() {
            (
                (GLOBE_WIDTH / 2) * GLOBE_CELL_CHUNKS,
                (GLOBE_HEIGHT / 2) * GLOBE_CELL_CHUNKS,
            )
        } else {
            let (sx, sy) = picks.iter().fold((0i64, 0i64), |(ax, ay), (mx, my)| {
                let cx = (*mx) * MEGACHUNK_SIZE_CHUNKS + MEGACHUNK_SIZE_CHUNKS / 2;
                let cy = (*my) * MEGACHUNK_SIZE_CHUNKS + MEGACHUNK_SIZE_CHUNKS / 2;
                (ax + cx as i64, ay + cy as i64)
            });
            let n = picks.len() as i64;
            ((sx / n) as i32, (sy / n) as i32)
        }
    };
    let start_cx = center_cx - (WORLD_CHUNKS_X / 2);
    let start_cy = center_cy - (WORLD_CHUNKS_Y / 2);

    let mut rng = rand::thread_rng();
    let total_tiles_x = WORLD_CHUNKS_X * CHUNK_SIZE as i32;
    let total_tiles_y = WORLD_CHUNKS_Y * CHUNK_SIZE as i32;

    let start_tx = start_cx * CHUNK_SIZE as i32;
    let start_ty = start_cy * CHUNK_SIZE as i32;

    let num_groups = INITIAL_POPULATION / GROUP_SIZE;
    // Player slots + `NEARBY_RIVAL_COUNT` rivals spawn as full entity groups
    // in the pre-generated window. Remaining slots seed abstractly via
    // `seed_abstract_factions_system`. Human slots eat into the cap before
    // AI rivals.
    let human_count = starts.slots.len() as u32;
    let near_factions = (NEARBY_RIVAL_COUNT + 1).min(num_groups);
    let ai_rival_count = near_factions.saturating_sub(human_count);
    let mut spawned = 0u32;
    let mut spawned_homes: Vec<(i32, i32)> = Vec::new();

    // Resolve which slot belongs to the local client. ListenServer host +
    // singleplayer both use HOST_SERVER_LOCAL_CLIENT_ID; remote clients
    // get their own NetConfig-derived id when client mode lands. For now
    // any slot at HOST_SERVER_LOCAL_CLIENT_ID owns the PlayerFaction +
    // camera seat.
    let local_client_id = net_cfg
        .as_ref()
        .map(|cfg| {
            use crate::net::NetMode;
            match cfg.mode {
                NetMode::Client => cfg
                    .player_name
                    .as_deref()
                    .map(crate::net::derive_client_id)
                    .unwrap_or(crate::HOST_SERVER_LOCAL_CLIENT_ID),
                _ => crate::HOST_SERVER_LOCAL_CLIENT_ID,
            }
        })
        .unwrap_or(crate::HOST_SERVER_LOCAL_CLIENT_ID);

    // Score a candidate home tile: positive = better. Combines river-proximity
    // (riverside camps preferred over arid interior) with a continuous
    // farthest-point inter-faction spacing reward (`faction_spacing_score`).
    let score_home_candidate = |tx: i32, ty: i32, others: &[(i32, i32)]| -> i32 {
        let spacing = faction_spacing_score(tx, ty, others);
        let river_score = match chunk_map.river_distance_at(tx, ty) {
            0..=4 => -80,
            5..=9 => -20,
            10..=12 => 20,
            13..=16 => 60,
            _ => 0,
        };
        let relief_score = globe.sample_relief(tx, ty).class.settlement_score_bonus();
        spacing + river_score + relief_score
    };

    let tile_landform_ok = |tx: i32, ty: i32| -> bool {
        !globe.sample_relief(tx, ty).class.rejects_settlement()
    };

    // Search for an AI rival home inside the pre-gen window. Shared between
    // the "no-megachunk human fallback" and the AI rival loop.
    let mut pick_ai_rival_home = |spawned_homes: &[(i32, i32)],
                                  rng: &mut rand::rngs::ThreadRng|
     -> Option<(i32, i32)> {
        let mut best: Option<((i32, i32), i32)> = None;
        for _ in 0..200 {
            let tx = start_tx + rng.gen_range(0..total_tiles_x);
            let ty = start_ty + rng.gen_range(0..total_tiles_y);
            if !chunk_map.is_passable(tx, ty)
                || matches!(chunk_map.tile_kind_at(tx, ty), Some(TileKind::Stone))
                || !tile_landform_ok(tx, ty)
            {
                continue;
            }
            let score = score_home_candidate(tx, ty, spawned_homes);
            if best.as_ref().map_or(true, |(_, s)| score > *s) {
                best = Some(((tx, ty), score));
            }
        }
        best.map(|(t, _)| t).or_else(|| {
            let mut result = None;
            for _ in 0..500 {
                let tx = start_tx + rng.gen_range(0..total_tiles_x);
                let ty = start_ty + rng.gen_range(0..total_tiles_y);
                if chunk_map.is_passable(tx, ty)
                    && !matches!(chunk_map.tile_kind_at(tx, ty), Some(TileKind::Stone))
                    && tile_landform_ok(tx, ty)
                {
                    result = Some((tx, ty));
                    break;
                }
            }
            result
        })
    };

    // --- Phase 1: human slots ---
    //
    // Each slot resolves its home via `pick_player_home_in_megachunk` when
    // the user picked a megachunk; otherwise falls through to the AI search.
    // PlayerFaction is set for the slot whose client_id matches the local
    // client. Every human slot is added to ControlledFactions.
    let mut human_slot_homes: Vec<(usize, (i32, i32))> = Vec::with_capacity(starts.slots.len());
    for slot_idx in 0..starts.slots.len() {
        let slot_mc = starts.slots[slot_idx].megachunk;
        let home: Option<(i32, i32)> = slot_mc
            .map(|(mx, my)| {
                let pick = crate::simulation::region::pick_player_home_in_megachunk(
                    &chunk_map,
                    &globe,
                    mx,
                    my,
                    world_seed.0,
                );
                pick.tile
            })
            .or_else(|| pick_ai_rival_home(&spawned_homes, &mut rng));

        let Some((home_tx, home_ty)) = home else {
            continue;
        };
        spawned_homes.push((home_tx, home_ty));
        human_slot_homes.push((slot_idx, (home_tx, home_ty)));
    }

    // --- Phase 2: AI rivals ---
    let mut ai_homes: Vec<(i32, i32)> = Vec::with_capacity(ai_rival_count as usize);
    for _ in 0..ai_rival_count {
        if let Some(home) = pick_ai_rival_home(&spawned_homes, &mut rng) {
            spawned_homes.push(home);
            ai_homes.push(home);
        }
    }

    // --- Spawn loop: humans first, then AI rivals ---
    //
    // Stamps PlayerFaction on the local-client slot's faction; every human
    // slot lands in ControlledFactions. AI rivals never enter ControlledFactions.
    let mut total_factions = 0u32;
    let mut spawn_for_faction = |commands: &mut Commands,
                                 registry: &mut FactionRegistry,
                                 clock: &mut SimClock,
                                 player_faction: &mut PlayerFaction,
                                 controlled: &mut crate::simulation::faction::ControlledFactions,
                                 settled: &mut crate::simulation::region::SettledRegions,
                                 home: (i32, i32),
                                 slot_ix: Option<usize>|
     -> (u32, u32, Vec<Entity>) {
        let (home_tx, home_ty) = home;
        let faction_id = registry.create_faction((home_tx as i32, home_ty as i32));

        let is_human = slot_ix.is_some();
        // Lifestyle for this faction:
        // - Human slot: read the slot's per-player lifestyle (lobby picks
        //   independently per player; SP slot mirrors GameStartOptions.lifestyle).
        // - AI rival: stay Settled by default (matches legacy behaviour).
        let faction_lifestyle = if let Some(ix) = slot_ix {
            starts.slots[ix].lifestyle
        } else {
            crate::simulation::faction::Lifestyle::Settled
        };

        if let Some(faction_data) = registry.factions.get_mut(&faction_id) {
            crate::economy::policy::apply_preset(
                &mut faction_data.economic_policy,
                options.economy,
                &catalog,
            );
            faction_data.land_policy = crate::economy::policy::land_policy_for(options.economy);
            faction_data.lifestyle = faction_lifestyle;
            if is_human
                && matches!(
                    faction_lifestyle,
                    crate::simulation::faction::Lifestyle::Nomadic
                )
            {
                // Human-driven nomadic factions take the manual command flow
                // (Pack/Pitch + Scout + Route). AI nomadic factions keep
                // autopilot on.
                faction_data.nomad_autopilot = false;
            }
            let key = crate::simulation::archetype::legacy_archetype_key(
                faction_lifestyle,
                options.economy,
            );
            faction_data.caps = crate::simulation::archetype::derive_from_archetype_key(
                &archetype_registry,
                key,
                Some((faction_lifestyle, options.economy, &catalog)),
            )
            .expect("derive_from_archetype_key with legacy fallback always returns Some");
        }

        let home_world = tile_to_world(home_tx, home_ty);

        if let Some(ix) = slot_ix {
            controlled.add(faction_id);
            starts.slots[ix].faction_id = Some(faction_id);

            // Local client owns PlayerFaction + the marked FactionCenter.
            let is_local = starts.slots[ix].client_id == local_client_id;
            if is_local {
                player_faction.faction_id = faction_id;

                commands.spawn((
                    FactionCenter,
                    PlayerFactionMarker,
                    Transform::from_xyz(home_world.x, home_world.y, 0.5),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ));

                let megachunk = MegaChunkCoord::from_tile(home_tx, home_ty);
                settled.settle(megachunk, clock.tick, "Home".to_string(), home_world, true);
            }
        }

        // Per-slot group size: humans use options.player_population (today
        // shared across all human slots; per-slot pop is a follow-up the
        // lobby can wire later). AI rivals stay on GROUP_SIZE.
        let group_size = if is_human {
            options.player_population
        } else {
            GROUP_SIZE
        };

        let band = spawn_faction_band(
            commands,
            &chunk_map,
            registry,
            clock,
            faction_id,
            (home_tx, home_ty),
            group_size,
            options.era,
        );
        total_factions += 1;
        let n = band.members.len() as u32;
        (faction_id, n, band.members)
    };

    for (slot_ix, home) in human_slot_homes.clone() {
        let (faction_id, count, members) = spawn_for_faction(
            &mut commands,
            &mut registry,
            &mut clock,
            &mut player_faction,
            &mut controlled,
            &mut settled,
            home,
            Some(slot_ix),
        );
        spawned += count;

        // M2: Market preset spawn-time household seeding for human factions.
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
                home,
                &members,
            );
        }
    }

    for home in ai_homes.clone() {
        let (_fid, count, _members) = spawn_for_faction(
            &mut commands,
            &mut registry,
            &mut clock,
            &mut player_faction,
            &mut controlled,
            &mut settled,
            home,
            None,
        );
        spawned += count;
    }

    // Promote the local-client slot's home into PendingStarts.primary_start
    // so downstream readers (camera, world pre-gen) see the right anchor.
    if let Some(local_slot) = starts.slots.iter().find(|s| s.client_id == local_client_id) {
        if let Some((hx, hy)) = local_slot.megachunk {
            starts.primary_start = Some((hx, hy));
        }
    }

    // Drop the helper-scope-only buffers (silences unused warnings for
    // intermediate planning state).
    {
        let _ = &spawned_homes;
        let _ = &human_slot_homes;
        let _ = &ai_homes;
        let _ = total_factions;
    }

    clock.population = spawned;
    clock.current_end = clock.bucket_size.min(spawned);

    info!(
        "Spawned {} people across {} human slot(s) + {} AI rival(s) (of {} groups, {} faction slots reserved for the world map) in {:?}",
        spawned,
        human_count,
        ai_rival_count,
        num_groups,
        num_groups.saturating_sub(near_factions),
        now.elapsed()
    );
}

/// Result of [`spawn_faction_band`] — the spawned member entities and the
/// designated chief (the first member, if any spawned).
pub(crate) struct FactionBandSpawn {
    pub members: Vec<Entity>,
    pub chief: Option<Entity>,
}

/// Deterministic chief sex per faction, seeded from `faction_id + home_tile`.
/// Mixed with splitmix64 so reruns with the same world seed reproduce the
/// same demographics. Paired with the kin-slot roster in `spawn_faction_band`
/// so kin groups of 4 lay out as `[chief, !chief, chief, !chief]`, which
/// `seed_starting_relationships_system` then bonds as opposite-sex spouse
/// pairs.
pub(crate) fn pair_chief_sex(faction_id: u32, home_tile: (i32, i32)) -> BiologicalSex {
    let (hx, hy) = home_tile;
    let mut x = (faction_id as u64) ^ (hx as u32 as u64) ^ ((hy as u32 as u64) << 32);
    // splitmix64
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    if x & 1 == 0 {
        BiologicalSex::Male
    } else {
        BiologicalSex::Female
    }
}

/// Fallback member-spawn tile: a passable non-stone tile within `SPAWN_RADIUS`
/// of `(cx, cy)`. Used only when the reachable flood pool is exhausted.
fn fallback_member_tile(
    rng: &mut rand::rngs::ThreadRng,
    chunk_map: &ChunkMap,
    cx: i32,
    cy: i32,
) -> Option<(i32, i32)> {
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
}

/// Spawn a faction's band: a home `FactionStorageTile` (FactionTile / Hybrid
/// storage archetypes), `group_size` `Person` members drawn from a reachable
/// flood out of `home_tile`, and chief designation on the first member.
///
/// Shared verbatim by `spawn_population` (game start) and
/// `abstract_faction::materialize_abstract_faction_system` (runtime
/// materialisation), so a faction the player travels to behaves identically to
/// one that spawned beside them. `clock.population` / `bucket_size` advance per
/// member exactly as `reproduction.rs` does for newborns, so the LOD bucket
/// scheduler stays consistent whether the band spawns at tick 0 or mid-game.
pub(crate) fn spawn_faction_band(
    commands: &mut Commands,
    chunk_map: &ChunkMap,
    registry: &mut FactionRegistry,
    clock: &mut SimClock,
    faction_id: u32,
    home_tile: (i32, i32),
    group_size: u32,
    era: crate::simulation::technology::Era,
) -> FactionBandSpawn {
    let (home_tx, home_ty) = home_tile;
    let home_world = tile_to_world(home_tx, home_ty);

    // Settled factions get a fixed storage tile at home; nomadic factions pool
    // storage across member / pack-animal inventories instead. Capability
    // check: only FactionTile / Hybrid storage backends spawn a tile.
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

    let mut rng = rand::thread_rng();
    let mut members: Vec<Entity> = Vec::with_capacity(group_size as usize);
    let mut first_member: Option<Entity> = None;
    // Draw member spawn tiles from a flood outward from the home tile, so
    // every member is reachable-from-home *by construction* instead of
    // random-scatter-then-passability (which could strand a member in a
    // passable pocket across a river/cliff from home).
    let reachable_pool = crate::simulation::placement_reachability::spawn_tiles_from(
        chunk_map,
        home_tile,
        group_size as usize,
    );
    let mut pool_iter = reachable_pool.into_iter();
    // Pre-computed sex roster aligned to spawn order. Mirrors
    // `seed_starting_relationships_system::chunks(MAX_KIN_GROUP)` so each kin
    // group of 4 lays out as `[chief, !chief, chief, !chief]`, guaranteeing
    // an opposite-sex spouse pair per group. Chief sex varies per faction
    // via `pair_chief_sex`. Solo/Market households still draw from the
    // roster but kin grouping never reads it.
    let chief_sex = pair_chief_sex(faction_id, home_tile);
    let sex_roster: Vec<BiologicalSex> = (0..group_size as usize)
        .map(|i| {
            let kin_slot = i % crate::simulation::settlement_bootstrap::MAX_KIN_GROUP;
            if kin_slot % 2 == 0 {
                chief_sex
            } else {
                chief_sex.opposite()
            }
        })
        .collect();
    for _ in 0..group_size {
        let Some((tx, ty)) = pool_iter
            .next()
            .or_else(|| fallback_member_tile(&mut rng, chunk_map, home_tx, home_ty))
        else {
            continue;
        };
        // Founder role assignment for realistic seeding:
        //   - the first spawned member becomes the chief.
        //   - index 1 and every ~8th member is a Specialist (one workshop
        //     hand per "family"), so even small bands get one beyond the chief.
        //   - everyone else carries the band's common knowledge.
        let role = if members.is_empty() {
            crate::simulation::knowledge::FounderRole::Chief
        } else if members.len() == 1 || members.len() % 8 == 0 {
            crate::simulation::knowledge::FounderRole::Specialist
        } else {
            crate::simulation::knowledge::FounderRole::Common
        };

        // LOD bucket slot — advance the clock exactly as `reproduction.rs`
        // does for newborns, so runtime materialisation stays consistent.
        let slot = clock.population;
        clock.population += 1;
        clock.bucket_size = clock.population.min(10_000);

        let world_pos = tile_to_world(tx, ty);
        let sex = sex_roster
            .get(members.len())
            .copied()
            .unwrap_or_else(BiologicalSex::random);

        let person_entity = commands
            .spawn((
                (
                    Person,
                    Transform::from_xyz(world_pos.x, world_pos.y, 1.0),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    Needs::new(30.0, 20.0, 10.0, 5.0, 40.0, 200.0),
                    Mood::default(),
                    Skills::default(),
                    SkillPeaks::default(),
                    SkillUseTicks::default(),
                    SkillsLastSeen::default(),
                    Stats::roll_3d6(),
                    PersonAI {
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
                    BucketSlot(slot),
                    MovementState {
                        wander_timer: (slot % 100) as f32 * 0.025,
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
                    crate::simulation::memory::AgentVisionCache::default(),
                    Name::new(generate_person_name(sex)),
                    PathFollow::default(),
                    Carrier::default(),
                    crate::simulation::reproduction::CoSleepTracker::default(),
                    crate::simulation::reproduction::MaleConceptionCooldown::default(),
                    Indexed::new(IndexedKind::Person),
                    PersonKnowledge::seeded_realistic_through_era(
                        era,
                        role,
                        clock.tick as u32,
                    ),
                    crate::simulation::typed_task::ActionQueue::idle(),
                    crate::simulation::goal_scorers::AgentDecisionState::default(),
                    // Phase 6 of wage-aware-labor-market-v2: per-agent
                    // psychological profile. Scattered by fastrand at spawn so
                    // populations have heterogeneous goal preferences.
                    crate::simulation::goal_scorers::Disposition {
                        entrepreneurial: fastrand::u8(..),
                        gregariousness: fastrand::u8(..),
                        curiosity: fastrand::u8(..),
                        martial: fastrand::u8(..),
                    },
                ),
                // Always-present (never insert/removed at runtime — see
                // social_contact.rs) so the Person archetype stays stable.
                (
                    crate::simulation::social_contact::SecondarySocial::inactive(),
                    crate::simulation::energy::Energy::default(),
                    crate::simulation::tools::ToolKit::new(
                        crate::simulation::tools::capacity_for_era(era),
                    ),
                ),
            ))
            .id();

        if first_member.is_none() {
            first_member = Some(person_entity);
        }
        members.push(person_entity);
        registry.add_member(faction_id);
    }

    // Designate the first spawned member as chief. Without this, chief-driven
    // systems (chief_directive_system, chief_job_posting, chief_hunt_order,
    // chief_tablet_posting) wait for a runtime bonding event that may never
    // fire on a freshly seeded faction.
    if let Some(chief) = first_member {
        if let Some(faction_data) = registry.factions.get_mut(&faction_id) {
            faction_data.chief_entity = Some(chief);
        }
        commands.entity(chief).insert(FactionChief);
    }

    FactionBandSpawn {
        members,
        chief: first_member,
    }
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
    // Household plot tiles are drawn from a flood outward from the village
    // home so each FactionStorageTile / home is reachable from the village by
    // construction; the legacy spiral search is the fallback only when the
    // reachable pool is exhausted.
    let pool = crate::simulation::placement_reachability::spawn_tiles_from(
        chunk_map,
        village_home,
        members.len() * 2 + 4,
    );
    let mut pool_iter = pool.into_iter();
    for &member in members {
        let plot = loop {
            match pool_iter.next() {
                Some(t) if used.contains(&t) => continue,
                Some(t) => break Some(t),
                None => {
                    break crate::simulation::construction::next_clear_tile(
                        village_home,
                        &used,
                        chunk_map,
                        16,
                    )
                }
            }
        };
        let Some(plot) = plot else {
            continue;
        };
        used.insert(plot);
        let household_id = registry.spawn_household(village_faction_id, plot, member, catalog);
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
            .insert(crate::simulation::reproduction::HouseholdMember { household_id });
    }
}

#[cfg(test)]
mod tests {
    use super::{faction_spacing_score, NEAR_FACTION_TARGET_SPACING};

    #[test]
    fn faction_spacing_score_empty_is_full() {
        // First faction placed: nothing to space away from → full reward.
        assert_eq!(faction_spacing_score(0, 0, &[]), 100);
    }

    #[test]
    fn faction_spacing_score_coincident_is_zero() {
        assert_eq!(faction_spacing_score(500, 500, &[(500, 500)]), 0);
    }

    #[test]
    fn faction_spacing_score_saturates() {
        // A candidate at or beyond the target spacing scores full marks.
        let far = NEAR_FACTION_TARGET_SPACING as i32 + 50;
        assert_eq!(faction_spacing_score(far, 0, &[(0, 0)]), 100);
        assert_eq!(
            faction_spacing_score(NEAR_FACTION_TARGET_SPACING as i32, 0, &[(0, 0)]),
            100
        );
    }

    #[test]
    fn faction_spacing_score_is_monotonic() {
        // Moving the candidate strictly farther from its nearest home never
        // decreases the score.
        let home = [(0, 0)];
        let mut prev = -1;
        for d in 0..=(NEAR_FACTION_TARGET_SPACING as i32 + 100) {
            let s = faction_spacing_score(d, 0, &home);
            assert!(s >= prev, "score dropped at d={d}: {s} < {prev}");
            prev = s;
        }
    }

    #[test]
    fn faction_spacing_score_picks_nearest_home() {
        // Score reflects the distance to the *closest* home (100), not the
        // far one (400): 100 / 280 * 100 ≈ 35.
        let s = faction_spacing_score(100, 0, &[(0, 0), (500, 0)]);
        let expected = (100.0 / NEAR_FACTION_TARGET_SPACING * 100.0) as i32;
        assert_eq!(s, expected);
    }

    #[test]
    fn farthest_point_spreads_homes() {
        // Behavioural regression: emulate the placement loop on the scorer
        // alone (river score zeroed). From one fixed home, repeatedly pick the
        // best of a deterministic grid of candidates in a 1024-tile window,
        // append the winner, repeat. The result must be well separated — the
        // old binary scorer would let later factions cluster.
        let mut homes: Vec<(i32, i32)> = vec![(512, 512)];
        for _ in 0..3 {
            let mut best: Option<((i32, i32), i32)> = None;
            let mut ty = 0;
            while ty <= 1024 {
                let mut tx = 0;
                while tx <= 1024 {
                    let score = faction_spacing_score(tx, ty, &homes);
                    if best.map_or(true, |(_, s)| score > s) {
                        best = Some(((tx, ty), score));
                    }
                    tx += 32;
                }
                ty += 32;
            }
            homes.push(best.unwrap().0);
        }
        // Every pair of placed homes is comfortably separated.
        for i in 0..homes.len() {
            for j in (i + 1)..homes.len() {
                let (ax, ay) = homes[i];
                let (bx, by) = homes[j];
                let d = (((ax - bx) as f32).powi(2) + ((ay - by) as f32).powi(2)).sqrt();
                assert!(d > 200.0, "homes {i} and {j} too close: {d}");
            }
        }
    }

    use super::PersonAI;

    #[test]
    fn add_work_progress_accumulates_sub_unit_fractions() {
        let mut ai = PersonAI::default();
        ai.add_work_progress(0.4);
        ai.add_work_progress(0.4);
        // Two 0.4s = 0.8, still below 1 whole unit.
        assert_eq!(ai.work_progress, 0);
        assert!((ai.work_progress_fraction - 0.8).abs() < 1e-5);

        ai.add_work_progress(0.4);
        // Third 0.4 pushes total to 1.2 → one whole unit, fraction 0.2.
        assert_eq!(ai.work_progress, 1);
        assert!((ai.work_progress_fraction - 0.2).abs() < 1e-5);
    }

    #[test]
    fn add_work_progress_saturates_at_u8_max() {
        let mut ai = PersonAI::default();
        ai.add_work_progress(300.0);
        assert_eq!(ai.work_progress, u8::MAX);
        // Saturation pushes the leftover whole units out — fraction is < 1.
        assert!(ai.work_progress_fraction < 1.0);
    }

    #[test]
    fn add_work_progress_ignores_nonpositive_inputs() {
        let mut ai = PersonAI::default();
        ai.work_progress_fraction = 0.3;
        ai.add_work_progress(0.0);
        ai.add_work_progress(-1.0);
        ai.add_work_progress(f32::NAN);
        // No changes.
        assert_eq!(ai.work_progress, 0);
        assert!((ai.work_progress_fraction - 0.3).abs() < 1e-5);
    }

    #[test]
    fn add_work_progress_slow_factor_eventually_advances() {
        // factor = 0.45 (between energy-tired floor 0.45 and 1.0). At
        // base = 1.0 per tick, the legacy `(base * factor) as u8` cast
        // truncated to 0 and stalled. Now we should accrue ~1 unit per
        // ~2.22 ticks.
        let mut ai = PersonAI::default();
        for _ in 0..3 {
            ai.add_work_progress(0.45);
        }
        // 3 * 0.45 = 1.35 → 1 whole unit.
        assert_eq!(ai.work_progress, 1);
    }
}
