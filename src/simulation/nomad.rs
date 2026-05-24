//! Nomadic-mode systems: migration trigger + caravan lifecycle.
//!
//! AI nomads survey for a final camp target, validate it before teardown,
//! pack the old camp through observable labor, physically travel as a
//! caravan, then unload/pitch a minimal final camp before normal chief
//! shelter repair resumes. Player nomads keep the explicit Pack → Move →
//! Pitch command flow.

use bevy::ecs::system::SystemState;
use bevy::prelude::*;

use crate::simulation::animals::{AnimalAI, Tamed};
use crate::simulation::construction::{
    best_hearth_for, seed_nomadic_camp, Bed, BedMap, Campfire, CampfireMap, FurnitureMaps,
    TentShelter,
};
use crate::simulation::faction::FactionRegistry;
use crate::simulation::memory::MemoryKind;
use crate::simulation::pack_deploy::Deployable;
use crate::simulation::person::UNEMPLOYED_TASK_KIND;
use crate::simulation::schedule::SimClock;
use crate::simulation::shared_knowledge::{KnowledgeTier, SharedKnowledge};
use crate::simulation::technology::current_era;
use crate::world::chunk::ChunkMap;
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::globe::{Biome, Globe};
use crate::world::seasons::{Calendar, Season, TICKS_PER_DAY, TICKS_PER_SEASON};
use crate::world::tile::TileKind;
use std::collections::VecDeque;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MigrationStage {
    PackingCamp,
    EnRoute,
    Arrived,
}

#[derive(Clone, Debug)]
pub struct MigrationOrder {
    pub target_tile: (i32, i32),
    pub stage: MigrationStage,
    pub started_tick: u32,
}

/// Tiles within `NOMAD_FORAGE_RADIUS` are "local" — clusters here count
/// toward the band's at-camp food score.
pub const NOMAD_FORAGE_RADIUS: i32 = 24;

/// Strict-AND migration trigger thresholds. A band migrates only when its
/// stored food is below `members × NOMAD_TRIGGER_FOOD_DAYS` *and* its known
/// local food clusters score below `members × NOMAD_KNOWN_FOOD_TARGET_PER_MEMBER`.
pub const NOMAD_TRIGGER_FOOD_DAYS: f32 = 3.0;
pub const NOMAD_KNOWN_FOOD_TARGET_PER_MEMBER: f32 = 1.0;
/// A survey that produces no candidate scoring at least this high returns
/// the band to `Idle` rather than committing to a poor site.
pub const NOMAD_MIN_ACCEPTABLE_SITE_SCORE: f32 = 15.0;
/// After a no-candidate survey, the band retries this many days later
/// instead of waiting a full migration period.
pub const NOMAD_NO_CANDIDATE_RETRY_DAYS: u32 = 2;

/// Minimum-stay cooldown derived from the faction's archetype: a `Mobile`
/// band must stay `migration_period_min_days`; anything else falls back to
/// one season.
pub fn migration_cooldown_ticks(home: crate::simulation::archetype::HomeMobility) -> u32 {
    match home {
        crate::simulation::archetype::HomeMobility::Mobile {
            migration_period_min_days,
        } => migration_period_min_days.saturating_mul(TICKS_PER_DAY),
        crate::simulation::archetype::HomeMobility::Anchored => TICKS_PER_SEASON,
    }
}

/// Min chebyshev distance from old camp the migration target must satisfy.
/// Phase D: kept as a soft floor — `pick_migration_target` no longer hard-
/// filters at the annulus boundary; instead it applies a continuous
/// distance penalty so very-close candidates lose to slightly-further
/// good ground but are not excluded.
pub const NOMAD_MIN_TARGET_DIST: i32 = 8;

/// Phase D: removed as a hard cap. Retained only as the *legacy*
/// fallback radius used by `nomad_migration_system` when no scouted
/// knowledge has yet expanded the band's view. The continuous distance
/// penalty in `pick_migration_target` lets scouted clusters compete from
/// far further out.
pub const NOMAD_MAX_TARGET_DIST: i32 = 200;

/// Per-tile chebyshev distance penalty added to migration candidate
/// scoring. Tuned so a 60-tile candidate loses ~24 points to a 0-tile
/// candidate of equal merit, but a 120-tile candidate that scores 50
/// higher (a real food bonanza) still wins over a mediocre nearby tile.
pub const DIST_WEIGHT: f32 = 0.4;

/// Phase D: how long the band runs in `MigrationPhase::Surveying`
/// before `nomad_survey_completion_system` re-scores and writes
/// `pending_migration`. Long enough for scouts to walk 60-150 tiles
/// out and for vision to seed faction-tier clusters.
pub const SURVEY_WINDOW_TICKS: u32 = TICKS_PER_DAY * 4;

/// Phase D: how many scouts each surveying band dispatches.
pub const SURVEY_SCOUT_COUNT: usize = 3;

/// Phase D: chebyshev radius from `home_tile` at which scouts are
/// assigned within their quadrant.
pub const SURVEY_SCOUT_RADIUS: i32 = 100;
/// Maximum Chebyshev distance for one scout travel episode. Longer trips
/// re-dispatch through checkpoints so maintenance can run between legs.
const SCOUT_CHECKPOINT_STEP: i32 = 24;

/// Phase D: per-agent companion to `AgentGoal::Scout`. Carries the
/// quadrant assignment and the actual tile to walk toward. Stamped by
/// `nomad_survey_trigger_system`; cleared by
/// `nomad_survey_completion_system` when the survey window closes.
///
/// Phase 2: also carries an optional `ScoutKind` discriminating
/// AI-survey scouts (one of four quadrants) from player-dispatched
/// manual scouts (one of eight cardinals), and an optional `report`
/// populated on arrival.
#[derive(Component, Clone, Debug)]
pub struct ScoutAssignment {
    pub quadrant: u8, // 0=NE, 1=NW, 2=SW, 3=SE (AI) / 0..8 cardinal (manual)
    pub target_tile: (i32, i32),
    pub assigned_tick: u32,
    pub kind: ScoutKind,
    pub report: Option<ScoutReport>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScoutKind {
    AiSurvey,
    PlayerManual { faction_id: u32 },
}

fn scout_checkpoint(current: (i32, i32), target: (i32, i32)) -> (i32, i32) {
    let dx = target.0 - current.0;
    let dy = target.1 - current.1;
    let dist = dx.abs().max(dy.abs());
    if dist <= SCOUT_CHECKPOINT_STEP {
        return target;
    }
    let mut next = (
        current.0 + dx.saturating_mul(SCOUT_CHECKPOINT_STEP) / dist,
        current.1 + dy.saturating_mul(SCOUT_CHECKPOINT_STEP) / dist,
    );
    if next == current {
        next.0 += dx.signum();
        next.1 += dy.signum();
    }
    next
}

/// Phase 2: result of a scouting trip; populated when a scout arrives
/// near `target_tile`. `candidates` is the local cluster summary
/// (`CampSiteCandidate`s) scored by `pick_migration_target`'s
/// component helpers; `danger_tiles` / `hostile_factions` flag risks.
#[derive(Clone, Debug, Default)]
pub struct ScoutReport {
    pub candidates: Vec<crate::simulation::faction::CampSiteCandidate>,
    pub danger_tiles: Vec<(i32, i32)>,
    pub hostile_factions: Vec<u32>,
    pub returned_tick: u32,
}

/// P3: composite-score helpers — each helper returns a signed score that's
/// summed into `MigrationScore.total`. Constants tuned so a dominant food
/// cluster (estimated_count ~4) still wins against a weak biome bonus, but
/// equal food candidates choose the better water/season/safety position.
pub const WATER_PROBE_RADIUS: i32 = 8;
pub const RECENT_CAMP_TTL: u32 = TICKS_PER_SEASON * 2;
pub const RECENT_CAMP_RING_CAP: usize = 6;
const PREDATOR_PROBE_RADIUS: i32 = 6;

/// P1: per-agent component pinning the final destination of an in-flight
/// migration. `tile` is always the final camp target. Connectivity self-heal
/// may temporarily route a member toward `route_tile`, but route waypoints
/// never count as camps and never unlock repair.
#[derive(Component, Clone, Copy, Debug)]
pub struct MigrationTarget {
    pub tile: (i32, i32),
    pub route_tile: Option<(i32, i32)>,
    pub started_tick: u32,
    /// Tick of the last successful `assign_task_with_routing` in
    /// `nomad_migration_dispatch_system`. Used by the arrival system's
    /// stall-release path: if dispatch never advances this for an Idle /
    /// UNEMPLOYED agent (Drafted, PlayerOrder, or otherwise filtered by
    /// the dispatcher), they release after `MIGRATE_STALL_TICKS` instead
    /// of waiting out the 3-day hard timeout.
    pub last_dispatched_tick: u32,
    /// Bug-fix #4 (connectivity reroute): how many times the dispatcher
    /// has rerouted this marker to a band-centroid fallback after
    /// hitting a connectivity dead end. Capped at 2 in the dispatcher
    /// to avoid spinning forever on bands that genuinely can't reach
    /// the target.
    pub bounce_count: u8,
}

/// P1: chebyshev arrival radius around the new camp. Below this, the
/// agent's `MigrationTarget` is stripped + their goal cleared so the
/// next 200-tick goal-eval picks a normal need-driven goal.
pub const MIGRATE_ARRIVAL_RADIUS: i32 = 4;

/// P1: hard timeout. After this many ticks of carrying a `MigrationTarget`,
/// the agent gives up — covers stuck-in-impassable-tile edge cases so a
/// lost member doesn't carry the marker forever.
pub const MIGRATE_TIMEOUT_TICKS: u32 = TICKS_PER_DAY * 3;

/// Stall-release window. If `last_dispatched_tick` hasn't advanced in
/// this many ticks and the agent is sitting Idle / UNEMPLOYED, the
/// arrival system releases the marker. Catches `Drafted` / `PlayerOrder`
/// members the dispatcher's filter never serves, plus genuinely stranded
/// agents whose path-worker keeps rejecting routes.
pub const MIGRATE_STALL_TICKS: u32 = TICKS_PER_DAY / 2;

/// Despawn radius for the old camp on commit. Sized to cover the seed
/// helpers' outer-ring tents (radius 5..=7 around each hearth, plus a
/// safety margin for offset hearth layouts).
pub const OLD_CAMP_RADIUS: i32 = 12;

/// Minimum chebyshev distance from current centroid for a `PitchCamp`
/// target. Set to 0 to give the player full freedom: they can pitch
/// anywhere, including the exact tile they packed from. AI caravan
/// migration validates its final target separately before teardown.
pub const MIN_PITCH_DISTANCE: i32 = 0;

/// Queue of player-issued camp state changes drained by
/// `apply_pack_camp_command_system` and `apply_pitch_camp_command_system`
/// (Sequential, exclusive `&mut World`). The dispatcher in ParallelB
/// validates each request and pushes onto the matching queue; the apply
/// systems perform the heavy world mutation (despawn + reseed +
/// registry flips).
#[derive(Resource, Default)]
pub struct PendingCampOps {
    pub packs: Vec<(u32, (i32, i32))>,
    pub pitches: Vec<PendingPitch>,
    /// Phase 2: manual scout dispatch requests. The chief actor's
    /// faction id + direction (8-cardinal) + range; apply system picks
    /// a member and stamps `ScoutAssignment`.
    pub manual_scouts: Vec<PendingManualScout>,
    /// Phase 3: faction-scoped intent set requests.
    pub intent_sets: Vec<(u32, crate::simulation::faction::MigrationIntent)>,
    /// Player-locked migration: faction-scoped autonomy mode for
    /// packed nomads. Drained alongside intent_sets.
    pub autonomy_sets: Vec<(u32, crate::simulation::faction::PackedMigrationAutonomy)>,
}

#[derive(Clone, Copy, Debug)]
pub struct PendingManualScout {
    pub fid: u32,
    pub direction: u8,
    pub range: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct PendingPitch {
    pub fid: u32,
    pub tile: (i32, i32),
    pub z: i8,
    pub command_actor: Entity,
}

/// Phase D trigger pass — Economy, every `TICKS_PER_DAY`. For each
/// nomadic band past its `TICKS_PER_SEASON` cooldown whose local food
/// cluster score is below `members × 3`, transitions
/// `migration_phase = Surveying` and dispatches up to
/// `SURVEY_SCOUT_COUNT` scouts to spread out across quadrants and seed
/// faction-tier `SharedKnowledge`. The eventual `pending_migration`
/// write happens in `nomad_survey_completion_system` once the survey
/// window closes.
pub fn nomad_migration_system(world: &mut World) {
    let tick = world.resource::<SimClock>().tick;
    if tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    let now = tick as u32;

    // Snapshot the factions that should enter Surveying this tick.
    struct Trigger {
        fid: u32,
        home: (i32, i32),
    }
    let triggers: Vec<Trigger> = {
        let registry = world.resource::<FactionRegistry>();
        let shared = world.resource::<SharedKnowledge>();
        registry
            .factions
            .iter()
            .filter_map(|(&fid, faction)| {
                if !faction.caps.home.is_mobile() {
                    return None;
                }
                // Phase 1: only autopilot factions auto-trigger Surveying.
                // Player-driven nomads stay Idle until an explicit
                // `PlayerCommand::SendScout` or `StartMigration`.
                if !faction.nomad_autopilot {
                    return None;
                }
                if faction.member_count == 0 {
                    return None;
                }
                if faction.pending_migration.is_some() {
                    return None;
                }
                if !matches!(
                    faction.migration_phase,
                    crate::simulation::faction::MigrationPhase::Idle
                ) {
                    return None;
                }
                let cooldown = migration_cooldown_ticks(faction.caps.home);
                if now < faction.last_migration_tick.saturating_add(cooldown) {
                    return None;
                }
                // Strict AND: stored-food deficit AND weak local knowledge.
                // Either alone is not enough to uproot the band.
                let members = faction.member_count.max(1) as f32;
                let food_deficit = faction.storage.food_total() < members * NOMAD_TRIGGER_FOOD_DAYS;
                let food_score =
                    score_local_food(shared, fid, faction.home_tile, NOMAD_FORAGE_RADIUS);
                let weak_local = (food_score as f32) < members * NOMAD_KNOWN_FOOD_TARGET_PER_MEMBER;
                if !(food_deficit && weak_local) {
                    return None;
                }
                Some(Trigger {
                    fid,
                    home: faction.home_tile,
                })
            })
            .collect()
    };
    if triggers.is_empty() {
        return;
    }

    // Pick scouts per faction. Lowest combined-need members; skip
    // `Drafted`, skip the chief unless the band only has 1 member.
    let mut chosen: ahash::AHashMap<u32, Vec<(Entity, u8)>> = ahash::AHashMap::default();
    {
        let mut state: SystemState<(
            Query<
                (
                    Entity,
                    &crate::simulation::faction::FactionMember,
                    &crate::simulation::needs::Needs,
                ),
                Without<crate::simulation::person::Drafted>,
            >,
            Res<FactionRegistry>,
        )> = SystemState::new(world);
        let (q, registry) = state.get(world);
        for trig in triggers.iter() {
            let chief = registry
                .factions
                .get(&trig.fid)
                .and_then(|f| f.chief_entity);
            let mut candidates: Vec<(Entity, u32)> = Vec::new();
            for (e, member, needs) in q.iter() {
                if registry.root_faction(member.faction_id) != trig.fid {
                    continue;
                }
                if Some(e) == chief {
                    continue;
                }
                let need_score = needs.shelter as u32 + needs.sleep as u32 + needs.hunger as u32;
                candidates.push((e, need_score));
            }
            candidates.sort_by_key(|(_, n)| *n);
            let pick: Vec<(Entity, u8)> = candidates
                .into_iter()
                .take(SURVEY_SCOUT_COUNT)
                .enumerate()
                .map(|(i, (e, _))| (e, i as u8))
                .collect();
            chosen.insert(trig.fid, pick);
        }
    }

    // Stamp Scout goal + ScoutAssignment on each chosen member.
    {
        let mut state: SystemState<(
            Commands,
            Query<(
                Entity,
                &mut crate::simulation::goals::AgentGoal,
                &mut crate::simulation::person::PersonAI,
                &mut crate::simulation::typed_task::ActionQueue,
            )>,
        )> = SystemState::new(world);
        let (mut commands, mut q) = state.get_mut(world);
        for trig in triggers.iter() {
            let Some(picks) = chosen.get(&trig.fid) else {
                continue;
            };
            for &(e, quadrant) in picks.iter() {
                let target = quadrant_target_tile(trig.home, quadrant);
                if let Ok((_, mut goal, mut ai, mut aq)) = q.get_mut(e) {
                    *goal = crate::simulation::goals::AgentGoal::Scout;
                    aq.cancel();
                    ai.state = crate::simulation::person::AiState::Idle;
                }
                commands.entity(e).insert(ScoutAssignment {
                    quadrant,
                    target_tile: target,
                    assigned_tick: now,
                    kind: ScoutKind::AiSurvey,
                    report: None,
                });
            }
        }
        state.apply(world);
    }

    // Update faction migration_phase + log.
    {
        let mut registry = world.resource_mut::<FactionRegistry>();
        for trig in triggers.iter() {
            if let Some(faction) = registry.factions.get_mut(&trig.fid) {
                let scouts: Vec<Entity> = chosen
                    .get(&trig.fid)
                    .map(|v| v.iter().map(|(e, _)| *e).collect())
                    .unwrap_or_default();
                let mut quadrants = [false; 4];
                for &(_, q) in chosen.get(&trig.fid).into_iter().flatten() {
                    if (q as usize) < 4 {
                        quadrants[q as usize] = true;
                    }
                }
                faction.migration_phase = crate::simulation::faction::MigrationPhase::Surveying {
                    started_tick: now,
                    scouts,
                    quadrants,
                };
                faction.last_phase_change_tick = now;
                info!(
                    "Faction {} entering Surveying tick {now}; scouts={}",
                    trig.fid,
                    chosen.get(&trig.fid).map(|v| v.len()).unwrap_or(0),
                );
            }
        }
    }
}

/// Compute a target tile for a scout in the given quadrant
/// (0=NE, 1=NW, 2=SW, 3=SE), at chebyshev radius
/// `SURVEY_SCOUT_RADIUS` from `home`. Adds a small per-quadrant jitter
/// so successive surveys don't always probe identical tiles.
fn quadrant_target_tile(home: (i32, i32), quadrant: u8) -> (i32, i32) {
    let r = SURVEY_SCOUT_RADIUS;
    let jitter = fastrand::i32(-12..=12);
    let (dx, dy) = match quadrant {
        0 => (r + jitter, r + jitter),
        1 => (-r + jitter, r + jitter),
        2 => (-r + jitter, -r + jitter),
        _ => (r + jitter, -r + jitter),
    };
    (home.0 + dx, home.1 + dy)
}

/// Phase D: completion pass. Economy, daily. For each faction in
/// `MigrationPhase::Surveying`, if the survey window has elapsed (or
/// every scout is already at-or-past their assignment), runs
/// `pick_migration_target` over the now-enlarged faction-tier
/// knowledge and writes `pending_migration`. Strips Scout goal +
/// `ScoutAssignment` from the surveyors and inserts them into
/// `ForceGoalReevaluate` so they re-pick goals next tick.
pub fn nomad_survey_completion_system(world: &mut World) {
    let tick = world.resource::<SimClock>().tick;
    if tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    let now = tick as u32;

    struct Done {
        fid: u32,
        home: (i32, i32),
        /// `Some` only when a candidate cleared `NOMAD_MIN_ACCEPTABLE_SITE_SCORE`.
        /// `None` → no acceptable site; the band returns to `Idle` with a
        /// short retry delay instead of committing to a poor camp.
        target: Option<(i32, i32)>,
        /// Retry cooldown anchor when `target` is `None`.
        retry_cooldown: u32,
        scouts: Vec<Entity>,
        candidates: Vec<crate::simulation::faction::CampSiteCandidate>,
    }

    // Gather completed surveys.
    let done: Vec<Done> = {
        let registry = world.resource::<FactionRegistry>();
        let shared = world.resource::<SharedKnowledge>();
        let chunk_map = world.resource::<ChunkMap>();
        let globe = world.resource::<Globe>();
        let calendar = world.resource::<Calendar>();
        registry
            .factions
            .iter()
            .filter_map(|(&fid, faction)| {
                let crate::simulation::faction::MigrationPhase::Surveying {
                    started_tick,
                    scouts,
                    ..
                } = &faction.migration_phase
                else {
                    return None;
                };
                if now.saturating_sub(*started_tick) < SURVEY_WINDOW_TICKS {
                    return None;
                }
                let candidates = pick_migration_candidates(
                    shared,
                    chunk_map,
                    globe,
                    calendar.season,
                    &faction.recent_camps,
                    now,
                    fid,
                    faction.home_tile,
                    NOMAD_MIN_TARGET_DIST,
                    NOMAD_MAX_TARGET_DIST,
                    faction.migration_intent,
                    crate::simulation::faction::MAX_CANDIDATE_SITES,
                );
                // Only commit to a candidate that clears the acceptable-site
                // bar. A weak best candidate → no migration; retry shortly.
                let target = candidates
                    .first()
                    .filter(|c| c.score >= NOMAD_MIN_ACCEPTABLE_SITE_SCORE)
                    .map(|c| c.anchor);
                Some(Done {
                    fid,
                    home: faction.home_tile,
                    target,
                    retry_cooldown: migration_cooldown_ticks(faction.caps.home),
                    scouts: scouts.clone(),
                    candidates,
                })
            })
            .collect()
    };
    if done.is_empty() {
        return;
    }

    // Strip Scout state from surveyors + force goal re-evaluation.
    {
        let mut state: SystemState<(
            Commands,
            ResMut<crate::simulation::goals::ForceGoalReevaluate>,
            Query<(
                Entity,
                &mut crate::simulation::goals::AgentGoal,
                &mut crate::simulation::person::PersonAI,
                &mut crate::simulation::typed_task::ActionQueue,
            )>,
        )> = SystemState::new(world);
        let (mut commands, mut force_reeval, mut q) = state.get_mut(world);
        for d in done.iter() {
            for &e in d.scouts.iter() {
                if let Ok((_, mut goal, mut ai, mut aq)) = q.get_mut(e) {
                    if matches!(*goal, crate::simulation::goals::AgentGoal::Scout) {
                        *goal = crate::simulation::goals::AgentGoal::GatherFood;
                    }
                    aq.cancel();
                    ai.state = crate::simulation::person::AiState::Idle;
                    commands.entity(e).remove::<ScoutAssignment>();
                    force_reeval.0.insert(e);
                }
            }
        }
        state.apply(world);
    }

    // Promote phase to PendingCommit + write pending_migration.
    {
        let mut registry = world.resource_mut::<FactionRegistry>();
        for d in done.iter() {
            if let Some(faction) = registry.factions.get_mut(&d.fid) {
                // Phase 2: persist the scored candidates so the panel /
                // route-edit UI can re-rank without re-running the survey.
                faction.candidate_sites.clear();
                for cand in d.candidates.iter() {
                    faction.candidate_sites.push_back(cand.clone());
                    while faction.candidate_sites.len()
                        > crate::simulation::faction::MAX_CANDIDATE_SITES
                    {
                        faction.candidate_sites.pop_front();
                    }
                }
                match d.target {
                    Some(target) => {
                        faction.pending_migration = Some(target);
                        faction.migration_phase =
                            crate::simulation::faction::MigrationPhase::PendingCommit {
                                target,
                                chosen_tick: now,
                            };
                        faction.last_phase_change_tick = now;
                        info!(
                            "Faction {} survey complete ({:?} -> {:?}) tick {now}",
                            d.fid, d.home, target,
                        );
                    }
                    None => {
                        // No acceptable site — return to Idle and retry in
                        // ~NOMAD_NO_CANDIDATE_RETRY_DAYS days rather than
                        // waiting a full migration period.
                        faction.pending_migration = None;
                        faction.migration_phase = crate::simulation::faction::MigrationPhase::Idle;
                        faction.last_phase_change_tick = now;
                        let retry = NOMAD_NO_CANDIDATE_RETRY_DAYS.saturating_mul(TICKS_PER_DAY);
                        faction.last_migration_tick =
                            now.saturating_add(retry).saturating_sub(d.retry_cooldown);
                        info!(
                            "Faction {} survey found no acceptable site tick {now}; retry ~{} days",
                            d.fid, NOMAD_NO_CANDIDATE_RETRY_DAYS,
                        );
                    }
                }
            }
        }
    }
}

/// Phase D: dispatch scouts to walk toward their assignment tile.
/// Routes a `Task::Explore { kind: AnyEdible }` so existing
/// vision-write-through systems seed clusters at the destination.
pub fn nomad_survey_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<crate::pathfinding::chunk_graph::ChunkGraph>,
    chunk_router: Res<crate::pathfinding::chunk_router::ChunkRouter>,
    chunk_connectivity: Res<crate::pathfinding::connectivity::ChunkConnectivity>,
    mut q: Query<(
        &mut crate::simulation::person::PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &crate::simulation::goals::AgentGoal,
        &Transform,
        &ScoutAssignment,
        &crate::simulation::lod::LodLevel,
    )>,
) {
    use crate::simulation::person::{AiState, PersonAI, UNEMPLOYED_TASK_KIND};
    use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
    use crate::simulation::typed_task::Task;
    use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};
    use crate::world::terrain::TILE_SIZE;
    for (mut ai, mut aq, goal, transform, scout, lod) in q.iter_mut() {
        if matches!(*lod, crate::simulation::lod::LodLevel::Dormant) {
            continue;
        }
        if !matches!(*goal, crate::simulation::goals::AgentGoal::Scout) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );
        let target = scout_checkpoint((cur_tx, cur_ty), scout.target_tile);
        let routed = assign_task_with_routing(
            &mut ai,
            (cur_tx, cur_ty),
            cur_chunk,
            target,
            TaskKind::Explore,
            None,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
        );
        if routed {
            aq.dispatch(Task::Explore {
                kind: MemoryKind::AnyEdible,
            });
        }
    }
}

struct AiPackingMigration {
    fid: u32,
    target: (i32, i32),
    old_home: (i32, i32),
    radius: i32,
}

struct AiTravelingMigration {
    fid: u32,
    target: (i32, i32),
    old_home: (i32, i32),
}

struct AiPitchingMigration {
    fid: u32,
    target: (i32, i32),
    pitch_started_tick: u32,
}

/// Caravan lifecycle pass — Sequential, every tick (exclusive system).
/// Advances AI nomads through PendingCommit → PackingCamp → Traveling →
/// PitchingCamp → Idle without teleporting the camp structures or members.
///
/// Exclusive (`&mut World`) because it touches several SystemParam bundles
/// (`FurnitureMaps`, `Commands`, multiple Queries) that together blow past
/// Bevy's 16-param ceiling. Early-outs cheaply when no faction has a
/// pending order.
pub fn nomad_migration_commit_system(world: &mut World) {
    let now = world.resource::<SimClock>().tick as u32;
    start_pending_ai_migrations(world, now);
    progress_packing_ai_migrations(world, now);
    progress_traveling_ai_migrations(world, now);
    progress_pitching_ai_migrations(world, now);
}

fn start_pending_ai_migrations(world: &mut World, now: u32) {
    struct Start {
        fid: u32,
        old_home: (i32, i32),
        target: (i32, i32),
        radius: i32,
    }

    let starts: Vec<Start> = {
        let registry = world.resource::<FactionRegistry>();
        registry
            .factions
            .iter()
            .filter_map(|(&fid, f)| {
                if !f.nomad_autopilot {
                    return None;
                }
                let crate::simulation::faction::MigrationPhase::PendingCommit { target, .. } =
                    f.migration_phase
                else {
                    return None;
                };
                if f.pending_migration != Some(target) {
                    return None;
                }
                let adoption = crate::simulation::technology_adoption::community_adoption_bitset(f);
                let era = current_era(&adoption);
                let radius =
                    crate::simulation::construction::seed_nomadic_camp_extent(f.member_count, era);
                Some(Start {
                    fid,
                    old_home: f.home_tile,
                    target,
                    radius,
                })
            })
            .collect()
    };
    if starts.is_empty() {
        return;
    }

    let decisions: Vec<(&Start, bool)> = starts
        .iter()
        .map(|s| (s, migration_target_ready(world, s.old_home, s.target)))
        .collect();
    let mut accepted: Vec<(u32, (i32, i32), i32)> = Vec::new();
    {
        let mut registry = world.resource_mut::<FactionRegistry>();
        for (s, valid) in decisions {
            let Some(faction) = registry.factions.get_mut(&s.fid) else {
                continue;
            };
            if !valid {
                faction.pending_migration = None;
                faction.migration_phase = crate::simulation::faction::MigrationPhase::Idle;
                faction.last_phase_change_tick = now;
                continue;
            }
            faction.camp_state = crate::simulation::faction::CampState::Packed { since_tick: now };
            faction.migration_phase = crate::simulation::faction::MigrationPhase::PackingCamp {
                target: s.target,
                old_home: s.old_home,
                started_tick: now,
                radius: s.radius,
            };
            faction.last_phase_change_tick = now;
            faction.cargo_manifest = crate::simulation::faction::CampCargoManifest::default();
            accepted.push((s.fid, s.old_home, s.radius));
        }
    }

    if accepted.is_empty() {
        return;
    }
    let fids: Vec<u32> = accepted.iter().map(|(fid, _, _)| *fid).collect();
    crate::simulation::nomad_pack_labor::stamp_pack_duty(world, &fids);
    crate::simulation::nomad_pack_labor::dispatch_unpitch_tasks(world, &accepted);
}

fn progress_packing_ai_migrations(world: &mut World, now: u32) {
    let packing: Vec<AiPackingMigration> = {
        let registry = world.resource::<FactionRegistry>();
        registry
            .factions
            .iter()
            .filter_map(|(&fid, f)| {
                if !f.nomad_autopilot {
                    return None;
                }
                let crate::simulation::faction::MigrationPhase::PackingCamp {
                    target,
                    old_home,
                    radius,
                    ..
                } = f.migration_phase
                else {
                    return None;
                };
                Some(AiPackingMigration {
                    fid,
                    target,
                    old_home,
                    radius,
                })
            })
            .collect()
    };
    if packing.is_empty() {
        return;
    }

    let packs: Vec<(u32, (i32, i32), i32)> = packing
        .iter()
        .map(|p| (p.fid, p.old_home, p.radius))
        .collect();
    let remaining = crate::simulation::nomad_pack_labor::pack_targets_remaining(world, &packs);
    let ready: Vec<AiPackingMigration> = packing
        .into_iter()
        .filter(|p| !remaining.contains(&p.fid))
        .collect();
    if ready.is_empty() {
        crate::simulation::nomad_pack_labor::dispatch_unpitch_tasks(world, &packs);
        return;
    }
    let still_packing: Vec<(u32, (i32, i32), i32)> = packs
        .iter()
        .copied()
        .filter(|(fid, _, _)| remaining.contains(fid))
        .collect();
    if !still_packing.is_empty() {
        crate::simulation::nomad_pack_labor::dispatch_unpitch_tasks(world, &still_packing);
    }

    let fids: Vec<u32> = ready.iter().map(|p| p.fid).collect();
    crate::simulation::nomad_pack_labor::clear_pack_duty(world, &fids);
    stamp_members_for_caravan_travel(world, &ready, now);
    redirect_owned_pack_animals(world, &ready, now);

    {
        let mut registry = world.resource_mut::<FactionRegistry>();
        for p in ready.iter() {
            if let Some(faction) = registry.factions.get_mut(&p.fid) {
                faction.migration_phase = crate::simulation::faction::MigrationPhase::Traveling {
                    target: p.target,
                    old_home: p.old_home,
                    departed_tick: now,
                    caravan_tile: p.old_home,
                };
                faction.last_phase_change_tick = now;
            }
        }
    }
}

fn stamp_members_for_caravan_travel(world: &mut World, ready: &[AiPackingMigration], now: u32) {
    let targets: ahash::AHashMap<u32, (i32, i32)> =
        ready.iter().map(|p| (p.fid, p.target)).collect();
    let mut state: SystemState<(
        Commands,
        Query<(
            Entity,
            &crate::simulation::faction::FactionMember,
            &mut crate::simulation::goals::AgentGoal,
            &mut crate::simulation::person::PersonAI,
            &mut crate::simulation::typed_task::ActionQueue,
        )>,
        Res<FactionRegistry>,
    )> = SystemState::new(world);
    let (mut commands, mut q, registry) = state.get_mut(world);
    for (e, member, mut goal, mut ai, mut aq) in q.iter_mut() {
        let root = registry.root_faction(member.faction_id);
        let Some(&target) = targets.get(&root) else {
            continue;
        };
        commands
            .entity(e)
            .insert(MigrationTarget {
                tile: target,
                route_tile: None,
                started_tick: now,
                last_dispatched_tick: now,
                bounce_count: 0,
            })
            .insert(crate::simulation::construction::HomeBed(None));
        *goal = crate::simulation::goals::AgentGoal::MigrateToCamp;
        aq.cancel();
        ai.state = crate::simulation::person::AiState::Idle;
    }
    state.apply(world);
}

fn redirect_owned_pack_animals(world: &mut World, ready: &[AiPackingMigration], now: u32) {
    let targets: ahash::AHashMap<u32, (i32, i32)> =
        ready.iter().map(|p| (p.fid, p.target)).collect();
    let mut state: SystemState<(Commands, Query<(Entity, &Tamed, &mut AnimalAI)>)> =
        SystemState::new(world);
    let (mut commands, mut q) = state.get_mut(world);
    for (e, tamed, mut ai) in q.iter_mut() {
        let Some(&target) = targets.get(&tamed.owner_faction) else {
            continue;
        };
        commands
            .entity(e)
            .insert(crate::simulation::animals::FollowingBand {
                faction: tamed.owner_faction,
                last_redirect_tick: now,
            });
        let seed = tamed.owner_faction.wrapping_mul(0x85EB_CA6B);
        let dx = ((seed & 0b11) as i32) - 2;
        let dy = (((seed >> 2) & 0b11) as i32) - 2;
        ai.target_tile = (target.0 + dx, target.1 + dy);
    }
    state.apply(world);
}

fn progress_traveling_ai_migrations(world: &mut World, now: u32) {
    let traveling: Vec<AiTravelingMigration> = {
        let registry = world.resource::<FactionRegistry>();
        registry
            .factions
            .iter()
            .filter_map(|(&fid, f)| {
                if !f.nomad_autopilot {
                    return None;
                }
                let crate::simulation::faction::MigrationPhase::Traveling {
                    target, old_home, ..
                } = f.migration_phase
                else {
                    return None;
                };
                Some(AiTravelingMigration {
                    fid,
                    target,
                    old_home,
                })
            })
            .collect()
    };
    if traveling.is_empty() {
        return;
    }

    let mut caravan_tiles: ahash::AHashMap<u32, (i32, i32)> = ahash::AHashMap::default();
    let mut arrived: Vec<AiTravelingMigration> = Vec::new();
    for t in traveling.iter() {
        let caravan = caravan_tile_for(world, t.fid, t.old_home);
        caravan_tiles.insert(t.fid, caravan);
        if any_member_near(world, t.fid, t.target, MIGRATE_ARRIVAL_RADIUS) {
            arrived.push(AiTravelingMigration {
                fid: t.fid,
                target: t.target,
                old_home: t.old_home,
            });
        }
    }

    {
        let mut registry = world.resource_mut::<FactionRegistry>();
        for (fid, caravan_tile) in caravan_tiles.iter() {
            if let Some(faction) = registry.factions.get_mut(fid) {
                if let crate::simulation::faction::MigrationPhase::Traveling {
                    target,
                    old_home,
                    departed_tick,
                    ..
                } = faction.migration_phase
                {
                    faction.migration_phase =
                        crate::simulation::faction::MigrationPhase::Traveling {
                            target,
                            old_home,
                            departed_tick,
                            caravan_tile: *caravan_tile,
                        };
                }
            }
        }
    }

    if arrived.is_empty() {
        return;
    }

    start_pitching_at_destination(world, &arrived, now);
}

fn start_pitching_at_destination(world: &mut World, arrived: &[AiTravelingMigration], now: u32) {
    {
        let mut registry = world.resource_mut::<FactionRegistry>();
        for t in arrived.iter() {
            if let Some(faction) = registry.factions.get_mut(&t.fid) {
                faction.recent_camps.push_back((t.old_home, now));
                while faction.recent_camps.len() > RECENT_CAMP_RING_CAP {
                    faction.recent_camps.pop_front();
                }
                faction.home_tile = t.target;
                faction.last_migration_tick = now;
                faction.camp_state =
                    crate::simulation::faction::CampState::Packed { since_tick: now };
                faction.migration_phase =
                    crate::simulation::faction::MigrationPhase::PitchingCamp {
                        target: t.target,
                        old_home: t.old_home,
                        started_tick: now,
                        pitch_started_tick: now,
                        repair_unlocked: false,
                    };
                faction.last_phase_change_tick = now;
                faction.cargo_manifest.pitching_started_tick = Some(now);
                faction.cargo_manifest.repair_unlocked = false;
            }
        }
    }

    sync_camp_home_tiles(world, arrived.iter().map(|t| (t.fid, t.target)));
    emit_camp_moved_events(world, arrived, now);
}

fn progress_pitching_ai_migrations(world: &mut World, now: u32) {
    let pitching: Vec<AiPitchingMigration> = {
        let registry = world.resource::<FactionRegistry>();
        registry
            .factions
            .iter()
            .filter_map(|(&fid, f)| {
                if !f.nomad_autopilot {
                    return None;
                }
                let crate::simulation::faction::MigrationPhase::PitchingCamp {
                    target,
                    pitch_started_tick,
                    ..
                } = f.migration_phase
                else {
                    return None;
                };
                Some(AiPitchingMigration {
                    fid,
                    target,
                    pitch_started_tick,
                })
            })
            .collect()
    };
    if pitching.is_empty() {
        return;
    }

    unload_pack_animals_at_destination(world, &pitching);
    dispatch_member_unload_tasks(world, &pitching);
    dispatch_pitch_tasks(world, &pitching);

    let mut completed: Vec<u32> = Vec::new();
    for p in pitching.iter() {
        if !minimal_final_camp_exists(world, p.target) {
            continue;
        }
        let enough_arrived = caravan_arrival_threshold_met(world, p.fid, p.target);
        let waited = now.saturating_sub(p.pitch_started_tick) >= TICKS_PER_DAY / 2;
        if enough_arrived || waited {
            completed.push(p.fid);
        }
    }
    if completed.is_empty() {
        return;
    }

    {
        let completed_set: ahash::AHashSet<u32> = completed.iter().copied().collect();
        let mut registry = world.resource_mut::<FactionRegistry>();
        for fid in completed.iter() {
            if let Some(faction) = registry.factions.get_mut(fid) {
                faction.pending_migration = None;
                faction.camp_state = crate::simulation::faction::CampState::Pitched;
                faction.migration_phase = crate::simulation::faction::MigrationPhase::Idle;
                faction.last_phase_change_tick = now;
                faction.cargo_manifest = crate::simulation::faction::CampCargoManifest::default();
            }
        }
        drop(registry);
        finish_caravan_member_markers(world, &completed_set);
    }
}

fn migration_target_ready(world: &mut World, old_home: (i32, i32), target: (i32, i32)) -> bool {
    let mut state: SystemState<(
        Res<ChunkMap>,
        Res<crate::pathfinding::chunk_graph::ChunkGraph>,
        Res<crate::pathfinding::connectivity::ChunkConnectivity>,
    )> = SystemState::new(world);
    let (chunk_map, chunk_graph, connectivity) = state.get(world);
    if !chunk_map.is_passable(target.0, target.1) {
        return false;
    }
    let oz = chunk_map.nearest_standable_z(old_home.0, old_home.1, 0) as i8;
    let tz = chunk_map.nearest_standable_z(target.0, target.1, oz as i32) as i8;
    connectivity.tile_reachable(
        &chunk_graph,
        (old_home.0, old_home.1, oz),
        (target.0, target.1, tz),
    )
}

fn caravan_tile_for(world: &mut World, fid: u32, fallback: (i32, i32)) -> (i32, i32) {
    let chief = {
        let registry = world.resource::<FactionRegistry>();
        registry.factions.get(&fid).and_then(|f| f.chief_entity)
    };
    if let Some(chief) = chief {
        let mut state: SystemState<Query<&Transform>> = SystemState::new(world);
        let q = state.get(world);
        if let Ok(transform) = q.get(chief) {
            return transform_tile(transform);
        }
    }

    let mut state: SystemState<(
        Query<(&crate::simulation::faction::FactionMember, &Transform)>,
        Res<FactionRegistry>,
    )> = SystemState::new(world);
    let (q, registry) = state.get(world);
    let mut sx = 0i64;
    let mut sy = 0i64;
    let mut n = 0i64;
    for (member, transform) in q.iter() {
        if registry.root_faction(member.faction_id) != fid {
            continue;
        }
        let tile = transform_tile(transform);
        sx += tile.0 as i64;
        sy += tile.1 as i64;
        n += 1;
    }
    if n == 0 {
        fallback
    } else {
        ((sx / n) as i32, (sy / n) as i32)
    }
}

fn any_member_near(world: &mut World, fid: u32, target: (i32, i32), radius: i32) -> bool {
    let mut state: SystemState<(
        Query<(&crate::simulation::faction::FactionMember, &Transform)>,
        Res<FactionRegistry>,
    )> = SystemState::new(world);
    let (q, registry) = state.get(world);
    q.iter().any(|(member, transform)| {
        registry.root_faction(member.faction_id) == fid
            && chebyshev(transform_tile(transform), target) <= radius
    })
}

fn sync_camp_home_tiles<I>(world: &mut World, homes: I)
where
    I: IntoIterator<Item = (u32, (i32, i32))>,
{
    let homes: Vec<(u32, (i32, i32))> = homes.into_iter().collect();
    if homes.is_empty() {
        return;
    }
    let mut state: SystemState<(
        Res<crate::simulation::camp::CampMap>,
        Query<&mut crate::simulation::camp::Camp>,
    )> = SystemState::new(world);
    let (map, mut q) = state.get_mut(world);
    for (fid, target) in homes {
        if let Some(e) = map.entity_for_faction(fid) {
            if let Ok(mut camp) = q.get_mut(e) {
                camp.home_tile = target;
            }
        }
    }
}

fn emit_camp_moved_events(world: &mut World, arrived: &[AiTravelingMigration], now: u32) {
    let actor_per_faction = {
        let mut state: SystemState<(
            Query<(Entity, &crate::simulation::faction::FactionMember)>,
            Res<FactionRegistry>,
        )> = SystemState::new(world);
        let (q, registry) = state.get(world);
        let mut actors: ahash::AHashMap<u32, Entity> = ahash::AHashMap::default();
        for (entity, member) in q.iter() {
            let root = registry.root_faction(member.faction_id);
            actors.entry(root).or_insert(entity);
        }
        actors
    };

    let mut state: SystemState<EventWriter<crate::ui::activity_log::ActivityLogEvent>> =
        SystemState::new(world);
    let mut writer = state.get_mut(world);
    for t in arrived.iter() {
        let Some(&actor) = actor_per_faction.get(&t.fid) else {
            continue;
        };
        writer.send(crate::ui::activity_log::ActivityLogEvent {
            tick: now as u64,
            actor,
            faction_id: t.fid,
            kind: crate::ui::activity_log::ActivityEntryKind::CampMoved {
                from: t.old_home,
                to: t.target,
            },
        });
    }
    state.apply(world);
}

fn unload_pack_animals_at_destination(world: &mut World, pitching: &[AiPitchingMigration]) {
    let targets: ahash::AHashMap<u32, (i32, i32)> =
        pitching.iter().map(|p| (p.fid, p.target)).collect();
    if targets.is_empty() {
        return;
    }
    let cargo = caravan_cargo_resources();
    let mut state: SystemState<(
        Commands,
        Query<(
            &Transform,
            &Tamed,
            &mut crate::simulation::animals::PackAnimalInventory,
        )>,
    )> = SystemState::new(world);
    let (mut commands, mut q) = state.get_mut(world);
    for (transform, tamed, mut inv) in q.iter_mut() {
        let Some(&target) = targets.get(&tamed.owner_faction) else {
            continue;
        };
        if chebyshev(transform_tile(transform), target) > MIGRATE_ARRIVAL_RADIUS + 3 {
            continue;
        }
        for rid in cargo.iter().copied() {
            let qty = inv.quantity_of(rid);
            if qty == 0 {
                continue;
            }
            let removed = inv.remove(rid, qty);
            if removed > 0 {
                crate::simulation::gather::spawn_ground_drop(
                    &mut commands,
                    target.0,
                    target.1,
                    rid,
                    removed,
                );
            }
        }
    }
    state.apply(world);
}

fn dispatch_member_unload_tasks(world: &mut World, pitching: &[AiPitchingMigration]) {
    let targets: ahash::AHashMap<u32, (i32, i32)> =
        pitching.iter().map(|p| (p.fid, p.target)).collect();
    if targets.is_empty() {
        return;
    }
    let cargo = caravan_cargo_resources();
    let mut state: SystemState<(
        Res<ChunkMap>,
        Res<crate::pathfinding::chunk_graph::ChunkGraph>,
        Res<crate::pathfinding::chunk_router::ChunkRouter>,
        Res<crate::pathfinding::connectivity::ChunkConnectivity>,
        Res<FactionRegistry>,
        Query<
            (
                Entity,
                &crate::simulation::faction::FactionMember,
                &Transform,
                &crate::economy::agent::EconomicAgent,
                &mut crate::simulation::person::PersonAI,
                &mut crate::simulation::typed_task::ActionQueue,
                &mut crate::simulation::goals::AgentGoal,
            ),
            With<crate::simulation::person::Person>,
        >,
    )> = SystemState::new(world);
    let (chunk_map, chunk_graph, chunk_router, connectivity, registry, mut q) =
        state.get_mut(world);
    for (_e, member, transform, agent, mut ai, mut aq, mut goal) in q.iter_mut() {
        let root = registry.root_faction(member.faction_id);
        let Some(&target) = targets.get(&root) else {
            continue;
        };
        let worker_tile = transform_tile(transform);
        if chebyshev(worker_tile, target) > MIGRATE_ARRIVAL_RADIUS + 4 {
            continue;
        }
        if aq.current_task_kind() != crate::simulation::person::UNEMPLOYED_TASK_KIND
            || !matches!(aq.current, crate::simulation::typed_task::Task::Idle)
        {
            continue;
        }
        let Some((rid, have)) = cargo.iter().find_map(|rid| {
            let have = agent.quantity_of_resource(*rid);
            (have > 0).then_some((*rid, have))
        }) else {
            continue;
        };
        let qty = have.min(4).min(u8::MAX as u32) as u8;
        let chunk = crate::world::chunk::ChunkCoord(
            worker_tile
                .0
                .div_euclid(crate::world::chunk::CHUNK_SIZE as i32),
            worker_tile
                .1
                .div_euclid(crate::world::chunk::CHUNK_SIZE as i32),
        );
        let routed = crate::simulation::tasks::assign_task_with_routing(
            &mut ai,
            worker_tile,
            chunk,
            target,
            crate::simulation::tasks::TaskKind::UnloadCampCargo,
            None,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &connectivity,
        );
        if routed {
            aq.cancel();
            aq.dispatch(crate::simulation::typed_task::Task::UnloadCampCargo {
                resource_id: rid,
                qty,
                tile: target,
            });
            *goal = crate::simulation::goals::AgentGoal::FollowingPlayerCommand;
        }
    }
    state.apply(world);
}

fn dispatch_pitch_tasks(world: &mut World, pitching: &[AiPitchingMigration]) {
    use crate::simulation::construction::BuildSiteKind;

    struct PitchRequest {
        fid: u32,
        kind: BuildSiteKind,
        anchor: (i32, i32),
    }

    let targets: ahash::AHashMap<u32, (i32, i32)> =
        pitching.iter().map(|p| (p.fid, p.target)).collect();
    if targets.is_empty() {
        return;
    }

    let requests: Vec<PitchRequest> = {
        let mut state: SystemState<(
            Res<ChunkMap>,
            Res<BedMap>,
            Res<CampfireMap>,
            Query<(&TentShelter, &Transform)>,
            Query<(&crate::simulation::items::GroundItem, &Transform)>,
            Query<(
                &crate::simulation::faction::FactionMember,
                &crate::simulation::typed_task::ActionQueue,
            )>,
            Res<FactionRegistry>,
        )> = SystemState::new(world);
        let (chunk_map, bed_map, campfire_map, tent_q, ground_q, active_q, registry) =
            state.get(world);
        let bedroll_id = crate::economy::core_ids::bedroll();
        let yurt_id = crate::economy::core_ids::packed_yurt();
        let mut out = Vec::new();

        for p in pitching.iter() {
            let mut used: ahash::AHashSet<(i32, i32)> = ahash::AHashSet::default();
            for tile in bed_map.0.keys().copied() {
                if chebyshev(tile, p.target) <= 8 {
                    used.insert(tile);
                }
            }
            for tile in campfire_map.0.keys().copied() {
                if chebyshev(tile, p.target) <= 4 {
                    used.insert(tile);
                }
            }
            let mut yurt_built = 0u32;
            for (shelter, transform) in tent_q.iter() {
                if matches!(
                    shelter.tier,
                    crate::simulation::construction::ShelterTier::Yurt
                ) && chebyshev(transform_tile(transform), p.target) <= 8
                {
                    yurt_built += 1;
                    used.insert(transform_tile(transform));
                }
            }

            let mut active_bedroll = 0u32;
            let mut active_yurt = 0u32;
            let mut active_campfire = false;
            for (member, aq) in active_q.iter() {
                if registry.root_faction(member.faction_id) != p.fid {
                    continue;
                }
                let Some((kind, anchor)) = aq.current.as_pitch_structure_at() else {
                    continue;
                };
                if chebyshev(anchor, p.target) > 8 {
                    continue;
                }
                used.insert(anchor);
                match kind {
                    BuildSiteKind::Bedroll => active_bedroll += 1,
                    BuildSiteKind::Yurt => active_yurt += 1,
                    BuildSiteKind::Campfire => active_campfire = true,
                    _ => {}
                }
            }

            let campfire_built = campfire_map
                .0
                .keys()
                .any(|tile| chebyshev(*tile, p.target) <= 2);
            if !campfire_built && !active_campfire {
                if let Some(anchor) =
                    find_pitch_anchor(p.target, BuildSiteKind::Campfire, &mut used, &chunk_map)
                {
                    out.push(PitchRequest {
                        fid: p.fid,
                        kind: BuildSiteKind::Campfire,
                        anchor,
                    });
                }
            }

            let ground_bedrolls = ground_q
                .iter()
                .filter(|(gi, t)| {
                    gi.item.resource_id == bedroll_id && chebyshev(transform_tile(t), p.target) <= 6
                })
                .fold(0u32, |acc, (gi, _)| acc.saturating_add(gi.qty));
            let desired_bedroll_tasks = ground_bedrolls.min(4).saturating_sub(active_bedroll);
            for _ in 0..desired_bedroll_tasks {
                if let Some(anchor) =
                    find_pitch_anchor(p.target, BuildSiteKind::Bedroll, &mut used, &chunk_map)
                {
                    out.push(PitchRequest {
                        fid: p.fid,
                        kind: BuildSiteKind::Bedroll,
                        anchor,
                    });
                }
            }

            let ground_yurts = ground_q
                .iter()
                .filter(|(gi, t)| {
                    gi.item.resource_id == yurt_id && chebyshev(transform_tile(t), p.target) <= 6
                })
                .fold(0u32, |acc, (gi, _)| acc.saturating_add(gi.qty));
            if ground_yurts > active_yurt && yurt_built < 2 {
                if let Some(anchor) =
                    find_pitch_anchor(p.target, BuildSiteKind::Yurt, &mut used, &chunk_map)
                {
                    out.push(PitchRequest {
                        fid: p.fid,
                        kind: BuildSiteKind::Yurt,
                        anchor,
                    });
                }
            }
        }
        out
    };
    if requests.is_empty() {
        return;
    }

    struct Worker {
        entity: Entity,
        tile: (i32, i32),
        chunk: crate::world::chunk::ChunkCoord,
        z: i8,
    }
    let workers_by_faction: ahash::AHashMap<u32, Vec<Worker>> = {
        let mut state: SystemState<(
            Query<
                (
                    Entity,
                    &crate::simulation::faction::FactionMember,
                    &Transform,
                    &crate::simulation::person::PersonAI,
                    &crate::simulation::typed_task::ActionQueue,
                ),
                With<crate::simulation::person::Person>,
            >,
            Res<FactionRegistry>,
        )> = SystemState::new(world);
        let (q, registry) = state.get(world);
        let mut acc: ahash::AHashMap<u32, Vec<Worker>> = ahash::AHashMap::default();
        for (entity, member, transform, ai, aq) in q.iter() {
            let root = registry.root_faction(member.faction_id);
            if !targets.contains_key(&root) {
                continue;
            }
            if aq.current_task_kind() != crate::simulation::person::UNEMPLOYED_TASK_KIND
                || !matches!(aq.current, crate::simulation::typed_task::Task::Idle)
            {
                continue;
            }
            let tile = transform_tile(transform);
            let chunk = crate::world::chunk::ChunkCoord(
                tile.0.div_euclid(crate::world::chunk::CHUNK_SIZE as i32),
                tile.1.div_euclid(crate::world::chunk::CHUNK_SIZE as i32),
            );
            acc.entry(root).or_default().push(Worker {
                entity,
                tile,
                chunk,
                z: ai.current_z,
            });
        }
        acc
    };

    let mut assignments: Vec<(
        Entity,
        PitchRequest,
        (i32, i32),
        crate::world::chunk::ChunkCoord,
        i8,
    )> = Vec::new();
    let mut used_workers: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    for req in requests.into_iter() {
        let Some(pool) = workers_by_faction.get(&req.fid) else {
            continue;
        };
        let Some(worker) = pool
            .iter()
            .filter(|w| !used_workers.contains(&w.entity))
            .min_by_key(|w| chebyshev(w.tile, req.anchor))
        else {
            continue;
        };
        used_workers.insert(worker.entity);
        assignments.push((worker.entity, req, worker.tile, worker.chunk, worker.z));
    }
    if assignments.is_empty() {
        return;
    }

    let mut state: SystemState<(
        Res<ChunkMap>,
        Res<crate::pathfinding::chunk_graph::ChunkGraph>,
        Res<crate::pathfinding::chunk_router::ChunkRouter>,
        Res<crate::pathfinding::connectivity::ChunkConnectivity>,
        Query<(
            &mut crate::simulation::person::PersonAI,
            &mut crate::simulation::typed_task::ActionQueue,
            &mut crate::simulation::goals::AgentGoal,
        )>,
    )> = SystemState::new(world);
    let (chunk_map, chunk_graph, chunk_router, connectivity, mut q) = state.get_mut(world);
    for (entity, req, worker_tile, worker_chunk, worker_z) in assignments.into_iter() {
        let Ok((mut ai, mut aq, mut goal)) = q.get_mut(entity) else {
            continue;
        };
        ai.current_z = worker_z;
        let routed = crate::simulation::tasks::assign_task_with_routing(
            &mut ai,
            worker_tile,
            worker_chunk,
            req.anchor,
            crate::simulation::tasks::TaskKind::PitchStructureAt,
            None,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &connectivity,
        );
        if routed {
            aq.cancel();
            aq.dispatch(crate::simulation::typed_task::Task::PitchStructureAt {
                kind: req.kind,
                anchor: req.anchor,
            });
            *goal = crate::simulation::goals::AgentGoal::FollowingPlayerCommand;
        }
    }
    state.apply(world);
}

fn find_pitch_anchor(
    target: (i32, i32),
    kind: crate::simulation::construction::BuildSiteKind,
    used: &mut ahash::AHashSet<(i32, i32)>,
    chunk_map: &ChunkMap,
) -> Option<(i32, i32)> {
    let (min_ring, max_ring): (i32, i32) = match kind {
        crate::simulation::construction::BuildSiteKind::Campfire => (0, 2),
        crate::simulation::construction::BuildSiteKind::Bedroll => (2, 6),
        crate::simulation::construction::BuildSiteKind::Yurt => (3, 6),
        _ => (1, 4),
    };
    for ring in min_ring..=max_ring {
        for dy in -ring..=ring {
            for dx in -ring..=ring {
                if dx.abs().max(dy.abs()) != ring {
                    continue;
                }
                let tile = (target.0 + dx, target.1 + dy);
                if used.contains(&tile) || !chunk_map.is_passable(tile.0, tile.1) {
                    continue;
                }
                let Some(kind) = chunk_map.tile_kind_at(tile.0, tile.1) else {
                    continue;
                };
                if kind == TileKind::Wall || kind == TileKind::Stone {
                    continue;
                }
                used.insert(tile);
                return Some(tile);
            }
        }
    }
    None
}

fn minimal_final_camp_exists(world: &mut World, target: (i32, i32)) -> bool {
    let mut state: SystemState<(Res<BedMap>, Res<CampfireMap>)> = SystemState::new(world);
    let (bed_map, campfire_map) = state.get(world);
    let has_campfire = campfire_map
        .0
        .keys()
        .any(|tile| chebyshev(*tile, target) <= 2);
    let has_bedroll = bed_map.0.keys().any(|tile| chebyshev(*tile, target) <= 6);
    has_campfire && has_bedroll
}

fn caravan_arrival_threshold_met(world: &mut World, fid: u32, target: (i32, i32)) -> bool {
    let mut state: SystemState<(
        Query<(&crate::simulation::faction::FactionMember, &Transform)>,
        Res<FactionRegistry>,
    )> = SystemState::new(world);
    let (q, registry) = state.get(world);
    let mut total = 0u32;
    let mut arrived = 0u32;
    for (member, transform) in q.iter() {
        if registry.root_faction(member.faction_id) != fid {
            continue;
        }
        total += 1;
        if chebyshev(transform_tile(transform), target) <= MIGRATE_ARRIVAL_RADIUS + 4 {
            arrived += 1;
        }
    }
    total == 0 || arrived.saturating_mul(5) >= total.saturating_mul(4)
}

fn finish_caravan_member_markers(world: &mut World, completed: &ahash::AHashSet<u32>) {
    if completed.is_empty() {
        return;
    }
    let fids: Vec<u32> = completed.iter().copied().collect();
    crate::simulation::nomad_pack_labor::clear_pack_duty(world, &fids);

    let mut state: SystemState<(
        Commands,
        Query<(
            Entity,
            &crate::simulation::faction::FactionMember,
            &mut crate::simulation::goals::AgentGoal,
            &mut crate::simulation::person::PersonAI,
            &mut crate::simulation::typed_task::ActionQueue,
        )>,
        Res<FactionRegistry>,
        ResMut<crate::simulation::goals::ForceGoalReevaluate>,
    )> = SystemState::new(world);
    let (mut commands, mut q, registry, mut force_reeval) = state.get_mut(world);
    for (entity, member, mut goal, mut ai, mut aq) in q.iter_mut() {
        let root = registry.root_faction(member.faction_id);
        if !completed.contains(&root) {
            continue;
        }
        commands
            .entity(entity)
            .remove::<MigrationTarget>()
            .remove::<crate::simulation::nomad_pack_labor::PackingDuty>();
        if matches!(
            *goal,
            crate::simulation::goals::AgentGoal::MigrateToCamp
                | crate::simulation::goals::AgentGoal::FollowingPlayerCommand
        ) {
            *goal = crate::simulation::goals::AgentGoal::GatherFood;
            force_reeval.0.insert(entity);
        }
        aq.cancel();
        ai.state = crate::simulation::person::AiState::Idle;
    }
    state.apply(world);
}

fn caravan_cargo_resources() -> [crate::economy::resource_catalog::ResourceId; 4] {
    [
        crate::economy::core_ids::bedroll(),
        crate::economy::core_ids::packed_yurt(),
        crate::economy::core_ids::wood(),
        crate::economy::core_ids::skin(),
    ]
}

#[inline]
fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

/// Pack and despawn the camp assets of the given factions at their
/// anchor tiles. Three passes (band redistribution → pack-into-animals
/// → despawn + refund drops). Retained as a synchronous utility for
/// legacy callers/debug tooling; AI caravan migration uses observable
/// `UnpitchStructure` labor instead. Does **not** mutate `home_tile`,
/// `camp_state`, or `migration_phase`.
///
/// Each pack entry's third field is the chebyshev radius around the
/// anchor to sweep — derive via `seed_nomadic_camp_extent(members, era)`
/// for the precise band footprint. Bug-fix #6.
pub(crate) fn pack_camp_assets_atomic(world: &mut World, packs: &[(u32, (i32, i32), i32)]) {
    if packs.is_empty() {
        return;
    }

    // ── P5: pre-migration band redistribution ───────────────────────
    // Even out essentials (bedroll, packed_yurt, preserved_meat) across
    // band members before the despawn / pack pass runs. Avoids the case
    // where one member at 99% capacity strands the band's only yurt.
    {
        let migrating: ahash::AHashSet<u32> = packs.iter().map(|(fid, _, _)| *fid).collect();
        let essentials = crate::simulation::nomad_pool::essentials_for_band();
        let mut state: SystemState<(
            Res<FactionRegistry>,
            Query<(
                Entity,
                &crate::simulation::faction::FactionMember,
                &mut crate::economy::agent::EconomicAgent,
            )>,
        )> = SystemState::new(world);
        let (registry, mut q) = state.get_mut(world);
        let mut updates: ahash::AHashMap<Entity, crate::economy::agent::EconomicAgent> =
            ahash::AHashMap::new();
        for &fid in migrating.iter() {
            let mut snapshot: Vec<(Entity, crate::economy::agent::EconomicAgent)> = q
                .iter()
                .filter(|(_, m, _)| registry.root_faction(m.faction_id) == fid)
                .map(|(e, _, a)| (e, *a))
                .collect();
            if snapshot.len() < 2 {
                continue;
            }
            let mut view: Vec<(Entity, &mut crate::economy::agent::EconomicAgent)> =
                snapshot.iter_mut().map(|(e, a)| (*e, &mut *a)).collect();
            let report =
                crate::simulation::nomad_pool::redistribute_essentials(&mut view, &essentials);
            if report.units_moved == 0 {
                continue;
            }
            for (e, a) in snapshot.into_iter() {
                updates.insert(e, a);
            }
        }
        for (e, _, mut agent) in q.iter_mut() {
            if let Some(updated) = updates.get(&e) {
                *agent = *updated;
            }
        }
        state.apply(world);
    }

    // ── P8: pack pass ─────────────────────────────────────────────
    // Walk fully-packable Deployables (Bedrolls/Yurts) within
    // OLD_CAMP_RADIUS of each anchor. Convert each to its `packed_form`
    // good and place onto the nearest tamed pack animal with capacity,
    // falling back to the nearest band member's EconomicAgent. Tents
    // (refund-only) are skipped here — the despawn pass below drops
    // their refund.
    {
        let migrating: ahash::AHashMap<u32, ((i32, i32), i32)> = packs
            .iter()
            .map(|(fid, anchor, radius)| (*fid, (*anchor, *radius)))
            .collect();
        let mut state: SystemState<(
            Query<(Entity, &Transform, &Deployable)>,
            Query<(
                Entity,
                &Transform,
                &Tamed,
                &mut crate::simulation::animals::PackAnimalInventory,
            )>,
            Query<(
                Entity,
                &crate::simulation::faction::FactionMember,
                &Transform,
                &mut crate::economy::agent::EconomicAgent,
            )>,
            Res<FactionRegistry>,
        )> = SystemState::new(world);
        let (deployable_q, mut animal_q, mut member_q, registry) = state.get_mut(world);

        // Phase 4: collect per-faction load events for manifest update.
        let mut load_events: Vec<(
            u32,
            crate::economy::resource_catalog::ResourceId,
            u32,
            Entity,
        )> = Vec::new();
        for (e, transform, deploy) in deployable_q.iter() {
            // Phase 4: enumerate both legacy `packed_form` and the new
            // `packed_bundles` so bundle-form structures (yurt v2) get
            // packed into multiple carriers.
            let mut load_set: Vec<(crate::economy::resource_catalog::ResourceId, u32)> = Vec::new();
            if let Some(packed_rid) = deploy.packed_form {
                load_set.push((packed_rid, 1));
            }
            for (rid, qty) in deploy.packed_bundles.iter() {
                load_set.push((*rid, *qty));
            }
            if load_set.is_empty() {
                continue;
            }
            let tile = transform_tile(transform);
            let mut owner: Option<u32> = None;
            let mut best_dist = i32::MAX;
            for (&fid, &(old_home, radius)) in migrating.iter() {
                let d = chebyshev(tile, old_home);
                if d <= radius && d < best_dist {
                    best_dist = d;
                    owner = Some(fid);
                }
            }
            let Some(fid) = owner else {
                continue;
            };
            // For now, only the first entry uses the existing pack
            // algorithm — bundle support extends below by looping.
            let packed_rid = load_set[0].0;
            let unit_w = packed_rid.unit_weight_g().max(1);
            let mut chosen_animal: Option<(Entity, i32)> = None;
            for (a_e, a_t, tamed, inv) in animal_q.iter() {
                if registry.root_faction(tamed.owner_faction) != fid {
                    continue;
                }
                if inv.free_capacity_g() < unit_w {
                    continue;
                }
                let a_tile = transform_tile(a_t);
                let d = chebyshev(a_tile, tile);
                if chosen_animal.map_or(true, |(_, prev_d)| d < prev_d) {
                    chosen_animal = Some((a_e, d));
                }
            }
            if let Some((a_e, _)) = chosen_animal {
                if let Ok((_, _, _, mut inv)) = animal_q.get_mut(a_e) {
                    let unfit = inv.add(packed_rid, 1);
                    if unfit == 0 {
                        load_events.push((fid, packed_rid, 1, a_e));
                        let _ = e;
                        continue;
                    }
                }
            }
            let mut chosen_member: Option<(Entity, i32)> = None;
            for (m_e, member, m_t, agent) in member_q.iter() {
                if registry.root_faction(member.faction_id) != fid {
                    continue;
                }
                if agent.free_capacity_g() < unit_w {
                    continue;
                }
                let m_tile = transform_tile(m_t);
                let d = chebyshev(m_tile, tile);
                if chosen_member.map_or(true, |(_, prev_d)| d < prev_d) {
                    chosen_member = Some((m_e, d));
                }
            }
            if let Some((m_e, _)) = chosen_member {
                if let Ok((_, _, _, mut agent)) = member_q.get_mut(m_e) {
                    let _unfit = agent.add_resource(packed_rid, 1);
                    load_events.push((fid, packed_rid, 1, m_e));
                }
            }
        }
        state.apply(world);

        // Phase 4: fold load events into the manifest.
        if !load_events.is_empty() {
            let mut registry = world.resource_mut::<FactionRegistry>();
            for (fid, rid, qty, carrier) in load_events.into_iter() {
                if let Some(faction) = registry.factions.get_mut(&fid) {
                    *faction.cargo_manifest.required.entry(rid).or_insert(0) += qty;
                    *faction
                        .cargo_manifest
                        .loaded
                        .entry((carrier, rid))
                        .or_insert(0) += qty;
                }
            }
        }
    }

    // ── Despawn pass + refund drops ─────────────────────────────────
    {
        let mut despawn_state: SystemState<(
            Commands,
            ResMut<BedMap>,
            ResMut<CampfireMap>,
            Res<crate::world::spatial::SpatialIndex>,
            Query<&mut crate::simulation::items::GroundItem>,
            Query<&Deployable>,
            Query<(Entity, &Transform), With<Deployable>>,
            Query<(Entity, &Transform), With<TentShelter>>,
        )> = SystemState::new(world);
        let (
            mut commands,
            mut bed_map,
            mut campfire_map,
            spatial,
            mut ground_q,
            deployable_data_q,
            deployable_q,
            tent_q,
        ) = despawn_state.get_mut(world);

        for &(_fid, anchor, radius) in packs.iter() {
            let mut despawned: ahash::AHashSet<Entity> = ahash::AHashSet::new();

            let bed_tiles: Vec<(i32, i32)> = bed_map
                .0
                .keys()
                .copied()
                .filter(|t| chebyshev(*t, anchor) <= radius)
                .collect();
            for tile in bed_tiles {
                if let Some(entity) = bed_map.0.remove(&tile) {
                    drop_refund_at_tile(
                        &deployable_data_q,
                        entity,
                        tile,
                        &mut commands,
                        &spatial,
                        &mut ground_q,
                    );
                    commands.entity(entity).despawn_recursive();
                    despawned.insert(entity);
                }
            }

            let fire_tiles: Vec<(i32, i32)> = campfire_map
                .0
                .keys()
                .copied()
                .filter(|t| chebyshev(*t, anchor) <= radius)
                .collect();
            for tile in fire_tiles {
                if let Some(entry) = campfire_map.0.remove(&tile) {
                    commands.entity(entry.entity).despawn_recursive();
                    despawned.insert(entry.entity);
                }
            }

            for (entity, transform) in tent_q.iter() {
                if despawned.contains(&entity) {
                    continue;
                }
                let tile = transform_tile(transform);
                if chebyshev(tile, anchor) <= radius {
                    drop_refund_at_tile(
                        &deployable_data_q,
                        entity,
                        tile,
                        &mut commands,
                        &spatial,
                        &mut ground_q,
                    );
                    commands.entity(entity).despawn_recursive();
                    despawned.insert(entity);
                }
            }

            for (entity, transform) in deployable_q.iter() {
                if despawned.contains(&entity) {
                    continue;
                }
                let tile = transform_tile(transform);
                if chebyshev(tile, anchor) <= radius {
                    drop_refund_at_tile(
                        &deployable_data_q,
                        entity,
                        tile,
                        &mut commands,
                        &spatial,
                        &mut ground_q,
                    );
                    commands.entity(entity).despawn_recursive();
                }
            }
        }
        despawn_state.apply(world);
    }
}

/// Phase 2: drain `PendingCampOps.manual_scouts` and stamp a fresh
/// `ScoutAssignment` (with `ScoutKind::PlayerManual`) on a chosen
/// member of each requesting faction. The dispatcher walks them to
/// `target_tile`; the arrival system writes a `ScoutReport` and folds
/// the local cluster summary into `candidate_sites`.
pub fn apply_manual_scout_command_system(world: &mut World) {
    let requests: Vec<PendingManualScout> = {
        let mut ops = world.resource_mut::<PendingCampOps>();
        if ops.manual_scouts.is_empty() {
            return;
        }
        std::mem::take(&mut ops.manual_scouts)
    };
    let now = world.resource::<SimClock>().tick as u32;

    // For each request, pick a free member with the lowest combined
    // need score. Skip Drafted, skip chief.
    struct Pick {
        fid: u32,
        member: Entity,
        target: (i32, i32),
        direction: u8,
    }
    let picks: Vec<Pick> = {
        let mut state: SystemState<(
            Query<
                (
                    Entity,
                    &crate::simulation::faction::FactionMember,
                    &crate::simulation::needs::Needs,
                ),
                Without<crate::simulation::person::Drafted>,
            >,
            Res<FactionRegistry>,
        )> = SystemState::new(world);
        let (q, registry) = state.get(world);
        let mut picks = Vec::new();
        for req in requests.iter() {
            let Some(faction) = registry.factions.get(&req.fid) else {
                continue;
            };
            let chief = faction.chief_entity;
            let mut best: Option<(Entity, u32)> = None;
            for (e, member, needs) in q.iter() {
                if registry.root_faction(member.faction_id) != req.fid {
                    continue;
                }
                if Some(e) == chief {
                    continue;
                }
                let n = needs.shelter as u32 + needs.sleep as u32 + needs.hunger as u32;
                if best.map_or(true, |(_, prev)| n < prev) {
                    best = Some((e, n));
                }
            }
            if let Some((member, _)) = best {
                let (dx, dy) = direction_offset(req.direction);
                let r = req.range as i32;
                let target = (faction.home_tile.0 + dx * r, faction.home_tile.1 + dy * r);
                picks.push(Pick {
                    fid: req.fid,
                    member,
                    target,
                    direction: req.direction,
                });
            }
        }
        picks
    };

    // Stamp the marker.
    {
        let mut state: SystemState<(
            Commands,
            Query<(
                Entity,
                &mut crate::simulation::goals::AgentGoal,
                &mut crate::simulation::person::PersonAI,
                &mut crate::simulation::typed_task::ActionQueue,
            )>,
        )> = SystemState::new(world);
        let (mut commands, mut q) = state.get_mut(world);
        for p in picks.iter() {
            if let Ok((_, mut goal, mut ai, mut aq)) = q.get_mut(p.member) {
                *goal = crate::simulation::goals::AgentGoal::Scout;
                aq.cancel();
                ai.state = crate::simulation::person::AiState::Idle;
            }
            commands.entity(p.member).insert(ScoutAssignment {
                quadrant: p.direction,
                target_tile: p.target,
                assigned_tick: now,
                kind: ScoutKind::PlayerManual { faction_id: p.fid },
                report: None,
            });
        }
        state.apply(world);
    }
}

/// Phase 2: arrival/completion pass for player-dispatched scouts.
/// When the scout is within `MIGRATE_ARRIVAL_RADIUS` of
/// `target_tile`, computes the local candidate set (using whatever
/// faction-tier knowledge has accumulated) and folds it into
/// `FactionData.candidate_sites`. Strips the marker and resets goal.
pub fn manual_scout_completion_system(world: &mut World) {
    use crate::simulation::faction::CampSiteCandidate;
    let now = world.resource::<SimClock>().tick as u32;

    struct Arrived {
        entity: Entity,
        fid: u32,
        anchor: (i32, i32),
    }
    let arrived: Vec<Arrived> = {
        let mut state: SystemState<Query<(Entity, &Transform, &ScoutAssignment)>> =
            SystemState::new(world);
        let q = state.get(world);
        let mut out = Vec::new();
        for (e, transform, scout) in q.iter() {
            let ScoutKind::PlayerManual { faction_id } = scout.kind else {
                continue;
            };
            let cur = transform_tile(transform);
            if chebyshev(cur, scout.target_tile) <= MIGRATE_ARRIVAL_RADIUS {
                out.push(Arrived {
                    entity: e,
                    fid: faction_id,
                    anchor: scout.target_tile,
                });
            }
        }
        out
    };
    if arrived.is_empty() {
        return;
    }

    // Compute candidates and fold them in.
    let folded: Vec<(u32, Vec<CampSiteCandidate>)> = {
        let registry = world.resource::<FactionRegistry>();
        let shared = world.resource::<SharedKnowledge>();
        let chunk_map = world.resource::<ChunkMap>();
        let globe = world.resource::<Globe>();
        let calendar = world.resource::<Calendar>();
        let mut acc = Vec::new();
        for a in arrived.iter() {
            let Some(faction) = registry.factions.get(&a.fid) else {
                continue;
            };
            // Score local clusters within ±SCOUT_LOCAL_RADIUS of arrival anchor.
            let cands = pick_migration_candidates(
                shared,
                chunk_map,
                globe,
                calendar.season,
                &faction.recent_camps,
                now,
                a.fid,
                a.anchor,
                0,
                NOMAD_MAX_TARGET_DIST,
                faction.migration_intent,
                4,
            );
            acc.push((a.fid, cands));
        }
        acc
    };

    {
        let mut registry = world.resource_mut::<FactionRegistry>();
        for (fid, cands) in folded.iter() {
            if let Some(faction) = registry.factions.get_mut(fid) {
                for cand in cands.iter() {
                    let mut cand = cand.clone();
                    cand.validated = true;
                    faction.candidate_sites.push_back(cand);
                    while faction.candidate_sites.len()
                        > crate::simulation::faction::MAX_CANDIDATE_SITES
                    {
                        faction.candidate_sites.pop_front();
                    }
                }
            }
        }
    }

    // Strip marker + reset scouts.
    {
        let mut state: SystemState<(
            Commands,
            Query<(
                Entity,
                &mut crate::simulation::goals::AgentGoal,
                &mut crate::simulation::person::PersonAI,
                &mut crate::simulation::typed_task::ActionQueue,
            )>,
            ResMut<crate::simulation::goals::ForceGoalReevaluate>,
        )> = SystemState::new(world);
        let (mut commands, mut q, mut force_reeval) = state.get_mut(world);
        for a in arrived.iter() {
            if let Ok((_, mut goal, mut ai, mut aq)) = q.get_mut(a.entity) {
                if matches!(*goal, crate::simulation::goals::AgentGoal::Scout) {
                    *goal = crate::simulation::goals::AgentGoal::GatherFood;
                }
                aq.cancel();
                ai.state = crate::simulation::person::AiState::Idle;
                commands.entity(a.entity).remove::<ScoutAssignment>();
                force_reeval.0.insert(a.entity);
            }
        }
        state.apply(world);
    }
}

/// Phase 3: apply intent set requests. Pure registry write.
pub fn apply_migration_intent_system(world: &mut World) {
    let sets: Vec<(u32, crate::simulation::faction::MigrationIntent)> = {
        let mut ops = world.resource_mut::<PendingCampOps>();
        if ops.intent_sets.is_empty() {
            return;
        }
        std::mem::take(&mut ops.intent_sets)
    };
    let mut registry = world.resource_mut::<FactionRegistry>();
    for (fid, intent) in sets.into_iter() {
        if let Some(faction) = registry.factions.get_mut(&fid) {
            faction.migration_intent = intent;
        }
    }
}

/// Player-locked migration: apply packed-autonomy set requests. Pure
/// registry write — the gate in `mobile_state_goal_gate_system` reads
/// the field next tick.
pub fn apply_packed_autonomy_system(world: &mut World) {
    let sets: Vec<(u32, crate::simulation::faction::PackedMigrationAutonomy)> = {
        let mut ops = world.resource_mut::<PendingCampOps>();
        if ops.autonomy_sets.is_empty() {
            return;
        }
        std::mem::take(&mut ops.autonomy_sets)
    };
    let mut registry = world.resource_mut::<FactionRegistry>();
    for (fid, mode) in sets.into_iter() {
        if let Some(faction) = registry.factions.get_mut(&fid) {
            faction.packed_autonomy = mode;
        }
    }
}

/// 8-cardinal direction unit vector for `SendScout`. 0=N, 1=NE, ...
fn direction_offset(dir: u8) -> (i32, i32) {
    match dir % 8 {
        0 => (0, 1),
        1 => (1, 1),
        2 => (1, 0),
        3 => (1, -1),
        4 => (0, -1),
        5 => (-1, -1),
        6 => (-1, 0),
        _ => (-1, 1),
    }
}

/// Apply system for `PlayerCommand::PackCamp`. Drains
/// `PendingCampOps.packs` and dispatches one `Task::UnpitchStructure`
/// per Deployable structure in the camp to the nearest eligible band
/// member. The faction is flipped to `CampState::Packed` immediately
/// so the goal gate stops settled-life work; the per-structure
/// dismantle is observable labor running over many ticks in
/// `unpitch_structure_task_system`. Sequential, exclusive `&mut World`.
///
pub fn apply_pack_camp_command_system(world: &mut World) {
    let raw: Vec<(u32, (i32, i32))> = {
        let mut ops = world.resource_mut::<PendingCampOps>();
        if ops.packs.is_empty() {
            return;
        }
        std::mem::take(&mut ops.packs)
    };
    let packs: Vec<(u32, (i32, i32), i32)> = {
        let registry = world.resource::<FactionRegistry>();
        raw.iter()
            .map(|(fid, anchor)| {
                let (members, era) = registry
                    .factions
                    .get(fid)
                    .map(|f| (f.member_count, current_era(&f.techs)))
                    .unwrap_or((6, crate::simulation::technology::Era::Paleolithic));
                let radius =
                    crate::simulation::construction::seed_nomadic_camp_extent(members, era);
                (*fid, *anchor, radius)
            })
            .collect()
    };

    // Stamp PackingDuty on every band member so they stay committed
    // to pack labor across the gaps between Unpitch tasks. The
    // continue-pack system below cycles them through remaining
    // structures until the camp is fully dismantled.
    crate::simulation::nomad_pack_labor::stamp_pack_duty(
        world,
        &packs.iter().map(|(fid, _, _)| *fid).collect::<Vec<_>>(),
    );

    crate::simulation::nomad_pack_labor::dispatch_unpitch_tasks(world, &packs);

    let now = world.resource::<SimClock>().tick as u32;
    let mut registry = world.resource_mut::<FactionRegistry>();
    for (fid, _anchor, _radius) in packs.iter() {
        if let Some(faction) = registry.factions.get_mut(fid) {
            faction.camp_state = crate::simulation::faction::CampState::Packed { since_tick: now };
            faction.migration_phase = crate::simulation::faction::MigrationPhase::Idle;
            faction.last_phase_change_tick = now;
            // Reset the manifest for this Pack episode.
            faction.cargo_manifest = crate::simulation::faction::CampCargoManifest::default();
            // Player-locked migration: every Pack resets autonomy to
            // `Hold` so the band defaults to "Awaiting Orders" between
            // Pack and Pitch. The player flips to `Forage` via the
            // migration panel / HUD when they want the old behavior.
            faction.packed_autonomy = crate::simulation::faction::PackedMigrationAutonomy::Hold;
        }
    }
    for (fid, anchor, _radius) in packs.iter() {
        info!("Faction {fid} PackCamp at {:?} tick {now}", anchor);
    }
}

/// Apply system for `PlayerCommand::PitchCamp`. Drains
/// `PendingCampOps.pitches`, calls `seed_nomadic_camp` at the target
/// tile for each, flips faction to `CampState::Pitched` with the new
/// `home_tile`, pushes the old home into `recent_camps`, and stamps
/// every faction member into `ForceGoalReevaluate` so they re-pick
/// goals against the fresh home next tick. Sequential, exclusive
/// `&mut World`. Runs after `apply_pack_camp_command_system` so a
/// same-tick Pack→Pitch (theoretical) is well-ordered.
pub fn apply_pitch_camp_command_system(world: &mut World) {
    let pitches: Vec<PendingPitch> = {
        let mut ops = world.resource_mut::<PendingCampOps>();
        if ops.pitches.is_empty() {
            return;
        }
        std::mem::take(&mut ops.pitches)
    };

    let now = world.resource::<SimClock>().tick as u32;

    let resolved: Vec<PitchResolved> = {
        let registry = world.resource::<FactionRegistry>();
        pitches
            .iter()
            .filter_map(|p| {
                registry.factions.get(&p.fid).map(|f| {
                    let adoption =
                        crate::simulation::technology_adoption::community_adoption_bitset(f);
                    PitchResolved {
                        fid: p.fid,
                        old_home: f.home_tile,
                        target: p.tile,
                        members: f.member_count,
                        era: current_era(&adoption),
                        hearth_tier: best_hearth_for(&adoption),
                    }
                })
            })
            .collect()
    };

    // Sweep any leftover `Deployable` structures within the OLD camp
    // footprint. Pack labor normally despawns these one by one, but if
    // the player pitches before workers finish (or before the band
    // has moved enough for everyone to reach their assigned shelter),
    // the leftovers would otherwise stay rooted at the old tile while
    // a fresh camp spawns at the new tile — exactly the "more
    // bedrolls than I started with" duplication. Refund-only Tents
    // drop their refund (loose materials) so the band can scavenge;
    // fully-packable shelters are silently discarded (their packed
    // equivalents are already in inventories from earlier pack work,
    // or the player is rage-pitching and forfeits them).
    despawn_old_camp_leftovers(world, &resolved);

    // Re-seed each pitched camp.
    for r in resolved.iter() {
        let mut seed_state: SystemState<(
            Commands,
            FurnitureMaps,
            Res<ChunkMap>,
            EventWriter<TileChangedEvent>,
        )> = SystemState::new(world);
        let (mut commands, mut maps, chunk_map, mut tile_changed) = seed_state.get_mut(world);
        let mut used: ahash::AHashSet<(i32, i32)> = ahash::AHashSet::default();
        used.insert(r.target);
        seed_nomadic_camp(
            &mut commands,
            &mut maps,
            &chunk_map,
            &mut tile_changed,
            &mut used,
            r.fid,
            r.target,
            r.members,
            r.era,
            r.hearth_tier,
        );
        seed_state.apply(world);
    }

    // Shelter conservation: `seed_nomadic_camp` spawns N bedrolls
    // (and Neolithic+ yurts) regardless of what the band already
    // carries. Pack labor stashed each unpitched shelter as a
    // `bedroll` / `packed_yurt` good in worker inventories / pack
    // animals. Without this pass the player would Pack→Pitch and
    // gain N free bedrolls every cycle. Debit the equivalent count
    // from the band pool — covers member inventories first, pack
    // animal inventories second. Missing supply is forgiven (the
    // first cycle of a fresh game has no packed goods); subsequent
    // cycles conserve exactly.
    consume_band_packed_goods_after_pitch(world, &resolved);

    // Registry mutation: flip home_tile + camp_state.
    {
        let mut registry = world.resource_mut::<FactionRegistry>();
        for r in resolved.iter() {
            if let Some(faction) = registry.factions.get_mut(&r.fid) {
                faction.recent_camps.push_back((r.old_home, now));
                while faction.recent_camps.len() > RECENT_CAMP_RING_CAP {
                    faction.recent_camps.pop_front();
                }
                faction.home_tile = r.target;
                faction.last_migration_tick = now;
                faction.camp_state = crate::simulation::faction::CampState::Pitched;
                faction.migration_phase = crate::simulation::faction::MigrationPhase::Idle;
                faction.last_phase_change_tick = now;
            }
        }
    }

    // Sync `Camp.home_tile` to the new anchor.
    {
        let mut state: SystemState<(
            Res<crate::simulation::camp::CampMap>,
            Query<&mut crate::simulation::camp::Camp>,
        )> = SystemState::new(world);
        let (map, mut q) = state.get_mut(world);
        for r in resolved.iter() {
            if let Some(e) = map.entity_for_faction(r.fid) {
                if let Ok(mut camp) = q.get_mut(e) {
                    camp.home_tile = r.target;
                }
            }
        }
    }

    // Sync the player faction's `FactionCenter` Transform. The marker
    // is spawned once at the founder's home tile and renders a
    // `camp_ascii` sprite on the world map; without this update the
    // visual stays rooted at the original spawn even after the band
    // pitches at a new location.
    {
        let player_fid = world
            .resource::<crate::simulation::faction::PlayerFaction>()
            .faction_id;
        if resolved.iter().any(|r| r.fid == player_fid) {
            let new_home = resolved
                .iter()
                .find(|r| r.fid == player_fid)
                .map(|r| r.target);
            if let Some(target) = new_home {
                let world_pos = crate::world::terrain::tile_to_world(target.0, target.1);
                let mut state: SystemState<
                    Query<
                        &mut Transform,
                        (
                            With<crate::simulation::faction::FactionCenter>,
                            With<crate::simulation::faction::PlayerFactionMarker>,
                        ),
                    >,
                > = SystemState::new(world);
                let mut q = state.get_mut(world);
                for mut t in q.iter_mut() {
                    t.translation.x = world_pos.x;
                    t.translation.y = world_pos.y;
                }
            }
        }
    }

    // Bug-fix #2: stamp `FollowingBand` on tamed animals owned by
    // freshly-pitched factions so they herd toward the new camp via
    // `following_band_animal_redirect_system` (survives Dormant LOD).
    {
        let pitched_fids: ahash::AHashSet<u32> = resolved.iter().map(|r| r.fid).collect();
        let mut state: SystemState<(Commands, Query<(Entity, &Tamed)>)> = SystemState::new(world);
        let (mut commands, q) = state.get_mut(world);
        for (e, tamed) in q.iter() {
            if pitched_fids.contains(&tamed.owner_faction) {
                commands
                    .entity(e)
                    .insert(crate::simulation::animals::FollowingBand {
                        faction: tamed.owner_faction,
                        last_redirect_tick: now,
                    });
            }
        }
        state.apply(world);
    }

    // Stamp ForceGoalReevaluate on every member of every pitched
    // faction so they re-pick goals against the fresh home next tick
    // (instead of holding stale GatherFood targets pointed at the old
    // location).
    {
        let pitched_fids: ahash::AHashSet<u32> = resolved.iter().map(|r| r.fid).collect();
        let mut state: SystemState<(
            ResMut<crate::simulation::goals::ForceGoalReevaluate>,
            Query<(Entity, &crate::simulation::faction::FactionMember)>,
            Res<FactionRegistry>,
        )> = SystemState::new(world);
        let (mut force_reeval, q, registry) = state.get_mut(world);
        for (e, member) in q.iter() {
            let root = registry.root_faction(member.faction_id);
            if pitched_fids.contains(&root) {
                force_reeval.0.insert(e);
            }
        }
    }

    // Activity log.
    {
        let mut state: SystemState<EventWriter<crate::ui::activity_log::ActivityLogEvent>> =
            SystemState::new(world);
        let mut writer = state.get_mut(world);
        for (r, p) in resolved.iter().zip(pitches.iter()) {
            writer.send(crate::ui::activity_log::ActivityLogEvent {
                tick: now as u64,
                actor: p.command_actor,
                faction_id: r.fid,
                kind: crate::ui::activity_log::ActivityEntryKind::CampMoved {
                    from: r.old_home,
                    to: r.target,
                },
            });
        }
        state.apply(world);
    }

    // Clear any lingering PackingDuty (safety net if the player pitches
    // before pack labor finished — members resume normal AI at the new
    // camp).
    let pitched_fids: Vec<u32> = resolved.iter().map(|r| r.fid).collect();
    crate::simulation::nomad_pack_labor::clear_pack_duty(world, &pitched_fids);

    for r in resolved.iter() {
        info!(
            "Faction {} PitchCamp ({:?} -> {:?}) tick {now}",
            r.fid, r.old_home, r.target,
        );
    }
}

/// Helper for the migration commit despawn pass. Looks up the entity's
/// `Deployable` data; if it carries a non-zero refund (sticks-and-leaves
/// Tent), spawns a `GroundItem` at the entity's tile via
/// `spawn_or_merge_ground_item`. No-op for Bedrolls / Yurts (their
/// `packed_form` covers the materials).
fn drop_refund_at_tile(
    deployable_q: &Query<&Deployable>,
    entity: Entity,
    tile: (i32, i32),
    commands: &mut Commands,
    spatial: &crate::world::spatial::SpatialIndex,
    ground_q: &mut Query<&mut crate::simulation::items::GroundItem>,
) {
    let Ok(deployable) = deployable_q.get(entity) else {
        return;
    };
    let Some((rid, qty)) = deployable.compute_refund_drop() else {
        return;
    };
    crate::simulation::items::spawn_or_merge_ground_item(
        commands, spatial, ground_q, tile.0, tile.1, rid, qty,
    );
}

#[inline]
fn transform_tile(transform: &Transform) -> (i32, i32) {
    use crate::world::terrain::TILE_SIZE;
    let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
    let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
    (tx, ty)
}

/// Resolved per-pitch context shared between
/// `apply_pitch_camp_command_system` and its helpers
/// (`despawn_old_camp_leftovers`, `consume_band_packed_goods_after_pitch`).
pub(crate) struct PitchResolved {
    pub(crate) fid: u32,
    pub(crate) old_home: (i32, i32),
    pub(crate) target: (i32, i32),
    pub(crate) members: u32,
    pub(crate) era: crate::simulation::technology::Era,
    pub(crate) hearth_tier: crate::simulation::construction::HearthTier,
}

/// Sweep `Deployable` structures within `seed_nomadic_camp_extent` of
/// each pitched faction's *old* home tile and despawn them. Refund-only
/// Tents drop their refund as `GroundItem`s; fully-packable shelters
/// (Bedroll / Yurt) are silently discarded — the conservation pass in
/// `consume_band_packed_goods_after_pitch` re-spawns the same count at
/// the new camp.
fn despawn_old_camp_leftovers(world: &mut World, resolved: &[PitchResolved]) {
    if resolved.is_empty() {
        return;
    }
    use crate::simulation::construction::seed_nomadic_camp_extent;

    let footprints: ahash::AHashMap<u32, ((i32, i32), i32)> = resolved
        .iter()
        .map(|r| {
            (
                r.fid,
                (r.old_home, seed_nomadic_camp_extent(r.members, r.era)),
            )
        })
        .collect();

    // Collect tear-down targets first so we don't hold queries while
    // mutating via Commands.
    struct TearDown {
        entity: Entity,
        tile: (i32, i32),
        refund: Option<(crate::economy::resource_catalog::ResourceId, u32)>,
        is_bed: bool,
        is_campfire: bool,
    }

    let teardowns: Vec<TearDown> = {
        let mut state: SystemState<(
            Query<(Entity, &Transform, &Deployable)>,
            Query<&Bed>,
            Query<&Campfire>,
        )> = SystemState::new(world);
        let (deployable_q, bed_q, campfire_q) = state.get(world);
        deployable_q
            .iter()
            .filter_map(|(e, transform, deploy)| {
                let tile = transform_tile(transform);
                // Must lie within at least one pitched faction's old
                // camp footprint.
                let mut hit = false;
                for &(old_home, radius) in footprints.values() {
                    if chebyshev(tile, old_home) <= radius {
                        hit = true;
                        break;
                    }
                }
                if !hit {
                    return None;
                }
                let refund = if deploy.packed_form.is_none() && deploy.packed_bundles.is_empty() {
                    deploy.compute_refund_drop()
                } else {
                    None
                };
                Some(TearDown {
                    entity: e,
                    tile,
                    refund,
                    is_bed: bed_q.get(e).is_ok(),
                    is_campfire: campfire_q.get(e).is_ok(),
                })
            })
            .collect()
    };

    if teardowns.is_empty() {
        return;
    }

    let mut state: SystemState<(
        Commands,
        ResMut<BedMap>,
        ResMut<CampfireMap>,
        EventWriter<TileChangedEvent>,
        Res<crate::world::spatial::SpatialIndex>,
        Query<&mut crate::simulation::items::GroundItem>,
    )> = SystemState::new(world);
    let (mut commands, mut bed_map, mut campfire_map, mut tile_changed, spatial, mut ground_q) =
        state.get_mut(world);
    for t in teardowns.iter() {
        if let Some((rid, qty)) = t.refund {
            crate::simulation::items::spawn_or_merge_ground_item(
                &mut commands,
                &spatial,
                &mut ground_q,
                t.tile.0,
                t.tile.1,
                rid,
                qty,
            );
        }
        if t.is_bed {
            bed_map.0.remove(&t.tile);
        }
        if t.is_campfire {
            campfire_map.0.remove(&t.tile);
        }
        commands.entity(t.entity).despawn_recursive();
        tile_changed.send(TileChangedEvent {
            tx: t.tile.0,
            ty: t.tile.1,
        });
    }
    state.apply(world);
}

/// Deduct `bedroll` / `packed_yurt` goods from the band pool
/// proportional to the shelters `seed_nomadic_camp` just spawned —
/// covers member inventories first, pack animal inventories second.
/// Missing supply is forgiven (first pitch of a fresh game has no
/// packed goods); a subsequent Pack→Pitch cycle conserves exactly.
fn consume_band_packed_goods_after_pitch(world: &mut World, resolved: &[PitchResolved]) {
    if resolved.is_empty() {
        return;
    }
    use crate::simulation::technology::Era;

    let bedroll_id = crate::economy::core_ids::bedroll();
    let packed_yurt_id = crate::economy::core_ids::packed_yurt();

    // Targets per faction match `seed_nomadic_camp`'s emission counts:
    // one bedroll per founder; Neolithic+ yurts at `(members/5).clamp(1, 2)`.
    let mut targets: ahash::AHashMap<u32, (u32, u32)> = ahash::AHashMap::default();
    for r in resolved.iter() {
        let bedrolls = r.members.max(1);
        let yurts = if (r.era as u8) >= (Era::Neolithic as u8) {
            (r.members.max(1) / 5).clamp(1, 2)
        } else {
            0
        };
        targets.insert(r.fid, (bedrolls, yurts));
    }

    // Pass 1: drain member inventories.
    {
        let mut state: SystemState<(
            Res<FactionRegistry>,
            Query<(
                &crate::simulation::faction::FactionMember,
                &mut crate::economy::agent::EconomicAgent,
            )>,
        )> = SystemState::new(world);
        let (registry, mut q) = state.get_mut(world);
        for (member, mut agent) in q.iter_mut() {
            let root = registry.root_faction(member.faction_id);
            let Some((bed_remaining, yurt_remaining)) = targets.get_mut(&root) else {
                continue;
            };
            if *bed_remaining > 0 {
                let have = agent.quantity_of_resource(bedroll_id);
                let take = have.min(*bed_remaining);
                if take > 0 {
                    agent.remove_resource(bedroll_id, take);
                    *bed_remaining -= take;
                }
            }
            if *yurt_remaining > 0 {
                let have = agent.quantity_of_resource(packed_yurt_id);
                let take = have.min(*yurt_remaining);
                if take > 0 {
                    agent.remove_resource(packed_yurt_id, take);
                    *yurt_remaining -= take;
                }
            }
        }
    }

    // Pass 2: drain pack-animal inventories for any leftover need.
    {
        let mut state: SystemState<(
            Res<FactionRegistry>,
            Query<(&Tamed, &mut crate::simulation::animals::PackAnimalInventory)>,
        )> = SystemState::new(world);
        let (registry, mut q) = state.get_mut(world);
        for (tamed, mut inv) in q.iter_mut() {
            let root = registry.root_faction(tamed.owner_faction);
            let Some((bed_remaining, yurt_remaining)) = targets.get_mut(&root) else {
                continue;
            };
            if *bed_remaining > 0 {
                let have = inv.quantity_of(bedroll_id);
                let take = have.min(*bed_remaining);
                if take > 0 {
                    inv.remove(bedroll_id, take);
                    *bed_remaining -= take;
                }
            }
            if *yurt_remaining > 0 {
                let have = inv.quantity_of(packed_yurt_id);
                let take = have.min(*yurt_remaining);
                if take > 0 {
                    inv.remove(packed_yurt_id, take);
                    *yurt_remaining -= take;
                }
            }
        }
    }
}

fn score_local_food(shared: &SharedKnowledge, fid: u32, home: (i32, i32), radius: i32) -> u16 {
    let Some(map) = shared.map(KnowledgeTier::Faction(fid)) else {
        return 0;
    };
    let mut score: u16 = 0;
    for c in map.clusters.values() {
        if !matches!(c.kind, MemoryKind::AnyEdible) {
            continue;
        }
        if chebyshev(c.center, home) <= radius {
            score = score.saturating_add(c.estimated_count);
        }
    }
    score
}

/// Composite score for a candidate migration target tile. `total` is the
/// authoritative ranking field; sub-scores are exposed for debug/inspect.
#[derive(Clone, Copy, Debug, Default)]
pub struct MigrationScore {
    pub food: i32,
    pub herd: i32,
    pub water: i32,
    pub biome_season: i32,
    pub danger: i32,
    pub recency: i32,
    pub total: i32,
}

/// P3 picker. Composite-scores known food clusters + wild-herd leaders,
/// adding water/biome-season bonuses and predator/recency penalties; picks
/// the highest-total candidate within the distance band.
///
/// Phase 3: the `intent` weight vector multiplies per-component scores
/// before summing. AI passes `MigrationIntent::FreeRoute` for the
/// uniform pre-Phase-3 baseline.
#[allow(clippy::too_many_arguments)]
pub fn pick_migration_target(
    shared: &SharedKnowledge,
    chunk_map: &ChunkMap,
    globe: &Globe,
    season: Season,
    recent_camps: &VecDeque<((i32, i32), u32)>,
    now: u32,
    fid: u32,
    home: (i32, i32),
    min_d: i32,
    max_d: i32,
    intent: crate::simulation::faction::MigrationIntent,
) -> Option<(i32, i32)> {
    pick_migration_candidates(
        shared,
        chunk_map,
        globe,
        season,
        recent_camps,
        now,
        fid,
        home,
        min_d,
        max_d,
        intent,
        1,
    )
    .into_iter()
    .next()
    .map(|c| c.anchor)
}

/// Phase 2: returns the top-`k` scored `CampSiteCandidate`s, in
/// descending score order. Used by the survey completion path to seed
/// `FactionData.candidate_sites` and by the player-side migration
/// panel for "show me my options" pin rendering.
#[allow(clippy::too_many_arguments)]
pub fn pick_migration_candidates(
    shared: &SharedKnowledge,
    chunk_map: &ChunkMap,
    globe: &Globe,
    season: Season,
    recent_camps: &VecDeque<((i32, i32), u32)>,
    now: u32,
    fid: u32,
    home: (i32, i32),
    min_d: i32,
    max_d: i32,
    intent: crate::simulation::faction::MigrationIntent,
    k: usize,
) -> Vec<crate::simulation::faction::CampSiteCandidate> {
    use crate::simulation::faction::{CampSiteCandidate, CandidateReason};
    use crate::world::tile::TileKind;
    let w = intent.weights();
    let mut scored: Vec<(CampSiteCandidate, i32)> = Vec::new();

    let mut consider = |tile: (i32, i32), food: i32, herd: i32| {
        let d = chebyshev(tile, home);
        if d < min_d || d > max_d {
            return;
        }
        // Early reject: never propose an impassable / unsettleable centre
        // tile — these would only fail at the `migration_target_ready`
        // commit gate after wasting a survey window.
        if let Some(tk) = chunk_map.tile_kind_at(tile.0, tile.1) {
            if matches!(
                tk,
                TileKind::Water | TileKind::River | TileKind::Wall | TileKind::Ore
            ) {
                return;
            }
        }
        let water = score_water(chunk_map, tile, WATER_PROBE_RADIUS);
        let biome_season = score_biome_season(globe, tile, season);
        let danger = score_danger(shared, fid, tile);
        let recency = score_recency(recent_camps, tile, now);
        let dist_pen = -((d as f32 * DIST_WEIGHT * w[5]) as i32);
        // Apply intent weights to each component.
        let weighted_food = (food as f32 * w[0]) as i32;
        let weighted_herd = (herd as f32 * w[1]) as i32;
        let weighted_water = (water as f32 * w[2]) as i32;
        let weighted_biome = (biome_season as f32 * w[3]) as i32;
        // Danger is a negative — weighting it higher means the player
        // *cares more* about avoiding it.
        let weighted_danger = (danger as f32 * w[4]) as i32;
        let total = weighted_food
            + weighted_herd
            + weighted_water
            + weighted_biome
            + weighted_danger
            + recency
            + dist_pen;
        let mut reasons = Vec::new();
        if water >= 20 {
            reasons.push(CandidateReason::FreshWater);
        }
        if food >= 4 {
            reasons.push(CandidateReason::Pasture);
        }
        if herd >= 20 {
            reasons.push(CandidateReason::Herd);
        }
        if danger <= -10 {
            reasons.push(CandidateReason::Wolves);
        }
        if biome_season <= -10 {
            reasons.push(CandidateReason::SnowRisk);
        }
        if d > 80 {
            reasons.push(CandidateReason::LongCarry);
        }
        scored.push((
            CampSiteCandidate {
                anchor: tile,
                z: 0,
                score: total as f32,
                reasons,
                discovered_tick: now,
                validated: false,
            },
            total,
        ));
    };

    // Knowledge-gated: only clusters the faction has actually scouted into
    // its faction-tier `SharedKnowledge` produce candidates — no omniscient
    // `WildHerdRegistry` scan.
    if let Some(map) = shared.map(KnowledgeTier::Faction(fid)) {
        for c in map.clusters.values() {
            match c.kind {
                MemoryKind::AnyEdible => consider(c.center, c.estimated_count as i32, 0),
                MemoryKind::HerdSighting => {
                    let herd_score = (c.estimated_count as i32 / 2).max(20);
                    consider(c.center, 0, herd_score);
                }
                _ => {}
            }
        }
    }

    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.into_iter().take(k.max(1)).map(|(c, _)| c).collect()
}

/// +30 at the candidate tile when adjacent water; falls off ~3 per chebyshev
/// tile, capped at 0 beyond `WATER_PROBE_RADIUS`. Fresh water (rivers) adds a
/// flat `+10` so a band picks a riverside camp over an equidistant salt
/// coast. Bands strongly prefer camps with reliable water access.
pub fn score_water(chunk_map: &ChunkMap, tile: (i32, i32), radius: i32) -> i32 {
    for r in 0..=radius {
        for dx in -r..=r {
            for dy in -r..=r {
                if dx.abs() != r && dy.abs() != r {
                    continue; // outline only — concentric expansion
                }
                if let Some(kind) = chunk_map.tile_kind_at(tile.0 + dx, tile.1 + dy) {
                    if kind.is_water_like() {
                        let base = (30 - r * 3).max(0);
                        let fresh = if kind.is_freshwater() { 10 } else { 0 };
                        return base + fresh;
                    }
                }
            }
        }
    }
    0
}

/// Per-biome × per-season suitability bonus. Winter penalises Tundra /
/// Mountain; Summer penalises Desert; Grassland gets a year-round bonus
/// (rich forage); Ocean is a hard reject.
pub fn score_biome_season(globe: &Globe, tile: (i32, i32), season: Season) -> i32 {
    let biome = crate::world::biome::classify_at_tile(globe, tile.0, tile.1);
    let base: i32 = match biome {
        Biome::Ocean => -100,
        Biome::Grassland => 15,
        Biome::Steppe => 12,
        Biome::Temperate => 10,
        Biome::Wetland => 8,
        Biome::Tropical => 5,
        Biome::Taiga => 0,
        Biome::Desert => -5,
        Biome::Tundra => -5,
        Biome::Badlands => -8,
        Biome::Mountain => -10,
    };
    let seasonal: i32 = match (biome, season) {
        (Biome::Tundra | Biome::Mountain, Season::Winter) => -15,
        (Biome::Desert | Biome::Badlands, Season::Summer) => -15,
        (Biome::Wetland, Season::Summer) => -8, // mosquito / disease load
        (Biome::Grassland | Biome::Steppe, Season::Spring | Season::Summer) => 5,
        (Biome::Tropical, Season::Winter) => 5,
        _ => 0,
    };
    base + seasonal
}

/// Penalises tiles near sighted hostile war parties. −15 per
/// `MemoryKind::HostileFactionSighting` cluster centre within
/// `PREDATOR_PROBE_RADIUS`.
///
/// **Do not** reinstate a `MemoryKind::Prey` arm here: `Prey` mixes
/// predators and game (deer), so it was never a real danger signal — a
/// deer-rich meadow is *good* grazing, not a threat. Wild-herd opportunity
/// is now scored separately via `MemoryKind::HerdSighting` in
/// `pick_migration_candidates`.
pub fn score_danger(shared: &SharedKnowledge, fid: u32, tile: (i32, i32)) -> i32 {
    let Some(map) = shared.map(KnowledgeTier::Faction(fid)) else {
        return 0;
    };
    let mut penalty: i32 = 0;
    for c in map.clusters.values() {
        if !matches!(c.kind, MemoryKind::HostileFactionSighting) {
            continue;
        }
        if chebyshev(c.center, tile) <= PREDATOR_PROBE_RADIUS {
            penalty -= 15;
        }
    }
    penalty
}

/// Penalises tiles near recent camp sites. Decays with age over
/// `RECENT_CAMP_TTL`. A freshly-vacated tile within 8 chebyshev gets
/// −25; older entries fade to ~0 as their age approaches the TTL.
pub fn score_recency(
    recent_camps: &VecDeque<((i32, i32), u32)>,
    tile: (i32, i32),
    now: u32,
) -> i32 {
    let mut penalty: i32 = 0;
    for &(pos, when) in recent_camps.iter() {
        if chebyshev(pos, tile) >= 8 {
            continue;
        }
        let age = now.saturating_sub(when);
        if age >= RECENT_CAMP_TTL {
            continue;
        }
        // Linear decay from -25 at age=0 to 0 at age=TTL.
        let factor = 1.0 - (age as f32 / RECENT_CAMP_TTL as f32);
        penalty -= (25.0 * factor) as i32;
    }
    penalty
}

/// Stable-camp duration before a nomadic faction may sedentarize. One
/// full game-year = 4 seasons. Bands moving more often than annually
/// stay nomadic indefinitely.
pub const NOMAD_SEDENTARIZE_TICKS: u32 = TICKS_PER_SEASON * 4;

/// Min member count for sedentarization. Small bands keep moving — they
/// need enough hands to build huts + walls before food runs out.
pub const NOMAD_SEDENTARIZE_MIN_MEMBERS: u32 = 12;

/// Phase 11: nomadic → settled lifestyle conversion. Economy, daily.
///
/// A nomadic faction that has stayed in one camp for ≥ `NOMAD_SEDENTARIZE_TICKS`
/// (one full game-year) AND has ≥ `NOMAD_SEDENTARIZE_MIN_MEMBERS` adults
/// flips `lifestyle = Settled`. From the next tick:
/// - `auto_found_default_settlements_system` founds a `Settlement` at the
///   current camp tile (which becomes the permanent home).
/// - `carve_plots_system` carves plots in the resulting `SettlementPlan`.
/// - `chief_directive_system` and `chief_job_posting_system` re-engage,
///   queuing huts/walls/granaries.
/// - `compute_faction_storage_system` switches back to the
///   `FactionStorageTile` rollup — but the storage tile doesn't exist yet,
///   so faction.storage.totals briefly reads 0 until the chief posts a
///   build for one (settled bands seed their storage tile at spawn; a
///   newly-sedentarized band would need it queued separately, follow-on).
///
/// Reverse direction (settled → nomadic on collapse) is deferred.
pub fn nomad_sedentarize_system(
    registry: Res<FactionRegistry>,
    clock: Res<SimClock>,
    mut lifecycle_queue: ResMut<crate::simulation::lifecycle::LifecycleEventQueue>,
) {
    if clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    let now = clock.tick as u32;
    for (&fid, faction) in registry.factions.iter() {
        // Capability check: only mobile-home archetypes can sedentarize.
        if !faction.caps.home.is_mobile() {
            continue;
        }
        if faction.member_count < NOMAD_SEDENTARIZE_MIN_MEMBERS {
            continue;
        }
        if faction.pending_migration.is_some() {
            continue; // about to move; not stable
        }
        // last_migration_tick == 0 means "never moved since spawn" — we
        // treat the spawn tick (0) as the start of the stay, so a faction
        // that hasn't migrated for a full year sedentarizes naturally.
        let stay_duration = now.saturating_sub(faction.last_migration_tick);
        if stay_duration < NOMAD_SEDENTARIZE_TICKS {
            continue;
        }
        info!(
            "Faction {fid} sedentarized (stable for {stay_duration} ticks at {:?}) tick {now}",
            faction.home_tile,
        );
        // P3: emit SwitchArchetype event. The lifecycle processor
        // (exclusive World, runs later in this tick) executes the
        // 7-step re-derivation: caps + land_policy + economic_policy
        // re-applied, old camp structures despawned, culture_hash
        // bumped, FactionStorageTile spawned synchronously, and the
        // `Sedentarized` activity log event emitted.
        let new_key = crate::simulation::lifecycle::settled_variant_of(&faction.caps.archetype_key);
        lifecycle_queue.push(
            crate::simulation::lifecycle::SettlementLifecycleEvent::SwitchArchetype {
                faction: fid,
                new_archetype_key: new_key,
                at_tile: faction.home_tile,
            },
        );
    }
}

/// Deterministic compass-direction fallback target. The autonomous survey
/// path no longer uses this — a no-candidate survey returns to `Idle` and
/// retries rather than blindly committing. Retained for the debug / manual
/// `PlayerCommand` migration tooling, which can still force a direction.
#[allow(dead_code)]
fn fallback_direction(fid: u32, home: (i32, i32), now: u32) -> (i32, i32) {
    let seed = fid.wrapping_mul(0x9E37_79B9).wrapping_add(now);
    let dir = (seed % 8) as i32;
    let (dx, dy) = match dir {
        0 => (35, 0),
        1 => (25, 25),
        2 => (0, 35),
        3 => (-25, 25),
        4 => (-35, 0),
        5 => (-25, -25),
        6 => (0, -35),
        _ => (25, -25),
    };
    (home.0 + dx, home.1 + dy)
}

/// P2 (slim nomad chief): per-faction shelter targets used by
/// `nomad_chief_directive_system` to size replacement blueprint queues.
fn nomad_shelter_targets(members: u32) -> NomadShelterTargets {
    NomadShelterTargets {
        bedrolls: members,
        tents: ((members + 3) / 4).max(1),
        yurts: (members / 5).clamp(1, 2),
    }
}

#[derive(Copy, Clone, Debug)]
pub struct NomadShelterTargets {
    pub bedrolls: u32,
    pub tents: u32,
    pub yurts: u32,
}

/// P2: max bps the nomad chief queues per tick (bounded so a brand-new
/// camp doesn't get carpet-bombed with 30 blueprints all at once).
const NOMAD_DIRECTIVE_BP_PER_TICK: usize = 2;

/// P2: scan radius around `home_tile` for shelter counts + new-blueprint
/// placement. Aligns with the seed/nomad_camp footprint.
const NOMAD_DIRECTIVE_RADIUS: i32 = 8;

/// P2: slim chief for nomadic bands. Daily, queues replacement Bedroll /
/// Tent / Yurt blueprints when the camp's shelter falls below the
/// per-member targets. Posts no jobs (members do autonomous gathering);
/// the existing `gather` / `scavenge` HTN methods + the new
/// `nomad_band_pool_balance_system` (P5) handle materials end-to-end.
#[allow(clippy::too_many_arguments)]
pub fn nomad_chief_directive_system(
    mut commands: Commands,
    registry: Res<FactionRegistry>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    mut bp_map: ResMut<crate::simulation::construction::BlueprintMap>,
    bed_map: Res<crate::simulation::construction::BedMap>,
    tent_q: Query<(&Transform, &crate::simulation::construction::TentShelter)>,
    bp_query: Query<&crate::simulation::construction::Blueprint>,
) {
    use crate::simulation::construction::{next_clear_tile, Blueprint, BuildSiteKind, ShelterTier};

    if clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    let now_tick = clock.tick;
    for (&fid, faction) in registry.factions.iter() {
        if !faction.caps.home.is_mobile() {
            continue;
        }
        if faction.pending_migration.is_some() {
            continue;
        }
        if faction.member_count == 0 {
            continue;
        }
        // Phase C: Packed bands have no shelters to maintain — wait
        // for the next Pitch before queueing replacement blueprints.
        if matches!(
            faction.camp_state,
            crate::simulation::faction::CampState::Packed { .. }
        ) {
            continue;
        }
        let home = faction.home_tile;
        let targets = nomad_shelter_targets(faction.member_count);

        // Count built shelter within radius of home.
        let bedroll_built = bed_map
            .0
            .keys()
            .filter(|&&t| chebyshev(t, home) <= NOMAD_DIRECTIVE_RADIUS)
            .count() as u32;
        let mut tent_built: u32 = 0;
        let mut yurt_built: u32 = 0;
        for (t_t, shelter) in tent_q.iter() {
            let tile = transform_tile(t_t);
            if chebyshev(tile, home) > NOMAD_DIRECTIVE_RADIUS {
                continue;
            }
            match shelter.tier {
                ShelterTier::Tent => tent_built += 1,
                ShelterTier::Yurt => yurt_built += 1,
            }
        }

        // Count pending blueprints (avoid re-queueing).
        let mut bedroll_pending: u32 = 0;
        let mut tent_pending: u32 = 0;
        let mut yurt_pending: u32 = 0;
        for bp in bp_query.iter() {
            if bp.faction_id != fid {
                continue;
            }
            if chebyshev(bp.tile, home) > NOMAD_DIRECTIVE_RADIUS {
                continue;
            }
            match bp.kind {
                BuildSiteKind::Bedroll => bedroll_pending += 1,
                BuildSiteKind::Tent => tent_pending += 1,
                BuildSiteKind::Yurt => yurt_pending += 1,
                _ => {}
            }
        }

        let mut budget = NOMAD_DIRECTIVE_BP_PER_TICK;
        let mut used: ahash::AHashSet<(i32, i32)> = bp_map.0.keys().copied().collect();
        // Helper: queue one Single blueprint of `kind` near home.
        let queue_one = |budget: &mut usize,
                         used: &mut ahash::AHashSet<(i32, i32)>,
                         bp_map: &mut crate::simulation::construction::BlueprintMap,
                         commands: &mut Commands,
                         kind: BuildSiteKind|
         -> bool {
            if *budget == 0 {
                return false;
            }
            let tile = match next_clear_tile(home, used, &chunk_map, NOMAD_DIRECTIVE_RADIUS) {
                Some(t) => t,
                None => return false,
            };
            let target_z = chunk_map.surface_z_at(tile.0, tile.1) as i8;
            use crate::world::terrain::tile_to_world;
            let wp = tile_to_world(tile.0, tile.1);
            let e = commands
                .spawn((
                    Blueprint::new(fid, None, kind, tile, target_z),
                    Transform::from_xyz(wp.x, wp.y, 0.3),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            bp_map.0.insert(tile, e);
            used.insert(tile);
            *budget -= 1;
            true
        };

        // Priority order: bedrolls (every member sleeps), then tents
        // (group shelter), then yurts (advanced, Neolithic+ tech-gated
        // by recipe).
        if bedroll_built + bedroll_pending < targets.bedrolls {
            queue_one(
                &mut budget,
                &mut used,
                &mut bp_map,
                &mut commands,
                BuildSiteKind::Bedroll,
            );
        }
        if tent_built + tent_pending < targets.tents {
            queue_one(
                &mut budget,
                &mut used,
                &mut bp_map,
                &mut commands,
                BuildSiteKind::Tent,
            );
        }
        if yurt_built + yurt_pending < targets.yurts
            && faction
                .techs
                .has(crate::simulation::technology::PORTABLE_DWELLINGS)
        {
            queue_one(
                &mut budget,
                &mut used,
                &mut bp_map,
                &mut commands,
                BuildSiteKind::Yurt,
            );
        }
        let _ = now_tick;
    }
}

/// P1 dispatcher — ParallelB. For every agent carrying a `MigrationTarget`
/// whose goal is `MigrateToCamp` and who is otherwise idle, dispatch
/// `Task::WalkTo { tile, why: Migration }` via `assign_task_with_routing`.
/// Bucket-gated like other ParallelB dispatchers via `BucketSlot`.
///
/// Self-heals two failure modes the plain "queue is Idle" gate would
/// otherwise leave permanently parked:
/// - A stale `Task::WalkTo { why: Migration }` left on `aq.current` by
///   `movement::release_to_idle` (which clears `task_id` but not `aq`).
///   `goal_dispatch_system`'s stale-reset is gated on
///   `task_id != UNEMPLOYED` and so misses this case.
/// - A target whose chunk is in a different connectivity component than
///   the agent's: routing always "succeeds" at this layer for non-adjacent
///   tasks, then the path worker rejects it forever. We fail fast here
///   and release the marker so the agent re-evaluates next tick.
#[allow(clippy::too_many_arguments)]
pub fn nomad_migration_dispatch_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    chunk_graph: Res<crate::pathfinding::chunk_graph::ChunkGraph>,
    chunk_router: Res<crate::pathfinding::chunk_router::ChunkRouter>,
    chunk_map: Res<ChunkMap>,
    chunk_connectivity: Res<crate::pathfinding::connectivity::ChunkConnectivity>,
    registry: Res<FactionRegistry>,
    mut qs: ParamSet<(
        // Pass 1: snapshot per-faction migrating-member tiles for centroid reroute.
        Query<(
            &MigrationTarget,
            &Transform,
            &crate::simulation::faction::FactionMember,
        )>,
        // Pass 2: actual dispatcher mutation pass.
        Query<(
            Entity,
            &mut MigrationTarget,
            &mut crate::simulation::goals::AgentGoal,
            &Transform,
            &mut crate::simulation::person::PersonAI,
            &mut crate::simulation::typed_task::ActionQueue,
            &crate::simulation::schedule::BucketSlot,
            &crate::simulation::lod::LodLevel,
            &crate::simulation::faction::FactionMember,
        )>,
    )>,
) {
    use crate::simulation::lod::LodLevel;
    use crate::simulation::person::{AiState, PersonAI};
    use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
    use crate::simulation::typed_task::{Task, WalkReason};
    use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};
    use crate::world::terrain::TILE_SIZE;

    let now = clock.tick as u32;
    // Pass 1: snapshot per-root-faction migrant tiles for centroid reroute.
    let mut tiles_per_faction: ahash::AHashMap<u32, Vec<(i32, i32)>> = ahash::AHashMap::default();
    {
        let snap_q = qs.p0();
        for (_t, peer_xform, peer_member) in snap_q.iter() {
            let root = registry.root_faction(peer_member.faction_id);
            let px = (peer_xform.translation.x / TILE_SIZE).floor() as i32;
            let py = (peer_xform.translation.y / TILE_SIZE).floor() as i32;
            tiles_per_faction.entry(root).or_default().push((px, py));
        }
    }
    let mut q = qs.p1();
    for (e, mut target, mut goal, transform, mut ai, mut aq, slot, lod, member) in q.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !clock.is_active(slot.0) {
            continue;
        }
        if *goal != crate::simulation::goals::AgentGoal::MigrateToCamp {
            continue;
        }
        if aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        // Self-heal: `release_to_idle` (movement.rs) wipes `task_id` and
        // `state` after a path-worker failure but does not touch
        // `aq.current` — leaving a stale `Task::WalkTo { Migration }` that
        // would otherwise block re-dispatch forever. Drop it here so the
        // route below can run.
        if !matches!(aq.current, Task::Idle) {
            let stale_migration_walk = matches!(
                aq.current,
                Task::WalkTo {
                    why: WalkReason::Migration,
                    ..
                },
            );
            if stale_migration_walk && ai.state == AiState::Idle {
                aq.cancel();
            } else {
                continue;
            }
        }
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let route_tile = target.route_tile.unwrap_or(target.tile);
        if target.route_tile.is_some()
            && chebyshev((cur_tx, cur_ty), route_tile) <= MIGRATE_ARRIVAL_RADIUS
        {
            target.route_tile = None;
            aq.cancel();
            ai.state = AiState::Idle;
            continue;
        }
        // Already at final camp? Skip — arrival system will strip the marker.
        if target.route_tile.is_none()
            && chebyshev((cur_tx, cur_ty), target.tile) <= MIGRATE_ARRIVAL_RADIUS
        {
            continue;
        }
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );
        // Connectivity fast-fail. `assign_task_with_routing` only
        // connectivity-checks for adjacent-task targets; for `Migrate` it
        // always returns true and the path worker would just keep
        // rejecting the request. Release the marker so the agent picks a
        // normal goal next tick instead of cycling dispatch ↔ path-fail.
        let target_z =
            chunk_map.nearest_standable_z(route_tile.0, route_tile.1, ai.current_z as i32) as i8;
        if !chunk_connectivity.tile_reachable(
            &chunk_graph,
            (cur_tx, cur_ty, ai.current_z),
            (route_tile.0, route_tile.1, target_z),
        ) {
            // Bug-fix #4: rather than dropping the marker outright,
            // reroute toward the band centroid (median tile of other
            // migrating members in reachable chunks). Cap retries via
            // `bounce_count` so a band split across an impassable
            // barrier eventually releases its stragglers.
            if target.bounce_count < 2 {
                let root = registry.root_faction(member.faction_id);
                let mut peers: Vec<(i32, i32)> = Vec::new();
                if let Some(tiles) = tiles_per_faction.get(&root) {
                    for &(px, py) in tiles.iter() {
                        let pz = chunk_map.nearest_standable_z(px, py, ai.current_z as i32) as i8;
                        if chunk_connectivity.tile_reachable(
                            &chunk_graph,
                            (cur_tx, cur_ty, ai.current_z),
                            (px, py, pz),
                        ) {
                            peers.push((px, py));
                        }
                    }
                }
                if !peers.is_empty() {
                    peers.sort_by_key(|&(x, y)| (x, y));
                    let mid = peers[peers.len() / 2];
                    target.route_tile = Some(mid);
                    target.bounce_count = target.bounce_count.saturating_add(1);
                } else {
                    commands.entity(e).remove::<MigrationTarget>();
                    *goal = crate::simulation::goals::AgentGoal::GatherFood;
                    aq.cancel();
                    ai.state = AiState::Idle;
                    continue;
                }
            } else {
                commands.entity(e).remove::<MigrationTarget>();
                *goal = crate::simulation::goals::AgentGoal::GatherFood;
                aq.cancel();
                ai.state = AiState::Idle;
                continue;
            }
        }
        let routed = assign_task_with_routing(
            &mut ai,
            (cur_tx, cur_ty),
            cur_chunk,
            target.route_tile.unwrap_or(target.tile),
            TaskKind::Migrate,
            None,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
        );
        if !routed {
            continue;
        }
        ai.state = AiState::Routing;
        let z = ai.target_z;
        let walk_tile = target.route_tile.unwrap_or(target.tile);
        aq.dispatch(Task::WalkTo {
            tile: walk_tile,
            z,
            why: WalkReason::Migration,
        });
        target.last_dispatched_tick = now;
    }
}

/// P1 arrival check — Sequential, after movement_system. Sweeps every
/// agent with a `MigrationTarget`; on chebyshev arrival within
/// `MIGRATE_ARRIVAL_RADIUS`, after `MIGRATE_TIMEOUT_TICKS`, or after
/// `MIGRATE_STALL_TICKS` of dispatch inactivity (Drafted / PlayerOrder /
/// stranded agents), removes the marker, drops back to Idle, and clears
/// the goal so the next 200-tick goal-eval picks a normal need-driven
/// goal.
pub fn nomad_migration_arrival_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut force_reeval: ResMut<crate::simulation::goals::ForceGoalReevaluate>,
    mut q: Query<(
        Entity,
        &mut MigrationTarget,
        &Transform,
        &crate::simulation::faction::FactionMember,
        &mut crate::simulation::goals::AgentGoal,
        &mut crate::simulation::person::PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
    )>,
) {
    use crate::simulation::person::{AiState, PersonAI};
    use crate::world::terrain::TILE_SIZE;
    let now = clock.tick as u32;
    for (e, mut target, transform, _member, mut goal, mut ai, mut aq) in q.iter_mut() {
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        if let Some(route_tile) = target.route_tile {
            if chebyshev((cur_tx, cur_ty), route_tile) <= MIGRATE_ARRIVAL_RADIUS {
                target.route_tile = None;
                aq.cancel();
                ai.state = AiState::Idle;
                continue;
            }
        }
        let arrived = chebyshev((cur_tx, cur_ty), target.tile) <= MIGRATE_ARRIVAL_RADIUS;
        let timed_out = now.saturating_sub(target.started_tick) > MIGRATE_TIMEOUT_TICKS;
        // Stall release: dispatch hasn't advanced `last_dispatched_tick`
        // for a while and the agent is sitting Idle / UNEMPLOYED — either
        // they're filtered out of the dispatcher (Drafted, PlayerOrder)
        // or stranded by repeated path-worker failures. Either way, no
        // further forward progress will happen on its own.
        let stalled = aq.current_task_kind() == UNEMPLOYED_TASK_KIND
            && ai.state == AiState::Idle
            && now.saturating_sub(target.last_dispatched_tick) > MIGRATE_STALL_TICKS;
        if !(arrived || timed_out || stalled) {
            continue;
        }
        commands.entity(e).remove::<MigrationTarget>();
        if *goal == crate::simulation::goals::AgentGoal::MigrateToCamp {
            *goal = if arrived {
                crate::simulation::goals::AgentGoal::FollowingPlayerCommand
            } else {
                crate::simulation::goals::AgentGoal::GatherFood
            };
            force_reeval.0.insert(e);
        }
        // Stop the walk; a normal goal will pick up next tick.
        aq.cancel();
        ai.state = AiState::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_recency_penalises_freshly_vacated_camp() {
        let mut camps: VecDeque<((i32, i32), u32)> = VecDeque::new();
        camps.push_back(((10, 10), 0));
        // Tile near the recent camp, age=0 → strong negative.
        let near = score_recency(&camps, (12, 11), 0);
        assert!(
            near < -20,
            "fresh near-camp penalty should be ~ -25; got {near}"
        );
        // Far tile gets nothing.
        let far = score_recency(&camps, (50, 50), 0);
        assert_eq!(far, 0);
        // Aged-out entry gets nothing.
        let aged = score_recency(&camps, (10, 10), RECENT_CAMP_TTL + 1);
        assert_eq!(aged, 0);
    }

    #[test]
    fn score_biome_season_winter_penalises_tundra() {
        // Use a default Globe — `classify_at_tile` will return whatever
        // biome the noise picks, but Tundra/Mountain/Ocean are all
        // negative regardless of season; the seasonal modifier just
        // doubles down. We can't deterministically place a tile in
        // tundra without seeding, so this test exercises the matrix
        // logic by walking biomes directly.
        for biome in [
            Biome::Tundra,
            Biome::Mountain,
            Biome::Desert,
            Biome::Grassland,
        ] {
            let summer = score_biome_season_for_biome(biome, Season::Summer);
            let winter = score_biome_season_for_biome(biome, Season::Winter);
            match biome {
                Biome::Tundra | Biome::Mountain => {
                    assert!(
                        winter < summer,
                        "{:?} winter should be worse than summer; w={winter} s={summer}",
                        biome
                    );
                }
                Biome::Desert => {
                    assert!(
                        summer < winter,
                        "Desert summer should be worse than winter; w={winter} s={summer}"
                    );
                }
                Biome::Grassland => {
                    assert!(
                        summer >= winter,
                        "Grassland summer should ≥ winter; w={winter} s={summer}"
                    );
                }
                _ => {}
            }
        }
    }

    /// Biome-season scoring extracted for unit-testing without a Globe.
    /// Mirrors the per-(biome, season) table in `score_biome_season`.
    fn score_biome_season_for_biome(biome: Biome, season: Season) -> i32 {
        let base: i32 = match biome {
            Biome::Ocean => -100,
            Biome::Grassland => 15,
            Biome::Steppe => 12,
            Biome::Temperate => 10,
            Biome::Wetland => 8,
            Biome::Tropical => 5,
            Biome::Taiga => 0,
            Biome::Desert => -5,
            Biome::Tundra => -5,
            Biome::Badlands => -8,
            Biome::Mountain => -10,
        };
        let seasonal: i32 = match (biome, season) {
            (Biome::Tundra | Biome::Mountain, Season::Winter) => -15,
            (Biome::Desert | Biome::Badlands, Season::Summer) => -15,
            (Biome::Wetland, Season::Summer) => -8,
            (Biome::Grassland | Biome::Steppe, Season::Spring | Season::Summer) => 5,
            (Biome::Tropical, Season::Winter) => 5,
            _ => 0,
        };
        base + seasonal
    }

    /// Stall release: an agent the dispatcher never serves (here simulated
    /// via `Drafted`, which excludes the agent from the dispatcher and
    /// from `goal_update_system`'s normal selection — the same code path
    /// hunters / lecture attendees take during migration) must still get
    /// out of `Goal::MigrateToCamp` once `MIGRATE_STALL_TICKS` elapses.
    /// Without this we'd be paying the 3-day `MIGRATE_TIMEOUT_TICKS`
    /// fallback for every drafted member of a migrating band.
    #[test]
    fn arrival_stall_releases_drafted_agent() {
        use crate::simulation::test_fixture::TestSim;
        use crate::world::tile::TileKind;
        use bevy::prelude::*;

        let mut sim = TestSim::new(0xBA11D);
        sim.flat_world(1, 0, TileKind::Grass);
        let agent = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});

        // Tick once so all the Startup / first-frame systems settle and
        // the agent acquires its components.
        sim.tick();

        let started_tick = sim.tick_count() as u32;
        // Stamp MigrationTarget at a far tile and lock the agent into
        // MigrateToCamp. `Drafted` keeps both the dispatcher and
        // `goal_update_system`'s normal selection from touching them.
        sim.app.world_mut().entity_mut(agent).insert((
            MigrationTarget {
                tile: (50, 50),
                route_tile: None,
                started_tick,
                last_dispatched_tick: started_tick,
                bounce_count: 0,
            },
            crate::simulation::goals::AgentGoal::MigrateToCamp,
            crate::simulation::person::Drafted,
        ));

        // Walk past the stall threshold. arrival_system runs every
        // tick, so the stall path should fire once the gap exceeds
        // MIGRATE_STALL_TICKS.
        sim.tick_n(MIGRATE_STALL_TICKS + 5);

        assert!(
            sim.app.world().get::<MigrationTarget>(agent).is_none(),
            "MigrationTarget should be removed by stall arrival path",
        );
        let goal = sim
            .app
            .world()
            .get::<crate::simulation::goals::AgentGoal>(agent)
            .copied();
        assert_ne!(
            goal,
            Some(crate::simulation::goals::AgentGoal::MigrateToCamp),
            "stall arrival should release the migration goal",
        );
    }

    /// Within `MIGRATE_ARRIVAL_RADIUS` of the target, the regular arrival
    /// path still releases the marker — this is the "happy path" that
    /// the existing migration pipeline already exercises end-to-end, but
    /// pin it explicitly so a regression in the stall-path edits doesn't
    /// break it.
    #[test]
    fn arrival_radius_releases_when_at_target_tile() {
        use crate::simulation::test_fixture::TestSim;
        use crate::world::tile::TileKind;

        let mut sim = TestSim::new(0xBA12D);
        sim.flat_world(1, 0, TileKind::Grass);
        // Spawn directly on the target tile — chebyshev = 0, well inside
        // `MIGRATE_ARRIVAL_RADIUS`.
        let agent = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        sim.tick();
        let now = sim.tick_count() as u32;
        sim.app.world_mut().entity_mut(agent).insert((
            MigrationTarget {
                tile: (0, 0),
                route_tile: None,
                started_tick: now,
                last_dispatched_tick: now,
                bounce_count: 0,
            },
            crate::simulation::goals::AgentGoal::MigrateToCamp,
        ));
        // One tick is enough — arrival runs in Sequential after movement.
        sim.tick_n(2);
        assert!(sim.app.world().get::<MigrationTarget>(agent).is_none());
    }

    /// Temporary route waypoints should not be treated as camp arrival.
    /// Reaching the route tile only clears `route_tile`; the final
    /// migration target remains pinned until the member reaches the real
    /// camp.
    #[test]
    fn arrival_route_tile_does_not_release_final_target() {
        use crate::simulation::test_fixture::TestSim;
        use crate::world::tile::TileKind;

        let mut sim = TestSim::new(0xCA2A_u64);
        sim.flat_world(1, 0, TileKind::Grass);
        let agent = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        sim.tick();
        let now = sim.tick_count() as u32;
        sim.app.world_mut().entity_mut(agent).insert((
            MigrationTarget {
                tile: (8, 0),
                route_tile: Some((0, 0)),
                started_tick: now,
                last_dispatched_tick: now,
                bounce_count: 1,
            },
            crate::simulation::goals::AgentGoal::MigrateToCamp,
        ));

        sim.tick_n(1);

        let marker = sim
            .app
            .world()
            .get::<MigrationTarget>(agent)
            .copied()
            .expect("route arrival should keep the final migration marker");
        assert_eq!(marker.tile, (8, 0));
        assert_eq!(marker.route_tile, None);
    }

    #[test]
    fn score_danger_ignores_prey_clusters() {
        use crate::simulation::shared_knowledge::ResourceOwner;
        use crate::simulation::shared_knowledge::{KnowledgeTier, SharedKnowledge};
        let mut shared = SharedKnowledge::default();
        // A Prey cluster (deer/wolf mix) right on the tile must NOT be
        // treated as danger — deer are good grazing, not a threat.
        shared.report_sighting(
            KnowledgeTier::Faction(1),
            (10, 10),
            MemoryKind::Prey,
            ResourceOwner::Public,
            0,
        );
        assert_eq!(score_danger(&shared, 1, (10, 10)), 0);
    }

    #[test]
    fn score_danger_penalises_hostile_faction() {
        use crate::simulation::shared_knowledge::ResourceOwner;
        use crate::simulation::shared_knowledge::{KnowledgeTier, SharedKnowledge};
        let mut shared = SharedKnowledge::default();
        shared.report_sighting(
            KnowledgeTier::Faction(1),
            (10, 10),
            MemoryKind::HostileFactionSighting,
            ResourceOwner::Public,
            0,
        );
        assert!(score_danger(&shared, 1, (10, 10)) < 0);
        // Far from the sighting → no penalty.
        assert_eq!(score_danger(&shared, 1, (500, 500)), 0);
    }

    #[test]
    fn migration_cooldown_tracks_capability() {
        use crate::simulation::archetype::HomeMobility;
        let fast = migration_cooldown_ticks(HomeMobility::Mobile {
            migration_period_min_days: 5,
        });
        let slow = migration_cooldown_ticks(HomeMobility::Mobile {
            migration_period_min_days: 30,
        });
        assert!(slow > fast);
        assert_eq!(fast, 5 * TICKS_PER_DAY);
    }
}
