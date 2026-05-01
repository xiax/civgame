use ahash::AHashMap;
use bevy::ecs::entity::Entities;
use bevy::prelude::*;

use crate::economy::goods::Good;
use crate::simulation::construction::{Blueprint, BlueprintMap};
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::goals::{AgentGoal, Personality};
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{PersonAI, Profession};
use crate::simulation::schedule::SimClock;
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::technology::CROP_CULTIVATION;

pub type JobId = u32;
pub type RecipeId = u8;

/// Faction-directed job categories. The 50%-of-population cap is enforced per
/// kind: at most `member_count / 2` workers may simultaneously hold a
/// `JobClaim` of any single kind for a given faction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobKind {
    Gather,
    Farm,
    Craft,
    Build,
}

impl JobKind {
    pub fn name(self) -> &'static str {
        match self {
            JobKind::Gather => "Gather",
            JobKind::Farm => "Farm",
            JobKind::Craft => "Craft",
            JobKind::Build => "Build",
        }
    }

    pub fn to_goal(self) -> AgentGoal {
        match self {
            JobKind::Gather => AgentGoal::GatherFood,
            JobKind::Farm => AgentGoal::Farm,
            JobKind::Craft => AgentGoal::Craft,
            JobKind::Build => AgentGoal::Build,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobSource {
    Chief,
    Player,
}

/// Axis-aligned tile bounding box (inclusive on both ends) used to scope Farm
/// jobs to a designated zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileAabb {
    pub min: (i16, i16),
    pub max: (i16, i16),
}

impl TileAabb {
    pub fn contains(&self, tile: (i16, i16)) -> bool {
        tile.0 >= self.min.0
            && tile.0 <= self.max.0
            && tile.1 >= self.min.1
            && tile.1 <= self.max.1
    }
}

/// Quantitative completion criterion for a posting. The job auto-releases when
/// `is_complete()` returns true.
#[derive(Clone, Debug)]
pub enum JobProgress {
    /// Gather food: total calories deposited at faction storage.
    Calories { deposited: u32, target: u32 },
    /// Farm: tiles successfully planted within the designated area.
    Planting {
        planted: u32,
        target: u32,
        area: TileAabb,
    },
    /// Craft: units of a specific recipe produced.
    Crafting {
        crafted: u32,
        target: u32,
        recipe: RecipeId,
        bench: Option<Entity>,
    },
    /// Build: completes when the named blueprint entity despawns.
    Building { blueprint: Entity },
}

impl JobProgress {
    pub fn is_complete(&self) -> bool {
        match self {
            JobProgress::Calories { deposited, target } => deposited >= target,
            JobProgress::Planting {
                planted, target, ..
            } => planted >= target,
            JobProgress::Crafting {
                crafted, target, ..
            } => crafted >= target,
            // Build completion is signalled externally by the despawn hook
            // (which removes the posting); this returns false because the
            // posting is removed before this would ever be re-checked.
            JobProgress::Building { .. } => false,
        }
    }

    pub fn fraction(&self) -> f32 {
        match self {
            JobProgress::Calories { deposited, target } => {
                if *target == 0 {
                    1.0
                } else {
                    (*deposited as f32 / *target as f32).clamp(0.0, 1.0)
                }
            }
            JobProgress::Planting {
                planted, target, ..
            } => {
                if *target == 0 {
                    1.0
                } else {
                    (*planted as f32 / *target as f32).clamp(0.0, 1.0)
                }
            }
            JobProgress::Crafting {
                crafted, target, ..
            } => {
                if *target == 0 {
                    1.0
                } else {
                    (*crafted as f32 / *target as f32).clamp(0.0, 1.0)
                }
            }
            JobProgress::Building { .. } => 0.0,
        }
    }
}

/// A faction-directed work order. Multiple workers can claim the same posting
/// and contribute to its `progress`; the per-worker `JobClaim` lock ensures
/// each worker holds only one job at a time.
#[derive(Clone, Debug)]
pub struct JobPosting {
    pub id: JobId,
    pub faction_id: u32,
    pub kind: JobKind,
    pub progress: JobProgress,
    pub claimants: Vec<Entity>,
    pub priority: u8,
    pub source: JobSource,
    pub posted_tick: u32,
    pub expiry_tick: Option<u32>,
}

/// Component attached to a worker holding an active claim. A worker holds at
/// most one `JobClaim` at any time; while present, `goal_update_system` will
/// lock the worker's `AgentGoal` to the job's mapped goal except for crisis
/// overrides (which also drop the claim).
#[derive(Component, Clone, Copy, Debug)]
pub struct JobClaim {
    pub job_id: JobId,
    pub faction_id: u32,
    pub kind: JobKind,
    pub posted_tick: u32,
    pub fail_count: u8,
}

/// Global resource holding all postings, sharded internally by faction.
#[derive(Resource, Default)]
pub struct JobBoard {
    pub postings: AHashMap<u32, Vec<JobPosting>>,
    pub next_id: JobId,
}

impl JobBoard {
    pub fn alloc_id(&mut self) -> JobId {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    pub fn faction_postings(&self, faction_id: u32) -> &[JobPosting] {
        self.postings
            .get(&faction_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn faction_postings_mut(&mut self, faction_id: u32) -> &mut Vec<JobPosting> {
        self.postings.entry(faction_id).or_default()
    }

    /// Find a posting by id across all factions. Returns (faction_id, index).
    pub fn locate(&self, job_id: JobId) -> Option<(u32, usize)> {
        for (fid, list) in self.postings.iter() {
            if let Some(idx) = list.iter().position(|p| p.id == job_id) {
                return Some((*fid, idx));
            }
        }
        None
    }

    pub fn get(&self, job_id: JobId) -> Option<&JobPosting> {
        self.locate(job_id)
            .and_then(|(fid, idx)| self.postings.get(&fid).and_then(|v| v.get(idx)))
    }

    pub fn get_mut(&mut self, job_id: JobId) -> Option<&mut JobPosting> {
        let (fid, idx) = self.locate(job_id)?;
        self.postings.get_mut(&fid).and_then(|v| v.get_mut(idx))
    }
}

/// External commands for the job board. UI / scripted overrides post and
/// cancel jobs by emitting these events; `job_board_command_system` (added in
/// Stage 6) consumes them.
#[derive(Event, Clone, Debug)]
pub enum JobBoardCommand {
    Post(JobPosting),
    Cancel(JobId),
    SetPriority(JobId, u8),
}

/// Event fired when a posting completes (its progress hit the target, the
/// build finished, etc.). A reactor system clears claimants from the board.
#[derive(Event, Clone, Copy, Debug)]
pub struct JobCompletedEvent {
    pub job_id: JobId,
    pub faction_id: u32,
    pub kind: JobKind,
}

pub struct JobsPlugin;

impl Plugin for JobsPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(JobBoard::default())
            .add_event::<JobBoardCommand>()
            .add_event::<JobCompletedEvent>();
    }
}

/// How often the chief reconciles the job board, in fixed-update ticks.
const CHIEF_POSTING_INTERVAL: u64 = 60;

/// Default priority for chief-posted jobs. Player postings use a higher value
/// so worker scoring favors them.
const CHIEF_PRIORITY: u8 = 100;
pub const PLAYER_PRIORITY: u8 = 200;

/// Gather posting threshold: post when `food_total / member_count` falls
/// below this value (in `Good::nutrition` units, which are per-stack).
const GATHER_TARGET_PER_HEAD: u32 = 8;

/// Maximum target size for any single Gather posting, so progress is visible
/// and partial completions release some workers earlier.
const GATHER_TARGET_CAP: u32 = 600;
const GATHER_TARGET_MIN: u32 = 80;

/// Farm posting target: number of tiles to plant in one posting.
const FARM_TILES_PER_POST: u32 = 6;

/// Chief job-posting reconciliation. Runs every `CHIEF_POSTING_INTERVAL` ticks
/// in `SimulationSet::Economy`. Posts Build jobs for non-personal blueprints,
/// Gather jobs when food per-head is low, Farm jobs when Agriculture is
/// researched and seeds/grain are low, and Craft jobs when supply<demand for
/// craftable goods. Reconciliation is idempotent: stale unclaimed Chief
/// postings whose target no longer needs work are dropped before new ones are
/// added.
pub fn chief_job_posting_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    bp_map: Res<BlueprintMap>,
    bp_query: Query<&Blueprint>,
    workbench_map: Res<crate::simulation::construction::WorkbenchMap>,
    mut board: ResMut<JobBoard>,
) {
    if clock.tick % CHIEF_POSTING_INTERVAL != 0 {
        return;
    }

    // Group all live, non-personal blueprints by faction.
    let mut bps_by_faction: AHashMap<u32, Vec<Entity>> = AHashMap::new();
    for &bp_entity in bp_map.0.values() {
        let Ok(bp) = bp_query.get(bp_entity) else {
            continue;
        };
        if bp.personal_owner.is_some() {
            continue;
        }
        bps_by_faction
            .entry(bp.faction_id)
            .or_default()
            .push(bp_entity);
    }

    let posted_tick = clock.tick as u32;

    for (&faction_id, faction) in registry.factions.iter() {
        if faction_id == SOLO {
            continue;
        }
        let live_bps: Vec<Entity> = bps_by_faction
            .get(&faction_id)
            .cloned()
            .unwrap_or_default();

        // 1. Drop stale unclaimed Chief postings whose target no longer needs work.
        {
            let postings = board.faction_postings_mut(faction_id);
            postings.retain(|p| {
                if !matches!(p.source, JobSource::Chief) {
                    return true;
                }
                if !p.claimants.is_empty() {
                    return true;
                }
                match p.progress {
                    JobProgress::Building { blueprint } => bp_query.get(blueprint).is_ok(),
                    JobProgress::Calories { .. }
                    | JobProgress::Planting { .. }
                    | JobProgress::Crafting { .. } => false,
                }
            });
        }

        // 2. Build postings — one per uncovered blueprint.
        let needed_builds: Vec<Entity> = {
            let postings = board.faction_postings_mut(faction_id);
            live_bps
                .iter()
                .copied()
                .filter(|bp_entity| {
                    !postings.iter().any(|p| matches!(
                        p.progress,
                        JobProgress::Building { blueprint } if blueprint == *bp_entity
                    ))
                })
                .collect()
        };
        for bp_entity in needed_builds {
            let id = board.alloc_id();
            board.faction_postings_mut(faction_id).push(JobPosting {
                id,
                faction_id,
                kind: JobKind::Build,
                progress: JobProgress::Building {
                    blueprint: bp_entity,
                },
                claimants: Vec::new(),
                priority: CHIEF_PRIORITY,
                source: JobSource::Chief,
                posted_tick,
                expiry_tick: None,
            });
        }

        // 3. Gather posting — one if storage food per-head is below threshold.
        if faction.member_count > 0 {
            let already_gather = board
                .faction_postings(faction_id)
                .iter()
                .any(|p| matches!(p.kind, JobKind::Gather));
            if !already_gather {
                let food_total = faction.storage.food_total() as u32;
                let target_supply = faction.member_count * GATHER_TARGET_PER_HEAD * 8;
                if food_total < target_supply {
                    let deficit_units = target_supply.saturating_sub(food_total);
                    // Convert deficit units to a calorie target (use Fruit nutrition
                    // as a conservative average; deposits contribute their actual
                    // good's nutrition).
                    let calories =
                        deficit_units * Good::Fruit.nutrition() as u32;
                    let target = calories.clamp(GATHER_TARGET_MIN, GATHER_TARGET_CAP);
                    let id = board.alloc_id();
                    board.faction_postings_mut(faction_id).push(JobPosting {
                        id,
                        faction_id,
                        kind: JobKind::Gather,
                        progress: JobProgress::Calories {
                            deposited: 0,
                            target,
                        },
                        claimants: Vec::new(),
                        priority: CHIEF_PRIORITY,
                        source: JobSource::Chief,
                        posted_tick,
                        expiry_tick: None,
                    });
                }
            }
        }

        // 4. Farm posting — gated on Agriculture (CROP_CULTIVATION).
        if faction.techs.has(CROP_CULTIVATION) && faction.member_count > 0 {
            let already_farm = board
                .faction_postings(faction_id)
                .iter()
                .any(|p| matches!(p.kind, JobKind::Farm));
            let grain = faction.storage.totals.get(&Good::Grain).copied().unwrap_or(0);
            let seed = faction.storage.seed_total();
            // Post farm if grain is low and seeds are available.
            if !already_farm && grain < faction.member_count * 4 && seed > 0 {
                let area = TileAabb {
                    min: (
                        faction.home_tile.0.saturating_sub(5),
                        faction.home_tile.1.saturating_sub(5),
                    ),
                    max: (
                        faction.home_tile.0.saturating_add(5),
                        faction.home_tile.1.saturating_add(5),
                    ),
                };
                let id = board.alloc_id();
                board.faction_postings_mut(faction_id).push(JobPosting {
                    id,
                    faction_id,
                    kind: JobKind::Farm,
                    progress: JobProgress::Planting {
                        planted: 0,
                        target: FARM_TILES_PER_POST.min(seed),
                        area,
                    },
                    claimants: Vec::new(),
                    priority: CHIEF_PRIORITY,
                    source: JobSource::Chief,
                    posted_tick,
                    expiry_tick: None,
                });
            }
        }

        // 5. Craft posting — Stone Tools (recipe 0) when Tools supply<demand
        // and a workbench is available.
        if faction.member_count > 0 {
            let already_craft = board
                .faction_postings(faction_id)
                .iter()
                .any(|p| matches!(p.kind, JobKind::Craft));
            let supply = faction.resource_supply.get(&Good::Tools).copied().unwrap_or(0);
            let demand = faction.resource_demand.get(&Good::Tools).copied().unwrap_or(0);
            if !already_craft && demand > supply {
                // Pick any workbench in the faction's home zone (proximity test).
                let bench: Option<Entity> = workbench_map
                    .0
                    .iter()
                    .filter(|((tx, ty), _)| {
                        let dx = (*tx as i32 - faction.home_tile.0 as i32).abs();
                        let dy = (*ty as i32 - faction.home_tile.1 as i32).abs();
                        dx <= 12 && dy <= 12
                    })
                    .map(|(_, e)| *e)
                    .next();
                if let Some(bench_entity) = bench {
                    let target = (demand - supply).min(5);
                    let id = board.alloc_id();
                    board.faction_postings_mut(faction_id).push(JobPosting {
                        id,
                        faction_id,
                        kind: JobKind::Craft,
                        progress: JobProgress::Crafting {
                            crafted: 0,
                            target,
                            recipe: 0,
                            bench: Some(bench_entity),
                        },
                        claimants: Vec::new(),
                        priority: CHIEF_PRIORITY,
                        source: JobSource::Chief,
                        posted_tick,
                        expiry_tick: None,
                    });
                }
            }
        }
    }
}

/// Skill axis used when scoring a candidate job for a worker.
fn skill_for(kind: JobKind) -> SkillKind {
    match kind {
        JobKind::Gather => SkillKind::Farming,
        JobKind::Farm => SkillKind::Farming,
        JobKind::Build => SkillKind::Building,
        JobKind::Craft => SkillKind::Crafting,
    }
}

/// Personality additive bias for job kinds. Range -0.2 .. +0.4.
fn personality_bias(p: Personality, kind: JobKind) -> f32 {
    match (p, kind) {
        (Personality::Gatherer, JobKind::Gather) => 0.4,
        (Personality::Gatherer, JobKind::Farm) => 0.2,
        (Personality::Nurturer, JobKind::Farm) => 0.3,
        (Personality::Nurturer, JobKind::Gather) => 0.2,
        (Personality::Nurturer, JobKind::Craft) => 0.1,
        (Personality::Explorer, JobKind::Gather) => 0.2,
        (Personality::Explorer, JobKind::Farm) => -0.1,
        (Personality::Socialite, JobKind::Build) => 0.1,
        (Personality::Socialite, JobKind::Craft) => 0.1,
        (Personality::Loner, _) => -0.2,
        _ => 0.0,
    }
}

/// Profession additive bias. Profession is the worker-directed baseline; this
/// makes a Farmer the first to claim an open Farm job, while still letting
/// the worker do farming via normal plan selection when no Farm job exists.
fn profession_bias(p: Profession, kind: JobKind) -> f32 {
    match (p, kind) {
        (Profession::Farmer, JobKind::Farm) => 0.5,
        (Profession::Farmer, JobKind::Gather) => 0.1,
        _ => 0.0,
    }
}

/// Claim available jobs for idle workers, with full scoring and the 50%
/// per-category cap.
pub fn job_claim_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    mut board: ResMut<JobBoard>,
    workers: Query<
        (
            Entity,
            &FactionMember,
            &PersonAI,
            &LodLevel,
            &Skills,
            &Personality,
            Option<&Profession>,
            &Transform,
        ),
        Without<JobClaim>,
    >,
) {
    let posted_tick = clock.tick as u32;

    // Pre-pass: count active claims per (faction_id, kind) by scanning the
    // board's claimant lists. This is the cap-enforcement input.
    let mut claim_counts: AHashMap<(u32, JobKind), u32> = AHashMap::new();
    for (faction_id, postings) in board.postings.iter() {
        for p in postings.iter() {
            *claim_counts.entry((*faction_id, p.kind)).or_insert(0) +=
                p.claimants.len() as u32;
        }
    }

    for (worker, member, ai, lod, skills, personality, profession_opt, transform) in workers.iter()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if ai.task_id != PersonAI::UNEMPLOYED {
            continue;
        }
        let faction_id = member.faction_id;
        if faction_id == SOLO {
            continue;
        }
        let Some(faction) = registry.factions.get(&faction_id) else {
            continue;
        };
        let cap = (faction.member_count / 2).max(1);
        let profession = profession_opt.copied().unwrap_or(Profession::None);
        let worker_tile = (
            (transform.translation.x / crate::world::terrain::TILE_SIZE).floor() as i16,
            (transform.translation.y / crate::world::terrain::TILE_SIZE).floor() as i16,
        );

        // Score every eligible posting and pick the best.
        let mut best: Option<(usize, f32)> = None;
        let postings = board.faction_postings(faction_id);
        for (idx, p) in postings.iter().enumerate() {
            // Cap: skip kinds already at 50%.
            let count = claim_counts
                .get(&(faction_id, p.kind))
                .copied()
                .unwrap_or(0);
            if count >= cap {
                continue;
            }
            // Skip postings that completed but haven't been removed yet.
            if p.progress.is_complete() {
                continue;
            }
            let skill_norm = skills.0[skill_for(p.kind) as usize] as f32 / 255.0;
            let target_tile = posting_target_tile(p);
            let dist = match target_tile {
                Some((tx, ty)) => {
                    let dx = tx as f32 - worker_tile.0 as f32;
                    let dy = ty as f32 - worker_tile.1 as f32;
                    (dx * dx + dy * dy).sqrt()
                }
                None => 0.0,
            };
            let score = (p.priority as f32) * 0.01
                + skill_norm
                + personality_bias(*personality, p.kind)
                + profession_bias(profession, p.kind)
                - dist * 0.001;
            match best {
                Some((_, s)) if s >= score => {}
                _ => best = Some((idx, score)),
            }
        }

        let Some((idx, _)) = best else { continue };
        // Apply the claim: insert component, push claimant, bump cap counter.
        let postings = board.faction_postings_mut(faction_id);
        let posting = &mut postings[idx];
        posting.claimants.push(worker);
        *claim_counts.entry((faction_id, posting.kind)).or_insert(0) += 1;
        commands.entity(worker).insert(JobClaim {
            job_id: posting.id,
            faction_id,
            kind: posting.kind,
            posted_tick,
            fail_count: 0,
        });
    }
}

/// Best-effort representative tile for a posting (used in distance scoring).
fn posting_target_tile(p: &JobPosting) -> Option<(i16, i16)> {
    match p.progress {
        JobProgress::Calories { .. } => None,
        JobProgress::Planting { area, .. } => Some((
            (area.min.0 as i32 + area.max.0 as i32) as i16 / 2,
            (area.min.1 as i32 + area.max.1 as i32) as i16 / 2,
        )),
        JobProgress::Crafting { .. } => None,
        JobProgress::Building { .. } => None,
    }
}

/// After `goal_update_system` has run for the tick, lock claimed workers'
/// goals to the job kind. If a crisis-class goal won (Survive/Defend/Raid/
/// Rescue), drop the claim instead — the crisis takes precedence and the
/// worker is freed from the job board.
pub fn job_goal_lock_system(
    mut commands: Commands,
    mut board: ResMut<JobBoard>,
    mut workers: Query<(Entity, &mut AgentGoal, &JobClaim)>,
) {
    for (worker, mut goal, claim) in workers.iter_mut() {
        let crisis = matches!(
            *goal,
            AgentGoal::Survive | AgentGoal::Defend | AgentGoal::Raid | AgentGoal::Rescue
        );
        if crisis {
            commands.entity(worker).remove::<JobClaim>();
            release_claimant(&mut board, claim.job_id, worker);
        } else {
            *goal = claim.kind.to_goal();
        }
    }
}

/// Blueprint-despawn detection for Build jobs. Any posting whose target
/// `Blueprint` entity no longer exists is treated as completed: claimants
/// drop their `JobClaim`, the posting is removed, and a completion event
/// fires for downstream listeners.
pub fn job_build_completion_system(
    mut commands: Commands,
    mut board: ResMut<JobBoard>,
    bp_query: Query<(), With<Blueprint>>,
    mut completed_events: EventWriter<JobCompletedEvent>,
) {
    let mut to_release: Vec<(JobId, u32, JobKind, Vec<Entity>)> = Vec::new();
    for (&faction_id, postings) in board.postings.iter_mut() {
        postings.retain(|p| match p.progress {
            JobProgress::Building { blueprint } => {
                if bp_query.get(blueprint).is_err() {
                    to_release.push((p.id, faction_id, p.kind, p.claimants.clone()));
                    false
                } else {
                    true
                }
            }
            _ => true,
        });
    }
    for (job_id, faction_id, kind, claimants) in to_release {
        for c in claimants {
            commands.entity(c).remove::<JobClaim>();
        }
        completed_events.send(JobCompletedEvent {
            job_id,
            faction_id,
            kind,
        });
    }
}

/// Helper: remove a single claimant from a posting (used on crisis override).
pub fn release_claimant(board: &mut JobBoard, job_id: JobId, worker: Entity) {
    if let Some(p) = board.get_mut(job_id) {
        p.claimants.retain(|&c| c != worker);
    }
}

/// Record progress against a worker's active claim. Called from concrete
/// mutation sites (food deposit, planter completion, craft completion). If the
/// posting completes as a result, all claimants are removed via `commands` and
/// a `JobCompletedEvent` is fired.
///
/// `kind_filter` lets callers gate on the posting kind — e.g. food deposits
/// only count for Gather postings, planting only for Farm postings.
pub fn record_progress(
    commands: &mut Commands,
    board: &mut JobBoard,
    completed_events: &mut EventWriter<JobCompletedEvent>,
    claim: &JobClaim,
    kind_filter: JobKind,
    increment: u32,
) {
    if claim.kind != kind_filter {
        return;
    }
    let Some(posting) = board.get_mut(claim.job_id) else {
        return;
    };
    let mut completed = false;
    match &mut posting.progress {
        JobProgress::Calories { deposited, target } => {
            *deposited = deposited.saturating_add(increment);
            if deposited >= target {
                completed = true;
            }
        }
        JobProgress::Planting {
            planted, target, ..
        } => {
            *planted = planted.saturating_add(increment);
            if planted >= target {
                completed = true;
            }
        }
        JobProgress::Crafting {
            crafted, target, ..
        } => {
            *crafted = crafted.saturating_add(increment);
            if crafted >= target {
                completed = true;
            }
        }
        JobProgress::Building { .. } => {
            // Build progress is signalled by Blueprint despawn, not increments.
        }
    }
    if completed {
        let job_id = posting.id;
        let faction_id = posting.faction_id;
        let kind = posting.kind;
        let claimants: Vec<Entity> = std::mem::take(&mut posting.claimants);
        // Remove the posting now that it's done.
        if let Some((fid, idx)) = board.locate(job_id) {
            board.postings.get_mut(&fid).unwrap().swap_remove(idx);
        }
        for c in claimants {
            commands.entity(c).remove::<JobClaim>();
        }
        completed_events.send(JobCompletedEvent {
            job_id,
            faction_id,
            kind,
        });
    }
}

/// Public helper for callers that have a tile and want to know whether it
/// falls within a posting's farm area (used by the planter completion hook).
pub fn planting_area_contains(progress: &JobProgress, tile: (i16, i16)) -> bool {
    match progress {
        JobProgress::Planting { area, .. } => area.contains(tile),
        _ => false,
    }
}

const RELEASE_SWEEP_INTERVAL: u64 = 60;
/// Maximum ticks a worker may hold a JobClaim while idle (no progress) before
/// fail_count increments. ~180 ticks is 9 seconds at 20 Hz fixed update.
const STUCK_FAIL_INTERVAL: u64 = 180;
const MAX_FAIL_COUNT: u8 = 3;

/// Process external `JobBoardCommand` events (UI / scripted overrides). Posts
/// new postings, cancels existing ones (releasing claimants), and updates
/// priority. Player-sourced postings supersede a Chief posting on the same
/// `(faction, kind, target)` if one exists.
pub fn job_board_command_system(
    mut commands: Commands,
    mut commands_in: EventReader<JobBoardCommand>,
    mut board: ResMut<JobBoard>,
    mut completed_events: EventWriter<JobCompletedEvent>,
) {
    for cmd in commands_in.read() {
        match cmd.clone() {
            JobBoardCommand::Post(mut new_posting) => {
                // Replace any existing posting with the same logical target so
                // the player override doesn't leave a duplicate Chief job in
                // place.
                if matches!(new_posting.source, JobSource::Player) {
                    let mut to_drop: Option<JobId> = None;
                    if let Some(list) = board.postings.get(&new_posting.faction_id) {
                        for p in list.iter() {
                            if p.kind == new_posting.kind
                                && same_target(&p.progress, &new_posting.progress)
                                && matches!(p.source, JobSource::Chief)
                            {
                                to_drop = Some(p.id);
                                break;
                            }
                        }
                    }
                    if let Some(id) = to_drop {
                        if let Some((fid, idx)) = board.locate(id) {
                            let dropped = board.postings.get_mut(&fid).unwrap().swap_remove(idx);
                            // Transfer claimants over to the new player posting.
                            new_posting.claimants = dropped.claimants;
                        }
                    }
                }
                let id = board.alloc_id();
                new_posting.id = id;
                board
                    .faction_postings_mut(new_posting.faction_id)
                    .push(new_posting);
            }
            JobBoardCommand::Cancel(job_id) => {
                if let Some((fid, idx)) = board.locate(job_id) {
                    let posting = board.postings.get_mut(&fid).unwrap().swap_remove(idx);
                    for c in posting.claimants {
                        commands.entity(c).remove::<JobClaim>();
                    }
                    completed_events.send(JobCompletedEvent {
                        job_id,
                        faction_id: fid,
                        kind: posting.kind,
                    });
                }
            }
            JobBoardCommand::SetPriority(job_id, priority) => {
                if let Some(posting) = board.get_mut(job_id) {
                    posting.priority = priority;
                }
            }
        }
    }
}

/// Two `JobProgress` values are considered to share a target if they refer to
/// the same blueprint, recipe, or farm area. Calorie-target Gather postings
/// are matched by kind alone.
fn same_target(a: &JobProgress, b: &JobProgress) -> bool {
    match (a, b) {
        (JobProgress::Building { blueprint: x }, JobProgress::Building { blueprint: y }) => {
            x == y
        }
        (
            JobProgress::Crafting { recipe: rx, .. },
            JobProgress::Crafting { recipe: ry, .. },
        ) => rx == ry,
        (JobProgress::Planting { area: ax, .. }, JobProgress::Planting { area: ay, .. }) => {
            ax == ay
        }
        (JobProgress::Calories { .. }, JobProgress::Calories { .. }) => true,
        _ => false,
    }
}

/// Periodic release sweep: drops claims whose target became invalid, expired
/// player postings, prunes dead claimants, and increments fail_count for
/// workers stuck idle. Releases claims when fail_count crosses MAX_FAIL_COUNT.
pub fn job_claim_release_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut board: ResMut<JobBoard>,
    mut completed_events: EventWriter<JobCompletedEvent>,
    bench_query: Query<(), With<crate::simulation::construction::Workbench>>,
    mut workers: Query<(Entity, &PersonAI, &mut JobClaim)>,
    entities: &Entities,
) {
    if clock.tick % RELEASE_SWEEP_INTERVAL != 0 {
        return;
    }
    let now = clock.tick as u32;

    // 1. Prune dead claimants and find postings to expire/release.
    let mut to_remove: Vec<JobId> = Vec::new();
    for postings in board.postings.values_mut() {
        for p in postings.iter_mut() {
            p.claimants.retain(|&c| entities.contains(c));
        }
        for p in postings.iter() {
            // Expiry (player postings).
            if let Some(expiry) = p.expiry_tick {
                if now >= expiry {
                    to_remove.push(p.id);
                    continue;
                }
            }
            // Workbench / bench-target invalid.
            if let JobProgress::Crafting {
                bench: Some(b), ..
            } = p.progress
            {
                if bench_query.get(b).is_err() {
                    to_remove.push(p.id);
                }
            }
        }
    }
    for job_id in to_remove {
        if let Some((fid, idx)) = board.locate(job_id) {
            let posting = board.postings.get_mut(&fid).unwrap().swap_remove(idx);
            for c in posting.claimants {
                commands.entity(c).remove::<JobClaim>();
            }
            completed_events.send(JobCompletedEvent {
                job_id,
                faction_id: fid,
                kind: posting.kind,
            });
        }
    }

    // 2. Stuck-idle fail-count bump. Workers idle without a task held longer
    // than STUCK_FAIL_INTERVAL since the claim was posted have their
    // fail_count incremented; once they hit MAX_FAIL_COUNT the claim is
    // released so the worker can pick something else.
    for (worker, ai, mut claim) in workers.iter_mut() {
        if ai.task_id != PersonAI::UNEMPLOYED {
            continue;
        }
        if (clock.tick as u32).saturating_sub(claim.posted_tick) < STUCK_FAIL_INTERVAL as u32 {
            continue;
        }
        claim.fail_count = claim.fail_count.saturating_add(1);
        // Reset posted_tick so we don't increment every sweep tick.
        claim.posted_tick = now;
        if claim.fail_count >= MAX_FAIL_COUNT {
            let job_id = claim.job_id;
            commands.entity(worker).remove::<JobClaim>();
            release_claimant(&mut board, job_id, worker);
        }
    }
}
