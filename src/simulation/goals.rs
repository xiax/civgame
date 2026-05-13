use super::construction::{AutonomousBuildingToggle, Blueprint, BlueprintMap};
use super::faction::{FactionChief, FactionMember, FactionRegistry, StorageTileMap, SOLO};
use super::jobs::JobClaim;
use super::lod::LodLevel;
use super::needs::Needs;
use super::person::{AiState, Drafted, PersonAI};
use super::schedule::{BucketSlot, SimClock};
use crate::economy::agent::EconomicAgent;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::simulation::animals::{Horse, Tamed};
use crate::simulation::items::{GroundItem, TargetItem};
use crate::simulation::plants::Plant;
use crate::simulation::tasks::TaskKind;
use crate::simulation::technology::{
    BOW_AND_ARROW, BRONZE_WEAPONS, COPPER_TOOLS, FIRED_POTTERY, FIRE_MAKING, FLINT_KNAPPING,
    HORSE_TAMING, HUNTING_SPEAR, LOOM_WEAVING,
};
use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};
use crate::world::seasons::{Calendar, Season};
use crate::world::terrain::TILE_SIZE;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

// ── Need thresholds used by goal_update_system ────────────────────────────────
//
// All need values run 0-255 (see `needs.rs`). These constants name the
// breakpoints so the goal logic reads as decisions rather than magic
// arithmetic. Tune them here in one place; `goal_update_system` references
// them by name.

/// Below this hunger an agent is willing to leave camp for a raid.
const HUNGER_RAID_CEILING: f32 = 120.0;
/// Below this hunger an agent will start a Build/Lead task; above it the
/// agent prioritises Survive.
const HUNGER_WORK_CEILING: f32 = 150.0;
/// Above this hunger an agent enters Survive even if they have food on hand.
const HUNGER_SURVIVE_DESPERATE: f32 = 200.0;
/// Above this hunger an agent enters Survive when they hold food (eating it
/// is faster than going hungry).
const HUNGER_EAT_HELD: f32 = 180.0;
/// Above this hunger an agent enters Survive when they have NO food (must go
/// hunt/forage immediately).
const HUNGER_FORAGE_REQUIRED: f32 = 150.0;
/// Above this hunger the starvation flag fires (used for emergency reactions).
/// Held at `EAT_TRIGGER_HUNGER` so the goal label flips at exactly the same
/// hunger level the AcquireFood dispatcher / methods are willing to act on —
/// otherwise an agent in (HUNGER_STARVING, EAT_TRIGGER_HUNGER) is labelled
/// "Starving (Faction has food)" while every dispatcher silently bails.
const HUNGER_STARVING: f32 = 180.0;
/// Above this sleep need an agent enters Sleep.
const SLEEP_TIRED: f32 = 180.0;
/// Above this sleep need an agent prefers not to start a long task.
const SLEEP_WORK_CEILING: f32 = 170.0;
/// Either-or threshold for "this agent is too needy to socialise/play."
const NEED_BUSY: f32 = 100.0;

/// Bundles the storage-reachability lookup resources so `goal_update_system`
/// stays under Bevy's 16-param limit.
#[derive(SystemParam)]
pub struct StorageReachability<'w> {
    pub chunk_graph: Res<'w, ChunkGraph>,
    pub chunk_router: Res<'w, ChunkRouter>,
    pub storage_tile_map: Res<'w, StorageTileMap>,
}

/// Bundles the scorer-pipeline inputs to keep `goal_update_system`
/// under Bevy's 16-param ceiling. The scorer pipeline is the only
/// goal-selection path as of Phase F-2 (Legacy mode removed); the
/// imperative cascade survives only as the SOLO / faction-miss
/// fallback inside the system.
#[derive(SystemParam)]
pub struct ScorerInputs<'w, 's> {
    pub registry: Res<'w, crate::simulation::goal_scorers::GoalScorerRegistry>,
    pub board: Res<'w, crate::simulation::jobs::JobBoard>,
    pub disposition_q: Query<'w, 's, &'static crate::simulation::goal_scorers::Disposition>,
    pub skills_q: Query<'w, 's, &'static crate::simulation::skills::Skills>,
    pub profession_q: Query<'w, 's, &'static crate::simulation::person::Profession>,
    pub injury_q: Query<'w, 's, &'static crate::simulation::medicine::Injury>,
    /// All currently-injured agents' faction membership. Walked once
    /// at the top of `goal_update_system` to build a per-faction
    /// "any injured" set — cheap because `Injury` is rare and the
    /// query iterates only injured entities.
    pub injured_faction_q: Query<
        'w,
        's,
        &'static FactionMember,
        With<crate::simulation::medicine::Injury>,
    >,
}

#[repr(u8)]
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Personality {
    #[default]
    Gatherer = 0,
    Socialite = 1,
    Explorer = 2,
    Nurturer = 3,
    /// Prefers solitary play with items over playing with other agents.
    /// Loners get a small bonus from solo play and a heavily reduced bonus
    /// from social play.
    Loner = 4,
}

impl Personality {
    pub fn name(self) -> &'static str {
        match self {
            Personality::Gatherer => "Gatherer",
            Personality::Socialite => "Socialite",
            Personality::Explorer => "Explorer",
            Personality::Nurturer => "Nurturer",
            Personality::Loner => "Loner",
        }
    }

    pub fn random() -> Self {
        // 10% Loner; rest split evenly across the original four.
        let r = fastrand::u8(..10);
        if r == 0 {
            return Personality::Loner;
        }
        match fastrand::u8(..4) {
            0 => Personality::Gatherer,
            1 => Personality::Socialite,
            2 => Personality::Explorer,
            _ => Personality::Nurturer,
        }
    }
}

#[repr(u8)]
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AgentGoal {
    #[default]
    GatherFood = 0,
    GatherWood = 1,
    GatherStone = 2,
    Survive = 3,
    ReturnCamp = 4,
    Socialize = 5,
    Raid = 7,
    Defend = 8,
    Sleep = 9,
    Build = 10,
    TameHorse = 11,
    Craft = 12,
    Lead = 13,
    Rescue = 14,
    /// Drained agents seek out a play partner or an item to play with to
    /// refill willpower.
    Play = 15,
    /// Plant or tend crops on a designated farm tile. Tech-gated by Agriculture.
    Farm = 16,
    /// Withdraw materials from faction storage and carry them to a specific
    /// blueprint. Set exclusively by `JobClaim` of `JobKind::Haul`.
    Haul = 17,
    /// Phase 5e-xiv: scavenge ambient ground items matching a chief-posted
    /// `JobKind::Stockpile { resource_id }` and deposit at faction storage.
    /// Generalizes the Wood/Stone-only `GatherWood`/`GatherStone` pattern to
    /// any catalog resource — the specific resource lives on the
    /// `ClaimTarget.resource_id` companion. Set exclusively by `JobClaim` of
    /// `JobKind::Stockpile` for resources outside the Wood/Stone fallback.
    Stockpile = 18,
    /// P1 (active migration): nomadic band member walking with the band
    /// to the new camp tile after a migration commit. Stamped by
    /// `nomad_migration_commit_system` alongside a `MigrationTarget`
    /// component; cleared by `nomad_migration_arrival_system` on
    /// arrival or timeout. Survive-tier needs (severe hunger / under
    /// raid / rescue) preempt naturally via the existing branch order
    /// in `goal_update_system`.
    MigrateToCamp = 19,
    /// Player-issued command is owning this agent. Forced by
    /// `goal_update_system` when `Commanded { status: Pending | Active }`
    /// is present. HTN dispatchers don't match this goal so they naturally
    /// skip the agent — replaces the 28 scattered `Without<PlayerOrder>`
    /// filters. The task chain is set up by
    /// `dispatch_player_command_system` and preserved by
    /// `goal_dispatch_system`'s short-circuit.
    FollowingPlayerCommand = 20,
    /// Phase D (migration scout): nomadic-band member dispatched by
    /// `nomad_survey_trigger_system` to walk to a quadrant tile and
    /// seed faction-tier `SharedKnowledge` for the chief's
    /// `pick_migration_target` re-score after the survey window.
    /// Stamped alongside a `ScoutAssignment` companion (in
    /// `nomad.rs`); cleared by `nomad_survey_completion_system` once
    /// the survey window closes.
    Scout = 21,
    /// Heal-pipeline patient side: an `Injury`-bearing agent walking
    /// to the nearest available Healer / Shrine. Set by
    /// `HealNeedScorer`; cleared when `Injury` despawns or the
    /// patient arrives at a treatment site.
    SeekCare = 22,
    /// Heal-pipeline provider side: a `Profession::Healer` walking to
    /// (or treating) the nearest patient. Set by `ProvideCareScorer`
    /// or by chief `JobKind::Heal` posting claim; cleared when no
    /// injured agent remains in range or the patient recovers.
    ProvideCare = 23,
}

/// True if `goal` is permitted for a member of a faction whose
/// `CampState` is `Packed`. Settled-life work (Build / Craft / Farm /
/// Haul / Stockpile / TameHorse / GatherWood / GatherStone /
/// ReturnCamp / Lead) is gated off; survival, food, social, defence,
/// migration, and player-command goals stay live so the band can
/// keep itself alive on the move.
#[inline]
pub fn allowed_while_packed(goal: AgentGoal) -> bool {
    matches!(
        goal,
        AgentGoal::Survive
            | AgentGoal::Sleep
            | AgentGoal::GatherFood
            | AgentGoal::Socialize
            | AgentGoal::Defend
            | AgentGoal::Rescue
            | AgentGoal::Play
            | AgentGoal::FollowingPlayerCommand
            | AgentGoal::MigrateToCamp
            | AgentGoal::Scout
            | AgentGoal::SeekCare
    )
}

impl AgentGoal {
    pub fn name(self) -> &'static str {
        match self {
            AgentGoal::GatherFood => "GatherFood",
            AgentGoal::GatherWood => "GatherWood",
            AgentGoal::GatherStone => "GatherStone",
            AgentGoal::Survive => "Survive",
            AgentGoal::ReturnCamp => "ReturnCamp",
            AgentGoal::Socialize => "Socialize",
            AgentGoal::Raid => "Raid",
            AgentGoal::Defend => "Defend",
            AgentGoal::Sleep => "Sleep",
            AgentGoal::Build => "Build",
            AgentGoal::TameHorse => "TameHorse",
            AgentGoal::Craft => "Craft",
            AgentGoal::Lead => "Lead",
            AgentGoal::Rescue => "Rescue",
            AgentGoal::Play => "Play",
            AgentGoal::FollowingPlayerCommand => "FollowingPlayerCommand",
            AgentGoal::Farm => "Farm",
            AgentGoal::Haul => "Haul",
            AgentGoal::Stockpile => "Stockpile",
            AgentGoal::MigrateToCamp => "MigrateToCamp",
            AgentGoal::Scout => "Scout",
            AgentGoal::SeekCare => "SeekCare",
            AgentGoal::ProvideCare => "ProvideCare",
        }
    }
}

/// Set on a responder by `sound::respond_to_distress_system` when they are recruited
/// to defend a faction-mate (or affinity-bonded ally). Carries the attacker plus
/// the attacker's last-known tile so the `RescueAlly` plan can route the responder
/// without re-querying the attacker's `Transform` (avoids borrow conflicts in
/// `plan_execution_system`). Refreshed on each distress event from the victim;
/// cleared when the attacker is dead/despawned or after a timeout.
#[derive(Component, Clone, Copy)]
pub struct RescueTarget {
    pub attacker: Entity,
    pub attacker_tile: (i32, i32),
    pub set_tick: u64,
}

#[derive(Component, Default)]
pub struct GoalReason(pub &'static str);

/// Per-agent cooldown table that gates *opportunistic* goal eligibility.
/// `(goal_disc, expires_tick)` ring with 4 slots; oldest evicted on
/// overflow. Phase 6B writes here on chronic method-failure for any
/// non-survival goal so the next `goal_update_system` evaluation
/// declines re-entering the same goal until the cooldown expires.
///
/// **Survive / Sleep are never cooldown-gated.** The `is_active` check
/// rejects those discriminants up front — a hungry agent who can't
/// find food via methods AND can't make Explore-fallback work would
/// otherwise starve, and a sleep-need-critical agent likewise.
///
/// `craft_until_tick` is preserved for `should_craft`'s legacy fast
/// path; `chronic_craft_cooldown_system` writes both `craft_until_tick`
/// *and* the ring entry so old + new readers stay in sync.
#[derive(Component, Default, Debug)]
pub struct GoalCooldown {
    pub craft_until_tick: u64,
    pub entries: [Option<(u8, u64)>; 4],
}

impl GoalCooldown {
    /// Goal discriminants we *allow* in the cooldown ring. `Survive` /
    /// `Sleep` are explicitly absent — see component-level doc.
    fn cooldown_eligible(goal: AgentGoal) -> bool {
        !matches!(goal, AgentGoal::Survive | AgentGoal::Sleep)
    }

    pub fn push(&mut self, goal: AgentGoal, expires_tick: u64) {
        if !Self::cooldown_eligible(goal) {
            return;
        }
        let disc = goal as u8;
        // Refresh-in-place if the same discriminant already lives in the
        // ring — keeps ring entries unique per goal.
        for slot in self.entries.iter_mut() {
            if let Some((d, t)) = slot {
                if *d == disc {
                    *t = (*t).max(expires_tick);
                    return;
                }
            }
        }
        // First empty slot wins.
        for slot in self.entries.iter_mut() {
            if slot.is_none() {
                *slot = Some((disc, expires_tick));
                return;
            }
        }
        // Ring full: evict the entry with the *earliest* expiry (the
        // most-likely-already-expired one).
        let mut evict_idx = 0usize;
        let mut earliest = u64::MAX;
        for (i, slot) in self.entries.iter().enumerate() {
            if let Some((_, t)) = slot {
                if *t < earliest {
                    earliest = *t;
                    evict_idx = i;
                }
            }
        }
        self.entries[evict_idx] = Some((disc, expires_tick));
    }

    pub fn is_active(&self, goal: AgentGoal, now: u64) -> bool {
        if !Self::cooldown_eligible(goal) {
            return false;
        }
        let disc = goal as u8;
        self.entries
            .iter()
            .any(|s| matches!(s, Some((d, t)) if *d == disc && *t > now))
    }
}

/// Phase 6B force-reevaluate set. `chronic_failure_release_system`
/// inserts entities into this set when their current goal has failed
/// chronically; `goal_update_system` then bypasses the 200-tick
/// cadence for those entities (so the goal flips on the very next
/// tick rather than waiting up to ~10 s) and drains the set after
/// evaluating. Without this, an autonomous-goal agent whose only
/// applicable methods are all bias-suppressed would idle for the
/// full cadence cycle before re-evaluating.
///
/// `AHashSet` for fast contains/insert/remove; populated and drained
/// each tick the system fires.
#[derive(Resource, Default)]
pub struct ForceGoalReevaluate(pub ahash::AHashSet<Entity>);

/// Pluralist Economy R8 follow-on: Maslow's hierarchy of needs as a
/// 5-tier comparator. Each agent's `next_unmet_tier` is the
/// lowest-numbered tier whose representative need is below its
/// satisfaction threshold. Higher tiers only fire when all lower
/// tiers are satisfied — agents with hunger pressure will not
/// pursue Esteem postings.
///
/// **Critical**: this enum is *additive*. It does **not** replace
/// or override `goal_update_system`'s existing goal-selection
/// logic — that's the load-bearing path for lower-tier goals
/// (Survive / Sleep / Socialize / etc.). Maslow-tier-aware systems
/// (today: `esteem_driven_posting_system`) consult this gate to
/// decide whether to apply their behaviour as a **side-effect**
/// without preempting the existing goal layer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MaslowTier {
    /// Hunger / sleep / reproduction.
    Physiological = 1,
    /// Shelter / safety.
    Safety = 2,
    /// Social / belonging (today: `Needs.social`).
    Belonging = 3,
    /// Esteem (status, mastery, recognition). Inverted polarity in
    /// `Needs.esteem` — 255 = satiated.
    Esteem = 4,
    /// Self-actualization (knowledge, teaching, descendants).
    /// Inverted polarity. 255 = satiated.
    SelfActualization = 5,
}

/// Need-pressure thresholds for each tier. A need above its
/// threshold (or below, for inverted-polarity Esteem /
/// SelfActualization) counts as unsatisfied. Calibrated to
/// produce sensible "all lower tiers met → higher tier fires"
/// transitions for typical gameplay.
const TIER_HUNGER_THRESHOLD: f32 = 100.0;
const TIER_SLEEP_THRESHOLD: f32 = 100.0;
const TIER_SHELTER_THRESHOLD: f32 = 100.0;
const TIER_SAFETY_THRESHOLD: f32 = 100.0;
const TIER_SOCIAL_THRESHOLD: f32 = 100.0;
const TIER_ESTEEM_SATIATED: f32 = 200.0; // inverted; 255 = full
const TIER_SELF_ACTUALIZATION_SATIATED: f32 = 200.0;

impl MaslowTier {
    /// Returns the lowest-numbered tier whose representative need
    /// is below its satisfaction threshold. Returns `None` when
    /// every tier is satisfied (a fully-flourishing agent).
    pub fn next_unmet(needs: &crate::simulation::needs::Needs) -> Option<MaslowTier> {
        if needs.hunger > TIER_HUNGER_THRESHOLD
            || needs.sleep > TIER_SLEEP_THRESHOLD
            || needs.reproduction > TIER_HUNGER_THRESHOLD
        {
            return Some(MaslowTier::Physiological);
        }
        if needs.shelter > TIER_SHELTER_THRESHOLD || needs.safety > TIER_SAFETY_THRESHOLD {
            return Some(MaslowTier::Safety);
        }
        if needs.social > TIER_SOCIAL_THRESHOLD {
            return Some(MaslowTier::Belonging);
        }
        if needs.esteem < TIER_ESTEEM_SATIATED {
            return Some(MaslowTier::Esteem);
        }
        if needs.self_actualization < TIER_SELF_ACTUALIZATION_SATIATED {
            return Some(MaslowTier::SelfActualization);
        }
        None
    }
}

/// Read-only validation queries goal_update_system uses to detect despawned
/// target entities. Bundled to keep the system under the 16-param ceiling.
#[derive(bevy::ecs::system::SystemParam)]
pub struct GoalValidationQueries<'w, 's> {
    pub plant_query: Query<'w, 's, &'static Plant>,
    pub item_query: Query<'w, 's, (), With<GroundItem>>,
    pub bp_query: Query<'w, 's, &'static Blueprint>,
    pub wild_horse_q: Query<'w, 's, (), (With<Horse>, Without<Tamed>)>,
    pub rescue_q: Query<'w, 's, &'static RescueTarget>,
    pub attacker_alive_q: Query<
        'w,
        's,
        Entity,
        Or<(
            With<crate::simulation::combat::Health>,
            With<crate::simulation::combat::Body>,
        )>,
    >,
    pub commanded_q: Query<'w, 's, &'static crate::simulation::player_command::Commanded>,
    pub packing_duty_q: Query<
        'w,
        's,
        (),
        With<crate::simulation::nomad_pack_labor::PackingDuty>,
    >,
}

pub fn goal_update_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    calendar: Res<Calendar>,
    auto_build: Res<AutonomousBuildingToggle>,
    storage: StorageReachability,
    validation: GoalValidationQueries,
    bp_map: Res<BlueprintMap>,
    // Scorer pipeline inputs bundled into one SystemParam to stay
    // under Bevy's 16-param ceiling. Phase F-2 removed the Legacy
    // imperative-cascade mode; the cascade body survives only as the
    // SOLO / faction-lookup-miss fallback (`fallback_pick` below).
    scorer_inputs: ScorerInputs,
    mut query: Query<
        (
            Entity,
            &mut AgentGoal,
            &Needs,
            &Personality,
            &EconomicAgent,
            &mut PersonAI,
            &BucketSlot,
            &LodLevel,
            &FactionMember,
            &Transform,
            &mut TargetItem,
            Option<&mut GoalReason>,
            Option<&FactionChief>,
            Option<&JobClaim>,
            (
                Option<&crate::simulation::nomad::MigrationTarget>,
                Option<&crate::simulation::nomad::ScoutAssignment>,
            ),
        ),
        Without<Drafted>,
    >,
    cooldown_query: Query<&GoalCooldown>,
    mut force_reeval: ResMut<ForceGoalReevaluate>,
) {
    let time_of_day_bonus =
        crate::simulation::utility_curves::time_of_day_bonus(calendar.time_phase());
    // Heal-2: precompute "any injured agent in faction" once per
    // system invocation. Cheap — query iterates only entities that
    // already carry an `Injury` component (rare in practice).
    let mut faction_has_injured: ahash::AHashSet<u32> = ahash::AHashSet::default();
    for member in scorer_inputs.injured_faction_q.iter() {
        faction_has_injured.insert(member.faction_id);
    }
    for (
        entity,
        mut goal,
        needs,
        personality,
        agent,
        mut ai,
        slot,
        lod,
        member,
        transform,
        mut target_item,
        reason_opt,
        chief_opt,
        claim_opt,
        (migration_target, scout_assignment),
    ) in query.iter_mut()
    {
        // Player command authority: when `Commanded` is non-terminal, force
        // the player-command goal and skip all autonomous evaluation. HTN
        // dispatchers don't match `FollowingPlayerCommand`, so they'll
        // naturally skip this agent without needing `Without<PlayerOrder>`
        // filters. This replaces the 28 scattered filters from the legacy
        // design. `dispatch_player_command_system` set up the task chain
        // already; `goal_dispatch_system` preserves it via a top-level
        // short-circuit on this goal.
        if let Ok(cmd) = validation.commanded_q.get(entity) {
            if !cmd.status.is_terminal() {
                if *goal != AgentGoal::FollowingPlayerCommand {
                    *goal = AgentGoal::FollowingPlayerCommand;
                }
                if let Some(mut r) = reason_opt {
                    r.0 = "Player Order";
                } else {
                    commands.entity(entity).insert(GoalReason("Player Order"));
                }
                continue;
            }
        }

        if *lod == LodLevel::Dormant {
            continue;
        }
        // Pack labor: workers mid-dismantle keep their task across
        // re-eval cycles. The goal stays whatever it was when
        // `apply_pack_camp_command_system` stamped them
        // (`FollowingPlayerCommand`); without this short-circuit
        // hunger / sleep / mobile-gate would flip the goal and the
        // dispatcher would clear the chain.
        if ai.task_id == crate::simulation::tasks::TaskKind::UnpitchStructure as u16 {
            continue;
        }
        // Pack duty: agents with PackingDuty stay on FollowingPlayerCommand
        // across the gaps between Unpitch tasks. The continue-pack
        // re-dispatcher assigns them their next structure on the next
        // cadence tick. Without this short-circuit they'd run off to
        // forage between dismantles.
        if validation.packing_duty_q.get(entity).is_ok() {
            if *goal != AgentGoal::FollowingPlayerCommand {
                *goal = AgentGoal::FollowingPlayerCommand;
            }
            if let Some(mut r) = reason_opt {
                r.0 = "Pack Duty";
            } else {
                commands.entity(entity).insert(GoalReason("Pack Duty"));
            }
            continue;
        }
        // Workers with an active job claim are owned by job_goal_lock_system (Economy).
        // Do not override their goal here or the ClaimedHaul/ClaimedBuild plans
        // will never be reached by plan_execution_system (Sequential).
        if claim_opt.is_some() {
            continue;
        }
        // Unemployed agents need immediate goal re-evaluation (e.g. just finished a deposit).
        // Bucket-gate only agents that are actively working a task.
        if ai.task_id != PersonAI::UNEMPLOYED && !clock.is_active(slot.0) {
            continue;
        }

        // 1. Cooldown & Staggered Update Fix.
        //
        // Active agents only re-evaluate every ~10 s (200 ticks at 20 Hz).
        // The previous 32-tick (1.6 s) cadence interacted badly with the
        // soft-invalidation block below: any momentary mismatch (entity
        // briefly missing from a query during LOD/chunk churn, plant
        // mid-stage transition, blueprint completed by another worker)
        // would clear `task_id`, causing the HTN dispatcher to install
        // a fresh `target_tile` next tick. `movement_system` then sees
        // `pf.goal != goal3` and re-enqueues the path, parking the
        // agent in `FollowStatus::Pending` — the visible "move one
        // tile, pause" symptom.
        // Phase 6B: agents in `ForceGoalReevaluate` bypass the 200-tick
        // cadence so chronic-failure release fires *this* tick rather
        // than up to ~10 s later. The set is drained per-entity below
        // after the re-evaluation completes (or earlier on bypass).
        let force_now = force_reeval.0.contains(&entity);
        if !force_now
            && ai.task_id != PersonAI::UNEMPLOYED
            && clock.tick.saturating_sub(ai.last_goal_eval_tick) < 200
        {
            continue;
        }
        if force_now {
            // Drop any stale task so the dispatchers see clean state on
            // the goal flip. Mirrors the goal-flip cleanup below.
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;
            target_item.0 = None;
            force_reeval.0.remove(&entity);
        }
        ai.last_goal_eval_tick = clock.tick;

        // 2. Target Validation — narrow form.
        //
        // Only invalidate when the target *entity* has truly despawned.
        // Stage / kind drift on a still-alive target is not actionable
        // from here: the executors (`gather::finish_gather`,
        // `items::finish_scavenge`, `construction::*`) detect arrival-
        // time failure and feed `MethodOutcome::FailedTarget` into
        // `MethodHistory` for failure-biased rescoring. Re-checking
        // here just races those paths and thrashes movement.
        if matches!(ai.state, AiState::Routing | AiState::Seeking) {
            let tid = ai.task_id;
            let mut invalid = false;

            if tid == TaskKind::Gather as u16 {
                if let Some(ent) = ai.target_entity {
                    if validation.plant_query.get(ent).is_err() {
                        invalid = true;
                    }
                }
            } else if tid == TaskKind::Scavenge as u16 {
                match ai.target_entity {
                    Some(ent) if validation.item_query.get(ent).is_err() => invalid = true,
                    None => invalid = true,
                    _ => {}
                }
            } else if tid == TaskKind::Construct as u16
                || tid == TaskKind::ConstructBed as u16
                || tid == TaskKind::HaulMaterials as u16
            {
                match ai.target_entity {
                    Some(ent) if validation.bp_query.get(ent).is_err() => invalid = true,
                    None => invalid = true,
                    _ => {}
                }
            }

            if invalid {
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
                target_item.0 = None;
            }
        }

        // Rescue override: if a distress responder still has a live attacker target,
        // hold the Rescue goal until they engage / it dies / or it times out.
        if let Ok(rt) = validation.rescue_q.get(entity) {
            let attacker_alive = validation.attacker_alive_q.get(rt.attacker).is_ok();
            let timed_out = clock.tick.saturating_sub(rt.set_tick) > 200;
            if attacker_alive && !timed_out {
                if *goal != AgentGoal::Rescue {
                    *goal = AgentGoal::Rescue;
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                }
                if let Some(mut r) = reason_opt {
                    r.0 = "Helping Ally";
                } else {
                    commands.entity(entity).insert(GoalReason("Helping Ally"));
                }
                continue;
            } else {
                // Attacker is dead or rescue timed out — drop the marker so the
                // agent re-evaluates a normal goal next tick.
                commands.entity(entity).remove::<RescueTarget>();
            }
        }

        // Phase D: active scout assignment. Hold AgentGoal::Scout so
        // the dispatcher keeps routing the agent toward their quadrant
        // tile. Survive-tier needs / under-raid / rescue above already
        // preempt; everything else defers until the survey window
        // closes and `nomad_survey_completion_system` strips the
        // marker.
        if scout_assignment.is_some() {
            if *goal != AgentGoal::Scout {
                *goal = AgentGoal::Scout;
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
            }
            if let Some(mut r) = reason_opt {
                r.0 = "Scouting";
            } else {
                commands.entity(entity).insert(GoalReason("Scouting"));
            }
            continue;
        }

        // P1: active migration. While the band is moving, hold the
        // MigrateToCamp goal so the dispatcher keeps walking the agent
        // toward the new camp tile. Survive-tier hunger / under-raid /
        // rescue branches above already preempt; everything else (Sleep,
        // Socialize, Play, etc.) defers until arrival or timeout.
        if migration_target.is_some() {
            if *goal != AgentGoal::MigrateToCamp {
                *goal = AgentGoal::MigrateToCamp;
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
            }
            if let Some(mut r) = reason_opt {
                r.0 = "Migrating to Camp";
            } else {
                commands
                    .entity(entity)
                    .insert(GoalReason("Migrating to Camp"));
            }
            continue;
        }

        // Don't interrupt combat or sleep
        if matches!(ai.state, AiState::Attacking | AiState::Sleeping) {
            continue;
        }

        // Faction war state overrides individual needs
        if member.faction_id != SOLO {
            if registry.is_under_raid(member.faction_id) {
                if *goal != AgentGoal::Defend {
                    *goal = AgentGoal::Defend;
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                }
                if let Some(mut r) = reason_opt {
                    r.0 = "Under Raid";
                } else {
                    commands.entity(entity).insert(GoalReason("Under Raid"));
                }
                continue;
            }
            if registry.raid_target(member.faction_id).is_some()
                && needs.hunger < HUNGER_RAID_CEILING
            {
                if *goal != AgentGoal::Raid {
                    *goal = AgentGoal::Raid;
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                }
                if let Some(mut r) = reason_opt {
                    r.0 = "Participating in Raid";
                } else {
                    commands
                        .entity(entity)
                        .insert(GoalReason("Participating in Raid"));
                }
                continue;
            }
        }

        // Chief override: tribal chiefs lead when not in crisis or at war.
        if chief_opt.is_some()
            && member.faction_id != SOLO
            && !registry.is_under_raid(member.faction_id)
            && registry.raid_target(member.faction_id).is_none()
            && needs.hunger < HUNGER_WORK_CEILING
            && needs.sleep < SLEEP_WORK_CEILING
        {
            if *goal != AgentGoal::Lead {
                *goal = AgentGoal::Lead;
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
            }
            if let Some(mut r) = reason_opt {
                r.0 = "Leading";
            } else {
                commands.entity(entity).insert(GoalReason("Leading"));
            }
            continue;
        }

        let social_threshold = if *personality == Personality::Socialite {
            120.0
        } else {
            160.0
        };

        // Loners get drained slightly faster (less social refill) and trigger
        // play earlier; everyone else waits until willpower is genuinely low.
        let play_threshold = if *personality == Personality::Loner {
            100.0
        } else {
            80.0
        };

        let has_horse_taming = member.faction_id != SOLO
            && registry
                .factions
                .get(&member.faction_id)
                .map(|f| f.techs.has(HORSE_TAMING))
                .unwrap_or(false)
            && !validation.wild_horse_q.is_empty();

        let (faction_food_ratio, can_return_camp) = if member.faction_id != SOLO {
            let per_member: f32 = match calendar.season {
                Season::Summer => 30.0,
                Season::Autumn => 25.0,
                Season::Spring => 15.0,
                Season::Winter => 5.0,
            };
            let cap = registry
                .factions
                .get(&member.faction_id)
                .map(|f| f.member_count as f32 * per_member)
                .unwrap_or(0.0);

            let stock = registry.food_stock(member.faction_id);
            let ratio = if cap > 0.0 { stock / cap } else { 1.0 };
            // Gate ReturnCamp on storage reachability so agents don't gather
            // food destined for storage they can't actually reach.
            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let storage_reachable = storage
                .storage_tile_map
                .nearest_for_faction(member.faction_id, (cur_tx, cur_ty))
                .map(|(stx, sty)| {
                    let cur_chunk = ChunkCoord(
                        cur_tx.div_euclid(CHUNK_SIZE as i32),
                        cur_ty.div_euclid(CHUNK_SIZE as i32),
                    );
                    let storage_chunk = ChunkCoord(
                        (stx as i32).div_euclid(CHUNK_SIZE as i32),
                        (sty as i32).div_euclid(CHUNK_SIZE as i32),
                    );
                    cur_chunk == storage_chunk
                        || storage
                            .chunk_router
                            .first_waypoint(
                                &storage.chunk_graph,
                                cur_chunk,
                                storage_chunk,
                                ai.current_z,
                            )
                            .is_some()
                })
                .unwrap_or(false);
            (ratio, stock < cap && storage_reachable)
        } else {
            (1.0, false)
        };

        let faction_has_food =
            member.faction_id != SOLO && registry.food_stock(member.faction_id) >= 1.0;
        let is_starving = needs.hunger > HUNGER_STARVING && agent.total_food() == 0;

        // Personal blueprints (player-commissioned, owned by this specific
        // agent) bypass the faction job board and drive AgentGoal::Build
        // directly. Faction-owned blueprints are routed exclusively through
        // the Stockpile/Haul/Build job pipeline (see jobs.rs).
        let has_personal_build_site = auto_build.0
            && bp_map.0.values().any(|&bp_e| {
                validation
                    .bp_query
                    .get(bp_e)
                    .map(|bp| bp.personal_owner == Some(entity))
                    .unwrap_or(false)
            });

        // Stable probabilistic selection: agents with high hash_val will gather if food ratio is low.
        let agent_hash_val = ((entity.index() as u64 * 2654435761) % 100) as f32 / 100.0;
        let prioritize_food = faction_food_ratio < 1.0 && agent_hash_val > faction_food_ratio;

        // Default fallback goal for unclaimed workers: gather whatever the
        // faction is short of vs. its anticipatory targets. This loses to
        // JobClaim-driven goals when the worker holds a claim (see
        // job_goal_lock_system) — those override `*goal` later in the tick.
        let mut gather_goal = AgentGoal::GatherFood;
        let mut gather_reason = "General Gathering (Food)";

        if prioritize_food {
            gather_goal = AgentGoal::GatherFood;
            gather_reason = "Prioritized Gathering (Food Low)";
        } else if member.faction_id != SOLO {
            if let Some(faction) = registry.factions.get(&member.faction_id) {
                // Use anticipatory material_targets (and current blueprint
                // demand baked in via resource_demand) to pick the highest
                // deficit material.
                let wood_id = crate::economy::core_ids::wood();
                let stone_id = crate::economy::core_ids::stone();
                let wood_target = faction
                    .material_target_of(wood_id)
                    .max(faction.demand_of(wood_id));
                let stone_target = faction
                    .material_target_of(stone_id)
                    .max(faction.demand_of(stone_id));
                let wood_stored = faction.storage.stock_of(wood_id);
                let stone_stored = faction.storage.stock_of(stone_id);
                let wood_deficit = wood_target.saturating_sub(wood_stored);
                let stone_deficit = stone_target.saturating_sub(stone_stored);

                if wood_deficit > 0 && wood_deficit >= stone_deficit {
                    gather_goal = AgentGoal::GatherWood;
                    gather_reason = "Gathering Wood (Faction Stockpile)";
                } else if stone_deficit > 0 {
                    gather_goal = AgentGoal::GatherStone;
                    gather_reason = "Gathering Stone (Faction Stockpile)";
                }
            }
        }

        let should_craft_now = should_craft(
            &registry,
            member.faction_id,
            needs,
            cooldown_query.get(entity).ok(),
            clock.tick,
        );

        // Fallback used when the scorer pipeline cannot run (SOLO
        // faction lookup miss). Mirrors the historical imperative
        // cascade so SOLO / synthetic agents retain sensible
        // behaviour without a populated `FactionData`.
        let fallback_pick = || -> (AgentGoal, &'static str) {
            if is_starving && faction_has_food {
                (AgentGoal::Survive, "Starving (Faction has food)")
            } else if needs.hunger > HUNGER_SURVIVE_DESPERATE && agent.total_food() == 0 {
                (AgentGoal::Survive, "Very Hungry")
            } else if needs.hunger > HUNGER_EAT_HELD && agent.total_food() > 0 {
                (AgentGoal::Survive, "Hungry (Eating)")
            } else if agent.total_food() >= 3 && can_return_camp {
                (AgentGoal::ReturnCamp, "Returning Surplus Food")
            } else if needs.hunger > HUNGER_FORAGE_REQUIRED && agent.total_food() == 0 {
                (AgentGoal::Survive, "Hungry")
            } else if needs.sleep > SLEEP_TIRED {
                (AgentGoal::Sleep, "Tired")
            } else if prioritize_food {
                (gather_goal, gather_reason)
            } else if has_horse_taming {
                (AgentGoal::TameHorse, "Taming Horse")
            } else if needs.social > social_threshold {
                (AgentGoal::Socialize, "Social Need")
            } else if needs.willpower < play_threshold {
                (AgentGoal::Play, "Low Willpower")
            } else if has_personal_build_site {
                (AgentGoal::Build, "Building Personal Project")
            } else if should_craft_now {
                (AgentGoal::Craft, "Crafting for Faction")
            } else {
                (gather_goal, gather_reason)
            }
        };

        let (new_goal, reason) = if let Some(faction_data) =
            registry.factions.get(&member.faction_id)
        {
            let agent_tile = (
                (transform.translation.x / TILE_SIZE).floor() as i32,
                (transform.translation.y / TILE_SIZE).floor() as i32,
            );
            let disposition = scorer_inputs
                .disposition_q
                .get(entity)
                .copied()
                .unwrap_or_default();
            let skills_default = crate::simulation::skills::Skills::default();
            let skills_ref = scorer_inputs
                .skills_q
                .get(entity)
                .unwrap_or(&skills_default);
            let profession = scorer_inputs
                .profession_q
                .get(entity)
                .copied()
                .unwrap_or(crate::simulation::person::Profession::None);
            let ctx = crate::simulation::goal_scorers::GoalScoringContext {
                agent: entity,
                agent_tile,
                now: clock.tick,
                needs,
                profession,
                skills: skills_ref,
                disposition,
                economic_agent: agent,
                faction_member: member,
                faction: faction_data,
                board: &scorer_inputs.board,
                is_starving,
                faction_has_food,
                can_return_camp,
                prioritize_food,
                fallback_gather: gather_goal,
                fallback_gather_reason: gather_reason,
                has_horse_taming,
                has_personal_build_site,
                should_craft: should_craft_now,
                injury: scorer_inputs.injury_q.get(entity).ok().copied(),
                faction_has_injured: faction_has_injured.contains(&member.faction_id),
                time_of_day_bonus,
                age_ticks: crate::simulation::utility_curves::ADULT_AGE_TICKS_PLACEHOLDER,
            };
            // Hysteresis margin damps single-tick flips around
            // utility crossover; Survival/Subsistence still
            // preempt lower classes via the class ordering in
            // `GoalScorerRegistry::best`.
            const GOAL_CHALLENGER_MARGIN: f32 = 0.10;
            let (best_opt, incumbent_score) = scorer_inputs
                .registry
                .best_with_incumbent(&ctx, Some(*goal));
            match best_opt {
                None => (gather_goal, gather_reason),
                Some(best) if best.goal == *goal => (best.goal, best.reason),
                Some(best)
                    if incumbent_score
                        .map_or(false, |s| best.score - s < GOAL_CHALLENGER_MARGIN) =>
                {
                    let cur_reason = reason_opt.as_deref().map(|r| r.0).unwrap_or("");
                    (*goal, cur_reason)
                }
                Some(best) => (best.goal, best.reason),
            }
        } else {
            fallback_pick()
        };

        if *goal != new_goal {
            *goal = new_goal;
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;
            target_item.0 = None;
        }
        if let Some(mut r) = reason_opt {
            r.0 = reason;
        } else {
            commands.entity(entity).insert(GoalReason(reason));
        }
    }
}

/// Returns true when a faction agent should switch to crafting.
/// Triggers when the faction has at least one craft tech unlocked and is short on
/// crafted goods (Tools + Weapon + Armor + Shield + Cloth < member_count / 3).
fn should_craft(
    registry: &FactionRegistry,
    faction_id: u32,
    needs: &Needs,
    cooldown: Option<&GoalCooldown>,
    now: u64,
) -> bool {
    if faction_id == SOLO {
        return false;
    }
    // Only craft when not hungry or tired
    if needs.hunger > NEED_BUSY || needs.sleep > NEED_BUSY {
        return false;
    }
    // Phase 6C/6B: per-agent cooldown after chronic Craft failure. Stops
    // Market-mode households from oscillating Craft → fail → Craft within
    // the 200-tick goal-eval cadence. Reads both legacy `craft_until_tick`
    // and the unified ring (`is_active`) so both code paths agree.
    if let Some(cd) = cooldown {
        if now < cd.craft_until_tick || cd.is_active(AgentGoal::Craft, now) {
            return false;
        }
    }
    let Some(faction) = registry.factions.get(&faction_id) else {
        return false;
    };
    let has_craft_tech = faction.techs.has(FLINT_KNAPPING)
        || faction.techs.has(HUNTING_SPEAR)
        || faction.techs.has(FIRE_MAKING)
        || faction.techs.has(BOW_AND_ARROW)
        || faction.techs.has(LOOM_WEAVING)
        || faction.techs.has(FIRED_POTTERY)
        || faction.techs.has(COPPER_TOOLS)
        || faction.techs.has(BRONZE_WEAPONS);
    if !has_craft_tech {
        return false;
    }
    use crate::economy::core_ids;
    let crafted_total: u32 = [
        core_ids::tools(),
        core_ids::weapon(),
        core_ids::armor(),
        core_ids::shield(),
        core_ids::cloth(),
    ]
    .iter()
    .map(|id| faction.storage.stock_of(*id))
    .sum();
    if crafted_total >= faction.member_count.saturating_div(3).max(1) {
        return false;
    }
    // Don't flood workers into Craft goal when no recipe's inputs are covered.
    // `resource_supply` includes agent inventories + faction storage totals,
    // refreshed each Economy tick — cheapest faction-wide material proxy.
    crate::simulation::crafting::craft_recipes()
        .iter()
        .any(|recipe| {
            if let Some(tech) = recipe.tech_gate {
                if !faction.techs.has(tech) {
                    return false;
                }
            }
            recipe.inputs.iter().all(|&(id, qty)| {
                // Phase 2d: resource_supply is now ResourceId-keyed, so the
                // recipe's id can index it directly — no reverse-resolve.
                faction.resource_supply.get(&id).copied().unwrap_or(0) >= qty
            })
        })
}

/// Phase 6 (wage-aware-labor-market-v2) — `EarnIncome` procedural
/// branch (degraded form, no peer-plan `GoalScorer` dependency).
///
/// `goal_update_system`'s cascade ends in a generic `gather_goal`
/// fallback (`GatherFood / GatherWood / GatherStone`) for any
/// professioned agent whose subsistence / social / sleep / chief
/// needs are all met. In Mixed / Market factions that ignores a real
/// signal: the agent could be earning currency right now by claiming
/// a paid posting whose `JobKind` matches their profession. This
/// system runs in `ParallelA` after `goal_update_system` and rewrites
/// any fallback gather goal to the matching `JobKind`'s goal when:
///
/// 1. The agent has no `JobClaim` (claimed workers are owned by
///    `job_goal_lock_system`).
/// 2. The agent's `Profession` is non-`None` and non-`Apprentice` —
///    professioned agents have a wage-signal anchor; idle Nones still
///    drift toward subsistence work via the legacy cascade.
/// 3. The agent's faction's `economic_policy` map is non-empty (the
///    Mixed / Market discriminator). Pure-Subsistence factions keep
///    communal labor allocation untouched.
/// 4. There's an unclaimed paid posting (`reward > 0`) in the
///    faction's `JobBoard` whose `JobKind` matches one of the
///    profession's `job_kinds_for(...)` entries.
///
/// Ranking is `posting.reward × skill_competence(primary_skill)` —
/// the same scoring the plan's `EarnIncomeScorer` calls for, modulo
/// the `(1 + disposition.entrepreneurial / 255)` factor that waits on
/// the still-deferred `Disposition` component. Travel cost is also
/// omitted today; `job_claim_system`'s `U_bid` already penalises
/// distance at claim time, so the goal layer can stay coarse.
///
/// Tier matches the plan's `GoalClass::Enterprise`: above the generic
/// gather fallback (which would otherwise win), below
/// Survive / Sleep / Socialize / Play / TameHorse / Build (personal) /
/// Craft / faction war state — all of which would have already won
/// in the upstream cascade and produced a goal that this system
/// short-circuits on (the `matches!` filter restricts the rewrite to
/// gather-fallback goals).
pub fn earnincome_goal_override_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    board: Res<crate::simulation::jobs::JobBoard>,
    scorer_registry: Res<crate::simulation::goal_scorers::GoalScorerRegistry>,
    mut commands: Commands,
    mut query: Query<
        (
            Entity,
            &mut AgentGoal,
            &mut PersonAI,
            &mut TargetItem,
            &FactionMember,
            &crate::simulation::person::Profession,
            &crate::simulation::skills::Skills,
            &Needs,
            &crate::economy::agent::EconomicAgent,
            &Transform,
            Option<&crate::simulation::goal_scorers::Disposition>,
            Option<&mut GoalReason>,
        ),
        (Without<Drafted>, Without<JobClaim>),
    >,
) {
    use crate::simulation::goal_scorers::{
        Disposition, GoalClass, GoalScoringContext,
    };

    for (
        entity,
        mut goal,
        mut ai,
        mut target_item,
        member,
        prof,
        skills,
        needs,
        agent,
        transform,
        disposition_opt,
        reason_opt,
    ) in query.iter_mut()
    {
        // Only rewrite the generic gather-fallback goals; everything
        // else won an upstream branch and shouldn't be overridden.
        if !matches!(
            *goal,
            AgentGoal::GatherFood | AgentGoal::GatherWood | AgentGoal::GatherStone
        ) {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }
        let Some(faction) = registry.factions.get(&member.faction_id) else {
            continue;
        };
        let disposition = disposition_opt.copied().unwrap_or_default();
        let agent_tile = (
            (transform.translation.x / TILE_SIZE).floor() as i32,
            (transform.translation.y / TILE_SIZE).floor() as i32,
        );
        // `earnincome_goal_override_system` only filters on existing
        // `Gather*` goals + `Enterprise+` tier results, so the
        // precomputed-gate fields below are unused on this path. Stub
        // them to neutral defaults so the type-checker is satisfied
        // without re-deriving expensive context. `goal_update_system`
        // fills these meaningfully in its `Scored` branch.
        let ctx = GoalScoringContext {
            agent: entity,
            agent_tile,
            now: clock.tick,
            needs,
            profession: *prof,
            skills,
            disposition,
            economic_agent: agent,
            faction_member: member,
            faction,
            board: &board,
            is_starving: false,
            faction_has_food: false,
            can_return_camp: false,
            prioritize_food: false,
            fallback_gather: AgentGoal::GatherFood,
            fallback_gather_reason: "",
            has_horse_taming: false,
            has_personal_build_site: false,
            should_craft: false,
            injury: None,
            faction_has_injured: false,
            time_of_day_bonus: 0.0,
            age_ticks: crate::simulation::utility_curves::ADULT_AGE_TICKS_PLACEHOLDER,
        };
        let Some(best) = scorer_registry.best(&ctx) else {
            continue;
        };
        // Only act on scorers at `Enterprise` tier or higher — the
        // legacy cascade in `goal_update_system` already drove every
        // Subsistence / Safety / Survival branch and we don't want
        // to double-act on those tiers here. Discretionary scorers
        // (future Play overrides, etc.) also pass through this gate
        // by virtue of running ONLY when the fallback gather goal
        // is set (the `matches!` filter above).
        if best.class < GoalClass::Enterprise {
            continue;
        }
        // For deterministic naming carry both the scorer's `reason`
        // (used by inspector / activity log) AND the goal it picks.
        let _ = (best.score, Disposition::default());
        if *goal != best.goal {
            *goal = best.goal;
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;
            target_item.0 = None;
        }
        if let Some(mut r) = reason_opt {
            r.0 = best.reason;
        } else {
            commands.entity(entity).insert(GoalReason(best.reason));
        }
    }
}

/// Phase 5: when `goal_update_system` flips an agent's `AgentGoal`, the
/// in-flight method's chain is dropped on the next `goal_dispatch_system`
/// tick (unless it's `MF_UNINTERRUPTIBLE`, in which case the preserve-arm
/// matrix in `tasks.rs` keeps it running). Record `MethodOutcome::Abandoned`
/// against the active method so its score gets the same recency penalty as
/// any other failure — without this, an agent who keeps switching goals
/// leaves phantom successes behind and never biases away from the methods
/// it kept abandoning.
///
/// Filters by `Changed<AgentGoal>` so the system fires only on actual goal
/// flips (not on every tick during a working chain). `goal_update_system`'s
/// existing `*goal != new_goal` guard ensures `Changed<AgentGoal>` only
/// triggers on real flips. Skips `MF_UNINTERRUPTIBLE` methods since their
/// chains survive the flip via the preserve-arm matrix.
/// Safety-net gate that demotes any settled-life goal to `GatherFood`
/// for members of a faction whose `CampState::Packed`. Catches goals
/// stamped outside `goal_update_system` (chief postings via
/// `JobClaim`, household systems, etc.). Runs in `ParallelA` after
/// `goal_update_system` so the per-tick selection is honoured first
/// and only blocked outcomes are corrected.
///
/// The block list is the inverse of `allowed_while_packed`: Build,
/// Craft, Farm, Haul, Stockpile, ReturnCamp, TameHorse, GatherWood,
/// GatherStone, Lead. Any of those flips to `GatherFood`, the agent's
/// task chain is cancelled, and any held `JobClaim` is dropped so the
/// chief can re-post when the band re-pitches.
pub fn mobile_state_goal_gate_system(
    mut commands: Commands,
    registry: Res<crate::simulation::faction::FactionRegistry>,
    mut q: Query<(
        Entity,
        &crate::simulation::faction::FactionMember,
        &mut AgentGoal,
        &mut crate::simulation::person::PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        Option<&crate::simulation::jobs::JobClaim>,
    )>,
) {
    for (e, member, mut goal, mut ai, mut aq, claim) in q.iter_mut() {
        let root = registry.root_faction(member.faction_id);
        let Some(faction) = registry.factions.get(&root) else {
            continue;
        };
        if !matches!(
            faction.camp_state,
            crate::simulation::faction::CampState::Packed { .. }
        ) {
            continue;
        }
        if allowed_while_packed(*goal) {
            continue;
        }
        // Settled-life goal on a Packed band — demote.
        *goal = AgentGoal::GatherFood;
        aq.cancel();
        ai.task_id = crate::simulation::person::PersonAI::UNEMPLOYED;
        ai.state = crate::simulation::person::AiState::Idle;
        if claim.is_some() {
            commands
                .entity(e)
                .remove::<crate::simulation::jobs::JobClaim>()
                .remove::<crate::simulation::jobs::ClaimTarget>();
        }
    }
}

pub fn record_abandoned_method_system(
    clock: Res<SimClock>,
    method_registry: Res<crate::simulation::htn::MethodRegistry>,
    mut query: Query<
        (&mut PersonAI, &mut crate::simulation::htn::MethodHistory),
        Changed<AgentGoal>,
    >,
) {
    use crate::simulation::htn::{MethodOutcome, MF_UNINTERRUPTIBLE};
    let now = clock.tick;
    for (mut ai, mut history) in query.iter_mut() {
        let Some(active_id) = ai.active_method else {
            continue;
        };
        let interruptible = method_registry
            .flags_by_id(active_id)
            .map(|f| f & MF_UNINTERRUPTIBLE == 0)
            .unwrap_or(true);
        if interruptible {
            history.push(active_id, MethodOutcome::Abandoned, now);
            ai.active_method = None;
        }
    }
}

/// Phase 6B/6C combined: chronic-failure release.
///
/// For each agent whose current goal is *not* `Survive` / `Sleep` and
/// whose `MethodHistory` shows ≥ `CHRONIC_FAIL_THRESHOLD` non-success
/// entries within the TTL window:
///
/// - **Without `JobClaim` (autonomous goal)**: insert the entity into
///   `ForceGoalReevaluate` so `goal_update_system` flips the goal *next*
///   tick instead of waiting up to 200 ticks. Stamp the goal into the
///   `GoalCooldown` ring so the same goal can't immediately re-fire via
///   `should_craft` / equivalent eligibility checks.
///
/// - **With `JobClaim` (chief / household / individual posting)**:
///   remove the `JobClaim` + `ClaimTarget` and release the claimant slot
///   on the posting so the chief reposts to a better-positioned worker.
///   Also stamp the goal cooldown so a subsequent `job_claim_system` run
///   doesn't immediately re-grant the same kind of claim. Orthogonal to
///   `job_claim_release_system`'s stuck-idle `fail_count` bump — that
///   one fires on `task_id == UNEMPLOYED` for STUCK_FAIL_INTERVAL ticks;
///   this one fires on the *quality* of recent attempts (whether they
///   succeeded), so an agent that keeps starting tasks but losing them
///   to target races / routing failures gets released too.
///
/// `Survive` / `Sleep` are exempt: a hungry agent who can't find food
/// via methods AND can't make Explore-fallback work would otherwise
/// be cooldown'd out of trying for food and starve. Same for sleep.
/// The terminal Explore fallback (Phase 3) is the design's relief
/// valve for chronic Survive/Sleep failure.
///
/// Cadence: every `TICKS_PER_DAY/4` (~3 min); threshold 3 failures;
/// cooldown duration matches `MethodHistory` TTL so bias and cooldown
/// expire together.
pub fn chronic_failure_release_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut force_reeval: ResMut<ForceGoalReevaluate>,
    mut board: ResMut<crate::simulation::jobs::JobBoard>,
    mut query: Query<(
        Entity,
        &AgentGoal,
        &crate::simulation::htn::MethodHistory,
        Option<&JobClaim>,
        Option<&mut GoalCooldown>,
    )>,
) {
    use crate::simulation::htn::{MethodOutcome, METHOD_HISTORY_TTL_TICKS};

    if clock.tick % (crate::world::seasons::TICKS_PER_DAY as u64 / 4) != 0 {
        return;
    }
    let now = clock.tick;
    const CHRONIC_FAIL_THRESHOLD: u32 = 3;
    const COOLDOWN_DURATION_TICKS: u64 = METHOD_HISTORY_TTL_TICKS;

    for (entity, goal, history, claim_opt, cooldown_opt) in query.iter_mut() {
        // Survive / Sleep exempt — see system-level doc.
        if matches!(*goal, AgentGoal::Survive | AgentGoal::Sleep) {
            continue;
        }
        let failures: u32 = history
            .entries
            .iter()
            .filter(|slot| {
                matches!(
                    slot,
                    Some((_, outcome, tick))
                        if !matches!(outcome, MethodOutcome::Success)
                            && now.saturating_sub(*tick) <= METHOD_HISTORY_TTL_TICKS
                )
            })
            .count() as u32;
        if failures < CHRONIC_FAIL_THRESHOLD {
            continue;
        }
        let until = now + COOLDOWN_DURATION_TICKS;

        // Stamp the cooldown ring (and legacy `craft_until_tick` for
        // `should_craft`'s fast path).
        match cooldown_opt {
            Some(mut existing) => {
                existing.push(*goal, until);
                if matches!(*goal, AgentGoal::Craft) {
                    existing.craft_until_tick = existing.craft_until_tick.max(until);
                }
            }
            None => {
                let mut cd = GoalCooldown::default();
                cd.push(*goal, until);
                if matches!(*goal, AgentGoal::Craft) {
                    cd.craft_until_tick = until;
                }
                commands.entity(entity).insert(cd);
            }
        }

        if let Some(claim) = claim_opt {
            // 6B-A: JobClaim'd path — release the claim + claimant slot.
            // The chief's next posting cycle picks a better-positioned
            // worker; this agent is free to pursue something else next
            // tick.
            let job_id = claim.job_id;
            commands.entity(entity).remove::<JobClaim>();
            commands
                .entity(entity)
                .remove::<crate::simulation::jobs::ClaimTarget>();
            crate::simulation::jobs::release_claimant(&mut board, job_id, entity);
        } else {
            // 6B-B: autonomous path — bypass the 200-tick cadence on
            // next `goal_update_system` so the new cooldown bites
            // immediately.
            force_reeval.0.insert(entity);
        }
    }
}
