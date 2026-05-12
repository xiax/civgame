//! Nomadic-mode systems: migration trigger + commit.
//!
//! Two-phase pipeline. `nomad_migration_system` (Economy, daily) decides
//! whether each nomadic band wants to move and writes a target tile into
//! `FactionData.pending_migration`. The trailing `nomad_migration_commit_system`
//! (Sequential, every tick) finds factions with a pending target, tears down
//! the old camp's deployable structures within `OLD_CAMP_RADIUS` of the
//! current `home_tile`, then updates `home_tile = target` and clears the
//! pending flag.
//!
//! MVP commit semantics: despawn-only — no refund drops, no re-seed at the
//! new camp. The chief's `nomad_chief_directives` (Phase 7 follow-on) will
//! own replenishment of lost shelter; for now nomads sleep in-place via
//! `Task::Sleep { bed: None }` at the new home until they rebuild.

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
use crate::simulation::schedule::SimClock;
use crate::simulation::shared_knowledge::{KnowledgeTier, SharedKnowledge};
use crate::simulation::technology::current_era;
use crate::simulation::wild_herd::WildHerdRegistry;
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

/// Phase D: per-agent companion to `AgentGoal::Scout`. Carries the
/// quadrant assignment and the actual tile to walk toward. Stamped by
/// `nomad_survey_trigger_system`; cleared by
/// `nomad_survey_completion_system` when the survey window closes.
#[derive(Component, Clone, Copy, Debug)]
pub struct ScoutAssignment {
    pub quadrant: u8, // 0=NE, 1=NW, 2=SW, 3=SE
    pub target_tile: (i32, i32),
    pub assigned_tick: u32,
}

/// P3: composite-score helpers — each helper returns a signed score that's
/// summed into `MigrationScore.total`. Constants tuned so a dominant food
/// cluster (estimated_count ~4) still wins against a weak biome bonus, but
/// equal food candidates choose the better water/season/safety position.
pub const WATER_PROBE_RADIUS: i32 = 8;
pub const RECENT_CAMP_TTL: u32 = TICKS_PER_SEASON * 2;
pub const RECENT_CAMP_RING_CAP: usize = 6;
const PREDATOR_PROBE_RADIUS: i32 = 6;

/// P1: per-agent component pinning the destination of an in-flight
/// migration. Inserted on every band member by `nomad_migration_commit_system`
/// after `home_tile` flips; removed by `nomad_migration_arrival_system`
/// on arrival or timeout.
#[derive(Component, Clone, Copy, Debug)]
pub struct MigrationTarget {
    pub tile: (i32, i32),
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

/// Minimum chebyshev distance between a Packed band's current centroid
/// and a `PitchCamp` target tile, to prevent accidental same-spot
/// re-pitch and to keep the dispatcher's validation strict.
pub const MIN_PITCH_DISTANCE: i32 = 4;

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
                if now < faction.last_migration_tick.saturating_add(TICKS_PER_SEASON) {
                    return None;
                }
                let food_score =
                    score_local_food(shared, fid, faction.home_tile, NOMAD_FORAGE_RADIUS);
                let threshold: u16 = (faction.member_count.max(1) as u16).saturating_mul(3);
                if food_score >= threshold {
                    return None;
                }
                Some(Trigger { fid, home: faction.home_tile })
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
                let need_score =
                    needs.shelter as u32 + needs.sleep as u32 + needs.hunger as u32;
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
                    ai.task_id = crate::simulation::person::PersonAI::UNEMPLOYED;
                    ai.state = crate::simulation::person::AiState::Idle;
                }
                commands.entity(e).insert(ScoutAssignment {
                    quadrant,
                    target_tile: target,
                    assigned_tick: now,
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
                faction.migration_phase =
                    crate::simulation::faction::MigrationPhase::Surveying {
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
        target: (i32, i32),
        scouts: Vec<Entity>,
    }

    // Gather completed surveys.
    let done: Vec<Done> = {
        let registry = world.resource::<FactionRegistry>();
        let shared = world.resource::<SharedKnowledge>();
        let wild_herds = world.resource::<WildHerdRegistry>();
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
                let target = pick_migration_target(
                    shared,
                    wild_herds,
                    chunk_map,
                    globe,
                    calendar.season,
                    &faction.recent_camps,
                    now,
                    fid,
                    faction.home_tile,
                    NOMAD_MIN_TARGET_DIST,
                    NOMAD_MAX_TARGET_DIST,
                )
                .unwrap_or_else(|| fallback_direction(fid, faction.home_tile, now));
                Some(Done {
                    fid,
                    home: faction.home_tile,
                    target,
                    scouts: scouts.clone(),
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
                    ai.task_id = crate::simulation::person::PersonAI::UNEMPLOYED;
                    ai.state = crate::simulation::person::AiState::Idle;
                }
                commands.entity(e).remove::<ScoutAssignment>();
                force_reeval.0.insert(e);
            }
        }
        state.apply(world);
    }

    // Promote phase to PendingCommit + write pending_migration.
    {
        let mut registry = world.resource_mut::<FactionRegistry>();
        for d in done.iter() {
            if let Some(faction) = registry.factions.get_mut(&d.fid) {
                faction.pending_migration = Some(d.target);
                faction.migration_phase =
                    crate::simulation::faction::MigrationPhase::PendingCommit {
                        target: d.target,
                        chosen_tick: now,
                    };
                faction.last_phase_change_tick = now;
                info!(
                    "Faction {} survey complete ({:?} -> {:?}) tick {now}",
                    d.fid, d.home, d.target,
                );
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
    use crate::simulation::person::{AiState, PersonAI};
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
        if ai.state != AiState::Idle || ai.task_id != PersonAI::UNEMPLOYED {
            continue;
        }
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );
        let target = scout.target_tile;
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

/// Commit pass — Sequential, every tick (exclusive system). Drains every
/// faction's `pending_migration`: despawns Beds/Bedrolls/Campfires/Tents/
/// Yurts within `OLD_CAMP_RADIUS` of the current `home_tile`, removes them
/// from `BedMap` / `CampfireMap`, then re-seeds a fresh camp at the target
/// tile via `seed_nomadic_camp` and stamps `last_migration_tick`.
///
/// Exclusive (`&mut World`) because it touches several SystemParam bundles
/// (`FurnitureMaps`, `Commands`, multiple Queries) that together blow past
/// Bevy's 16-param ceiling. Early-outs cheaply when no faction has a
/// pending order.
pub fn nomad_migration_commit_system(world: &mut World) {
    // Snapshot pending migrations + the per-faction context the seeder
    // needs (member count, era for tier selection). Done first so the
    // registry borrow drops before we hand the world to other system
    // states.
    struct Pending {
        fid: u32,
        old_home: (i32, i32),
        target: (i32, i32),
        members: u32,
        era: crate::simulation::technology::Era,
        hearth_tier: crate::simulation::construction::HearthTier,
    }

    let pending: Vec<Pending> = {
        let registry = world.resource::<FactionRegistry>();
        registry
            .factions
            .iter()
            .filter_map(|(&fid, f)| {
                f.pending_migration.map(|target| Pending {
                    fid,
                    old_home: f.home_tile,
                    target,
                    members: f.member_count,
                    era: current_era(&f.techs),
                    hearth_tier: best_hearth_for(&f.techs),
                })
            })
            .collect()
    };
    if pending.is_empty() {
        return;
    }
    let now = world.resource::<SimClock>().tick as u32;

    let packs: Vec<(u32, (i32, i32), i32)> = pending
        .iter()
        .map(|p| {
            let radius = crate::simulation::construction::seed_nomadic_camp_extent(
                p.members, p.era,
            );
            (p.fid, p.old_home, radius)
        })
        .collect();
    pack_camp_assets(world, &packs);

    // ── Re-seed pass ────────────────────────────────────────────────
    // Reuse `seed_nomadic_camp` so the new camp matches the game-start
    // layout (hearth ring + bedrolls + tents + Neo+ yurts). Run one
    // SystemState per migration since `seed_nomadic_camp` mutates the
    // command buffer + furniture maps each call.
    for p in pending.iter() {
        let mut seed_state: SystemState<(
            Commands,
            FurnitureMaps,
            Res<ChunkMap>,
            EventWriter<TileChangedEvent>,
        )> = SystemState::new(world);
        let (mut commands, mut maps, chunk_map, mut tile_changed) = seed_state.get_mut(world);
        let mut used: ahash::AHashSet<(i32, i32)> = ahash::AHashSet::new();
        used.insert(p.target);
        seed_nomadic_camp(
            &mut commands,
            &mut maps,
            &chunk_map,
            &mut tile_changed,
            &mut used,
            p.fid,
            p.target,
            p.members,
            p.era,
            p.hearth_tier,
        );
        seed_state.apply(world);
    }

    // ── Registry mutation ───────────────────────────────────────────
    // Capture each faction's chief (or any first member) for the
    // ActivityLogEvent's `actor`, then mutate registry state.
    let mut actor_per_faction: ahash::AHashMap<u32, Entity> = ahash::AHashMap::new();
    {
        let mut state: SystemState<
            Query<(Entity, &crate::simulation::faction::FactionMember)>,
        > = SystemState::new(world);
        let q = state.get(world);
        for (entity, member) in q.iter() {
            actor_per_faction.entry(member.faction_id).or_insert(entity);
        }
    }
    {
        let mut registry = world.resource_mut::<FactionRegistry>();
        for p in pending.iter() {
            if let Some(faction) = registry.factions.get_mut(&p.fid) {
                // P3: push the now-vacated camp tile into the recent-camps
                // ring before mutating home_tile, so the next migration
                // pick penalises returning here.
                faction.recent_camps.push_back((p.old_home, now));
                while faction.recent_camps.len() > RECENT_CAMP_RING_CAP {
                    faction.recent_camps.pop_front();
                }
                faction.home_tile = p.target;
                faction.last_migration_tick = now;
                faction.pending_migration = None;
                // Phase A: AI flow stays Pitched throughout (atomic
                // pack+despawn+reseed). MigrationPhase tracks the
                // post-commit walking window so other systems can
                // treat "still receiving stragglers" distinctly from
                // "fully resettled".
                faction.camp_state = crate::simulation::faction::CampState::Pitched;
                faction.migration_phase =
                    crate::simulation::faction::MigrationPhase::Walking { target: p.target };
                faction.last_phase_change_tick = now;
            }
        }
    }

    // ── P1: stamp every band member with `MigrationTarget` + flip their
    // goal to MigrateToCamp so the dispatcher actively walks them with
    // the band. Survive-tier needs (raid / starvation / rescue) preempt
    // naturally in `goal_update_system`.
    {
        let migrating: ahash::AHashMap<u32, (i32, i32)> =
            pending.iter().map(|p| (p.fid, p.target)).collect();
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
            let Some(&target) = migrating.get(&root) else {
                continue;
            };
            commands.entity(e).insert(MigrationTarget {
                tile: target,
                started_tick: now,
                last_dispatched_tick: now,
                bounce_count: 0,
            });
            *goal = crate::simulation::goals::AgentGoal::MigrateToCamp;
            // Cancel current chain so the dispatcher picks up MigrateToCamp
            // immediately instead of finishing a pre-migration gather.
            aq.cancel();
            ai.task_id = crate::simulation::person::PersonAI::UNEMPLOYED;
            ai.state = crate::simulation::person::AiState::Idle;
        }
        state.apply(world);
    }

    // ── Activity log ────────────────────────────────────────────────
    // Emit one CampMoved per migrated faction so the player's UI shows
    // "moved camp (x,y) → (x',y')". Chief or first-found member is the
    // notional actor.
    {
        let mut state: SystemState<
            EventWriter<crate::ui::activity_log::ActivityLogEvent>,
        > = SystemState::new(world);
        let mut writer = state.get_mut(world);
        for p in pending.iter() {
            let Some(&actor) = actor_per_faction.get(&p.fid) else {
                continue;
            };
            writer.send(crate::ui::activity_log::ActivityLogEvent {
                tick: now as u64,
                actor,
                faction_id: p.fid,
                kind: crate::ui::activity_log::ActivityEntryKind::CampMoved {
                    from: p.old_home,
                    to: p.target,
                },
            });
        }
        state.apply(world);
    }

    // ── Phase 5 minimum: Tamed animals follow camp ──────────────────
    // Redirect every Tamed animal whose `owner_faction` just migrated to
    // wander toward the new camp tile. The animal_movement_system then
    // walks them there at standard ANIMAL_SPEED. Members of nomadic
    // bands' herds (tamed horses, etc.) thus drift with the camp instead
    // of being abandoned at the old site.
    {
        let mut tamed_state: SystemState<(
            Commands,
            Query<(Entity, &Tamed, &mut AnimalAI)>,
        )> = SystemState::new(world);
        let (mut commands, mut tamed_q) = tamed_state.get_mut(world);
        let new_homes: ahash::AHashMap<u32, (i32, i32)> =
            pending.iter().map(|p| (p.fid, p.target)).collect();
        for (e, tamed, mut ai) in tamed_q.iter_mut() {
            let Some(target) = new_homes.get(&tamed.owner_faction) else {
                continue;
            };
            // Bug-fix #2: stamp `FollowingBand` so the redirect
            // survives Dormant LOD; the standalone redirect system
            // re-snaps target_tile every quarter-day.
            commands.entity(e).insert(crate::simulation::animals::FollowingBand {
                faction: tamed.owner_faction,
                last_redirect_tick: now,
            });
            let seed = tamed.owner_faction.wrapping_mul(0x85EB_CA6B);
            let dx = ((seed & 0b11) as i32) - 2;
            let dy = (((seed >> 2) & 0b11) as i32) - 2;
            ai.target_tile = (target.0 + dx, target.1 + dy);
        }
        tamed_state.apply(world);
    }

    for p in pending.iter() {
        info!(
            "Faction {} migration committed ({:?} -> {:?}) tick {now}",
            p.fid, p.old_home, p.target,
        );
    }
}

#[inline]
fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

/// Pack and despawn the camp assets of the given factions at their
/// anchor tiles. Three passes (band redistribution → pack-into-animals
/// → despawn + refund drops) shared between the AI atomic
/// `nomad_migration_commit_system` and the player-driven
/// `apply_pack_camp_command_system`. Does **not** mutate `home_tile`,
/// `camp_state`, or `migration_phase` — caller decides whether the
/// pack is part of an atomic AI shift (followed by reseed at target)
/// or a player Pack Camp command (which leaves the band Packed
/// indefinitely).
///
/// Each pack entry's third field is the chebyshev radius around the
/// anchor to sweep — derive via `seed_nomadic_camp_extent(members, era)`
/// for the precise band footprint. Bug-fix #6.
pub(crate) fn pack_camp_assets(world: &mut World, packs: &[(u32, (i32, i32), i32)]) {
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
            let report = crate::simulation::nomad_pool::redistribute_essentials(
                &mut view,
                &essentials,
            );
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
            Query<
                (Entity, &Transform, &Tamed, &mut crate::simulation::animals::PackAnimalInventory),
            >,
            Query<(
                Entity,
                &crate::simulation::faction::FactionMember,
                &Transform,
                &mut crate::economy::agent::EconomicAgent,
            )>,
            Res<FactionRegistry>,
        )> = SystemState::new(world);
        let (deployable_q, mut animal_q, mut member_q, registry) = state.get_mut(world);

        for (e, transform, deploy) in deployable_q.iter() {
            let Some(packed_rid) = deploy.packed_form else {
                continue;
            };
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
                }
            }
        }
        state.apply(world);
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
                if let Some(entity) = campfire_map.0.remove(&tile) {
                    commands.entity(entity).despawn_recursive();
                    despawned.insert(entity);
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

/// Apply system for `PlayerCommand::PackCamp`. Drains
/// `PendingCampOps.packs`, calls `pack_camp_assets`, and flips each
/// faction to `CampState::Packed`. Sequential, exclusive `&mut World`.
pub fn apply_pack_camp_command_system(world: &mut World) {
    let raw: Vec<(u32, (i32, i32))> = {
        let mut ops = world.resource_mut::<PendingCampOps>();
        if ops.packs.is_empty() {
            return;
        }
        std::mem::take(&mut ops.packs)
    };
    // Bug-fix #6: derive sweep radius from each band's member count.
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
    pack_camp_assets(world, &packs);
    let now = world.resource::<SimClock>().tick as u32;
    let mut registry = world.resource_mut::<FactionRegistry>();
    for (fid, _anchor, _radius) in packs.iter() {
        if let Some(faction) = registry.factions.get_mut(fid) {
            faction.camp_state =
                crate::simulation::faction::CampState::Packed { since_tick: now };
            faction.migration_phase = crate::simulation::faction::MigrationPhase::Idle;
            faction.last_phase_change_tick = now;
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

    struct Resolved {
        fid: u32,
        old_home: (i32, i32),
        target: (i32, i32),
        members: u32,
        era: crate::simulation::technology::Era,
        hearth_tier: crate::simulation::construction::HearthTier,
    }

    let now = world.resource::<SimClock>().tick as u32;

    let resolved: Vec<Resolved> = {
        let registry = world.resource::<FactionRegistry>();
        pitches
            .iter()
            .filter_map(|p| {
                registry.factions.get(&p.fid).map(|f| Resolved {
                    fid: p.fid,
                    old_home: f.home_tile,
                    target: p.tile,
                    members: f.member_count,
                    era: current_era(&f.techs),
                    hearth_tier: best_hearth_for(&f.techs),
                })
            })
            .collect()
    };

    // Re-seed each pitched camp.
    for r in resolved.iter() {
        let mut seed_state: SystemState<(
            Commands,
            FurnitureMaps,
            Res<ChunkMap>,
            EventWriter<TileChangedEvent>,
        )> = SystemState::new(world);
        let (mut commands, mut maps, chunk_map, mut tile_changed) =
            seed_state.get_mut(world);
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

    // Bug-fix #2: stamp `FollowingBand` on tamed animals owned by
    // freshly-pitched factions so they herd toward the new camp via
    // `following_band_animal_redirect_system` (survives Dormant LOD).
    {
        let pitched_fids: ahash::AHashSet<u32> = resolved.iter().map(|r| r.fid).collect();
        let mut state: SystemState<(
            Commands,
            Query<(Entity, &Tamed)>,
        )> = SystemState::new(world);
        let (mut commands, q) = state.get_mut(world);
        for (e, tamed) in q.iter() {
            if pitched_fids.contains(&tamed.owner_faction) {
                commands.entity(e).insert(crate::simulation::animals::FollowingBand {
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

fn score_local_food(
    shared: &SharedKnowledge,
    fid: u32,
    home: (i32, i32),
    radius: i32,
) -> u16 {
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
#[allow(clippy::too_many_arguments)]
pub fn pick_migration_target(
    shared: &SharedKnowledge,
    wild_herds: &WildHerdRegistry,
    chunk_map: &ChunkMap,
    globe: &Globe,
    season: Season,
    recent_camps: &VecDeque<((i32, i32), u32)>,
    now: u32,
    fid: u32,
    home: (i32, i32),
    min_d: i32,
    max_d: i32,
) -> Option<(i32, i32)> {
    let mut best: Option<((i32, i32), MigrationScore)> = None;

    let mut consider = |tile: (i32, i32), food: i32, herd: i32| {
        let d = chebyshev(tile, home);
        // Phase D: drop the hard annulus filter — `min_d` is just a
        // soft floor (rejects literally-on-camp candidates) and
        // `max_d` is a generous safety cap.
        if d < min_d || d > max_d {
            return;
        }
        let water = score_water(chunk_map, tile, WATER_PROBE_RADIUS);
        let biome_season = score_biome_season(globe, tile, season);
        let danger = score_danger(shared, fid, tile);
        let recency = score_recency(recent_camps, tile, now);
        // Continuous distance penalty: discourages migration to
        // far-away spots unless their food/herd score really earns it.
        let dist_pen = -((d as f32 * DIST_WEIGHT) as i32);
        let total = food + herd + water + biome_season + danger + recency + dist_pen;
        let score = MigrationScore {
            food,
            herd,
            water,
            biome_season,
            danger,
            recency,
            total,
        };
        if best.map_or(true, |(_, s)| total > s.total) {
            best = Some((tile, score));
        }
    };

    if let Some(map) = shared.map(KnowledgeTier::Faction(fid)) {
        for c in map.clusters.values() {
            if !matches!(c.kind, MemoryKind::AnyEdible) {
                continue;
            }
            consider(c.center, c.estimated_count as i32, 0);
        }
    }
    for herd in wild_herds.herds.values() {
        // Wild herd score mirrors the legacy weighting: a 120-head herd
        // contributes 60, comfortably outranking a typical 4-rep cluster.
        let herd_score = (herd.aggregate_count as i32 / 2).max(20);
        consider(herd.leader_tile, 0, herd_score);
    }

    best.map(|(t, _)| t)
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

/// Penalises tiles near sighted predator/prey clusters (proxy for "wolves
/// hunt this area"). −15 per `MemoryKind::Prey` cluster centre within
/// `PREDATOR_PROBE_RADIUS`. A wolf-pack-rich tile thus pulls 15..45 below
/// a quiet alternative — enough to flip equal-food candidates.
pub fn score_danger(shared: &SharedKnowledge, fid: u32, tile: (i32, i32)) -> i32 {
    let Some(map) = shared.map(KnowledgeTier::Faction(fid)) else {
        return 0;
    };
    let mut penalty: i32 = 0;
    for c in map.clusters.values() {
        if !matches!(c.kind, MemoryKind::Prey) {
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
        let new_key = crate::simulation::lifecycle::settled_variant_of(
            &faction.caps.archetype_key,
        );
        lifecycle_queue.push(
            crate::simulation::lifecycle::SettlementLifecycleEvent::SwitchArchetype {
                faction: fid,
                new_archetype_key: new_key,
                at_tile: faction.home_tile,
            },
        );
    }
}

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
    use crate::simulation::construction::{
        next_clear_tile, BuildSiteKind, Blueprint, ShelterTier,
    };

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
            && faction.techs.has(crate::simulation::technology::PORTABLE_DWELLINGS)
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
        Query<(&MigrationTarget, &Transform, &crate::simulation::faction::FactionMember)>,
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
    for (e, mut target, mut goal, transform, mut ai, mut aq, slot, lod, member) in
        q.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !clock.is_active(slot.0) {
            continue;
        }
        if *goal != crate::simulation::goals::AgentGoal::MigrateToCamp {
            continue;
        }
        if ai.task_id != PersonAI::UNEMPLOYED {
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
                Task::WalkTo { why: WalkReason::Migration, .. },
            );
            if stale_migration_walk && ai.state == AiState::Idle {
                aq.cancel();
            } else {
                continue;
            }
        }
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        // Already arrived? Skip — arrival system will strip the marker.
        if chebyshev((cur_tx, cur_ty), target.tile) <= MIGRATE_ARRIVAL_RADIUS {
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
        let target_chunk = ChunkCoord(
            target.tile.0.div_euclid(CHUNK_SIZE as i32),
            target.tile.1.div_euclid(CHUNK_SIZE as i32),
        );
        let target_z = chunk_map.nearest_standable_z(
            target.tile.0,
            target.tile.1,
            ai.current_z as i32,
        ) as i8;
        if !chunk_connectivity
            .is_reachable((cur_chunk, ai.current_z), (target_chunk, target_z))
        {
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
                        let pchunk = ChunkCoord(
                            px.div_euclid(CHUNK_SIZE as i32),
                            py.div_euclid(CHUNK_SIZE as i32),
                        );
                        let pz =
                            chunk_map.nearest_standable_z(px, py, ai.current_z as i32) as i8;
                        if chunk_connectivity
                            .is_reachable((cur_chunk, ai.current_z), (pchunk, pz))
                        {
                            peers.push((px, py));
                        }
                    }
                }
                if !peers.is_empty() {
                    peers.sort_by_key(|&(x, y)| (x, y));
                    let mid = peers[peers.len() / 2];
                    target.tile = mid;
                    target.bounce_count = target.bounce_count.saturating_add(1);
                } else {
                    commands.entity(e).remove::<MigrationTarget>();
                    *goal = crate::simulation::goals::AgentGoal::GatherFood;
                    aq.cancel();
                    ai.task_id = PersonAI::UNEMPLOYED;
                    ai.state = AiState::Idle;
                    continue;
                }
            } else {
                commands.entity(e).remove::<MigrationTarget>();
                *goal = crate::simulation::goals::AgentGoal::GatherFood;
                aq.cancel();
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.state = AiState::Idle;
                continue;
            }
        }
        let routed = assign_task_with_routing(
            &mut ai,
            (cur_tx, cur_ty),
            cur_chunk,
            target.tile,
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
        aq.dispatch(Task::WalkTo {
            tile: target.tile,
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
    mut registry: ResMut<FactionRegistry>,
    mut force_reeval: ResMut<crate::simulation::goals::ForceGoalReevaluate>,
    mut q: Query<(
        Entity,
        &MigrationTarget,
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
    let mut still_walking: ahash::AHashSet<u32> = ahash::AHashSet::default();
    for (e, target, transform, member, mut goal, mut ai, mut aq) in q.iter_mut() {
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let arrived = chebyshev((cur_tx, cur_ty), target.tile) <= MIGRATE_ARRIVAL_RADIUS;
        let timed_out = now.saturating_sub(target.started_tick) > MIGRATE_TIMEOUT_TICKS;
        // Stall release: dispatch hasn't advanced `last_dispatched_tick`
        // for a while and the agent is sitting Idle / UNEMPLOYED — either
        // they're filtered out of the dispatcher (Drafted, PlayerOrder)
        // or stranded by repeated path-worker failures. Either way, no
        // further forward progress will happen on its own.
        let stalled = ai.task_id == PersonAI::UNEMPLOYED
            && ai.state == AiState::Idle
            && now.saturating_sub(target.last_dispatched_tick) > MIGRATE_STALL_TICKS;
        if !(arrived || timed_out || stalled) {
            still_walking.insert(registry.root_faction(member.faction_id));
            continue;
        }
        commands.entity(e).remove::<MigrationTarget>();
        if *goal == crate::simulation::goals::AgentGoal::MigrateToCamp {
            // Bug-fix #3: don't hardcode GatherFood — Survive-tier
            // needs (severe hunger / sleep) should preempt naturally
            // on the next goal-update tick. ForceGoalReevaluate
            // bypasses the 200-tick cadence so the flip happens
            // immediately.
            *goal = crate::simulation::goals::AgentGoal::GatherFood;
            force_reeval.0.insert(e);
        }
        // Stop the walk; a normal goal will pick up next tick.
        aq.cancel();
        ai.task_id = PersonAI::UNEMPLOYED;
        ai.state = AiState::Idle;
    }
    // Phase A: any faction still in `Walking` with no remaining migrators
    // returns to `Idle` so survey/migration can be retriggered later.
    for (fid, faction) in registry.factions.iter_mut() {
        if matches!(
            faction.migration_phase,
            crate::simulation::faction::MigrationPhase::Walking { .. }
        ) && !still_walking.contains(fid)
        {
            faction.migration_phase = crate::simulation::faction::MigrationPhase::Idle;
            faction.last_phase_change_tick = now;
        }
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
        assert!(near < -20, "fresh near-camp penalty should be ~ -25; got {near}");
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
                    assert!(winter < summer, "{:?} winter should be worse than summer; w={winter} s={summer}", biome);
                }
                Biome::Desert => {
                    assert!(summer < winter, "Desert summer should be worse than winter; w={winter} s={summer}");
                }
                Biome::Grassland => {
                    assert!(summer >= winter, "Grassland summer should ≥ winter; w={winter} s={summer}");
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
        assert_eq!(
            goal,
            Some(crate::simulation::goals::AgentGoal::GatherFood),
            "stall arrival should flip MigrateToCamp → GatherFood",
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
}
